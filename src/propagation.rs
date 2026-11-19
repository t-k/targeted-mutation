//! What a surviving mutant changes further down: the difference between the trace of the
//! original and the trace of the same tests with the mutant switched on.
//!
//! An assertion gap says "these tests run the changed code and pass". This says what the
//! change did in those tests, which is where an assertion can catch it. If it did
//! nothing, the tests do not use the result of the mutated code.

use std::collections::{BTreeMap, BTreeSet};

use crate::plan::{LogicObs, Obs, ReturnObs};
use crate::trace::Trace;

/// One difference in one test, at a site other than the mutated one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Change {
    /// A `-> bool` function now also returns this value.
    Returns { site: u32, value: bool },
    /// A statement, operator or comparison that the test did not execute before.
    NowRuns { site: u32 },
    /// Executed before, not any more.
    NoLongerRuns { site: u32 },
    /// A comparison or condition is evaluated with operands in a relation not seen before.
    NewOutcome { site: u32 },
}

impl Change {
    pub fn site(&self) -> u32 {
        match self {
            Change::Returns { site, .. }
            | Change::NowRuns { site }
            | Change::NoLongerRuns { site }
            | Change::NewOutcome { site } => *site,
        }
    }
}

/// Everything one test did at one site, reduced to what can be compared.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct Seen {
    reached: bool,
    /// Relations of a comparison, or the observation bits of `&&` / `||`.
    outcomes: u8,
    returned_true: bool,
    returned_false: bool,
}

fn per_test(trace: &Trace) -> BTreeMap<(&str, u32), Seen> {
    let mut all: BTreeMap<(&str, u32), Seen> = BTreeMap::new();
    for (site, tests) in &trace.reach {
        for test in tests {
            all.entry((test, *site)).or_default().reached = true;
        }
    }
    for (site, tests) in &trace.cmp {
        for (test, obs) in tests {
            let Obs { lt, eq, gt, un, .. } = *obs;
            let seen = all.entry((test, *site)).or_default();
            seen.reached |= obs.reached();
            // tmt:ignore the bits do not overlap, so `^` joins them just as well
            seen.outcomes |= lt as u8 | (eq as u8) << 1 | (gt as u8) << 2 | (un as u8) << 3;
        }
    }
    for (site, tests) in &trace.logic {
        for (test, obs) in tests {
            let LogicObs {
                short,
                rhs_true,
                rhs_false,
            } = *obs;
            let seen = all.entry((test, *site)).or_default();
            seen.reached |= obs.reached();
            // tmt:ignore the bits do not overlap, so `^` joins them just as well
            seen.outcomes |= short as u8 | (rhs_true as u8) << 1 | (rhs_false as u8) << 2;
        }
    }
    for (site, tests) in &trace.arith {
        for (test, obs) in tests {
            all.entry((test, *site)).or_default().reached |= obs.same || obs.differs || obs.opaque;
        }
    }
    for (site, tests) in &trace.returns {
        for (test, obs) in tests {
            let ReturnObs {
                saw_true,
                saw_false,
            } = *obs;
            let seen = all.entry((test, *site)).or_default();
            seen.reached |= obs.reached();
            seen.returned_true |= saw_true;
            seen.returned_false |= saw_false;
        }
    }
    all
}

