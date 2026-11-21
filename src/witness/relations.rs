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
        let start = out.len();
        match unparen(expression) {
            Expr::Binary(comparison) if semicolon.is_none() => {
                if let Some(parameters) = parameters(function, &records) {
                    compare_candidates(source, function, comparison, &parameters, out);
                }
            }
            Expr::MethodCall(call) => sort_candidates(source, function, call, &records, out),
            _ => {}
        }
        out.truncate(start.saturating_add(MAX_CANDIDATES_PER_FUNCTION));
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
        BinOp::Eq(_) => lhs == rhs,
        BinOp::Ne(_) => lhs != rhs,
        BinOp::Lt(_) => lhs < rhs,
        BinOp::Le(_) => lhs <= rhs,
        BinOp::Gt(_) => lhs > rhs,
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
        for &second in &values[index + 1..] {
            if evaluate(first)? != evaluate(second)? {
                return Some(
                    if matches!(op, BinOp::Eq(_) | BinOp::Ne(_)) && second == pivot {
                        (second, first)
                    } else {
                        (first, second)
                    },
                );
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
            Operand::Literal(value) if integer.contains(value) => value,
            Operand::Place(value) if value != chosen && integer_type(value, params) == integer => 1,
            _ => continue,
        };
        let Some((original, alternative)) = across(pivot, integer, &expression.op, change_lhs)
        else {
            continue;
        };
        for new in alternatives(chosen, params) {
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
}
