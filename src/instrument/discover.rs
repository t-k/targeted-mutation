//! AST discovery of mutation sites.

use std::collections::HashSet;

use anyhow::{Context, Result};
use proc_macro2::{Span, TokenStream, TokenTree};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use crate::plan::CmpOp;

use super::body_values::{
    body_values_with_errors, does_nothing_else, may_bypass_result_probe, tokens_of,
};
use super::cfg::{is_skipped, is_test_only};
use super::model::{
    ArithOp, ArmGuard, Discovery, LiteralSide, NO_ID, RoundingOp, Site, SiteKind, TestModule,
};

/// Finds the sites in one source file, numbering them from `first_id`.
pub fn discover(source: &str, first_id: u32) -> Result<Discovery> {
    discover_with_errors(source, first_id, &[])
}

pub(crate) fn discover_with_errors(
    source: &str,
    first_id: u32,
    error_values: &[&str],
) -> Result<Discovery> {
    let file = parse_source(source)?;
    Ok(discover_parsed(source, &file, first_id, error_values))
}

pub(crate) fn parse_source(source: &str) -> Result<syn::File> {
    // The parser drops a byte order mark and a `#!` line before it numbers bytes, so its
    // offsets would not be offsets into `source`.
    if source.starts_with('\u{feff}') || (source.starts_with("#!") && !source.starts_with("#![")) {
        anyhow::bail!("starts with a byte order mark or a `#!` line");
    }
    syn::parse_file(source).context("not parseable as a Rust source file")
}

pub(crate) fn discover_parsed(
    source: &str,
    file: &syn::File,
    first_id: u32,
    error_values: &[&str],
) -> Discovery {
    // A macro defined here under the name of a standard one (`macro_rules! vec`) can do
    // anything with its arguments.
    let local_macros = file
        .items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Macro(m) => m.ident.as_ref().map(|i| i.to_string()),
            _ => None,
        })
        .collect();
    let mut finder = Finder {
        source,
        next_id: first_id,
        found: Discovery::default(),
        local_macros,
        const_depth: 0,
        error_values,
        for_ranges: std::collections::HashSet::new(),
    };
    finder.visit_file(file);
    finder.found
}

struct Finder<'s> {
    error_values: &'s [&'s str],
    /// Names of the macros this file defines at its top level.
    local_macros: Vec<String>,
    source: &'s str,
    next_id: u32,
    found: Discovery,
    /// How many `const fn` the visitor is inside of.
    const_depth: usize,
    /// Where the ranges start that a `for` runs through directly.
    for_ranges: std::collections::HashSet<usize>,
}

impl Finder<'_> {
    /// Registers the sites of a function as a whole: its result (for `bool` and
    /// `Option<T>`), and its body replaced by each value its return type suggests.
    fn function_sites(
        &mut self,
        attrs: &[syn::Attribute],
        sig: &syn::Signature,
        body: &syn::Block,
    ) {
        if body.stmts.is_empty() {
            return;
        }
        let braces = body.brace_token.span;
        let (Some((_, mut start)), Some((end, _)), Some((sig_start, sig_end))) = (
            self.range(braces.open()),
            self.range(braces.close()),
            self.range(sig.span()),
        ) else {
            return;
        };
        // Inner attributes (`#![allow(...)]`) must stay at the very start of the body block,
        // before any statement inserted by a Body or Return probe.
        for attr in attrs {
            if matches!(attr.style, syn::AttrStyle::Inner(_))
                && let Some((_, attr_end)) = self.range(attr.span())
            {
                start = start.max(attr_end);
            }
        }
        if !in_order(start, end) {
            return;
        }
        let at = sig.span().start();
        let text = self.source[sig_start..sig_end].to_string();
        let (sole_expr, sole_plain) = match &body.stmts[..] {
            [syn::Stmt::Expr(expr, None)] => {
                let range = self.range(expr.span());
                (range, range.filter(|_| does_nothing_else(expr)))
            }
            _ => (None, None),
        };
        let push = |finder: &mut Self, kind: SiteKind| {
            finder.add(Site {
                id: 0,
                kind,
                line: at.line,
                column: at.column + 1,
                start,
                end,
                text: text.clone(),
            });
        };
        let name = sig.ident.to_string();
        if let Some(option) = self.result_kind(&sig.output)
            && !may_bypass_result_probe(body)
        {
            let kind = SiteKind::Return {
                name: name.clone(),
                option,
                sole_expr,
            };
            push(self, kind);
        }
        // Deleting the only statement of a function that returns nothing is the same
        // mutant as an empty body.
        let one_deletable_statement = matches!(sig.output, syn::ReturnType::Default)
            && matches!(&body.stmts[..], [syn::Stmt::Expr(expr, Some(_))] if deletable(expr));
        if sig.unsafety.is_some() {
            return;
        }
        if one_deletable_statement {
            self.found.single_statement_bodies.push(name);
            return;
        }
        let written = sole_expr.map(|(from, to)| tokens_of(&self.source[from..to]));
        let is_written = |value: &str| {
            written
                .as_deref()
                .is_some_and(|text| text == tokens_of(value))
        };
        let probe_values = body_values_with_errors(&sig.output, self.error_values, false);
        // tmt:ignore performance only: the set answers what `probe_values.contains` answers
        let probe_index = (probe_values.len() > 32).then(|| {
            probe_values
                .iter()
                .map(String::as_str)
                .collect::<HashSet<_>>()
        });
        for value in body_values_with_errors(&sig.output, self.error_values, true) {
            let is_probe = probe_index.as_ref().map_or_else(
                || probe_values.contains(&value),
                |index| index.contains(value.as_str()),
            );
            if !is_probe && !is_written(&value) {
                self.add_rebuild(Site {
                    id: NO_ID,
                    kind: SiteKind::Body {
                        name: name.clone(),
                        value,
                        sole_expr: sole_plain,
                    },
                    line: at.line,
                    column: at.column + 1,
                    start,
                    end,
                    text: text.clone(),
                });
            }
        }
        for value in probe_values {
            if !is_written(&value) {
                push(
                    self,
                    SiteKind::Body {
                        name: name.clone(),
                        value,
                        sole_expr: sole_plain,
                    },
                );
            }
        }
    }

    /// `Some(None)` for a function that returns `bool`, `Some(Some(type))` for one that
    /// returns `Option<T>` written in a way a `let` can repeat.
    fn result_kind(&self, output: &syn::ReturnType) -> Option<Option<String>> {
        let syn::ReturnType::Type(_, ty) = output else {
            return None;
        };
        match &**ty {
            // `<T>::bool` and `<T as Tr>::bool` are not a single identifier either.
            syn::Type::Path(p) if p.path.is_ident("bool") => Some(None),
            syn::Type::Path(p)
                if p.qself.is_none()
                    && p.path.segments.last().is_some_and(|s| {
                        s.ident == "Option"
                            && matches!(s.arguments, syn::PathArguments::AngleBracketed(_))
                    }) =>
            {
                match self
                    .range(ty.span())
                    .map(|(from, to)| &self.source[from..to])
                {
                    // `impl Trait` cannot be written in a `let`.
                    Some(written) if !written.contains("impl ") => Some(Some(written.to_string())),
                    _ => None,
                }
            }
            _ => None,
        }
    }
    fn range(&self, span: Span) -> Option<(usize, usize)> {
        let range = span.byte_range();
        // Spans produced by macro hygiene tricks or empty token streams carry no location.
        // tmt:ignore defensive: every span of a file parsed from `source` lies inside it
        let located = !range.is_empty() && self.source.get(range.clone()).is_some();
        located.then_some((range.start, range.end))
    }

    fn push(&mut self, kind: SiteKind, start: usize, end: usize, span: Span) {
        let at = span.start();
        self.add(Site {
            id: 0,
            kind,
            line: at.line,
            column: at.column + 1,
            start,
            end,
            text: self.source[start..end].to_string(),
        });
    }

    /// The value of a `const` or `static`. The compiler computes it: like the code of a
    /// `const fn`, it is mutated by building.
    fn constant(&mut self, attrs: &[syn::Attribute], value: &syn::Expr) {
        if !is_test_only(attrs) && !is_skipped(attrs) {
            self.const_depth += 1;
            self.visit_expr(value);
            self.const_depth -= 1;
        }
    }

    fn typed_constant(
        &mut self,
        attrs: &[syn::Attribute],
        ty: &syn::Type,
        value: Option<&syn::Expr>,
    ) {
        if !is_test_only(attrs) && !is_skipped(attrs) {
            self.visit_type(ty);
            if let Some(value) = value {
                self.constant(&[], value);
            }
        }
    }

    /// Registers a site: numbered, or among those that need a build of their own.
    fn add_rebuild(&mut self, mut site: Site) {
        site.id = NO_ID;
        self.found.rebuild_sites.push(site);
    }

    fn add(&mut self, mut site: Site) {
        if self.const_depth > 0 {
            self.add_rebuild(site);
        } else {
            site.id = self.next_id;
            self.found.sites.push(site);
            self.next_id += 1;
        }
    }
}

