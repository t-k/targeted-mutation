//! Distinguishing inputs for bit masks, string lengths, and existing range mutants.

use std::collections::BTreeMap;

use syn::parse::Parser;
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::{BinOp, Expr, FnArg, Item, ItemFn, Pat, ReturnType, Stmt, Type};

use super::{Candidate, Edit, Scenario};

pub(super) fn discover(
    source: &str,
    file: &syn::File,
    out: &mut Vec<Candidate>,
    include_existing: bool,
) {
    let mut fixtures = RangeFixtures {
        source,
        tests: Vec::new(),
        seeds: BTreeMap::new(),
    };
    if include_existing {
        fixtures.visit_file(file);
    }
    for item in &file.items {
        let Item::Fn(function) = item else { continue };
        if !supports(function) {
            continue;
        }
        out.extend(bit_mask(source, function));
        out.extend(string_length(source, function));
        if include_existing && let Some(seeds) = fixtures.seeds.get(&function.sig.ident.to_string())
        {
            out.extend(range_mutation(source, function, seeds));
        }
    }
}

fn supports(function: &ItemFn) -> bool {
    let signature = &function.sig;
    signature.generics.params.is_empty()
        && signature.asyncness.is_none()
        && signature.unsafety.is_none()
        && signature.abi.is_none()
        && function.attrs.iter().all(|attr| {
            ["doc", "inline", "must_use"]
                .iter()
                .any(|name| attr.path().is_ident(name))
        })
}

fn parameter(item: &FnArg) -> Option<(&syn::Ident, &Type)> {
    let FnArg::Typed(typed) = item else {
        return None;
    };
    let Pat::Ident(binding) = typed.pat.as_ref() else {
        return None;
    };
    (binding.by_ref.is_none() && binding.subpat.is_none()).then_some((&binding.ident, &typed.ty))
}

fn type_name(ty: &Type) -> Option<&syn::Ident> {
    let Type::Path(named) = ty else { return None };
    named.path.get_ident().filter(|_| named.qself.is_none())
}

fn returns(function: &ItemFn, name: &str) -> bool {
    matches!(&function.sig.output, ReturnType::Type(_, ty) if type_name(ty).is_some_and(|id| id == name))
}

