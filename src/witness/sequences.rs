//! Source candidates that depend on the operations performed by an existing test.

use std::collections::{BTreeMap, BTreeSet};

use quote::ToTokens;
use syn::parse::Parser as _;
use syn::spanned::Spanned as _;
use syn::visit::Visit;
use syn::{Expr, ExprCall, FnArg, Item, ItemFn, Pat, Stmt, Type};

use super::{Candidate, Edit, Placement, Scenario};

const MAX_SCENARIOS: usize = 4;

pub(super) fn discover(source: &str, file: &syn::File, out: &mut Vec<Candidate>) {
    let mut tests = Vec::new();
    collect_tests(&file.items, "", None, &mut tests);
    round_trip_candidates(source, file, &tests, out);
    for item in &file.items {
        if let Item::Impl(block) = item
            && block.trait_.is_none()
            && let Type::Path(owner) = block.self_ty.as_ref()
            && let Some(owner) = owner.path.get_ident()
        {
            batch_candidates(source, owner, block, &tests, out);
        }
    }
}

struct Test<'a> {
    function: &'a ItemFn,
    module: String,
    insertion: Option<usize>,
}

impl Test<'_> {
    fn name(&self) -> String {
        qualified(&self.module, &self.function.sig.ident)
    }
}

fn qualified(module: &str, item: &syn::Ident) -> String {
    let item = item.to_string();
    let name = item.trim_start_matches("r#");
    if module.is_empty() {
        name.to_owned()
    } else {
        format!("{module}::{name}")
    }
}

fn collect_tests<'a>(
    items: &'a [Item],
    module: &str,
    insertion: Option<usize>,
    tests: &mut Vec<Test<'a>>,
) {
    for item in items {
        match item {
            Item::Fn(function)
                if function
                    .attrs
                    .iter()
                    .any(|attr| attr.path().is_ident("test")) =>
            {
                tests.push(Test {
                    function,
                    module: module.to_owned(),
                    insertion,
                });
            }
            Item::Mod(child) => {
                if let Some((braces, items)) = &child.content {
                    collect_tests(
                        items,
                        &qualified(module, &child.ident),
                        Some(braces.span.close().byte_range().start),
                        tests,
                    );
                }
            }
            _ => {}
        }
    }
}

fn binding(pat: &Pat) -> Option<&syn::Ident> {
    match pat {
        Pat::Ident(binding) if binding.by_ref.is_none() && binding.subpat.is_none() => {
            Some(&binding.ident)
        }
        Pat::Type(typed) => binding(&typed.pat),
        _ => None,
    }
}

fn path(expr: &Expr) -> Option<&syn::Path> {
    let Expr::Path(expr) = expr else {
        return None;
    };
    expr.qself.is_none().then_some(&expr.path)
}

fn variable(expr: &Expr) -> Option<&syn::Ident> {
    path(expr)?.get_ident()
}

fn local(stmt: &Stmt) -> Option<(&syn::Ident, &Expr)> {
    let Stmt::Local(local) = stmt else {
        return None;
    };
    let init = local.init.as_ref()?;
    init.diverge
        .is_none()
        .then_some((binding(&local.pat)?, init.expr.as_ref()))
}

fn single_arg<T>(nodes: &syn::punctuated::Punctuated<T, syn::Token![,]>) -> Option<&T> {
    (nodes.len() == 1).then(|| nodes.first()).flatten()
}

fn assert_eq_args(mac: &syn::Macro) -> Option<syn::punctuated::Punctuated<Expr, syn::Token![,]>> {
    mac.path
        .is_ident("assert_eq")
        .then(|| {
            syn::punctuated::Punctuated::<Expr, syn::Token![,]>::parse_terminated
                .parse2(mac.tokens.clone())
                .ok()
        })
        .flatten()
}

fn statement_assertion(stmt: &Stmt) -> Option<syn::punctuated::Punctuated<Expr, syn::Token![,]>> {
    let mac = match stmt {
        Stmt::Macro(stmt) => &stmt.mac,
        Stmt::Expr(Expr::Macro(expr), _) => &expr.mac,
        _ => return None,
    };
    assert_eq_args(mac)
}

fn primitive(ty: &Type) -> Option<&syn::Ident> {
    let Type::Path(ty) = ty else {
        return None;
    };
    if ty.qself.is_some() {
        return None;
    }
    ty.path.get_ident()
}