fn cmp_op(op: &syn::BinOp) -> Option<CmpOp> {
    Some(match op {
        syn::BinOp::Lt(_) => CmpOp::Lt,
        syn::BinOp::Le(_) => CmpOp::Le,
        syn::BinOp::Gt(_) => CmpOp::Gt,
        syn::BinOp::Ge(_) => CmpOp::Ge,
        syn::BinOp::Eq(_) => CmpOp::Eq,
        syn::BinOp::Ne(_) => CmpOp::Ne,
        _ => return None,
    })
}

/// The arithmetic or bit operator, and whether it is the compound assignment form.
fn arith_op(op: &syn::BinOp) -> Option<(ArithOp, bool)> {
    use syn::BinOp as B;
    Some(match op {
        B::Add(_) => (ArithOp::Add, false),
        B::Sub(_) => (ArithOp::Sub, false),
        B::Mul(_) => (ArithOp::Mul, false),
        B::Div(_) => (ArithOp::Div, false),
        B::Rem(_) => (ArithOp::Rem, false),
        B::BitAnd(_) => (ArithOp::BitAnd, false),
        B::BitOr(_) => (ArithOp::BitOr, false),
        B::BitXor(_) => (ArithOp::BitXor, false),
        B::Shl(_) => (ArithOp::Shl, false),
        B::Shr(_) => (ArithOp::Shr, false),
        B::AddAssign(_) => (ArithOp::Add, true),
        B::SubAssign(_) => (ArithOp::Sub, true),
        B::MulAssign(_) => (ArithOp::Mul, true),
        B::DivAssign(_) => (ArithOp::Div, true),
        B::RemAssign(_) => (ArithOp::Rem, true),
        B::BitAndAssign(_) => (ArithOp::BitAnd, true),
        B::BitOrAssign(_) => (ArithOp::BitOr, true),
        B::BitXorAssign(_) => (ArithOp::BitXor, true),
        B::ShlAssign(_) => (ArithOp::Shl, true),
        B::ShrAssign(_) => (ArithOp::Shr, true),
        _ => return None,
    })
}

/// A place whose evaluation has no effect and cannot fail: a variable, a field of one, or
/// a dereference of one.
///
/// For numbers `place op= value` evaluates `value` first, for other types `place` first.
/// The probe always takes the place first, which is the same thing only when evaluating
/// the place cannot be observed. Indexing can panic and calls can do anything, so
/// `v[i] += 1` and `f().x += 1` are not instrumented as operators (they can still be
/// deleted as statements).
fn is_plain_place(expr: &syn::Expr) -> bool {
    if has_conditional_or_skip_attr(expr) {
        return false;
    }
    match expr {
        syn::Expr::Path(e) => e.attrs.is_empty(),
        syn::Expr::Field(e) => is_plain_place(&e.base),
        syn::Expr::Unary(syn::ExprUnary {
            op: syn::UnOp::Deref(_),
            expr,
            ..
        }) => is_plain_place(expr),
        syn::Expr::Paren(e) => is_plain_place(&e.expr),
        syn::Expr::Group(e) => is_plain_place(&e.expr),
        _ => false,
    }
}

/// The value of an expression that is the word `true` or `false`, in parentheses or not.
pub(super) fn written_bool(expr: &syn::Expr) -> Option<bool> {
    match expr {
        syn::Expr::Paren(e) => written_bool(&e.expr),
        syn::Expr::Group(e) => written_bool(&e.expr),
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Bool(value),
            ..
        }) => Some(value.value),
        _ => None,
    }
}

fn is_literal(expr: &syn::Expr) -> bool {
    match expr {
        syn::Expr::Paren(e) => is_literal(&e.expr),
        syn::Expr::Group(e) => is_literal(&e.expr),
        syn::Expr::Lit(_) => true,
        syn::Expr::Unary(syn::ExprUnary {
            op: syn::UnOp::Neg(_),
            expr,
            ..
        }) => is_literal(expr),
        _ => false,
    }
}

/// The spans of the parts of one parsed expression come in source order. The edits are
/// built on that, so it is checked rather than assumed; no source makes it false.
// tmt:ignore defensive: never false, so no test can make it return false
fn in_order(earlier: usize, later: usize) -> bool {
    earlier <= later // tmt:ignore defensive: as above
}

