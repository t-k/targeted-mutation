//! Turns planner results into findings a person can act on: which wrong implementation
//! survives, and which input or assertion would catch it.

use serde::Serialize;
use std::path::PathBuf;

use crate::instrument::{ArithOp, LiteralSide, Site, SiteKind};
use crate::plan::{CmpOp, InputGap, LogicGap, Mutation, Obs};

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Location {
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub code: String,
    /// The function or constant the code is in (`Store::commit`); empty where unknown.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub item: String,
    /// What the finding at this place is known by across runs: see [`finding_id`].
    #[serde(skip_serializing_if = "String::is_empty")]
    pub id: String,
}

/// The name a finding keeps from run to run: its kind, its file, the item it is in, its
/// code, and which of the places with that code in that item it is (0 for the first).
/// Lines are not in it, so code that moves keeps its name, and neither are the words
/// that describe the surviving mutants, so those can be improved.
pub fn finding_id(kind: &str, at: &Location, nth: usize) -> String {
    format!("{kind}|{}|{}|{}|{nth}", at.file, at.item, at.code)
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum Finding {
    /// No test input can tell these mutants from the original.
    InputGap {
        #[serde(flatten)]
        at: Location,
        /// Source-level form of mutants that pass every test.
        survivors: Vec<String>,
        /// How many mutants the gap lets through in total.
        survivor_count: u128,
        /// Inputs to add, in words.
        add_tests: Vec<String>,
        /// What the tests did feed the comparison.
        observed: Option<String>,
        /// Whether the tests were re-run against a survivor to confirm.
        verified: bool,
    },
    /// Tests execute the changed behaviour, and still pass.
    AssertionGap {
        #[serde(flatten)]
        at: Location,
        survivors: Vec<String>,
        /// Tests whose execution changed under the mutants but did not fail.
        tests: Vec<String>,
        /// What one surviving mutant changes later in those tests: the places an
        /// assertion could catch it. Empty when that was not looked into.
        effects: Vec<String>,
        /// The mutant whose effects those are, as it is written in `survivors`.
        effects_of: Option<String>,
    },
    /// A mutant that was built on its own passes every test, and nothing was recorded
    /// that says why: whether no test runs the code (nothing can be recorded in a
    /// `const fn`), or one does and does not look at the result.
    Survivor {
        #[serde(flatten)]
        at: Location,
        survivors: Vec<String>,
    },
}

impl Finding {
    pub fn at(&self) -> &Location {
        let (Finding::InputGap { at, .. }
        | Finding::AssertionGap { at, .. }
        | Finding::Survivor { at, .. }) = self;
        at
    }

    pub fn at_mut(&mut self) -> &mut Location {
        let (Finding::InputGap { at, .. }
        | Finding::AssertionGap { at, .. }
        | Finding::Survivor { at, .. }) = self;
        at
    }

    pub fn survivors(&self) -> &[String] {
        let (Finding::InputGap { survivors, .. }
        | Finding::AssertionGap { survivors, .. }
        | Finding::Survivor { survivors, .. }) = self;
        survivors
    }

    /// The kind as the JSON report and the names of findings spell it.
    pub fn kind(&self) -> &'static str {
        match self {
            Finding::InputGap { .. } => "input_gap",
            Finding::AssertionGap { .. } => "assertion_gap",
            Finding::Survivor { .. } => "survivor",
        }
    }
}

/// What decides which code cargo compiles, as given on the command line.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct CargoConfig {
    pub features: Vec<String>,
    pub all_features: bool,
    pub no_default_features: bool,
    pub locked: bool,
}

/// What a run looked at and how: a report says nothing about what is outside of it.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Analysis {
    /// The package's directory in its git repository (`crates/core/`; empty at the top).
    pub package_path: String,
    /// Only the code changed since this revision.
    pub since: Option<String>,
    /// Only these lines.
    pub lines: Vec<String>,
    /// The packages whose tests were run.
    pub tests_of: Vec<String>,
    pub cargo: CargoConfig,
    pub opt_level: Option<u8>,
    /// Mutants that need a build of their own were run.
    pub rebuild: bool,
    /// Predicted survivors were re-run.
    pub verified: bool,
    pub doc_tests: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub source_only: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_selection: Option<TestSelection>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub git_index: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub copy_fixtures: Vec<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct TestSelection {
    pub wrapper: Option<String>,
    pub tests: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Evaluation {
    NoSites,
    NoMutantsExecuted,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    ProbeCompileError,
    ProbeReplacedBySourceRebuild,
    RequiresSourceRebuild,
    UnparseableSource,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SkippedSite {
    #[serde(flatten)]
    pub at: Location,
    pub end_line: usize,
    pub end_column: usize,
    pub reason: SkipReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<String>,
}

impl SkippedSite {
    pub(crate) fn new(
        at: Location,
        source: &str,
        end: usize,
        reason: SkipReason,
        diagnostic: Option<String>,
    ) -> Self {
        let before = source.get(..end).unwrap_or(source);
        let end_line = crate::scope::line_of(before, before.len());
        let end_column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
        Self {
            at,
            end_line,
            end_column,
            reason,
            diagnostic,
        }
    }
}

/// What a run that was asked to check itself found (`--audit`).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Audit {
    /// Surviving mutants that were run once more, against every test of every binary.
    pub sampled: usize,
    /// How many surviving mutants there were to choose from.
    pub survivors: usize,
    /// Where one of those failed a test it had not been run against: the tests were not
    /// chosen as they should have been, or do not always do the same.
    pub disagreements: Vec<Location>,
    /// Test binaries whose second recorded run was not like the first, with the number
    /// of records (what one test saw at one site) that differ.
    pub trace_differences: Vec<TraceDifference>,
    /// Mutants that were run as probes and chosen to be built from the source as well.
    pub rebuilt: usize,
    /// Of those, the ones that built and whose two results could be compared.
    pub compared: usize,
    /// And the ones that did not build (or whose probe run could not be evaluated):
    /// nothing is known about them.
    pub not_compared: usize,
    /// Where the two did not agree on whether the tests pass.
    pub source_disagreements: Vec<SourceDisagreement>,
}

/// A mutant on which its probe and its source form do not agree.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct SourceDisagreement {
    #[serde(flatten)]
    pub at: Location,
    pub mutant: String,
    /// `passes` or `fails`: what the tests did with the probe, and with the source.
    pub as_probe: String,
    pub built: String,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct TraceDifference {
    pub binary: String,
    pub records: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Report {
    pub package: String,
    pub analysis: Analysis,
    /// Present when the run was asked to check itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audit: Option<Audit>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evaluation: Option<Evaluation>,
    pub sites: usize,
    /// Sites that could not be instrumented (the probe did not compile there).
    pub skipped_sites: usize,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkippedSite>,
    /// Values a function's body was to be replaced by that do not have its type
    /// (`Default::default()` for a type without a default): those mutants do not exist.
    pub unviable_mutants: usize,
    /// Mutants that need a build of their own and were not asked for (`--rebuild`).
    pub mutants_not_built: usize,
    /// Mutants that could not be run as what they are (a range that ends at the largest
    /// value of its type cannot be made to include it): neither killed nor survivors.
    pub mutants_not_evaluated: usize,
    pub mutants_run: usize,
    pub mutants_killed: usize,
    /// Killed mutants that were stopped by the timeout (typically an endless loop).
    pub mutants_timed_out: usize,
    pub findings: Vec<Finding>,
    /// Findings left out because of a `// tmt:ignore` comment or the baseline.
    pub suppressed: usize,
    /// Code no test executes.
    pub unreached: Vec<Location>,
    /// Survivors predicted from the trace that a re-run killed: the tests are not deterministic.
    pub unstable: Vec<Location>,
}

impl Report {
    /// Puts the findings in reading order. Sites are numbered as they are discovered, and
    /// a statement is discovered before the expressions of the statements above it.
    pub fn sort_findings(&mut self) {
        self.findings.sort_by(|a, b| {
            let key = |finding: &Finding| {
                let at = finding.at();
                (at.file.clone(), at.line, at.column)
            };
            key(a).cmp(&key(b))
        });
    }

    /// Whether the run found something to act on: a gap, or a prediction that did not
    /// hold (which means the report cannot be trusted as it stands).
    pub fn has_gaps(&self) -> bool {
        !self.findings.is_empty() || !self.unstable.is_empty()
    }
}

pub fn location(file: &str, site: &Site) -> Location {
    Location {
        file: file.to_string(),
        line: site.line,
        column: site.column,
        code: one_line(&site.text),
        ..Location::default()
    }
}

fn one_line(code: &str) -> String {
    let joined = code.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() > 100 {
        format!("{}...", joined.chars().take(97).collect::<String>())
    } else {
        joined
    }
}

/// Value of a float literal such as `30.5`, `1e3` or `2.5_f32`.
fn float_literal(text: &str) -> Option<f64> {
    let text = text.trim().replace('_', "");
    let text = text
        .strip_suffix("f32")
        .or(text.strip_suffix("f64"))
        .unwrap_or(&text);
    let looks_numeric = text.starts_with(|c: char| c.is_ascii_digit() || c == '-')
        && text
            .chars()
            .all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | '+' | 'e' | 'E'));
    if looks_numeric {
        text.parse().ok()
    } else {
        None
    }
}

/// Floats in reports: the shortest text that reads back as the same value, so nothing is
/// rounded away.
fn show_float(v: f64) -> String {
    format!("{v:?}")
}

