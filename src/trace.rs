//! Reads the trace written by the injected runtime (`src/runtime/tmt_rt.rs`).
//!
//! Lines are tab separated:
//! - `O <site> <test> <rels> <unvalued> <max_neg|-> <min_pos|-> <unfloat> <f_max_neg|-> <f_min_pos|->`:
//!   cumulative summary of a comparison
//! - `R <site> <test> <bits>`: a statement site was executed
//! - `L <site> <test> <bits>`: observations at `&&` / `||` (cumulative)
//! - `A <site> <test> <bits>`: observations at an arithmetic operator (cumulative)
//! - `B <site> <test> <bits>`: values a `-> bool` function returned (cumulative)
//! - `T <site> <lhs_min> <lhs_max> <rhs_min> <rhs_max>`: integer type bounds of the operands
//!
//! A process killed mid-write can leave a truncated last line; malformed lines are skipped
//! and counted rather than trusted.

use std::collections::{BTreeMap, BTreeSet};

use crate::plan::{ArithObs, LogicObs, Obs, ReturnObs};

/// Test name used when the observing thread is not a libtest test thread.
pub const UNKNOWN_TEST: &str = "?";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TypeBounds {
    pub lhs: (i128, i128),
    pub rhs: (i128, i128),
}

#[derive(Default, Debug, PartialEq)]
pub struct Trace {
    /// site -> test -> observations
    pub cmp: BTreeMap<u32, BTreeMap<String, Obs>>,
    pub logic: BTreeMap<u32, BTreeMap<String, LogicObs>>,
    pub arith: BTreeMap<u32, BTreeMap<String, ArithObs>>,
    pub returns: BTreeMap<u32, BTreeMap<String, ReturnObs>>,
    /// site -> tests that executed the statement
    pub reach: BTreeMap<u32, BTreeSet<String>>,
    pub bounds: BTreeMap<u32, TypeBounds>,
    pub malformed_lines: usize,
}