/// `if let P = e && cond` is a let chain, not a boolean expression: it cannot be wrapped.
fn in_let_chain(expr: &syn::Expr) -> bool {
    match expr {
        syn::Expr::Let(_) => true,
        syn::Expr::Binary(syn::ExprBinary {
            op: syn::BinOp::And(_),
            left,
            right,
            ..
        }) => in_let_chain(left) || matches!(**right, syn::Expr::Let(_)),
        _ => false,
    }
}

/// Macros whose arguments are ordinary expressions separated by commas. Macros that turn
/// their argument into text (`assert!`, `debug_assert!`, `dbg!`, `stringify!`) are left
/// alone: rewriting the argument would change the message a test may expect.
const EXPR_MACROS: [&str; 18] = [
    "assert_eq",
    "assert_ne",
    "debug_assert_eq",
    "debug_assert_ne",
    "vec",
    "format",
    "print",
    "println",
    "eprint",
    "eprintln",
    "write",
    "writeln",
    "panic",
    "unreachable",
    "todo",
    "unimplemented",
    "format_args",
    "matches",
];

fn is_assign_op(op: &syn::BinOp) -> bool {
    use syn::BinOp::*;
    matches!(
        op,
        AddAssign(_)
            | SubAssign(_)
            | MulAssign(_)
            | DivAssign(_)
            | RemAssign(_)
            | BitXorAssign(_)
            | BitAndAssign(_)
            | BitOrAssign(_)
            | ShlAssign(_)
            | ShrAssign(_)
    )
}

fn int_literal(expr: &syn::Expr) -> Option<i128> {
    match expr {
        syn::Expr::Paren(e) => int_literal(&e.expr),
        syn::Expr::Group(e) => int_literal(&e.expr),
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Int(lit),
            ..
        }) => lit.base10_parse().ok(),
        syn::Expr::Unary(syn::ExprUnary {
            op: syn::UnOp::Neg(_),
            expr,
            ..
        }) => int_literal(expr).and_then(i128::checked_neg),
        _ => None,
    }
}

fn expr_attrs(expr: &syn::Expr) -> &[syn::Attribute] {
    match expr {
        syn::Expr::Array(e) => &e.attrs,
        syn::Expr::Assign(e) => &e.attrs,
        syn::Expr::Async(e) => &e.attrs,
        syn::Expr::Await(e) => &e.attrs,
        syn::Expr::Binary(e) => &e.attrs,
        syn::Expr::Block(e) => &e.attrs,
        syn::Expr::Break(e) => &e.attrs,
        syn::Expr::Call(e) => &e.attrs,
        syn::Expr::Cast(e) => &e.attrs,
        syn::Expr::Closure(e) => &e.attrs,
        syn::Expr::Const(e) => &e.attrs,
        syn::Expr::Continue(e) => &e.attrs,
        syn::Expr::Field(e) => &e.attrs,
        syn::Expr::ForLoop(e) => &e.attrs,
        syn::Expr::Group(e) => &e.attrs,
        syn::Expr::If(e) => &e.attrs,
        syn::Expr::Index(e) => &e.attrs,
        syn::Expr::Infer(e) => &e.attrs,
        syn::Expr::Let(e) => &e.attrs,
        syn::Expr::Lit(e) => &e.attrs,
        syn::Expr::Loop(e) => &e.attrs,
        syn::Expr::Macro(e) => &e.attrs,
        syn::Expr::Match(e) => &e.attrs,
        syn::Expr::MethodCall(e) => &e.attrs,
        syn::Expr::Paren(e) => &e.attrs,
        syn::Expr::Path(e) => &e.attrs,
        syn::Expr::Range(e) => &e.attrs,
        syn::Expr::RawAddr(e) => &e.attrs,
        syn::Expr::Reference(e) => &e.attrs,
        syn::Expr::Repeat(e) => &e.attrs,
        syn::Expr::Return(e) => &e.attrs,
        syn::Expr::Struct(e) => &e.attrs,
        syn::Expr::Try(e) => &e.attrs,
        syn::Expr::TryBlock(e) => &e.attrs,
        syn::Expr::Tuple(e) => &e.attrs,
        syn::Expr::Unary(e) => &e.attrs,
        syn::Expr::Unsafe(e) => &e.attrs,
        syn::Expr::While(e) => &e.attrs,
        syn::Expr::Yield(e) => &e.attrs,
        _ => &[],
    }
}

/// Whether an expression (or its leftmost place/receiver where `syn` attaches statement
/// attributes such as `#[cfg(feature = "x")] x += 1;` or `#[cfg(..)] (a, b) = (1, 2);`)
/// carries a conditional compilation or skip attribute that could erase the expression
/// and leave an empty statement guard.
fn has_conditional_or_skip_attr(expr: &syn::Expr) -> bool {
    let attrs_block = |attrs: &[syn::Attribute]| {
        is_test_only(attrs)
            || is_skipped(attrs)
            || attrs
                .iter()
                .any(|attr| attr.path().is_ident("cfg") || attr.path().is_ident("cfg_attr"))
    };
    if attrs_block(expr_attrs(expr)) {
        return true;
    }
    match expr {
        syn::Expr::Field(e) => has_conditional_or_skip_attr(&e.base),
        syn::Expr::Index(e) => has_conditional_or_skip_attr(&e.expr),
        syn::Expr::Unary(e) => has_conditional_or_skip_attr(&e.expr),
        syn::Expr::Paren(e) => has_conditional_or_skip_attr(&e.expr),
        syn::Expr::Group(e) => has_conditional_or_skip_attr(&e.expr),
        syn::Expr::Call(e) => has_conditional_or_skip_attr(&e.func),
        syn::Expr::MethodCall(e) => has_conditional_or_skip_attr(&e.receiver),
        _ => false,
    }
}

/// Statements whose removal is a plausible bug: a forgotten call, check or update.
fn deletable(expr: &syn::Expr) -> bool {
    match expr {
        syn::Expr::Call(e) => e.attrs.is_empty(),
        syn::Expr::MethodCall(e) => e.attrs.is_empty(),
        syn::Expr::Assign(e) => e.attrs.is_empty() && !has_conditional_or_skip_attr(&e.left),
        // An attribute in front of `a += 1` belongs to `a`, never to the operation.
        // Allow harmless lint attributes (`#[allow(..)]`), but never wrap `#[cfg(..)]`
        // or `#[mutants::skip]` whose removal would leave an empty statement guard.
        syn::Expr::Binary(e) => {
            is_assign_op(&e.op) && e.attrs.is_empty() && !has_conditional_or_skip_attr(&e.left)
        }
        syn::Expr::Try(e) => e.attrs.is_empty() && deletable_call(&e.expr),
        syn::Expr::Await(e) => e.attrs.is_empty() && deletable_call(&e.base),
        _ => false,
    }
}