fn return_type(signature: &syn::Signature) -> Option<&Type> {
    let syn::ReturnType::Type(_, ty) = &signature.output else {
        return None;
    };
    Some(ty)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Endian {
    Little,
    Big,
}

struct Codec {
    integer: syn::Ident,
    order: Endian,
    method: syn::Ident,
    replacement: &'static str,
    signature_line: usize,
}

fn word_width(integer: &syn::Ident) -> Option<usize> {
    match integer.to_string().as_str() {
        "u16" | "i16" => Some(2),
        "u32" | "i32" => Some(4),
        "u64" | "i64" => Some(8),
        "u128" | "i128" => Some(16),
        _ => None,
    }
}

fn byte_array(ty: &Type) -> Option<usize> {
    let Type::Array(array) = ty else {
        return None;
    };
    if primitive(&array.elem)?.ne("u8") {
        return None;
    }
    let Expr::Lit(len) = &array.len else {
        return None;
    };
    let syn::Lit::Int(len) = &len.lit else {
        return None;
    };
    len.base10_parse().ok()
}

fn codec(function: &ItemFn) -> Option<(bool, Codec)> {
    let signature = &function.sig;
    let FnArg::Typed(input) = single_arg(&signature.inputs)? else {
        return None;
    };
    let output = return_type(signature)?;
    let argument = binding(&input.pat)?;
    let [Stmt::Expr(body, None)] = &function.block.stmts[..] else {
        return None;
    };
    let (encoding, integer, order, method, replacement) = match body {
        Expr::MethodCall(call)
            if call.args.is_empty() && variable(&call.receiver) == Some(argument) =>
        {
            let integer = primitive(&input.ty)?;
            if byte_array(output)? != word_width(integer)? {
                return None;
            }
            let (order, replacement) = match call.method.to_string().as_str() {
                "to_le_bytes" => (Endian::Little, "to_be_bytes"),
                "to_be_bytes" => (Endian::Big, "to_le_bytes"),
                _ => return None,
            };
            (true, integer, order, &call.method, replacement)
        }
        Expr::Call(call) if variable(single_arg(&call.args)?) == Some(argument) => {
            let function = path(&call.func)?;
            let [word, method] = function.segments.iter().collect::<Vec<_>>()[..] else {
                return None;
            };
            let integer = primitive(output)?;
            if &word.ident != integer || byte_array(&input.ty)? != word_width(integer)? {
                return None;
            }
            let (order, replacement) = match method.ident.to_string().as_str() {
                "from_le_bytes" => (Endian::Little, "from_be_bytes"),
                "from_be_bytes" => (Endian::Big, "from_le_bytes"),
                _ => return None,
            };
            (false, integer, order, &method.ident, replacement)
        }
        _ => return None,
    };
    Some((
        encoding,
        Codec {
            integer: integer.clone(),
            order,
            method: method.clone(),
            replacement,
            signature_line: signature.span().start().line,
        },
    ))
}

fn called_function(call: &ExprCall) -> Option<&syn::Ident> {
    let path = path(&call.func)?;
    let first = &path.segments.first()?.ident;
    match path.segments.len() {
        1 => path.get_ident(),
        2 if matches!(first.to_string().as_str(), "crate" | "super") => {
            Some(&path.segments.last()?.ident)
        }
        _ => None,
    }
}

#[derive(Default)]
struct RoundTrips {
    functions: BTreeSet<(String, String)>,
}

impl<'ast> Visit<'ast> for RoundTrips {
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let Some(args) = assert_eq_args(mac) else {
            return;
        };
        if let (Some(left), Some(right)) = (args.first(), args.iter().nth(1))
            && let Some(pair) = round_trip(left, right).or_else(|| round_trip(right, left))
        {
            self.functions.insert(pair);
        }
    }
}

fn round_trip(round_trip: &Expr, expected: &Expr) -> Option<(String, String)> {
    let Expr::Call(decode) = round_trip else {
        return None;
    };
    let Expr::Call(encode) = single_arg(&decode.args)? else {
        return None;
    };
    let input = single_arg(&encode.args)?;
    if input.to_token_stream().to_string() != expected.to_token_stream().to_string() {
        return None;
    }
    Some((
        called_function(encode)?.to_string(),
        called_function(decode)?.to_string(),
    ))
}

