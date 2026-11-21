//! Finds a source mutation and a candidate test for first-versus-last slice searches.

use std::collections::BTreeMap;
use std::ops::{Range, RangeInclusive};

use syn::parse::Parser;
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::{Expr, FnArg, Item, ItemFn, Pat, Type};

use crate::report::VerifiedWitness;

pub(crate) const TEST_NAME: &str = "__tmt_position_witness";
pub(crate) const OUTPUT_ENV: &str = "TMT_WITNESS_OUTPUT";
pub(crate) const MUTATION: &str = "replace `position` with `rposition`";

#[derive(Debug)]
pub(crate) struct PositionSearch {
    pub method: Range<usize>,
    pub lines: RangeInclusive<usize>,
    pub signature_line: usize,
    pub method_line: usize,
    pub column: usize,
    pub call: String,
}

impl PositionSearch {
    pub fn verified(&self, original: String, mutant: String) -> Option<VerifiedWitness> {
        (original != mutant).then(|| VerifiedWitness {
            call: self.call.clone(),
            assertion: format!("assert_eq!({}, {original});", self.call),
            original,
            mutant,
        })
    }

    pub fn test_source(&self) -> String {
        format!(
            r#"
#[test]
fn {TEST_NAME}() {{
    extern crate std as __tmt_witness_std;
    let result: __tmt_witness_std::option::Option<__tmt_witness_std::primitive::usize> = self::{};
    let path = __tmt_witness_std::env::var_os("{OUTPUT_ENV}").unwrap();
    let mut output = __tmt_witness_std::fs::OpenOptions::new().append(true).open(path).unwrap();
    __tmt_witness_std::io::Write::write_all(&mut output, __tmt_witness_std::format!("{{result:?}}\n").as_bytes()).unwrap();
}}
"#,
            self.call,
        )
    }
}

pub(crate) fn position_searches(source: &str) -> Vec<PositionSearch> {
    let Ok(file) = syn::parse_file(source) else {
        return vec![];
    };
    let mut seeds = SearchValues::default();
    seeds.visit_file(&file);
    file.items
        .iter()
        .filter_map(|item| {
            let Item::Fn(function) = item else {
                return None;
            };
            let search = position_method(function)?;
            let span = search.method.span();
            let expression = search.span();
            let range = span.byte_range();
            if source.get(range.clone()) != Some("position") {
                return None;
            }
            let function_name = &function.sig.ident;
            let needle = seeds
                .values
                .get(&function_name.to_string())
                .copied()
                .unwrap_or(0);
            Some(PositionSearch {
                method: range,
                lines: expression.start().line..=expression.end().line,
                signature_line: function.sig.span().start().line,
                method_line: span.start().line,
                column: span.start().column + 1,
                call: format!("{function_name}(&[{needle}, {needle}], {needle})"),
            })
        })
        .collect()
}

fn binding(pattern: &Pat) -> Option<&syn::Ident> {
    let Pat::Ident(binding) = pattern else {
        return None;
    };
    (binding.by_ref.is_none() && binding.mutability.is_none() && binding.subpat.is_none())
        .then_some(&binding.ident)
}

fn path_ident(expr: &Expr) -> Option<&syn::Ident> {
    let Expr::Path(path) = expr else { return None };
    path.qself
        .is_none()
        .then(|| path.path.get_ident())
        .flatten()
}

