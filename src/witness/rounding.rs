//! Finds `a * b / c` on integer parameters: dividing first rounds earlier, which only
//! inputs that are not multiples of `c` can tell.

use std::collections::BTreeMap;

use syn::parse::Parser;
use syn::spanned::Spanned;
use syn::visit::Visit;
use syn::{BinOp, Expr, FnArg, Item, ItemFn, Pat, Type};

use super::{Candidate, Edit, Scenario};

/// The largest value of an integer type that a literal of it can be on every target.
fn largest(ty: &Type) -> Option<u128> {
    let Type::Path(path) = ty else { return None };
    let bits = match path.path.get_ident()?.to_string().as_str() {
        "u8" => 8,
        "u16" | "usize" => 16,
        "u32" => 32,
        "u64" => 64,
        "u128" => 128,
        "i8" => 7,
        "i16" | "isize" => 15,
        "i32" => 31,
        "i64" => 63,
        "i128" => 127,
        _ => return None,
    };
    Some(if bits == 128 {
        u128::MAX
    } else {
        (1 << bits) - 1
    })
}

/// The parameter an operand is: its name alone, in parentheses, cast (`x as u64`) or
/// converted (`u64::from(x)`).
fn parameter(expr: &Expr, names: &[&syn::Ident]) -> Option<usize> {
    match expr {
        Expr::Path(_) => {
            let name = super::path_ident(expr)?;
            names.iter().position(|known| *known == name)
        }
        Expr::Paren(inner) => parameter(&inner.expr, names),
        Expr::Group(inner) => parameter(&inner.expr, names),
        Expr::Cast(cast) => parameter(&cast.expr, names),
        Expr::Call(call) if call.args.len() == 1 => {
            let Expr::Path(function) = call.func.as_ref() else {
                return None;
            };
            let segments = &function.path.segments;
            (segments.len() == 2 && segments[1].ident == "from")
                .then(|| parameter(&call.args[0], names))
                .flatten()
        }
        _ => None,
    }
}

/// An integer literal as a number, whatever its suffix.
fn literal(expr: &Expr) -> Option<u128> {
    match expr {
        Expr::Lit(literal) => match &literal.lit {
            syn::Lit::Int(integer) => integer.base10_parse().ok(),
            _ => None,
        },
        Expr::Paren(inner) => self::literal(&inner.expr),
        Expr::Group(inner) => self::literal(&inner.expr),
        _ => None,
    }
}

/// `a * b / c` in the body of a function: the division, the two factors as parameters,
/// and the divisor.
struct Scaled<'a> {
    division: &'a syn::ExprBinary,
    product: &'a syn::ExprBinary,
    first: usize,
    second: usize,
    divisor: u128,
}

struct Finder<'a, 'n> {
    names: &'n [&'a syn::Ident],
    found: Vec<Scaled<'a>>,
}

