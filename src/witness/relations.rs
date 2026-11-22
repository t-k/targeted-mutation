//! Finds direct comparisons and sort keys that can use another same-type value.

use std::collections::BTreeMap;

use syn::spanned::Spanned;
use syn::{BinOp, Expr, FnArg, Ident, Item, ItemFn, Member, Pat, Stmt, Type};

use super::{Candidate, Edit, Scenario};

const MAX_PARAMETERS: usize = 8;
const MAX_FIELDS: usize = 16;
const MAX_CANDIDATES_PER_FUNCTION: usize = 12;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Integer {
    Signed(Option<u32>),
    Unsigned(Option<u32>),
}

impl Integer {
    fn from_type(ty: &Type) -> Option<Self> {
        let name = type_name(ty)?.to_string();
        Some(match name.as_str() {
            "i8" => Self::Signed(Some(8)),
            "i16" => Self::Signed(Some(16)),
            "i32" => Self::Signed(Some(32)),
            "i64" => Self::Signed(Some(64)),
            "i128" => Self::Signed(Some(128)),
            "isize" => Self::Signed(None),
            "u8" => Self::Unsigned(Some(8)),
            "u16" => Self::Unsigned(Some(16)),
            "u32" => Self::Unsigned(Some(32)),
            "u64" => Self::Unsigned(Some(64)),
            "u128" => Self::Unsigned(Some(128)),
            "usize" => Self::Unsigned(None),
            _ => return None,
        })
    }

    // Use the minimum pointer width so the suggested literal fits on all targets.
    fn contains(self, value: i128) -> bool {
        match self {
            Self::Signed(Some(128)) => true,
            Self::Signed(bits) => {
                let bound = 1_i128 << (bits.unwrap_or(16) - 1);
                (-bound..bound).contains(&value)
            }
            Self::Unsigned(Some(128)) => value >= 0,
            Self::Unsigned(bits) => (0..(1_i128 << bits.unwrap_or(16))).contains(&value),
        }
    }
}

struct Record<'a> {
    name: &'a Ident,
    fields: Vec<(&'a Ident, Integer)>,
}

impl Record<'_> {
    fn literal(&self, value: impl Fn(usize) -> i128) -> String {
        let fields = self
            .fields
            .iter()
            .enumerate()
            .map(|(index, (field, _))| format!("{field}: {}", value(index)))
            .collect::<Vec<_>>()
            .join(", ");
        format!("{} {{ {fields} }}", self.name)
    }
}

enum ParameterType<'a> {
    Integer(Integer),
    Record {
        record: &'a Record<'a>,
        borrowed: bool,
    },
}

struct Parameter<'a> {
    name: &'a Ident,
    ty: ParameterType<'a>,
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Value {
    parameter: usize,
    field: Option<usize>,
}

enum Operand {
    Place(Value),
    Literal(i128),
}

pub(super) fn discover(source: &str, file: &syn::File, out: &mut Vec<Candidate>) {
    let records: BTreeMap<_, _> = file
        .items
        .iter()
        .filter_map(|item| {
            let Item::Struct(item) = item else {
                return None;
            };
            let syn::Fields::Named(fields) = &item.fields else {
                return None;
            };
            if !item.generics.params.is_empty() || fields.named.len() > MAX_FIELDS {
                return None;
            }
            let fields = fields
                .named
                .iter()
                .map(|field| Some((field.ident.as_ref()?, Integer::from_type(&field.ty)?)))
                .collect::<Option<Vec<_>>>()?;
            Some((
                &item.ident,
                Record {
                    name: &item.ident,
                    fields,
                },
            ))
        })
        .collect();

    for item in &file.items {
        let Item::Fn(function) = item else { continue };
        if !is_direct_function(function) {
            continue;
        }
        let [Stmt::Expr(expression, semicolon)] = &function.block.stmts[..] else {
            continue;
        };
        let limit = out.len().saturating_add(MAX_CANDIDATES_PER_FUNCTION);
        match unparen(expression) {
            Expr::Binary(comparison) if semicolon.is_none() => {
                if let Some(parameters) = parameters(function, &records) {
                    compare_candidates(source, function, comparison, &parameters, out, limit);
                }
            }
            Expr::MethodCall(call) => sort_candidates(source, function, call, &records, out, limit),
            _ => {}
        }
    }
}

