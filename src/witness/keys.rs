//! Finds functions that put several parameters into one value (a `format!`, a tuple):
//! the value without one of them is what tests let through that never have two values
//! in one place which differ in that parameter alone.

use syn::parse::Parser;
use syn::spanned::Spanned;
use syn::{Expr, FnArg, Item, Pat, ReturnType, Stmt, Type};

use syn::visit::Visit;

use super::{Candidate, Edit, Scenario};

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

/// A method that makes the value from its own parameters: `key(language, slug)` in
/// `fn get(&self, language: &str, slug: &str)`.
struct User<'a> {
    name: &'a syn::Ident,
    writes: bool,
    /// For each part of the value, which of the method's parameters it is.
    parts: Vec<usize>,
    parameters: usize,
}

/// The first call of `function` with nothing but parameters of the method around it.
struct KeyCall<'a, 'n> {
    function: &'n syn::Ident,
    names: &'n [&'a syn::Ident],
    parts: Option<Vec<usize>>,
}

impl<'ast> Visit<'ast> for KeyCall<'_, '_> {
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if self.parts.is_none() && super::path_ident(&call.func) == Some(self.function) {
            self.parts = call
                .args
                .iter()
                .map(|arg| {
                    let name = parameter(arg, self.names)?;
                    self.names.iter().position(|known| *known == name)
                })
                .collect();
        }
        syn::visit::visit_expr_call(self, call);
    }
}

fn users<'a>(file: &'a syn::File, function: &syn::Ident, arity: usize) -> Vec<User<'a>> {
    let mut found = vec![];
    for item in &file.items {
        let Item::Impl(block) = item else { continue };
        for inner in &block.items {
            let syn::ImplItem::Fn(method) = inner else {
                continue;
            };
            let Some(FnArg::Receiver(receiver)) = method.sig.inputs.first() else {
                continue;
            };
            let Some(names) = method
                .sig
                .inputs
                .iter()
                .skip(1)
                .map(|input| match input {
                    FnArg::Typed(typed) => match typed.pat.as_ref() {
                        Pat::Ident(name) => Some(&name.ident),
                        _ => None,
                    },
                    FnArg::Receiver(_) => None,
                })
                .collect::<Option<Vec<_>>>()
            else {
                continue;
            };
            let mut call = KeyCall {
                function,
                names: &names,
                parts: None,
            };
            call.visit_block(&method.block);
            if let Some(parts) = call.parts.filter(|parts| parts.len() == arity) {
                found.push(User {
                    name: &method.sig.ident,
                    writes: receiver.mutability.is_some(),
                    parts,
                    parameters: names.len(),
                });
            }
        }
    }
    found
}

/// The calls of methods on local values in the tests: the statement that makes the
/// value (`let mut labels = Labels::default();`) and each call with its arguments.
#[derive(Default)]
struct TestCalls<'a> {
    in_test: bool,
    /// `(receiver, the statement that makes it)` of the test being read.
    made: Vec<(&'a syn::Ident, &'a syn::Local)>,
    calls: Vec<(Option<&'a syn::Local>, &'a syn::ExprMethodCall)>,
}

impl<'a> Visit<'a> for TestCalls<'a> {
    fn visit_item_fn(&mut self, function: &'a syn::ItemFn) {
        if function
            .attrs
            .iter()
            .any(|attr| attr.path().is_ident("test"))
        {
            self.in_test = true;
            self.made.clear();
            self.visit_block(&function.block);
            self.in_test = false;
        }
    }

    fn visit_local(&mut self, local: &'a syn::Local) {
        if let Pat::Ident(name) = &local.pat
            && name.mutability.is_some()
            && local
                .init
                .as_ref()
                .is_some_and(|init| init.diverge.is_none())
        {
            self.made.push((&name.ident, local));
        }
        syn::visit::visit_local(self, local);
    }

    fn visit_expr_method_call(&mut self, call: &'a syn::ExprMethodCall) {
        if self.in_test {
            let made = super::path_ident(&call.receiver).and_then(|receiver| {
                self.made
                    .iter()
                    .rev()
                    .find(|(name, _)| *name == receiver)
                    .map(|(_, local)| *local)
            });
            self.calls.push((made, call));
        }
        syn::visit::visit_expr_method_call(self, call);
    }

    fn visit_macro(&mut self, mac: &'a syn::Macro) {
        // The arguments of an assertion are not parsed by the visitor: what they call
        // is found by its text below.
        let _ = mac;
    }
}

