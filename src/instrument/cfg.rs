//! `cfg`, `mutants::skip`, test-only range, and expression reach analysis.

use std::collections::BTreeSet;

use proc_macro2::Span;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use super::model::{BitValueType, LocalBitType};

/// `#[mutants::skip]`, also inside `cfg_attr`: the mark cargo-mutants uses for code that
/// is not to be mutated.
pub(crate) fn is_skipped(attrs: &[syn::Attribute]) -> bool {
    fn marked(meta: &syn::Meta) -> bool {
        let path = meta.path();
        if path.segments.len() == 2
            && path.segments[0].ident == "mutants"
            && path.segments[1].ident == "skip"
        {
            return true;
        }
        if path.is_ident("cfg_attr")
            && let syn::Meta::List(list) = meta
            && let Ok(parts) = list.parse_args_with(
                syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
            )
        {
            return parts.iter().skip(1).any(marked);
        }
        false
    }

    attrs.iter().any(|attr| marked(&attr.meta))
}

/// Whether the predicate may have the desired value while `test` is false.
pub(crate) fn cfg_possible_without_test(meta: &syn::Meta, desired: bool) -> bool {
    if meta.path().is_ident("test") {
        return !desired;
    }
    let syn::Meta::List(list) = meta else {
        return true;
    };
    let Ok(parts) = list.parse_args_with(
        syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
    ) else {
        return true;
    };
    if list.path.is_ident("not") && parts.len() == 1 {
        return cfg_possible_without_test(&parts[0], !desired);
    }
    if list.path.is_ident("all") || list.path.is_ident("any") {
        return if list.path.is_ident("all") == desired {
            parts
                .iter()
                .all(|part| cfg_possible_without_test(part, desired))
        } else {
            parts
                .iter()
                .any(|part| cfg_possible_without_test(part, desired))
        };
    }
    true
}

/// What a `cfg` predicate is, as far as the features that are on decide it: `None` when
/// it depends on something else (the target, `test`, a `--cfg` of the build).
pub(super) fn cfg_value(meta: &syn::Meta, features: &BTreeSet<String>) -> Option<bool> {
    match meta {
        syn::Meta::NameValue(pair) if pair.path.is_ident("feature") => match &pair.value {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(name),
                ..
            }) => Some(features.contains(&name.value())),
            _ => None,
        },
        syn::Meta::List(list) => {
            let parts = list
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                )
                .ok()?;
            let mut values = parts.iter().map(|part| cfg_value(part, features));
            if list.path.is_ident("not") && parts.len() == 1 {
                values.next()?.map(|value| !value)
            } else if list.path.is_ident("all") {
                // One part that is off settles it; otherwise every part must be known.
                values
                    .try_fold(Some(true), |all, value| match (all, value) {
                        (_, Some(false)) => Err(()),
                        (Some(true), Some(true)) => Ok(Some(true)),
                        _ => Ok(None),
                    })
                    .unwrap_or(Some(false))
            } else if list.path.is_ident("any") {
                values
                    .try_fold(Some(false), |any, value| match (any, value) {
                        (_, Some(true)) => Err(()),
                        (Some(false), Some(false)) => Ok(Some(false)),
                        _ => Ok(None),
                    })
                    .unwrap_or(Some(true))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn cfg_is_off(attrs: &[syn::Attribute], features: &BTreeSet<String>) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<syn::Meta>()
                .is_ok_and(|meta| cfg_value(&meta, features) == Some(false))
    })
}

/// Byte ranges of the `cfg` attributes that are on whatever else holds, given the
/// features that are on: with or without one of them the build is the same.
pub(crate) fn settled_cfg_attributes(
    source: &str,
    file: &syn::File,
    features: &BTreeSet<String>,
) -> Vec<(usize, usize)> {
    struct Settled<'a> {
        source: &'a str,
        features: &'a BTreeSet<String>,
        found: Vec<(usize, usize)>,
    }
    impl<'ast> Visit<'ast> for Settled<'_> {
        fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
            let range = attr.span().byte_range();
            if attr.path().is_ident("cfg")
                && attr
                    .parse_args::<syn::Meta>()
                    .is_ok_and(|meta| cfg_value(&meta, self.features) == Some(true))
                && self.source.get(range.clone()).is_some()
            {
                self.found.push((range.start, range.end));
            }
        }
    }
    let mut settled = Settled {
        source,
        features,
        found: vec![],
    };
    settled.visit_file(file);
    settled.found
}

