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

/// The calls of one function: (call, test) pairs that were taken, and what to run for them.
type Tried = (BTreeSet<(String, String)>, Vec<Scenario>);

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
    let mut found: BTreeMap<&str, Tried> = BTreeMap::new();
    for test in tests.iter().filter(|test| plain(test.function)) {
        let shadowed = bound_names(&test.function.block);
        let body = test.function.block.brace_token.span.open().byte_range().end;
        let stmts = &test.function.block.stmts;
        // What comes after the call is not done again: a test that goes on to do more
        // than look (it removes what it made) is not cut short.
        let ends_looking = stmts
            .iter()
            .rposition(|stmt| !looks(stmt))
            .map_or(0, |last| last + 1);
        for stmt in &stmts[ends_looking.min(stmts.len())..] {
            let Some(setup) = source.get(body..stmt.span().byte_range().start) else {
                continue;
            };
            let mut calls = Calls {
                source,
                names: &functions,
                found: vec![],
                ran: vec![],
            };
            calls.visit_stmt(stmt);
            let ran = calls.ran;
            for (name, call, at) in calls.found {
                // The same call again in the same test is tried once; in another test
                // it comes after another beginning.
                // Whatever the statement runs before the call may be what the call
                // depends on, and is not run before the call on its own.
                let first = !ran
                    .iter()
                    .any(|other| other.start < at.start && other.end <= at.start);
                if !first || shadowed.contains(name) {
                    continue;
                }
                let (seen, scenarios) = found.entry(name).or_default();
                if scenarios.len() < MAX_CALLS && seen.insert((call.clone(), test.name())) {
                    scenarios.push(scenario(test, setup, call));
                }
            }
        }
    }
    found
        .into_iter()
        .map(|(name, (_, scenarios))| {
            let function = functions[name];
            CalledFunction {
                item: function.span().byte_range(),
                signature_line: function.sig.span().start().line,
                scenarios,
            }
        })
        .collect()
}

