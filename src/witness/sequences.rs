//! Source candidates that depend on the operations performed by an existing test.

use std::collections::{BTreeMap, BTreeSet};

use quote::ToTokens;
use syn::spanned::Spanned as _;
use syn::visit::Visit;
use syn::{Expr, ExprCall, FnArg, Item, ItemFn, Pat, Stmt, Type};

use super::{Candidate, Edit, Placement, Scenario};

const MAX_SCENARIOS: usize = 4;

pub(super) fn discover(
    source: &str,
    file: &syn::File,
    out: &mut Vec<Candidate>,
    include_existing: bool,
) {
    let mut tests = Vec::new();
    collect_tests(&file.items, "", None, &mut tests);
    round_trip_candidates(source, file, &tests, out);
    if !include_existing {
        return;
    }
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

pub(super) struct Test<'a> {
    pub(super) function: &'a ItemFn,
    pub(super) module: String,
    pub(super) insertion: Option<usize>,
}

impl Test<'_> {
    pub(super) fn name(&self) -> String {
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

pub(super) fn collect_tests<'a>(
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
        .then(|| super::common::parse_macro_exprs(mac))
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
    super::common::type_name(ty)
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
    if crate::instrument::is_skipped(&function.attrs)
        || crate::instrument::is_test_only(&function.attrs)
    {
        return None;
    }
    let signature = &function.sig;
    if !signature.generics.params.is_empty()
        || signature.asyncness.is_some()
        || signature.unsafety.is_some()
        || signature.abi.is_some()
    {
        return None;
    }
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
    // tmt:ignore performance only: without a codec of each kind no round trip below finds its pair
    if encoders.is_empty() || decoders.is_empty() {
        return;
    }
    let mut round_trips = BTreeMap::<_, Vec<Scenario>>::new();
    for test in tests {
        let mut round_trip = RoundTrips::default();
        round_trip.visit_block(&test.function.block);
        for (encode, decode) in round_trip.functions {
            let scenarios = round_trips.entry((encode.clone(), decode)).or_default();
            scenarios.push(Scenario {
                expression: format!("{encode}(0x0102)"),
                guidance: Some("Check the encoded bytes against the format contract.".into()),
                source_test: Some(test.name()),
                ..Scenario::default()
            });
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
            signature_lines: vec![encoder.signature_line, decoder.signature_line],
            description: format!(
                "reverse the byte order together in `{encode}` (`{}`) and `{decode}` (`{}`)",
                encoder.replacement, decoder.replacement
            ),
            existing: false,
            advice: None,
            file: Default::default(),
            finding_at: None,
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
            let limit = scenarios.len() + MAX_SCENARIOS;
            let braces = test.function.block.brace_token.span;
            let body = braces.open().byte_range().end..braces.close().byte_range().start;
            let Some(setup) = source.get(body) else {
                continue;
            };
            let sibling_queries: BTreeSet<_> = tests
                .iter()
                .filter(|other| {
                    // tmt:ignore leaving out the test itself states the intent: a test `failed_batch` accepts has no read
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
                    if scenarios.len() >= limit {
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
                signature_lines: vec![method.sig.span().start().line],
                description: format!("delete `{code}`"),
                existing: true,
                advice: None,
                file: Default::default(),
                finding_at: None,
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
        discover(source, &syn::parse_file(source).unwrap(), &mut output, true);
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

    fn codec_with(from: &str, to: &str) -> String {
        assert!(CODEC.contains(from), "{from}");
        CODEC.replace(from, to)
    }

    #[test]
    fn the_pair_says_where_it_is_and_what_to_check() {
        let found = candidates(CODEC);
        assert_eq!(found.len(), 1);
        // `encode` is on the second line of the text, `decode` on the third.
        assert_eq!(found[0].scope, 2..=3);
        assert_eq!(found[0].signature_lines, [2, 3]);
        assert_eq!(
            found[0].scenarios[0].guidance.as_deref(),
            Some("Check the encoded bytes against the format contract.")
        );
    }

    #[test]
    fn the_edits_are_in_source_order_when_the_decoder_comes_first() {
        let source = r#"
        pub fn decode(bytes: [u8; 2]) -> u16 { u16::from_le_bytes(bytes) }
        pub fn encode(word: u16) -> [u8; 2] { word.to_le_bytes() }
        #[test]
        fn round_trip() {
            assert_eq!(decode(encode(7)), 7);
        }
        "#;
        let found = candidates(source);
        assert_eq!(found.len(), 1);
        let actual: Vec<_> = found[0]
            .edits
            .iter()
            .map(|edit| (&source[edit.span.clone()], edit.replacement.as_str()))
            .collect();
        assert_eq!(
            actual,
            [
                ("from_le_bytes", "from_be_bytes"),
                ("to_le_bytes", "to_be_bytes")
            ]
        );
        assert_eq!(found[0].scope, 2..=3);
        assert_eq!(found[0].signature_lines, [3, 2]);
        assert_eq!(
            found[0].scenarios[0].source_test.as_deref(),
            Some("round_trip")
        );
    }

    #[test]
    fn a_big_endian_pair_is_reversed_to_little_endian() {
        let source = CODEC.replace("_le_", "_be_");
        let found = candidates(&source);
        assert_eq!(found.len(), 1);
        let actual: Vec<_> = found[0]
            .edits
            .iter()
            .map(|edit| (&source[edit.span.clone()], edit.replacement.as_str()))
            .collect();
        assert_eq!(
            actual,
            [
                ("to_be_bytes", "to_le_bytes"),
                ("from_be_bytes", "from_le_bytes")
            ]
        );
    }

    #[test]
    fn words_of_every_width_with_a_byte_order_are_codecs() {
        for (word, width, expected) in [
            ("u16", 2, 1),
            ("i16", 2, 1),
            ("u32", 4, 1),
            ("i32", 4, 1),
            ("u64", 8, 1),
            ("i64", 8, 1),
            ("u128", 16, 1),
            ("i128", 16, 1),
            // One byte has no order.
            ("u8", 1, 0),
        ] {
            let source = CODEC
                .replace("u16", word)
                .replace("[u8; 2]", &format!("[u8; {width}]"));
            assert_eq!(candidates(&source).len(), expected, "{word}");
        }
    }

    #[test]
    fn a_codec_converts_its_one_plain_parameter_and_nothing_else() {
        for source in [
            // Not a plain binding.
            codec_with("encode(word: u16)", "encode(ref word: u16)"),
            codec_with("encode(word: u16)", "encode(word @ _: u16)"),
            // A second parameter.
            codec_with("encode(word: u16)", "encode(word: u16, pad: u8)")
                .replace("decode(encode(word))", "decode(encode(word, 0))"),
            // Code the tool does not mutate.
            codec_with("pub fn encode", "#[cfg(test)] pub fn encode"),
            codec_with("pub fn encode", "#[mutants::skip] pub fn encode"),
            // Not a conversion of the parameter.
            codec_with("word.to_le_bytes()", "word.to_le_bytes(1)"),
            codec_with("word.to_le_bytes()", "MAGIC.to_le_bytes()"),
            codec_with("from_le_bytes(bytes)", "from_le_bytes(MAGIC)"),
            // Not the bytes of the word.
            codec_with("-> [u8; 2] { word", "-> [u8; 4] { word"),
            codec_with("decode(bytes: [u8; 2])", "decode(bytes: [u8; 4])"),
            codec_with("u16::from_le_bytes", "Word::from_le_bytes"),
            // Two codecs of different words.
            codec_with(
                "decode(bytes: [u8; 2]) -> u16 { u16::",
                "decode(bytes: [u8; 4]) -> u32 { u32::",
            ),
        ] {
            assert!(candidates(&source).is_empty(), "{source}");
        }
    }

    #[test]
    fn a_round_trip_is_asserted_by_a_test_against_its_own_input() {
        for source in [
            codec_with("#[test]", ""),
            codec_with(
                "assert_eq!(word, decode(encode(word)));",
                "assert_eq!(0x1234, decode(encode(word)));",
            ),
        ] {
            assert!(candidates(&source).is_empty(), "{source}");
        }
    }

    const READS: &str = r#"
            #[test]
            fn reads() {
                let mut store = Store::default();
                assert_eq!(store.get("a"), None);
            }
"#;

    const STORE: &str = r#"
        pub struct Store;
        impl Store {
            pub fn get(&self, key: &str) -> Option<&str> { None }
            pub fn has(&self, key: &str) -> bool { false }
            pub fn commit(&mut self, batch: &[Write]) -> Result<(), CommitError> {
                self.check(batch)?;
                Ok(())
            }
        }
        #[cfg(test)]
        mod tests {
            use super::*;

            #[test]
            fn reads() {
                let mut store = Store::default();
                assert_eq!(store.get("a"), None);
            }

            #[test]
            fn fails() {
                let mut store = Store::default();
                let result = store.commit(&[set("a", "1"), update("b", "2")]);
                assert_eq!(result, Err(CommitError::NotFound("b".into())));
            }
        }
    "#;

    fn store_with(from: &str, to: &str) -> String {
        assert!(STORE.contains(from), "{from}");
        STORE.replace(from, to)
    }

    fn observations(source: &str) -> Vec<String> {
        candidates(source)
            .iter()
            .flat_map(|candidate| &candidate.scenarios)
            .map(|scenario| scenario.expression.clone())
            .collect()
    }

    #[test]
    fn each_key_of_the_rejected_batch_is_read_after_the_error_check() {
        let found = candidates(STORE);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].description, "delete `self.check(batch)?;`");
        assert_eq!(
            observations(STORE),
            ["store.get(\"a\")", "store.get(\"b\")"]
        );
        for scenario in &found[0].scenarios {
            assert_eq!(
                scenario.guidance.as_deref(),
                Some("Add this assertion after the error check in `tests::fails`.")
            );
            assert_eq!(scenario.source_test.as_deref(), Some("tests::fails"));
        }
    }

    #[test]
    fn the_error_may_be_on_either_side_of_the_assertion() {
        let source = store_with(
            "assert_eq!(result, Err(CommitError::NotFound(\"b\".into())));",
            "assert_eq!(Err(CommitError::NotFound(\"b\".into())), result);",
        );
        assert_eq!(
            observations(&source),
            ["store.get(\"a\")", "store.get(\"b\")"]
        );
    }

    #[test]
    fn a_read_is_a_public_method_of_the_shared_store_with_a_string_key() {
        let get = "pub fn get(&self, key: &str)";
        for replacement in [
            "fn get(&self, key: &str)",
            "pub fn get(&mut self, key: &str)",
            "pub fn get(self, key: &str)",
            "pub fn get(&self, key: &mut str)",
            "pub fn get(&self, key: &String)",
            "pub fn get(&self, key: u32)",
            "pub fn get(&self, key: &str, default: &str)",
            "pub fn get(key: &str)",
        ] {
            let source = store_with(get, replacement);
            assert!(candidates(&source).is_empty(), "{replacement}");
        }
    }

    #[test]
    fn a_guard_is_the_first_statement_of_a_method_that_changes_the_store_with_a_slice() {
        let commit = "pub fn commit(&mut self, batch: &[Write])";
        for replacement in [
            "pub fn commit(&self, batch: &[Write])",
            "pub fn commit(mut self, batch: &[Write])",
            "pub fn commit(&mut self, batch: &Vec<Write>)",
            "pub fn commit(&mut self, batch: Vec<Write>)",
            "pub fn commit(&mut self, batch: &[Write], force: bool)",
        ] {
            let source = store_with(commit, replacement);
            assert!(candidates(&source).is_empty(), "{replacement}");
        }
        let guard = "self.check(batch)?;";
        for replacement in [
            "self.check(other)?;",
            "CHECKER.check(batch)?;",
            "self.inner.check(batch)?;",
        ] {
            let source = store_with(guard, replacement);
            assert!(candidates(&source).is_empty(), "{replacement}");
        }
    }

    #[test]
    fn a_guard_longer_than_a_line_of_the_report_is_not_proposed() {
        // `self.` and `(batch)?;` are 14 characters.
        let fits = store_with(
            "self.check(batch)?;",
            &format!("self.{}(batch)?;", "c".repeat(86)),
        );
        let found = candidates(&fits);
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].description.chars().count(),
            "delete ``".len() + 100
        );
        let long = store_with(
            "self.check(batch)?;",
            &format!("self.{}(batch)?;", "c".repeat(87)),
        );
        assert!(candidates(&long).is_empty());
    }

    #[test]
    fn the_tests_must_use_a_fresh_store_of_the_type_and_the_guarded_method() {
        for source in [
            STORE.replace("Store::default()", "Store::with_capacity(4)"),
            STORE.replace("Store::default()", "Other::default()"),
            store_with(
                "assert_eq!(store.get(\"a\"), None);",
                "assert_eq!(other.get(\"a\"), None);",
            ),
            store_with("store.commit(&[", "store.apply(&["),
            store_with("store.commit(&[", "OTHER.commit(&["),
            store_with(
                "assert_eq!(result, Err(CommitError::NotFound(\"b\".into())));",
                "assert_eq!(result, Ok(()));",
            ),
            store_with("assert_eq!(result, Err(", "assert_eq!(LAST, Err("),
        ] {
            assert!(candidates(&source).is_empty(), "{source}");
        }
    }

    #[test]
    fn a_read_from_a_test_of_another_module_is_not_borrowed() {
        let source = format!(
            "{}\n#[cfg(test)]\nmod other {{\n{READS}}}\n",
            store_with(READS.trim(), "")
        );
        assert!(source.contains("fn reads()"));
        assert!(candidates(&source).is_empty(), "{source}");
    }

    #[test]
    fn one_failing_test_gets_a_bounded_number_of_observations() {
        let source = store_with(
            "assert_eq!(store.get(\"a\"), None);",
            "assert_eq!(store.get(\"a\"), None);\nassert_eq!(store.has(\"a\"), false);",
        )
        .replace(
            "update(\"b\", \"2\")]",
            "update(\"b\", \"2\"), update(\"c\", \"3\")]",
        );
        assert_eq!(
            observations(&source),
            [
                "store.get(\"a\")",
                "store.get(\"b\")",
                "store.get(\"c\")",
                "store.has(\"a\")"
            ]
        );
    }

    #[test]
    fn async_unsafe_and_generic_codecs_are_skipped() {
        for head in [
            "async fn encode(x: u16) -> [u8; 2] { x.to_le_bytes() }",
            "unsafe fn encode(x: u16) -> [u8; 2] { x.to_le_bytes() }",
            "fn encode<T>(x: u16) -> [u8; 2] { x.to_le_bytes() }",
        ] {
            let source = format!(
                "{head}\nfn decode(b: [u8; 2]) -> u16 {{ u16::from_le_bytes(b) }}\n#[test] fn rt() {{ assert_eq!(decode(encode(1)), 1); }}\n"
            );
            assert!(candidates(&source).is_empty(), "{source}");
        }
    }
}