/// Byte ranges of the code that the features that are on leave out of the build: what is
/// under a `cfg` that is off whatever the target is. Its mutants would change nothing.
pub(crate) fn inactive_ranges(
    source: &str,
    file: &syn::File,
    features: &BTreeSet<String>,
) -> Vec<(usize, usize)> {
    struct Inactive<'a> {
        source: &'a str,
        features: &'a BTreeSet<String>,
        found: Vec<(usize, usize)>,
        /// The file defines a `vec!` of its own, which may do anything.
        own_vec: bool,
    }
    impl Inactive<'_> {
        /// Whether the node is left out; then its range is noted and it is not entered.
        fn off(&mut self, attrs: &[syn::Attribute], span: Span) -> bool {
            if !cfg_is_off(attrs, self.features) {
                return false;
            }
            let range = span.byte_range();
            if self.source.get(range.clone()).is_some() {
                self.found.push((range.start, range.end));
            }
            true
        }
    }
    macro_rules! nodes {
        ($($visit:ident, $default:path, $node:ty;)*) => {
            impl<'ast> Visit<'ast> for Inactive<'_> {
                $(fn $visit(&mut self, node: &'ast $node) {
                    if !self.off(&node.attrs, node.span()) {
                        $default(self, node);
                    }
                })*
                fn visit_item(&mut self, node: &'ast syn::Item) {
                    if !self.off(item_attrs(node), node.span()) {
                        visit::visit_item(self, node);
                    }
                }
                fn visit_impl_item(&mut self, node: &'ast syn::ImplItem) {
                    let attrs = match node {
                        syn::ImplItem::Const(item) => &item.attrs[..],
                        syn::ImplItem::Fn(item) => &item.attrs,
                        syn::ImplItem::Type(item) => &item.attrs,
                        syn::ImplItem::Macro(item) => &item.attrs,
                        _ => &[],
                    };
                    if !self.off(attrs, node.span()) {
                        visit::visit_impl_item(self, node);
                    }
                }
                fn visit_foreign_item(&mut self, node: &'ast syn::ForeignItem) {
                    let attrs = match node {
                        syn::ForeignItem::Fn(item) => &item.attrs[..],
                        syn::ForeignItem::Static(item) => &item.attrs,
                        syn::ForeignItem::Type(item) => &item.attrs,
                        syn::ForeignItem::Macro(item) => &item.attrs,
                        _ => &[],
                    };
                    if !self.off(attrs, node.span()) {
                        visit::visit_foreign_item(self, node);
                    }
                }
                // The elements of `vec![a, b]` are those of an array: one under a
                // `cfg` that is off is left out of it.
                fn visit_macro(&mut self, node: &'ast syn::Macro) {
                    type Args = syn::punctuated::Punctuated<syn::Expr, syn::Token![,]>;
                    if node.path.segments.last().is_some_and(|name| name.ident == "vec")
                        && !self.own_vec
                        && let Ok(args) = node.parse_body_with(Args::parse_terminated)
                    {
                        for arg in &args {
                            self.visit_expr(arg);
                        }
                    }
                }
                fn visit_trait_item(&mut self, node: &'ast syn::TraitItem) {
                    let attrs = match node {
                        syn::TraitItem::Const(item) => &item.attrs[..],
                        syn::TraitItem::Fn(item) => &item.attrs,
                        syn::TraitItem::Type(item) => &item.attrs,
                        syn::TraitItem::Macro(item) => &item.attrs,
                        _ => &[],
                    };
                    if !self.off(attrs, node.span()) {
                        visit::visit_trait_item(self, node);
                    }
                }
            }
        };
    }
    nodes! {
        visit_local, visit::visit_local, syn::Local;
        visit_arm, visit::visit_arm, syn::Arm;
        visit_field_value, visit::visit_field_value, syn::FieldValue;
        visit_variant, visit::visit_variant, syn::Variant;
        visit_field, visit::visit_field, syn::Field;
        visit_stmt_macro, visit::visit_stmt_macro, syn::StmtMacro;
        visit_expr_block, visit::visit_expr_block, syn::ExprBlock;
        visit_expr_if, visit::visit_expr_if, syn::ExprIf;
        visit_expr_call, visit::visit_expr_call, syn::ExprCall;
        visit_expr_method_call, visit::visit_expr_method_call, syn::ExprMethodCall;
        visit_expr_macro, visit::visit_expr_macro, syn::ExprMacro;
        visit_expr_match, visit::visit_expr_match, syn::ExprMatch;
        visit_expr_assign, visit::visit_expr_assign, syn::ExprAssign;
        visit_expr_return, visit::visit_expr_return, syn::ExprReturn;
        visit_expr_for_loop, visit::visit_expr_for_loop, syn::ExprForLoop;
        visit_expr_while, visit::visit_expr_while, syn::ExprWhile;
        visit_expr_loop, visit::visit_expr_loop, syn::ExprLoop;
        visit_expr_unsafe, visit::visit_expr_unsafe, syn::ExprUnsafe;
        visit_expr_binary, visit::visit_expr_binary, syn::ExprBinary;
        visit_expr_lit, visit::visit_expr_lit, syn::ExprLit;
        visit_expr_path, visit::visit_expr_path, syn::ExprPath;
        visit_expr_array, visit::visit_expr_array, syn::ExprArray;
        visit_expr_tuple, visit::visit_expr_tuple, syn::ExprTuple;
        visit_expr_reference, visit::visit_expr_reference, syn::ExprReference;
        visit_expr_unary, visit::visit_expr_unary, syn::ExprUnary;
        visit_expr_field, visit::visit_expr_field, syn::ExprField;
        visit_expr_index, visit::visit_expr_index, syn::ExprIndex;
        visit_expr_cast, visit::visit_expr_cast, syn::ExprCast;
        visit_expr_closure, visit::visit_expr_closure, syn::ExprClosure;
        visit_expr_struct, visit::visit_expr_struct, syn::ExprStruct;
        visit_expr_repeat, visit::visit_expr_repeat, syn::ExprRepeat;
        visit_expr_range, visit::visit_expr_range, syn::ExprRange;
        visit_expr_paren, visit::visit_expr_paren, syn::ExprParen;
        visit_expr_try, visit::visit_expr_try, syn::ExprTry;
        visit_expr_await, visit::visit_expr_await, syn::ExprAwait;
        visit_expr_break, visit::visit_expr_break, syn::ExprBreak;
        visit_expr_continue, visit::visit_expr_continue, syn::ExprContinue;
        visit_expr_let, visit::visit_expr_let, syn::ExprLet;
        visit_expr_async, visit::visit_expr_async, syn::ExprAsync;
        visit_expr_const, visit::visit_expr_const, syn::ExprConst;
        visit_expr_yield, visit::visit_expr_yield, syn::ExprYield;
        visit_expr_try_block, visit::visit_expr_try_block, syn::ExprTryBlock;
        visit_expr_raw_addr, visit::visit_expr_raw_addr, syn::ExprRawAddr;
    }
    if cfg_is_off(&file.attrs, features) {
        return vec![(0, source.len())];
    }
    let mut inactive = Inactive {
        source,
        features,
        found: vec![],
        // Anywhere in it: in a module, in a function.
        own_vec: source.split("macro_rules").skip(1).any(|after| {
            after
                .trim_start()
                .strip_prefix('!')
                .is_some_and(|name| name.trim_start().starts_with("vec"))
        }),
    };
    inactive.visit_file(file);
    inactive.found
}