fn is_direct_function(function: &ItemFn) -> bool {
    let signature = &function.sig;
    signature.generics.params.is_empty()
        && signature.asyncness.is_none()
        && signature.unsafety.is_none()
        && signature.constness.is_none()
        && signature.abi.is_none()
        && function.attrs.iter().all(|attr| {
            ["doc", "inline", "must_use"]
                .iter()
                .any(|name| attr.path().is_ident(name))
        })
}

fn type_name(ty: &Type) -> Option<&Ident> {
    let Type::Path(path) = ty else { return None };
    if path.qself.is_some() {
        return None;
    }
    path.path.get_ident()
}

fn binding(pattern: &Pat) -> Option<&Ident> {
    let Pat::Ident(binding) = pattern else {
        return None;
    };
    (binding.by_ref.is_none() && binding.subpat.is_none()).then_some(&binding.ident)
}

fn unparen(mut expression: &Expr) -> &Expr {
    loop {
        expression = match expression {
            Expr::Paren(paren) => &paren.expr,
            Expr::Group(group) => &group.expr,
            _ => return expression,
        };
    }
}

fn path_name(expression: &Expr) -> Option<&Ident> {
    let Expr::Path(path) = unparen(expression) else {
        return None;
    };
    if path.qself.is_some() {
        return None;
    }
    path.path.get_ident()
}

fn parameters<'a>(
    function: &'a ItemFn,
    records: &'a BTreeMap<&Ident, Record<'a>>,
) -> Option<Vec<Parameter<'a>>> {
    if function.sig.inputs.len() > MAX_PARAMETERS {
        return None;
    }
    function
        .sig
        .inputs
        .iter()
        .map(|arg| {
            let FnArg::Typed(arg) = arg else { return None };
            let name = binding(&arg.pat)?;
            let ty = if let Some(integer) = Integer::from_type(&arg.ty) {
                ParameterType::Integer(integer)
            } else {
                let (ty, borrowed) = match arg.ty.as_ref() {
                    Type::Reference(reference) if reference.mutability.is_none() => {
                        (reference.elem.as_ref(), true)
                    }
                    ty => (ty, false),
                };
                ParameterType::Record {
                    record: records.get(type_name(ty)?)?,
                    borrowed,
                }
            };
            Some(Parameter { name, ty })
        })
        .collect()
}

fn named_field(expression: &Expr) -> Option<(&Ident, &Ident)> {
    let Expr::Field(field) = unparen(expression) else {
        return None;
    };
    let Member::Named(name) = &field.member else {
        return None;
    };
    Some((path_name(&field.base)?, name))
}

fn place<'a>(expression: &'a Expr, params: &[Parameter<'_>]) -> Option<(Value, &'a Ident)> {
    if let Some(name) = path_name(expression) {
        let parameter = params.iter().position(|parameter| {
            parameter.name == name && matches!(parameter.ty, ParameterType::Integer(_))
        })?;
        return Some((
            Value {
                parameter,
                field: None,
            },
            name,
        ));
    }
    let (base, field_name) = named_field(expression)?;
    let parameter = params.iter().position(|parameter| parameter.name == base)?;
    let ParameterType::Record { record, .. } = params[parameter].ty else {
        return None;
    };
    let field = record
        .fields
        .iter()
        .position(|(name, _)| *name == field_name)?;
    Some((
        Value {
            parameter,
            field: Some(field),
        },
        field_name,
    ))
}

fn integer_literal(expression: &Expr) -> Option<i128> {
    let (expression, negative) = match unparen(expression) {
        Expr::Unary(unary) if matches!(unary.op, syn::UnOp::Neg(_)) => (unparen(&unary.expr), true),
        expression => (expression, false),
    };
    let Expr::Lit(literal) = expression else {
        return None;
    };
    let syn::Lit::Int(integer) = &literal.lit else {
        return None;
    };
    let value = integer.base10_parse::<u128>().ok()?;
    if negative && value == 1_u128 << 127 {
        return Some(i128::MIN);
    }
    let value = i128::try_from(value).ok()?;
    Some(if negative { -value } else { value })
}

