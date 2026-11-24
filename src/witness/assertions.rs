//! Calls that a test of the file already makes, with a result it does not pin down.
//!
//! A mutant that passes such a test may still change what the call returns: the test
//! then has the input, and only its assertion is too weak (`assert_ne!(bucket(3), "none")`
//! passes for "few" and for "many"). The calls found here are run on the original and
//! on the mutant; where the results differ, the stronger assertion is the finding's advice.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use syn::parse::Parser;
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::{Expr, Item, ItemFn, ReturnType};

use super::sequences::{Test, collect_tests};
use super::{Placement, Scenario};

/// At most this many calls are tried for one function.
const MAX_CALLS: usize = 4;

/// A function of the file and the calls its tests make of it.
#[derive(Debug)]
pub(crate) struct CalledFunction {
    /// Byte range of the whole function.
    pub item: Range<usize>,
    pub signature_line: usize,
    pub scenarios: Vec<Scenario>,
}

/// The functions at the top of the file that a test calls with nothing but literals,
/// somewhere other than as one side of an `assert_eq!`.
pub(crate) fn unpinned_calls(source: &str, file: &syn::File) -> Vec<CalledFunction> {
    let functions: BTreeMap<String, &ItemFn> = file
        .items
        .iter()
        .filter_map(|item| match item {
            Item::Fn(function) if callable(function) => {
                Some((function.sig.ident.to_string(), function))
            }
            _ => None,
        })
        .collect();
    if functions.is_empty() {
        return vec![];
    }
    let mut tests = Vec::new();
    collect_tests(&file.items, "", None, &mut tests);
    let mut found: BTreeMap<&str, Vec<Scenario>> = BTreeMap::new();
    for test in tests.iter().filter(|test| plain(test.function)) {
        // Nested `fn` items are hoisted across the block, while `let` bindings take
        // effect only after their initialiser (so `let f = f(3);` still finds `f(3)`).
        let mut shadowed = item_fn_names(&test.function.block);
        let body = test.function.block.brace_token.span.open().byte_range().end;
        let end = test
            .function
            .block
            .brace_token
            .span
            .close()
            .byte_range()
            .start;
        for stmt in &test.function.block.stmts {
            if looks(stmt) {
                let mut calls = Calls {
                    source,
                    names: &functions,
                    found: vec![],
                };
                calls.visit_stmt(stmt);
                for (name, call, at) in calls.found {
                    let (Some(before), Some(after)) =
                        (source.get(body..at.start), source.get(at.end..end))
                    else {
                        continue;
                    };
                    if shadowed.contains(name) {
                        continue;
                    }
                    // The same call written twice is two calls: the test may get to one
                    // and not to the other, or change something in between.
                    let scenarios = found.entry(name).or_default();
                    if scenarios.len() < MAX_CALLS {
                        let line_start = source[..at.start].rfind('\n').map_or(0, |end| end + 1);
                        let place = (
                            source[..at.start].matches('\n').count() + 1,
                            source[line_start..at.start].chars().count() + 1,
                        );
                        scenarios.push(scenario(test, before, call, after, place));
                    }
                }
            }
            if let syn::Stmt::Local(local) = stmt {
                collect_pat_idents(&local.pat, &mut shadowed);
            }
        }
    }
    found
        .into_iter()
        .map(|(name, scenarios)| {
            let function = functions[name];
            CalledFunction {
                item: function.span().byte_range(),
                signature_line: function.sig.span().start().line,
                scenarios,
            }
        })
        .collect()
}

/// The test is run again as it is written, and the value of the call is recorded where
/// the test makes it: after everything the test does before, in the order the test does
/// it, and only if the test gets there. What the test does afterwards it does here too.
fn scenario(
    test: &Test<'_>,
    before: &str,
    call: String,
    after: &str,
    at: (usize, usize),
) -> Scenario {
    let name = test.name();
    Scenario {
        setup: before.to_owned(),
        after: Some(after.to_owned()),
        call_at: Some(at),
        expression: call,
        placement: test.insertion.map(|offset| Placement {
            offset,
            module: test.module.clone(),
        }),
        source_test: Some(name),
        ..Scenario::default()
    }
}

/// A statement of a test that only looks at results: an assertion or a `let`, always
/// part of the test (no `cfg` on it). Only what the test itself does, one statement after
/// the other, is taken: not what is in a loop or a branch, which may run never or often.
fn looks(stmt: &syn::Stmt) -> bool {
    match stmt {
        // Which macros are assertions is for `Calls` to say.
        syn::Stmt::Macro(stmt) => stmt.attrs.is_empty(),
        syn::Stmt::Expr(Expr::Macro(expr), _) => expr.attrs.is_empty(),
        syn::Stmt::Local(local) => {
            local.attrs.is_empty()
                && local
                    .init
                    .as_ref()
                    .is_some_and(|init| init.diverge.is_none())
        }
        _ => false,
    }
}