/// What an integer expression can be, as far as its last operation with a literal says:
/// a multiple (`x * 8`, `x << 3`), the bits of a mask (`x & 0xF0`), a remainder
/// (`x % 10`). Only what also holds when the arithmetic wraps: a product by a number
/// that is not a power of two can then be anything.
pub(crate) fn reach(expr: &str) -> Option<(crate::plan::Reach, Option<u32>)> {
    use crate::plan::Reach;
    fn literal(expr: &syn::Expr) -> Option<i128> {
        match expr {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Int(integer),
                ..
            }) => integer.base10_parse().ok(),
            syn::Expr::Paren(inner) => literal(&inner.expr),
            _ => None,
        }
    }
    let mut expr = &syn::parse_str::<syn::Expr>(expr).ok()?;
    while let syn::Expr::Paren(inner) = expr {
        expr = &inner.expr;
    }
    let syn::Expr::Binary(binary) = expr else {
        return None;
    };
    let (left, right) = (literal(&binary.left), literal(&binary.right));
    let power_of_two = |k: i128| k >= 2 && k & (k - 1) == 0;
    // With the number of bits of a shift: it is a multiple only when the type is wider.
    let reach = match binary.op {
        syn::BinOp::Mul(_) => left
            .or(right)
            .filter(|k| power_of_two(*k))
            .map(Reach::Multiples),
        syn::BinOp::Shl(_) => {
            let bits = right.filter(|bits| (1..=100).contains(bits))?;
            return Some((Reach::Multiples(1 << bits), Some(bits as u32)));
        }
        syn::BinOp::BitAnd(_) => left.or(right).filter(|mask| *mask >= 0).map(Reach::Bits),
        syn::BinOp::Rem(_) => right.filter(|limit| *limit >= 1).map(Reach::Below),
        _ => None,
    };
    reach.map(|reach| (reach, None))
}

/// Bit width and signedness of a primitive integer type from its inclusive `[min, max]` bounds.
fn int_type_info(bounds: (i128, i128)) -> Option<(u32, bool)> {
    let (lo, hi) = bounds;
    if lo == 0 && hi > 0 {
        let hi_u = hi as u128;
        let width = hi_u.trailing_ones();
        let expected = if width == 128 {
            u128::MAX
        } else {
            (1u128 << width) - 1
        };
        (width > 0 && hi_u == expected).then_some((width, false))
    } else if lo < 0 && hi > 0 {
        let hi_u = hi as u128;
        let ones = hi_u.trailing_ones();
        let width = ones + 1;
        let expected_hi = (1u128 << ones) - 1;
        // Two's complement: `iN::MIN == !iN::MAX`, avoiding `hi + 1` overflow on `i128::MAX`.
        (width <= 128 && hi_u == expected_hi && lo == !hi).then_some((width, true))
    } else {
        None
    }
}

