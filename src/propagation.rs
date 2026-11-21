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

type Sites = BTreeMap<u32, Seen>;
type IndexedSites = Box<[(u32, Seen)]>;

fn per_test(trace: &Trace) -> BTreeMap<&str, Sites> {
    let mut all: BTreeMap<&str, Sites> = BTreeMap::new();
    for (site, tests) in &trace.reach {
        for test in tests {
            all.entry(test)
                .or_default()
                .entry(*site)
                .or_default()
                .reached = true;
        }
    }
    for (site, tests) in &trace.cmp {
        for (test, obs) in tests {
            let Obs { lt, eq, gt, un, .. } = *obs;
            let seen = all.entry(test).or_default().entry(*site).or_default();
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
                // Not an outcome: it comes with every value the right operand gives.
                rhs_started: _,
            } = *obs;
            let seen = all.entry(test).or_default().entry(*site).or_default();
            seen.reached |= obs.reached();
            // tmt:ignore the bits do not overlap, so `^` joins them just as well
            seen.outcomes |= short as u8 | (rhs_true as u8) << 1 | (rhs_false as u8) << 2;
        }
    }
    for (site, tests) in &trace.arith {
        for (test, obs) in tests {
            all.entry(test)
                .or_default()
                .entry(*site)
                .or_default()
                .reached |= obs.same || obs.differs || obs.opaque;
        }
    }
    for (site, tests) in &trace.returns {
        for (test, obs) in tests {
            let ReturnObs {
                saw_true,
                saw_false,
            } = *obs;
            let seen = all.entry(test).or_default().entry(*site).or_default();
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
    BaselineIndex::new(original).changes(mutated, tests, mutated_site)
}

/// Baseline observations grouped by test so survivor replays can look up only the tests
/// they ran and share the index across comparisons.
pub(crate) struct BaselineIndex(BTreeMap<String, IndexedSites>);

impl BaselineIndex {
    pub(crate) fn new(original: &Trace) -> Self {
        Self(
            per_test(original)
                .into_iter()
                .map(|(test, sites)| (test.to_owned(), sites.into_iter().collect()))
                .collect(),
        )
    }

    pub(crate) fn changes(
        &self,
        mutated: &Trace,
        tests: Option<&BTreeSet<String>>,
        mutated_site: u32,
    ) -> BTreeMap<String, BTreeSet<Change>> {
        let mut out = BTreeMap::new();
        if tests.is_some_and(BTreeSet::is_empty) {
            return out;
        }
        let mut after = per_test(mutated);
        let mut compare = |test: &str, was: Option<&[(u32, Seen)]>, now: Option<&Sites>| {
            let found = compare_sites(was.unwrap_or_default(), now, mutated_site);
            if !found.is_empty() {
                out.insert(test.to_owned(), found);
            }
        };
        if let Some(tests) = tests {
            for test in tests {
                compare(
                    test,
                    self.0.get(test).map(Box::as_ref),
                    after.get(test.as_str()),
                );
            }
        } else {
            for (test, was) in &self.0 {
                compare(
                    test,
                    Some(was.as_ref()),
                    after.remove(test.as_str()).as_ref(),
                );
            }
            for (test, now) in &after {
                compare(test, None, Some(now));
            }
        }
        out
    }
}

fn compare_sites(
    before: &[(u32, Seen)],
    after: Option<&Sites>,
    mutated_site: u32,
) -> BTreeSet<Change> {
    let mut found = BTreeSet::new();
    // Both sides are ordered by site, so each observation is visited once.
    let mut before = before.iter().copied().peekable();
    let mut after = after
        .into_iter()
        .flatten()
        .map(|(&site, &seen)| (site, seen))
        .peekable();
    loop {
        let site = match (before.peek(), after.peek()) {
            (Some(&(left, _)), Some(&(right, _))) => left.min(right),
            (Some(&(site, _)), None) | (None, Some(&(site, _))) => site,
            (None, None) => break,
        };
        let was = before
            .next_if(|entry| entry.0 == site)
            .unwrap_or_default()
            .1;
        let now = after.next_if(|entry| entry.0 == site).unwrap_or_default().1;
        if site == mutated_site {
            continue;
        }
        if was.reached && !now.reached {
            found.insert(Change::NoLongerRuns { site });
        }
        if now.reached && !was.reached {
            found.insert(Change::NowRuns { site });
        } else if now.outcomes & !was.outcomes != 0 {
            found.insert(Change::NewOutcome { site });
        }
        for (value, now_seen, was_seen) in [
            (true, now.returned_true, was.returned_true),
            (false, now.returned_false, was.returned_false),
        ] {
            // Only worth saying for a function the test already called.
            if now_seen && !was_seen && was.reached {
                found.insert(Change::Returns { site, value });
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn only(test: &str) -> BTreeSet<String> {
        [test.to_string()].into_iter().collect()
    }

    #[test]
    fn indexed_effects_cover_attribution_filter_and_mutated_sites() {
        let cases = [
            ("", "R\t1\t{test}\t1\n", vec![Change::NowRuns { site: 1 }]),
            (
                "R\t1\t{test}\t1\n",
                "",
                vec![Change::NoLongerRuns { site: 1 }],
            ),
            (
                "O\t1\t{test}\t1\t0\t-\t-\t1\t-\t-\n",
                "O\t1\t{test}\t9\t0\t-\t-\t1\t-\t-\n",
                vec![Change::NewOutcome { site: 1 }],
            ),
            (
                "L\t1\t{test}\t8\n",
                "L\t1\t{test}\t10\n",
                vec![Change::NewOutcome { site: 1 }],
            ),
            (
                "B\t1\t{test}\t2\n",
                "B\t1\t{test}\t3\n",
                vec![Change::Returns {
                    site: 1,
                    value: true,
                }],
            ),
            (
                "B\t1\t{test}\t1\n",
                "B\t1\t{test}\t3\n",
                vec![Change::Returns {
                    site: 1,
                    value: false,
                }],
            ),
            ("O\t1\t{test}\t0\t0\t-\t-\t1\t-\t-\n", "", Vec::new()),
            ("R\t1\t{test}\t1\n", "R\t1\t{test}\t1\n", Vec::new()),
            (
                "R\t1\t{test}\t1\nL\t1\t{test}\t8\nB\t1\t{test}\t2\n",
                "L\t1\t{test}\t10\nB\t1\t{test}\t3\nA\t1\t{test}\t4\n",
                vec![
                    Change::Returns {
                        site: 1,
                        value: true,
                    },
                    Change::NewOutcome { site: 1 },
                ],
            ),
        ];
        let empty = BTreeSet::new();
        for (before, after, changes) in cases {
            for test in ["selected", crate::trace::UNKNOWN_TEST] {
                let selected = only(test);
                let before = Trace::parse(&format!(
                    "{}R\t2\tbaseline_only\t1\n",
                    before.replace("{test}", test)
                ));
                let after = Trace::parse(&format!(
                    "{}R\t{}\tmutant_only\t1\n",
                    after.replace("{test}", test),
                    u32::MAX,
                ));
                let index = BaselineIndex::new(&before);
                for tests in [None, Some(&selected), Some(&empty)] {
                    for mutated_site in [0, 1] {
                        let mut expected = BTreeMap::new();
                        if tests.is_none() {
                            expected.insert(
                                "baseline_only".to_string(),
                                BTreeSet::from([Change::NoLongerRuns { site: 2 }]),
                            );
                            expected.insert(
                                "mutant_only".to_string(),
                                BTreeSet::from([Change::NowRuns { site: u32::MAX }]),
                            );
                        }
                        if tests != Some(&empty) && mutated_site != 1 && !changes.is_empty() {
                            expected.insert(test.to_string(), changes.iter().cloned().collect());
                        }
                        assert_eq!(index.changes(&after, tests, mutated_site), expected);
                    }
                }
            }
        }
    }

    #[test]
    fn reusing_index_keeps_disjoint_test_runs_independent() {
        let original = Trace::parse("R\t1\tt\t1\nR\t2\tu\t1\n");
        let index = BaselineIndex::new(&original);
        let mutant_t = Trace::parse("R\t3\tt\t1\nR\t4\t?\t1\n");
        let mutant_u = Trace::parse("R\t2\tu\t1\n");
        let expected = BTreeMap::from([(
            "t".to_string(),
            BTreeSet::from([
                Change::NoLongerRuns { site: 1 },
                Change::NowRuns { site: 3 },
            ]),
        )]);
        assert_eq!(
            index.changes(&mutant_t, Some(&only("t")), u32::MAX),
            expected
        );
        assert!(
            index
                .changes(&mutant_u, Some(&only("u")), u32::MAX)
                .is_empty()
        );
        assert_eq!(
            index.changes(&mutant_t, Some(&only("t")), u32::MAX),
            expected
        );
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