fn is_integer(name: &syn::Ident) -> bool {
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

fn tail(function: &ItemFn) -> Option<&Expr> {
    let [Stmt::Expr(expr, None)] = function.block.stmts.as_slice() else {
        return None;
    };
    Some(expr)
}

fn unparen(mut expr: &Expr) -> &Expr {
    while let Expr::Paren(paren) = expr {
        expr = &paren.expr;
    }
    expr
}

fn path(expr: &Expr) -> Option<&syn::Ident> {
    let Expr::Path(named) = unparen(expr) else {
        return None;
    };
    named.path.get_ident().filter(|_| named.qself.is_none())
}

fn integer_literal(expr: &Expr) -> Option<&syn::LitInt> {
    let Expr::Lit(lit) = unparen(expr) else {
        return None;
    };
    let syn::Lit::Int(value) = &lit.lit else {
        return None;
    };
    Some(value)
}

fn decimal_value(expr: &Expr) -> Option<String> {
    let expr = unparen(expr);
    let (negative, value) = match expr {
        Expr::Unary(unary) if matches!(unary.op, syn::UnOp::Neg(_)) => (true, unary.expr.as_ref()),
        _ => (false, expr),
    };
    let value = integer_literal(value)?.base10_parse::<u128>().ok()?;
    Some(if negative && value != 0 {
        format!("-{value}")
    } else {
        value.to_string()
    })
}

fn bit_mask(source: &str, function: &ItemFn) -> Option<Candidate> {
    let mut arguments = function.sig.inputs.iter();
    let (bits, bits_type) = parameter(arguments.next()?)?;
    let (mask, mask_type) = parameter(arguments.next()?)?;
    if arguments.next().is_some() || !returns(function, "bool") {
        return None;
    }
    let integer = type_name(bits_type)?;
    if !is_integer(integer) || type_name(mask_type) != Some(integer) {
        return None;
    }
    let expr = tail(function)?;
    let Expr::Binary(comparison) = unparen(expr) else {
        return None;
    };
    if !matches!(comparison.op, BinOp::Ne(_)) {
        return None;
    }
    let zero = |expr: &Expr| {
        integer_literal(expr).and_then(|int| int.base10_parse::<u128>().ok()) == Some(0)
    };
    let operation = if zero(&comparison.right) {
        unparen(&comparison.left)
    } else if zero(&comparison.left) {
        unparen(&comparison.right)
    } else {
        return None;
    };
    let Expr::Binary(and) = operation else {
        return None;
    };
    if !matches!(and.op, BinOp::BitAnd(_)) {
        return None;
    }
    let lhs = path(&and.left)?;
    let rhs = path(&and.right)?;
    if !((lhs == bits && rhs == mask) || (lhs == mask && rhs == bits)) {
        return None;
    }
    let original = source.get(expr.span().byte_range())?;
    let replacement = format!("{lhs} & {rhs} == {mask}");
    let description =
        format!("replace any-bit test `{original}` with all-bit test `{replacement}`");
    Some(candidate(
        function,
        expr.span(),
        replacement,
        description,
        false,
        vec![Scenario {
            expression: format!("{}(0b01, 0b11)", function.sig.ident),
            guidance: Some(
                "Use this input if the API accepts masks with multiple bits set.".into(),
            ),
            ..Scenario::default()
        }],
    ))
}

fn string_length(source: &str, function: &ItemFn) -> Option<Candidate> {
    let mut arguments = function.sig.inputs.iter();
    let (text, ty) = parameter(arguments.next()?)?;
    if arguments.next().is_some() || !returns(function, "usize") {
        return None;
    }
    let Type::Reference(reference) = ty else {
        return None;
    };
    if reference.mutability.is_some() || type_name(&reference.elem)? != "str" {
        return None;
    }
    let expr = tail(function)?;
    let Expr::MethodCall(method) = unparen(expr) else {
        return None;
    };
    if !plain_method(method) {
        return None;
    }
    let replacement = if method.method == "len" && path(&method.receiver) == Some(text) {
        format!("{text}.chars().count()")
    } else if method.method == "count"
        && let Expr::MethodCall(chars) = unparen(&method.receiver)
        && chars.method == "chars"
        && plain_method(chars)
        && path(&chars.receiver) == Some(text)
    {
        format!("{text}.len()")
    } else {
        return None;
    };
    let original = source.get(expr.span().byte_range())?;
    let description = format!("replace `{original}` with `{replacement}`");
    Some(candidate(
        function,
        expr.span(),
        replacement,
        description,
        false,
        vec![Scenario {
            expression: format!("{}(\"é\")", function.sig.ident),
            // The input is this tool's, not one the tests gave: it may be outside of
            // what the function is for.
            guidance: Some("Use this input if the API accepts text outside ASCII.".into()),
            ..Scenario::default()
        }],
    ))
}

fn plain_method(method: &syn::ExprMethodCall) -> bool {
    method.args.is_empty() && method.turbofish.is_none()
}

fn range_mutation(source: &str, function: &ItemFn, seeds: &[RangeSeed]) -> Option<Candidate> {
    let mut arguments = function.sig.inputs.iter();
    let (slice_name, ty) = parameter(arguments.next()?)?;
    let (bound, bound_type) = parameter(arguments.next()?)?;
    if arguments.next().is_some() || type_name(bound_type)? != "usize" {
        return None;
    }
    let Type::Reference(reference) = ty else {
        return None;
    };
    let Type::Slice(slice) = reference.elem.as_ref() else {
        return None;
    };
    let element = type_name(&slice.elem)?;
    if reference.mutability.is_some()
        || !is_integer(element)
        || !returns(function, &element.to_string())
    {
        return None;
    }
    let range = function.block.stmts.iter().find_map(|stmt| {
        let Stmt::Expr(Expr::ForLoop(iteration), _) = stmt else {
            return None;
        };
        let Pat::Ident(index) = iteration.pat.as_ref() else {
            return None;
        };
        let Expr::Range(range) = unparen(&iteration.expr) else {
            return None;
        };
        let first = integer_literal(range.start.as_deref()?)?;
        if first.base10_parse::<u128>().ok()? != 0
            || !matches!(range.limits, syn::RangeLimits::HalfOpen(_))
            || path(range.end.as_deref()?)? != bound
        {
            return None;
        }
        let [Stmt::Expr(Expr::Binary(accumulation), _)] = iteration.body.stmts.as_slice() else {
            return None;
        };
        let Expr::Index(value) = unparen(&accumulation.right) else {
            return None;
        };
        (matches!(accumulation.op, BinOp::AddAssign(_))
            && path(&accumulation.left).is_some()
            && path(&value.expr) == Some(slice_name)
            && path(&value.index) == Some(&index.ident))
        .then_some(range)
    })?;
    let first = source.get(range.start.as_deref()?.span().byte_range())?;
    let end = source.get(range.end.as_deref()?.span().byte_range())?;
    let replacement = format!("{first}..={end}");
    let description = format!(
        "`{}..={}`",
        crate::report::one_line(first),
        crate::report::one_line(end)
    );
    Some(candidate(
        function,
        range.span(),
        replacement,
        description,
        true,
        seeds
            .iter()
            .map(|seed| Scenario {
                expression: seed.expression.clone(),
                guidance: seed.expected.as_ref().map(|_| seed.guidance.clone()),
                fixture_expectation: seed.expected.clone(),
                source_test: Some(seed.test.clone()),
                ..Scenario::default()
            })
            .collect(),
    ))
}

struct RangeSeed {
    expression: String,
    guidance: String,
    test: String,
    expected: Option<String>,
}

struct RangeFixtures<'src> {
    source: &'src str,
    tests: Vec<String>,
    seeds: BTreeMap<String, Vec<RangeSeed>>,
}