fn type_mask(width: u32) -> u128 {
    if width >= 128 {
        u128::MAX
    } else {
        (1u128 << width) - 1
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CastTarget {
    Fixed(BitValueType),
    PtrSized,
}

pub(super) fn is_prim_type_name(ident: &str) -> bool {
    parse_prim_ident(ident).is_some()
}

fn parse_prim_ident(ident: &str) -> Option<CastTarget> {
    Some(match ident {
        "u8" => CastTarget::Fixed(BitValueType::Unsigned(8)),
        "u16" => CastTarget::Fixed(BitValueType::Unsigned(16)),
        "u32" => CastTarget::Fixed(BitValueType::Unsigned(32)),
        "u64" => CastTarget::Fixed(BitValueType::Unsigned(64)),
        "u128" => CastTarget::Fixed(BitValueType::Unsigned(128)),
        "i8" => CastTarget::Fixed(BitValueType::Signed(8)),
        "i16" => CastTarget::Fixed(BitValueType::Signed(16)),
        "i32" => CastTarget::Fixed(BitValueType::Signed(32)),
        "i64" => CastTarget::Fixed(BitValueType::Signed(64)),
        "i128" => CastTarget::Fixed(BitValueType::Signed(128)),
        "bool" => CastTarget::Fixed(BitValueType::Bool),
        "char" => CastTarget::Fixed(BitValueType::Char),
        "usize" | "isize" => CastTarget::PtrSized,
        _ => return None,
    })
}

fn parse_cast_target(ty: &syn::Type, shadowed_prims: &[String]) -> Option<CastTarget> {
    match ty {
        syn::Type::Paren(inner) => parse_cast_target(&inner.elem, shadowed_prims),
        syn::Type::Group(inner) => parse_cast_target(&inner.elem, shadowed_prims),
        syn::Type::Path(path) if path.qself.is_none() => {
            let segs: Vec<&syn::PathSegment> = path.path.segments.iter().collect();
            if !segs.iter().all(|s| s.arguments.is_none()) {
                return None;
            }
            match segs.as_slice() {
                [only] => {
                    let name = only.ident.to_string();
                    if shadowed_prims.iter().any(|s| s == &name) {
                        return None;
                    }
                    parse_prim_ident(&name)
                }
                [root, prim, name]
                    if (root.ident == "core" || root.ident == "std")
                        && prim.ident == "primitive"
                        && (path.path.leading_colon.is_some()
                            || !shadowed_prims.iter().any(|s| s == &root.ident.to_string())) =>
                {
                    parse_prim_ident(&name.ident.to_string())
                }
                _ => None,
            }
        }
        _ => None,
    }
}

pub(super) fn parse_bit_value_type(
    ty: &syn::Type,
    shadowed_prims: &[String],
) -> Option<BitValueType> {
    match parse_cast_target(ty, shadowed_prims) {
        Some(CastTarget::Fixed(t)) => Some(t),
        _ => None,
    }
}

pub(super) fn parse_local_bit_type(
    ty: &syn::Type,
    shadowed_prims: &[String],
) -> Option<LocalBitType> {
    match ty {
        syn::Type::Paren(inner) => parse_local_bit_type(&inner.elem, shadowed_prims),
        syn::Type::Group(inner) => parse_local_bit_type(&inner.elem, shadowed_prims),
        syn::Type::Reference(reference) => parse_local_bit_type(&reference.elem, shadowed_prims),
        syn::Type::Slice(slice) => {
            parse_bit_value_type(&slice.elem, shadowed_prims).map(LocalBitType::SliceOf)
        }
        syn::Type::Array(array) => {
            parse_bit_value_type(&array.elem, shadowed_prims).map(LocalBitType::SliceOf)
        }
        _ => parse_bit_value_type(ty, shadowed_prims).map(LocalBitType::Value),
    }
}

fn parse_prim_from_call<'a>(
    call: &'a syn::ExprCall,
    shadowed_prims: &[String],
) -> Option<(CastTarget, &'a syn::Expr)> {
    if call.args.len() != 1 {
        return None;
    }
    let syn::Expr::Path(path) = &*call.func else {
        return None;
    };
    if path.qself.is_some() {
        return None;
    }
    let segs: Vec<&syn::PathSegment> = path.path.segments.iter().collect();
    if !segs.iter().all(|s| s.arguments.is_none()) {
        return None;
    }
    let target = match segs.as_slice() {
        [ty, method] if method.ident == "from" => {
            let name = ty.ident.to_string();
            if shadowed_prims.iter().any(|s| s == &name) {
                return None;
            }
            parse_prim_ident(&name)?
        }
        [root, prim, ty, method]
            if (root.ident == "core" || root.ident == "std")
                && prim.ident == "primitive"
                && method.ident == "from"
                && (path.path.leading_colon.is_some()
                    || !shadowed_prims.iter().any(|s| s == &root.ident.to_string())) =>
        {
            parse_prim_ident(&ty.ident.to_string())?
        }
        _ => return None,
    };
    Some((target, call.args.first()?))
}

fn single_ident(expr: &syn::Expr) -> Option<String> {
    match expr {
        syn::Expr::Paren(inner) => single_ident(&inner.expr),
        syn::Expr::Group(inner) => single_ident(&inner.expr),
        syn::Expr::Path(path) if path.qself.is_none() => {
            path.path.get_ident().map(ToString::to_string)
        }
        _ => None,
    }
}

fn lookup_local_type(name: &str, local_types: &[(String, LocalBitType)]) -> Option<LocalBitType> {
    local_types
        .iter()
        .find(|(k, _)| k == name)
        .map(|(_, ty)| *ty)
}

pub(super) fn infer_bit_value_type(
    expr: &syn::Expr,
    local_types: &[(String, LocalBitType)],
    shadowed_prims: &[String],
) -> Option<BitValueType> {
    match expr {
        syn::Expr::Paren(inner) => infer_bit_value_type(&inner.expr, local_types, shadowed_prims),
        syn::Expr::Group(inner) => infer_bit_value_type(&inner.expr, local_types, shadowed_prims),
        syn::Expr::Unary(syn::ExprUnary {
            op: syn::UnOp::Deref(_),
            expr,
            ..
        }) => infer_bit_value_type(expr, local_types, shadowed_prims),
        syn::Expr::Path(_) => {
            let name = single_ident(expr)?;
            match lookup_local_type(&name, local_types) {
                Some(LocalBitType::Value(ty)) => Some(ty),
                _ => None,
            }
        }
        syn::Expr::Index(index) => {
            if matches!(*index.index, syn::Expr::Range(_)) {
                return None;
            }
            let name = single_ident(&index.expr)?;
            match lookup_local_type(&name, local_types) {
                Some(LocalBitType::SliceOf(ty)) => Some(ty),
                _ => None,
            }
        }
        syn::Expr::Cast(cast) => parse_bit_value_type(&cast.ty, shadowed_prims),
        syn::Expr::Call(call) => match parse_prim_from_call(call, shadowed_prims) {
            Some((CastTarget::Fixed(ty), _)) => Some(ty),
            _ => None,
        },
        syn::Expr::Lit(syn::ExprLit { lit, .. }) => match lit {
            syn::Lit::Byte(_) => Some(BitValueType::Unsigned(8)),
            syn::Lit::Bool(_) => Some(BitValueType::Bool),
            syn::Lit::Char(_) => Some(BitValueType::Char),
            syn::Lit::Int(integer) => match parse_prim_ident(integer.suffix()) {
                Some(CastTarget::Fixed(ty)) => Some(ty),
                _ => None,
            },
            _ => None,
        },
        // Do not infer binary expression result types from operands: overloaded operator
        // traits can have an arbitrary `Output` type wider than either operand.
        _ => None,
    }
}

fn bit_value_mask(ty: BitValueType) -> u128 {
    match ty {
        BitValueType::Unsigned(w) | BitValueType::Signed(w) => type_mask(w),
        BitValueType::Bool => 0x1,
        BitValueType::Char => 0x1f_ffff,
    }
}

fn cast_possible_ones(
    inner: &syn::Expr,
    target: CastTarget,
    outer_width: u32,
    base_offset: usize,
    is_primitive: &dyn Fn(usize, usize) -> bool,
    local_types: &[(String, LocalBitType)],
    shadowed_prims: &[String],
) -> u128 {
    let outer_full = type_mask(outer_width);
    let src_ty = infer_bit_value_type(inner, local_types, shadowed_prims);
    match target {
        CastTarget::Fixed(dst_ty) => {
            let dst_mask = bit_value_mask(dst_ty) & outer_full;
            match src_ty {
                Some(BitValueType::Unsigned(src_w)) => {
                    possible_ones(
                        inner,
                        src_w,
                        false,
                        base_offset,
                        is_primitive,
                        local_types,
                        shadowed_prims,
                    ) & dst_mask
                }
                Some(BitValueType::Bool) => 0x1 & dst_mask,
                Some(BitValueType::Char) => 0x1f_ffff & dst_mask,
                Some(BitValueType::Signed(src_w)) => {
                    let src_ones = possible_ones(
                        inner,
                        src_w,
                        true,
                        base_offset,
                        is_primitive,
                        local_types,
                        shadowed_prims,
                    );
                    let sign_bit = 1u128 << (src_w - 1);
                    if dst_ty.width() <= src_w || (src_ones & sign_bit) == 0 {
                        src_ones & dst_mask
                    } else {
                        // Signed widening sign-extends negative values into all upper bits,
                        // while preserving guaranteed-zero lower bits of `src_ones`.
                        let ext_mask = dst_mask & !type_mask(src_w);
                        (src_ones | ext_mask) & dst_mask
                    }
                }
                None => match dst_ty {
                    BitValueType::Unsigned(_) | BitValueType::Bool | BitValueType::Char => dst_mask,
                    BitValueType::Signed(_) => outer_full,
                },
            }
        }
        CastTarget::PtrSized => match src_ty {
            Some(BitValueType::Unsigned(src_w)) if src_w <= outer_width => {
                possible_ones(
                    inner,
                    src_w,
                    false,
                    base_offset,
                    is_primitive,
                    local_types,
                    shadowed_prims,
                ) & outer_full
            }
            Some(BitValueType::Bool) => 0x1 & outer_full,
            Some(BitValueType::Char) if outer_width >= 21 => 0x1f_ffff & outer_full,
            Some(BitValueType::Signed(src_w)) if src_w <= outer_width => {
                let src_ones = possible_ones(
                    inner,
                    src_w,
                    true,
                    base_offset,
                    is_primitive,
                    local_types,
                    shadowed_prims,
                );
                let sign_bit = 1u128 << (src_w - 1);
                if (src_ones & sign_bit) == 0 {
                    src_ones & outer_full
                } else {
                    let ext_mask = outer_full & !type_mask(src_w);
                    (src_ones | ext_mask) & outer_full
                }
            }
            _ => outer_full,
        },
    }
}

fn nonneg_int_literal(expr: &syn::Expr) -> Option<u128> {
    match expr {
        syn::Expr::Paren(inner) => nonneg_int_literal(&inner.expr),
        syn::Expr::Group(inner) => nonneg_int_literal(&inner.expr),
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Int(integer),
            ..
        }) => integer.base10_parse().ok(),
        _ => None,
    }
}