/// A test that runs as it is written and passes by returning: not one that is ignored,
/// and not one that is meant to panic (a stronger assertion there is one more way to).
fn plain(test: &ItemFn) -> bool {
    test.attrs
        .iter()
        .all(|attr| attr.path().is_ident("test") || attr.path().is_ident("doc"))
        && test.sig.inputs.is_empty()
        && matches!(test.sig.output, ReturnType::Default)
        && test.sig.asyncness.is_none()
}

/// Nested `fn` items declared in the block (visible throughout the block in Rust).
fn item_fn_names(block: &syn::Block) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for stmt in &block.stmts {
        if let syn::Stmt::Item(Item::Fn(function)) = stmt {
            names.insert(function.sig.ident.to_string());
        }
    }
    names
}

fn collect_pat_idents(pat: &syn::Pat, names: &mut BTreeSet<String>) {
    struct PatIdents<'a>(&'a mut BTreeSet<String>);
    impl<'ast> Visit<'ast> for PatIdents<'_> {
        fn visit_pat_ident(&mut self, pat: &'ast syn::PatIdent) {
            self.0.insert(pat.ident.to_string());
            syn::visit::visit_pat_ident(self, pat);
        }
    }
    PatIdents(names).visit_pat(pat);
}

/// A function a test can call by its name alone and whose result can be looked at.
fn callable(function: &ItemFn) -> bool {
    let signature = &function.sig;
    super::simple::supports(function)
        && signature.constness.is_none()
        && matches!(&signature.output, ReturnType::Type(_, ty) if !unit_or_never(ty))
}

fn unit_or_never(ty: &syn::Type) -> bool {
    match ty {
        syn::Type::Tuple(tuple) => tuple.elems.is_empty(),
        syn::Type::Never(_) => true,
        _ => false,
    }
}

/// An argument whose value is the same wherever it is written: no variable of the test,
/// no call, nothing that is moved or changed by being evaluated once more.
fn literal(expr: &Expr) -> bool {
    match expr {
        Expr::Lit(value) => !matches!(value.lit, syn::Lit::Verbatim(_)),
        Expr::Unary(value) if matches!(value.op, syn::UnOp::Neg(_)) => {
            matches!(value.expr.as_ref(), Expr::Lit(number) if matches!(number.lit, syn::Lit::Int(_) | syn::Lit::Float(_)))
        }
        Expr::Reference(reference) => reference.mutability.is_none() && literal(&reference.expr),
        Expr::Array(array) => array.elems.iter().all(literal),
        Expr::Tuple(tuple) => tuple.elems.iter().all(literal),
        Expr::Paren(paren) => literal(&paren.expr),
        _ => false,
    }
}

struct Calls<'a> {
    source: &'a str,
    names: &'a BTreeMap<String, &'a ItemFn>,
    found: Vec<(&'a str, String, Range<usize>)>,
}

impl<'a> Calls<'a> {
    /// The call as it is written, if it is one of a function of the file with literal
    /// arguments, as many as the function takes.
    fn call(&self, call: &syn::ExprCall) -> Option<(&'a str, String)> {
        let Expr::Path(path) = call.func.as_ref() else {
            return None;
        };
        // An attribute of the call may leave it out. A path that is one name has no
        // type before it, and an attribute in front of it is the call's.
        if !call.attrs.is_empty() {
            return None;
        }
        let name = path.path.get_ident()?.to_string();
        let (name, function) = self.names.get_key_value(&name)?;
        if function.sig.inputs.len() != call.args.len() || !call.args.iter().all(literal) {
            return None;
        }
        // The call as the test has it, to the letter: space inside a text is part of
        // the argument.
        let text = self.source.get(call.span().byte_range())?;
        (text.chars().count() <= 100 && !text.contains('\n'))
            .then(|| (name.as_str(), text.to_owned()))
    }

    /// Whether the expression calls a function of the file by its name, with whatever
    /// arguments.
    fn calls_the_file(&self, expr: &Expr) -> bool {
        struct Named<'a>(&'a BTreeMap<String, &'a ItemFn>, bool);
        impl Named<'_> {
            /// A name of the file with parentheses after it, anywhere in what a macro
            /// is given (`format!("{}", f(2))`, `dbg!(f(2))`).
            fn tokens(&mut self, tokens: proc_macro2::TokenStream) {
                let mut named = false;
                for token in tokens {
                    match token {
                        proc_macro2::TokenTree::Ident(name) => {
                            named = self.0.contains_key(&name.to_string());
                        }
                        proc_macro2::TokenTree::Group(group) => {
                            if named && group.delimiter() == proc_macro2::Delimiter::Parenthesis {
                                self.1 = true;
                            }
                            named = false;
                            self.tokens(group.stream());
                        }
                        _ => named = false,
                    }
                }
            }
        }
        impl<'ast> Visit<'ast> for Named<'_> {
            fn visit_macro(&mut self, mac: &'ast syn::Macro) {
                self.tokens(mac.tokens.clone());
            }
            fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
                // By its name alone or through a path (`super::f(2)`).
                if let Expr::Path(path) = call.func.as_ref()
                    && path
                        .path
                        .segments
                        .last()
                        .is_some_and(|name| self.0.contains_key(&name.ident.to_string()))
                {
                    self.1 = true;
                }
                syn::visit::visit_expr_call(self, call);
            }
        }
        let mut named = Named(self.names, false);
        named.visit_expr(expr);
        named.1
    }
}