impl RangeFixtures<'_> {
    fn seed(&mut self, expr: &Expr, expected: &Expr) -> Option<()> {
        let Expr::Call(call) = unparen(expr) else {
            return None;
        };
        let Expr::Path(callee) = unparen(&call.func) else {
            return None;
        };
        let name = callee.path.segments.last()?.ident.to_string();
        if callee.qself.is_some()
            || !(callee.path.get_ident().is_some()
                || (callee.path.segments.len() == 2
                    && (callee.path.segments[0].ident == "crate"
                        || (self.tests.len() == 2 && callee.path.segments[0].ident == "super"))))
        {
            return None;
        }
        let mut arguments = call.args.iter();
        let slice = arguments.next()?;
        let bound = integer_literal(arguments.next()?)?;
        if arguments.next().is_some() {
            return None;
        }
        let Expr::Reference(reference) = unparen(slice) else {
            return None;
        };
        let Expr::Array(array) = unparen(&reference.expr) else {
            return None;
        };
        if reference.mutability.is_some()
            || !array.elems.iter().all(|item| {
                integer_literal(item).is_some()
                    || matches!(unparen(item), Expr::Unary(negative) if matches!(negative.op, syn::UnOp::Neg(_)) && integer_literal(&negative.expr).is_some())
            })
        {
            return None;
        }
        let zero = integer_literal(
            array
                .elems
                .iter()
                .nth(bound.base10_parse::<usize>().ok()?)?,
        )?;
        if zero.base10_parse::<u128>().ok()? != 0 {
            return None;
        }
        let span = slice.span().byte_range();
        let original = self.source.get(span.clone())?;
        let zero_span = zero.span().byte_range();
        let token = self.source.get(zero_span.clone())?;
        // Keep the original radix, separators, and type suffix. Only the last zero digit changes.
        let digit = token.strip_suffix(zero.suffix())?.rfind('0')?;
        let index = zero_span.start.checked_sub(span.start)? + digit;
        let mut changed = original.to_owned();
        changed.replace_range(index..index + 1, "1");
        let test = self.tests.join("::");
        let expression = format!("{name}({changed}, {bound})");
        let scenarios = self.seeds.entry(name).or_default();
        if scenarios.iter().filter(|seed| seed.test == test).count() < 4
            && !scenarios
                .iter()
                .any(|seed| seed.expression == expression && seed.test == test)
        {
            scenarios.push(RangeSeed {
                expression,
                guidance: format!("In `{test}`, change `{original}` to `{changed}`."),
                test,
                expected: decimal_value(expected),
            });
        }
        Some(())
    }
}

impl<'ast> Visit<'ast> for RangeFixtures<'_> {
    fn visit_item_mod(&mut self, module: &'ast syn::ItemMod) {
        self.tests.push(module.ident.to_string());
        syn::visit::visit_item_mod(self, module);
        self.tests.pop();
    }

    fn visit_item_fn(&mut self, function: &'ast ItemFn) {
        if function
            .attrs
            .iter()
            .any(|attr| attr.path().is_ident("test"))
        {
            self.tests.push(function.sig.ident.to_string());
            self.visit_block(&function.block);
            self.tests.pop();
        }
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        if mac.path.is_ident("assert_eq")
            && let Ok(args) = syn::punctuated::Punctuated::<Expr, syn::Token![,]>::parse_terminated
                .parse2(mac.tokens.clone())
            && let Some((left, right)) = args.first().zip(args.iter().nth(1))
        {
            self.seed(left, right);
            self.seed(right, left);
        }
    }
}