/// Sound upper bound on the set of bits that may be `1` in `expr` when evaluated at
/// primitive integer `(width, signed)`. Any bit that is `0` in the returned mask is
/// guaranteed to be `0` in every evaluation that does not overflow.
///
/// `is_primitive` verifies via runtime trace that a binary sub-expression at
/// `[base_offset + start, base_offset + end)` actually executed as a primitive integer
/// operator rather than a user-defined trait implementation.
fn possible_ones(
    expr: &syn::Expr,
    width: u32,
    signed: bool,
    base_offset: usize,
    is_primitive: &dyn Fn(usize, usize) -> bool,
    local_types: &[(String, LocalBitType)],
    shadowed_prims: &[String],
) -> u128 {
    let full = type_mask(width);
    match expr {
        syn::Expr::Paren(inner) => possible_ones(
            &inner.expr,
            width,
            signed,
            base_offset,
            is_primitive,
            local_types,
            shadowed_prims,
        ),
        syn::Expr::Group(inner) => possible_ones(
            &inner.expr,
            width,
            signed,
            base_offset,
            is_primitive,
            local_types,
            shadowed_prims,
        ),
        syn::Expr::Lit(syn::ExprLit { lit, .. }) => match lit {
            syn::Lit::Int(integer) => integer
                .base10_parse::<u128>()
                .ok()
                .filter(|v| *v <= full)
                .unwrap_or(full),
            syn::Lit::Byte(b) => (b.value() as u128) & full,
            syn::Lit::Bool(b) => u128::from(b.value) & full,
            syn::Lit::Char(c) => (c.value() as u32 as u128) & full,
            _ => full,
        },
        syn::Expr::Cast(cast) => match parse_cast_target(&cast.ty, shadowed_prims) {
            Some(target) => cast_possible_ones(
                &cast.expr,
                target,
                width,
                base_offset,
                is_primitive,
                local_types,
                shadowed_prims,
            ),
            None => full,
        },
        syn::Expr::Call(call) => match parse_prim_from_call(call, shadowed_prims) {
            Some((target, arg)) => cast_possible_ones(
                arg,
                target,
                width,
                base_offset,
                is_primitive,
                local_types,
                shadowed_prims,
            ),
            None => full,
        },
        syn::Expr::Binary(binary) => {
            let range = binary.span().byte_range();
            if !is_primitive(base_offset + range.start, base_offset + range.end) {
                return full;
            }
            match binary.op {
                syn::BinOp::BitAnd(_) => {
                    possible_ones(
                        &binary.left,
                        width,
                        signed,
                        base_offset,
                        is_primitive,
                        local_types,
                        shadowed_prims,
                    ) & possible_ones(
                        &binary.right,
                        width,
                        signed,
                        base_offset,
                        is_primitive,
                        local_types,
                        shadowed_prims,
                    )
                }
                syn::BinOp::BitOr(_) | syn::BinOp::BitXor(_) => {
                    (possible_ones(
                        &binary.left,
                        width,
                        signed,
                        base_offset,
                        is_primitive,
                        local_types,
                        shadowed_prims,
                    ) | possible_ones(
                        &binary.right,
                        width,
                        signed,
                        base_offset,
                        is_primitive,
                        local_types,
                        shadowed_prims,
                    )) & full
                }
                syn::BinOp::Shl(_) => match nonneg_int_literal(&binary.right) {
                    Some(k) if k < width as u128 => {
                        (possible_ones(
                            &binary.left,
                            width,
                            signed,
                            base_offset,
                            is_primitive,
                            local_types,
                            shadowed_prims,
                        ) << k)
                            & full
                    }
                    _ => full,
                },
                syn::BinOp::Shr(_) => {
                    if !signed {
                        match nonneg_int_literal(&binary.right) {
                            Some(k) if k < width as u128 => {
                                possible_ones(
                                    &binary.left,
                                    width,
                                    false,
                                    base_offset,
                                    is_primitive,
                                    local_types,
                                    shadowed_prims,
                                ) >> k
                            }
                            _ => full,
                        }
                    } else {
                        // Arithmetic right shift sign-extends negative values.
                        full
                    }
                }
                syn::BinOp::Rem(_) => {
                    if !signed {
                        match nonneg_int_literal(&binary.right) {
                            Some(k) if k >= 1 && k <= full => {
                                let max_val = k - 1;
                                let bits = 128 - max_val.leading_zeros();
                                type_mask(bits) & full
                            }
                            _ => full,
                        }
                    } else {
                        full
                    }
                }
                syn::BinOp::Div(_) => {
                    if !signed {
                        match nonneg_int_literal(&binary.right) {
                            Some(k) if k >= 2 && k.is_power_of_two() => {
                                let shift = k.trailing_zeros();
                                if shift < width {
                                    possible_ones(
                                        &binary.left,
                                        width,
                                        false,
                                        base_offset,
                                        is_primitive,
                                        local_types,
                                        shadowed_prims,
                                    ) >> shift
                                } else {
                                    full
                                }
                            }
                            _ => full,
                        }
                    } else {
                        full
                    }
                }
                syn::BinOp::Mul(_) => {
                    let factor = nonneg_int_literal(&binary.right)
                        .map(|k| (k, &binary.left))
                        .or_else(|| nonneg_int_literal(&binary.left).map(|k| (k, &binary.right)));
                    match factor {
                        Some((k, other)) if k >= 2 && k.is_power_of_two() => {
                            let shift = k.trailing_zeros();
                            if shift < width {
                                (possible_ones(
                                    other,
                                    width,
                                    signed,
                                    base_offset,
                                    is_primitive,
                                    local_types,
                                    shadowed_prims,
                                ) << shift)
                                    & full
                            } else {
                                full
                            }
                        }
                        _ => full,
                    }
                }
                _ => full,
            }
        }
        _ => full,
    }
}