fn assertion(mac: &syn::Macro) -> bool {
    [
        "assert",
        "assert_eq",
        "assert_ne",
        "debug_assert",
        "debug_assert_eq",
        "debug_assert_ne",
    ]
    .iter()
    .any(|name| mac.path.is_ident(name))
}

impl<'ast> Visit<'ast> for Calls<'_> {
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        let at = call.span().byte_range();
        if let Some((name, text)) = self.call(call) {
            self.found.push((name, text, at));
        }
        syn::visit::visit_expr_call(self, call);
    }

    /// What a closure calls is called when the closure is, if ever.
    fn visit_expr_closure(&mut self, _: &'ast syn::ExprClosure) {}
    /// A block may leave before it gets to a call.
    fn visit_block(&mut self, _: &'ast syn::Block) {}

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if !assertion(mac) {
            return;
        }
        let Ok(args) = syn::punctuated::Punctuated::<Expr, syn::Token![,]>::parse_terminated
            .parse2(mac.tokens.clone())
        else {
            return;
        };
        let compares = mac.path.is_ident("assert_eq") || mac.path.is_ident("debug_assert_eq");
        let differs = mac.path.is_ident("assert_ne") || mac.path.is_ident("debug_assert_ne");
        for (index, arg) in args.iter().enumerate() {
            // The message of an assertion is no call of the test.
            let asserted = index < if compares || differs { 2 } else { 1 };
            if !asserted {
                continue;
            }
            // One side of an `assert_eq!` is pinned down already, negated or not, and so
            // is what an `assert!` takes as it is or negated: a mutant that passes the
            // test returns that very value. Not where the other side calls a function
            // of the file too: `assert_eq!(f(1), f(2))` holds for any value both have.
            let other = args.iter().nth(1 - index.min(1)).filter(|_| compares);
            let pinned = match arg {
                _ if differs => false,
                _ if other.is_some_and(|other| self.calls_the_file(other)) => false,
                Expr::Unary(not) if matches!(not.op, syn::UnOp::Not(_)) => {
                    matches!(not.expr.as_ref(), Expr::Call(call) if self.call(call).is_some())
                }
                Expr::Call(call) => self.call(call).is_some(),
                _ => false,
            };
            if !pinned {
                // Spans of a macro's own tokens are those of the file: the text of a
                // call in it can be read from the source like any other.
                Visit::visit_expr(self, arg);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calls(source: &str) -> Vec<(String, Option<String>, Vec<String>)> {
        let file = syn::parse_file(source).unwrap();
        unpinned_calls(source, &file)
            .into_iter()
            .map(|function| {
                let name = source[function.item.clone()]
                    .split('(')
                    .next()
                    .unwrap()
                    .rsplit(' ')
                    .next()
                    .unwrap()
                    .to_string();
                let test = function.scenarios[0].source_test.clone();
                let calls = function
                    .scenarios
                    .into_iter()
                    .map(|scenario| scenario.expression)
                    .collect();
                (name, test, calls)
            })
            .collect()
    }

    const BUCKET: &str =
        "pub fn bucket(n: u32) -> &'static str { if n == 0 { \"none\" } else { \"few\" } }\n";

    #[test]
    fn a_call_with_a_weak_assertion_is_found_with_its_test() {
        let source = format!(
            "{BUCKET}#[cfg(test)] mod tests {{ use super::*;\n\
             #[test] fn buckets() {{ assert_eq!(bucket(0), \"none\"); assert_ne!(bucket(3), \"none\"); }} }}"
        );
        assert_eq!(
            calls(&source),
            [(
                "bucket".to_string(),
                Some("tests::buckets".to_string()),
                vec!["bucket(3)".to_string()]
            )]
        );
        let file = syn::parse_file(&source).unwrap();
        let found = unpinned_calls(&source, &file);
        assert_eq!(found[0].signature_line, 1);
        assert_eq!(&source[found[0].item.clone()], BUCKET.trim_end());
        let scenario = &found[0].scenarios[0];
        // The test that looks is put where the test that calls is.
        let placement = scenario.placement.as_ref().unwrap();
        assert_eq!(placement.module, "tests");
        assert_eq!(placement.offset, source.len() - 1);
        assert_eq!(scenario.source_test.as_deref(), Some("tests::buckets"));
        assert_eq!(scenario.guidance, None);
    }

    #[test]
    fn only_a_few_calls_are_tried_for_a_function() {
        let asserts: String = (0..9)
            .map(|n| format!("assert!(bucket({n}) != \"\"); assert!(bucket({n}) != \"x\");"))
            .collect();
        let source = format!("{BUCKET}#[test] fn many() {{ {asserts} }}");
        assert_eq!(
            calls(&source),
            [(
                "bucket".to_string(),
                Some("many".to_string()),
                vec![
                    "bucket(0)".to_string(),
                    "bucket(0)".into(),
                    "bucket(1)".into(),
                    "bucket(1)".into()
                ]
            )]
        );
        let file = syn::parse_file(&source).unwrap();
        assert!(
            unpinned_calls(&source, &file)[0].scenarios[0]
                .placement
                .is_none()
        );
    }

    #[test]
    fn what_is_looked_at_and_what_is_not() {
        let found = |test: &str| {
            let source =
                format!("{BUCKET}fn other(n: u32) -> u32 {{ n }}\n#[test] fn t() {{ {test} }}");
            calls(&source)
                .into_iter()
                .flat_map(|(_, _, calls)| calls)
                .collect::<Vec<_>>()
        };
        // Either side of a comparison is pinned down; what is inside a call is not.
        assert_eq!(found("assert_eq!(\"few\", bucket(3));"), [] as [&str; 0]);
        assert_eq!(found("assert_eq!(bucket(3).len(), 3);"), ["bucket(3)"]);
        assert_eq!(
            found("assert_eq!(other(1), 1, \"{}\", bucket(3));"),
            [] as [&str; 0]
        );
        assert_eq!(
            found("assert!(bucket(3).starts_with('f'), \"{}\", bucket(4));"),
            ["bucket(3)"]
        );
        assert_eq!(
            found("debug_assert_ne!(bucket( 3 ), \"none\");"),
            ["bucket( 3 )"]
        );
        assert_eq!(
            found("let b = bucket(3); assert!(!b.is_empty());"),
            ["bucket(3)"]
        );
        // What an `assert!` takes as it is, or negated, can only be one value.
        assert_eq!(
            found("assert!(other(1) == 1); assert!(!(other(2) == 2));"),
            ["other(1)", "other(2)"]
        );
        let flag = |test: &str| {
            let source = format!("fn on(n: u32) -> bool {{ n > 1 }}\n#[test] fn t() {{ {test} }}");
            calls(&source)
                .into_iter()
                .flat_map(|(_, _, calls)| calls)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            flag("assert!(on(2)); assert!(!on(1)); debug_assert!(on(3), \"{}\", on(4));"),
            [] as [&str; 0]
        );
        assert_eq!(
            flag("assert!(on(2) && !on(1)); assert_ne!(on(5), false);"),
            ["on(2)", "on(1)", "on(5)"]
        );
        assert_eq!(found("println!(\"{}\", bucket(3));"), [] as [&str; 0]);
        // Arguments that are not the same wherever they are written, or not as many.
        assert_eq!(
            found("let n = 3; assert_ne!(bucket(n), \"none\");"),
            [] as [&str; 0]
        );
        assert_eq!(
            found("assert_ne!(bucket(other(3)), \"none\");"),
            ["other(3)"]
        );
        assert_eq!(
            found("assert_ne!(self::bucket(3), \"none\");"),
            [] as [&str; 0]
        );
        assert_eq!(
            found("assert_ne!(bucket(-3 as u32), \"none\");"),
            [] as [&str; 0]
        );
        assert_eq!(
            found("assert_ne!(other((1)), 2); assert_ne!(other(-1i32 as u32), 2);"),
            ["other((1))"]
        );
    }

    #[test]
    fn each_rule_leaves_out_what_it_is_for() {
        let none = [] as [&str; 0];
        // A statement that is not always part of the test, and one that may leave it.
        assert_eq!(
            found_in("#[cfg(unix)] assert_ne!(bucket(3), \"none\");"),
            none
        );
        assert_eq!(
            found_in("#[cfg(unix)] assert_ne!(bucket(3), \"none\")"),
            none
        );
        assert_eq!(
            found_in("let Some(n) = Some(bucket(3).len()) else { return; };"),
            none
        );
        assert_eq!(found_in("let n = Some(bucket(3).len());"), ["bucket(3)"]);
        // The last statement, with no semicolon, is a statement like the others.
        assert_eq!(found_in("assert_ne!(bucket(3), \"none\")"), ["bucket(3)"]);
        assert_eq!(found_in("assert_ne!(<u8>::bucket(3), \"none\");"), none);
        // Only an assertion is known to evaluate what it is given, once and in order.
        assert_eq!(found_in("let all = vec![bucket(3).len()];"), none);
        assert_eq!(found_in("matches!(bucket(3).len(), 3);"), none);
        // A name bound anywhere in a pattern.
        assert_eq!(
            found_in("let whole @ bucket = size; assert_ne!(bucket(\"3\"), 4);"),
            none
        );
        // Arguments: a negated number is a literal and a negated flag is computed, a
        // reference is one to a literal, and there are as many as the function takes.
        assert_eq!(found_in("assert!(size(!1) > 0);"), none);
        assert_eq!(
            found_in("let text = \"a\"; assert!(size(&text) > 0);"),
            none
        );
        assert_eq!(found_in("assert!(size(&mut \"a\") > 0);"), none);
        assert_eq!(found_in("assert!(size(&\"a\") > 0);"), ["size(&\"a\")"]);
        assert_eq!(found_in("assert_ne!(bucket(3, 4), \"none\");"), none);
        assert_eq!(found_in("assert_ne!(bucket(), \"none\");"), none);
        // A call with an attribute may not be there at all.
        assert_eq!(
            found_in("assert_ne!(#[cfg(unix)] bucket(3), \"none\");"),
            none
        );
        // The call is a line of the report: not more than a hundred characters.
        let long = |length: usize| format!("size(\"{}\")", "é".repeat(length - 8));
        assert_eq!(
            found_in(&format!("assert!({} > 0);", long(100))),
            [long(100)]
        );
        assert_eq!(found_in(&format!("assert!({} > 0);", long(101))), none);
        // Both sides of a comparison are looked at, of either kind and either name, and
        // nothing after them.
        for (assertion, found) in [
            ("debug_assert_eq!(bucket(3), \"few\");", vec![]),
            ("debug_assert_eq!(1, bucket(3).len());", vec!["bucket(3)"]),
            ("assert_eq!(1, bucket(3).len());", vec!["bucket(3)"]),
            ("assert_ne!(\"none\", bucket(3));", vec!["bucket(3)"]),
            ("debug_assert_ne!(\"none\", bucket(3));", vec!["bucket(3)"]),
            (
                "assert_ne!(size(\"a\"), 2, bucket(3).len());",
                vec!["size(\"a\")"],
            ),
            (
                "assert_eq!(size(\"a\") + 1, 2, bucket(3).len());",
                vec!["size(\"a\")"],
            ),
            (
                "assert!(size(\"a\") > 0, bucket(3).len());",
                vec!["size(\"a\")"],
            ),
            // What is negated is pinned down like what is not, where it is compared
            // and where it is asserted; another operator leaves the call to be seen.
            ("assert_eq!(!bucket(3), 1);", vec![]),
            // Two calls that are compared with each other pin nothing down.
            (
                "assert_eq!(bucket(1), bucket(2));",
                vec!["bucket(1)", "bucket(2)"],
            ),
            (
                "assert_eq!(!bucket(3), !bucket(4));",
                vec!["bucket(3)", "bucket(4)"],
            ),
            ("assert_eq!(bucket(1), size(text) + 1);", vec!["bucket(1)"]),
            ("assert_eq!(bucket(1), helper(2));", vec![]),
            (
                "assert_eq!(bucket(1), format!(\"{}\", bucket(2)));",
                vec!["bucket(1)"],
            ),
            (
                "assert_eq!(dbg!((bucket(2))), bucket(1));",
                vec!["bucket(1)"],
            ),
            (
                "assert_eq!(bucket(1), super::bucket(2));",
                vec!["bucket(1)"],
            ),
            ("assert_eq!(bucket(1), format!(\"{}\", bucket));", vec![]),
            ("assert_eq!(bucket(1), format!(\"{}\", helper(2)));", vec![]),
            ("assert_eq!(\"x\", bucket(1), \"{}\", size(\"a\"));", vec![]),
            ("assert!(!bucket(3));", vec![]),
            ("assert!(-bucket(3));", vec!["bucket(3)"]),
            ("assert_ne!(!bucket(3), 1);", vec!["bucket(3)"]),
        ] {
            assert_eq!(found_in(assertion), found, "{assertion}");
        }
    }

    #[test]
    fn a_test_with_parameters_or_one_that_is_async_is_not_asked() {
        for test in [
            "#[test] fn t(n: u8) { assert_ne!(bucket(3), \"none\"); }",
            "#[test] async fn t() { assert_ne!(bucket(3), \"none\"); }",
        ] {
            assert_eq!(calls(&format!("{BUCKET}{test}")), [], "{test}");
        }
    }

    fn found_in(test: &str) -> Vec<String> {
        let source = format!(
            "{BUCKET}fn size(text: &str) -> usize {{ text.len() }}\n#[test] fn t() {{ {test} }}"
        );
        calls(&source)
            .into_iter()
            .flat_map(|(_, _, calls)| calls)
            .collect()
    }

    #[test]
    fn the_call_is_taken_to_the_letter_from_where_the_test_makes_it() {
        assert_eq!(found_in("assert!(size(\"a  b\") > 1);"), ["size(\"a  b\")"]);
        assert_eq!(found_in("assert!(size(\n\"a\") > 0);"), [] as [&str; 0]);
        // What is in a loop, a branch, a block or a closure may run never or often.
        for nested in [
            "for n in 0..3 { assert_ne!(bucket(3), \"none\"); }",
            "if true { assert_ne!(bucket(3), \"none\"); }",
            "assert!({ bucket(3); true });",
            "assert!((|| bucket(3) != \"none\")());",
            "helper(bucket(3));",
        ] {
            assert_eq!(found_in(nested), [] as [&str; 0], "{nested}");
        }
        // A name the test binds itself is not the function of the file.
        assert_eq!(
            found_in("let bucket = size; assert_ne!(bucket(\"3\"), 4);"),
            [] as [&str; 0]
        );
        assert_eq!(
            found_in("fn bucket(n: u32) -> u32 { n } assert_ne!(bucket(3), 4);"),
            [] as [&str; 0]
        );
        assert_eq!(
            found_in("use super::bucket; assert_ne!(bucket(3), \"none\");"),
            ["bucket(3)"]
        );
    }

    #[test]
    fn the_test_is_run_again_as_written_with_the_value_recorded_on_its_way() {
        let source = format!(
            "{BUCKET}#[cfg(test)] mod tests {{\n#[test] fn t() {{ use super::bucket; make();\n\
             assert!(enable() && bucket(3) != \"none\"); remove(); }} }}"
        );
        let file = syn::parse_file(&source).unwrap();
        let found = unpinned_calls(&source, &file);
        let scenario = &found[0].scenarios[0];
        assert_eq!(scenario.expression, "bucket(3)");
        // Everything the test does, before the call and after it, in the test's order.
        assert_eq!(
            scenario.setup,
            " use super::bucket; make();\nassert!(enable() && "
        );
        assert_eq!(scenario.after.as_deref(), Some(" != \"none\"); remove(); "));
        let test = scenario
            .test_edit(&source, "__tmt_witness", Some("\"few\""))
            .replacement;
        let body = test
            .split_once("extern crate std as __tmt_witness_std;")
            .unwrap()
            .1;
        assert!(
            body.trim_start().starts_with(
                "use super::bucket; make();\nassert!(enable() && { let result = bucket(3);"
            ),
            "{test}"
        );
        assert!(body.contains("&result == &(\"few\")"), "{test}");
        assert!(
            body.trim_end()
                .ends_with("result } != \"none\"); remove(); \n}"),
            "{test}"
        );
        // A statement that is not always part of the test is not looked into.
        assert_eq!(
            found_in("#[cfg(unix)] let got = bucket(3);"),
            [] as [&str; 0]
        );
    }

    #[test]
    fn the_same_call_written_again_is_another_call() {
        let source = format!(
            "{BUCKET}#[test] fn a() {{ set(0); assert_ne!(bucket(3), \"none\"); }}\n\
             #[test] fn b() {{ set(1); assert_ne!(bucket(3), \"none\"); assert_ne!(bucket(3), \"x\"); }}"
        );
        let file = syn::parse_file(&source).unwrap();
        let found = unpinned_calls(&source, &file);
        let tests: Vec<_> = found[0]
            .scenarios
            .iter()
            .map(|s| (s.source_test.as_deref().unwrap(), s.setup.as_str()))
            .collect();
        assert_eq!(
            tests,
            [
                ("a", " set(0); assert_ne!("),
                ("b", " set(1); assert_ne!("),
                ("b", " set(1); assert_ne!(bucket(3), \"none\"); assert_ne!(")
            ]
        );
        // Each is on a line of the file (the function is on the first).
        let places: Vec<_> = found[0].scenarios.iter().map(|s| s.call_at).collect();
        assert_eq!(places, [Some((2, 37)), Some((3, 37)), Some((3, 68))]);
    }

    #[test]
    fn tests_that_are_ignored_or_meant_to_panic_are_not_asked() {
        for attribute in [
            "#[ignore]",
            "#[should_panic]",
            "#[cfg(unix)]",
            "#[tokio::test]",
        ] {
            let source = format!(
                "{BUCKET}#[test] {attribute} fn t() {{ assert_ne!(bucket(3), \"none\"); }}"
            );
            assert_eq!(calls(&source), [], "{attribute}");
        }
        let source = format!(
            "{BUCKET}#[test] fn t() -> Result<(), String> {{ assert_ne!(bucket(3), \"none\"); Ok(()) }}"
        );
        assert_eq!(calls(&source), []);
        let source =
            format!("{BUCKET}/// Looks.\n#[test] fn t() {{ assert_ne!(bucket(3), \"none\"); }}");
        assert_eq!(calls(&source).len(), 1);
    }

    #[test]
    fn arguments_that_are_literals() {
        let source = "fn total(values: &[i32], scale: (f64, bool), name: &str) -> i32 { 0 }\n\
                      fn broken(values: &mut [i32]) -> i32 { 0 }\n\
                      #[test] fn t() { assert!(total(&[1, -2], (0.5, true), \"a\") < 9); \
                      assert!(broken(&mut [1]) < 9); }";
        assert_eq!(
            calls(source),
            [(
                "total".to_string(),
                Some("t".to_string()),
                vec!["total(&[1, -2], (0.5, true), \"a\")".to_string()]
            )]
        );
    }

    #[test]
    fn functions_that_are_not_called_by_name_or_give_nothing() {
        for function in [
            "fn f<T: Copy>(n: T) -> T { n }",
            "async fn f(n: u8) -> u8 { n }",
            "unsafe fn f(n: u8) -> u8 { n }",
            "const fn f(n: u8) -> u8 { n }",
            "fn f(n: u8) { let _ = n; }",
            "fn f(n: u8) -> () { let _ = n; }",
            "fn f(n: u8) -> ! { loop {} }",
            "#[cfg(unix)] fn f(n: u8) -> u8 { n }",
            "struct S; impl S { fn f(n: u8) -> u8 { n } }",
        ] {
            let source = format!("{function}\n#[test] fn t() {{ assert!(f(1) != 0); }}");
            assert_eq!(calls(&source), [], "{function}");
        }
        assert_eq!(calls("fn f(n: u8) -> u8 { n }"), []);
    }

    #[test]
    fn pict_table() {
        let table = include_str!("../../tests/data/unpinned_calls.pict.tsv");
        for row in table.lines().skip(1) {
            let cells: Vec<&str> = row.split('\t').collect();
            let [place, args, callee, module] = cells[..] else {
                panic!("{row}")
            };
            let (parameter, argument) = match args {
                "literal" => ("n: u32", "3"),
                "variable" => ("n: u32", "n"),
                "no_argument" => ("", ""),
                _ => panic!("{row}"),
            };
            let (definition, call) = match callee {
                "free_fn" => (
                    format!("pub fn f({parameter}) -> u32 {{ 1 }}"),
                    format!("f({argument})"),
                ),
                "generic_fn" => (
                    format!("pub fn f<T>({parameter}) -> u32 {{ 1 }}"),
                    format!("f::<u8>({argument})"),
                ),
                "method" => (
                    format!("pub struct S; impl S {{ pub fn f({parameter}) -> u32 {{ 1 }} }}"),
                    format!("S::f({argument})"),
                ),
                "unit_fn" => (
                    format!("pub fn f({parameter}) {{}}"),
                    format!("f({argument})"),
                ),
                _ => panic!("{row}"),
            };
            let unit = callee == "unit_fn";
            let body = match (place, unit) {
                ("assert_ne", false) => format!("assert_ne!({call}, 2);"),
                ("assert_ne", true) => format!("assert_ne!({call}, panic!());"),
                ("assert_true", false) => format!("assert!({call} > 0);"),
                ("assert_true", true) => format!("assert!({call} == ());"),
                ("let_binding", _) => format!("let got = {call}; let _ = got;"),
                ("assert_eq_direct", false) => format!("assert_eq!({call}, 1);"),
                ("assert_eq_direct", true) => format!("assert_eq!({call}, ());"),
                ("assert_eq_nested", false) => format!("assert_eq!(u64::from({call}), 1);"),
                ("assert_eq_nested", true) => format!("assert_eq!(Some({call}), Some(()));"),
                _ => panic!("{row}"),
            };
            let test = format!("#[test] fn t() {{ let n = 3; let _ = n; {body} }}");
            let source = match module {
                "nested" => {
                    format!("{definition}\n#[cfg(test)] mod tests {{ use super::*; {test} }}")
                }
                "top" => format!("{definition}\n{test}"),
                _ => panic!("{row}"),
            };
            let expected = place != "assert_eq_direct" && args != "variable" && callee == "free_fn";
            let found = calls(&source);
            assert_eq!(found.len(), usize::from(expected), "{row}: {source}");
            if let Some((_, test, calls)) = found.first() {
                let name = if module == "nested" { "tests::t" } else { "t" };
                assert_eq!(test.as_deref(), Some(name), "{row}");
                assert_eq!(calls, &[call], "{row}");
            }
        }
    }
    /// Statements a test may consist of: calls that are looked at and calls that are
    /// not, and text that only looks like either.
    fn statement() -> impl proptest::strategy::Strategy<Value = String> {
        use proptest::prelude::*;
        let call = prop_oneof![
            (0..5u32).prop_map(|n| format!("bucket({n})")),
            (0..5u32).prop_map(|n| format!("on({n})")),
            Just("bucket( 3 )".to_string()),
            Just("bucket(\n        3,\n    )".to_string()),
            Just("bucket(n)".to_string()),
            Just("self::bucket(1)".to_string()),
            Just("other(2)".to_string()),
        ];
        let calls = move || call.clone();
        prop_oneof![
            calls().prop_map(|c| format!("assert_eq!({c}, \"none\");")),
            calls().prop_map(|c| format!("assert_ne!({c}, \"none\");")),
            calls().prop_map(|c| format!("assert_ne!(\n        {c},\n        \"none\",\n    );")),
            calls().prop_map(|c| format!("assert!({c} != \"few\", \"{{}}\", 1);")),
            (calls(), calls())
                .prop_map(|(c, d)| format!("assert!(!{c}.is_empty() || {d} == \"a\");")),
            (calls(), calls()).prop_map(|(c, d)| format!("assert_eq!(enable(2), {c} > {d});")),
            calls().prop_map(|c| format!("let got = {c};")),
            (calls(), calls()).prop_map(|(c, d)| format!("let pair = ({c}, [{d}]);")),
            calls().prop_map(|c| format!("let lazy = || {c};")),
            calls().prop_map(|c| format!("{{ let inner = {c}; }}")),
            calls().prop_map(|c| format!("#[cfg(unix)] let got = {c};")),
            calls().prop_map(|c| format!("let text = \"}} {c} {{ é日本\";")),
            calls().prop_map(|c| format!("// {c} }}\n")),
            calls().prop_map(|c| format!("/* {{ {c} */")),
            calls().prop_map(|c| format!("{c};")),
            calls().prop_map(|c| format!("if flag {{ assert_ne!({c}, \"none\"); }}")),
            calls().prop_map(|c| format!("let Some(got) = Some({c}) else {{ return; }};")),
            Just("let bucket = |n: u32| n;".to_string()),
            Just("let n = 3;".to_string()),
            Just("set('}');".to_string()),
            Just("fn helper() -> u32 { 0 }".to_string()),
        ]
    }

    proptest::proptest! {
        /// Whatever a test consists of, a call taken from it is a stretch of its body,
        /// between all that comes before it and all that comes after.
        #[test]
        fn a_call_is_taken_with_all_of_its_test_around_it(
            statements in proptest::collection::vec(statement(), 0..6),
            separator in proptest::prop_oneof![
                proptest::strategy::Just(" "),
                proptest::strategy::Just("\n    "),
            ],
            nested in proptest::bool::ANY,
        ) {
            let body = format!(" {}{separator}", statements.join(separator));
            let test = format!("#[test] fn looks() {{{body}}}");
            let functions = format!(
                "{BUCKET}pub fn on(n: u32) -> bool {{ n > 1 }}\nfn other(n: u32) -> u32 {{ n }}\n"
            );
            let source = if nested {
                format!("{functions}#[cfg(test)] mod tests {{ use super::*;\n{test}\n}}\n")
            } else {
                format!("{functions}{test}\n")
            };
            let Ok(file) = syn::parse_file(&source) else {
                return Ok(());
            };
            for function in unpinned_calls(&source, &file) {
                let name = source[function.item.clone()]
                    .split('(')
                    .next()
                    .unwrap()
                    .rsplit(' ')
                    .next()
                    .unwrap()
                    .to_string();
                proptest::prop_assert!(function.scenarios.len() <= MAX_CALLS, "{source}");
                let mut places = BTreeSet::new();
                for scenario in &function.scenarios {
                    let after = scenario.after.as_deref().unwrap_or("<none>");
                    let whole = format!("{}{}{after}", scenario.setup, scenario.expression);
                    proptest::prop_assert_eq!(&whole, &body, "in\n{}", source);
                    // The same call written twice is taken at two places.
                    proptest::prop_assert!(places.insert(scenario.setup.len()), "{source}");
                    let Ok(Expr::Call(call)) = syn::parse_str::<Expr>(&scenario.expression) else {
                        return Err(proptest::test_runner::TestCaseError::fail(format!(
                            "`{}` is not a call, in\n{source}",
                            scenario.expression
                        )));
                    };
                    let Expr::Path(path) = &*call.func else {
                        panic!("{} in {source}", scenario.expression)
                    };
                    proptest::prop_assert!(path.path.is_ident(&name), "{source}");
                    proptest::prop_assert!(call.args.iter().all(literal), "{source}");
                    let written = scenario.test_edit(&source, "__tmt_witness", None).replacement;
                    proptest::prop_assert!(
                        syn::parse_file(&written).is_ok(),
                        "the test that looks does not parse:\n{}\nin\n{}",
                        written,
                        source
                    );
                }
            }
        }
    }

    #[test]
    fn let_shadowing_applies_only_after_its_initialiser() {
        let source = "\
fn bucket(n: i32) -> &'static str { if n > 0 { \"pos\" } else { \"neg\" } }

#[test]
fn rebinds_name() {
    let bucket = bucket(3);
    assert_ne!(bucket, \"none\");
}
";
        let file = syn::parse_file(source).unwrap();
        let found = unpinned_calls(source, &file);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].scenarios.len(), 1);
        assert_eq!(found[0].scenarios[0].expression, "bucket(3)");
    }
}