fn flag(text: &str) -> Option<bool> {
    match text {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

fn optional<T: std::str::FromStr>(text: &str) -> Option<Option<T>> {
    if text == "-" {
        Some(None)
    } else {
        text.parse().ok().map(Some)
    }
}

impl Trace {
    pub fn parse(text: &str) -> Trace {
        let mut trace = Trace::default();
        for line in text.lines() {
            if trace.parse_line(line).is_none() {
                trace.malformed_lines += 1;
            }
        }
        trace
    }

    fn parse_line(&mut self, line: &str) -> Option<()> {
        let cols: Vec<&str> = line.split('\t').collect();
        match cols[..] {
            [
                "O",
                site,
                test,
                rels,
                unvalued,
                max_neg,
                min_pos,
                unfloat,
                f_max_neg,
                f_min_pos,
            ] => {
                let rels: u8 = rels.parse().ok().filter(|r| *r < 16)?;
                let obs = Obs {
                    lt: rels & 1 != 0,
                    eq: rels & 2 != 0,
                    gt: rels & 4 != 0,
                    un: rels & 8 != 0,
                    unvalued: flag(unvalued)?,
                    max_neg: optional::<i128>(max_neg)?.filter(|d| *d < 0),
                    min_pos: optional::<i128>(min_pos)?.filter(|d| *d > 0),
                    unfloat: flag(unfloat)?,
                    f_max_neg: optional::<f64>(f_max_neg)?.filter(|d| d.is_finite() && *d < 0.0),
                    f_min_pos: optional::<f64>(f_min_pos)?.filter(|d| d.is_finite() && *d > 0.0),
                };
                let site = site.parse().ok()?;
                (!test.is_empty()).then_some(())?;
                self.cmp
                    .entry(site)
                    .or_default()
                    .entry(test.to_string())
                    .or_default()
                    .merge(&obs);
            }
            [kind @ ("R" | "L" | "A" | "B"), site, test, bits] => {
                let site: u32 = site.parse().ok()?;
                let limit = match kind {
                    "B" => 4,
                    "A" => 32,
                    _ => 8,
                };
                let bits: u8 = bits.parse().ok().filter(|b| (1..limit).contains(b))?;
                (!test.is_empty()).then_some(())?;
                let test = test.to_string();
                match kind {
                    "R" => {
                        self.reach.entry(site).or_default().insert(test);
                    }
                    "B" => self
                        .returns
                        .entry(site)
                        .or_default()
                        .entry(test)
                        .or_default()
                        .merge(&ReturnObs::from_bits(bits)),
                    "L" => self
                        .logic
                        .entry(site)
                        .or_default()
                        .entry(test)
                        .or_default()
                        .merge(&LogicObs::from_bits(bits)),
                    _ => self
                        .arith
                        .entry(site)
                        .or_default()
                        .entry(test)
                        .or_default()
                        .merge(&ArithObs::from_bits(bits)),
                }
            }
            ["T", site, lhs_min, lhs_max, rhs_min, rhs_max] => {
                let bounds = TypeBounds {
                    lhs: (lhs_min.parse().ok()?, lhs_max.parse().ok()?),
                    rhs: (rhs_min.parse().ok()?, rhs_max.parse().ok()?),
                };
                self.bounds.insert(site.parse().ok()?, bounds);
            }
            _ => return None,
        }
        Some(())
    }

    /// Everything any test observed at a comparison site.
    pub fn merged(&self, site: u32) -> Obs {
        let mut all = Obs::default();
        for obs in self
            .cmp
            .get(&site)
            .into_iter()
            .flat_map(|tests| tests.values())
        {
            all.merge(obs);
        }
        all
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{CmpOp, Mutation, Rel, eval_mutated_d};
    use crate::tmt_rt;

    #[test]
    fn parses_and_merges_lines() {
        let trace = Trace::parse(
            "O\t3\ttests::a\t1\t0\t-4\t-\t1\t-\t-\n\
             O\t3\ttests::a\t5\t0\t-1\t2\t1\t-\t-\n\
             O\t3\ttests::b\t2\t0\t-\t-\t1\t-\t-\n\
             R\t9\ttests::a\t1\n\
             R\t9\t?\t1\n\
             T\t3\t0\t255\t0\t255\n",
        );
        assert_eq!(trace.malformed_lines, 0);
        let a = trace.cmp[&3]["tests::a"];
        assert_eq!(
            (a.lt, a.eq, a.gt, a.max_neg, a.min_pos),
            (true, false, true, Some(-1), Some(2))
        );
        assert!(trace.merged(3).eq);
        assert_eq!(
            trace.reach[&9].iter().collect::<Vec<_>>(),
            ["?", "tests::a"]
        );
        assert_eq!(
            trace.bounds[&3],
            TypeBounds {
                lhs: (0, 255),
                rhs: (0, 255)
            }
        );
        assert_eq!(trace.merged(42), Obs::default());
    }

    #[test]
    fn skips_truncated_and_malformed_lines() {
        let trace = Trace::parse(
            "O\t3\ttests::a\t1\t0\t-4\nX\t1\nO\tx\tt\t1\t0\t-\t-\t1\t-\t-\nO\t1\tt\t99\t0\t-\t-\t1\t-\t-\nR\t1\t\t1\nR\t2\tok\t1\n",
        );
        assert_eq!(trace.malformed_lines, 5);
        assert!(trace.cmp.is_empty());
        assert_eq!(trace.reach.len(), 1);
    }

    #[test]
    fn relation_bits_and_field_validation() {
        for (rels, expected) in [(1, "lt"), (2, "eq"), (4, "gt"), (8, "un")] {
            let trace = Trace::parse(&format!("O\t1\tt\t{rels}\t1\t-\t-\t1\t-\t-\n"));
            let o = trace.cmp[&1]["t"];
            let set: Vec<&str> = [(o.lt, "lt"), (o.eq, "eq"), (o.gt, "gt"), (o.un, "un")]
                .into_iter()
                .filter_map(|(on, name)| on.then_some(name))
                .collect();
            assert_eq!(set, [expected]);
            assert!(o.unvalued);
        }
        // 15 is every relation, 16 is not a relation mask.
        assert_eq!(
            Trace::parse("O\t1\tt\t15\t0\t-\t-\t1\t-\t-\n").malformed_lines,
            0
        );
        assert_eq!(
            Trace::parse("O\t1\tt\t16\t0\t-\t-\t1\t-\t-\n").malformed_lines,
            1
        );
        // A comparison line needs a test name too.
        let nameless = Trace::parse("O\t1\t\t1\t0\t-\t-\t1\t-\t-\n");
        assert_eq!((nameless.malformed_lines, nameless.cmp.len()), (1, 0));
        // The nearest-below value is negative and the nearest-above positive, or they are dropped.
        let signs = Trace::parse("O\t1\tt\t5\t0\t0\t0\t1\t-\t-\nO\t2\tt\t5\t0\t3\t-3\t1\t-\t-\n");
        for site in [1, 2] {
            let o = signs.cmp[&site]["t"];
            assert_eq!((o.max_neg, o.min_pos), (None, None));
        }
    }

    fn rt_op(op: CmpOp) -> u8 {
        op.code()
    }

    fn rt_mutant(m: Mutation) -> tmt_rt::Mutant {
        let spec = format!("7:{}", m.encode());
        let (site, parsed) =
            tmt_rt::parse_mutant(&spec).unwrap_or_else(|| panic!("runtime rejects {spec}"));
        assert_eq!(site, 7);
        parsed
    }

    /// The injected runtime is a separate source file; it must agree with the planner on
    /// what every mutation does and on how observations are summarised.
    #[test]
    fn runtime_agrees_with_the_planner() {
        for op in CmpOp::ALL {
            let mut ms = vec![
                Mutation::Const(true),
                Mutation::Const(false),
                Mutation::Negate,
            ];
            ms.extend(CmpOp::ALL.into_iter().map(Mutation::Op));
            if op.is_ordering() {
                ms.extend((-4..=4).filter(|s| *s != 0).map(Mutation::Shift));
            }
            for m in ms {
                for d in -6..=6i128 {
                    let rel = if d < 0 {
                        1
                    } else if d == 0 {
                        2
                    } else {
                        4
                    };
                    assert_eq!(
                        tmt_rt::apply(
                            rt_op(op),
                            rt_mutant(m),
                            op.eval_d(d),
                            rel,
                            tmt_rt::Diff::Int(d)
                        ),
                        eval_mutated_d(op, m, d),
                        "{op:?} {m:?} d={d}"
                    );
                }
            }
            // Unordered operands (NaN): only `!=` is true.
            assert_eq!(
                tmt_rt::apply(
                    rt_op(op),
                    tmt_rt::Mutant::Op(rt_op(op)),
                    false,
                    8,
                    tmt_rt::Diff::None
                ),
                op.eval_rel(Rel::Unordered)
            );
            // A shift needs values of its own kind; without them the original result stands.
            for orig in [true, false] {
                assert_eq!(
                    tmt_rt::apply(
                        rt_op(op),
                        tmt_rt::Mutant::Shift(1),
                        orig,
                        2,
                        tmt_rt::Diff::None
                    ),
                    orig
                );
                assert_eq!(
                    tmt_rt::apply(
                        rt_op(op),
                        tmt_rt::Mutant::ShiftF(1.0),
                        orig,
                        2,
                        tmt_rt::Diff::Int(0)
                    ),
                    orig
                );
                assert_eq!(
                    tmt_rt::apply(
                        rt_op(op),
                        tmt_rt::Mutant::Delete,
                        orig,
                        2,
                        tmt_rt::Diff::Int(0)
                    ),
                    orig
                );
            }
        }
        assert_eq!(
            tmt_rt::parse_mutant("12:del"),
            Some((12, tmt_rt::Mutant::Delete))
        );
        for (m, expected) in [
            (Mutation::LogicSwap, tmt_rt::Mutant::LogicSwap),
            (Mutation::LogicDropRhs, tmt_rt::Mutant::LogicDropRhs),
            (Mutation::LogicDropLhs, tmt_rt::Mutant::LogicDropLhs),
            (Mutation::ArithSwap, tmt_rt::Mutant::ArithSwap),
            (Mutation::ArithSwap2, tmt_rt::Mutant::ArithSwap2),
            (Mutation::ShiftF(-10.25), tmt_rt::Mutant::ShiftF(-10.25)),
            (Mutation::ShiftF(1e-7), tmt_rt::Mutant::ShiftF(1e-7)),
        ] {
            assert_eq!(rt_mutant(m), expected);
        }
        for bad in [
            "", "x:neg", "1:", "1:op9", "1:shx", "1:zzz", "neg", "1:shfnan", "1:shfinf", "1:shf",
        ] {
            assert_eq!(tmt_rt::parse_mutant(bad), None, "{bad:?}");
        }
    }

    /// Float shifts: the runtime's result on the observed differences never changes for a
    /// shift the planner calls safe.
    #[test]
    fn runtime_float_shifts_agree_with_the_planner() {
        let observed = [-20.5f64, -3.0, 9.5, 12.0];
        let mut obs = Obs::default();
        for d in observed {
            obs.merge(&Obs::of_f(d));
        }
        for op in [CmpOp::Lt, CmpOp::Le, CmpOp::Gt, CmpOp::Ge] {
            for shift in [-20.5, -10.0, -3.0, -1.5, 4.75, 9.5, 11.0] {
                let changes_something = observed.iter().any(|d| {
                    let rel = if *d < 0.0 { 1 } else { 4 };
                    let orig = op.eval_d(if *d < 0.0 { -1 } else { 1 });
                    tmt_rt::apply(
                        rt_op(op),
                        tmt_rt::Mutant::ShiftF(shift),
                        orig,
                        rel,
                        tmt_rt::Diff::Float(*d),
                    ) != orig
                });
                let predicted = crate::plan::infects(op, Mutation::ShiftF(shift), &obs);
                // Safe by the planner implies unchanged at run time (the converse need not hold
                // exactly at the edges of the gap).
                assert!(predicted || !changes_something, "{op:?} shift={shift}");
            }
        }
        // Equal floats are reported as a zero difference even where subtraction is not finite.
        use tmt_rt::{Diff, Val};
        assert_eq!(
            tmt_rt::diff(Val::Float(f64::INFINITY), Val::Float(f64::INFINITY), true),
            Diff::Float(0.0)
        );
        assert_eq!(
            tmt_rt::diff(Val::Float(f64::INFINITY), Val::Float(1.0), false),
            Diff::None
        );
        assert_eq!(
            tmt_rt::diff(Val::Float(f64::NAN), Val::Float(1.0), false),
            Diff::None
        );
        assert_eq!(
            tmt_rt::diff(Val::Float(1.5), Val::Float(1.0), false),
            Diff::Float(0.5)
        );
        assert_eq!(
            tmt_rt::diff(Val::Int(3, 0, 9), Val::Int(5, 0, 9), false),
            Diff::Int(-2)
        );
        assert_eq!(
            tmt_rt::diff(Val::Int(3, 0, 9), Val::Float(5.0), false),
            Diff::None
        );
        assert_eq!(tmt_rt::diff(Val::None, Val::None, true), Diff::None);
    }

    /// `&&` / `||` in the runtime against the plain operators and the planner's bits.
    #[test]
    fn runtime_logic_matches_the_operators() {
        let run = |and: bool, m: Option<tmt_rt::Mutant>, a: bool, b: bool| -> (bool, bool) {
            match tmt_rt::lhs_code(and, a, m) {
                2 => (tmt_rt::rhs_value(and, b, m), true),
                code => (code == 1, false),
            }
        };
        for and in [true, false] {
            for a in [true, false] {
                for b in [true, false] {
                    let real = if and { a && b } else { a || b };
                    let swapped = if and { a || b } else { a && b };
                    assert_eq!(run(and, None, a, b).0, real);
                    assert_eq!(run(and, Some(tmt_rt::Mutant::LogicSwap), a, b).0, swapped);
                    assert_eq!(
                        run(and, Some(tmt_rt::Mutant::LogicDropLhs), a, b),
                        (b, true)
                    );
                    // The right operand is evaluated exactly when the original evaluates it.
                    assert_eq!(
                        run(and, Some(tmt_rt::Mutant::LogicDropRhs), a, b),
                        (a, run(and, None, a, b).1)
                    );
                    // Short-circuiting: the right operand is skipped exactly as in Rust.
                    assert_eq!(run(and, None, a, b).1, if and { a } else { !a });

                    // What the planner predicts from the observation bits is what changes.
                    let (_, evaluated) = run(and, None, a, b);
                    let bits = if !evaluated {
                        tmt_rt::LOGIC_SHORT
                    } else if b {
                        tmt_rt::LOGIC_RHS_TRUE
                    } else {
                        tmt_rt::LOGIC_RHS_FALSE
                    };
                    let obs = crate::plan::LogicObs::from_bits(bits);
                    for (m, rt) in [
                        (Mutation::LogicDropRhs, tmt_rt::Mutant::LogicDropRhs),
                        (Mutation::LogicDropLhs, tmt_rt::Mutant::LogicDropLhs),
                    ] {
                        let changed = run(and, Some(rt), a, b) != run(and, None, a, b);
                        assert_eq!(
                            crate::plan::logic_infects(and, m, &obs),
                            changed,
                            "and={and} a={a} b={b} {m:?}"
                        );
                    }
                }
            }
        }
        // Without a mutant or a trace the probes are the plain operators.
        assert_eq!(tmt_rt::lhs(0, true, false), 0);
        assert_eq!(tmt_rt::lhs(0, false, true), 1);
        assert_eq!(tmt_rt::lhs(0, true, true), 2);
        assert!(tmt_rt::rhs(0, true, true));
        assert!(!tmt_rt::rhs(0, false, false));
    }

    #[test]
    #[allow(clippy::needless_borrow)] // the explicit `&` is the call form the probes use
    fn runtime_arithmetic_probes() {
        use tmt_rt::{AnyAdd as _, AnyMul as _, AnySub as _, ArithNum, ArithPrim as _};
        // Primitive numbers: the real operator.
        assert_eq!((&tmt_rt::Ar::new(7u32, 2u32)).tmt_add(0), 9);
        assert_eq!((&tmt_rt::Ar::new(7i64, 2)).tmt_sub(0), 5);
        assert_eq!((&tmt_rt::Ar::new(7usize, 2)).tmt_mul(0), 14);
        assert_eq!((&tmt_rt::Ar::new(7u8, 2)).tmt_div(0), 3);
        assert_eq!((&tmt_rt::Ar::new(7i16, 2)).tmt_rem(0), 1);
        assert_eq!((&tmt_rt::Ar::new(1.5f64, 2.0)).tmt_mul(0), 3.0);
        // Everything else falls back to the operator's own implementation.
        assert_eq!((&tmt_rt::Ar::new(String::from("a"), "b")).tmt_add(0), "ab");
        let second = std::time::Duration::from_secs(1);
        assert_eq!((&tmt_rt::Ar::new(second, second)).tmt_add(0), second * 2);
        assert_eq!((&tmt_rt::Ar::new(second, 3u32)).tmt_mul(0), second * 3);
        assert_eq!((&tmt_rt::Ar::new(&5i32, &3i32)).tmt_sub(0), 2);

        // The swapped operator of each operator, and when it agrees.
        for (op, swapped) in [
            (tmt_rt::ADD, tmt_rt::SUB),
            (tmt_rt::SUB, tmt_rt::ADD),
            (tmt_rt::MUL, tmt_rt::DIV),
            (tmt_rt::DIV, tmt_rt::MUL),
            (tmt_rt::REM, tmt_rt::DIV),
        ] {
            assert_eq!(tmt_rt::arith_swapped(op), swapped);
            for a in -6..=6i32 {
                for b in -6..=6i32 {
                    let real = |op: u8| match op {
                        tmt_rt::ADD => a.checked_add(b),
                        tmt_rt::SUB => a.checked_sub(b),
                        tmt_rt::MUL => a.checked_mul(b),
                        tmt_rt::DIV => a.checked_div(b),
                        _ => a.checked_rem(b),
                    };
                    // Equal only when both are defined: a fault on either side has to be run.
                    let same = matches!((real(op), real(swapped)), (Some(x), Some(y)) if x == y);
                    assert_eq!(
                        i32::tmt_same(op, tmt_rt::arith_swapped(op), a, b),
                        same,
                        "op={op} {a} {b}"
                    );
                    if let Some(value) = real(op) {
                        assert_eq!(i32::tmt_real(op, a, b), value);
                    }
                }
            }
        }
        assert!(u8::tmt_same(
            tmt_rt::ADD,
            tmt_rt::arith_swapped(tmt_rt::ADD),
            9,
            0
        ));
        // Both shifts overflow: not "the same", the tests decide.
        assert!(!u8::tmt_same(
            tmt_rt::SHL,
            tmt_rt::arith_swapped(tmt_rt::SHL),
            2,
            9
        ));
        assert!(u8::tmt_same(
            tmt_rt::SHL,
            tmt_rt::arith_swapped(tmt_rt::SHL),
            2,
            0
        ));
        assert!(!u8::tmt_same(
            tmt_rt::DIV,
            tmt_rt::arith_swapped(tmt_rt::DIV),
            4,
            0
        ));
        assert!(
            !u8::tmt_same(tmt_rt::ADD, tmt_rt::arith_swapped(tmt_rt::ADD), 250, 9),
            "overflow in one of the two counts as different"
        );
        assert!(
            !u8::tmt_same(tmt_rt::MUL, tmt_rt::arith_swapped(tmt_rt::MUL), 4, 0),
            "division by zero counts as different"
        );
        assert!(f64::tmt_same(
            tmt_rt::MUL,
            tmt_rt::arith_swapped(tmt_rt::MUL),
            4.0,
            1.0
        ));
        assert!(!f64::tmt_same(
            tmt_rt::ADD,
            tmt_rt::arith_swapped(tmt_rt::ADD),
            4.0,
            0.5
        ));
        assert!(
            !f32::tmt_same(tmt_rt::MUL, tmt_rt::arith_swapped(tmt_rt::MUL), 4.0, 0.0),
            "4*0 is 0, 4/0 is infinite"
        );
    }

    #[test]
    fn runtime_summary_lines_round_trip() {
        use tmt_rt::Diff;
        let observations = [
            (1, Diff::Int(-5)),
            (1, Diff::Int(-2)),
            (4, Diff::Int(9)),
            (2, Diff::Int(0)),
            (4, Diff::Int(3)),
            (1, Diff::Float(-2.5)),
            (1, Diff::Float(-0.125)),
            (4, Diff::Float(1e-7)),
            (8, Diff::None),
        ];
        let mut summary = tmt_rt::Summary::default();
        let mut expected = Obs::default();
        let mut text = String::new();
        for (rel, d) in observations {
            let changed = summary.observe(rel, d);
            assert!(
                changed,
                "each of these observations adds information: {d:?}"
            );
            assert!(
                !summary.observe(rel, d),
                "repeating an observation changes nothing"
            );
            text.push_str(&summary.line(4, "tests::t"));
            expected.merge(&match d {
                Diff::Int(d) => Obs::of_d(d),
                Diff::Float(d) => Obs::of_f(d),
                Diff::None => Obs::of_rel(Rel::Unordered),
            });
        }
        let trace = Trace::parse(&text);
        assert_eq!(trace.malformed_lines, 0);
        assert_eq!(trace.cmp[&4]["tests::t"], expected);
        assert_eq!(expected.f_min_pos, Some(1e-7));

        // Floats only: the summary keeps the float gap.
        let mut floats = tmt_rt::Summary::default();
        floats.observe(1, Diff::Float(-20.5));
        floats.observe(4, Diff::Float(9.5));
        let trace = Trace::parse(&floats.line(1, "t"));
        assert_eq!(trace.cmp[&1]["t"].float_gap(), Some((-20.5, 9.5)));
    }

    #[test]
    fn float_columns_are_validated() {
        let gap = |line: &str| {
            let trace = Trace::parse(line);
            assert_eq!(trace.malformed_lines, 0, "{line:?}");
            let o = trace.cmp[&1]["t"];
            (o.f_max_neg, o.f_min_pos)
        };
        assert_eq!(
            gap("O\t1\tt\t5\t1\t-\t-\t0\t-2.5\t0.5\n"),
            (Some(-2.5), Some(0.5))
        );
        // Wrong sign, zero, or not finite: dropped.
        assert_eq!(gap("O\t1\tt\t5\t1\t-\t-\t0\t2.5\t-0.5\n"), (None, None));
        assert_eq!(gap("O\t1\tt\t5\t1\t-\t-\t0\t0.0\t0.0\n"), (None, None));
        assert_eq!(gap("O\t1\tt\t5\t1\t-\t-\t0\t-inf\tinf\n"), (None, None));
        assert_eq!(gap("O\t1\tt\t5\t1\t-\t-\t0\tNaN\tNaN\n"), (None, None));
        assert_eq!(
            Trace::parse("O\t1\tt\t5\t1\t-\t-\t2\t-\t-\n").malformed_lines,
            1,
            "bad flag"
        );
        assert_eq!(
            Trace::parse("O\t1\tt\t5\t1\t-\t-\t0\tx\t-\n").malformed_lines,
            1
        );
    }

    #[test]
    fn return_lines() {
        let returns = Trace::parse("B\t4\tt\t1\nB\t4\tt\t2\nB\t4\tu\t2\n");
        assert_eq!(returns.malformed_lines, 0);
        assert_eq!(
            returns.returns[&4]["t"],
            ReturnObs {
                saw_true: true,
                saw_false: true
            }
        );
        assert_eq!(
            returns.returns[&4]["u"],
            ReturnObs {
                saw_true: false,
                saw_false: true
            }
        );
        assert_eq!(
            Trace::parse("B\t4\tt\t4\n").malformed_lines,
            1,
            "only two return bits exist"
        );
        assert_eq!(Trace::parse("B\t4\tt\t3\n").malformed_lines, 0);
        assert_eq!(Trace::parse("A\t4\tt\t7\n").malformed_lines, 0);
    }

    #[test]
    fn logic_arithmetic_and_reach_lines() {
        let trace = Trace::parse(
            "L\t1\tt\t1\nL\t1\tt\t5\nL\t1\tu\t2\nA\t2\tt\t1\nA\t2\tu\t6\nR\t3\tt\t1\n",
        );
        assert_eq!(trace.malformed_lines, 0);
        assert_eq!(
            trace.logic[&1]["t"],
            crate::plan::LogicObs {
                short: true,
                rhs_true: false,
                rhs_false: true
            }
        );
        assert_eq!(
            trace.logic[&1]["u"],
            crate::plan::LogicObs {
                short: false,
                rhs_true: true,
                rhs_false: false
            }
        );
        assert_eq!(
            trace.arith[&2]["t"],
            crate::plan::ArithObs {
                same: true,
                differs: false,
                opaque: false,
                ..Default::default()
            }
        );
        assert_eq!(
            trace.arith[&2]["u"],
            crate::plan::ArithObs {
                same: false,
                differs: true,
                opaque: true,
                ..Default::default()
            }
        );
        assert!(trace.reach[&3].contains("t"));
        for bad in [
            "L\t1\tt\t0\n",
            "L\t1\tt\t8\n",
            "A\t1\t\t1\n",
            "R\t1\tt\n",
            "L\tx\tt\t1\n",
            "Z\t1\tt\t1\n",
        ] {
            assert_eq!(Trace::parse(bad).malformed_lines, 1, "{bad:?}");
        }
    }

    #[test]
    #[allow(clippy::needless_borrow)] // the explicit `&` is the call form the probes use
    fn runtime_probes_are_transparent_without_env() {
        use tmt_rt::{Val, ValAny as _, ValNum as _};
        // Autoref specialisation: numbers carry values, everything else does not.
        assert_eq!((&tmt_rt::W(&7u8)).tmt_val(), Val::Int(7, 0, 255));
        assert_eq!(
            (&tmt_rt::W(&&-3i64)).tmt_val(),
            Val::Int(-3, i64::MIN as i128, i64::MAX as i128)
        );
        assert_eq!((&tmt_rt::W(&1.5f64)).tmt_val(), Val::Float(1.5));
        assert_eq!((&tmt_rt::W(&&2.5f32)).tmt_val(), Val::Float(2.5));
        assert_eq!((&tmt_rt::W("text")).tmt_val(), Val::None);
        assert_eq!((&tmt_rt::W(&'c')).tmt_val(), Val::None);

        for (a, b) in [(1, 2), (2, 2), (3, 2)] {
            assert_eq!(
                tmt_rt::ord(0, tmt_rt::LT, &a, &b, Val::None, Val::None),
                a < b
            );
            assert_eq!(
                tmt_rt::ord(0, tmt_rt::LE, &a, &b, Val::None, Val::None),
                a <= b
            );
            assert_eq!(
                tmt_rt::ord(0, tmt_rt::GT, &a, &b, Val::None, Val::None),
                a > b
            );
            assert_eq!(
                tmt_rt::ord(0, tmt_rt::GE, &a, &b, Val::None, Val::None),
                a >= b
            );
            assert_eq!(
                tmt_rt::eq(0, tmt_rt::EQ, &a, &b, Val::None, Val::None),
                a == b
            );
            assert_eq!(
                tmt_rt::eq(0, tmt_rt::NE, &a, &b, Val::None, Val::None),
                a != b
            );
        }
        assert!(!tmt_rt::ord(
            0,
            tmt_rt::GE,
            &f64::NAN,
            &1.0,
            Val::None,
            Val::None
        ));
        assert!(tmt_rt::eq(0, tmt_rt::NE, "a", "b", Val::None, Val::None));
        assert!(tmt_rt::live(0));
    }
    /// The two replacements of each operator are those of cargo-mutants, and the runtime
    /// and the report name the same ones.
    #[test]
    fn both_replacements_of_every_operator() {
        use crate::instrument::ArithOp;
        let code = |op: ArithOp| match op {
            ArithOp::Add => tmt_rt::ADD,
            ArithOp::Sub => tmt_rt::SUB,
            ArithOp::Mul => tmt_rt::MUL,
            ArithOp::Div => tmt_rt::DIV,
            ArithOp::Rem => tmt_rt::REM,
            ArithOp::BitAnd => tmt_rt::BITAND,
            ArithOp::BitOr => tmt_rt::BITOR,
            ArithOp::BitXor => tmt_rt::BITXOR,
            ArithOp::Shl => tmt_rt::SHL,
            ArithOp::Shr => tmt_rt::SHR,
        };
        let table = [
            (ArithOp::Add, "-", Some("*")),
            (ArithOp::Sub, "+", Some("/")),
            (ArithOp::Mul, "/", Some("+")),
            (ArithOp::Div, "*", Some("%")),
            (ArithOp::Rem, "/", Some("+")),
            (ArithOp::BitAnd, "|", Some("^")),
            (ArithOp::BitOr, "&", Some("^")),
            (ArithOp::BitXor, "&", Some("|")),
            (ArithOp::Shl, ">>", None),
            (ArithOp::Shr, "<<", None),
        ];
        assert_eq!(table.map(|row| row.0), ArithOp::ALL);
        for (op, first, second) in table {
            assert_eq!(op.swapped().symbol(), first, "{op:?}");
            assert_eq!(op.swapped2().map(ArithOp::symbol), second, "{op:?}");
            assert_eq!(
                tmt_rt::arith_swapped(code(op)),
                code(op.swapped()),
                "{op:?}"
            );
            assert_eq!(
                tmt_rt::arith_swapped2(code(op)),
                op.swapped2().map(code),
                "{op:?}"
            );
        }
        assert_eq!(
            tmt_rt::parse_mutant("3:as2"),
            Some((3, tmt_rt::Mutant::ArithSwap2))
        );

        // What is recorded for a pair of operands: whether each replacement agrees.
        use tmt_rt::ArithNum;
        // 2 + 2 = 2 * 2, but not 2 - 2.
        assert!(!i32::tmt_same(tmt_rt::ADD, tmt_rt::SUB, 2, 2));
        assert!(i32::tmt_same(tmt_rt::ADD, tmt_rt::MUL, 2, 2));
        // 6 / 1 = 6 * 1, but not 6 % 1.
        assert!(i32::tmt_same(tmt_rt::DIV, tmt_rt::MUL, 6, 1));
        assert!(!i32::tmt_same(tmt_rt::DIV, tmt_rt::REM, 6, 1));
        // 4 | 1 = 4 ^ 1, but not 4 & 1.
        assert!(i32::tmt_same(tmt_rt::BITOR, tmt_rt::BITXOR, 4, 1));
        assert!(!i32::tmt_same(tmt_rt::BITOR, tmt_rt::BITAND, 4, 1));
        assert!(f64::tmt_same(tmt_rt::MUL, tmt_rt::ADD, 2.0, 2.0));

        // And as the planner reads it.
        let seen = |bits: u8| crate::plan::ArithObs::from_bits(bits);
        let both = seen(tmt_rt::ARITH_DIFFERS | tmt_rt::ARITH_SAME2);
        assert!(both.differs && both.same2 && !both.same && !both.differs2 && !both.opaque);
        assert!(both.infected_by(Mutation::ArithSwap));
        assert!(!both.infected_by(Mutation::ArithSwap2));
        let other = seen(tmt_rt::ARITH_SAME | tmt_rt::ARITH_DIFFERS2);
        assert!(!other.infected_by(Mutation::ArithSwap));
        assert!(other.infected_by(Mutation::ArithSwap2));
        assert_eq!(crate::plan::plan_arith(&both), crate::plan::ArithPlan::Run);
        assert_eq!(
            crate::plan::plan_arith(&both.second()),
            crate::plan::ArithPlan::NeverDiffers
        );
        assert_eq!(
            crate::plan::plan_arith(&other.second()),
            crate::plan::ArithPlan::Run
        );
        let mut merged = both;
        merged.merge(&other);
        assert!(merged.same && merged.differs && merged.same2 && merged.differs2);
        let mut merged = other;
        merged.merge(&both);
        assert!(merged.same && merged.differs && merged.same2 && merged.differs2);
        // An assignment that collects bits has one replacement, as in cargo-mutants.
        assert_eq!(ArithOp::BitOr.second(true), None);
        assert_eq!(ArithOp::BitAnd.second(true), None);
        assert_eq!(ArithOp::BitXor.second(true), Some(ArithOp::BitOr));
        assert_eq!(ArithOp::BitOr.second(false), Some(ArithOp::BitXor));
        assert_eq!(ArithOp::Add.second(true), Some(ArithOp::Mul));
        // Operands that are not numbers stay that for both.
        assert_eq!(
            crate::plan::plan_arith(&seen(tmt_rt::ARITH_OPAQUE).second()),
            crate::plan::ArithPlan::Opaque
        );
        assert_eq!(Mutation::ArithSwap2.encode(), "as2");

        // A trace line with the wider bits is read; one beyond them is not.
        let trace = Trace::parse("A\t1\tt\t18\nA\t2\tt\t31\nA\t3\tt\t32\n");
        assert_eq!(trace.arith[&1]["t"], seen(18));
        assert!(trace.arith.contains_key(&2) && !trace.arith.contains_key(&3));
        assert_eq!(trace.malformed_lines, 1);
    }

    /// A probe in a loop asks the tracer only when it has something new to say.
    #[test]
    fn a_thread_remembers_what_it_recorded() {
        // Sites of their own, far from the ones other tests of this binary use.
        let site = 900_000;
        assert!(!tmt_rt::bits_known(b'A', site, 1));
        assert!(tmt_rt::bits_are_news(b'A', site, 1));
        assert!(!tmt_rt::bits_are_news(b'A', site, 1));
        assert!(tmt_rt::bits_known(b'A', site, 1));
        // Another bit of the same site is news once, and then both are known.
        assert!(!tmt_rt::bits_known(b'A', site, 3));
        assert!(tmt_rt::bits_are_news(b'A', site, 3));
        assert!(!tmt_rt::bits_are_news(b'A', site, 2));
        assert!(tmt_rt::bits_known(b'A', site, 3));
        // Each kind of record and each site has its own memory.
        for kind in *b"RLB" {
            assert!(!tmt_rt::bits_known(kind, site, 1), "{}", kind as char);
            assert!(tmt_rt::bits_are_news(kind, site, 1), "{}", kind as char);
            assert!(!tmt_rt::bits_are_news(kind, site, 1), "{}", kind as char);
        }
        assert!(tmt_rt::bits_are_news(b'A', site + 1, 1));
        assert!(!tmt_rt::bits_known(b'A', site - 1, 1));
        // And each thread: a test is a thread.
        std::thread::spawn(move || {
            assert!(!tmt_rt::bits_known(b'A', site, 1));
            assert!(tmt_rt::bits_are_news(b'A', site, 1));
        })
        .join()
        .unwrap();
    }
}