/// Whether `lhs` and `rhs` can be statically proved to have disjoint `1` bits, making
/// `lhs | rhs` and `lhs ^ rhs` equivalent for every input.
///
/// `lhs_start` and `rhs_start` are the byte offsets of `lhs` and `rhs` in the original
/// source file; `is_primitive` checks whether a binary sub-expression in that source range
/// was observed executing as a primitive integer operator.
#[allow(clippy::too_many_arguments)]
pub(crate) fn disjoint_bits(
    lhs: &str,
    lhs_start: usize,
    rhs: &str,
    rhs_start: usize,
    bounds: Option<(i128, i128)>,
    is_primitive: &dyn Fn(usize, usize) -> bool,
    local_types: &[(String, LocalBitType)],
    shadowed_prims: &[String],
) -> bool {
    let Some((width, signed)) = bounds.and_then(int_type_info) else {
        return false;
    };
    let (Ok(lhs_expr), Ok(rhs_expr)) = (
        syn::parse_str::<syn::Expr>(lhs),
        syn::parse_str::<syn::Expr>(rhs),
    ) else {
        return false;
    };
    let lhs_ones = possible_ones(
        &lhs_expr,
        width,
        signed,
        lhs_start,
        is_primitive,
        local_types,
        shadowed_prims,
    );
    let rhs_ones = possible_ones(
        &rhs_expr,
        width,
        signed,
        rhs_start,
        is_primitive,
        local_types,
        shadowed_prims,
    );
    (lhs_ones & rhs_ones) == 0
}

