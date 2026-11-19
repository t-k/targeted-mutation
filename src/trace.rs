//! Reads the trace written by the injected runtime (`src/runtime/tmt_rt.rs`).
//!
//! Lines are tab separated:
//! - `O <site> <test> <rels> <unvalued> <max_neg|-> <min_pos|->`: cumulative summary of a comparison
//! - `R <site> <test>`: a statement site was executed
//! - `T <site> <lhs_min> <lhs_max> <rhs_min> <rhs_max>`: integer type bounds of the operands
//!
//! A process killed mid-write can leave a truncated last line; malformed lines are skipped
//! and counted rather than trusted.

use std::collections::{BTreeMap, BTreeSet};

use crate::plan::Obs;

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
    /// site -> tests that executed the statement
    pub reach: BTreeMap<u32, BTreeSet<String>>,
    pub bounds: BTreeMap<u32, TypeBounds>,
    pub malformed_lines: usize,
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
            ["O", site, test, rels, unvalued, max_neg, min_pos] => {
                let rels: u8 = rels.parse().ok().filter(|r| *r < 16)?;
                let opt = |s: &str| {
                    if s == "-" {
                        Some(None)
                    } else {
                        s.parse::<i128>().ok().map(Some)
                    }
                };
                let obs = Obs {
                    lt: rels & 1 != 0,
                    eq: rels & 2 != 0,
                    gt: rels & 4 != 0,
                    un: rels & 8 != 0,
                    unvalued: match unvalued {
                        "0" => false,
                        "1" => true,
                        _ => return None,
                    },
                    max_neg: opt(max_neg)?.filter(|d| *d < 0),
                    min_pos: opt(min_pos)?.filter(|d| *d > 0),
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
            ["R", site, test] => {
                (!test.is_empty()).then_some(())?;
                self.reach
                    .entry(site.parse().ok()?)
                    .or_default()
                    .insert(test.to_string());
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
            "O\t3\ttests::a\t1\t0\t-4\t-\n\
             O\t3\ttests::a\t5\t0\t-1\t2\n\
             O\t3\ttests::b\t2\t0\t-\t-\n\
             R\t9\ttests::a\n\
             R\t9\t?\n\
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
            "O\t3\ttests::a\t1\t0\t-4\nX\t1\nO\tx\tt\t1\t0\t-\t-\nO\t1\tt\t99\t0\t-\t-\nR\t1\t\nR\t2\tok\n",
        );
        assert_eq!(trace.malformed_lines, 5);
        assert!(trace.cmp.is_empty());
        assert_eq!(trace.reach.len(), 1);
    }

    #[test]
    fn relation_bits_and_field_validation() {
        for (rels, expected) in [(1, "lt"), (2, "eq"), (4, "gt"), (8, "un")] {
            let trace = Trace::parse(&format!("O\t1\tt\t{rels}\t1\t-\t-\n"));
            let o = trace.cmp[&1]["t"];
            let set: Vec<&str> = [(o.lt, "lt"), (o.eq, "eq"), (o.gt, "gt"), (o.un, "un")]
                .into_iter()
                .filter_map(|(on, name)| on.then_some(name))
                .collect();
            assert_eq!(set, [expected]);
            assert!(o.unvalued);
        }
        // 15 is every relation, 16 is not a relation mask.
        assert_eq!(Trace::parse("O\t1\tt\t15\t0\t-\t-\n").malformed_lines, 0);
        assert_eq!(Trace::parse("O\t1\tt\t16\t0\t-\t-\n").malformed_lines, 1);
        // A comparison line needs a test name too.
        let nameless = Trace::parse("O\t1\t\t1\t0\t-\t-\n");
        assert_eq!((nameless.malformed_lines, nameless.cmp.len()), (1, 0));
        // The nearest-below value is negative and the nearest-above positive, or they are dropped.
        let signs = Trace::parse("O\t1\tt\t5\t0\t0\t0\nO\t2\tt\t5\t0\t3\t-3\n");
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
                        tmt_rt::apply(rt_op(op), rt_mutant(m), op.eval_d(d), rel, Some(d)),
                        eval_mutated_d(op, m, d),
                        "{op:?} {m:?} d={d}"
                    );
                }
            }
            // Unordered operands (NaN): only `!=` is true.
            assert_eq!(
                tmt_rt::apply(rt_op(op), tmt_rt::Mutant::Op(rt_op(op)), false, 8, None),
                op.eval_rel(Rel::Unordered)
            );
        }
        assert_eq!(
            tmt_rt::parse_mutant("12:del"),
            Some((12, tmt_rt::Mutant::Delete))
        );
        for bad in ["", "x:neg", "1:", "1:op9", "1:shx", "1:zzz", "neg"] {
            assert_eq!(tmt_rt::parse_mutant(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn runtime_summary_lines_round_trip() {
        let observations: [(u8, Option<i128>); 6] = [
            (1, Some(-5)),
            (1, Some(-2)),
            (4, Some(9)),
            (2, Some(0)),
            (4, Some(3)),
            (8, None),
        ];
        let mut summary = tmt_rt::Summary::default();
        let mut expected = Obs::default();
        let mut text = String::new();
        for (rel, d) in observations {
            let changed = summary.observe(rel, d);
            assert!(changed, "each of these observations adds information");
            assert!(
                !summary.observe(rel, d),
                "repeating an observation changes nothing"
            );
            text.push_str(&summary.line(4, "tests::t"));
            expected.merge(&match d {
                Some(d) => Obs::of_d(d),
                None => Obs::of_rel(Rel::Unordered),
            });
        }
        let trace = Trace::parse(&text);
        assert_eq!(trace.malformed_lines, 0);
        assert_eq!(trace.cmp[&4]["tests::t"], expected);
    }

    #[test]
    #[allow(clippy::needless_borrow)] // the explicit `&` is the call form the probes use
    fn runtime_probes_are_transparent_without_env() {
        use tmt_rt::{ValAny as _, ValInt as _};
        // Autoref specialisation: integers carry values, everything else does not.
        assert_eq!((&tmt_rt::W(&7u8)).tmt_val(), Some((7, 0, 255)));
        assert_eq!(
            (&tmt_rt::W(&&-3i64)).tmt_val(),
            Some((-3, i64::MIN as i128, i64::MAX as i128))
        );
        assert_eq!((&tmt_rt::W("text")).tmt_val(), None);
        assert_eq!((&tmt_rt::W(&1.5f64)).tmt_val(), None);

        for (a, b) in [(1, 2), (2, 2), (3, 2)] {
            assert_eq!(tmt_rt::ord(0, tmt_rt::LT, &a, &b, None, None), a < b);
            assert_eq!(tmt_rt::ord(0, tmt_rt::LE, &a, &b, None, None), a <= b);
            assert_eq!(tmt_rt::ord(0, tmt_rt::GT, &a, &b, None, None), a > b);
            assert_eq!(tmt_rt::ord(0, tmt_rt::GE, &a, &b, None, None), a >= b);
            assert_eq!(tmt_rt::eq(0, tmt_rt::EQ, &a, &b, None, None), a == b);
            assert_eq!(tmt_rt::eq(0, tmt_rt::NE, &a, &b, None, None), a != b);
        }
        assert!(!tmt_rt::ord(0, tmt_rt::GE, &f64::NAN, &1.0, None, None));
        assert!(tmt_rt::eq(0, tmt_rt::NE, "a", "b", None, None));
        assert!(tmt_rt::live(0));
    }
}
