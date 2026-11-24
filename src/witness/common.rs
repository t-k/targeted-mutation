//! Shared AST predicates and helpers for witness discovery modules.

use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::{Expr, ItemFn, Pat, Token, Type};

/// Whether a function has a plain, directly callable Rust signature with harmless
/// attributes (`doc`, `inline`, `must_use`), no generics, and no `async`/`unsafe`/ABI.
pub(super) fn is_plain_fn(function: &ItemFn, allow_const: bool) -> bool {
    let signature = &function.sig;
    signature.generics.params.is_empty()
        && signature.asyncness.is_none()
        && signature.unsafety.is_none()
        && (allow_const || signature.constness.is_none())
        && signature.abi.is_none()
        && function.attrs.iter().all(|attr| {
            ["doc", "inline", "must_use"]
                .iter()
                .any(|name| attr.path().is_ident(name))
        })
}

/// Peels wrapping parentheses and invisible macro delimiter groups from an expression.
pub(super) fn unparen(mut expr: &Expr) -> &Expr {
    loop {
        expr = match expr {
            Expr::Paren(paren) => &paren.expr,
            Expr::Group(group) => &group.expr,
            _ => return expr,
        };
    }
}

/// Extracts a single unqualified identifier from a plain type path (`u32`, `Entry`, etc.).
pub(super) fn type_name(ty: &Type) -> Option<&syn::Ident> {
    let Type::Path(named) = ty else { return None };
    named
        .qself
        .is_none()
        .then(|| named.path.get_ident())
        .flatten()
}

/// Extracts a plain identifier binding pattern (`x`, `mut x`), rejecting `ref` and subpatterns.
pub(super) fn ident_binding(pattern: &Pat) -> Option<&syn::Ident> {
    let Pat::Ident(binding) = pattern else {
        return None;
    };
    (binding.by_ref.is_none() && binding.subpat.is_none()).then_some(&binding.ident)
}

/// Extracts an immutable plain identifier binding pattern (`x`), rejecting `mut`, `ref`, and subpatterns.
pub(super) fn immutable_binding(pattern: &Pat) -> Option<&syn::Ident> {
    let Pat::Ident(binding) = pattern else {
        return None;
    };
    (binding.by_ref.is_none() && binding.mutability.is_none() && binding.subpat.is_none())
        .then_some(&binding.ident)
}

/// Extracts an unqualified identifier from a direct path expression without peeling parentheses.
pub(super) fn path_ident(expr: &Expr) -> Option<&syn::Ident> {
    let Expr::Path(path) = expr else { return None };
    path.qself
        .is_none()
        .then(|| path.path.get_ident())
        .flatten()
}

/// Extracts an unqualified identifier from a path expression after peeling parentheses/groups.
pub(super) fn unparen_path_ident(expr: &Expr) -> Option<&syn::Ident> {
    path_ident(unparen(expr))
}

/// Whether an identifier names one of Rust's built-in integer primitive types.
pub(super) fn is_integer_type_name(name: &syn::Ident) -> bool {
    matches!(
        name.to_string().as_str(),
        "i8" | "i16"
            | "i32"
            | "i64"
            | "i128"
            | "isize"
            | "u8"
            | "u16"
            | "u32"
            | "u64"
            | "u128"
            | "usize"
    )
}

/// Parses comma-separated expression arguments inside a macro invocation.
pub(super) fn parse_macro_exprs(mac: &syn::Macro) -> Option<Punctuated<Expr, Token![,]>> {
    Punctuated::<Expr, Token![,]>::parse_terminated
        .parse2(mac.tokens.clone())
        .ok()
}