/// Whether a boolean right-hand side expression is syntactically side-effect-free (no
/// blocks, assignments, mutable borrows, arbitrary function calls, or early exits), so
/// skipping its evaluation under `&&` <-> `||` cannot hide an observable side effect.
pub(crate) fn is_pure_logic_operand(expr_text: &str) -> bool {
    let Ok(expr) = syn::parse_str::<syn::Expr>(expr_text) else {
        return false;
    };
    is_pure_expr(&expr)
}

fn is_pure_expr(expr: &syn::Expr) -> bool {
    const PURE_METHODS: &[&str] = &[
        "len",
        "is_empty",
        "starts_with",
        "ends_with",
        "contains",
        "eq",
        "ne",
        "eq_ignore_ascii_case",
        "is_some",
        "is_none",
        "is_ok",
        "is_err",
        "is_ascii",
        "is_ascii_alphabetic",
        "is_ascii_alphanumeric",
        "is_ascii_digit",
        "is_ascii_hexdigit",
        "is_ascii_lowercase",
        "is_ascii_uppercase",
        "is_ascii_whitespace",
        "is_alphabetic",
        "is_alphanumeric",
        "is_numeric",
        "is_whitespace",
        "is_finite",
        "is_nan",
        "is_infinite",
        "is_sign_positive",
        "is_sign_negative",
    ];
    match expr {
        syn::Expr::Lit(_) | syn::Expr::Path(_) => true,
        syn::Expr::Paren(e) => is_pure_expr(&e.expr),
        syn::Expr::Group(e) => is_pure_expr(&e.expr),
        syn::Expr::Field(e) => is_pure_expr(&e.base),
        syn::Expr::Index(e) => is_pure_expr(&e.expr) && is_pure_expr(&e.index),
        syn::Expr::Cast(e) => is_pure_expr(&e.expr),
        syn::Expr::Reference(e) => e.mutability.is_none() && is_pure_expr(&e.expr),
        syn::Expr::Unary(e) => is_pure_expr(&e.expr),
        syn::Expr::Binary(e) => {
            !matches!(
                e.op,
                syn::BinOp::AddAssign(_)
                    | syn::BinOp::SubAssign(_)
                    | syn::BinOp::MulAssign(_)
                    | syn::BinOp::DivAssign(_)
                    | syn::BinOp::RemAssign(_)
                    | syn::BinOp::BitXorAssign(_)
                    | syn::BinOp::BitAndAssign(_)
                    | syn::BinOp::BitOrAssign(_)
                    | syn::BinOp::ShlAssign(_)
                    | syn::BinOp::ShrAssign(_)
            ) && is_pure_expr(&e.left)
                && is_pure_expr(&e.right)
        }
        syn::Expr::Tuple(e) => e.elems.iter().all(is_pure_expr),
        syn::Expr::Array(e) => e.elems.iter().all(is_pure_expr),
        syn::Expr::MethodCall(e) => {
            PURE_METHODS.contains(&e.method.to_string().as_str())
                && is_pure_expr(&e.receiver)
                && e.args.iter().all(is_pure_expr)
        }
        _ => false,
    }
}