fn position_method(function: &ItemFn) -> Option<&syn::ExprMethodCall> {
    let signature = &function.sig;
    if function.attrs.iter().any(|attr| {
        !["doc", "inline", "must_use"]
            .iter()
            .any(|name| attr.path().is_ident(name))
    }) || !signature.generics.params.is_empty()
        || signature.asyncness.is_some()
        || signature.unsafety.is_some()
        || signature.constness.is_some()
        || signature.abi.is_some()
    {
        return None;
    }
    let [FnArg::Typed(values), FnArg::Typed(needle)] =
        signature.inputs.iter().collect::<Vec<_>>()[..]
    else {
        return None;
    };
    let Type::Reference(reference) = values.ty.as_ref() else {
        return None;
    };
    let Type::Slice(slice) = reference.elem.as_ref() else {
        return None;
    };
    let (Type::Path(element), Type::Path(needle_type)) = (slice.elem.as_ref(), needle.ty.as_ref())
    else {
        return None;
    };
    let integer = element.path.get_ident()?;
    if reference.mutability.is_some()
        || element.qself.is_some()
        || needle_type.qself.is_some()
        || needle_type.path.get_ident() != Some(integer)
        || !matches!(
            integer.to_string().as_str(),
            "u8" | "u16"
                | "u32"
                | "u64"
                | "u128"
                | "usize"
                | "i8"
                | "i16"
                | "i32"
                | "i64"
                | "i128"
                | "isize"
        )
    {
        return None;
    }
    let [syn::Stmt::Expr(Expr::MethodCall(search), None)] = &function.block.stmts[..] else {
        return None;
    };
    let Expr::MethodCall(iter) = search.receiver.as_ref() else {
        return None;
    };
    let Expr::Closure(predicate) = search.args.first()? else {
        return None;
    };
    if search.method != "position"
        || search.args.len() != 1
        || search.turbofish.is_some()
        || iter.method != "iter"
        || !iter.args.is_empty()
        || iter.turbofish.is_some()
        || path_ident(&iter.receiver) != Some(binding(&values.pat)?)
        || predicate.inputs.len() != 1
    {
        return None;
    }
    let value = binding(predicate.inputs.first()?)?;
    let needle = binding(&needle.pat)?;
    let Expr::Binary(equal) = predicate.body.as_ref() else {
        return None;
    };
    let matches_operands = |lhs: &Expr, rhs: &Expr| {
        matches!(lhs, Expr::Unary(deref) if matches!(deref.op, syn::UnOp::Deref(_))
            && path_ident(&deref.expr) == Some(value))
            && path_ident(rhs) == Some(needle)
    };
    (matches!(equal.op, syn::BinOp::Eq(_))
        && (matches_operands(&equal.left, &equal.right)
            || matches_operands(&equal.right, &equal.left)))
    .then_some(search)
}

#[derive(Default)]
struct SearchValues {
    values: BTreeMap<String, u8>,
}

impl<'ast> Visit<'ast> for SearchValues {
    fn visit_item_fn(&mut self, function: &'ast ItemFn) {
        if function
            .attrs
            .iter()
            .any(|attr| attr.path().is_ident("test"))
        {
            self.visit_block(&function.block);
        }
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let Some(name) = path_ident(&call.func)
            && let [_, Expr::Lit(literal)] = call.args.iter().collect::<Vec<_>>()[..]
            && let syn::Lit::Int(integer) = &literal.lit
            // This range fits all primitive integer types supported by the rule.
            && let Ok(value) = integer.base10_parse::<i8>()
        {
            self.values.entry(name.to_string()).or_insert(value as u8);
        }
        syn::visit::visit_expr_call(self, call);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if (mac.path.is_ident("assert_eq") || mac.path.is_ident("assert_ne"))
            && let Ok(args) = syn::punctuated::Punctuated::<Expr, syn::Token![,]>::parse_terminated
                .parse2(mac.tokens.clone())
        {
            for arg in &args {
                self.visit_expr(arg);
            }
        }
    }
}

