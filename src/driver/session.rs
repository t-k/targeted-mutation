//! Trace-driven mutation planning, job execution, timeout confirmation, and sharding.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::checkpoint::Phase;
use crate::instrument::{self, LiteralSide, Site, SiteKind};
use crate::plan::{self, Mutation, Obs};
use crate::progress;
use crate::propagation::{self, Change};
use crate::report::{self, Finding};
use crate::trace::Trace;

use super::*;

pub(super) struct Session {
    pub(super) progress: bool,
    pub(super) serial_timeout_rechecks: bool,
    pub(super) package_dir: PathBuf,
    pub(super) binaries: Vec<TestBinary>,
}

/// One mutant and the tests it has to face, per test binary.
pub(super) struct Job {
    pub(super) site: u32,
    pub(super) mutation: Mutation,
    /// `(index of the binary, tests to run or None for all of them)`.
    pub(super) tests: Vec<(usize, Option<Vec<String>>)>,
}

// Jobs are appended in the same order as the numbered probe sites.
pub(super) fn jobs_for_site(jobs: &[Job], site: u32) -> std::ops::Range<usize> {
    let first = jobs.partition_point(|job| job.site < site);
    let count = jobs[first..].partition_point(|job| job.site == site);
    first..first + count
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) struct JobResult {
    pub(super) first_pass_timeout: bool,
    pub(super) unconfirmed_timeout: bool,
    /// The tests the mutant passed, when it passed all of them.
    pub(super) survived: Option<Vec<String>>,
    pub(super) timed_out: bool,
    /// The mutant could not be run as what it is: neither found out nor passed.
    pub(super) unable: bool,
}

/// What the re-runs say of a survivor the trace predicted.
#[derive(Debug, PartialEq)]
pub(super) enum Prediction {
    /// Every re-run passed all the tests.
    Confirmed,
    /// A re-run was found out by the tests: the trace did not see everything.
    Refuted,
    /// No re-run was found out, and one could not be run or was only stopped by a time
    /// limit that was not confirmed: neither a finding nor a contradiction.
    Unknown,
}

impl Prediction {
    pub(super) fn of<'a>(runs: impl Iterator<Item = &'a JobResult>) -> Prediction {
        let mut verdict = Prediction::Confirmed;
        for run in runs {
            if run.unable || run.unconfirmed_timeout {
                verdict = Prediction::Unknown;
            } else if run.survived.is_none() {
                return Prediction::Refuted;
            }
        }
        verdict
    }
}

impl report::TimeoutRechecks {
    pub(super) fn include(&mut self, result: &JobResult) {
        if result.first_pass_timeout {
            self.first_pass += 1;
            if result.unconfirmed_timeout || result.unable {
                self.unconfirmed += 1;
            } else if result.timed_out {
                self.confirmed += 1;
            } else {
                self.recovered += 1;
            }
        }
    }
}

impl JobResult {
    pub(super) fn progress_outcome(&self) -> progress::Outcome {
        if self.unable {
            progress::Outcome::Unable
        } else if self.timed_out {
            progress::Outcome::TimedOut
        } else if self.survived.is_some() {
            progress::Outcome::Survived
        } else {
            progress::Outcome::Failed
        }
    }
}

/// The tests of one binary whose observations satisfy `pick`.
pub(super) fn tests_where<T>(
    per_test: Option<&BTreeMap<String, T>>,
    pick: impl Fn(&T) -> bool,
) -> BTreeSet<String> {
    per_test
        .into_iter()
        .flatten()
        .filter(|(_, obs)| pick(obs))
        .map(|(test, _)| test.clone())
        .collect()
}

pub(super) type InfectedTests<'a> = Box<dyn Fn(&TestBinary, Mutation) -> BTreeSet<String> + 'a>;
pub(super) type ReachingTests<'a> = Box<dyn Fn(&TestBinary) -> BTreeSet<String> + 'a>;

/// A predicted survivor.
pub(super) struct PlannedGap {
    pub(super) finding: Finding,
    /// Mutations to confirm the prediction with.
    pub(super) survivors: Vec<Mutation>,
    /// The gap says nothing more than "the expression in this byte range had one value in
    /// every test". If that expression is a site of its own that already says so, the
    /// gap is not reported a second time.
    pub(super) constant_expr: Option<(usize, usize)>,
}