fn round_trip_candidates(
    source: &str,
    file: &syn::File,
    tests: &[Test<'_>],
    out: &mut Vec<Candidate>,
) {
    let mut encoders = BTreeMap::new();
    let mut decoders = BTreeMap::new();
    for item in &file.items {
        if let Item::Fn(function) = item
            && let Some((encoding, codec)) = codec(function)
        {
            let codecs = if encoding {
                &mut encoders
            } else {
                &mut decoders
            };
            codecs.insert(function.sig.ident.to_string(), codec);
        }
    }
    if encoders.is_empty() || decoders.is_empty() {
        return;
    }
    let mut round_trips = BTreeMap::<_, Vec<Scenario>>::new();
    for test in tests {
        let mut round_trip = RoundTrips::default();
        round_trip.visit_block(&test.function.block);
        for (encode, decode) in round_trip.functions {
            let scenarios = round_trips.entry((encode.clone(), decode)).or_default();
            if scenarios.len() < MAX_SCENARIOS {
                scenarios.push(Scenario {
                    expression: format!("{encode}(0x0102)"),
                    guidance: Some("Check the encoded bytes against the format contract.".into()),
                    source_test: Some(test.name()),
                    ..Scenario::default()
                });
            }
        }
    }
    for ((encode, decode), scenarios) in round_trips {
        let (Some(encoder), Some(decoder)) = (encoders.get(&encode), decoders.get(&decode)) else {
            continue;
        };
        if encoder.integer != decoder.integer || encoder.order != decoder.order {
            continue;
        }
        if [encoder, decoder].iter().any(|codec| {
            source.get(codec.method.span().byte_range()) != Some(&codec.method.to_string())
        }) {
            continue;
        }
        let mut edits = [encoder, decoder].map(|codec| Edit {
            span: codec.method.span().byte_range(),
            replacement: codec.replacement.to_owned(),
        });
        edits.sort_unstable_by_key(|edit| edit.span.start);
        let encoder_line = encoder.method.span().start().line;
        let decoder_line = decoder.method.span().start().line;
        out.push(Candidate {
            edits: edits.into(),
            scope: encoder_line.min(decoder_line)..=encoder_line.max(decoder_line),
            signature_line: encoder.signature_line,
            description: format!(
                "reverse the byte order together in `{encode}` (`{}`) and `{decode}` (`{}`)",
                encoder.replacement, decoder.replacement
            ),
            existing: false,
            scenarios,
        });
    }
}

fn method_with_key(signature: &syn::Signature) -> bool {
    let mut inputs = signature.inputs.iter();
    let (Some(FnArg::Receiver(receiver)), Some(FnArg::Typed(key)), None) =
        (inputs.next(), inputs.next(), inputs.next())
    else {
        return false;
    };
    receiver.reference.is_some()
        && receiver.mutability.is_none()
        && matches!(key.ty.as_ref(), Type::Reference(key) if key.mutability.is_none()
            && primitive(&key.elem).is_some_and(|ty| ty == "str"))
}

fn batch_guard(method: &syn::ImplItemFn) -> Option<&Stmt> {
    let mut params = method.sig.inputs.iter();
    let (Some(FnArg::Receiver(receiver)), Some(FnArg::Typed(batch)), None) =
        (params.next(), params.next(), params.next())
    else {
        return None;
    };
    if receiver.reference.is_none()
        || receiver.mutability.is_none()
        || !matches!(batch.ty.as_ref(), Type::Reference(reference) if matches!(reference.elem.as_ref(), Type::Slice(_)))
    {
        return None;
    }
    let first = method.block.stmts.first()?;
    let Stmt::Expr(Expr::Try(guard), Some(_)) = first else {
        return None;
    };
    let Expr::MethodCall(call) = guard.expr.as_ref() else {
        return None;
    };
    let parameter = binding(&batch.pat)?;
    let same_batch = variable(single_arg(&call.args)?) == Some(parameter);
    (same_batch && variable(&call.receiver).is_some_and(|var| var == "self")).then_some(first)
}

fn store_init<'a>(stmt: &'a Stmt, owner: &syn::Ident) -> Option<&'a syn::Ident> {
    let (store, init) = local(stmt)?;
    let Expr::Call(init) = init else {
        return None;
    };
    let mut path = path(&init.func)?.segments.iter().rev();
    path.next()?;
    if !init.args.is_empty() || &path.next()?.ident != owner {
        return None;
    }
    Some(store)
}

