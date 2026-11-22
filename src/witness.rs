//! Finds source mutations and candidate tests whose results can be compared by execution.

use std::collections::BTreeMap;
use std::ops::{Range, RangeInclusive};

use syn::parse::Parser;
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::{Expr, FnArg, Item, ItemFn, Pat, Type};

use crate::report::VerifiedWitness;

mod relations;
mod sequences;
mod simple;

pub(crate) const TEST_NAME: &str = "__tmt_witness";
pub(crate) const OUTPUT_ENV: &str = "TMT_WITNESS_OUTPUT";

#[derive(Clone, Debug)]
pub(crate) struct Edit {
    pub span: Range<usize>,
    pub replacement: String,
}

#[derive(Debug)]
pub(crate) struct Candidate {
    pub edits: Vec<Edit>,
    pub scope: RangeInclusive<usize>,
    pub signature_lines: Vec<usize>,
    pub description: String,
    pub existing: bool,
    pub scenarios: Vec<Scenario>,
}

#[derive(Debug, Default)]
pub(crate) struct Scenario {
    pub setup: String,
    pub expression: String,
    pub assertion_prefix: String,
    pub guidance: Option<String>,
    pub fixture_expectation: Option<String>,
    pub placement: Option<Placement>,
    pub source_test: Option<String>,
    position: bool,
}

#[derive(Debug)]
pub(crate) struct Placement {
    pub offset: usize,
    pub module: String,
}

#[derive(Debug)]
pub(crate) struct Observation {
    pub value: String,
    pub equals_expected: Option<bool>,
}

impl Scenario {
    pub fn test_name(&self, name: &str) -> String {
        self.placement.as_ref().map_or_else(
            || name.to_owned(),
            |placement| format!("{}::{name}", placement.module),
        )
    }

    pub fn test_edit(&self, source: &str, name: &str, expected: Option<&str>) -> Edit {
        let offset = self.placement.as_ref().map_or(source.len(), |p| p.offset);
        Edit {
            span: offset..offset,
            replacement: self.test_source(name, expected),
        }
    }

    fn test_source(&self, name: &str, expected: Option<&str>) -> String {
        let result_type = if self.position {
            ": __tmt_witness_std::option::Option<__tmt_witness_std::primitive::usize>"
        } else {
            ""
        };
        let observation = match expected {
            Some(expected) => format!(
                r#"__tmt_witness_std::format!("{{result:?}}\t{{}}\n", &result == &({expected}))"#
            ),
            None => r#"__tmt_witness_std::format!("{result:?}\n")"#.to_owned(),
        };
        format!(
            r#"
#[test]
fn {name}() {{
    extern crate std as __tmt_witness_std;
    {setup}
    let result{result_type} = {expression};
    let path = __tmt_witness_std::env::var_os("{OUTPUT_ENV}").unwrap();
    let mut output = __tmt_witness_std::fs::OpenOptions::new().append(true).open(path).unwrap();
    __tmt_witness_std::io::Write::write_all(&mut output, ({observation}).as_bytes()).unwrap();
}}
"#,
            setup = self.setup,
            expression = self.expression,
        )
    }

    pub fn observation(&self, output: &str, compares: bool) -> Option<Observation> {
        let line = output.strip_suffix('\n')?;
        let (value, equals_expected) = if compares {
            let (value, compared) = line.rsplit_once('\t')?;
            (value, Some(compared.parse::<bool>().ok()?))
        } else {
            (line, None)
        };
        if value.is_empty() || value.chars().any(char::is_control) {
            return None;
        }
        if self.position {
            parse_observation(&format!("{value}\n"))?;
        }
        Some(Observation {
            value: value.to_owned(),
            equals_expected,
        })
    }

    pub fn verified(&self, original: String, mutant: String) -> Option<VerifiedWitness> {
        (original != mutant).then(|| {
            let call = self
                .expression
                .strip_prefix("self::")
                .unwrap_or(&self.expression);
            let guidance = self
                .guidance
                .as_ref()
                .filter(|_| {
                    self.fixture_expectation
                        .as_ref()
                        .is_none_or(|expected| *expected == original)
                })
                .cloned();
            VerifiedWitness {
                call: call.to_owned(),
                assertion: if self.assertion_prefix.is_empty() {
                    format!("assert_eq!({call}, {original});")
                } else {
                    format!(
                        "{} assert_eq!({call}, {original});",
                        self.assertion_prefix.trim_end()
                    )
                },
                original,
                mutant,
                guidance,
                individually_killed: None,
            }
        })
    }
}