/// A test of two values in one place that differ in one part alone: a value written in
/// a test, read with another test's value for the part. `None` when the tests do not
/// have what it takes (a writer and a reader of the value, called with literals).
fn mixing(
    source: &str,
    file: &syn::File,
    function: &syn::Ident,
    arity: usize,
    part: usize,
    part_name: &syn::Ident,
) -> Option<Scenario> {
    let users = users(file, function, arity);
    let writer = users.iter().find(|user| user.writes)?;
    // The reader takes the parts and nothing else: its call can then be written.
    let reader = users
        .iter()
        .find(|user| !user.writes && user.parameters == arity)?;
    let mut tests = TestCalls::default();
    tests.visit_file(file);
    let text = |expr: &Expr| source.get(expr.span().byte_range());
    let literal = |expr: &Expr| matches!(expr, Expr::Lit(_)).then(|| text(expr)).flatten();
    // A write on a value a test makes, with literals for the parts.
    let (made, write, parts) = tests.calls.iter().find_map(|(made, call)| {
        let made = (*made)?;
        if call.method != *writer.name || call.args.len() != writer.parameters {
            return None;
        }
        let parts = writer
            .parts
            .iter()
            .map(|index| literal(&call.args[*index]))
            .collect::<Option<Vec<_>>>()?;
        Some((made, *call, parts))
    })?;
    // Another value of the part: what any call of the two methods has there.
    let other = tests.calls.iter().find_map(|(_, call)| {
        let user = [writer, reader]
            .into_iter()
            .find(|user| call.method == *user.name && call.args.len() == user.parameters)?;
        literal(&call.args[user.parts[part]]).filter(|value| *value != parts[part])
    })?;
    let receiver = super::path_ident(&write.receiver)?;
    let mut read = vec![""; arity];
    for (index, parameter) in reader.parts.iter().enumerate() {
        read[*parameter] = if index == part { other } else { parts[index] };
    }
    let setup = format!(
        "{} {};",
        source.get(made.span().byte_range())?,
        source.get(write.span().byte_range())?
    );
    Some(Scenario {
        expression: format!("{receiver}.{}({})", reader.name, read.join(", ")),
        assertion_prefix: setup.clone(),
        setup,
        guidance: Some(format!(
            "The tests pass without `{part_name}` in the value; here {} and {other} meet in one place.",
            parts[part]
        )),
        ..Scenario::default()
    })
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
                scenarios: names
                    .iter()
                    .position(|name| *name == without)
                    .and_then(|part| {
                        mixing(source, file, &signature.ident, names.len(), part, without)
                    })
                    .into_iter()
                    .collect(),
                advice: Some(format!(
                    "no test has two values in one place that differ in `{without}` alone: add one that puts two such values together and checks that they stay apart"
                )),
                file: Default::default(),
                finding_at: None,
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

    const STORE: &str = r#"
fn key(language: &str, slug: &str) -> String { format!("{language}/{slug}") }
struct Labels { texts: std::collections::BTreeMap<String, String> }
impl Labels {
    fn new() -> Self { Labels { texts: Default::default() } }
    fn put(&mut self, language: &str, slug: &str, text: &str) {
        self.texts.insert(key(language, slug), text.to_owned());
    }
    fn get(&self, language: &str, slug: &str) -> Option<&str> {
        self.texts.get(&key(language, slug)).map(String::as_str)
    }
    fn len(&self) -> usize { self.texts.len() }
}
"#;

    /// The scenario of each candidate of the store with these tests.
    fn mixed(tests: &str) -> Vec<Option<(String, String, String)>> {
        let source = format!("{STORE}{tests}");
        let mut candidates = Vec::new();
        discover(&source, &syn::parse_file(&source).unwrap(), &mut candidates);
        assert_eq!(candidates.len(), 2);
        candidates
            .into_iter()
            .map(|mut candidate| {
                assert!(candidate.advice.is_some());
                candidate.scenarios.pop().map(|scenario| {
                    assert_eq!(scenario.assertion_prefix, scenario.setup);
                    (
                        scenario.setup,
                        scenario.expression,
                        scenario.guidance.unwrap(),
                    )
                })
            })
            .collect()
    }

    #[test]
    fn a_value_of_one_test_is_read_with_a_part_of_another() {
        let tests = r#"
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn japanese() {
        let mut labels = Labels::new();
        labels.put("ja", "home", "konnichiwa");
        assert_eq!(labels.get("ja", "home"), Some("konnichiwa"));
    }
    #[test]
    fn english() {
        let mut labels = Labels::new();
        labels.put("en", "about", "About");
        assert_eq!(labels.len(), 1);
    }
}"#;
        let own = |text: &str| text.to_string();
        let setup = r#"let mut labels = Labels::new(); labels.put("ja", "home", "konnichiwa");"#;
        assert_eq!(
            mixed(tests),
            [
                Some((
                    own(setup),
                    own(r#"labels.get("en", "home")"#),
                    own(
                        r#"The tests pass without `language` in the value; here "ja" and "en" meet in one place."#
                    )
                )),
                Some((
                    own(setup),
                    own(r#"labels.get("ja", "about")"#),
                    own(
                        r#"The tests pass without `slug` in the value; here "home" and "about" meet in one place."#
                    )
                )),
            ]
        );
    }

    #[test]
    fn without_what_it_takes_no_test_is_made() {
        for tests in [
            // No test at all, one that is no test, and one that writes nothing.
            "",
            r#"fn helper() { let mut labels = Labels::new(); labels.put("ja", "home", "x"); labels.put("en", "home", "y"); }"#,
            r#"#[test] fn t() { let labels = Labels::new(); assert_eq!(labels.get("ja", "home"), None); }"#,
            // One value only for each part: nothing to mix it with.
            r#"#[test] fn t() { let mut labels = Labels::new(); labels.put("ja", "home", "x"); labels.put("ja", "home", "y"); }"#,
            // Parts that are no literals, and a value the test does not make itself.
            r#"#[test] fn t() { let l = "ja"; let mut labels = Labels::new(); labels.put(l, "home", "x"); labels.put(l, "about", "y"); }"#,
            r#"fn made() -> Labels { Labels::new() }
               #[test] fn t() { made().put("ja", "home", "x"); made().put("en", "about", "y"); }"#,
        ] {
            assert_eq!(mixed(tests), [None, None], "{tests}");
        }
        // Without a method that reads by the parts alone, or one that writes, none either.
        for (from, to) in [
            (
                "fn get(&self, language: &str, slug: &str)",
                "fn get(&self, language: &str, slug: &str, _strict: bool)",
            ),
            ("fn put(&mut self,", "fn put(&self,"),
            (
                "self.texts.get(&key(language, slug))",
                "self.texts.get(&key(language, \"fixed\"))",
            ),
        ] {
            assert!(STORE.contains(from), "{from}");
            let source = format!(
                "{}\n#[test] fn t() {{ let mut labels = Labels::new(); labels.put(\"ja\", \"home\", \"x\"); labels.put(\"en\", \"about\", \"y\"); }}",
                STORE.replace(from, to)
            );
            let mut candidates = Vec::new();
            discover(&source, &syn::parse_file(&source).unwrap(), &mut candidates);
            assert!(
                candidates
                    .iter()
                    .all(|candidate| candidate.scenarios.is_empty()),
                "{to}"
            );
        }
    }

    #[test]
    fn the_parts_are_taken_in_the_order_each_method_has_them() {
        // The reader takes the page first: its call is written that way round.
        let source = STORE.replace(
            "fn get(&self, language: &str, slug: &str)",
            "fn get(&self, slug: &str, language: &str)",
        ) + r#"#[test] fn t() { let mut labels = Labels::new(); labels.put("ja", "home", "x"); labels.put("en", "about", "y"); }"#;
        let mut candidates = Vec::new();
        discover(&source, &syn::parse_file(&source).unwrap(), &mut candidates);
        assert_eq!(
            candidates[0].scenarios[0].expression,
            r#"labels.get("home", "en")"#
        );
        assert_eq!(
            candidates[1].scenarios[0].expression,
            r#"labels.get("about", "ja")"#
        );
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