fn reads_in_test(
    test: &Test<'_>,
    owner: &syn::Ident,
    queries: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut reads = BTreeSet::new();
    let Some(store) = test
        .function
        .block
        .stmts
        .first()
        .and_then(|stmt| store_init(stmt, owner))
    else {
        return reads;
    };
    for assertion in test
        .function
        .block
        .stmts
        .iter()
        .filter_map(statement_assertion)
    {
        for arg in assertion.iter().take(2) {
            if let Expr::MethodCall(read) = arg
                && variable(&read.receiver) == Some(store)
                && single_arg(&read.args).and_then(string_literal).is_some()
                && queries.contains(&read.method.to_string())
            {
                reads.insert(read.method.to_string());
            }
        }
    }
    reads
}

fn string_literal(expr: &Expr) -> Option<String> {
    let Expr::Lit(literal) = expr else {
        return None;
    };
    let syn::Lit::Str(literal) = &literal.lit else {
        return None;
    };
    Some(literal.value())
}

fn first_key(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Call(entry) => string_literal(entry.args.first()?),
        Expr::Struct(entry) => {
            let value = &entry
                .fields
                .iter()
                .find(|field| matches!(&field.member, syn::Member::Named(name) if name == "key"))?
                .expr;
            if let Expr::MethodCall(conversion) = value
                && conversion.method == "into"
                && conversion.args.is_empty()
            {
                string_literal(&conversion.receiver)
            } else {
                string_literal(value)
            }
        }
        _ => None,
    }
}

fn failed_batch<'a>(
    test: &'a Test<'a>,
    owner: &syn::Ident,
    method: &syn::Ident,
) -> Option<(&'a syn::Ident, &'a syn::ExprArray)> {
    let [init, operation, assertion] = &test.function.block.stmts[..] else {
        return None;
    };
    let store = store_init(init, owner)?;
    let (result, Expr::MethodCall(call)) = local(operation)? else {
        return None;
    };
    if &call.method != method || variable(&call.receiver) != Some(store) {
        return None;
    }
    let args = statement_assertion(assertion)?;
    let compares_error = |name: &Expr, expected: &Expr| {
        variable(name) == Some(result)
            && matches!(expected, Expr::Call(call)
            if path(&call.func).and_then(|path| path.segments.last()).is_some_and(|name| name.ident == "Err"))
    };
    let (left, right) = (args.first()?, args.iter().nth(1)?);
    if !compares_error(left, right) && !compares_error(right, left) {
        return None;
    }
    let Expr::Reference(argument) = single_arg(&call.args)? else {
        return None;
    };
    let Expr::Array(batch) = argument.expr.as_ref() else {
        return None;
    };
    Some((store, batch))
}