/// The byte range of an operand without the parentheses and spaces around it.
pub(super) fn core_range(start: usize, text: &str) -> (usize, usize) {
    let (mut from, mut to) = (0, text.len());
    loop {
        let inner = &text[from..to];
        let trimmed = inner.trim();
        from += inner.len() - inner.trim_start().len();
        to = from + trimmed.len();
        let wrapped = trimmed.starts_with('(') && trimmed.ends_with(')') && {
            // The first parenthesis must be the one the last closes: `(a) && (b)` is not wrapped.
            let mut depth = 0usize;
            trimmed.char_indices().all(|(i, c)| {
                match c {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                depth > 0 || i == trimmed.len() - 1
            })
        };
        if !wrapped {
            return (start + from, start + to);
        }
        from += 1;
        to -= 1;
    }
}

/// What the planner decided for one site.
pub(super) struct SitePlan<'a> {
    /// Predicted survivors: the finding to report and the mutations to confirm it with.
    pub(super) gaps: Vec<PlannedGap>,
    /// The comparison cannot have another outcome within its operand types. A function
    /// or condition built on it is constant for a reason no test can change.
    pub(super) cannot_differ: bool,
    /// The expression had a single value in every test.
    pub(super) constant: bool,
    /// Mutations some test observes differently.
    pub(super) run: Vec<Mutation>,
    /// Tests of a binary whose execution the mutation can change.
    pub(super) infected: InfectedTests<'a>,
    /// Tests of a binary that execute the site at all.
    pub(super) reaching: ReachingTests<'a>,
}

pub(super) enum Planned<'a> {
    Unreached,
    /// Reached, and its mutant cannot be run as a probe here: said, not passed over.
    NotEvaluated,
    /// Reached, but nothing to mutate (operands that are not numbers).
    Nothing,
    Plan(SitePlan<'a>),
}

/// A predicted survivor with the jobs that confirm it and the expression it calls constant.
pub(super) type ConfirmedGap = (Finding, Vec<usize>, Option<(usize, usize)>);

/// A site with its jobs: indices into the job list.
pub(super) struct SiteWork<'a> {
    pub(super) file: &'a str,
    pub(super) site: &'a Site,
    pub(super) gaps: Vec<ConfirmedGap>,
    pub(super) constant: bool,
    pub(super) cannot_differ: bool,
    pub(super) runs: Vec<(Mutation, usize)>,
}

impl Session {
    pub(super) fn note(&self, text: &str) {
        if self.progress {
            eprintln!("tmt: {text}");
        }
    }

    pub(super) fn merged<T: Default + Copy>(
        &self,
        select: impl Fn(&Trace) -> Option<&BTreeMap<String, T>>,
        merge: impl Fn(&mut T, &T),
    ) -> T {
        let mut all = T::default();
        for obs in self
            .binaries
            .iter()
            .filter_map(|b| select(&b.trace))
            .flat_map(|t| t.values())
        {
            merge(&mut all, obs);
        }
        all
    }