/// Whether the receiver of a `last()` is written as an iterator: a call of one of the
/// methods of the standard library that make or adapt one. `Iterator::last` takes the
/// iterator itself, which a probe that only borrows its receiver could not give it.
fn yields_iterator(receiver: &syn::Expr) -> bool {
    const MAKERS: &[&str] = &[
        "iter",
        "iter_mut",
        "into_iter",
        "chars",
        "char_indices",
        "bytes",
        "lines",
        "keys",
        "values",
        "values_mut",
        "into_keys",
        "into_values",
        "drain",
        "windows",
        "chunks",
        "chunks_exact",
        "rchunks",
        "split",
        "rsplit",
        "splitn",
        "rsplitn",
        "split_terminator",
        "split_whitespace",
        "split_ascii_whitespace",
        "split_inclusive",
        "matches",
        "match_indices",
        "components",
        "ancestors",
        "range",
        "map",
        "filter",
        "filter_map",
        "flat_map",
        "flatten",
        "rev",
        "skip",
        "skip_while",
        "take",
        "take_while",
        "step_by",
        "zip",
        "chain",
        "enumerate",
        "peekable",
        "cloned",
        "copied",
        "inspect",
        "scan",
        "fuse",
        "by_ref",
        "map_while",
        "cycle",
        "args",
        "encode_utf16",
    ];
    match receiver {
        syn::Expr::Paren(e) => yields_iterator(&e.expr),
        syn::Expr::Group(e) => yields_iterator(&e.expr),
        syn::Expr::MethodCall(e) => MAKERS.contains(&e.method.to_string().as_str()),
        syn::Expr::Range(_) => true,
        _ => false,
    }
}

/// The name of a receiver that is a variable, with or without parentheses around it.
/// A name that starts with a capital is taken for a constant, which is a new value each
/// time it is named.
pub(super) fn variable_name(receiver: &str) -> Option<String> {
    let mut expr = &syn::parse_str::<syn::Expr>(receiver).ok()?;
    while let syn::Expr::Paren(inner) = expr {
        if !inner.attrs.is_empty() {
            return None;
        }
        expr = &inner.expr;
    }
    let syn::Expr::Path(path) = expr else {
        return None;
    };
    if path.qself.is_some() || !path.attrs.is_empty() {
        return None;
    }
    let name = path.path.get_ident()?.to_string();
    let first = name.trim_start_matches("r#").chars().next()?;
    (!first.is_uppercase()).then_some(name)
}

fn deletable_call(expr: &syn::Expr) -> bool {
    match expr {
        syn::Expr::Call(_) | syn::Expr::MethodCall(_) => true,
        syn::Expr::Await(e) => deletable_call(&e.base),
        _ => false,
    }
}

pub(super) fn has_ident(tokens: TokenStream, name: &str) -> bool {
    tokens.into_iter().any(|tree| match tree {
        TokenTree::Ident(ident) => ident == name,
        TokenTree::Group(group) => has_ident(group.stream(), name),
        _ => false,
    })
}