/// Source-level form of a mutant.
pub fn describe(site: &Site, m: Mutation) -> String {
    match &site.kind {
        SiteKind::Stmt => format!("delete `{}`", one_line(&site.text)),
        SiteKind::Neg { operand, .. } => format!("`{}`", one_line(operand)),
        SiteKind::Guard { .. } => match m {
            Mutation::Const(value) => {
                format!("the guard `{}` replaced by `{value}`", one_line(&site.text))
            }
            _ => format!("another guard than `{}`", one_line(&site.text)),
        },
        SiteKind::Field { from_base, .. } => format!(
            "without the field `{}` (it is then `{from_base}`)",
            one_line(&site.text)
        ),
        SiteKind::Body { name, value, .. } => format!("the body of `{name}` replaced by `{value}`"),
        SiteKind::Not { operand, .. } => format!("`{}`", one_line(operand)),
        SiteKind::Return { name, option, .. } => match (m, option) {
            (Mutation::Const(false), Some(_)) => format!("`{name}` returning `None`"),
            (Mutation::Const(value), _) => format!("`{name}` returning `{value}`"),
            _ => format!("`{name}` returning something else"),
        },
        SiteKind::Range {
            inclusive,
            lhs,
            rhs,
            ..
        } => {
            let dots = if *inclusive { ".." } else { "..=" };
            format!("`{}{dots}{}`", one_line(lhs), one_line(rhs))
        }
        SiteKind::Arm { pattern, .. } => format!("remove the arm `{} => ..`", one_line(pattern)),
        SiteKind::MinMax {
            max,
            receiver,
            argument,
            ..
        } => {
            let other = if *max { "min" } else { "max" };
            format!("`{}.{other}({})`", one_line(receiver), one_line(argument))
        }
        SiteKind::Logic { and, lhs, rhs, .. } => {
            let (lhs, rhs) = (one_line(lhs), one_line(rhs));
            match m {
                // Both operands are still evaluated whenever the original evaluates them;
                // only the value of one is ignored. The forms below say exactly that.
                Mutation::LogicDropRhs if *and => format!("`{lhs} && {{ {rhs}; true }}`"),
                Mutation::LogicDropRhs => format!("`{lhs} || {{ {rhs}; false }}`"),
                Mutation::LogicDropLhs => format!("`{{ {lhs}; {rhs} }}`"),
                _ => format!("`{lhs} {} {rhs}`", if *and { "||" } else { "&&" }),
            }
        }
        SiteKind::Arith {
            op,
            assign,
            lhs,
            rhs,
            ..
        } => {
            let eq = if *assign { "=" } else { "" };
            let other = match m {
                Mutation::ArithSwap2 => op.swapped2().unwrap_or(op.swapped()),
                _ => op.swapped(),
            };
            format!(
                "`{} {}{eq} {}`",
                one_line(lhs),
                other.symbol(),
                one_line(rhs)
            )
        }
        SiteKind::Cmp {
            op,
            lhs,
            rhs,
            literal,
            ..
        } => {
            let (lhs, rhs) = (one_line(lhs), one_line(rhs));
            let sym = op.symbol();
            match m {
                Mutation::Const(c) => format!("`{c}`"),
                Mutation::Op(op2) => format!("`{lhs} {} {rhs}`", op2.symbol()),
                Mutation::Shift(d) => match literal {
                    LiteralSide::Rhs(k) => format!("`{lhs} {sym} {}`", k + d),
                    // k op rhs + d  <=>  k - d op rhs
                    LiteralSide::Lhs(k) => format!("`{} {sym} {rhs}`", k - d),
                    LiteralSide::None => match (op, d) {
                        (CmpOp::Ge, 1) => format!("`{lhs} > {rhs}`"),
                        (CmpOp::Gt, -1) => format!("`{lhs} >= {rhs}`"),
                        (CmpOp::Le, -1) => format!("`{lhs} < {rhs}`"),
                        (CmpOp::Lt, 1) => format!("`{lhs} <= {rhs}`"),
                        // Meant as arithmetic on whole numbers: in the operands' own
                        // type the addition could overflow, the mutant that was run cannot.
                        // tmt:ignore a shift of 0 is never planned, so `>=` is the same
                        (_, d) if d > 0 => format!("`{lhs} {sym} {rhs} + {d}` (without overflow)"),
                        (_, d) => format!("`{lhs} {sym} {rhs} - {}` (without overflow)", -d),
                    },
                },
                Mutation::ShiftF(d) => match (float_literal(&lhs), float_literal(&rhs)) {
                    (None, Some(k)) => format!("`{lhs} {sym} {}`", show_float(k + d)),
                    (Some(k), None) => format!("`{} {sym} {rhs}`", show_float(k - d)),
                    // tmt:ignore a shift of 0 is never planned, so any threshold near it is the same
                    _ if d > 0.0 => format!("`{lhs} {sym} {rhs} + {}`", show_float(d)),
                    _ => format!("`{lhs} {sym} {rhs} - {}`", show_float(-d)),
                },
                _ => format!("`!({lhs} {sym} {rhs})`"),
            }
        }
    }
}

/// The operand values that give `lhs - rhs = d`, in words.
fn input_for(lhs: &str, rhs: &str, literal: LiteralSide, d: i128) -> String {
    match (literal, d) {
        (LiteralSide::Rhs(k), d) => format!("`{lhs}` = {}", k + d),
        (LiteralSide::Lhs(k), d) => format!("`{rhs}` = {}", k - d),
        (LiteralSide::None, 0) => format!("`{lhs}` equal to `{rhs}`"),
        // tmt:ignore 0 is taken by the arm above, so `>=` is the same
        (LiteralSide::None, d) if d > 0 => format!("`{lhs}` = `{rhs}` + {d}"),
        (LiteralSide::None, d) => format!("`{lhs}` = `{rhs}` - {}", -d),
    }
}

fn observed_values(lhs: &str, rhs: &str, literal: LiteralSide, obs: &Obs) -> Option<String> {
    if !obs.valued() {
        return None;
    }
    let ds: Vec<i128> = [obs.max_neg, obs.eq.then_some(0), obs.min_pos]
        .into_iter()
        .flatten()
        .collect();
    let text = match literal {
        LiteralSide::Rhs(k) => {
            let values: Vec<String> = ds.iter().map(|d| (k + d).to_string()).collect();
            format!("`{lhs}` = {}", values.join(", "))
        }
        LiteralSide::Lhs(k) => {
            let mut values: Vec<i128> = ds.iter().map(|d| k - d).collect();
            values.sort();
            let values: Vec<String> = values.iter().map(i128::to_string).collect();
            format!("`{rhs}` = {}", values.join(", "))
        }
        LiteralSide::None => {
            let values: Vec<String> = ds.iter().map(i128::to_string).collect();
            format!("`{lhs}` - `{rhs}` = {}", values.join(", "))
        }
    };
    Some(format!("nearest values the tests passed: {text}"))
}

/// Builds the input-gap finding for a comparison site. `None` for gaps that are reported
/// elsewhere (unreached code).
pub fn input_gap_finding(
    file: &str,
    site: &Site,
    gap: &InputGap,
    obs: &Obs,
    verified: bool,
) -> Option<Finding> {
    let SiteKind::Cmp {
        op,
        lhs,
        rhs,
        literal,
        ..
    } = &site.kind
    else {
        return None;
    };
    let (lhs_text, rhs_text) = (one_line(lhs), one_line(rhs));
    // A float shift too small to move the literal is not a different program. The
    // literal moves the way `describe` writes it.
    let moves = |d: f64| match (float_literal(&lhs_text), float_literal(&rhs_text)) {
        (None, Some(k)) => k + d != k,
        (Some(k), None) => k - d != k,
        _ => true,
    };
    let survivors: Vec<String> = gap
        .survivors()
        .into_iter()
        .filter(|m| !matches!(m, Mutation::ShiftF(d) if !moves(*d)))
        .map(|m| describe(site, m))
        .collect();
    let mut observed = observed_values(&lhs_text, &rhs_text, *literal, obs);
    let (survivor_count, add_tests) = match gap {
        InputGap::Unreached => return None,
        InputGap::OneSided { always } => (
            1,
            vec![format!(
                "an input that makes `{}` {}",
                one_line(&site.text),
                !always
            )],
        ),
        InputGap::EqualNeverObserved { flipped, float_gap } => {
            if let Some((lo, hi)) = float_gap {
                observed = Some(match (float_literal(&lhs_text), float_literal(&rhs_text)) {
                    (None, Some(k)) => format!(
                        "nearest values the tests passed: `{lhs_text}` = {}, {}; any threshold between them passes",
                        show_float(k + lo),
                        show_float(k + hi)
                    ),
                    (Some(k), None) => format!(
                        "nearest values the tests passed: `{rhs_text}` = {}, {}; any threshold between them passes",
                        show_float(k - hi),
                        show_float(k - lo)
                    ),
                    _ => format!(
                        "nearest values the tests passed: `{lhs_text}` - `{rhs_text}` = {}, {}",
                        show_float(*lo),
                        show_float(*hi)
                    ),
                });
            }
            (
                survivors.len() as u128,
                vec![format!(
                    "`{lhs_text}` equal to `{rhs_text}` (the original gives {}, the mutant {})",
                    op.eval_d(0),
                    flipped.eval_d(0)
                )],
            )
        }
        InputGap::Boundary {
            min_shift,
            max_shift,
            missing,
        } => {
            let add = missing
                .iter()
                .map(|d| {
                    format!(
                        "{} (the original gives {})",
                        input_for(&lhs_text, &rhs_text, *literal, *d),
                        op.eval_d(*d)
                    )
                })
                .collect();
            ((max_shift - min_shift) as u128, add)
        }
    };
    Some(Finding::InputGap {
        at: location(file, site),
        survivors,
        survivor_count,
        add_tests,
        observed,
        verified,
    })
}