    /// `primitive` says of a byte range of the file whether it is an arithmetic
    /// operation that was run on primitive numbers.
    pub(super) fn plan<'a>(
        &'a self,
        file: &str,
        site: &'a Site,
        verify: bool,
        primitive: &dyn Fn(usize, usize) -> bool,
    ) -> Planned<'a> {
        let id = site.id;
        let reach_plan = |mutation: Mutation| SitePlan {
            gaps: vec![],
            constant: false,
            cannot_differ: false,
            run: vec![mutation],
            infected: Box::new(move |bin, _| bin.trace.reach.get(&id).cloned().unwrap_or_default()),
            reaching: Box::new(move |bin| bin.trace.reach.get(&id).cloned().unwrap_or_default()),
        };
        let reached = || {
            self.binaries
                .iter()
                .any(|b| b.trace.reach.contains_key(&id))
        };
        Planned::Plan(match &site.kind {
            SiteKind::Stmt
            | SiteKind::Neg { .. }
            | SiteKind::Field { .. }
            | SiteKind::Body { .. }
                if reached() =>
            {
                reach_plan(Mutation::Delete)
            }
            SiteKind::Stmt
            | SiteKind::Neg { .. }
            | SiteKind::Field { .. }
            | SiteKind::Body { .. } => return Planned::Unreached,
            SiteKind::Guard { can_be_false } => {
                let can_be_false = *can_be_false;
                let allowed = move |m: &Mutation| can_be_false || *m != Mutation::Const(false);
                let merged = self.merged(|t| t.returns.get(&id), plan::ReturnObs::merge);
                let guard_plan = plan::plan_return(&merged);
                if guard_plan.unreached {
                    return Planned::Unreached;
                }
                let gaps = guard_plan
                    .always
                    .filter(|always| allowed(&Mutation::Const(*always)))
                    .map(|always| PlannedGap {
                        finding: report::guard_gap_finding(file, site, always, verify),
                        survivors: vec![Mutation::Const(always)],
                        // The guard is the condition it consists of.
                        constant_expr: Some((site.start, site.end)),
                    })
                    .into_iter()
                    .collect();
                SitePlan {
                    constant: false,
                    cannot_differ: false,
                    gaps,
                    run: guard_plan.run.into_iter().filter(allowed).collect(),
                    infected: Box::new(move |bin, m| {
                        tests_where(bin.trace.returns.get(&id), |obs| match m {
                            Mutation::Const(value) => obs.infected_by_const(value),
                            _ => obs.reached(),
                        })
                    }),
                    reaching: Box::new(move |bin| {
                        tests_where(bin.trace.returns.get(&id), plan::ReturnObs::reached)
                    }),
                }
            }
            SiteKind::Arm { .. } if reached() => reach_plan(Mutation::Delete),
            SiteKind::Arm { .. } => return Planned::Unreached,
            SiteKind::Not { .. } if reached() => reach_plan(Mutation::DropNot),
            // `a..=b` with `b` the largest value of its type is no range the probe could
            // return in place of `a..b`: the mutant would run as the original and pass.
            SiteKind::Range { .. } if reached() => {
                if self
                    .binaries
                    .iter()
                    .any(|b| b.trace.ranges_at_top.contains(&id))
                {
                    return Planned::NotEvaluated;
                }
                reach_plan(Mutation::RangeEnd)
            }
            // `!` on an integer or a user type is not negation of a condition, and a range
            // of something other than integers has no "one more".
            SiteKind::Not { .. } | SiteKind::Range { .. } => {
                if self
                    .merged(|t| t.arith.get(&id), plan::ArithObs::merge)
                    .opaque
                {
                    return Planned::Nothing;
                }
                return Planned::Unreached;
            }
            SiteKind::Cmp {
                op,
                literal,
                lhs,
                rhs,
                gap: operator,
            } => {
                let op = *op;
                let mut merged = Obs::default();
                for bin in &self.binaries {
                    merged.merge(&bin.trace.merged(id));
                }
                // The types of the operands, where every binary that recorded them
                // agrees (a type can be another under `cfg(test)`).
                let mut recorded = self.binaries.iter().filter_map(|b| b.trace.bounds.get(&id));
                let bounds = recorded
                    .next()
                    .filter(|first| recorded.all(|other| other == *first));
                // Compared with a number, what cannot be every number has a gap only
                // where a value it can be lies between the values the tests gave it,
                // and another outcome only if a value it can be gives one.
                // Only for an operation of the language: one a type defines for itself
                // (`impl Mul<u32> for Eighths`) can give any number.
                let against = match literal {
                    LiteralSide::Rhs(number) => {
                        Some((lhs, (site.start, operator.0), *number, true))
                    }
                    LiteralSide::Lhs(number) => Some((rhs, (operator.1, site.end), *number, false)),
                    LiteralSide::None => None,
                }
                .and_then(|(expr, (from, to), number, is_rhs)| {
                    let (reach, shift) = instrument::reach(expr)?;
                    // A number past the values of the type it is compared with is
                    // another number there: nothing is read from how it is written.
                    let (low, high) = bounds.map(|b| if is_rhs { b.lhs } else { b.rhs })?;
                    if number < low || number > high {
                        return None;
                    }
                    // A shift by the width of its type or more is a shift by less when
                    // overflow is not checked: the type says by how much.
                    let reach = match shift {
                        Some(bits) => plan::Reach::of_shift(
                            bits,
                            bounds.map(|b| if is_rhs { b.lhs } else { b.rhs })?,
                        )?,
                        None => reach,
                    };
                    // A literal past the largest value of the type is another number
                    // there (`255i8` is -1 where that is allowed): nothing is read from it.
                    // Without the type nothing is read from it. A shift was checked
                    // against the width already (its stride need not be a value).
                    let fits = |value: i128| {
                        bounds.is_some_and(|b| value <= if is_rhs { b.lhs.1 } else { b.rhs.1 })
                    };
                    let (plan::Reach::Multiples(value)
                    | plan::Reach::Bits(value)
                    | plan::Reach::Below(value)) = reach;
                    ((shift.is_some() || fits(value)) && primitive(from, to))
                        .then_some((reach, number, is_rhs))
                });
                let mut domain = domain_of(*literal, bounds);
                // Between the least and the greatest value it can be: by what it is
                // (a mask, a remainder), or within its type (the multiples).
                let ends = against.and_then(|(reach, _, is_rhs)| {
                    reach.interval().or_else(|| {
                        let (low, high) = bounds.map(|b| if is_rhs { b.lhs } else { b.rhs })?;
                        reach.within(low, high)
                    })
                });
                if let Some((_, number, is_rhs)) = against
                    && let Some((low, high)) = ends
                    && let (Some(a), Some(b)) = if is_rhs {
                        (low.checked_sub(number), high.checked_sub(number))
                    } else {
                        (number.checked_sub(high), number.checked_sub(low))
                    }
                {
                    domain = Some(match domain {
                        Some((lo, hi)) => (lo.max(a), hi.min(b)),
                        None => (a, b),
                    });
                }
                let mut cmp_plan = plan::plan_cmp(op, &merged, domain);
                if let Some((reach, number, is_rhs)) = against
                    && let Some(gap) = cmp_plan.gap.take()
                {
                    // Equal to a number it cannot be: there is no other outcome.
                    if matches!(op, plan::CmpOp::Eq | plan::CmpOp::Ne)
                        && !reach.holds(number)
                        && matches!(gap, plan::InputGap::OneSided { .. })
                    {
                        cmp_plan.cannot_differ = true;
                    } else {
                        cmp_plan.gap = plan::within_reach(gap, op, &merged, reach, number, is_rhs);
                    }
                }
                let mut gaps = vec![];
                let mut constant = false;
                match &cmp_plan.gap {
                    Some(plan::InputGap::Unreached) => return Planned::Unreached,
                    Some(gap) => {
                        constant = matches!(gap, plan::InputGap::OneSided { .. });
                        if let Some(finding) =
                            report::input_gap_finding(file, site, gap, &merged, verify)
                        {
                            gaps.push(PlannedGap {
                                finding,
                                survivors: gap.survivors(),
                                constant_expr: None,
                            });
                        }
                    }
                    None => {}
                }
                SitePlan {
                    gaps,
                    constant,
                    cannot_differ: cmp_plan.cannot_differ,
                    run: cmp_plan.run,
                    infected: Box::new(move |bin, m| {
                        tests_where(bin.trace.cmp.get(&id), |obs| plan::infects(op, m, obs))
                    }),
                    reaching: Box::new(move |bin| {
                        tests_where(bin.trace.cmp.get(&id), Obs::reached)
                    }),
                }
            }
            SiteKind::Logic {
                and,
                lhs,
                rhs,
                gap: op_gap,
            } => {
                let and = *and;
                let lhs_range = core_range(site.start, lhs);
                let rhs_range = core_range(op_gap.1, rhs);
                let merged = self.merged(|t| t.logic.get(&id), plan::LogicObs::merge);
                let logic_plan = plan::plan_logic(and, &merged);
                if logic_plan.gaps.contains(&plan::LogicGap::Unreached) {
                    return Planned::Unreached;
                }
                let gaps: Vec<_> = logic_plan
                    .gaps
                    .iter()
                    .filter_map(|gap| {
                        // "The operand never decides" is the same statement as "the operand
                        // had one value whenever it was evaluated".
                        let (mutation, operand) = match gap {
                            plan::LogicGap::RhsNeverDecides => (Mutation::LogicDropRhs, rhs_range),
                            _ => (Mutation::LogicDropLhs, lhs_range),
                        };
                        Some(PlannedGap {
                            finding: report::logic_gap_finding(file, site, *gap, verify)?,
                            survivors: vec![mutation],
                            constant_expr: Some(operand),
                        })
                    })
                    .collect();
                SitePlan {
                    constant: merged.constant_value(and).is_some(),
                    cannot_differ: false,
                    gaps,
                    run: logic_plan.run,
                    infected: Box::new(move |bin, m| {
                        tests_where(bin.trace.logic.get(&id), |obs| {
                            plan::logic_infects(and, m, obs)
                        })
                    }),
                    reaching: Box::new(move |bin| {
                        tests_where(bin.trace.logic.get(&id), plan::LogicObs::reached)
                    }),
                }
            }
            SiteKind::Arith { .. } | SiteKind::MinMax { .. } | SiteKind::SliceEnd { .. } => {
                // `max` / `min` are recorded like an operator and its swap: they agree
                // exactly when the operands are equal.
                let merged = self.merged(|t| t.arith.get(&id), plan::ArithObs::merge);
                let mutations: Vec<(Mutation, plan::ArithObs)> = match &site.kind {
                    SiteKind::MinMax { .. } => vec![(Mutation::MinMax, merged)],
                    SiteKind::SliceEnd { .. } => vec![(Mutation::OtherEnd, merged)],
                    SiteKind::Arith { op, assign, .. } if op.second(*assign).is_some() => vec![
                        (Mutation::ArithSwap, merged),
                        (Mutation::ArithSwap2, merged.second()),
                    ],
                    _ => vec![(Mutation::ArithSwap, merged)],
                };
                // A `first()` that did not give the first element of the sequence is a
                // method of another name's making: its `last()` is anyone's guess.
                if matches!(site.kind, SiteKind::SliceEnd { .. }) && merged.opaque {
                    return Planned::Nothing;
                }
                let (mut gaps, mut run) = (vec![], vec![]);
                for (mutation, seen) in mutations {
                    match plan::plan_arith(&seen) {
                        plan::ArithPlan::Unreached => return Planned::Unreached,
                        // Not numbers (`String + &str`, `Duration + Duration`): nothing to mutate.
                        plan::ArithPlan::Opaque => return Planned::Nothing,
                        plan::ArithPlan::NeverDiffers => {
                            let finding = if mutation == Mutation::MinMax {
                                report::min_max_gap_finding(file, site, verify)
                            } else if mutation == Mutation::OtherEnd {
                                report::slice_end_gap_finding(file, site, verify)
                            } else {
                                report::arith_gap_finding(file, site, mutation, verify)
                            };
                            gaps.extend(finding.map(|finding| PlannedGap {
                                finding,
                                survivors: vec![mutation],
                                constant_expr: None,
                            }));
                        }
                        plan::ArithPlan::Run => run.push(mutation),
                    }
                }
                SitePlan {
                    gaps,
                    constant: false,
                    cannot_differ: false,
                    run,
                    infected: Box::new(move |bin, m| {
                        tests_where(bin.trace.arith.get(&id), |obs| obs.infected_by(m))
                    }),
                    reaching: Box::new(move |bin| {
                        tests_where(bin.trace.arith.get(&id), |obs| obs.same || obs.differs)
                    }),
                }
            }
            SiteKind::Return {
                sole_expr, option, ..
            } => {
                let sole_expr = *sole_expr;
                let is_option = option.is_some();
                let merged = self.merged(|t| t.returns.get(&id), plan::ReturnObs::merge);
                let return_plan = if is_option {
                    plan::plan_option_return(&merged)
                } else {
                    plan::plan_return(&merged)
                };
                if return_plan.unreached {
                    return Planned::Unreached;
                }
                let gaps: Vec<_> = return_plan
                    .always
                    .and_then(|always| {
                        Some(PlannedGap {
                            finding: report::return_gap_finding(file, site, always, verify)?,
                            survivors: vec![Mutation::Const(always)],
                            // A body that is one expression returns that expression's value.
                            constant_expr: sole_expr,
                        })
                    })
                    .into_iter()
                    .collect();
                SitePlan {
                    constant: false,
                    cannot_differ: false,
                    gaps,
                    run: return_plan.run,
                    infected: Box::new(move |bin, m| {
                        tests_where(bin.trace.returns.get(&id), |obs| match m {
                            Mutation::Const(value) => obs.infected_by_const(value),
                            _ => obs.reached(),
                        })
                    }),
                    reaching: Box::new(move |bin| {
                        tests_where(bin.trace.returns.get(&id), plan::ReturnObs::reached)
                    }),
                }
            }
        })
    }

    pub(super) fn job(
        &self,
        site: u32,
        mutation: Mutation,
        tests_of: &dyn Fn(&TestBinary) -> BTreeSet<String>,
    ) -> Job {
        let tests = self
            .binaries
            .iter()
            .enumerate()
            .filter_map(|(index, bin)| {
                let tests = tests_of(bin);
                if tests.is_empty() {
                    return None;
                }
                let filter = match bin.runner {
                    Runner::Exe(_) => test_filter(&tests, &bin.tests),
                    Runner::Doc { .. } | Runner::Isolated { .. } | Runner::Nextest { .. } => None,
                };
                Some((index, filter))
            })
            .collect();
        Job {
            site,
            mutation,
            tests,
        }
    }

    /// Runs one mutant against its tests, stopping at the first test that fails.
    /// Runs one mutant while `sharing` jobs run at the same time.
    pub(super) fn run_job(&self, job: &Job, sharing: usize) -> Result<JobResult> {
        let spec = format!("{}:{}", job.site, job.mutation.encode());
        let mut passed = vec![];
        // One failure is enough: the quick binaries first.
        let mut tests: Vec<&(usize, Option<Vec<String>>)> = job.tests.iter().collect();
        tests.sort_by_key(|(index, _)| self.binaries[*index].baseline);
        for (index, filter) in tests {
            let bin = &self.binaries[*index];
            let outcome = run_binary(
                bin,
                filter.as_deref(),
                &[("TMT_MUTANT", &spec)],
                bin.filtered_limit(filter.as_deref(), false),
                RunOutput::FirstFailure,
                sharing,
            )?;
            if outcome != Outcome::Pass {
                return Ok(JobResult {
                    first_pass_timeout: false,
                    unconfirmed_timeout: false,
                    survived: None,
                    timed_out: outcome == Outcome::Timeout,
                    unable: outcome == Outcome::Unable,
                });
            }
            match filter {
                Some(names) => passed.extend(names.iter().cloned()),
                None if matches!(bin.runner, Runner::Isolated { .. }) => {
                    passed.push(bin.name.clone())
                }
                None => passed.push(format!("all tests of `{}`", bin.name)),
            }
        }
        Ok(JobResult {
            first_pass_timeout: false,
            unconfirmed_timeout: false,
            survived: Some(passed),
            timed_out: false,
            unable: false,
        })
    }

    /// Runs a surviving mutant once more with tracing on, and returns what it changed in
    /// each test compared with the original run.
    pub(super) fn trace_job(
        &self,
        job: &Job,
        scratch: &Path,
        sharing: usize,
    ) -> Result<BTreeMap<String, BTreeSet<Change>>> {
        let spec = format!("{}:{}", job.site, job.mutation.encode());
        let mut all: BTreeMap<String, BTreeSet<Change>> = BTreeMap::new();
        for (index, filter) in &job.tests {
            let bin = &self.binaries[*index];
            let path = scratch.join(format!("effects-{}-{index}.tsv", job.site));
            let _ = fs::remove_file(&path);
            let trace_env = path.to_string_lossy().into_owned();
            let outcome = run_binary(
                bin,
                filter.as_deref(),
                &[("TMT_MUTANT", &spec), ("TMT_TRACE", &trace_env)],
                bin.filtered_limit(filter.as_deref(), true),
                RunOutput::Quiet,
                sharing,
            )?;
            let text = fs::read_to_string(&path).unwrap_or_default();
            let _ = fs::remove_file(&path);
            if outcome != Outcome::Pass {
                continue; // not the run we saw before: say nothing rather than guess
            }
            let mutated = Trace::parse(&text);
            let ran: Option<BTreeSet<String>> =
                filter.as_ref().map(|names| names.iter().cloned().collect());
            let baseline = bin
                .effects_baseline
                .get_or_init(|| propagation::BaselineIndex::new(&bin.trace));
            for (test, found) in baseline.changes(&mutated, ran.as_ref(), job.site) {
                all.entry(test).or_default().extend(found);
            }
        }
        Ok(all)
    }

    /// Runs every job exactly once on `workers` threads (formal/tla/JobPool.tla) and
    /// returns the results in job order.
    /// Runs the mutants, `workers` at a time. A mutant that ran out of time while others
    /// were running may only have been slowed down by them: it gets a second run on its
    /// own, and that one counts.
    pub(super) fn run_jobs(
        &self,
        jobs: &[Job],
        workers: usize,
        label: &str,
        checkpoint: Option<&Phase<'_>>,
    ) -> Result<Vec<JobResult>> {
        if self
            .binaries
            .first()
            .is_some_and(|bin| bin.timeout.is_some())
        {
            for (index, filter) in jobs.iter().flat_map(|job| &job.tests) {
                self.binaries[*index].warn_timeout_conflict(filter.as_deref());
            }
        }
        let run = |index: usize, sharing| self.run_job(&jobs[index], sharing);
        evaluate_jobs(
            jobs.len(),
            workers,
            self.serial_timeout_rechecks,
            self.progress,
            label,
            checkpoint,
            &run,
        )
    }
}