fn operand(expression: &Expr, params: &[Parameter<'_>]) -> Option<Operand> {
    if let Some((value, _)) = place(expression, params) {
        return Some(Operand::Place(value));
    }
    integer_literal(expression).map(Operand::Literal)
}

fn integer_type(value: Value, params: &[Parameter<'_>]) -> Integer {
    match &params[value.parameter].ty {
        ParameterType::Integer(integer) => *integer,
        ParameterType::Record { record, .. } => record.fields[value.field.unwrap()].1,
    }
}

fn alternatives(value: Value, params: &[Parameter<'_>]) -> Vec<Value> {
    let integer = integer_type(value, params);
    match &params[value.parameter].ty {
        ParameterType::Integer(_) => params
            .iter()
            .enumerate()
            .filter_map(|(parameter, arg)| {
                matches!(arg.ty, ParameterType::Integer(other) if integer == other).then_some(
                    Value {
                        parameter,
                        field: None,
                    },
                )
            })
            .collect(),
        ParameterType::Record { record, .. } => record
            .fields
            .iter()
            .enumerate()
            .filter_map(|(field, (_, other))| {
                (*other == integer).then_some(Value {
                    parameter: value.parameter,
                    field: Some(field),
                })
            })
            .collect(),
    }
}

fn place_name(value: Value, params: &[Parameter<'_>]) -> String {
    let parameter = &params[value.parameter];
    match &parameter.ty {
        ParameterType::Integer(_) => parameter.name.to_string(),
        ParameterType::Record { record, .. } => {
            format!(
                "{}.{}",
                parameter.name,
                record.fields[value.field.unwrap()].0
            )
        }
    }
}

fn alternative_name(value: Value, params: &[Parameter<'_>]) -> String {
    let parameter = &params[value.parameter];
    match &parameter.ty {
        ParameterType::Integer(_) => parameter.name.to_string(),
        ParameterType::Record { record, .. } => record.fields[value.field.unwrap()].0.to_string(),
    }
}

fn compare(op: &BinOp, lhs: i128, rhs: i128) -> Option<bool> {
    Some(match op {
        // tmt:ignore the negation is the same: `across` only asks whether two outcomes differ
        BinOp::Eq(_) => lhs == rhs,
        // tmt:ignore the negation is the same: `across` only asks whether two outcomes differ
        BinOp::Ne(_) => lhs != rhs,
        // tmt:ignore the negation is the same: `across` only asks whether two outcomes differ
        BinOp::Lt(_) => lhs < rhs,
        // tmt:ignore the negation is the same: `across` only asks whether two outcomes differ
        BinOp::Le(_) => lhs <= rhs,
        // tmt:ignore the negation is the same: `across` only asks whether two outcomes differ
        BinOp::Gt(_) => lhs > rhs,
        // tmt:ignore the negation is the same: `across` only asks whether two outcomes differ
        BinOp::Ge(_) => lhs >= rhs,
        _ => return None,
    })
}

fn across(pivot: i128, primitive: Integer, op: &BinOp, change_lhs: bool) -> Option<(i128, i128)> {
    let values: Vec<_> = [pivot.checked_sub(1), pivot.checked_add(1), Some(pivot)]
        .into_iter()
        .flatten()
        .filter(|&value| primitive.contains(value))
        .collect();
    let evaluate = |value| {
        if change_lhs {
            compare(op, value, pivot)
        } else {
            compare(op, pivot, value)
        }
    };
    for (index, &first) in values.iter().enumerate() {
        // tmt:ignore a value paired with itself never differs, so starting at `index` finds the same pair
        for &second in &values[index + 1..] {
            if evaluate(first)? != evaluate(second)? {
                // Equality tells the pivot from the rest, and the pivot is the last value:
                // it is `second` here. The original of an equality is the equal case.
                return Some(if matches!(op, BinOp::Eq(_) | BinOp::Ne(_)) {
                    (second, first)
                } else {
                    (first, second)
                });
            }
        }
    }
    None
}

fn compare_candidates(
    source: &str,
    function: &ItemFn,
    expression: &syn::ExprBinary,
    params: &[Parameter<'_>],
    out: &mut Vec<Candidate>,
    limit: usize,
) {
    for (change_lhs, changed, other) in [
        (true, expression.left.as_ref(), expression.right.as_ref()),
        (false, expression.right.as_ref(), expression.left.as_ref()),
    ] {
        let Some((chosen, token)) = place(changed, params) else {
            continue;
        };
        let Some(opposite) = operand(other, params) else {
            continue;
        };
        let integer = integer_type(chosen, params);
        let pivot = match opposite {
            // A literal outside the type needs no check: `across` then has at most one
            // value in range and finds no pair.
            Operand::Literal(value) => value,
            Operand::Place(value) if value != chosen && integer_type(value, params) == integer => 1,
            _ => continue,
        };
        let Some((original, alternative)) = across(pivot, integer, &expression.op, change_lhs)
        else {
            continue;
        };
        for new in alternatives(chosen, params) {
            if out.len() == limit {
                return;
            }
            if new == chosen || matches!(opposite, Operand::Place(value) if new == value) {
                continue;
            }
            let mut values = BTreeMap::from([(chosen, original), (new, alternative)]);
            if let Operand::Place(value) = opposite {
                values.insert(value, pivot);
            }
            let args = params
                .iter()
                .enumerate()
                .map(|(parameter, input)| {
                    let value = |field| {
                        values
                            .get(&Value { parameter, field })
                            .copied()
                            .unwrap_or(0)
                    };
                    match &input.ty {
                        ParameterType::Integer(_) => value(None).to_string(),
                        ParameterType::Record { record, borrowed } => {
                            let literal = record.literal(|index| value(Some(index)));
                            if *borrowed {
                                format!("&{literal}")
                            } else {
                                literal
                            }
                        }
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            push_candidate(
                source,
                function,
                token,
                expression.span(),
                alternative_name(new, params),
                format!(
                    "replace `{}` with `{}`",
                    place_name(chosen, params),
                    place_name(new, params)
                ),
                Scenario {
                    expression: format!("{}({args})", function.sig.ident),
                    // Arguments of this tool's making: together they may be a case
                    // the function is never given.
                    guidance: Some("Use these arguments if the API accepts them together.".into()),
                    ..Scenario::default()
                },
                out,
            );
        }
    }
}

fn sort_candidates(
    source: &str,
    function: &ItemFn,
    expression: &syn::ExprMethodCall,
    records: &BTreeMap<&Ident, Record<'_>>,
    out: &mut Vec<Candidate>,
    limit: usize,
) {
    if expression.method != "sort_by_key" && expression.method != "sort_unstable_by_key" {
        return;
    }
    let mut args = function.sig.inputs.iter();
    let (Some(FnArg::Typed(arg)), None) = (args.next(), args.next()) else {
        return;
    };
    let Type::Reference(reference) = arg.ty.as_ref() else {
        return;
    };
    let Type::Slice(slice) = reference.elem.as_ref() else {
        return;
    };
    let Some(record) = type_name(&slice.elem).and_then(|name| records.get(name)) else {
        return;
    };
    let mut calls = expression.args.iter();
    let (Some(Expr::Closure(key)), None) = (calls.next(), calls.next()) else {
        return;
    };
    let mut inputs = key.inputs.iter();
    let (Some(param), None) = (inputs.next(), inputs.next()) else {
        return;
    };
    let Some((base, chosen)) = named_field(&key.body) else {
        return;
    };
    if reference.mutability.is_none()
        || expression.turbofish.is_some()
        || path_name(&expression.receiver).is_none()
        || path_name(&expression.receiver) != binding(&arg.pat)
        || Some(base) != binding(param)
    {
        return;
    }
    let Some((index, (_, integer))) = record
        .fields
        .iter()
        .enumerate()
        .find(|(_, (name, _))| *name == chosen)
    else {
        return;
    };
    let function_name = &function.sig.ident;
    let rows = if function_name == "rows" {
        "tmt_rows"
    } else {
        "rows"
    };
    for (alternative, (field, ty)) in record.fields.iter().enumerate() {
        if out.len() == limit {
            break;
        }
        if alternative == index || ty != integer {
            continue;
        }
        let first = record.literal(|i| {
            if i == index {
                10
            } else if i == alternative {
                40
            } else {
                0
            }
        });
        let second = record.literal(|i| {
            if i == index {
                20
            } else if i == alternative {
                30
            } else {
                0
            }
        });
        let setup = format!("let mut {rows} = [{first}, {second}]; {function_name}(&mut {rows});");
        push_candidate(
            source,
            function,
            chosen,
            expression.span(),
            field.to_string(),
            format!("replace sort key `{chosen}` with `{field}`"),
            Scenario {
                setup: setup.clone(),
                expression: format!("[{rows}[0].{chosen}, {rows}[1].{chosen}]"),
                assertion_prefix: setup,
                guidance: Some("Use these rows if the API accepts such values.".into()),
                ..Scenario::default()
            },
            out,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn push_candidate(
    source: &str,
    function: &ItemFn,
    token: &Ident,
    expression: proc_macro2::Span,
    replacement: String,
    description: String,
    scenario: Scenario,
    out: &mut Vec<Candidate>,
) {
    let span = token.span().byte_range();
    if source.get(span.clone()) != Some(token.to_string().as_str()) {
        return;
    }
    out.push(Candidate {
        edits: vec![Edit { span, replacement }],
        scope: expression.start().line..=expression.end().line,
        signature_lines: vec![function.sig.span().start().line],
        description,
        existing: false,
        scenarios: vec![scenario],
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates(source: &str) -> Vec<Candidate> {
        let mut candidates = Vec::new();
        discover(source, &syn::parse_file(source).unwrap(), &mut candidates);
        candidates
    }

    #[test]
    fn arguments_straddle_the_other_operand() {
        let source =
            "fn expiry(created: u64, updated: u64, expires: u64) -> bool { created < expires }";
        let all = candidates(source);
        let candidate = all
            .iter()
            .find(|entry| entry.description == "replace `created` with `updated`")
            .unwrap();
        assert_eq!(&source[candidate.edits[0].span.clone()], "created");
        assert_eq!(candidate.edits[0].replacement, "updated");
        assert_eq!(candidate.scenarios[0].expression, "expiry(0, 2, 1)");
    }

    #[test]
    fn fields_can_straddle_an_argument_or_a_fixed_boundary() {
        let source = r#"
struct Entry { created_at: u64, updated_at: u64 }
fn field_expiry(row: &Entry, expires: u64) -> bool { row.created_at < expires }
fn mature(row: Entry) -> bool { row.created_at >= 5000 }
"#;
        let all = candidates(source);
        let created_to_updated: Vec<_> = all
            .iter()
            .filter(|entry| entry.description == "replace `row.created_at` with `row.updated_at`")
            .collect();
        assert_eq!(created_to_updated.len(), 2);
        assert_eq!(
            &source[created_to_updated[0].edits[0].span.clone()],
            "created_at"
        );
        assert_eq!(
            created_to_updated[0].scenarios[0].expression,
            "field_expiry(&Entry { created_at: 0, updated_at: 2 }, 1)"
        );
        assert_eq!(
            created_to_updated[1].scenarios[0].expression,
            "mature(Entry { created_at: 4999, updated_at: 5001 })"
        );
    }

    #[test]
    fn equality_and_primitive_type_boundaries_are_reachable() {
        let source = r#"
fn unsigned(a: u8, b: u8) -> bool { a == 0 }
fn signed(a: i8, b: i8) -> bool { -128 > a }
fn reversed(a: i16, b: i16) -> bool { -128 >= a }
"#;
        let all = candidates(source);
        let witnesses: Vec<_> = all
            .iter()
            .map(|entry| entry.scenarios[0].expression.as_str())
            .collect();
        assert_eq!(witnesses, ["unsigned(0, 1)", "reversed(-129, -127)"]);
    }

    #[test]
    fn sort_key_alternatives_reverse_two_rows() {
        let source = r#"
struct Entry { created_at: u64, updated_at: u64, id: u8 }
fn sort_entries(rows: &mut [Entry]) { rows.sort_by_key(|row| row.created_at); }
"#;
        let all = candidates(source);
        assert_eq!(all.len(), 1);
        let candidate = &all[0];
        assert_eq!(
            candidate.description,
            "replace sort key `created_at` with `updated_at`"
        );
        assert_eq!(&source[candidate.edits[0].span.clone()], "created_at");
        let observation = &candidate.scenarios[0];
        assert!(
            observation
                .setup
                .contains("Entry { created_at: 10, updated_at: 40, id: 0 }")
        );
        assert!(
            observation
                .setup
                .contains("Entry { created_at: 20, updated_at: 30, id: 0 }")
        );
        assert!(observation.setup.contains("sort_entries(&mut rows);"));
        assert_eq!(observation.assertion_prefix, observation.setup);
        assert_eq!(
            observation.expression,
            "[rows[0].created_at, rows[1].created_at]"
        );
    }

    #[test]
    fn requires_compatible_types_and_direct_syntax() {
        let source = r#"
struct Entry { created_at: u64, updated_at: u32 }
fn mixed(a: u64, alternate: i64, limit: u64) -> bool { a < limit }
fn unrelated(row: &Entry, limit: u64) -> bool { row.created_at < limit }
fn computed(a: u64, alternate: u64, limit: u64) -> bool { a + 1 < limit }
fn hidden(a: u64, alternate: u64, limit: u64) -> bool { let a = limit; a < limit }
fn wrong_key(rows: &mut [Entry]) { rows.sort_by_key(|row| row.created_at + 1); }
"#;
        assert!(candidates(source).is_empty());
    }

    fn witnesses(source: &str) -> Vec<String> {
        candidates(source)
            .iter()
            .map(|entry| entry.scenarios[0].expression.clone())
            .collect()
    }

    const ENTRY: &str = "struct Entry { created_at: u64, updated_at: u64, id: u8 }\n";

    fn with_entry(function: &str) -> String {
        format!("{ENTRY}{function}")
    }

    #[test]
    fn every_primitive_integer_type_is_compared() {
        for ty in [
            "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16", "u32", "u64", "u128", "usize",
        ] {
            let source = format!("fn below(a: {ty}, b: {ty}) -> bool {{ a < 5 }}");
            assert_eq!(witnesses(&source), ["below(4, 6)"], "{ty}");
        }
        assert!(witnesses("fn below(a: f64, b: f64) -> bool { a < 5 }").is_empty());
    }

    #[test]
    fn each_comparison_gets_arguments_on_both_sides_of_its_boundary() {
        for (comparison, expected) in [
            ("a < 5", "f(4, 6)"),
            ("a <= 5", "f(4, 6)"),
            ("a > 5", "f(4, 6)"),
            ("a >= 5", "f(4, 6)"),
            ("5 > a", "f(4, 6)"),
            ("a == 5", "f(5, 4)"),
            ("a != 5", "f(5, 4)"),
            // Nothing lies below 0: the pair is the boundary and the value above it.
            ("a > 0", "f(1, 0)"),
            ("a <= 0", "f(1, 0)"),
            ("a != 0", "f(0, 1)"),
            // Nothing lies above 255.
            ("a < 255", "f(254, 255)"),
            ("a >= 255", "f(254, 255)"),
        ] {
            let source = format!("fn f(a: u8, b: u8) -> bool {{ {comparison} }}");
            assert_eq!(witnesses(&source), [expected], "{comparison}");
        }
    }

    #[test]
    fn a_comparison_no_value_of_the_type_can_change_has_no_witness() {
        for comparison in [
            "a < 0", "a >= 0", "a > 255", "a <= 255", "a < 256", "a < 300", "a > -1", "a == 256",
        ] {
            let source = format!("fn f(a: u8, b: u8) -> bool {{ {comparison} }}");
            assert!(witnesses(&source).is_empty(), "{comparison}");
        }
        assert!(witnesses("fn f(a: u128, b: u128) -> bool { a > -1 }").is_empty());
        assert_eq!(
            witnesses("fn f(a: u128, b: u128) -> bool { a > 0 }"),
            ["f(1, 0)"]
        );
        assert!(witnesses("fn f(a: u8, b: u8) -> u8 { a & 5 }").is_empty());
    }

    #[test]
    fn literals_may_be_negative_or_parenthesized_but_not_otherwise_computed() {
        for (comparison, expected) in [
            ("a < -5", "f(-6, -4)"),
            ("a < -(5)", "f(-6, -4)"),
            ("(a) < (5)", "f(4, 6)"),
            (
                "a > -170141183460469231731687303715884105728",
                "f(-170141183460469231731687303715884105727, -170141183460469231731687303715884105728)",
            ),
        ] {
            let source = format!("fn f(a: i128, b: i128) -> bool {{ {comparison} }}");
            assert_eq!(witnesses(&source), [expected], "{comparison}");
        }
        for comparison in [
            "a < !5",
            "a < 5 + 1",
            // One more than the largest i128.
            "a > 170141183460469231731687303715884105728",
        ] {
            let source = format!("fn f(a: i128, b: i128) -> bool {{ {comparison} }}");
            assert!(witnesses(&source).is_empty(), "{comparison}");
        }
    }

    #[test]
    fn only_plain_functions_are_proposed() {
        let rest = "(a: u64, b: u64, c: u64) -> bool { a < c }";
        for start in [
            "fn f",
            "#[inline] fn f",
            "/// Says whether `a` is below `c`.\n#[must_use] pub fn f",
        ] {
            assert_eq!(
                witnesses(&format!("{start}{rest}")),
                ["f(0, 2, 1)", "f(1, 2, 0)"],
                "{start}"
            );
        }
        for start in [
            "fn f<T>",
            "async fn f",
            "unsafe fn f",
            "const fn f",
            "extern \"C\" fn f",
            "#[cfg(unix)] fn f",
        ] {
            assert!(witnesses(&format!("{start}{rest}")).is_empty(), "{start}");
        }
        // A statement, not the value of the function.
        assert!(witnesses("fn f(a: u64, b: u64, c: u64) { a < c; }").is_empty());
    }

    #[test]
    fn parameters_must_be_plain_bindings() {
        assert_eq!(
            witnesses("fn f(mut a: u64, b: u64, c: u64) -> bool { b < c }"),
            ["f(2, 0, 1)", "f(2, 1, 0)"]
        );
        for parameter in ["ref a: u64", "a @ _: u64", "(a, _): (u64, u64)"] {
            let source = format!("fn f({parameter}, b: u64, c: u64) -> bool {{ b < c }}");
            assert!(witnesses(&source).is_empty(), "{parameter}");
        }
    }

    fn many_parameters(count: usize) -> String {
        let parameters: Vec<_> = (0..count).map(|index| format!("p{index}: u64")).collect();
        format!("fn f({}) -> bool {{ p0 < 5 }}", parameters.join(", "))
    }

    fn wide(fields: usize) -> String {
        let fields: Vec<_> = (0..fields).map(|index| format!("f{index}: u64")).collect();
        format!("struct Wide {{ {} }}\n", fields.join(", "))
    }

    const COMPARE_WIDE: &str = "fn first(row: Wide) -> bool { row.f0 < 5 }\n";
    const SORT_WIDE: &str = "fn sort_wide(rows: &mut [Wide]) { rows.sort_by_key(|row| row.f0); }\n";

    #[test]
    fn long_parameter_lists_and_large_records_are_left_alone() {
        assert_eq!(witnesses(&many_parameters(MAX_PARAMETERS)).len(), 7);
        assert!(witnesses(&many_parameters(MAX_PARAMETERS + 1)).is_empty());
        for function in [COMPARE_WIDE, SORT_WIDE] {
            let source = format!("{}{function}", wide(MAX_FIELDS + 1));
            assert!(witnesses(&source).is_empty(), "{function}");
        }
    }

    #[test]
    fn each_function_is_given_a_bounded_number_of_candidates() {
        // 15 other fields could replace `f0`.
        for function in [COMPARE_WIDE, SORT_WIDE] {
            let source = format!("{}{function}", wide(MAX_FIELDS));
            assert_eq!(
                witnesses(&source).len(),
                MAX_CANDIDATES_PER_FUNCTION,
                "{function}"
            );
        }
        let source = format!("{}{COMPARE_WIDE}{SORT_WIDE}", wide(MAX_FIELDS));
        assert_eq!(witnesses(&source).len(), 2 * MAX_CANDIDATES_PER_FUNCTION);
    }

    #[test]
    fn a_field_is_found_by_its_parameter_and_its_name() {
        let source =
            with_entry("fn late(limit: u64, row: &Entry) -> bool { row.updated_at < limit }");
        let all = candidates(&source);
        assert_eq!(all.len(), 1);
        let candidate = &all[0];
        assert_eq!(
            candidate.description,
            "replace `row.updated_at` with `row.created_at`"
        );
        assert_eq!(&source[candidate.edits[0].span.clone()], "updated_at");
        assert_eq!(candidate.edits[0].replacement, "created_at");
        let scenario = &candidate.scenarios[0];
        assert_eq!(
            scenario.expression,
            "late(1, &Entry { created_at: 2, updated_at: 0, id: 0 })"
        );
        assert_eq!(
            scenario.guidance.as_deref(),
            Some("Use these arguments if the API accepts them together.")
        );
    }

    #[test]
    fn records_are_compared_by_their_integer_fields_only() {
        for function in [
            // Whole records: there is no integer to move across a boundary.
            "fn same(row: Entry, other: Entry) -> bool { row == other }",
            "fn below(row: Entry, limit: u64) -> bool { row < limit }",
            // The suggested call passes a record by value or behind `&`.
            "fn late(row: &mut Entry, limit: u64) -> bool { row.created_at < limit }",
        ] {
            assert!(witnesses(&with_entry(function)).is_empty(), "{function}");
        }
        // No call can name the parameter of a generic record.
        let source = "struct Entry<const N: usize = 0> { created_at: u64, updated_at: u64 }
fn late(row: Entry, limit: u64) -> bool { row.created_at < limit }";
        assert!(witnesses(source).is_empty());
    }

    #[test]
    fn the_other_operand_is_another_value_of_the_same_type() {
        for function in [
            "fn f(a: u64, b: u64) -> bool { a < a }",
            "fn f(a: u64, b: u64, c: u32) -> bool { a < c }",
        ] {
            assert!(witnesses(function).is_empty(), "{function}");
        }
    }

    #[test]
    fn either_sort_by_key_method_is_a_sort_key() {
        for (method, expected) in [
            ("sort_by_key", 1),
            ("sort_unstable_by_key", 1),
            ("dedup_by_key", 0),
        ] {
            let source = with_entry(&format!(
                "fn sort_entries(rows: &mut [Entry]) {{ rows.{method}(|row| row.created_at); }}"
            ));
            assert_eq!(candidates(&source).len(), expected, "{method}");
        }
    }

    #[test]
    fn the_sort_key_is_replaced_whichever_field_it_is() {
        let source = with_entry(
            "fn sort_entries(rows: &mut [Entry]) { rows.sort_by_key(|row| row.updated_at); }",
        );
        let all = candidates(&source);
        assert_eq!(all.len(), 1);
        assert_eq!(
            all[0].description,
            "replace sort key `updated_at` with `created_at`"
        );
        assert_eq!(&source[all[0].edits[0].span.clone()], "updated_at");
        assert_eq!(all[0].edits[0].replacement, "created_at");
        let scenario = &all[0].scenarios[0];
        assert_eq!(
            scenario.setup,
            "let mut rows = [Entry { created_at: 40, updated_at: 10, id: 0 }, Entry { created_at: 30, updated_at: 20, id: 0 }]; sort_entries(&mut rows);"
        );
        assert_eq!(
            scenario.expression,
            "[rows[0].updated_at, rows[1].updated_at]"
        );
        assert_eq!(
            scenario.guidance.as_deref(),
            Some("Use these rows if the API accepts such values.")
        );
    }

    #[test]
    fn the_rows_of_a_function_named_rows_get_another_name() {
        let source =
            with_entry("fn rows(rows: &mut [Entry]) { rows.sort_by_key(|row| row.created_at); }");
        let all = candidates(&source);
        assert_eq!(all.len(), 1);
        let scenario = &all[0].scenarios[0];
        assert!(scenario.setup.starts_with("let mut tmt_rows = [Entry {"));
        assert!(scenario.setup.ends_with("; rows(&mut tmt_rows);"));
        assert_eq!(
            scenario.expression,
            "[tmt_rows[0].created_at, tmt_rows[1].created_at]"
        );
    }

    #[test]
    fn a_sort_must_be_of_the_mutable_slice_parameter_by_a_field_of_the_closure_parameter() {
        for function in [
            "fn sort_entries(rows: &[Entry]) { rows.sort_by_key(|row| row.created_at); }",
            "fn sort_entries(rows: &mut [Entry]) { rows.sort_by_key::<u64, _>(|row| row.created_at); }",
            "fn sort_entries(rows: &mut [Entry]) { OTHER.sort_by_key(|row| row.created_at); }",
            "fn sort_entries(_: &mut [Entry]) { other().sort_by_key(|row| row.created_at); }",
            "fn sort_entries(rows: &mut [Entry]) { rows.sort_by_key(|row| FIRST.created_at); }",
        ] {
            assert!(witnesses(&with_entry(function)).is_empty(), "{function}");
        }
    }

    #[test]
    fn nothing_is_proposed_for_a_text_the_file_was_not_parsed_from() {
        let source =
            "fn expiry(created: u64, updated: u64, expires: u64) -> bool { created < expires }";
        let file = syn::parse_file(source).unwrap();
        for other in [format!(" {source}"), String::new()] {
            let mut out = Vec::new();
            discover(&other, &file, &mut out);
            assert!(out.is_empty(), "{other}");
        }
    }
}