fn batch_candidates(
    source: &str,
    owner: &syn::Ident,
    implementation: &syn::ItemImpl,
    tests: &[Test<'_>],
    out: &mut Vec<Candidate>,
) {
    let methods: Vec<_> = implementation
        .items
        .iter()
        .filter_map(|item| {
            if let syn::ImplItem::Fn(method) = item {
                Some(method)
            } else {
                None
            }
        })
        .collect();
    let queries: BTreeSet<_> = methods
        .iter()
        .filter(|method| {
            matches!(method.vis, syn::Visibility::Public(_)) && method_with_key(&method.sig)
        })
        .map(|method| method.sig.ident.to_string())
        .collect();
    if queries.is_empty() {
        return;
    }
    let guards: Vec<_> = methods
        .into_iter()
        .filter_map(|method| Some((method, batch_guard(method)?)))
        .collect();
    if guards.is_empty() {
        return;
    }
    let reads: BTreeMap<_, _> = tests
        .iter()
        .map(|test| (test.name(), reads_in_test(test, owner, &queries)))
        .collect();
    for (method, guard) in guards {
        let span = guard.span();
        let Some(code) = source.get(span.byte_range()) else {
            continue;
        };
        let code = code.split_whitespace().collect::<Vec<_>>().join(" ");
        if code.chars().count() > 100 {
            continue;
        }
        let mut scenarios = Vec::new();
        for test in tests {
            let Some((store, batch)) = failed_batch(test, owner, &method.sig.ident) else {
                continue;
            };
            let test_name = test.name();
            let braces = test.function.block.brace_token.span;
            let body = braces.open().byte_range().end..braces.close().byte_range().start;
            let Some(setup) = source.get(body) else {
                continue;
            };
            let sibling_queries: BTreeSet<_> = tests
                .iter()
                .filter(|other| {
                    other.module == test.module
                        && other.function.sig.ident != test.function.sig.ident
                })
                .flat_map(|other| reads.get(&other.name()).into_iter().flatten())
                .collect();
            let mut seen = BTreeSet::new();
            let keys: Vec<_> = batch
                .elems
                .iter()
                .filter_map(first_key)
                .filter(|key| seen.insert(key.clone()))
                .take(MAX_SCENARIOS)
                .collect();
            for query in sibling_queries {
                for key in &keys {
                    if scenarios.len() >= MAX_SCENARIOS {
                        break;
                    }
                    scenarios.push(Scenario {
                        setup: setup.to_owned(),
                        expression: format!("{store}.{query}({key:?})"),
                        guidance: Some(format!(
                            "Add this assertion after the error check in `{test_name}`."
                        )),
                        placement: test.insertion.map(|offset| Placement {
                            offset,
                            module: test.module.clone(),
                        }),
                        source_test: Some(test_name.clone()),
                        ..Scenario::default()
                    });
                }
            }
        }
        if !scenarios.is_empty() {
            out.push(Candidate {
                edits: vec![Edit {
                    span: span.byte_range(),
                    replacement: String::new(),
                }],
                scope: span.start().line..=span.end().line,
                signature_line: method.sig.span().start().line,
                description: format!("delete `{code}`"),
                existing: true,
                scenarios,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates(source: &str) -> Vec<Candidate> {
        let mut output = Vec::new();
        discover(source, &syn::parse_file(source).unwrap(), &mut output);
        output
    }

    #[test]
    fn a_rejected_batch_can_be_observed_with_a_read_from_a_sibling_test() {
        let source = include_str!("../../fixtures/batch-store/src/lib.rs");
        let found = candidates(source);
        assert_eq!(found.len(), 1);
        let candidate = &found[0];
        assert!(candidate.existing);
        assert_eq!(
            candidate.description,
            "delete `self.check_preconditions(batch)?;`"
        );
        assert_eq!(candidate.edits.len(), 1);
        let edit = &candidate.edits[0];
        assert_eq!(
            &source[edit.span.clone()],
            "self.check_preconditions(batch)?;"
        );
        assert!(edit.replacement.is_empty());
        let scenario = &candidate.scenarios[0];
        assert_eq!(scenario.expression, "store.get(\"a\")");
        assert!(scenario.setup.contains("assert_eq!(result, Err("));
        assert_eq!(
            scenario.source_test.as_deref(),
            Some("tests::commit_fails_when_an_update_target_is_missing")
        );
        let placement = scenario.placement.as_ref().unwrap();
        assert_eq!(placement.module, "tests");
        assert!(source[placement.offset..].starts_with('}'));
    }

    const CODEC: &str = r#"
        pub fn encode(word: u16) -> [u8; 2] { word.to_le_bytes() }
        pub fn decode(bytes: [u8; 2]) -> u16 { u16::from_le_bytes(bytes) }
        #[cfg(test)]
        mod tests {
            use super::*;
            #[test]
            fn round_trip() {
                let word = 0x1234;
                assert_eq!(word, decode(encode(word)));
            }
        }
    "#;

    #[test]
    fn an_asserted_round_trip_connects_the_two_endian_edits() {
        let found = candidates(CODEC);
        assert_eq!(found.len(), 1);
        let candidate = &found[0];
        assert!(!candidate.existing);
        let actual: Vec<_> = candidate
            .edits
            .iter()
            .map(|edit| (&CODEC[edit.span.clone()], edit.replacement.as_str()))
            .collect();
        assert_eq!(
            actual,
            [
                ("to_le_bytes", "to_be_bytes"),
                ("from_le_bytes", "from_be_bytes")
            ]
        );
        assert_eq!(candidate.scenarios[0].expression, "encode(0x0102)");
        assert!(candidate.description.contains("`to_be_bytes`"));
        assert!(candidate.description.contains("`from_be_bytes`"));
        assert_eq!(
            candidate.scenarios[0].source_test.as_deref(),
            Some("tests::round_trip")
        );
    }

    #[test]
    fn an_unused_composition_or_mismatched_codec_is_not_a_pair() {
        for source in [
            CODEC.replace(
                "assert_eq!(word, decode(encode(word)));",
                "let _ = decode(encode(word));",
            ),
            CODEC.replace("decode(encode(word))", "decode([0, 0])"),
            CODEC.replace("from_le_bytes", "from_be_bytes"),
        ] {
            assert!(candidates(&source).is_empty(), "{source}");
        }
    }
}