impl<'ast> Visit<'ast> for Finder<'_> {
    fn visit_attribute(&mut self, _: &'ast syn::Attribute) {}
    fn visit_local(&mut self, node: &'ast syn::Local) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            visit::visit_local(self, node);
        }
    }
    fn visit_pat(&mut self, node: &'ast syn::Pat) {
        if let syn::Pat::Type(binding) = node {
            self.visit_type(&binding.ty);
        }
    }
    fn visit_type_macro(&mut self, _: &'ast syn::TypeMacro) {}
    fn visit_type_array(&mut self, node: &'ast syn::TypeArray) {
        self.visit_type(&node.elem);
        self.constant(&[], &node.len);
    }
    fn visit_const_param(&mut self, node: &'ast syn::ConstParam) {
        self.visit_type(&node.ty);
        if let Some(default) = &node.default {
            self.constant(&node.attrs, default);
        }
    }
    fn visit_generic_argument(&mut self, node: &'ast syn::GenericArgument) {
        match node {
            syn::GenericArgument::Const(value) => self.constant(&[], value),
            _ => visit::visit_generic_argument(self, node),
        }
    }
    fn visit_variant(&mut self, node: &'ast syn::Variant) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            self.visit_fields(&node.fields);
            if let Some((_, value)) = &node.discriminant {
                self.constant(&[], value);
            }
        }
    }
    fn visit_file(&mut self, node: &'ast syn::File) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            visit::visit_file(self, node);
        }
    }
    fn visit_item_enum(&mut self, node: &'ast syn::ItemEnum) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            visit::visit_item_enum(self, node);
        }
    }
    fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            visit::visit_item_struct(self, node);
        }
    }
    fn visit_item_union(&mut self, node: &'ast syn::ItemUnion) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            visit::visit_item_union(self, node);
        }
    }
    fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            visit::visit_item_trait(self, node);
        }
    }
    fn visit_item_type(&mut self, node: &'ast syn::ItemType) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            visit::visit_item_type(self, node);
        }
    }
    fn visit_field(&mut self, node: &'ast syn::Field) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            visit::visit_field(self, node);
        }
    }
    fn visit_impl_item_type(&mut self, node: &'ast syn::ImplItemType) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            visit::visit_impl_item_type(self, node);
        }
    }
    fn visit_trait_item_type(&mut self, node: &'ast syn::TraitItemType) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            visit::visit_trait_item_type(self, node);
        }
    }
    fn visit_item_const(&mut self, node: &'ast syn::ItemConst) {
        self.typed_constant(&node.attrs, &node.ty, Some(&node.expr));
    }
    fn visit_item_static(&mut self, node: &'ast syn::ItemStatic) {
        self.typed_constant(&node.attrs, &node.ty, Some(&node.expr));
    }
    fn visit_impl_item_const(&mut self, node: &'ast syn::ImplItemConst) {
        self.typed_constant(&node.attrs, &node.ty, Some(&node.expr));
    }
    fn visit_trait_item_const(&mut self, node: &'ast syn::TraitItemConst) {
        self.typed_constant(
            &node.attrs,
            &node.ty,
            node.default.as_ref().map(|(_, value)| value),
        );
    }
    fn visit_expr_const(&mut self, node: &'ast syn::ExprConst) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            self.const_depth += 1;
            self.visit_block(&node.block);
            self.const_depth -= 1;
        }
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        let Some(name) = node.path.segments.last().map(|s| s.ident.to_string()) else {
            return;
        };
        if !EXPR_MACROS.contains(&name.as_str()) || self.local_macros.contains(&name) {
            return;
        }
        if name == "matches" {
            // matches!(expr, pattern): only the scrutinee is an expression.
            let scrutinee = node.parse_body_with(|input: syn::parse::ParseStream| {
                let expr: syn::Expr = input.parse()?;
                input.parse::<proc_macro2::TokenStream>()?;
                Ok(expr)
            });
            if let Ok(expr) = scrutinee {
                self.visit_expr(&expr);
            }
            return;
        }
        type Args = syn::punctuated::Punctuated<syn::Expr, syn::Token![,]>;
        // `vec![x; n]`, named format arguments with odd syntax and the like do not parse
        // as a plain argument list: skipped.
        if let Ok(args) = node.parse_body_with(Args::parse_terminated) {
            for arg in &args {
                // `name = value` in a format string parses as an assignment; nothing
                // treats it as one here, and only its value holds sites.
                self.visit_expr(arg);
            }
        }
    }

    fn visit_expr_repeat(&mut self, node: &'ast syn::ExprRepeat) {
        self.visit_expr(&node.expr);
        self.constant(&node.attrs, &node.len);
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        // The file of a module that is not analysed is left alone as a whole.
        if is_test_only(&node.attrs) || is_skipped(&node.attrs) {
            if node.content.is_none() {
                let path = node.attrs.iter().find_map(|attr| match &attr.meta {
                    syn::Meta::NameValue(nv) if nv.path.is_ident("path") => match &nv.value {
                        syn::Expr::Lit(syn::ExprLit {
                            lit: syn::Lit::Str(s),
                            ..
                        }) => Some(s.value()),
                        _ => None,
                    },
                    _ => None,
                });
                self.found.test_modules.push(TestModule {
                    name: node.ident.to_string(),
                    path,
                });
            }
            return;
        }
        visit::visit_item_mod(self, node);
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            // Nothing in a `const fn` can ask at run time which mutant is on.
            let constant = node.sig.constness.is_some() as usize;
            self.const_depth += constant;
            self.function_sites(&node.attrs, &node.sig, &node.block);
            visit::visit_item_fn(self, node);
            self.const_depth -= constant;
        }
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            // Nothing in a `const fn` can ask at run time which mutant is on.
            let constant = node.sig.constness.is_some() as usize;
            self.const_depth += constant;
            self.function_sites(&node.attrs, &node.sig, &node.block);
            visit::visit_impl_item_fn(self, node);
            self.const_depth -= constant;
        }
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            let constant = node.sig.constness.is_some() as usize;
            self.const_depth += constant;
            if let Some(body) = &node.default {
                self.function_sites(&node.attrs, &node.sig, body);
            }
            visit::visit_trait_item_fn(self, node);
            self.const_depth -= constant;
        }
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            visit::visit_item_impl(self, node);
        }
    }

    fn visit_expr_closure(&mut self, node: &'ast syn::ExprClosure) {
        if node.constness.is_none() {
            visit::visit_expr_closure(self, node);
        }
    }

    fn visit_block(&mut self, node: &'ast syn::Block) {
        for stmt in &node.stmts {
            if let syn::Stmt::Expr(expr, Some(semi)) = stmt
                && deletable(expr)
                && let (Some((start, _)), Some((_, end))) =
                    (self.range(expr.span()), self.range(semi.span()))
                && in_order(start, end)
            {
                self.push(SiteKind::Stmt, start, end, expr.span());
            }
        }
        visit::visit_block(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        let name = node.method.to_string();
        if (name == "max" || name == "min")
            && node.args.len() == 1
            && node.turbofish.is_none()
            && node.attrs.is_empty()
            && let (
                Some((start, receiver_end)),
                Some((argument_start, argument_end)),
                Some((_, end)),
            ) = (
                self.range(node.receiver.span()),
                self.range(node.args[0].span()),
                self.range(node.paren_token.span.close()),
            )
            && in_order(receiver_end, argument_start)
            && in_order(argument_end, end)
        {
            let kind = SiteKind::MinMax {
                max: name == "max",
                receiver: self.source[start..receiver_end].to_string(),
                argument: self.source[argument_start..argument_end].to_string(),
                call: (receiver_end, argument_start),
                close: (argument_end, end),
            };
            self.push(kind, start, end, node.receiver.span());
        }
        if (name == "first" || name == "last")
            && node.args.is_empty()
            && node.turbofish.is_none()
            && node.attrs.is_empty()
            && !(name == "last" && yields_iterator(&node.receiver))
            && let (Some((start, receiver_end)), Some(method), Some((_, end))) = (
                self.range(node.receiver.span()),
                self.range(node.method.span()),
                self.range(node.paren_token.span.close()),
            )
            && in_order(receiver_end, method.0)
            && in_order(method.1, end)
        {
            let kind = SiteKind::SliceEnd {
                first: name == "first",
                receiver: self.source[start..receiver_end].to_string(),
                call: (receiver_end, end),
                method,
            };
            self.push(kind, start, end, node.receiver.span());
        }
        if (name == "starts_with" || name == "ends_with")
            && node.args.len() == 1
            && node.turbofish.is_none()
            && node.attrs.is_empty()
            && let (
                Some((start, receiver_end)),
                Some(method),
                Some((pat_start, pat_end)),
                Some((_, end)),
            ) = (
                self.range(node.receiver.span()),
                self.range(node.method.span()),
                self.range(node.args[0].span()),
                self.range(node.paren_token.span.close()),
            )
            && in_order(receiver_end, method.0)
            && in_order(method.1, pat_start)
            && in_order(pat_end, end)
        {
            let kind = SiteKind::StrAffix {
                starts: name == "starts_with",
                receiver: self.source[start..receiver_end].to_string(),
                pattern: self.source[pat_start..pat_end].to_string(),
                call: (receiver_end, pat_start),
                close: (pat_end, end),
                method,
            };
            self.push(kind, start, end, node.receiver.span());
        }
        if ((name == "replace" && node.args.len() == 2)
            || (name == "replacen" && node.args.len() == 3 && is_int_one(&node.args[2])))
            && node.turbofish.is_none()
            && node.attrs.is_empty()
            && let (
                Some((start, receiver_end)),
                Some(method),
                Some((from_start, from_end)),
                Some((to_start, to_end)),
                Some((_, end)),
            ) = (
                self.range(node.receiver.span()),
                self.range(node.method.span()),
                self.range(node.args[0].span()),
                self.range(node.args[1].span()),
                self.range(node.paren_token.span.close()),
            )
            && in_order(receiver_end, method.0)
            && in_order(method.1, from_start)
            && in_order(from_end, to_start)
            && in_order(to_end, end)
        {
            let kind = SiteKind::StrReplace {
                replace_all: name == "replace",
                receiver: self.source[start..receiver_end].to_string(),
                from: self.source[from_start..from_end].to_string(),
                to: self.source[to_start..to_end].to_string(),
                call: (receiver_end, from_start),
                comma: (from_end, to_start),
                close: (to_end, end),
                method,
            };
            self.push(kind, start, end, node.receiver.span());
        }
        if name == "clamp"
            && node.args.len() == 2
            && node.turbofish.is_none()
            && node.attrs.is_empty()
            && let (
                Some((start, receiver_end)),
                Some((low_start, low_end)),
                Some((high_start, high_end)),
                Some((_, end)),
            ) = (
                self.range(node.receiver.span()),
                self.range(node.args[0].span()),
                self.range(node.args[1].span()),
                self.range(node.paren_token.span.close()),
            )
            && in_order(receiver_end, low_start)
            && in_order(low_end, high_start)
            && in_order(high_end, end)
        {
            let kind = SiteKind::Clamp {
                receiver: self.source[start..receiver_end].to_string(),
                low: self.source[low_start..low_end].to_string(),
                high: self.source[high_start..high_end].to_string(),
                call: (receiver_end, low_start),
                comma: (low_end, high_start),
                close: (high_end, end),
            };
            self.push(kind, start, end, node.receiver.span());
        }
        if let Some(op) = rounding_op(&name)
            && node.args.is_empty()
            && node.turbofish.is_none()
            && node.attrs.is_empty()
            && let (Some((start, receiver_end)), Some(method), Some((_, end))) = (
                self.range(node.receiver.span()),
                self.range(node.method.span()),
                self.range(node.paren_token.span.close()),
            )
            && in_order(receiver_end, method.0)
            && in_order(method.1, end)
        {
            let kind = SiteKind::Rounding {
                op,
                receiver: self.source[start..receiver_end].to_string(),
                call: (receiver_end, end),
                method,
            };
            self.push(kind, start, end, node.receiver.span());
        }
        if (name == "all" || name == "any")
            && node.args.len() == 1
            && matches!(&node.args[0], syn::Expr::Closure(_))
            && node.turbofish.is_none()
            && node.attrs.is_empty()
            && let (
                Some((start, receiver_end)),
                Some(method),
                Some((pred_start, pred_end)),
                Some((_, end)),
            ) = (
                self.range(node.receiver.span()),
                self.range(node.method.span()),
                self.range(node.args[0].span()),
                self.range(node.paren_token.span.close()),
            )
            && in_order(receiver_end, method.0)
            && in_order(method.1, pred_start)
            && in_order(pred_end, end)
        {
            let kind = SiteKind::Quantifier {
                all: name == "all",
                receiver: self.source[start..receiver_end].to_string(),
                predicate: self.source[pred_start..pred_end].to_string(),
                call: (receiver_end, pred_start),
                close: (pred_end, end),
                method,
            };
            self.push(kind, start, end, node.receiver.span());
        }
        if name == "contains"
            && node.args.len() == 1
            && node.turbofish.is_none()
            && node.attrs.is_empty()
            && let Some(array) = literal_array_receiver(&node.receiver)
            && (2..=8).contains(&array.elems.len())
            && array.elems.iter().all(is_primitive_literal)
            && let (Some((start, receiver_end)), Some((arg_start, arg_end)), Some((_, end))) = (
                self.range(node.receiver.span()),
                self.range(node.args[0].span()),
                self.range(node.paren_token.span.close()),
            )
            && in_order(receiver_end, arg_start)
            && in_order(arg_end, end)
        {
            let elem_spans: Option<Vec<(usize, usize)>> = array
                .elems
                .iter()
                .map(|elem| self.range(elem.span()))
                .collect();
            if let Some(spans) = elem_spans {
                let mut seen = HashSet::new();
                let all_unique = spans
                    .iter()
                    .all(|&(es, ee)| seen.insert(tokens_of(&self.source[es..ee])));
                if all_unique && spans.windows(2).all(|w| in_order(w[0].1, w[1].0)) {
                    let elements = spans
                        .iter()
                        .enumerate()
                        .map(|(i, &(es, ee))| {
                            let (rem_start, rem_end) = if i + 1 < spans.len() {
                                (es, spans[i + 1].0)
                            } else {
                                (spans[i - 1].1, ee)
                            };
                            (self.source[es..ee].to_string(), rem_start, rem_end)
                        })
                        .collect();
                    let at = node.receiver.span().start();
                    self.found.rebuild_sites.push(Site {
                        id: NO_ID,
                        kind: SiteKind::LiteralSet {
                            receiver: self.source[start..receiver_end].to_string(),
                            argument: self.source[arg_start..arg_end].to_string(),
                            elements,
                        },
                        line: at.line,
                        column: at.column + 1,
                        start,
                        end,
                        text: self.source[start..end].to_string(),
                    });
                }
            }
        }
        visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_for_loop(&mut self, node: &'ast syn::ExprForLoop) {
        let mut over = &*node.expr;
        while let syn::Expr::Paren(syn::ExprParen { expr, .. })
        | syn::Expr::Group(syn::ExprGroup { expr, .. }) = over
        {
            over = expr;
        }
        if let syn::Expr::Range(syn::ExprRange {
            start: Some(from), ..
        }) = over
            && let Some((start, _)) = self.range(from.span())
        {
            self.for_ranges.insert(start);
        }
        visit::visit_expr_for_loop(self, node);
    }

    fn visit_expr_range(&mut self, node: &'ast syn::ExprRange) {
        // With two literals nothing fixes the integer type before the probe is resolved,
        // and it would be taken for `i32` (`for i in 0..16 { words[i] }`).
        if let (Some(from), Some(to)) = (&node.start, &node.end)
            && node.attrs.is_empty()
            && let (Some((start, lhs_end)), Some((rhs_start, end))) =
                (self.range(from.span()), self.range(to.span()))
            && in_order(lhs_end, rhs_start)
        {
            let kind = SiteKind::Range {
                inclusive: matches!(node.limits, syn::RangeLimits::Closed(_)),
                lhs: self.source[start..lhs_end].to_string(),
                rhs: self.source[rhs_start..end].to_string(),
                gap: (lhs_end, rhs_start),
            };
            // `a..b + 1` in place of `a..=b` gives a `for` the same numbers, and that is
            // all a `for` takes from its range. Anywhere else the two are told apart (by
            // their type, by `end_bound`, by a field): there the mutant is the range as it
            // is written in the source, and it is built like the code of a `const fn`.
            // So is a range of two literals, wherever it is: nothing fixes their type
            // before a probe is resolved, and it would be taken for `i32`.
            let built = !self.for_ranges.contains(&start) || (is_literal(from) && is_literal(to));
            self.const_depth += built as usize;
            self.push(kind, start, end, from.span());
            self.const_depth -= built as usize;
        }
        visit::visit_expr_range(self, node);
    }

    fn visit_expr_match(&mut self, node: &'ast syn::ExprMatch) {
        // A binding or `_` takes everything: with such a last arm the match stays
        // exhaustive when an arm above it stops matching.
        let catches_all = |arm: &syn::Arm| {
            arm.guard.is_none()
                && match &arm.pat {
                    syn::Pat::Wild(_) => true,
                    syn::Pat::Ident(p) => {
                        p.subpat.is_none()
                            && p.ident
                                .to_string()
                                .starts_with(|c: char| c.is_lowercase() || c == '_')
                    }
                    _ => false,
                }
        };
        let mut removable = vec![];
        let has_wildcard = node
            .arms
            .iter()
            .any(|arm| matches!(arm.pat, syn::Pat::Wild(_)));
        let final_catches_all = node.arms.last().is_some_and(catches_all);
        for arm in &node.arms {
            if is_test_only(&arm.attrs) || is_skipped(&arm.attrs) {
                continue;
            }
            // The last arm is not probed: where it takes everything, it is a catch-all.
            let probe = final_catches_all && arm.attrs.is_empty() && !catches_all(arm);
            let literal =
                has_wildcard && !matches!(arm.pat, syn::Pat::Wild(_)) && arm.guard.is_none();
            if !probe && !literal {
                continue;
            }
            let (Some((start, pat_end)), Some((remove_start, remove_end)), Some((arrow, _))) = (
                self.range(arm.pat.span()),
                self.range(arm.span()),
                self.range(arm.fat_arrow_token.span()),
            ) else {
                continue;
            };
            let guard = match &arm.guard {
                None => ArmGuard::None(arrow),
                // The arm with `false` for its guard is the program as it is.
                Some((_, expr)) if written_bool(expr) == Some(false) => continue,
                Some((_, expr)) => match self.range(expr.span()) {
                    Some((from, to)) => ArmGuard::Existing(from, to),
                    None => continue,
                },
            };
            let pattern = match guard {
                ArmGuard::Existing(from, to) => format!(
                    "{} if {}",
                    &self.source[start..pat_end],
                    &self.source[from..to]
                ),
                ArmGuard::None(_) => self.source[start..pat_end].to_string(),
            };
            let end = arm
                .comma
                .as_ref()
                .and_then(|comma| self.range(comma.span()))
                .map_or(remove_end, |(_, end)| end);
            let at = arm.pat.span().start();
            removable.push(start);
            let site = Site {
                id: 0,
                kind: SiteKind::Arm {
                    pattern: pattern.clone(),
                    guard,
                    removal: (remove_start, end),
                },
                line: at.line,
                column: at.column + 1,
                start,
                end,
                text: format!("{pattern} => .."),
            };
            if probe {
                self.add(site);
            } else {
                self.add_rebuild(site);
            }
        }
        // Any guard can be `true` or `false` instead: a guarded arm never counts for
        // the match being exhaustive. A `let` in a guard binds names for the arm.
        for arm in node.arms.iter().filter(|arm| arm.attrs.is_empty()) {
            if let Some((_, guard)) = &arm.guard
                && !matches!(**guard, syn::Expr::Let(_))
                && !in_let_chain(guard)
                && let (Some((start, end)), Some((pattern, _))) =
                    (self.range(guard.span()), self.range(arm.pat.span()))
            {
                let can_be_false = !removable.contains(&pattern);
                let kind = SiteKind::Guard { can_be_false };
                if written_bool(guard) == Some(true) && !can_be_false {
                    // Written `true`, and `false` is the arm removed: no mutant is left.
                } else if written_bool(guard).is_some() {
                    // A guard written `true` or `false` is one of its two mutants: the
                    // other is built, as a probe would report the one that is the
                    // program for never being told from it.
                    let at = guard.span().start();
                    self.add_rebuild(Site {
                        id: 0,
                        kind,
                        line: at.line,
                        column: at.column + 1,
                        start,
                        end,
                        text: self.source[start..end].to_string(),
                    });
                } else {
                    self.push(kind, start, end, guard.span());
                }
            }
        }
        visit::visit_expr_match(self, node);
    }

    fn visit_arm(&mut self, arm: &'ast syn::Arm) {
        if !is_test_only(&arm.attrs) && !is_skipped(&arm.attrs) {
            visit::visit_arm(self, arm);
        }
    }

    fn visit_field_value(&mut self, field: &'ast syn::FieldValue) {
        if !is_test_only(&field.attrs) && !is_skipped(&field.attrs) {
            visit::visit_field_value(self, field);
        }
    }

    fn visit_expr_struct(&mut self, node: &'ast syn::ExprStruct) {
        if is_test_only(&node.attrs) || is_skipped(&node.attrs) {
            return;
        }
        // Only probes read the base again. Source mutants delete a field in place.
        let from_base = node.rest.as_deref().and_then(|base| {
            if !node.attrs.is_empty() || node.qself.is_some() {
                return None;
            }
            let (from, to) = self.range(base.span())?;
            let written = &self.source[from..to];
            match base {
                base if is_plain_place(base) => Some(format!("({written})")),
                syn::Expr::Call(call)
                    if call.attrs.is_empty()
                        && call.args.is_empty()
                        && matches!(*call.func, syn::Expr::Path(_)) =>
                {
                    let (from, to) = self.range(node.path.span())?;
                    Some(format!("({} {{ ..{written} }})", &self.source[from..to]))
                }
                _ => None,
            }
        });
        if node.rest.is_some() {
            for pair in node.fields.pairs() {
                let field = pair.value();
                if is_test_only(&field.attrs) || is_skipped(&field.attrs) {
                    continue;
                }
                let name = match &field.member {
                    syn::Member::Named(name) => name.to_string(),
                    syn::Member::Unnamed(index) => index.index.to_string(),
                };
                let (Some((start, _)), Some((value, end)), Some((remove_start, _))) = (
                    self.range(field.member.span()),
                    self.range(field.expr.span()),
                    self.range(field.span()),
                ) else {
                    continue;
                };
                let remove_end = pair
                    .punct()
                    .and_then(|comma| self.range(comma.span()))
                    .map_or(end, |(_, end)| end);
                let from_base = from_base
                    .as_ref()
                    .filter(|_| field.attrs.is_empty())
                    .map(|base| format!("{base}.{name}"));
                let probe = from_base.is_some();
                let kind = SiteKind::Field {
                    from_base,
                    name,
                    shorthand: field.colon_token.is_none(),
                    removal: (remove_start, remove_end),
                };
                let at = field.member.span().start();
                let site = Site {
                    id: 0,
                    kind,
                    line: at.line,
                    column: at.column + 1,
                    start: if probe { value } else { remove_start },
                    end,
                    text: self.source[start..end].to_string(),
                };
                if probe {
                    self.add(site);
                } else {
                    self.add_rebuild(site);
                }
            }
        }
        visit::visit_expr_struct(self, node);
    }

    fn visit_expr_unary(&mut self, node: &'ast syn::ExprUnary) {
        let joins = |start: usize, operand_start: usize| {
            let prev = self.source[..start].chars().next_back();
            let next = self.source[operand_start..].chars().next();
            // Any two adjacent non-delimiter characters can glue into a different token or
            // comment (`return!x` -> `returnx`, `return!℘` -> `return℘`, `a/!*b` -> `a/*b`).
            let glues = |c: char| {
                !c.is_whitespace() && !matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';')
            };
            prev.is_some_and(glues) && next.is_some_and(glues)
        };
        if let syn::UnOp::Not(bang) = &node.op
            && node.attrs.is_empty()
            && let (Some((start, _)), Some((operand_start, end))) =
                (self.range(bang.span()), self.range(node.expr.span()))
            && in_order(start, operand_start)
        {
            let kind = SiteKind::Not {
                bang: (start, operand_start),
                operand: self.source[operand_start..end].to_string(),
                needs_space: joins(start, operand_start),
            };
            self.push(kind, start, end, bang.span());
        }
        if let syn::UnOp::Neg(minus) = &node.op
            && node.attrs.is_empty()
            && let (Some((start, _)), Some((operand_start, end))) =
                (self.range(minus.span()), self.range(node.expr.span()))
            && in_order(start, operand_start)
        {
            let zero = match &*node.expr {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Int(lit),
                    ..
                }) => lit.base10_parse::<u128>().is_ok_and(|v| v == 0),
                // Not a float: `-0.0` is not `0.0` (`1.0 / -0.0` is minus infinity).
                _ => false,
            };
            if !zero {
                let kind = SiteKind::Neg {
                    minus: (start, operand_start),
                    operand: self.source[operand_start..end].to_string(),
                    literal: matches!(*node.expr, syn::Expr::Lit(_)),
                    needs_space: joins(start, operand_start),
                };
                self.push(kind, start, end, minus.span());
            }
        }
        visit::visit_expr_unary(self, node);
    }

    fn visit_expr_binary(&mut self, node: &'ast syn::ExprBinary) {
        if let Some(op) = cmp_op(&node.op)
            && node.attrs.is_empty()
            && let (Some((start, lhs_end)), Some((rhs_start, end))) =
                (self.range(node.left.span()), self.range(node.right.span()))
            && in_order(lhs_end, rhs_start)
        {
            let literal = match (int_literal(&node.left), int_literal(&node.right)) {
                (None, Some(k)) => LiteralSide::Rhs(k),
                (Some(k), None) => LiteralSide::Lhs(k),
                _ => LiteralSide::None,
            };
            let kind = SiteKind::Cmp {
                op,
                lhs: self.source[start..lhs_end].to_string(),
                rhs: self.source[rhs_start..end].to_string(),
                literal,
                gap: (lhs_end, rhs_start),
            };
            self.push(kind, start, end, node.left.span());
        } else if node.attrs.is_empty()
            && let (Some((start, lhs_end)), Some((rhs_start, end))) =
                (self.range(node.left.span()), self.range(node.right.span()))
            && in_order(lhs_end, rhs_start)
        {
            let lhs = self.source[start..lhs_end].to_string();
            let rhs = self.source[rhs_start..end].to_string();
            let gap = (lhs_end, rhs_start);
            let logic = match &node.op {
                syn::BinOp::And(_) => Some(true),
                syn::BinOp::Or(_) => Some(false),
                _ => None,
            };
            if let Some(and) = logic {
                if !in_let_chain(&node.left) && !matches!(*node.right, syn::Expr::Let(_)) {
                    self.push(
                        SiteKind::Logic { and, lhs, rhs, gap },
                        start,
                        end,
                        node.left.span(),
                    );
                }
            } else if let Some((op, assign)) = arith_op(&node.op) {
                let kind = SiteKind::Arith {
                    op,
                    assign,
                    lhs,
                    rhs,
                    gap,
                };
                // Two literals have no type until the context gives them one, and a
                // probe is a context of its own.
                let literals = is_literal(&node.left) && is_literal(&node.right);
                if !literals && (!assign || is_plain_place(&node.left)) {
                    self.push(kind, start, end, node.left.span());
                } else {
                    // Which of the place and the value is evaluated first depends on
                    // the type, which a probe would have to know.
                    let at = node.left.span().start();
                    self.found.rebuild_sites.push(Site {
                        id: NO_ID,
                        kind,
                        line: at.line,
                        column: at.column + 1,
                        start,
                        end,
                        text: self.source[start..end].to_string(),
                    });
                }
            }
        }
        visit::visit_expr_binary(self, node);
    }
}