/// Input gap at `a && b` / `a || b`: one operand never decides the result in any test.
pub fn logic_gap_finding(
    file: &str,
    site: &Site,
    gap: LogicGap,
    verified: bool,
) -> Option<Finding> {
    let SiteKind::Logic { and, lhs, rhs, .. } = &site.kind else {
        return None;
    };
    let (lhs, rhs) = (one_line(lhs), one_line(rhs));
    let (mutation, add) = match (gap, and) {
        (LogicGap::Unreached, _) => return None,
        (LogicGap::RhsNeverDecides, true) => (
            Mutation::LogicDropRhs,
            format!("a case where `{lhs}` is true and `{rhs}` is false"),
        ),
        (LogicGap::RhsNeverDecides, false) => (
            Mutation::LogicDropRhs,
            format!("a case where `{lhs}` is false and `{rhs}` is true"),
        ),
        (LogicGap::LhsNeverDecides, true) => (
            Mutation::LogicDropLhs,
            format!("a case where `{lhs}` is false"),
        ),
        (LogicGap::LhsNeverDecides, false) => (
            Mutation::LogicDropLhs,
            format!("a case where `{lhs}` is true"),
        ),
    };
    Some(Finding::InputGap {
        at: location(file, site),
        survivors: vec![describe(site, mutation)],
        survivor_count: 1,
        add_tests: vec![add],
        observed: None,
        verified,
    })
}

/// Input gap at `a.max(b)` / `a.min(b)`: the two operands were equal in every test.
pub fn min_max_gap_finding(file: &str, site: &Site, verified: bool) -> Option<Finding> {
    let SiteKind::MinMax {
        receiver, argument, ..
    } = &site.kind
    else {
        return None;
    };
    Some(Finding::InputGap {
        at: location(file, site),
        survivors: vec![describe(site, Mutation::MinMax)],
        survivor_count: 1,
        add_tests: vec![format!(
            "`{}` different from `{}`",
            one_line(receiver),
            one_line(argument)
        )],
        observed: None,
        verified,
    })
}

/// Input gap at a `-> bool` function that returned the same value in every test.
pub fn return_gap_finding(
    file: &str,
    site: &Site,
    always: bool,
    verified: bool,
) -> Option<Finding> {
    let SiteKind::Return { name, option, .. } = &site.kind else {
        return None;
    };
    let other = match (option, always) {
        (None, always) => format!("{}", !always),
        (Some(_), false) => "`Some`".to_string(),
        (Some(_), true) => "`None`".to_string(),
    };
    Some(Finding::InputGap {
        at: location(file, site),
        survivors: vec![describe(site, Mutation::Const(always))],
        survivor_count: 1,
        add_tests: vec![format!("a case where `{name}` returns {other}")],
        observed: None,
        verified,
    })
}

/// Input gap at the guard of a match arm that had the same value in every test.
pub fn guard_gap_finding(file: &str, site: &Site, always: bool, verified: bool) -> Finding {
    Finding::InputGap {
        at: location(file, site),
        survivors: vec![describe(site, Mutation::Const(always))],
        survivor_count: 1,
        add_tests: vec![format!(
            "a case where the guard `{}` is {}",
            one_line(&site.text),
            !always
        )],
        observed: None,
        verified,
    }
}

/// Input gap at an arithmetic operator: the swapped operator computes the same value on
/// every pair of operands the tests pass.
pub fn arith_gap_finding(
    file: &str,
    site: &Site,
    mutation: Mutation,
    verified: bool,
) -> Option<Finding> {
    let SiteKind::Arith {
        op,
        assign,
        lhs,
        rhs,
        ..
    } = &site.kind
    else {
        return None;
    };
    let (lhs, rhs) = (one_line(lhs), one_line(rhs));
    let eq = if *assign { "=" } else { "" };
    let other = match mutation {
        Mutation::ArithSwap2 => op.second(*assign)?,
        _ => op.swapped(),
    };
    let add = match (op, other) {
        (ArithOp::Add, ArithOp::Sub) | (ArithOp::Sub, ArithOp::Add) => {
            format!("`{rhs}` other than 0")
        }
        _ => format!(
            "operands for which `{lhs} {}{eq} {rhs}` and `{lhs} {}{eq} {rhs}` differ",
            op.symbol(),
            other.symbol()
        ),
    };
    Some(Finding::InputGap {
        at: location(file, site),
        survivors: vec![describe(site, mutation)],
        survivor_count: 1,
        add_tests: vec![add],
        observed: None,
        verified,
    })
}

/// The report as SARIF 2.1.0, for code scanning UIs. `prefix` is put in front of the file
/// names (the package's path inside the repository).
/// SARIF with the files named from the top of the repository.
pub fn render_sarif(report: &Report) -> String {
    let prefix = report.analysis.package_path.as_str();
    let results: Vec<serde_json::Value> = report
        .findings
        .iter()
        .map(|finding| {
            let (kind, at) = finding_parts(finding);
            let rule = match kind {
                "INPUT GAP" => "input-gap",
                "ASSERTION GAP" => "assertion-gap",
                _ => "survivor",
            };
            let text = format!("`{}`: {}", at.code, finding_body(finding).join("; "));
            serde_json::json!({
                "ruleId": rule,
                "level": "warning",
                "message": { "text": text },
                // What the service tells a finding of one run from that of another by.
                "partialFingerprints": { "tmtFindingId/v2": at.id },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": format!("{prefix}{}", at.file) },
                        "region": { "startLine": at.line, "startColumn": at.column }
                    }
                }]
            })
        })
        .collect();
    let sarif = serde_json::json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": { "driver": {
                "name": "tmt",
                "rules": [
                    { "id": "input-gap", "shortDescription": { "text": "No test input tells this mutant from the original" } },
                    { "id": "assertion-gap", "shortDescription": { "text": "Tests run the mutated code and still pass" } },
                    { "id": "survivor", "shortDescription": { "text": "A mutant built on its own passes every test; why is not known" } }
                ]
            } },
            // What was looked at: a run of a part says nothing about the rest.
            "properties": { "analysis": report.analysis, "evaluation": report.evaluation, "skipped": report.skipped },
            "results": results
        }]
    });
    serde_json::to_string_pretty(&sarif).expect("JSON values serialise")
}

fn list(items: &[String], limit: usize) -> String {
    let shown: Vec<&str> = items.iter().take(limit).map(String::as_str).collect();
    let more = items.len().saturating_sub(limit);
    if more > 0 {
        format!("{} (+{more} more)", shown.join(", "))
    } else {
        shown.join(", ")
    }
}

/// The lines of one finding, without its header.
fn finding_body(finding: &Finding) -> Vec<String> {
    let mut lines = vec![];
    match finding {
        Finding::InputGap {
            survivors,
            survivor_count,
            add_tests,
            observed,
            verified,
            ..
        } => {
            let more = survivor_count.saturating_sub(survivors.len() as u128);
            let more = if more > 0 {
                format!(" (+{more} more)")
            } else {
                String::new()
            };
            let note = if *verified {
                "re-run: tests pass"
            } else {
                "from the trace, not re-run"
            };
            lines.push(format!(
                "passes every test as: {}{more}  [{note}]",
                survivors.join(", ")
            ));
            lines.extend(observed.iter().cloned());
            lines.extend(
                add_tests
                    .iter()
                    .map(|test| format!("add a test with {test}")),
            );
        }
        Finding::AssertionGap {
            survivors,
            tests,
            effects,
            effects_of,
            ..
        } => {
            lines.push(format!("passes every test as: {}", survivors.join(", ")));
            lines.push(format!(
                "these tests run the changed code and still pass: {}",
                list(tests, 5)
            ));
            if effects.is_empty() {
                lines.push("add an assertion on what this code is responsible for".to_string());
            } else {
                // Which of several mutants was followed, so that what is said of "the
                // mutant" below can be tried.
                if let (Some(followed), true) = (effects_of, survivors.len() > 1) {
                    lines.push(format!(
                        "followed one of them through the tests: {followed}"
                    ));
                }
                lines.extend(effects.iter().cloned());
            }
        }
        Finding::Survivor { survivors, .. } => {
            lines.push(format!("passes every test as: {}", survivors.join(", ")));
            lines.push(
                "built on its own, where nothing can be recorded: either no test runs this code, or one does and does not check what it gives"
                    .to_string(),
            );
        }
    }
    lines
}

fn finding_parts(finding: &Finding) -> (&'static str, &Location) {
    match finding {
        Finding::InputGap { at, .. } => ("INPUT GAP", at),
        Finding::AssertionGap { at, .. } => ("ASSERTION GAP", at),
        Finding::Survivor { at, .. } => ("SURVIVOR", at),
    }
}