pub(super) fn evaluate_jobs(
    count: usize,
    workers: usize,
    serial_rechecks: bool,
    progress_enabled: bool,
    label: &str,
    checkpoint: Option<&Phase<'_>>,
    run: &(impl Fn(usize, usize) -> Result<JobResult> + Sync),
) -> Result<Vec<JobResult>> {
    let sharing = workers.clamp(1, count.max(1));
    let mut results = progress::track(progress_enabled, label, count, |progress| {
        run_pool(count, workers, |index| {
            if let Some(cached) = checkpoint
                .map(|phase| phase.get::<JobResult>(index))
                .transpose()?
                .flatten()
            {
                progress.complete(cached.progress_outcome());
                return Ok((cached, false));
            }
            let mut result = run(index, sharing)?;
            if sharing > 1 && result.timed_out {
                result.first_pass_timeout = true;
                result.unconfirmed_timeout = true;
            }
            let pending = result.unconfirmed_timeout && serial_rechecks;
            if !pending {
                if let Some(phase) = checkpoint {
                    phase.record(index, &result)?;
                }
                progress.complete(result.progress_outcome());
            }
            Ok((result, pending))
        })
    })?;
    let pending: Vec<_> = results
        .iter()
        .enumerate()
        .filter_map(|(index, (_, pending))| pending.then_some(index))
        .collect();
    if !pending.is_empty() && progress_enabled {
        eprintln!(
            "tmt: {label}: {} first-pass timeouts, {} pending serial rechecks",
            pending.len(),
            if serial_rechecks { pending.len() } else { 0 }
        );
    }
    if serial_rechecks {
        progress::track_timeout_rechecks(progress_enabled, pending.len(), |progress| {
            for index in pending {
                let mut confirmed = run(index, 1)?;
                confirmed.first_pass_timeout = true;
                let outcome = if confirmed.timed_out {
                    progress::Outcome::TimedOut
                } else if confirmed.unable {
                    progress::Outcome::Unable
                } else {
                    progress::Outcome::Survived
                };
                if let Some(phase) = checkpoint {
                    phase.record(index, &confirmed)?;
                }
                results[index] = (confirmed, false);
                progress.complete(outcome);
            }
            Ok::<_, anyhow::Error>(())
        })?;
    }
    if let Some(phase) = checkpoint {
        phase.finish()?;
    }
    Ok(results.into_iter().map(|(result, _)| result).collect())
}

impl std::str::FromStr for report::Shard {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let (index, count) = text.split_once('/').context("expected a shard index K/N")?;
        let index: usize = index.parse().context("invalid shard index")?;
        let count: usize = count.parse().context("invalid shard count")?;
        if index >= count {
            bail!("shard index must be less than the shard count");
        }
        Ok(Self {
            index,
            count,
            round_robin: false,
        })
    }
}

impl report::Shard {
    pub(super) fn contains(self, candidate: usize, candidates: usize) -> bool {
        candidate < candidates
            && (if self.round_robin {
                candidate % self.count
            } else {
                candidate / candidates.div_ceil(self.count)
            }) == self.index
    }
}