/// The changes per test between `original` and `mutated`, ignoring the mutated site itself
/// (its probe does not record while its mutant is active). `tests` limits the comparison
/// to the tests that were run with the mutant; `None` means the whole binary ran.
pub fn changes(
    original: &Trace,
    mutated: &Trace,
    tests: Option<&BTreeSet<String>>,
    mutated_site: u32,
) -> BTreeMap<String, BTreeSet<Change>> {
    let (before, after) = (per_test(original), per_test(mutated));
    let in_scope =
        |test: &str, site: u32| site != mutated_site && tests.is_none_or(|t| t.contains(test));
    let mut out: BTreeMap<String, BTreeSet<Change>> = BTreeMap::new();
    for ((test, site), now) in &after {
        if !in_scope(test, *site) {
            continue;
        }
        let was = before.get(&(*test, *site)).copied().unwrap_or_default();
        let found = out.entry(test.to_string()).or_default();
        if now.reached && !was.reached {
            found.insert(Change::NowRuns { site: *site });
        } else if now.outcomes & !was.outcomes != 0 {
            found.insert(Change::NewOutcome { site: *site });
        }
        for (value, now_seen, was_seen) in [
            (true, now.returned_true, was.returned_true),
            (false, now.returned_false, was.returned_false),
        ] {
            // Only worth saying for a function the test already called.
            if now_seen && !was_seen && was.reached {
                found.insert(Change::Returns { site: *site, value });
            }
        }
    }
    for ((test, site), was) in &before {
        if in_scope(test, *site)
            && was.reached
            && !after.get(&(*test, *site)).is_some_and(|now| now.reached)
        {
            out.entry(test.to_string())
                .or_default()
                .insert(Change::NoLongerRuns { site: *site });
        }
    }
    out.retain(|_, found| !found.is_empty());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn only(test: &str) -> BTreeSet<String> {
        [test.to_string()].into_iter().collect()
    }

    #[test]
    fn statements_that_start_or_stop_running() {
        let original = Trace::parse("R\t1\tt\t1\nR\t2\tt\t1\nR\t2\tu\t1\n");
        let mutated = Trace::parse("R\t1\tt\t1\nR\t3\tt\t1\nR\t2\tu\t1\n");
        let found = changes(&original, &mutated, None, 9);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(
            found["t"].iter().cloned().collect::<Vec<_>>(),
            [
                Change::NowRuns { site: 3 },
                Change::NoLongerRuns { site: 2 }
            ]
        );
        // The mutated site itself is never reported.
        assert!(
            changes(&original, &mutated, None, 3)["t"]
                .iter()
                .all(|c| c.site() != 3)
        );
        assert!(
            !changes(&original, &mutated, None, 2)["t"]
                .iter()
                .any(|c| c.site() == 2)
        );
    }

    #[test]
    fn tests_that_were_not_rerun_are_not_compared() {
        // With the mutant only `t` ran, so `u` is missing from the mutated trace.
        let original = Trace::parse("R\t1\tt\t1\nR\t1\tu\t1\n");
        let mutated = Trace::parse("R\t1\tt\t1\n");
        assert!(changes(&original, &mutated, Some(&only("t")), 9).is_empty());
        // Had the whole binary run, `u` not running the statement would be a change.
        assert_eq!(
            changes(&original, &mutated, None, 9)["u"]
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            [Change::NoLongerRuns { site: 1 }]
        );
    }

    #[test]
    fn comparisons_conditions_and_results() {
        let original =
            Trace::parse("O\t1\tt\t1\t0\t-3\t-\t1\t-\t-\nL\t2\tt\t1\nB\t3\tt\t2\nA\t4\tt\t1\n");
        let mutated = Trace::parse(
            "O\t1\tt\t5\t0\t-3\t2\t1\t-\t-\nL\t2\tt\t3\nB\t3\tt\t3\nA\t4\tt\t2\nB\t5\tt\t1\n",
        );
        let found = changes(&original, &mutated, None, 9);
        assert_eq!(
            found["t"].iter().cloned().collect::<Vec<_>>(),
            [
                Change::Returns {
                    site: 3,
                    value: true
                },
                // A function the test did not call before is "now runs", not "now returns".
                Change::NowRuns { site: 5 },
                Change::NewOutcome { site: 1 },
                Change::NewOutcome { site: 2 },
            ]
        );
        // Identical traces: nothing propagates.
        assert!(changes(&original, &original, None, 9).is_empty());
        // Seeing fewer outcomes than before is not reported as a new outcome.
        assert!(
            changes(&mutated, &original, None, 9)["t"]
                .iter()
                .all(|c| matches!(c, Change::NoLongerRuns { site: 5 }))
        );
    }
    fn kinds(found: &BTreeMap<String, BTreeSet<Change>>) -> Vec<Change> {
        found
            .get("t")
            .map(|c| c.iter().cloned().collect())
            .unwrap_or_default()
    }

    fn cmp_line(rels: u8) -> String {
        format!("O\t1\tt\t{rels}\t0\t-\t-\t1\t-\t-\n")
    }

    #[test]
    fn a_site_of_any_kind_counts_as_run() {
        let nothing = Trace::parse("");
        for line in [
            cmp_line(1),
            cmp_line(2),
            cmp_line(4),
            cmp_line(8),
            "L\t1\tt\t1\n".to_string(),
            "L\t1\tt\t2\n".to_string(),
            "L\t1\tt\t4\n".to_string(),
            "A\t1\tt\t1\n".to_string(),
            "A\t1\tt\t2\n".to_string(),
            "A\t1\tt\t4\n".to_string(),
            "B\t1\tt\t1\n".to_string(),
            "B\t1\tt\t2\n".to_string(),
        ] {
            let ran = Trace::parse(&line);
            assert_eq!(
                kinds(&changes(&nothing, &ran, None, 9)),
                [Change::NowRuns { site: 1 }],
                "{line}"
            );
            assert_eq!(
                kinds(&changes(&ran, &nothing, None, 9)),
                [Change::NoLongerRuns { site: 1 }],
                "{line}"
            );
        }
    }

    #[test]
    fn a_record_without_an_outcome_is_not_a_run() {
        // A comparison record that saw no relation, as a truncated trace could give.
        let nothing = Trace::parse("");
        let empty = Trace::parse(&cmp_line(0));
        assert!(changes(&nothing, &empty, None, 9).is_empty());
        assert!(changes(&empty, &nothing, None, 9).is_empty());
    }

    #[test]
    fn every_new_relation_and_condition_outcome_is_a_new_outcome() {
        for (base, extra) in [(1u8, 2u8), (2, 1), (1, 4), (1, 8), (4, 2)] {
            let found = changes(
                &Trace::parse(&cmp_line(base)),
                &Trace::parse(&cmp_line(base | extra)),
                None,
                9,
            );
            assert_eq!(
                kinds(&found),
                [Change::NewOutcome { site: 1 }],
                "{base} {extra}"
            );
        }
        for (base, extra) in [(1u8, 2u8), (2, 1), (1, 4), (4, 2)] {
            let found = changes(
                &Trace::parse(&format!("L\t1\tt\t{base}\n")),
                &Trace::parse(&format!("L\t1\tt\t{}\n", base | extra)),
                None,
                9,
            );
            assert_eq!(
                kinds(&found),
                [Change::NewOutcome { site: 1 }],
                "{base} {extra}"
            );
        }
    }

    #[test]
    fn a_new_result_of_a_called_function() {
        let (only_true, only_false, both) = (
            Trace::parse("B\t1\tt\t1\n"),
            Trace::parse("B\t1\tt\t2\n"),
            Trace::parse("B\t1\tt\t3\n"),
        );
        assert_eq!(
            kinds(&changes(&only_true, &both, None, 9)),
            [Change::Returns {
                site: 1,
                value: false
            }]
        );
        assert_eq!(
            kinds(&changes(&only_false, &both, None, 9)),
            [Change::Returns {
                site: 1,
                value: true
            }]
        );
        assert!(changes(&both, &only_true, None, 9).is_empty());
    }
}