/// Debug output may resemble code that has side effects. Only try literal expectations.
pub(crate) fn is_expected_literal(text: &str) -> bool {
    fn literal(expr: &Expr) -> bool {
        match expr {
            Expr::Lit(value) => !matches!(value.lit, syn::Lit::Verbatim(_)),
            Expr::Unary(value) if matches!(value.op, syn::UnOp::Neg(_)) => {
                matches!(value.expr.as_ref(), Expr::Lit(number) if matches!(number.lit, syn::Lit::Int(_) | syn::Lit::Float(_)))
            }
            Expr::Path(_) => path_ident(expr).is_some_and(|name| name == "None"),
            Expr::Array(array) => array.elems.iter().all(literal),
            Expr::Tuple(tuple) => tuple.elems.iter().all(literal),
            Expr::Call(call) => {
                path_ident(&call.func)
                    .is_some_and(|name| matches!(name.to_string().as_str(), "Some" | "Ok" | "Err"))
                    && call.args.len() == 1
                    && call.args.first().is_some_and(literal)
            }
            _ => false,
        }
    }
    syn::parse_str::<Expr>(text).is_ok_and(|expr| literal(&expr))
}

pub(crate) fn render_edits(
    source: &str,
    edits: &[Edit],
    insertion: Option<(usize, &str)>,
) -> Option<String> {
    let mut replacements: Vec<_> = edits
        .iter()
        .map(|edit| (edit.span.clone(), edit.replacement.as_str()))
        .collect();
    if let Some((offset, text)) = insertion {
        replacements.push((offset..offset, text));
    }
    replacements.sort_by_key(|(range, _)| (range.start, range.end));

    let mut capacity = source.len();
    let mut end = 0;
    for (range, replacement) in &replacements {
        source.get(end..range.start)?;
        // tmt:ignore the capacity is a hint: any number gives the same text
        capacity -= source.get(range.clone())?.len();
        // tmt:ignore the capacity is a hint: any number gives the same text
        capacity = capacity.checked_add(replacement.len())?;
        end = range.end;
    }

    let mut result = String::with_capacity(capacity);
    let mut end = 0;
    for (range, replacement) in replacements {
        result.push_str(&source[end..range.start]);
        result.push_str(replacement);
        end = range.end;
    }
    result.push_str(&source[end..]);
    Some(result)
}

#[derive(Debug)]
struct PositionSearch {
    pub method: Range<usize>,
    pub lines: RangeInclusive<usize>,
    pub signature_line: usize,
    pub call: String,
}

impl PositionSearch {
    fn scenario(&self) -> Scenario {
        Scenario {
            expression: format!("self::{}", self.call),
            position: true,
            ..Default::default()
        }
    }
}

pub(crate) fn discover_parsed(
    source: &str,
    file: &syn::File,
    include_existing: bool,
) -> Vec<Candidate> {
    if crate::instrument::is_test_only(&file.attrs) || crate::instrument::is_skipped(&file.attrs) {
        return vec![];
    }
    let mut candidates = position_searches(source, file)
        .into_iter()
        .map(|search| {
            let scenario = search.scenario();
            Candidate {
                edits: vec![Edit {
                    span: search.method,
                    replacement: "rposition".to_owned(),
                }],
                scope: search.lines,
                signature_lines: vec![search.signature_line],
                description: "replace `position` with `rposition`".to_owned(),
                existing: false,
                scenarios: vec![scenario],
            }
        })
        .collect();
    simple::discover(source, file, &mut candidates, include_existing);
    relations::discover(source, file, &mut candidates);
    sequences::discover(source, file, &mut candidates, include_existing);
    candidates
}