pub fn render_text(report: &Report, show_unreached: bool) -> String {
    let mut out = String::new();
    let mut previous: Option<(&str, usize)> = None;
    for finding in &report.findings {
        let (kind, at) = finding_parts(finding);
        // Findings on one line are usually one thought (a chain of conditions): they are
        // printed as one block.
        let same_line = previous == Some((at.file.as_str(), at.line));
        if same_line {
            out.push_str(&format!(
                "  + {kind}  column {}\n    {}\n",
                at.column, at.code
            ));
        } else {
            if previous.is_some() {
                out.push('\n');
            }
            out.push_str(&format!(
                "{kind}  {}:{}:{}\n    {}\n",
                at.file, at.line, at.column, at.code
            ));
        }
        for line in finding_body(finding) {
            out.push_str(&format!("  {line}\n"));
        }
        previous = Some((at.file.as_str(), at.line));
    }
    if previous.is_some() {
        out.push('\n');
    }
    if show_unreached {
        for at in &report.unreached {
            out.push_str(&format!(
                "UNREACHED  {}:{}:{}\n    {}\n\n",
                at.file, at.line, at.column, at.code
            ));
        }
    }
    for at in &report.unstable {
        out.push_str(&format!(
            "UNSTABLE  {}:{}:{}\n    {}\n  the trace predicted a survivor but the re-run failed: the tests are not deterministic, or do something the trace cannot see\n\n",
            at.file, at.line, at.column, at.code
        ));
    }
    let input = report
        .findings
        .iter()
        .filter(|f| matches!(f, Finding::InputGap { .. }))
        .count();
    let assertion = report
        .findings
        .iter()
        .filter(|f| matches!(f, Finding::AssertionGap { .. }))
        .count();
    let unexplained = report.findings.len() - input - assertion;
    // A mutant stopped by the time limit costs the limit: worth knowing when a run is slow.
    let by_time_limit = if report.mutants_timed_out > 0 {
        format!(", {} of them by the time limit", report.mutants_timed_out)
    } else {
        String::new()
    };
    out.push_str(&format!(
        "{}: {} sites, {} input gaps, {} assertion gaps, {} mutants run ({} killed{by_time_limit}), {} sites unreached",
        report.package,
        report.sites,
        input,
        assertion,
        report.mutants_run,
        report.mutants_killed,
        report.unreached.len()
    ));
    if unexplained > 0 {
        out.push_str(&format!(
            ", {unexplained} survivors of mutants built on their own"
        ));
    }
    if report.suppressed > 0 {
        out.push_str(&format!(", {} findings suppressed", report.suppressed));
    }
    if report.skipped_sites > 0 {
        out.push_str(&format!(
            ", {} sites skipped (probe did not compile)",
            report.skipped_sites
        ));
    }
    if report.unviable_mutants > 0 {
        out.push_str(&format!(
            ", {} mutants left out (they do not compile)",
            report.unviable_mutants
        ));
    }
    out.push('\n');
    if let Some(audit) = &report.audit {
        out.push_str(&format!(
            "audit: {} of {} surviving mutants run against all tests, {} failed one they had not been run against\n",
            audit.sampled,
            audit.survivors,
            audit.disagreements.len()
        ));
        for at in &audit.disagreements {
            out.push_str(&format!(
                "  {}:{}:{}  {}\n",
                at.file, at.line, at.column, at.code
            ));
        }
        if audit.rebuilt > 0 {
            out.push_str(&format!(
                "audit: {} mutants chosen to be built from the source as well: {} compared, {} with another result than as a probe, {} that could not be compared\n",
                audit.rebuilt,
                audit.compared,
                audit.source_disagreements.len(),
                audit.not_compared
            ));
        }
        for differs in &audit.source_disagreements {
            out.push_str(&format!(
                "  {}:{}:{}  {}: {} as a probe, {} built\n",
                differs.at.file,
                differs.at.line,
                differs.at.column,
                differs.mutant,
                differs.as_probe,
                differs.built
            ));
        }
        if audit.trace_differences.is_empty() {
            out.push_str("audit: recorded once more, the tests did the same\n");
        }
        for binary in &audit.trace_differences {
            out.push_str(&format!(
                "audit: recorded once more, `{}` differs in {} records: its tests do not always do the same\n",
                binary.binary, binary.records
            ));
        }
    }
    if report.mutants_not_evaluated > 0 {
        out.push_str(&format!(
            "({} mutants not evaluated: a range that ends at the largest value of its type cannot be made to include it)\n",
            report.mutants_not_evaluated
        ));
    }
    if report.mutants_not_built > 0 {
        out.push_str(&format!(
            "({} more mutants need a build of their own: --rebuild runs them)\n",
            report.mutants_not_built
        ));
    }
    if !report.skipped.is_empty() {
        out.push_str(&format!(
            "({} skipped entries with locations and reasons in --json)\n",
            report.skipped.len()
        ));
    }
    if report.evaluation == Some(Evaluation::NoMutantsExecuted) {
        out.push_str("No mutants executed. A subprocess that clears the environment cannot report probes; use --source-only on the affected lines if needed.\n");
    }
    if !show_unreached && !report.unreached.is_empty() {
        out.push_str("(--show-unreached lists the code no test executes)\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::discover;

    fn results_of(sarif: &serde_json::Value) -> &Vec<serde_json::Value> {
        sarif["runs"][0]["results"].as_array().unwrap()
    }

    fn or_site() -> Site {
        site_of("fn f(a: bool, b: bool) -> bool { a || b }")
    }

    fn assign_kind() -> SiteKind {
        SiteKind::Arith {
            op: ArithOp::Add,
            assign: true,
            lhs: "*total".into(),
            rhs: "fee".into(),
            gap: (0, 0),
        }
    }

    fn site_of(src: &str) -> Site {
        discover(src, 0)
            .unwrap()
            .sites
            .into_iter()
            .find(|s| !matches!(s.kind, SiteKind::Return { .. } | SiteKind::Body { .. }))
            .unwrap()
    }

    #[test]
    fn describes_mutants_at_source_level() {
        let rhs = site_of("fn f(age: u32) -> bool { age >= 18 }");
        assert_eq!(describe(&rhs, Mutation::Shift(1)), "`age >= 19`");
        assert_eq!(describe(&rhs, Mutation::Shift(-2)), "`age >= 16`");
        assert_eq!(describe(&rhs, Mutation::Negate), "`!(age >= 18)`");
        assert_eq!(describe(&rhs, Mutation::Const(false)), "`false`");
        assert_eq!(describe(&rhs, Mutation::Op(CmpOp::Gt)), "`age > 18`");

        // 18 <= age shifted by +1 is 18 <= age + 1, i.e. 17 <= age.
        let lhs = site_of("fn f(age: i32) -> bool { 18 <= age }");
        assert_eq!(describe(&lhs, Mutation::Shift(1)), "`17 <= age`");

        let none = site_of("fn f(a: u32, b: u32) -> bool { a >= b }");
        assert_eq!(describe(&none, Mutation::Shift(1)), "`a > b`");
        assert_eq!(
            describe(&none, Mutation::Shift(2)),
            "`a >= b + 2` (without overflow)"
        );
        assert_eq!(
            describe(&none, Mutation::Shift(-1)),
            "`a >= b - 1` (without overflow)"
        );
        let lt = site_of("fn f(a: u32, b: u32) -> bool { a < b }");
        assert_eq!(describe(&lt, Mutation::Shift(1)), "`a <= b`");

        let stmt = site_of("fn f(s: &mut S) { s.check(\n  1,\n  2); }");
        assert_eq!(
            describe(&stmt, Mutation::Delete),
            "delete `s.check( 1, 2);`"
        );
    }

    /// The source-level form must mean the same as the planner's `Shift`.
    #[test]
    fn literal_shift_rendering_matches_shift_semantics() {
        use crate::plan::eval_mutated_d;
        for op in [CmpOp::Lt, CmpOp::Le, CmpOp::Gt, CmpOp::Ge] {
            for shift in [-2, -1, 1, 2] {
                for other in -5..=5i128 {
                    let k = 1i128;
                    // literal on the right: `x op k+shift`
                    assert_eq!(
                        op.eval_d(other - (k + shift)),
                        eval_mutated_d(op, Mutation::Shift(shift), other - k)
                    );
                    // literal on the left: `k-shift op x`
                    assert_eq!(
                        op.eval_d((k - shift) - other),
                        eval_mutated_d(op, Mutation::Shift(shift), k - other)
                    );
                }
            }
        }
        // And the operator flips used when neither side is a literal.
        for (op, shift, flipped) in [
            (CmpOp::Ge, 1, CmpOp::Gt),
            (CmpOp::Gt, -1, CmpOp::Ge),
            (CmpOp::Le, -1, CmpOp::Lt),
            (CmpOp::Lt, 1, CmpOp::Le),
        ] {
            for d in -3..=3 {
                assert_eq!(
                    flipped.eval_d(d),
                    eval_mutated_d(op, Mutation::Shift(shift), d)
                );
            }
        }
    }

    #[test]
    fn boundary_finding_for_the_age_check() {
        let site = site_of("fn f(age: u32) -> bool { age >= 18 }");
        let mut obs = Obs::of_d(-1);
        obs.merge(&Obs::of_d(2));
        let gap = InputGap::Boundary {
            min_shift: 0,
            max_shift: 2,
            missing: vec![0],
        };
        let finding = input_gap_finding("src/lib.rs", &site, &gap, &obs, true).unwrap();
        assert_eq!(
            finding,
            Finding::InputGap {
                at: Location {
                    file: "src/lib.rs".into(),
                    line: 1,
                    column: 26,
                    code: "age >= 18".into(),
                    ..Location::default()
                },
                survivors: vec!["`age >= 19`".into(), "`age >= 20`".into()],
                survivor_count: 2,
                add_tests: vec!["`age` = 18 (the original gives true)".into()],
                observed: Some("nearest values the tests passed: `age` = 17, 20".into()),
                verified: true,
            }
        );
        let report = Report {
            package: "p".into(),
            sites: 1,
            findings: vec![finding],
            ..Report::default()
        };
        let text = render_text(&report, false);
        assert!(text.contains("INPUT GAP  src/lib.rs:1:26"), "{text}");
        assert!(
            text.contains("passes every test as: `age >= 19`, `age >= 20`  [re-run: tests pass]"),
            "{text}"
        );
        assert!(
            text.contains("add a test with `age` = 18 (the original gives true)"),
            "{text}"
        );
        assert!(
            text.contains("p: 1 sites, 1 input gaps, 0 assertion gaps"),
            "{text}"
        );
        assert!(report.has_gaps());
    }

    #[test]
    fn other_gap_kinds() {
        let site = site_of("fn f(a: f64, b: f64) -> bool { a < b }");
        let mut obs = Obs::of_rel(crate::plan::Rel::Less);
        obs.merge(&Obs::of_rel(crate::plan::Rel::Greater));
        let gap = InputGap::EqualNeverObserved {
            flipped: CmpOp::Le,
            float_gap: None,
        };
        let Some(Finding::InputGap {
            survivors,
            add_tests,
            observed,
            ..
        }) = input_gap_finding("f.rs", &site, &gap, &obs, false)
        else {
            panic!()
        };
        assert_eq!(survivors, ["`a <= b`"]);
        assert_eq!(
            add_tests,
            ["`a` equal to `b` (the original gives false, the mutant true)"]
        );
        assert_eq!(observed, None);

        let Some(Finding::InputGap {
            survivors,
            add_tests,
            ..
        }) = input_gap_finding(
            "f.rs",
            &site,
            &InputGap::OneSided { always: true },
            &obs,
            false,
        )
        else {
            panic!()
        };
        assert_eq!(survivors, ["`true`"]);
        assert_eq!(add_tests, ["an input that makes `a < b` false"]);

        assert_eq!(
            input_gap_finding("f.rs", &site, &InputGap::Unreached, &obs, false),
            None
        );
    }

    #[test]
    fn float_gap_finding() {
        let site = site_of("fn f(celsius: f64) -> bool { celsius > 30.5 }");
        let mut obs = Obs::of_f(-20.5);
        obs.merge(&Obs::of_f(9.5));
        let gap = InputGap::EqualNeverObserved {
            flipped: CmpOp::Ge,
            float_gap: obs.float_gap(),
        };
        let Some(Finding::InputGap {
            survivors,
            add_tests,
            observed,
            survivor_count,
            ..
        }) = input_gap_finding("f.rs", &site, &gap, &obs, true)
        else {
            panic!()
        };
        assert_eq!(
            survivors,
            [
                "`celsius >= 30.5`",
                "`celsius > 35.25`",
                "`celsius > 20.25`"
            ]
        );
        assert_eq!(survivor_count, 3);
        assert_eq!(
            add_tests,
            ["`celsius` equal to `30.5` (the original gives false, the mutant true)"]
        );
        assert_eq!(
            observed.as_deref(),
            Some(
                "nearest values the tests passed: `celsius` = 10.0, 40.0; any threshold between them passes"
            )
        );

        let lhs = site_of("fn f(x: f32) -> bool { 1.5_f32 <= x }");
        assert_eq!(describe(&lhs, Mutation::ShiftF(0.25)), "`1.25 <= x`");
        let none = site_of("fn f(x: f64, y: f64) -> bool { x < y }");
        assert_eq!(describe(&none, Mutation::ShiftF(0.5)), "`x < y + 0.5`");
        assert_eq!(describe(&none, Mutation::ShiftF(-0.5)), "`x < y - 0.5`");
        let finding = input_gap_finding(
            "f.rs",
            &none,
            &InputGap::EqualNeverObserved {
                flipped: CmpOp::Le,
                float_gap: Some((-1.0, 2.0)),
            },
            &obs,
            false,
        );
        let Some(Finding::InputGap { observed, .. }) = finding else {
            panic!()
        };
        assert_eq!(
            observed.as_deref(),
            Some("nearest values the tests passed: `x` - `y` = -1.0, 2.0")
        );

        assert_eq!(float_literal("30.5"), Some(30.5));
        assert_eq!(float_literal("-1e3"), Some(-1000.0));
        assert_eq!(float_literal("1_000.5f64"), Some(1000.5));
        assert_eq!(float_literal("limit"), None);
        assert_eq!(float_literal("x1"), None);
        assert_eq!(float_literal("1x"), None);
        assert_eq!(float_literal("-x"), None);
        assert_eq!(float_literal("-2.5"), Some(-2.5));
        assert_eq!(float_literal("e"), None);
        // Nothing is rounded away: a tiny shift stays visible.
        assert_eq!(show_float(0.1 + 0.2), "0.30000000000000004");
        assert_eq!(show_float(1e-10), "1e-10");
        assert_eq!(show_float(10.0), "10.0");
        assert_eq!(show_float(35.25), "35.25");
        // A shift that does not change the literal is not listed as a mutant.
        let big = site_of("fn f(x: f64) -> bool { x > 1e300 }");
        let tiny = InputGap::EqualNeverObserved {
            flipped: CmpOp::Ge,
            float_gap: Some((-2e-300, 2e-300)),
        };
        let Some(Finding::InputGap { survivors, .. }) =
            input_gap_finding("f.rs", &big, &tiny, &obs, true)
        else {
            panic!()
        };
        assert_eq!(survivors, ["`x >= 1e300`"]);
    }

    #[test]
    fn logic_and_arithmetic_findings() {
        let and = site_of("fn f(a: bool, b: bool) -> bool { a && b }");
        assert_eq!(describe(&and, Mutation::LogicSwap), "`a || b`");
        assert_eq!(describe(&and, Mutation::LogicDropRhs), "`a && { b; true }`");
        assert_eq!(
            describe(&or_site(), Mutation::LogicDropRhs),
            "`a || { b; false }`"
        );
        assert_eq!(describe(&and, Mutation::LogicDropLhs), "`{ a; b }`");
        let or = site_of("fn f(a: bool, b: bool) -> bool { a || b }");
        assert_eq!(describe(&or, Mutation::LogicSwap), "`a && b`");
        let advice = |site: &Site, gap| match logic_gap_finding("f.rs", site, gap, true) {
            Some(Finding::InputGap {
                survivors,
                add_tests,
                ..
            }) => (survivors[0].clone(), add_tests[0].clone()),
            _ => panic!(),
        };
        assert_eq!(
            advice(&and, LogicGap::RhsNeverDecides),
            (
                "`a && { b; true }`".into(),
                "a case where `a` is true and `b` is false".into()
            )
        );
        assert_eq!(
            advice(&and, LogicGap::LhsNeverDecides),
            ("`{ a; b }`".into(), "a case where `a` is false".into())
        );
        assert_eq!(
            advice(&or, LogicGap::RhsNeverDecides),
            (
                "`a || { b; false }`".into(),
                "a case where `a` is false and `b` is true".into()
            )
        );
        assert_eq!(
            advice(&or, LogicGap::LhsNeverDecides),
            ("`{ a; b }`".into(), "a case where `a` is true".into())
        );
        assert_eq!(
            logic_gap_finding("f.rs", &and, LogicGap::Unreached, true),
            None
        );
        assert_eq!(
            arith_gap_finding("f.rs", &and, Mutation::ArithSwap, true),
            None
        );

        let add = site_of("fn f(total: u32, fee: u32) -> u32 { total + fee }");
        assert_eq!(describe(&add, Mutation::ArithSwap), "`total - fee`");
        let Some(Finding::InputGap {
            survivors,
            add_tests,
            ..
        }) = arith_gap_finding("f.rs", &add, Mutation::ArithSwap, true)
        else {
            panic!()
        };
        assert_eq!(
            (survivors[0].as_str(), add_tests[0].as_str()),
            ("`total - fee`", "`fee` other than 0")
        );
        let mul = site_of("fn f(n: u32, scale: u32) -> u32 { n * scale }");
        let Some(Finding::InputGap {
            survivors,
            add_tests,
            ..
        }) = arith_gap_finding("f.rs", &mul, Mutation::ArithSwap, true)
        else {
            panic!()
        };
        assert_eq!(survivors[0], "`n / scale`");
        assert_eq!(
            add_tests[0],
            "operands for which `n * scale` and `n / scale` differ"
        );
        let assign = Site {
            kind: assign_kind(),
            ..add.clone()
        };
        assert_eq!(describe(&assign, Mutation::ArithSwap), "`*total -= fee`");
        let Some(Finding::InputGap { add_tests, .. }) =
            arith_gap_finding("f.rs", &assign, Mutation::ArithSwap, true)
        else {
            panic!()
        };
        assert_eq!(add_tests[0], "`fee` other than 0");
        let shift = Site {
            kind: SiteKind::Arith {
                op: ArithOp::Shl,
                assign: true,
                lhs: "bits".into(),
                rhs: "n".into(),
                gap: (0, 0),
            },
            ..assign.clone()
        };
        assert_eq!(describe(&shift, Mutation::ArithSwap), "`bits >>= n`");
        let Some(Finding::InputGap { add_tests, .. }) =
            arith_gap_finding("f.rs", &shift, Mutation::ArithSwap, true)
        else {
            panic!()
        };
        assert_eq!(
            add_tests[0],
            "operands for which `bits <<= n` and `bits >>= n` differ"
        );

        let not = Site {
            kind: SiteKind::Not {
                bang: (0, 1),
                operand: "is_ready(x)".into(),
            },
            ..assign.clone()
        };
        assert_eq!(describe(&not, Mutation::DropNot), "`is_ready(x)`");
        let ret = Site {
            kind: SiteKind::Return {
                name: "is_ready".into(),
                option: None,
                sole_expr: None,
            },
            ..assign.clone()
        };
        assert_eq!(
            describe(&ret, Mutation::Const(true)),
            "`is_ready` returning `true`"
        );
        let Some(Finding::InputGap {
            survivors,
            add_tests,
            ..
        }) = return_gap_finding("f.rs", &ret, false, true)
        else {
            panic!()
        };
        assert_eq!(survivors[0], "`is_ready` returning `false`");
        assert_eq!(add_tests[0], "a case where `is_ready` returns true");
        assert_eq!(return_gap_finding("f.rs", &not, false, true), None);

        let find = Site {
            kind: SiteKind::Return {
                name: "find".into(),
                option: Some("Option<&str>".into()),
                sole_expr: None,
            },
            ..assign.clone()
        };
        assert_eq!(
            describe(&find, Mutation::Const(false)),
            "`find` returning `None`"
        );
        let Some(Finding::InputGap { add_tests, .. }) =
            return_gap_finding("f.rs", &find, false, true)
        else {
            panic!()
        };
        assert_eq!(add_tests[0], "a case where `find` returns `Some`");
        let range = Site {
            kind: SiteKind::Range {
                inclusive: false,
                lhs: "0".into(),
                rhs: "n".into(),
                gap: (0, 0),
            },
            ..assign.clone()
        };
        assert_eq!(describe(&range, Mutation::RangeEnd), "`0..=n`");
        let closed = Site {
            kind: SiteKind::Range {
                inclusive: true,
                lhs: "1".into(),
                rhs: "n".into(),
                gap: (0, 0),
            },
            ..assign.clone()
        };
        assert_eq!(describe(&closed, Mutation::RangeEnd), "`1..n`");
        let arm = Site {
            kind: SiteKind::Arm {
                pattern: "Kind::Update { key, .. }".into(),
                guard: crate::instrument::ArmGuard::None(0),
            },
            ..assign.clone()
        };
        assert_eq!(
            describe(&arm, Mutation::Delete),
            "remove the arm `Kind::Update { key, .. } => ..`"
        );

        let clamp = Site {
            kind: SiteKind::MinMax {
                max: true,
                receiver: "drift".into(),
                argument: "peak.get()".into(),
                call: (0, 0),
                close: (0, 0),
            },
            ..assign.clone()
        };
        assert_eq!(
            describe(&clamp, Mutation::MinMax),
            "`drift.min(peak.get())`"
        );
        let Some(Finding::InputGap { add_tests, .. }) = min_max_gap_finding("f.rs", &clamp, true)
        else {
            panic!()
        };
        assert_eq!(add_tests[0], "`drift` different from `peak.get()`");
        assert_eq!(min_max_gap_finding("f.rs", &assign, true), None);

        let rem = site_of("fn f(n: u32, k: u32) -> u32 { n % k }");
        assert_eq!(describe(&rem, Mutation::ArithSwap), "`n / k`");
        assert_eq!(
            logic_gap_finding("f.rs", &add, LogicGap::RhsNeverDecides, true),
            None
        );
    }

    #[test]
    fn long_code_is_shortened() {
        let exactly_100 = "x".repeat(100);
        assert_eq!(one_line(&exactly_100), exactly_100);
        let shortened = one_line(&"x".repeat(101));
        assert_eq!(shortened, format!("{}...", "x".repeat(97)));
        assert_eq!(one_line("a\n   b"), "a b");
    }

    #[test]
    fn at_most_three_survivors_are_listed_nearest_first() {
        let site = site_of("fn f(a: i64) -> bool { a >= 10 }");
        let obs = Obs::default();
        let survivors = |min_shift, max_shift| {
            let gap = InputGap::Boundary {
                min_shift,
                max_shift,
                missing: vec![0],
            };
            match input_gap_finding("f.rs", &site, &gap, &obs, false) {
                Some(Finding::InputGap {
                    survivors,
                    survivor_count,
                    ..
                }) => (survivors, survivor_count),
                _ => panic!(),
            }
        };
        assert_eq!(
            survivors(-9, 9),
            (
                vec!["`a >= 11`".into(), "`a >= 9`".into(), "`a >= 12`".into()],
                18
            )
        );
        assert_eq!(survivors(-1, 0), (vec!["`a >= 9`".into()], 1));
        assert_eq!(survivors(0, 1), (vec!["`a >= 11`".into()], 1));
        assert_eq!(
            survivors(-2, 0),
            (vec!["`a >= 9`".into(), "`a >= 8`".into()], 2)
        );
    }

    #[test]
    fn text_report_is_exact() {
        let at = Location {
            file: "src/lib.rs".into(),
            line: 3,
            column: 9,
            code: "a >= b".into(),
            ..Location::default()
        };
        let below = Location {
            file: "src/lib.rs".into(),
            line: 7,
            column: 5,
            code: "s.reset();".into(),
            ..Location::default()
        };
        let report = Report {
            package: "p".into(),
            sites: 2,
            mutants_run: 1,
            suppressed: 2,
            findings: vec![
                Finding::InputGap {
                    at: at.clone(),
                    survivors: vec!["`a > b`".into()],
                    survivor_count: 4,
                    add_tests: vec!["`a` equal to `b` (the original gives true)".into()],
                    observed: Some("nearest values the tests passed: `a` - `b` = -1, 4".into()),
                    verified: false,
                },
                Finding::InputGap {
                    at: at.clone(),
                    survivors: vec!["`true`".into()],
                    survivor_count: 1,
                    add_tests: vec![],
                    observed: None,
                    verified: true,
                },
                Finding::AssertionGap {
                    at: Location { column: 4, ..at },
                    survivors: vec!["`!(a >= b)`".into()],
                    tests: vec!["t".into()],
                    effects: vec![],
                    effects_of: None,
                },
                Finding::AssertionGap {
                    at: below,
                    survivors: vec!["delete `s.reset();`".into()],
                    tests: vec!["t".into()],
                    effects: vec![
                        "with the mutant, `t` no longer runs `s.count = 0;` (src/lib.rs:12)".into(),
                    ],
                    effects_of: None,
                },
            ],
            ..Report::default()
        };
        // Findings on one line form one block; a new line starts a new block.
        assert_eq!(
            render_text(&report, false),
            "INPUT GAP  src/lib.rs:3:9\n    a >= b\n\
             \x20 passes every test as: `a > b` (+3 more)  [from the trace, not re-run]\n\
             \x20 nearest values the tests passed: `a` - `b` = -1, 4\n\
             \x20 add a test with `a` equal to `b` (the original gives true)\n\
             \x20 + INPUT GAP  column 9\n    a >= b\n\
             \x20 passes every test as: `true`  [re-run: tests pass]\n\
             \x20 + ASSERTION GAP  column 4\n    a >= b\n\
             \x20 passes every test as: `!(a >= b)`\n\
             \x20 these tests run the changed code and still pass: t\n\
             \x20 add an assertion on what this code is responsible for\n\
             \n\
             ASSERTION GAP  src/lib.rs:7:5\n    s.reset();\n\
             \x20 passes every test as: delete `s.reset();`\n\
             \x20 these tests run the changed code and still pass: t\n\
             \x20 with the mutant, `t` no longer runs `s.count = 0;` (src/lib.rs:12)\n\
             \n\
             p: 2 sites, 2 input gaps, 2 assertion gaps, 1 mutants run (0 killed), 0 sites unreached, 2 findings suppressed\n"
        );
        assert_eq!(
            render_text(
                &Report {
                    package: "q".into(),
                    ..Report::default()
                },
                false
            ),
            "q: 0 sites, 0 input gaps, 0 assertion gaps, 0 mutants run (0 killed), 0 sites unreached\n"
        );

        let mut in_repository = report.clone();
        in_repository.analysis.package_path = "crates/p/".into();
        in_repository.analysis.since = Some("abc".into());
        if let Finding::AssertionGap { at, .. } = &mut in_repository.findings[3] {
            at.id = "assertion_gap|src/lib.rs|f|s.reset();|0".into();
        }
        let sarif: serde_json::Value = serde_json::from_str(&render_sarif(&in_repository)).unwrap();
        assert_eq!(
            results_of(&sarif)[3]["partialFingerprints"]["tmtFindingId/v2"],
            "assertion_gap|src/lib.rs|f|s.reset();|0"
        );
        assert_eq!(sarif["runs"][0]["properties"]["analysis"]["since"], "abc");
        assert_eq!(sarif["version"], "2.1.0");
        let results = sarif["runs"][0]["results"].as_array().unwrap();
        assert_eq!(results.len(), 4);
        assert_eq!(results[0]["ruleId"], "input-gap");
        assert_eq!(results[3]["ruleId"], "assertion-gap");
        let location = &results[3]["locations"][0]["physicalLocation"];
        assert_eq!(location["artifactLocation"]["uri"], "crates/p/src/lib.rs");
        assert_eq!(
            (
                &location["region"]["startLine"],
                &location["region"]["startColumn"]
            ),
            (&serde_json::json!(7), &serde_json::json!(5))
        );
        assert!(
            results[3]["message"]["text"]
                .as_str()
                .unwrap()
                .starts_with("`s.reset();`: passes every test as: delete `s.reset();`; ")
        );
        let rules = sarif["runs"][0]["tool"]["driver"]["rules"]
            .as_array()
            .unwrap();
        assert_eq!(rules.len(), 3);
    }

    #[test]
    fn a_mutant_built_on_its_own_is_a_survivor_without_a_cause() {
        let survivor = Finding::Survivor {
            at: Location {
                file: "src/lib.rs".into(),
                line: 2,
                column: 1,
                code: "const fn limit() -> u8".into(),
                ..Location::default()
            },
            survivors: vec!["the body of `limit` replaced by `0`".into()],
        };
        assert_eq!(survivor.kind(), "survivor");
        assert_eq!(survivor.at().line, 2);
        assert_eq!(survivor.survivors().len(), 1);
        let report = Report {
            package: "p".into(),
            findings: vec![survivor],
            ..Report::default()
        };
        // Neither of the two gaps: it is counted and named as what it is.
        assert_eq!(
            render_text(&report, false),
            "SURVIVOR  src/lib.rs:2:1\n    const fn limit() -> u8\n\
             \x20 passes every test as: the body of `limit` replaced by `0`\n\
             \x20 built on its own, where nothing can be recorded: either no test runs this code, or one does and does not check what it gives\n\
             \n\
             p: 0 sites, 0 input gaps, 0 assertion gaps, 0 mutants run (0 killed), 0 sites unreached, 1 survivors of mutants built on their own\n"
        );
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["findings"][0]["kind"], "survivor");
        let sarif: serde_json::Value = serde_json::from_str(&render_sarif(&report)).unwrap();
        assert_eq!(results_of(&sarif)[0]["ruleId"], "survivor");
        assert!(report.has_gaps());
    }

    #[test]
    fn non_literal_inputs_in_words() {
        assert_eq!(
            input_for("a", "b", LiteralSide::None, 0),
            "`a` equal to `b`"
        );
        assert_eq!(input_for("a", "b", LiteralSide::None, 1), "`a` = `b` + 1");
        assert_eq!(input_for("a", "b", LiteralSide::None, -1), "`a` = `b` - 1");
        assert_eq!(
            input_for("18", "age", LiteralSide::Lhs(18), -1),
            "`age` = 19"
        );
    }

    #[test]
    fn text_report_sections() {
        let at = Location {
            file: "src/lib.rs".into(),
            line: 3,
            column: 9,
            code: "s.check()?;".into(),
            ..Location::default()
        };
        let report = Report {
            package: "p".into(),
            analysis: Analysis::default(),
            audit: None,
            evaluation: None,
            sites: 4,
            skipped_sites: 1,
            skipped: vec![],
            unviable_mutants: 2,
            mutants_not_built: 0,
            mutants_not_evaluated: 0,
            mutants_run: 3,
            mutants_killed: 2,
            mutants_timed_out: 0,
            suppressed: 0,
            findings: vec![Finding::AssertionGap {
                at: at.clone(),
                survivors: vec!["delete `s.check()?;`".into()],
                tests: (0..7).map(|i| format!("tests::t{i}")).collect(),
                effects: vec![],
                effects_of: None,
            }],
            unreached: vec![at.clone()],
            unstable: vec![at],
        };
        let text = render_text(&report, false);
        assert!(
            text.contains("ASSERTION GAP  src/lib.rs:3:9\n    s.check()?;"),
            "{text}"
        );
        assert!(text.contains("tests::t4 (+2 more)"), "{text}");
        assert!(text.contains("UNSTABLE  src/lib.rs:3:9"), "{text}");
        assert!(!text.contains("UNREACHED"), "{text}");
        assert!(
            text.contains("3 mutants run (2 killed), 1 sites unreached, 1 sites skipped"),
            "{text}"
        );
        assert!(
            text.contains(", 2 mutants left out (they do not compile)\n"),
            "{text}"
        );
        assert!(text.contains("--show-unreached"), "{text}");
        let listed = render_text(&report, true);
        assert!(listed.contains("UNREACHED  src/lib.rs:3:9"));
        assert!(!listed.contains("--show-unreached"), "{listed}");
        let one_more = Report {
            findings: vec![Finding::InputGap {
                at: report.unreached[0].clone(),
                survivors: vec!["`a`".into()],
                survivor_count: 2,
                add_tests: vec![],
                observed: None,
                verified: true,
            }],
            ..report.clone()
        };
        assert!(render_text(&one_more, false).contains("`a` (+1 more)"));
        let nothing_unreached = Report {
            unreached: vec![],
            ..report.clone()
        };
        assert!(!render_text(&nothing_unreached, false).contains("--show-unreached"));
        assert!(!Report::default().has_gaps());
    }
    #[test]
    fn unstable_predictions_alone_are_something_to_act_on() {
        let at = Location {
            file: "src/lib.rs".into(),
            line: 1,
            column: 1,
            code: "a > b".into(),
            ..Location::default()
        };
        assert!(!Report::default().has_gaps());
        assert!(
            Report {
                unstable: vec![at],
                ..Report::default()
            }
            .has_gaps()
        );
    }

    #[test]
    fn what_counts_as_a_float_literal() {
        assert_eq!(float_literal("30.5"), Some(30.5));
        assert_eq!(float_literal(" 2.5_f32 "), Some(2.5));
        assert_eq!(float_literal("-1e3"), Some(-1000.0));
        // These parse as floats too, and are not literals: a variable called `inf`, and
        // forms Rust does not accept.
        for text in ["inf", "nan", "-inf", ".5", "+5", "e5", "limit", ""] {
            assert_eq!(float_literal(text), None, "{text}");
        }
    }

    #[test]
    fn a_float_shift_that_does_not_move_the_literal_is_dropped() {
        // At 2^53 the next float below is 1 away and the next above 2 away.
        let survivors_of = |source: &str| {
            let site = site_of(source);
            let mut obs = Obs::of_f(-4.0);
            obs.merge(&Obs::of_f(4.0));
            let gap = InputGap::EqualNeverObserved {
                flipped: CmpOp::Ge,
                float_gap: obs.float_gap(),
            };
            match input_gap_finding("f.rs", &site, &gap, &obs, true) {
                Some(Finding::InputGap {
                    survivors,
                    survivor_count,
                    ..
                }) => (survivors, survivor_count),
                _ => panic!(),
            }
        };
        // Shifts of +2 and -2 are proposed. Both move the literal.
        assert_eq!(
            survivors_of("fn f(x: f64) -> bool { x > 9007199254740992.0 }"),
            (
                vec![
                    "`x >= 9007199254740992.0`".to_string(),
                    "`x > 9007199254740994.0`".to_string(),
                    "`x > 9007199254740990.0`".to_string(),
                ],
                3
            )
        );
        // At 2^54 only the step down is a different number, and on the left of the
        // comparison the literal moves the other way.
        assert_eq!(
            survivors_of("fn f(x: f64) -> bool { x > 18014398509481984.0 }"),
            (
                vec![
                    "`x >= 18014398509481984.0`".to_string(),
                    "`x > 1.8014398509481982e16`".to_string(),
                ],
                2
            )
        );
        assert_eq!(
            survivors_of("fn f(x: f64) -> bool { 18014398509481984.0 > x }"),
            (
                vec![
                    "`18014398509481984.0 >= x`".to_string(),
                    "`1.8014398509481982e16 > x`".to_string(),
                ],
                2
            )
        );
        // With the literal on the left, the nearest values are those of the other side.
        let site = site_of("fn f(x: f64) -> bool { 1.5 > x }");
        let mut obs = Obs::of_f(-1.0);
        obs.merge(&Obs::of_f(2.0));
        let gap = InputGap::EqualNeverObserved {
            flipped: CmpOp::Ge,
            float_gap: obs.float_gap(),
        };
        let Some(Finding::InputGap { observed, .. }) =
            input_gap_finding("f.rs", &site, &gap, &obs, true)
        else {
            panic!()
        };
        assert_eq!(
            observed.as_deref(),
            Some(
                "nearest values the tests passed: `x` = -0.5, 2.5; any threshold between them passes"
            )
        );
        // Far above, neither does.
        assert_eq!(
            survivors_of("fn f(x: f64) -> bool { x > 1e30 }"),
            (vec!["`x >= 1e30`".to_string()], 1)
        );
    }

    #[test]
    fn the_effects_are_said_of_the_mutant_they_were_seen_with() {
        let gap = |survivors: &[&str], effects_of: Option<&str>| Finding::AssertionGap {
            at: Location {
                file: "src/lib.rs".into(),
                line: 1,
                column: 1,
                code: "a < b".into(),
                ..Location::default()
            },
            survivors: survivors.iter().map(|s| s.to_string()).collect(),
            tests: vec!["tests::t".into()],
            effects: vec!["with the mutant, `tests::t` now runs `x;` (src/lib.rs:2)".into()],
            effects_of: effects_of.map(str::to_string),
        };
        // Of several mutants one was followed: the report says which.
        let body = finding_body(&gap(&["`a <= b`", "`a == b`"], Some("`a <= b`")));
        assert_eq!(
            body[2..],
            [
                "followed one of them through the tests: `a <= b`",
                "with the mutant, `tests::t` now runs `x;` (src/lib.rs:2)"
            ]
        );
        // A single mutant needs no saying, and nothing is said where nothing is known.
        for single in [
            gap(&["`a <= b`"], Some("`a <= b`")),
            gap(&["`a <= b`", "`a == b`"], None),
        ] {
            let body = finding_body(&single);
            assert_eq!(body.len(), 3, "{body:?}");
            assert!(body[2].starts_with("with the mutant"), "{body:?}");
        }
    }

    #[test]
    fn an_audit_is_said_in_the_summary() {
        let mut report = Report {
            package: "p".into(),
            audit: Some(Audit {
                sampled: 2,
                survivors: 7,
                ..Audit::default()
            }),
            ..Report::default()
        };
        let text = render_text(&report, false);
        assert!(
            text.ends_with(
                "\naudit: 2 of 7 surviving mutants run against all tests, 0 failed one they had not been run against\n\
                 audit: recorded once more, the tests did the same\n"
            ),
            "{text}"
        );
        let audit = report.audit.as_mut().unwrap();
        audit.disagreements.push(Location {
            file: "src/lib.rs".into(),
            line: 3,
            column: 5,
            code: "x > 5".into(),
            ..Location::default()
        });
        audit.trace_differences.push(TraceDifference {
            binary: "p".into(),
            records: 4,
        });
        let text = render_text(&report, false);
        assert!(
            text.ends_with(
                "all tests, 1 failed one they had not been run against\n\
                 \x20 src/lib.rs:3:5  x > 5\n\
                 audit: recorded once more, `p` differs in 4 records: its tests do not always do the same\n"
            ),
            "{text}"
        );
        // Mutants built from the source are said only where some were, with what differs.
        assert!(!text.contains("built from the source"), "{text}");
        let audit = report.audit.as_mut().unwrap();
        audit.rebuilt = 1;
        audit.not_compared = 1;
        let text = render_text(&report, false);
        assert!(
            text.contains(
                "\naudit: 1 mutants chosen to be built from the source as well: 0 compared, 0 with another result than as a probe, 1 that could not be compared\n"
            ),
            "{text}"
        );
        let audit = report.audit.as_mut().unwrap();
        audit.rebuilt = 5;
        audit.compared = 4;
        audit.source_disagreements.push(SourceDisagreement {
            at: Location {
                file: "src/lib.rs".into(),
                line: 9,
                column: 14,
                code: "note(asked)".into(),
                ..Location::default()
            },
            mutant: "the guard `note(asked)` replaced by `true`".into(),
            as_probe: "passes".into(),
            built: "fails".into(),
        });
        let text = render_text(&report, false);
        assert!(
            text.contains(
                "\naudit: 5 mutants chosen to be built from the source as well: 4 compared, 1 with another result than as a probe, 1 that could not be compared\n\
                 \x20 src/lib.rs:9:14  the guard `note(asked)` replaced by `true`: passes as a probe, fails built\n"
            ),
            "{text}"
        );
        assert!(!render_text(&Report::default(), false).contains("audit"));
    }

    #[test]
    fn lists_and_counts_at_their_limits() {
        let items: Vec<String> = ["a", "b", "c"].iter().map(|s| s.to_string()).collect();
        assert_eq!(list(&items, 3), "a, b, c");
        assert_eq!(list(&items, 2), "a, b (+1 more)");
        assert_eq!(list(&items, 1), "a (+2 more)");
        let one = Report {
            package: "p".into(),
            suppressed: 1,
            unviable_mutants: 1,
            ..Report::default()
        };
        assert!(
            render_text(&one, false).contains(", 1 mutants left out (they do not compile)"),
            "{}",
            render_text(&one, false)
        );
        assert!(!render_text(&Report::default(), false).contains("left out"));
        assert!(!render_text(&one, false).contains("--rebuild"));
        assert!(!render_text(&one, false).contains("not evaluated"));
        let at_top = Report {
            mutants_not_evaluated: 1,
            ..Report::default()
        };
        assert!(
            render_text(&at_top, false).ends_with(
                "\n(1 mutants not evaluated: a range that ends at the largest value of its type cannot be made to include it)\n"
            ),
            "{}",
            render_text(&at_top, false)
        );
        let quick = Report {
            mutants_not_built: 1,
            ..Report::default()
        };
        assert!(
            render_text(&quick, false)
                .ends_with("\n(1 more mutants need a build of their own: --rebuild runs them)\n"),
            "{}",
            render_text(&quick, false)
        );
        let slow = Report {
            mutants_run: 5,
            mutants_killed: 4,
            mutants_timed_out: 2,
            ..Report::default()
        };
        assert!(
            render_text(&slow, false)
                .contains("5 mutants run (4 killed, 2 of them by the time limit), 0 sites"),
            "{}",
            render_text(&slow, false)
        );
        assert!(!render_text(&one, false).contains("time limit"));
        let once = Report {
            mutants_timed_out: 1,
            ..slow.clone()
        };
        assert!(
            render_text(&once, false).contains("(4 killed, 1 of them by the time limit)"),
            "{}",
            render_text(&once, false)
        );
        assert!(
            render_text(&one, false).contains(", 1 findings suppressed"),
            "{}",
            render_text(&one, false)
        );
        assert!(!render_text(&Report::default(), false).contains("suppressed"));
    }

    #[test]
    fn findings_are_put_in_reading_order() {
        let gap = |file: &str, line: usize, column: usize, survivor: &str| Finding::AssertionGap {
            at: Location {
                file: file.into(),
                line,
                column,
                code: "x".into(),
                ..Location::default()
            },
            survivors: vec![survivor.into()],
            tests: vec![],
            effects: vec![],
            effects_of: None,
        };
        let mut report = Report {
            findings: vec![
                gap("src/b.rs", 1, 1, "b"),
                gap("src/a.rs", 9, 5, "statement"),
                gap("src/a.rs", 7, 2, "above it"),
                gap("src/a.rs", 9, 5, "its operator"),
                gap("src/a.rs", 9, 3, "left of both"),
            ],
            ..Report::default()
        };
        report.sort_findings();
        let order: Vec<&str> = report
            .findings
            .iter()
            .map(|f| match f {
                Finding::AssertionGap { survivors, .. } => survivors[0].as_str(),
                _ => panic!(),
            })
            .collect();
        // Findings at one position keep the order they were made in.
        assert_eq!(
            order,
            ["above it", "left of both", "statement", "its operator", "b"]
        );
    }
    #[test]
    fn second_replacements_guards_fields_and_bodies_in_words() {
        let all = |src: &str| discover(src, 0).unwrap().sites;
        let add = site_of("fn f(total: u32, fee: u32) -> u32 { total + fee }");
        assert_eq!(describe(&add, Mutation::ArithSwap2), "`total * fee`");
        let Some(Finding::InputGap {
            survivors,
            add_tests,
            ..
        }) = arith_gap_finding("f.rs", &add, Mutation::ArithSwap2, true)
        else {
            panic!()
        };
        assert_eq!(survivors, ["`total * fee`"]);
        assert_eq!(
            add_tests,
            ["operands for which `total + fee` and `total * fee` differ"]
        );
        let sites = all("fn f(bits: &mut u8, mask: u8) { *bits ^= mask; *bits <<= 1 }");
        let arith: Vec<&Site> = sites
            .iter()
            .filter(|s| matches!(s.kind, SiteKind::Arith { .. }))
            .collect();
        assert_eq!(describe(arith[0], Mutation::ArithSwap), "`*bits &= mask`");
        assert_eq!(describe(arith[0], Mutation::ArithSwap2), "`*bits |= mask`");
        let collect = site_of("fn f(bits: &mut u8, mask: u8) { *bits |= mask }");
        assert_eq!(
            arith_gap_finding("f.rs", &collect, Mutation::ArithSwap2, true),
            None
        );
        // A shift has one replacement: no second finding.
        assert_eq!(
            arith_gap_finding("f.rs", arith[1], Mutation::ArithSwap2, true),
            None
        );

        let sites = all("fn f(k: Option<i32>, base: O) -> O { \
             let d = match k { Some(n) if n > 1 => -n, _ => 0 }; O { depth: d, ..base } }");
        let of = |pick: &dyn Fn(&SiteKind) -> bool| sites.iter().find(|s| pick(&s.kind)).unwrap();
        let guard = of(&|k| matches!(k, SiteKind::Guard { .. }));
        assert_eq!(
            describe(guard, Mutation::Const(true)),
            "the guard `n > 1` replaced by `true`"
        );
        assert_eq!(
            guard_gap_finding("f.rs", guard, false, true),
            Finding::InputGap {
                at: location("f.rs", guard),
                survivors: vec!["the guard `n > 1` replaced by `false`".into()],
                survivor_count: 1,
                add_tests: vec!["a case where the guard `n > 1` is true".into()],
                observed: None,
                verified: true,
            }
        );
        let neg = of(&|k| matches!(k, SiteKind::Neg { .. }));
        assert_eq!(describe(neg, Mutation::Delete), "`n`");
        let field = of(&|k| matches!(k, SiteKind::Field { .. }));
        assert_eq!(
            describe(field, Mutation::Delete),
            "without the field `depth: d` (it is then `(base).depth`)"
        );
        let body = of(&|k| matches!(k, SiteKind::Body { .. }));
        assert_eq!(
            describe(body, Mutation::Delete),
            "the body of `f` replaced by `Default::default()`"
        );
        assert_eq!(
            location("f.rs", body).code,
            "fn f(k: Option<i32>, base: O) -> O"
        );
    }
}