/// Exactly one complete line, produced by one successful invocation of the test.
pub(crate) fn parse_observation(output: &str) -> Option<String> {
    let line = output.strip_suffix('\n')?;
    if line != "None" {
        let index = line.strip_prefix("Some(")?.strip_suffix(')')?;
        if index.parse::<usize>().ok()?.to_string() != index {
            return None;
        }
    }
    Some(line.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn position_searches_require_an_integer_slice_and_direct_equality() {
        let source = r#"
            /// Índice
            fn search(xs: &[u16], key: u16) -> Option<usize> { xs.iter().position(|x| *x == key) }
            #[inline]
            #[must_use]
            fn reverse(xs: &[i8], key: i8) -> Option<usize> { xs.iter().position(|x| key == *x) }
            fn nested(xs: &[i8], key: i8) -> Option<usize> { xs.iter().position(|x| *x == key) }
            fn other_type(xs: &[u8], key: u16) -> Option<usize> { xs.iter().position(|x| *x == key) }
            fn other_slice(xs: &[u8], key: u8) -> Option<usize> { other.iter().position(|x| *x == key) }
            fn not_equal(xs: &[u8], key: u8) -> Option<usize> { xs.iter().position(|x| *x != key) }
            fn condition(xs: &[u8], key: u8) -> Option<usize> { xs.iter().position(|x| *x == key && f()) }
            fn shadow(xs: &[i8], key: i8) -> Option<usize> { xs.iter().position(|x| x == x) }
            #[cfg(test)]
            fn test_helper(xs: &[u8], key: u8) -> Option<usize> { xs.iter().position(|x| *x == key) }
            #[test]
            fn hits() {
                assert_eq!(search(&[2, 9], 9), Some(1));
                assert_eq!(search(&[2], 4), None);
                assert_eq!(reverse(&[1], 255), None);
                assert_ne!(Some(nested(&[11], 11)), None);
            }
        "#;
        let searches = position_searches(source);
        assert_eq!(searches.len(), 3, "{searches:?}");
        assert_eq!(searches[0].call, "search(&[9, 9], 9)");
        assert_eq!(searches[1].call, "reverse(&[0, 0], 0)");
        assert_eq!(searches[2].call, "nested(&[11, 11], 11)");
        assert_eq!(&source[searches[0].method.clone()], "position");
        assert_eq!(searches[0].lines, 3..=3);
        assert_eq!(searches[0].signature_line, 3);
        assert_eq!(searches[0].method_line, 3);
        let prefix = source[..searches[0].method.start].lines().last().unwrap();
        assert_eq!(searches[0].column, prefix.chars().count() + 1);
    }

    #[test]
    fn position_observations_require_one_complete_value_and_a_difference() {
        for invalid in [
            "",
            "Some(0)",
            "Some(01)\n",
            "Some(0)\nSome(1)\n",
            "None\n\n",
            "panic\n",
        ] {
            assert_eq!(parse_observation(invalid), None, "{invalid:?}");
        }
        assert_eq!(parse_observation("None\n").as_deref(), Some("None"));
        let first = parse_observation("Some(0)\n").unwrap();
        let last = parse_observation("Some(1)\n").unwrap();
        let search = PositionSearch {
            method: 0..0,
            lines: 1..=1,
            signature_line: 1,
            method_line: 1,
            column: 1,
            call: "f(&[0, 0], 0)".into(),
        };
        assert_eq!(search.verified(first.clone(), first.clone()), None);
        let witness = search.verified(first, last).unwrap();
        assert_eq!(witness.original, "Some(0)");
        assert_eq!(witness.mutant, "Some(1)");
        assert_eq!(witness.assertion, "assert_eq!(f(&[0, 0], 0), Some(0));");
    }

    #[test]
    fn position_search_discovery_rejects_unsupported_syntax() {
        for (prefix, params, body) in [
            (
                "fn f",
                "xs: &[u8], x: u8",
                "xs.iter().rposition(|v| *v == x)",
            ),
            (
                "fn f",
                "xs: &[u8], x: u8",
                "xs.iter().position(|v| *v == x, x)",
            ),
            (
                "fn f",
                "xs: &[u8], x: u8",
                "xs.iter().position::<_>(|v| *v == x)",
            ),
            (
                "fn f",
                "xs: &[u8], x: u8",
                "xs.iter_mut().position(|v| *v == x)",
            ),
            (
                "fn f",
                "xs: &[u8], x: u8",
                "xs.iter::<_>().position(|v| *v == x)",
            ),
            (
                "fn f",
                "xs: &[u8], x: u8",
                "xs.iter(x).position(|v| *v == x)",
            ),
            (
                "fn f",
                "xs: &[u8], x: u8",
                "xs.iter().position(|v, _| *v == x)",
            ),
            (
                "fn f",
                "xs: &[u8], x: u8",
                "xs.iter().position(|v| *v == other)",
            ),
            ("fn f", "xs: &[u8], x: u8", "xs.iter().position(|v| v == x)"),
            (
                "fn f",
                "xs: &mut [u8], x: u8",
                "xs.iter().position(|v| *v == x)",
            ),
            (
                "fn f",
                "xs: &[f32], x: f32",
                "xs.iter().position(|v| *v == x)",
            ),
            (
                "fn f",
                "xs: &[<T>::u8], x: u8",
                "xs.iter().position(|v| *v == x)",
            ),
            (
                "fn f",
                "xs: &[u8], x: <T>::u8",
                "xs.iter().position(|v| *v == x)",
            ),
            (
                "fn f",
                "ref xs: &[u8], x: u8",
                "xs.iter().position(|v| *v == x)",
            ),
            (
                "fn f",
                "xs: &[u8], mut x: u8",
                "xs.iter().position(|v| *v == x)",
            ),
            (
                "fn f",
                "xs: &[u8], x @ _: u8",
                "xs.iter().position(|v| *v == x)",
            ),
            (
                "async fn f",
                "xs: &[u8], x: u8",
                "xs.iter().position(|v| *v == x)",
            ),
            (
                "unsafe fn f",
                "xs: &[u8], x: u8",
                "xs.iter().position(|v| *v == x)",
            ),
            (
                "const fn f",
                "xs: &[u8], x: u8",
                "xs.iter().position(|v| *v == x)",
            ),
            (
                "extern \"Rust\" fn f",
                "xs: &[u8], x: u8",
                "xs.iter().position(|v| *v == x)",
            ),
            (
                "fn f<T>",
                "xs: &[u8], x: u8",
                "xs.iter().position(|v| *v == x)",
            ),
        ] {
            let source = format!("{prefix}({params}) -> Option<usize> {{ {body} }}");
            assert!(syn::parse_file(&source).is_ok(), "{source}");
            assert!(position_searches(&source).is_empty(), "{source}");
        }
    }
}