fn position_searches(source: &str, file: &syn::File) -> Vec<PositionSearch> {
    let mut seeds = None;
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
                .get_or_insert_with(|| {
                    let mut seeds = SearchValues::default();
                    seeds.visit_file(file);
                    seeds
                })
                .values
                .get(&function_name.to_string())
                .copied()
                .unwrap_or(0);
            Some(PositionSearch {
                method: range,
                lines: expression.start().line..=expression.end().line,
                signature_line: function.sig.span().start().line,
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
    // tmt:ignore the two `qself` checks are defensive: a parsed qualified path is never a lone identifier, so `get_ident` has refused it already
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
    use quote::ToTokens;

    #[test]
    fn test_only_library_does_not_propose_source_witnesses() {
        let discover =
            |source: &str| discover_parsed(source, &syn::parse_file(source).unwrap(), true);
        let code = "pub fn chars(text: &str) -> usize { text.chars().count() }";
        assert!(!discover(code).is_empty());
        assert!(discover(&format!("#![cfg(test)]\n{code}")).is_empty());
        assert!(discover(&format!("#![mutants::skip]\n{code}")).is_empty());
    }

    #[test]
    fn witness_test_edits_keep_names_and_placement_for_each_input() {
        // Full 3-way table from witness.pict.
        for nested in [false, true] {
            for expected in [None, Some("7")] {
                for count in [1, 2] {
                    let source = "mod tests { fn helper() {} }";
                    let scenario = Scenario {
                        expression: "7".into(),
                        placement: nested.then(|| Placement {
                            module: "tests".into(),
                            offset: source.len() - 1,
                        }),
                        ..Scenario::default()
                    };
                    let edits: Vec<_> = (0..count)
                        .map(|index| scenario.test_edit(source, &format!("case_{index}"), expected))
                        .collect();
                    let rendered = render_edits(source, &edits, None).unwrap();
                    let parsed = syn::parse_file(&rendered).unwrap();
                    let syn::Item::Mod(module) = &parsed.items[0] else {
                        panic!("{rendered}");
                    };
                    let functions = if nested {
                        &module.content.as_ref().unwrap().1[1..]
                    } else {
                        &parsed.items[1..]
                    };
                    assert_eq!(functions.len(), count);
                    for (index, item) in functions.iter().enumerate() {
                        let syn::Item::Fn(item) = item else {
                            panic!("{rendered}");
                        };
                        let name = format!("case_{index}");
                        assert_eq!(item.sig.ident, name);
                        let expected_name = if nested {
                            format!("tests::{name}")
                        } else {
                            name.clone()
                        };
                        assert_eq!(scenario.test_name(&name), expected_name);
                        assert_eq!(
                            item.block.to_token_stream().to_string().contains("=="),
                            expected.is_some()
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn witness_edits_compose_and_keep_insertion_offsets_in_the_original_source() {
        let source = "éxy";
        let first = Edit {
            span: 2..3,
            replacement: "first".into(),
        };
        let second = Edit {
            span: 3..4,
            replacement: "last".into(),
        };
        assert_eq!(
            render_edits(source, &[second, first], Some((4, "!"))).as_deref(),
            Some("éfirstlast!")
        );
        let bad = Edit {
            span: 1..3,
            replacement: "wrong".into(),
        };
        assert_eq!(render_edits(source, &[bad], None), None);
        let mut reversed = Edit {
            span: 2..3,
            replacement: "wrong".into(),
        };
        reversed.span.end = 0;
        assert_eq!(render_edits(source, &[reversed], None), None);
        assert_eq!(
            render_edits(source, &[], Some((2, "!"))).as_deref(),
            Some("é!xy")
        );
        let overlapping = [
            Edit {
                span: 0..3,
                replacement: "one".into(),
            },
            Edit {
                span: 2..4,
                replacement: "two".into(),
            },
        ];
        assert_eq!(render_edits(source, &overlapping, None), None);
    }

    #[test]
    fn witness_expected_values_are_literals_and_equality_output_is_explicit() {
        for valid in [
            "None",
            "Some(\"old\")",
            "Ok(-3)",
            "Err(\"why\")",
            "[2, 1]",
            "false",
            "-3",
            "(2, true)",
        ] {
            assert!(is_expected_literal(valid), "{valid}");
        }
        for invalid in [
            "Missing",
            "Some(fetch())",
            "Some(1, 2)",
            "None(1)",
            "-true",
            "!1",
            "[file.remove()]",
            "counter.next()",
            "{ 1 }",
            "!true",
        ] {
            assert!(!is_expected_literal(invalid), "{invalid}");
        }
        let scenario = Scenario::default();
        let observation = scenario.observation("Some(2)\tfalse\n", true).unwrap();
        assert_eq!(observation.value, "Some(2)");
        assert_eq!(observation.equals_expected, Some(false));
        let observed_only = scenario.observation("2\n", false).unwrap();
        assert_eq!(observed_only.value, "2");
        assert_eq!(observed_only.equals_expected, None);
        assert!(scenario.observation("\n", false).is_none());
        for corrupted in ["2\n", "2\tunknown\n", "2\n2\ttrue\n", "2\ttrue"] {
            assert!(
                scenario.observation(corrupted, true).is_none(),
                "{corrupted:?}"
            );
        }
    }

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
        let searches = position_searches(source, &syn::parse_file(source).unwrap());
        assert_eq!(searches.len(), 3, "{searches:?}");
        assert_eq!(searches[0].call, "search(&[9, 9], 9)");
        assert_eq!(searches[1].call, "reverse(&[0, 0], 0)");
        assert_eq!(searches[2].call, "nested(&[11, 11], 11)");
        assert_eq!(&source[searches[0].method.clone()], "position");
        assert_eq!(searches[0].lines, 3..=3);
        assert_eq!(searches[0].signature_line, 3);
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
            call: "f(&[0, 0], 0)".into(),
        };
        assert_eq!(
            search.scenario().verified(first.clone(), first.clone()),
            None
        );
        let witness = search.scenario().verified(first, last).unwrap();
        assert_eq!(witness.original, "Some(0)");
        assert_eq!(witness.mutant, "Some(1)");
        assert_eq!(witness.assertion, "assert_eq!(f(&[0, 0], 0), Some(0));");
    }

    #[test]
    fn every_rule_family_contributes_candidates_for_one_file() {
        let source = r#"
            pub fn find(xs: &[u8], key: u8) -> Option<usize> { xs.iter().position(|x| *x == key) }
            pub fn chars(text: &str) -> usize { text.chars().count() }
            pub fn expiry(created: u64, updated: u64, expires: u64) -> bool { created < expires }
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
        let found = discover_parsed(source, &syn::parse_file(source).unwrap(), true);
        let has = |description: &str| found.iter().any(|entry| entry.description == description);
        assert!(has("replace `position` with `rposition`"), "{found:?}");
        assert!(
            has("replace `text.chars().count()` with `text.len()`"),
            "{found:?}"
        );
        assert!(has("replace `created` with `updated`"), "{found:?}");
        let round_trip = found
            .iter()
            .find(|entry| entry.edits.len() == 2)
            .unwrap_or_else(|| panic!("{found:?}"));
        assert_eq!(round_trip.edits[0].replacement, "to_be_bytes");
        assert_eq!(round_trip.edits[1].replacement, "from_be_bytes");
    }

    #[test]
    fn a_position_scenario_declares_and_requires_an_optional_index() {
        let search = PositionSearch {
            method: 0..0,
            lines: 1..=1,
            signature_line: 1,
            call: "f(&[0, 0], 0)".into(),
        };
        let scenario = search.scenario();
        assert_eq!(scenario.expression, "self::f(&[0, 0], 0)");
        let test = scenario.test_edit("", "case", None).replacement;
        assert!(
            test.contains(
                "let result: __tmt_witness_std::option::Option<__tmt_witness_std::primitive::usize> = self::f(&[0, 0], 0);"
            ),
            "{test}"
        );
        assert_eq!(
            scenario.observation("Some(1)\n", false).unwrap().value,
            "Some(1)"
        );
        // Another scenario takes any printed value; a position has to be an index or `None`.
        for invalid in ["Some(01)\n", "7\n", "panic\n"] {
            assert!(
                scenario.observation(invalid, false).is_none(),
                "{invalid:?}"
            );
            assert!(
                Scenario::default().observation(invalid, false).is_some(),
                "{invalid:?}"
            );
        }
    }

    #[test]
    fn only_the_text_position_in_the_given_source_is_replaced() {
        let source = "fn f(xs: &[u8], x: u8) -> Option<usize> { xs.iter().position(|v| *v == x) }";
        let parsed = syn::parse_file(source).unwrap();
        assert_eq!(position_searches(source, &parsed).len(), 1);
        // The edit is a byte range of the source: text that does not hold the method there must stay as it is.
        let other = source.replace("position", "pos1tion");
        assert!(position_searches(&other, &parsed).is_empty());
    }

    #[test]
    fn the_search_method_itself_has_to_be_position() {
        let method = |source: &str| {
            let function: ItemFn = syn::parse_str(source).unwrap();
            position_method(&function).map(|search| search.method.to_string())
        };
        assert_eq!(
            method("fn f(xs: &[u8], x: u8) -> Option<usize> { xs.iter().position(|v| *v == x) }")
                .as_deref(),
            Some("position")
        );
        assert_eq!(
            method("fn f(xs: &[u8], x: u8) -> Option<usize> { xs.iter().rposition(|v| *v == x) }"),
            None
        );
    }

    #[test]
    fn syn_qualified_paths_are_never_a_lone_identifier() {
        // `position_method` and the rules below rely on this to refuse `<T>::u8` and `<T as A>::u8`.
        for qualified in ["<T>::u8", "<T as A>::u8"] {
            let Type::Path(path) = syn::parse_str::<Type>(qualified).unwrap() else {
                panic!("{qualified}");
            };
            assert!(path.qself.is_some(), "{qualified}");
            assert!(path.path.get_ident().is_none(), "{qualified}");
        }
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
            let parsed = syn::parse_file(&source).unwrap_or_else(|err| panic!("{source}: {err}"));
            assert!(position_searches(&source, &parsed).is_empty(), "{source}");
        }
    }
}