fn rounding_op(name: &str) -> Option<RoundingOp> {
    match name {
        "floor" => Some(RoundingOp::Floor),
        "ceil" => Some(RoundingOp::Ceil),
        "round" => Some(RoundingOp::Round),
        "trunc" => Some(RoundingOp::Trunc),
        _ => None,
    }
}

fn is_int_one(mut expr: &syn::Expr) -> bool {
    while let syn::Expr::Paren(syn::ExprParen { expr: inner, .. })
    | syn::Expr::Group(syn::ExprGroup { expr: inner, .. }) = expr
    {
        expr = inner;
    }
    matches!(
        expr,
        syn::Expr::Lit(syn::ExprLit {
            attrs,
            lit: syn::Lit::Int(n),
        }) if attrs.is_empty() && n.base10_parse::<usize>().ok() == Some(1)
    )
}

fn literal_array_receiver(mut expr: &syn::Expr) -> Option<&syn::ExprArray> {
    loop {
        match expr {
            syn::Expr::Paren(syn::ExprParen { expr: inner, .. })
            | syn::Expr::Group(syn::ExprGroup { expr: inner, .. }) => expr = inner,
            syn::Expr::Reference(syn::ExprReference {
                mutability: None,
                expr: inner,
                ..
            }) => expr = inner,
            syn::Expr::Array(array) if array.attrs.is_empty() => return Some(array),
            _ => return None,
        }
    }
}

fn is_primitive_literal(expr: &syn::Expr) -> bool {
    match expr {
        syn::Expr::Lit(syn::ExprLit { attrs, lit }) if attrs.is_empty() => {
            !matches!(lit, syn::Lit::Verbatim(_))
        }
        syn::Expr::Unary(syn::ExprUnary {
            attrs,
            op: syn::UnOp::Neg(_),
            expr,
        }) if attrs.is_empty() => matches!(
            expr.as_ref(),
            syn::Expr::Lit(syn::ExprLit {
                attrs,
                lit: syn::Lit::Int(_) | syn::Lit::Float(_),
            }) if attrs.is_empty()
        ),
        _ => false,
    }
}