impl<'a> Visit<'a> for Finder<'a, '_> {
    fn visit_expr_binary(&mut self, node: &'a syn::ExprBinary) {
        let mut left = node.left.as_ref();
        while let Expr::Paren(inner) = left {
            left = &inner.expr;
        }
        if let (BinOp::Div(_), Expr::Binary(product)) = (&node.op, left)
            && matches!(product.op, BinOp::Mul(_))
            && let Some(divisor) = literal(&node.right).filter(|divisor| *divisor >= 2)
            && let Some(first) = parameter(&product.left, self.names)
            && let Some(second) = parameter(&product.right, self.names)
            && first != second
        {
            self.found.push(Scaled {
                division: node,
                product,
                first,
                second,
                divisor,
            });
        }
        syn::visit::visit_expr_binary(self, node);
    }
    // A closure or an inner function has names of its own.
    fn visit_expr_closure(&mut self, _: &'a syn::ExprClosure) {}
    fn visit_item(&mut self, _: &'a Item) {}
}

/// The calls of functions with nothing but integer literals, in the tests of the file.
#[derive(Default)]
struct Calls {
    in_test: bool,
    of: BTreeMap<String, Vec<Vec<u128>>>,
}

impl<'ast> Visit<'ast> for Calls {
    fn visit_item_fn(&mut self, function: &'ast ItemFn) {
        let outer = self.in_test;
        self.in_test |= function
            .attrs
            .iter()
            .any(|attr| attr.path().is_ident("test"));
        if self.in_test {
            self.visit_block(&function.block);
        }
        self.in_test = outer;
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let Expr::Path(function) = call.func.as_ref()
            && let Some(name) = function.path.segments.last()
            && let Some(args) = call.args.iter().map(literal).collect::<Option<Vec<_>>>()
        {
            self.of
                .entry(name.ident.to_string())
                .or_default()
                .push(args);
        }
        syn::visit::visit_expr_call(self, call);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let asserts = ["assert", "assert_eq", "assert_ne", "debug_assert_eq"];
        if asserts.iter().any(|name| mac.path.is_ident(name))
            && let Ok(args) = syn::punctuated::Punctuated::<Expr, syn::Token![,]>::parse_terminated
                .parse2(mac.tokens.clone())
        {
            for arg in &args {
                self.visit_expr(arg);
            }
        }
    }
}

/// The remainder that makes dividing first give less, and the value of the first factor
/// that has it: `(a0 / c) * c + ceil(c / b0)`. With `r = a mod c`, multiplying first
/// gives `floor(r * b / c)` more than dividing first, and this is the least `r` for which
/// that is 1 or more. `None` when `b0` is too small for any remainder to show.
fn telling_input(a0: u128, b0: u128, divisor: u128) -> Option<u128> {
    if b0 < 2 {
        return None;
    }
    let remainder = divisor.div_ceil(b0);
    (remainder < divisor)
        .then(|| (a0 / divisor).checked_mul(divisor)?.checked_add(remainder))
        .flatten()
}

/// An operand as it can stand next to another operator.
fn operand(source: &str, expr: &Expr) -> Option<String> {
    let text = source.get(expr.span().byte_range())?;
    Some(match expr {
        Expr::Path(_) | Expr::Paren(_) | Expr::Call(_) | Expr::Lit(_) => text.to_string(),
        _ => format!("({text})"),
    })
}

pub(super) fn discover(source: &str, file: &syn::File, out: &mut Vec<Candidate>) {
    let mut calls = None;
    for item in &file.items {
        let Item::Fn(function) = item else { continue };
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
            continue;
        }
        // Every parameter is an integer under a plain name: then a call with literals
        // can be written.
        let Some(params) = signature
            .inputs
            .iter()
            .map(|input| {
                let FnArg::Typed(typed) = input else {
                    return None;
                };
                let Pat::Ident(name) = typed.pat.as_ref() else {
                    return None;
                };
                Some((&name.ident, largest(&typed.ty)?))
            })
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };
        let names: Vec<&syn::Ident> = params.iter().map(|(name, _)| *name).collect();
        let mut finder = Finder {
            names: &names,
            found: vec![],
        };
        finder.visit_block(&function.block);
        for scaled in finder.found {
            let span = scaled.division.span().byte_range();
            let (Some(first), Some(second), Some(divisor)) = (
                operand(source, &scaled.product.left),
                operand(source, &scaled.product.right),
                source.get(scaled.division.right.span().byte_range()),
            ) else {
                continue;
            };
            if source.get(span.clone()).is_none() {
                continue;
            }
            let name = signature.ident.to_string();
            let seeds = calls
                .get_or_insert_with(|| {
                    let mut calls = Calls::default();
                    calls.visit_file(file);
                    calls
                })
                .of
                .get(&name)
                .map(Vec::as_slice)
                .unwrap_or_default()
                .iter()
                .filter(|args| args.len() == params.len())
                .collect::<Vec<_>>();
            let factor = names[scaled.first];
            // A call of the tests with one argument changed: first one whose value is a
            // multiple, where the change is the smallest that shows.
            let mut ordered = seeds.clone();
            ordered.sort_by_key(|args| args[scaled.first] % scaled.divisor != 0);
            let scenario = ordered.into_iter().find_map(|args| {
                let value =
                    telling_input(args[scaled.first], args[scaled.second], scaled.divisor)?;
                if value > params[scaled.first].1 {
                    return None;
                }
                let mut args = args.clone();
                args[scaled.first] = value;
                let args: Vec<String> = args.iter().map(u128::to_string).collect();
                Some(Scenario {
                    expression: format!("{name}({})", args.join(", ")),
                    guidance: Some(format!(
                        "The tests pass with the division by {} done first; this `{factor}` is not a multiple of it.",
                        scaled.divisor
                    )),
                    ..Scenario::default()
                })
            });
            out.push(Candidate {
                edits: vec![Edit {
                    span,
                    replacement: format!("(({first} / {divisor}) * {second})"),
                }],
                scope: scaled.division.span().start().line..=scaled.division.span().end().line,
                signature_lines: vec![signature.span().start().line],
                description: format!(
                    "`{first} / {divisor} * {second}` (divide before multiplying)"
                ),
                existing: false,
                advice: None,
                file: Default::default(),
                finding_at: None,
                scenarios: scenario.into_iter().collect(),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates(source: &str) -> Vec<Candidate> {
        let mut candidates = Vec::new();
        discover(source, &syn::parse_file(source).unwrap(), &mut candidates);
        candidates
    }

    const TESTS: &str = "
        #[cfg(test)]
        mod tests {
            use super::*;
            #[test]
            fn round_amounts() {
                assert_eq!(fee(10_000, 125), 125);
                assert_eq!(fee(20_000u64, 250), 500);
            }
        }";

    #[test]
    fn dividing_first_is_tried_with_an_input_that_tells() {
        let source =
            format!("pub fn fee(cents: u64, bps: u64) -> u64 {{ cents * bps / 10_000 }}{TESTS}");
        let all = candidates(&source);
        assert_eq!(all.len(), 1);
        let candidate = &all[0];
        assert_eq!(
            &source[candidate.edits[0].span.clone()],
            "cents * bps / 10_000"
        );
        assert_eq!(candidate.edits[0].replacement, "((cents / 10_000) * bps)");
        assert_eq!(
            candidate.description,
            "`cents / 10_000 * bps` (divide before multiplying)"
        );
        assert_eq!(candidate.scope, 1..=1);
        assert_eq!(candidate.signature_lines, [1]);
        let scenario = &candidate.scenarios[0];
        // 80 is the least remainder with 80 * 125 >= 10000.
        assert_eq!(scenario.expression, "fee(10080, 125)");
        assert_eq!(
            scenario.guidance.as_deref(),
            Some(
                "The tests pass with the division by 10000 done first; this `cents` is not a multiple of it."
            )
        );
    }

    #[test]
    fn the_shapes_that_are_a_product_divided_by_a_literal() {
        for (body, replacement) in [
            ("(cents * bps) / 100", "((cents / 100) * bps)"),
            (
                "(cents as u64) * (bps as u64) / 100",
                "(((cents as u64) / 100) * (bps as u64))",
            ),
            (
                "cents as u64 * bps as u64 / 100",
                "(((cents as u64) / 100) * (bps as u64))",
            ),
            (
                "u64::from(cents) * u64::from(bps) / 100u64",
                "((u64::from(cents) / 100u64) * u64::from(bps))",
            ),
            ("bps * cents / (100)", "((bps / (100)) * cents)"),
            ("1 + cents * bps / 100 - 2", "((cents / 100) * bps)"),
        ] {
            let source = format!("fn fee(cents: u32, bps: u32) -> u64 {{ ({body}) as u64 }}");
            let all = candidates(&source);
            assert_eq!(all.len(), 1, "{body}");
            assert_eq!(all[0].edits[0].replacement, replacement, "{body}");
            // No call in the tests: tried all the same, with nothing to suggest.
            assert!(all[0].scenarios.is_empty(), "{body}");
        }
        for body in [
            "cents * bps",
            "cents / 100 * bps",
            "cents * bps / 1",
            "cents * bps / bps",
            "cents * 3 / 100",
            "cents * cents / 100",
            "cents + bps / 100",
            "cents * bps % 100",
            "(|bps: u32| cents * bps / 100)(bps)",
            "{ fn inner(cents: u32, bps: u32) -> u32 { cents * bps / 100 } inner(cents, bps) }",
            "cents * other() / 100",
        ] {
            let source = format!("fn fee(cents: u32, bps: u32) -> u32 {{ {body} }}");
            assert!(candidates(&source).is_empty(), "{body}");
        }
        for signature in [
            "fn fee<T>(cents: u32, bps: u32) -> u32",
            "async fn fee(cents: u32, bps: u32) -> u32",
            "unsafe fn fee(cents: u32, bps: u32) -> u32",
            "const fn fee(cents: u32, bps: u32) -> u32",
            "extern \"C\" fn fee(cents: u32, bps: u32) -> u32",
            "#[cfg(unix)] fn fee(cents: u32, bps: u32) -> u32",
            "fn fee(cents: f64, bps: u32) -> u32",
            "fn fee((cents, bps): (u32, u32)) -> u32",
            "fn fee(cents: u32, bps: u32, name: &str) -> u32",
        ] {
            let source = format!("{signature} {{ cents * bps / 100 }}");
            assert!(candidates(&source).is_empty(), "{signature}");
        }
        // A method takes `self`: not a call that can be written with literals.
        let method =
            "struct S; impl S { fn fee(&self, cents: u32, bps: u32) -> u32 { cents * bps / 100 } }";
        assert!(candidates(method).is_empty());
    }

    #[test]
    fn the_call_to_change_is_one_of_the_tests() {
        let with = |tests: &str| {
            let source = format!(
                "fn fee(cents: u8, bps: u8, flat: u8) -> u8 {{ cents * bps / 50 + flat }}\n\
                 #[test] fn t() {{ {tests} }}"
            );
            let mut all = candidates(&source);
            assert_eq!(all.len(), 1, "{tests}");
            all.remove(0)
                .scenarios
                .pop()
                .map(|scenario| (scenario.expression, scenario.guidance.unwrap()))
        };
        let multiples = "The tests pass with the division by 50 done first; this `cents` is not a multiple of it.";
        let none_shows = multiples;
        // The other arguments stay as the test has them.
        assert_eq!(
            with("assert_eq!(fee(100, 5, 7), 17);"),
            Some(("fee(110, 5, 7)".to_string(), multiples.to_string()))
        );
        // A factor of 1 or 0 shows nothing, a call that is no literal cannot be changed,
        // and neither can one of another shape.
        assert_eq!(with("assert_eq!(fee(100, 1, 0), 2); fee(50, 0, 0);"), None);
        assert_eq!(with("let c = 100; assert_eq!(fee(c, 5, 7), 17);"), None);
        assert_eq!(with("assert_eq!(other(100, 5, 7), 17); fee(1, 2);"), None);
        assert_eq!(with(""), None);
        // A value the type cannot hold is not suggested: 250 + 2 is fine, 250 + 25 is not.
        assert_eq!(
            with("fee(250, 25, 0);"),
            Some(("fee(252, 25, 0)".to_string(), multiples.to_string()))
        );
        assert_eq!(with("fee(250, 2, 0);"), None);
        // A call whose value is a multiple is preferred; with none, any call serves.
        assert_eq!(
            with("fee(51, 2, 0); fee(100, 10, 0);"),
            Some(("fee(105, 10, 0)".to_string(), none_shows.to_string()))
        );
        assert_eq!(
            with("fee(51, 2, 0);"),
            Some(("fee(75, 2, 0)".to_string(), none_shows.to_string()))
        );
        // Calls outside the tests are not the tests' inputs.
        let source = "fn fee(cents: u8, bps: u8) -> u8 { cents * bps / 50 }\n\
                      fn user() -> u8 { fee(100, 5) }";
        assert!(candidates(source)[0].scenarios.is_empty());
    }

    #[test]
    fn the_input_is_the_least_that_tells_the_two_apart() {
        // Over every small case: multiplying first gives floor((a mod c) * b / c) more,
        // and the suggested value is the least above the multiple where that is not 0.
        for divisor in 2..=24u128 {
            for a in 0..=60u128 {
                for b in 0..=30u128 {
                    let (first, second) = (a * b / divisor, (a / divisor) * b);
                    assert_eq!(first - second, (a % divisor) * b / divisor);
                    let base = (a / divisor) * divisor;
                    match telling_input(a, b, divisor) {
                        Some(value) => {
                            assert!(base < value && value < base + divisor);
                            assert_ne!(value * b / divisor, (value / divisor) * b);
                            for less in base..value {
                                assert_eq!(less * b / divisor, (less / divisor) * b);
                            }
                        }
                        None => {
                            for other in base..base + divisor {
                                assert_eq!(other * b / divisor, (other / divisor) * b);
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(telling_input(10_000, 125, 10_000), Some(10_080));
        // A value past the largest there is, is none.
        assert_eq!(telling_input(u128::MAX, 2, 4), Some(u128::MAX - 1));
        assert_eq!(telling_input(u128::MAX, 2, u128::MAX), None);
    }

    #[test]
    fn the_largest_literal_of_each_type() {
        let of = |name: &str| largest(&syn::parse_str::<Type>(name).unwrap());
        assert_eq!(of("u8"), Some(255));
        assert_eq!(of("i8"), Some(127));
        assert_eq!(of("usize"), Some(65_535));
        assert_eq!(of("isize"), Some(32_767));
        assert_eq!(of("u64"), Some(u128::from(u64::MAX)));
        assert_eq!(of("i64"), Some(i64::MAX as u128));
        assert_eq!(of("u128"), Some(u128::MAX));
        assert_eq!(of("i128"), Some(i128::MAX as u128));
        assert_eq!(of("f64"), None);
        assert_eq!(of("&u8"), None);
    }
}
