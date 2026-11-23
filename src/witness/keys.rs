//! Finds functions that put several parameters into one value (a `format!`, a tuple):
//! the value without one of them is what tests let through that never have two values
//! in one place which differ in that parameter alone.

use syn::parse::Parser;
use syn::spanned::Spanned;
use syn::{Expr, FnArg, Item, Pat, ReturnType, Stmt, Type};

use super::{Candidate, Edit};

/// A placeholder of a format string, as a byte range of the literal's source text, and
/// the parameter it shows: by name (`{name}`, `{name:>4}`) or the next argument (`{}`).
struct Placeholder {
    range: std::ops::Range<usize>,
    name: Option<String>,
}

/// The placeholders of a string literal as it is written, quotes included. `None` for
/// what is not understood: a raw string, a placeholder by number or with a width taken
/// from an argument.
fn placeholders(literal: &str) -> Option<Vec<Placeholder>> {
    let bytes = literal.as_bytes();
    if bytes.first() != Some(&b'"') || bytes.last() != Some(&b'"') || bytes.len() < 2 {
        return None;
    }
    let mut found = vec![];
    let mut at = 1;
    let end = bytes.len() - 1;
    while at < end {
        match bytes[at] {
            b'\\' => {
                // `\u{..}` has braces of its own.
                if bytes.get(at + 1) == Some(&b'u') {
                    at += literal[at..end].find('}')? + 1;
                } else {
                    at += 2;
                }
            }
            b'{' if bytes.get(at + 1) == Some(&b'{') => at += 2,
            b'}' if bytes.get(at + 1) == Some(&b'}') => at += 2,
            b'{' => {
                let close = at + literal[at..end].find('}')?;
                let inside = &literal[at + 1..close];
                let (name, spec) = inside.split_once(':').unwrap_or((inside, ""));
                if spec.contains('$') || spec.contains('*') {
                    return None;
                }
                let name = match name {
                    "" => None,
                    name if syn::parse_str::<syn::Ident>(name).is_ok() => Some(name.to_string()),
                    _ => return None,
                };
                found.push(Placeholder {
                    range: at..close + 1,
                    name,
                });
                at = close + 1;
            }
            b'}' => return None,
            _ => at += 1,
        }
    }
    Some(found)
}

/// A parameter as an element of a tuple or an argument: its name, borrowed or not, or
/// one method without arguments called on it (`to_owned`, `clone`, `into`).
fn parameter<'a>(expr: &Expr, names: &[&'a syn::Ident]) -> Option<&'a syn::Ident> {
    match expr {
        Expr::Path(_) => {
            let name = super::path_ident(expr)?;
            names.iter().copied().find(|known| *known == name)
        }
        Expr::Reference(reference) => parameter(&reference.expr, names),
        Expr::Paren(inner) => parameter(&inner.expr, names),
        Expr::Unary(unary) if matches!(unary.op, syn::UnOp::Deref(_)) => {
            parameter(&unary.expr, names)
        }
        Expr::MethodCall(call) if call.args.is_empty() => parameter(&call.receiver, names),
        _ => None,
    }
}

fn returns_one_value_of_parts(output: &ReturnType) -> bool {
    let ReturnType::Type(_, ty) = output else {
        return false;
    };
    match ty.as_ref() {
        Type::Path(path) => path.path.is_ident("String"),
        Type::Tuple(tuple) => tuple.elems.len() >= 2,
        _ => false,
    }
}