/// `setup`: what the test does before the statement of the call, done again before the
/// call is made on its own: the call may depend on it.
fn scenario(test: &Test<'_>, setup: &str, call: String) -> Scenario {
    let name = test.name();
    Scenario {
        setup: setup.to_owned(),
        guidance: Some(format!(
            "`{name}` already calls `{call}`: compare its result with this value."
        )),
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
        syn::Stmt::Macro(stmt) => stmt.attrs.is_empty() && assertion(&stmt.mac),
        syn::Stmt::Expr(Expr::Macro(expr), _) => expr.attrs.is_empty() && assertion(&expr.mac),
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

/// The names a test binds itself: a call of such a name is not one of the file's function.
fn bound_names(block: &syn::Block) -> BTreeSet<String> {
    struct Bound(BTreeSet<String>);
    impl<'ast> Visit<'ast> for Bound {
        fn visit_pat_ident(&mut self, pat: &'ast syn::PatIdent) {
            self.0.insert(pat.ident.to_string());
            syn::visit::visit_pat_ident(self, pat);
        }
        fn visit_item_fn(&mut self, function: &'ast ItemFn) {
            self.0.insert(function.sig.ident.to_string());
        }
    }
    let mut bound = Bound(BTreeSet::new());
    bound.visit_block(block);
    bound.0
}

/// A function a test can call by its name alone and whose result can be looked at.
fn callable(function: &ItemFn) -> bool {
    let signature = &function.sig;
    super::simple::supports(function)
        && signature.constness.is_none()
        && signature.variadic.is_none()
        && matches!(&signature.output, ReturnType::Type(_, ty) if !unit_or_never(ty))
        && signature
            .inputs
            .iter()
            .all(|input| matches!(input, syn::FnArg::Typed(_)))
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
    /// Where each call of the statement is, a candidate or not.
    ran: Vec<Range<usize>>,
}

impl<'a> Calls<'a> {
    /// The call as it is written, if it is one of a function of the file with literal
    /// arguments, as many as the function takes.
    fn call(&self, call: &syn::ExprCall) -> Option<(&'a str, String)> {
        let Expr::Path(path) = call.func.as_ref() else {
            return None;
        };
        if path.qself.is_some() || !call.attrs.is_empty() || !path.attrs.is_empty() {
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
            self.found.push((name, text, at.clone()));
        }
        self.ran.push(at);
        syn::visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        self.ran.push(call.span().byte_range());
        syn::visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_macro(&mut self, mac: &'ast syn::ExprMacro) {
        self.ran.push(mac.span().byte_range());
        syn::visit::visit_expr_macro(self, mac);
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
            // One side of an `assert_eq!` is pinned down already, and so is what an
            // `assert!` takes as it is or negated: a mutant that passes the test
            // returns that very value.
            let pinned = match arg {
                _ if differs => false,
                Expr::Unary(not) if !compares && matches!(not.op, syn::UnOp::Not(_)) => {
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
        assert_eq!(
            scenario.guidance.as_deref(),
            Some("`tests::buckets` already calls `bucket(3)`: compare its result with this value.")
        );
    }

    #[test]
    fn each_call_is_tried_once_and_only_a_few_of_them() {
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
                    "bucket(1)".into(),
                    "bucket(2)".into(),
                    "bucket(3)".into()
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
            ["on(2)", "on(5)"]
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
    fn what_the_test_does_before_the_call_is_done_again() {
        let source = format!(
            "{BUCKET}#[cfg(test)] mod tests {{\n#[test] fn t() {{ use super::bucket; MODE.set(2);\n\
             assert!(enable()); let b = bucket(1); assert_ne!(bucket(3), b); }} }}"
        );
        let file = syn::parse_file(&source).unwrap();
        let found = unpinned_calls(&source, &file);
        let setups: Vec<(&str, &str)> = found[0]
            .scenarios
            .iter()
            .map(|scenario| (scenario.expression.as_str(), scenario.setup.as_str()))
            .collect();
        assert_eq!(
            setups,
            [
                (
                    "bucket(1)",
                    " use super::bucket; MODE.set(2);\nassert!(enable()); "
                ),
                (
                    "bucket(3)",
                    " use super::bucket; MODE.set(2);\nassert!(enable()); let b = bucket(1); "
                ),
            ]
        );
    }

    #[test]
    fn only_the_first_thing_a_statement_runs_and_only_before_the_test_is_done_looking() {
        // What runs before the call in its statement may be what it depends on.
        for after in [
            "assert_ne!(size(\"a\"), bucket(3).len());",
            "assert!(size(\"a\") > 0 && bucket(3) != \"none\");",
            "assert!(enable() && bucket(3) != \"none\");",
            "let both = (\"a\".len(), bucket(3));",
            "assert!(matches!(1, 1) && bucket(3) != \"none\");",
        ] {
            let found = found_in(after);
            assert!(
                !found.contains(&"bucket(3)".to_string()),
                "{after}: {found:?}"
            );
        }
        // What is called with its result, or on it, runs after it.
        assert_eq!(
            found_in("assert_ne!(Some(bucket(3)).map(str::len), Some(9));"),
            ["bucket(3)"]
        );
        assert_eq!(
            found_in("assert!(bucket(3).len() > size(\"a\"));"),
            ["bucket(3)"]
        );
        // A statement that is not always part of the test.
        assert_eq!(
            found_in("#[cfg(unix)] let got = bucket(3);"),
            [] as [&str; 0]
        );
        // What the test does after the call is not done after the call on its own.
        assert_eq!(
            found_in("make(); assert_ne!(bucket(3), \"none\"); remove();"),
            [] as [&str; 0]
        );
        assert_eq!(
            found_in(
                "make(); assert_ne!(bucket(3), \"none\"); remove(); assert_ne!(bucket(4), \"none\"); let n = 1;"
            ),
            ["bucket(4)"]
        );
    }

    #[test]
    fn the_same_call_after_another_setup_is_another_call() {
        let source = format!(
            "{BUCKET}#[test] fn a() {{ set(0); assert_ne!(bucket(3), \"none\"); }}\n\
             #[test] fn b() {{ set(1); assert_ne!(bucket(3), \"none\"); assert_ne!(bucket(3), \"x\"); }}"
        );
        let file = syn::parse_file(&source).unwrap();
        let found = unpinned_calls(&source, &file);
        let setups: Vec<&str> = found[0]
            .scenarios
            .iter()
            .map(|s| s.setup.as_str())
            .collect();
        assert_eq!(setups, [" set(0); ", " set(1); ",]);
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
}