/// Byte ranges of the code that only a test build has: what is under `cfg(test)` or
/// marked as a test.
pub(crate) fn test_only_ranges(source: &str, file: &syn::File) -> Vec<(usize, usize)> {
    if is_test_only(&file.attrs) {
        return vec![(0, source.len())];
    }
    let mut found = vec![];
    for item in &file.items {
        test_only_items(source, item, &mut found);
    }
    found
}

fn test_only_items(source: &str, item: &syn::Item, found: &mut Vec<(usize, usize)>) {
    let mut note = |attrs: &[syn::Attribute], span: Span| {
        let test_only = is_test_only(attrs);
        let range = span.byte_range();
        if test_only && source.get(range.clone()).is_some() {
            found.push((range.start, range.end));
        }
        test_only
    };
    if note(item_attrs(item), item.span()) {
        return;
    }
    match item {
        syn::Item::Mod(module) => {
            for inner in module.content.iter().flat_map(|(_, items)| items) {
                test_only_items(source, inner, found);
            }
        }
        syn::Item::Impl(block) => {
            for inner in &block.items {
                if let syn::ImplItem::Fn(function) = inner {
                    note(&function.attrs, inner.span());
                }
            }
        }
        _ => {}
    }
}

fn item_attrs(item: &syn::Item) -> &[syn::Attribute] {
    match item {
        syn::Item::Const(item) => &item.attrs,
        syn::Item::Enum(item) => &item.attrs,
        syn::Item::ExternCrate(item) => &item.attrs,
        syn::Item::Fn(item) => &item.attrs,
        syn::Item::ForeignMod(item) => &item.attrs,
        syn::Item::Impl(item) => &item.attrs,
        syn::Item::Macro(item) => &item.attrs,
        syn::Item::Mod(item) => &item.attrs,
        syn::Item::Static(item) => &item.attrs,
        syn::Item::Struct(item) => &item.attrs,
        syn::Item::Trait(item) => &item.attrs,
        syn::Item::TraitAlias(item) => &item.attrs,
        syn::Item::Type(item) => &item.attrs,
        syn::Item::Union(item) => &item.attrs,
        syn::Item::Use(item) => &item.attrs,
        _ => &[],
    }
}

/// Detect test attributes and cfg predicates that require `test` to be true.
pub(crate) fn is_test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        let path = attr.path();
        if path.segments.last().is_some_and(|s| s.ident == "test") {
            return true;
        }
        path.is_ident("cfg")
            && attr
                .parse_args::<syn::Meta>()
                .is_ok_and(|meta| !cfg_possible_without_test(&meta, true))
    })
}