fn candidate(
    function: &ItemFn,
    span: proc_macro2::Span,
    replacement: String,
    description: String,
    existing: bool,
    scenarios: Vec<Scenario>,
) -> Candidate {
    Candidate {
        edits: vec![Edit {
            span: span.byte_range(),
            replacement,
        }],
        scope: span.start().line..=span.end().line,
        signature_lines: vec![function.sig.span().start().line],
        description,
        existing,
        scenarios,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates(source: &str) -> Vec<Candidate> {
        let file = syn::parse_file(source).unwrap();
        let mut found = Vec::new();
        discover(source, &file, &mut found, true);
        found
    }

    #[test]
    fn a_partial_mask_distinguishes_any_bit_from_all_bits() {
        let source = "fn enabled(bits: u64, active: u64) -> bool { bits & active != 0 }";
        let found = candidates(source);
        assert_eq!(found.len(), 1);
        let candidate = &found[0];
        assert!(!candidate.existing);
        assert_eq!(
            &source[candidate.edits[0].span.clone()],
            "bits & active != 0"
        );
        assert_eq!(candidate.edits[0].replacement, "bits & active == active");
        assert_eq!(candidate.scenarios[0].expression, "enabled(0b01, 0b11)");
        assert!(
            candidate.scenarios[0]
                .guidance
                .as_ref()
                .unwrap()
                .contains("multiple bits")
        );
    }

    #[test]
    fn a_non_ascii_scalar_distinguishes_char_and_byte_counts() {
        let source = r#"
fn chars(label: &str) -> usize { label.chars().count() }
fn bytes(label: &str) -> usize { label.len() }
fn unrelated(values: &[u8]) -> usize { values.len() }
"#;
        let found = candidates(source);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].edits[0].replacement, "label.len()");
        assert_eq!(found[0].scenarios[0].expression, r#"chars("é")"#);
        assert_eq!(found[1].edits[0].replacement, "label.chars().count()");
        assert_eq!(found[1].scenarios[0].expression, r#"bytes("é")"#);
    }

    #[test]
    fn a_range_witness_changes_only_the_unused_zero_in_an_existing_test() {
        let source = r#"
fn total_of(items: &[u32], bound: usize) -> u32 {
    let mut total = 0;
    for i in 0 .. bound { total += items[i]; }
    total
}
#[cfg(test)]
mod checks {
    use super::*;
    #[test]
    fn preserves_sum() {
        assert_eq!((0x9_u32), total_of(&[4, 5, 0], 2));
        assert_eq!(total_of(&[4], 1), 4);
    }
}
"#;
        let found = candidates(source);
        assert_eq!(found.len(), 1);
        let candidate = &found[0];
        assert!(candidate.existing);
        assert_eq!(candidate.description, "`0..=bound`");
        assert_eq!(&source[candidate.edits[0].span.clone()], "0 .. bound");
        assert_eq!(candidate.edits[0].replacement, "0..=bound");
        assert_eq!(candidate.scenarios.len(), 1);
        let scenario = &candidate.scenarios[0];
        assert_eq!(scenario.expression, "total_of(&[4, 5, 1], 2)");
        assert_eq!(scenario.fixture_expectation.as_deref(), Some("9"));
        assert_eq!(
            scenario.source_test.as_deref(),
            Some("checks::preserves_sum")
        );
        assert_eq!(
            scenario.guidance.as_deref(),
            Some("In `checks::preserves_sum`, change `&[4, 5, 0]` to `&[4, 5, 1]`."),
        );
    }

    #[test]
    fn fixture_expectations_use_the_same_integer_spelling_as_runtime_values() {
        for (input, expected) in [
            ("-0x9_i32", Some("-9")),
            ("-0", Some("0")),
            ("EXPECTED", None),
            ("!0", None),
        ] {
            let value = syn::parse_str(input).unwrap();
            assert_eq!(decimal_value(&value).as_deref(), expected, "{input}");
        }
    }
}