pub(super) fn discover(source: &str, file: &syn::File, out: &mut Vec<Candidate>) {
    for item in &file.items {
        let Item::Fn(function) = item else { continue };
        let signature = &function.sig;
        if function.attrs.iter().any(|attr| {
            !["doc", "inline", "must_use"]
                .iter()
                .any(|name| attr.path().is_ident(name))
        }) || !signature.generics.type_params().next().is_none()
            || signature.asyncness.is_some()
            || signature.unsafety.is_some()
            || signature.constness.is_some()
            || signature.abi.is_some()
            || !returns_one_value_of_parts(&signature.output)
        {
            continue;
        }
        let names: Vec<&syn::Ident> = signature
            .inputs
            .iter()
            .filter_map(|input| match input {
                FnArg::Typed(typed) => match typed.pat.as_ref() {
                    Pat::Ident(name) => Some(&name.ident),
                    _ => None,
                },
                FnArg::Receiver(_) => None,
            })
            .collect();
        let [Stmt::Expr(body, None)] = &function.block.stmts[..] else {
            continue;
        };
        let line = body.span().start().line..=body.span().end().line;
        let mut push = |span: std::ops::Range<usize>, replacement: String, without: &syn::Ident| {
            if source.get(span.clone()).is_none() {
                return;
            }
            out.push(Candidate {
                description: format!("`{}` (without `{without}`)", crate::report::one_line(&replacement)),
                edits: vec![Edit { span, replacement }],
                scope: line.clone(),
                signature_lines: vec![signature.span().start().line],
                existing: false,
                advice: Some(format!(
                    "no test has two values in one place that differ in `{without}` alone: add one that puts two such values together and checks that they stay apart"
                )),
                scenarios: vec![],
            });
        };
        match body {
            Expr::Macro(call) if call.mac.path.is_ident("format") => {
                let Ok(args) =
                    syn::punctuated::Punctuated::<Expr, syn::Token![,]>::parse_terminated
                        .parse2(call.mac.tokens.clone())
                else {
                    continue;
                };
                let mut args = args.iter();
                let Some(Expr::Lit(text)) = args.next() else {
                    continue;
                };
                let rest: Vec<&Expr> = args.collect();
                let Some(literal) = source.get(text.span().byte_range()) else {
                    continue;
                };
                let Some(holders) = placeholders(literal) else {
                    continue;
                };
                // Which parameter each placeholder shows; all of them, or it is not a
                // value made of parameters alone.
                let mut next = rest.iter();
                let Some(shown) = holders
                    .iter()
                    .map(|holder| match &holder.name {
                        Some(name) => names.iter().copied().find(|known| *known == name),
                        None => parameter(next.next()?, &names),
                    })
                    .collect::<Option<Vec<_>>>()
                else {
                    continue;
                };
                if next.next().is_some() || shown.len() < 2 {
                    continue;
                }
                for (dropped, holder) in holders.iter().enumerate() {
                    // The same parameter shown twice is not gone with one of them.
                    if shown.iter().filter(|name| **name == shown[dropped]).count() != 1 {
                        continue;
                    }
                    let mut literal = literal.to_string();
                    literal.replace_range(holder.range.clone(), "");
                    let positional = holders[..dropped]
                        .iter()
                        .filter(|earlier| earlier.name.is_none())
                        .count();
                    let kept: Vec<&str> = rest
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| holder.name.is_some() || *index != positional)
                        .filter_map(|(_, arg)| source.get(arg.span().byte_range()))
                        .collect();
                    let mut tokens = literal;
                    for arg in kept {
                        tokens.push_str(", ");
                        tokens.push_str(arg);
                    }
                    push(
                        call.span().byte_range(),
                        format!("format!({tokens})"),
                        shown[dropped],
                    );
                }
            }
            Expr::Tuple(tuple) if tuple.elems.len() >= 2 => {
                let Some(shown) = tuple
                    .elems
                    .iter()
                    .map(|element| parameter(element, &names))
                    .collect::<Option<Vec<_>>>()
                else {
                    continue;
                };
                for (dropped, element) in tuple.elems.iter().enumerate() {
                    if shown.iter().filter(|name| **name == shown[dropped]).count() != 1 {
                        continue;
                    }
                    push(
                        element.span().byte_range(),
                        "Default::default()".to_string(),
                        shown[dropped],
                    );
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What each candidate makes of the source, and how it is described.
    fn mutants(source: &str) -> Vec<(String, String)> {
        let mut candidates = Vec::new();
        discover(source, &syn::parse_file(source).unwrap(), &mut candidates);
        candidates
            .into_iter()
            .map(|candidate| {
                assert!(candidate.scenarios.is_empty() && !candidate.existing);
                assert!(
                    candidate
                        .advice
                        .as_ref()
                        .unwrap()
                        .starts_with("no test has two values in one place that differ in `")
                );
                assert_eq!(candidate.signature_lines, [1]);
                let edit = &candidate.edits[0];
                let mut text = source.to_string();
                text.replace_range(edit.span.clone(), &edit.replacement);
                syn::parse_file(&text).unwrap_or_else(|e| panic!("{e}: {text}"));
                (text, candidate.description)
            })
            .collect()
    }

    fn pair(text: &str, description: &str) -> (String, String) {
        (text.to_string(), description.to_string())
    }

    #[test]
    fn a_key_is_tried_without_each_of_its_parts() {
        let source =
            r#"fn key(language: &str, slug: &str) -> String { format!("{language}/{slug}") }"#;
        assert_eq!(
            mutants(source),
            [
                pair(
                    r#"fn key(language: &str, slug: &str) -> String { format!("/{slug}") }"#,
                    r#"`format!("/{slug}")` (without `language`)"#
                ),
                pair(
                    r#"fn key(language: &str, slug: &str) -> String { format!("{language}/") }"#,
                    r#"`format!("{language}/")` (without `slug`)"#
                ),
            ]
        );
        // Arguments in their order, borrowed or converted, and a format of their own.
        let source = r#"fn key(tenant: u32, id: &str, v: u8) -> String { format!("{}:{:>4}:{v}", tenant, id.trim()) }"#;
        assert_eq!(
            mutants(source),
            [
                pair(
                    r#"fn key(tenant: u32, id: &str, v: u8) -> String { format!(":{:>4}:{v}", id.trim()) }"#,
                    r#"`format!(":{:>4}:{v}", id.trim())` (without `tenant`)"#
                ),
                pair(
                    r#"fn key(tenant: u32, id: &str, v: u8) -> String { format!("{}::{v}", tenant) }"#,
                    r#"`format!("{}::{v}", tenant)` (without `id`)"#
                ),
                pair(
                    r#"fn key(tenant: u32, id: &str, v: u8) -> String { format!("{}:{:>4}:", tenant, id.trim()) }"#,
                    r#"`format!("{}:{:>4}:", tenant, id.trim())` (without `v`)"#
                ),
            ]
        );
    }

    #[test]
    fn a_tuple_is_tried_without_each_of_its_parts() {
        let source = "fn key(tenant: u32, id: &str) -> (u32, String) { (tenant, id.to_owned()) }";
        assert_eq!(
            mutants(source),
            [
                pair(
                    "fn key(tenant: u32, id: &str) -> (u32, String) { (Default::default(), id.to_owned()) }",
                    "`Default::default()` (without `tenant`)"
                ),
                pair(
                    "fn key(tenant: u32, id: &str) -> (u32, String) { (tenant, Default::default()) }",
                    "`Default::default()` (without `id`)"
                ),
            ]
        );
        let borrowed = "fn key<'a>(a: &'a str, b: &'a str) -> (&'a str, &'a str) { (a, &*b) }";
        assert_eq!(mutants(borrowed).len(), 2);
    }

    #[test]
    fn what_is_not_one_value_made_of_parameters_is_left_alone() {
        for source in [
            // One part, or the same one twice: nothing to leave out.
            r#"fn f(a: &str) -> String { format!("key/{a}") }"#,
            r#"fn f(a: &str, b: &str) -> String { format!("{a}/{a}") }"#,
            // Parts that are not parameters, or not understood.
            r#"fn f(a: &str, b: &str) -> String { format!("{a}/{}", other()) }"#,
            r#"fn f(a: &str, b: &str) -> String { format!("{a}/{c}") }"#,
            r#"fn f(a: &str, b: &str) -> String { format!("{0}/{1}", a, b) }"#,
            r#"fn f(a: &str, b: usize) -> String { format!("{a:>b$}") }"#,
            r#"fn f(a: &str, b: usize) -> String { format!("{a:>1$}/{}", a, b) }"#,
            r#"fn f(a: &str, b: usize) -> String { format!("{:.*}/{a}", b, 1.0) }"#,
            r#"fn f(a: &str, b: &str) -> String { format!("{a}/{b}", a = b) }"#,
            r#"fn f(a: &str, b: &str) -> String { format!("{a}/{}", b, a) }"#,
            r#"fn f(a: &str, b: &str) -> String { format!(r"{a}/{b}") }"#,
            r#"fn f(a: &str, b: &str) -> String { format!("{a}/{b}}") }"#,
            r#"fn f(a: &str, b: &str) -> String { format!(concat!("{}", "{}"), a, b) }"#,
            r#"fn f(a: &str, b: &str) -> String { format!() }"#,
            "fn f(a: u8, b: u8) -> (u8, u8) { (a, b + 1) }",
            "fn f(a: u8, b: u8) -> (u8, u8) { (a, a) }",
            // Not the whole body, or not that kind of value.
            r#"fn f(a: &str, b: &str) -> String { let key = format!("{a}/{b}"); key }"#,
            r#"fn f(a: &str, b: &str) -> String { println!("{a}/{b}"); String::new() }"#,
            r#"fn f(a: &str, b: &str) { format!("{a}/{b}"); }"#,
            r#"fn f(a: &str, b: &str) -> Box<str> { format!("{a}/{b}").into() }"#,
            "fn f(a: u8, b: u8) -> [u8; 2] { [a, b] }",
            "fn f(a: u8) -> (u8,) { (a,) }",
            // Functions that are more than plain.
            r#"fn f<T: std::fmt::Display>(a: T, b: T) -> String { format!("{a}/{b}") }"#,
            r#"async fn f(a: &str, b: &str) -> String { format!("{a}/{b}") }"#,
            r#"unsafe fn f(a: &str, b: &str) -> String { format!("{a}/{b}") }"#,
            r#"const fn f(a: u8, b: u8) -> (u8, u8) { (a, b) }"#,
            r#"extern "C" fn f(a: u8, b: u8) -> (u8, u8) { (a, b) }"#,
            r#"#[cfg(unix)] fn f(a: &str, b: &str) -> String { format!("{a}/{b}") }"#,
            r#"struct S; impl S { fn f(a: &str, b: &str) -> String { format!("{a}/{b}") } }"#,
        ] {
            assert_eq!(mutants(source), [], "{source}");
        }
    }

    #[test]
    fn braces_that_are_text_are_not_placeholders() {
        let source = r#"fn f(a: &str, b: &str) -> String { format!("{{{a}}}\u{1F600}\"\\{b}") }"#;
        let all = mutants(source);
        assert_eq!(all.len(), 2);
        assert!(
            all[0].0.contains(r#"format!("{{}}\u{1F600}\"\\{b}")"#),
            "{}",
            all[0].0
        );
        assert!(
            all[1].0.contains(r#"format!("{{{a}}}\u{1F600}\"\\")"#),
            "{}",
            all[1].0
        );
        for literal in ["\"{a\"", "\"a}\"", "\"\\u{12\"", "{a}", "\"", "\"{a b}\""] {
            assert!(placeholders(literal).is_none(), "{literal}");
        }
        assert_eq!(placeholders("\"\"").map(|found| found.len()), Some(0));
    }
}
