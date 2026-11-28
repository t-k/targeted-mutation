//! Turns planner results into findings a person can act on: which wrong implementation
//! survives, and which input or assertion would catch it.

use serde::Serialize;
use std::path::PathBuf;

use crate::instrument::{ArithOp, LiteralSide, Site, SiteKind};
use crate::plan::{CmpOp, InputGap, LogicGap, Mutation, Obs};

/// Top-level JSON/JSONL schema version emitted by `tmt`.
pub const SCHEMA_VERSION: u32 = 1;

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
        #[serde(skip_serializing_if = "Option::is_none")]
        witness: Option<VerifiedWitness>,
    },
    /// A mutant that was built on its own passes every test, and nothing was recorded
    /// that says why: whether no test runs the code (nothing can be recorded in a
    /// `const fn`), or one does and does not look at the result.
    Survivor {
        #[serde(flatten)]
        at: Location,
        survivors: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        witness: Option<VerifiedWitness>,
        /// Set when the mutant is a change undone (`--since`): the revision whose code
        /// was put back.
        #[serde(skip_serializing_if = "Option::is_none")]
        change_since: Option<String>,
        /// What to add, when the kind of mutant says it and nothing was verified.
        #[serde(skip_serializing_if = "Option::is_none")]
        advice: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct VerifiedWitness {
    pub call: String,
    pub original: String,
    pub mutant: String,
    pub assertion: String,
    /// When the original and mutant returned the same multi-field struct differing on one
    /// field (recursively), the narrowed dotted field path (e.g. `"fee"` or `"inner.fee"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guidance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub individually_killed: Option<usize>,
    /// The test that makes the call, where the values are those the call has there:
    /// after what that test does before it. In a test of its own it may return another.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_test: Option<String>,
    /// The line of the finding's file on which that test makes the call.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_test_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub in_test_column: Option<usize>,
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

    pub(crate) fn set_witness(&mut self, evidence: VerifiedWitness) {
        match self {
            Self::AssertionGap { witness, .. } | Self::Survivor { witness, .. } => {
                if witness.is_none() {
                    *witness = Some(evidence);
                }
            }
            Self::InputGap { .. } => {}
        }
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
pub struct Nextest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
pub struct Shard {
    pub index: usize,
    pub count: usize,
    pub round_robin: bool,
}

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_timeout_seconds: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_jobs: Option<usize>,
    pub timeout_recheck: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wrapper_grace_seconds: Option<u64>,
    /// Mutants that need a build of their own were run.
    pub rebuild: bool,
    /// Predicted survivors were re-run.
    pub verified: bool,
    pub doc_tests: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub source_only: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub literal_operators: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shard: Option<Shard>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nextest: Option<Nextest>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub error_values: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test_selection: Option<TestSelection>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub git_index: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub copy_fixtures: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub copy_fixture_excludes: Vec<PathBuf>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub output_fixtures: Vec<crate::output_fixture::Fixture>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub stage_external: Vec<crate::driver::StageExternal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<PathBuf>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct TestSelection {
    pub build_scope: TestBuildScope,
    pub wrapper: Option<String>,
    pub tests: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TestBuildScope {
    #[default]
    PackageWide,
    SelectedTargets,
}

/// Structured diagnostic written when a baseline run exceeds its time budget.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct BaselineTimeoutDiagnostic {
    pub schema_version: u32,
    pub package: String,
    pub stage: &'static str,
    pub runner: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub test: Option<String>,
    pub elapsed_seconds: f64,
    pub budget_seconds: u64,
    pub completed_baselines: usize,
    pub total_baselines: usize,
    pub partial_baseline: bool,
    pub log: PathBuf,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lines: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rerun_args: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude_args: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Evaluation {
    NoSites,
    NoMutantsExecuted,
    /// At least one mutant ran, but other mutants were left unbuilt (`--rebuild`),
    /// could not be evaluated, had skipped probes, or had unconfirmed timeouts.
    Partial,
}

/// Partition of [`Report::mutants_survived`] by what happened to each surviving mutant.
///
/// The fields are mutually exclusive and exhaustive:
/// `reported + suppressed + redundant_constant + preexisting_unchanged + unchanged_not_compared + unconfirmed_gap == mutants_survived`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SurvivorBreakdown {
    /// Surviving mutants emitted in [`Report::findings`] (after suppressions).
    /// Note that one finding can group multiple surviving mutants at the same site.
    pub reported: usize,
    /// Surviving mutants whose finding was suppressed by `// tmt:ignore` or `--baseline`.
    pub suppressed: usize,
    /// Surviving mutants omitted because an enclosing or sole expression was already
    /// classified as constant in the tests.
    pub redundant_constant: usize,
    /// Surviving mutants in unchanged code (reached by changed tests under `--since`)
    /// confirmed to have also survived the earlier tests, so they are not regressions.
    pub preexisting_unchanged: usize,
    /// Surviving mutants in unchanged code (under `--since`) whose status against the
    /// earlier tests could not be evaluated (earlier tests failed to build/pass or the
    /// mutant run was unable/unconfirmed).
    pub unchanged_not_compared: usize,
    /// Surviving confirmation mutants whose predicted gap was not confirmed
    /// (another confirmation mutant for the same gap was refuted or not evaluated).
    pub unconfirmed_gap: usize,
}

impl SurvivorBreakdown {
    /// Total surviving mutants accounted for across all categories, returning `None` on overflow.
    pub fn checked_total(&self) -> Option<usize> {
        self.reported
            .checked_add(self.suppressed)?
            .checked_add(self.redundant_constant)?
            .checked_add(self.preexisting_unchanged)?
            .checked_add(self.unchanged_not_compared)?
            .checked_add(self.unconfirmed_gap)
    }

    /// Total surviving mutants accounted for across all categories.
    pub fn total(&self) -> usize {
        self.reported
            + self.suppressed
            + self.redundant_constant
            + self.preexisting_unchanged
            + self.unchanged_not_compared
            + self.unconfirmed_gap
    }

    /// Surviving mutants intentionally not emitted as findings (excluding suppressions).
    pub fn unreported(&self) -> usize {
        self.redundant_constant
            + self.preexisting_unchanged
            + self.unchanged_not_compared
            + self.unconfirmed_gap
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct TimeoutRechecks {
    pub first_pass: usize,
    pub confirmed: usize,
    pub recovered: usize,
    pub unconfirmed: usize,
}

impl TimeoutRechecks {
    fn is_empty(&self) -> bool {
        self.first_pass == 0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    ProbeCompileError,
    ProbeReplacedBySourceRebuild,
    RequiresSourceRebuild,
    SourceNotBuilt,
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
        (end_line, end_column): (usize, usize),
        reason: SkipReason,
        diagnostic: Option<String>,
    ) -> Self {
        Self {
            at,
            end_line,
            end_column,
            reason,
            diagnostic,
        }
    }
}

fn is_zero_usize(value: &usize) -> bool {
    *value == 0
}

/// What a run that was asked to check itself found (`--audit`).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Audit {
    /// Surviving mutants that were run once more, against every test of every binary.
    pub sampled: usize,
    /// How many surviving mutants there were to choose from.
    pub survivors: usize,
    /// Sampled survivors whose all-tests re-run could not be judged (`Unable` or unconfirmed timeout).
    #[serde(skip_serializing_if = "is_zero_usize")]
    pub not_audited: usize,
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
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub rerun_failed: bool,
}

/// A source edit in the candidate catalogue. Byte offsets refer to the original file.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CandidateEdit {
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub start: usize,
    pub end: usize,
    pub before: String,
    pub replacement: String,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Candidate {
    pub what: String,
    pub into: String,
    pub requires_rebuild: bool,
    pub edits: Vec<CandidateEdit>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Report {
    pub schema_version: u32,
    pub package: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mutants: Option<Vec<Candidate>>,
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
    /// Of those, the mutants of code that is not part of this build (another target's
    /// file, or a `cfg` the features turn off).
    pub mutants_not_compiled: usize,
    pub mutants_run: usize,
    pub mutants_killed: usize,
    /// Killed mutants that were stopped by the timeout (typically an endless loop).
    /// This is a subset of `mutants_killed`.
    pub mutants_timed_out: usize,
    /// Evaluated mutants that passed the tests (`mutants_run - mutants_killed`).
    pub mutants_survived: usize,
    /// Complete accounting of `mutants_survived` by disposition.
    pub survivor_breakdown: SurvivorBreakdown,
    #[serde(skip_serializing_if = "TimeoutRechecks::is_empty")]
    pub timeout_rechecks: TimeoutRechecks,
    pub findings: Vec<Finding>,
    /// Findings left out because of a `// tmt:ignore` comment or the baseline.
    pub suppressed: usize,
    /// Code no test executes.
    pub unreached: Vec<Location>,
    /// Survivors predicted from the trace that a re-run killed: the tests are not deterministic.
    pub unstable: Vec<Location>,
    /// Findings in code that did not change, of a file whose tests did (`--since`): the
    /// tests as they were at the revision found these mutants out.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub weakened: Vec<Location>,
}

impl Default for Report {
    fn default() -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            package: String::new(),
            mutants: None,
            analysis: Analysis::default(),
            audit: None,
            evaluation: None,
            sites: 0,
            skipped_sites: 0,
            skipped: Vec::new(),
            unviable_mutants: 0,
            mutants_not_built: 0,
            mutants_not_evaluated: 0,
            mutants_not_compiled: 0,
            mutants_run: 0,
            mutants_killed: 0,
            mutants_timed_out: 0,
            mutants_survived: 0,
            survivor_breakdown: SurvivorBreakdown::default(),
            timeout_rechecks: TimeoutRechecks::default(),
            findings: Vec::new(),
            suppressed: 0,
            unreached: Vec::new(),
            unstable: Vec::new(),
            weakened: Vec::new(),
        }
    }
}

impl Report {
    pub(crate) fn record_evaluation(&mut self) {
        self.evaluation = if self.mutants_run == 0 {
            if self.sites == 0
                && self.skipped.is_empty()
                && self.mutants_not_built == 0
                && self.mutants_not_evaluated == 0
            {
                Some(Evaluation::NoSites)
            } else {
                Some(Evaluation::NoMutantsExecuted)
            }
        } else if self.mutants_not_built > 0
            || self.mutants_not_evaluated > 0
            || self.skipped_sites > 0
            || self.timeout_rechecks.unconfirmed > 0
            || self.survivor_breakdown.unchanged_not_compared > 0
            || (self.analysis.source_only && self.mutants_timed_out > 0)
            || self
                .skipped
                .iter()
                .any(|entry| entry.reason == SkipReason::UnparseableSource)
        {
            Some(Evaluation::Partial)
        } else {
            None
        };
    }

    /// Whether the mutant counts are internally consistent:
    /// - `mutants_run == mutants_killed + mutants_survived`
    /// - `mutants_timed_out <= mutants_killed`
    /// - `mutants_survived == survivor_breakdown.checked_total()`
    pub fn counts_reconcile(&self) -> bool {
        self.mutants_killed.checked_add(self.mutants_survived) == Some(self.mutants_run)
            && self.mutants_timed_out <= self.mutants_killed
            && self.survivor_breakdown.checked_total() == Some(self.mutants_survived)
    }

    /// Puts the findings in reading order. Sites are numbered as they are discovered, and
    /// a statement is discovered before the expressions of the statements above it.
    pub fn sort_findings(&mut self) {
        self.findings.sort_by(|a, b| {
            let (a, b) = (a.at(), b.at());
            (&a.file, a.line, a.column).cmp(&(&b.file, b.line, b.column))
        });
    }

    /// Whether the run found something to act on: a gap, an unstable prediction, or an
    /// audit discrepancy (which means the report cannot be trusted as it stands).
    pub fn has_gaps(&self) -> bool {
        !self.findings.is_empty()
            || !self.unstable.is_empty()
            || self.audit.as_ref().is_some_and(|audit| {
                !audit.disagreements.is_empty()
                    || !audit.source_disagreements.is_empty()
                    || !audit.trace_differences.is_empty()
            })
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

/// A revision as a reader wants it: a full object name cut to its first twelve digits.
pub(crate) fn short_revision(revision: &str) -> &str {
    let full = revision.len() >= 40 && revision.bytes().all(|byte| byte.is_ascii_hexdigit());
    if full { &revision[..12] } else { revision }
}

pub(crate) fn one_line(code: &str) -> String {
    const LIMIT: usize = 100;
    const ELLIPSIS: &str = "...";
    const PREFIX: usize = LIMIT - ELLIPSIS.len();

    // A code preview can contain four bytes per displayed character.
    // tmt:ignore performance only: the loop below gives the same text for code of any length
    if code.len() <= 4 * LIMIT {
        let mut words = code.split_whitespace();
        let Some(first) = words.next() else {
            return String::new();
        };
        let mut output = String::with_capacity(code.len());
        output.push_str(first);
        for word in words {
            output.push(' ');
            output.push_str(word);
        }
        // tmt:ignore performance only: text of more than LIMIT characters has more than LIMIT bytes, the first test only saves counting
        if output.len() > LIMIT && output.chars().count() > LIMIT {
            let cut = output.char_indices().nth(PREFIX).unwrap().0;
            output.truncate(cut);
            output.push_str(ELLIPSIS);
        }
        return output;
    }

    let mut output = String::with_capacity(LIMIT);
    let mut width = 0;
    let mut shortened_at = 0;
    let mut separated = false;
    for ch in code.chars() {
        if ch.is_whitespace() {
            separated = width != 0;
            continue;
        }
        for ch in [separated.then_some(' '), Some(ch)].into_iter().flatten() {
            if width == LIMIT {
                output.truncate(shortened_at);
                output.push_str(ELLIPSIS);
                return output;
            }
            output.push(ch);
            width += 1;
            if width == PREFIX {
                shortened_at = output.len();
            }
        }
        separated = false;
    }
    output
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
        SiteKind::Field {
            from_base: Some(from_base),
            ..
        } => format!(
            "without the field `{}` (it is then `{from_base}`)",
            one_line(&site.text)
        ),
        SiteKind::Field {
            from_base: None, ..
        } => format!("without the field `{}`", one_line(&site.text)),
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
        SiteKind::RangeContains {
            inclusive,
            lhs,
            rhs,
            argument,
            ..
        } => {
            let dots = if *inclusive { ".." } else { "..=" };
            format!(
                "`({}{dots}{}).contains({})`",
                one_line(lhs),
                one_line(rhs),
                one_line(argument)
            )
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
        SiteKind::SliceEnd {
            first, receiver, ..
        } => {
            let other = if *first { "last" } else { "first" };
            format!("`{}.{other}()`", one_line(receiver))
        }
        SiteKind::StrAffix {
            receiver, pattern, ..
        } => format!("`{}.contains({})`", one_line(receiver), one_line(pattern)),
        SiteKind::StrReplace {
            replace_all,
            receiver,
            from,
            to,
            ..
        } => {
            let (r, f, t) = (one_line(receiver), one_line(from), one_line(to));
            if *replace_all {
                format!("`{r}.replacen({f}, {t}, 1)`")
            } else {
                format!("`{r}.replace({f}, {t})`")
            }
        }
        SiteKind::Clamp {
            receiver,
            low,
            high,
            ..
        } => match m {
            Mutation::ClampDropLower => format!("`{}.min({})`", one_line(receiver), one_line(high)),
            _ => format!("`{}.max({})`", one_line(receiver), one_line(low)),
        },
        SiteKind::Rounding { op, receiver, .. } => {
            let alt = match m {
                Mutation::RoundAlt2 => op.alt2(),
                _ => op.alt1(),
            };
            format!("`{}.{}()`", one_line(receiver), alt.name())
        }
        SiteKind::Quantifier {
            all,
            receiver,
            predicate,
            ..
        } => {
            let other = if *all { "any" } else { "all" };
            format!("`{}.{other}({})`", one_line(receiver), one_line(predicate))
        }
        SiteKind::Position {
            forward,
            receiver,
            predicate,
            ..
        } => {
            let other = if *forward { "rposition" } else { "position" };
            format!("`{}.{other}({})`", one_line(receiver), one_line(predicate))
        }
        SiteKind::SaturatingArith {
            op,
            receiver,
            argument,
            ..
        } => format!(
            "`{}.{}({})`",
            one_line(receiver),
            op.swapped().name(),
            one_line(argument)
        ),
        SiteKind::StrTrim { op, receiver, .. } => {
            let alt = match m {
                Mutation::TrimAlt2 => op.alt2().unwrap_or_else(|| op.alt1()),
                _ => op.alt1(),
            };
            format!("`{}.{}()`", one_line(receiver), alt.name())
        }
        SiteKind::StrCaseEq {
            receiver, argument, ..
        } => format!("`{} == {}`", one_line(receiver), one_line(argument)),
        SiteKind::Abs { receiver, .. } => format!("`{}` (without `.abs()`)", one_line(receiver)),
        SiteKind::IterLimit { take, receiver, .. } => {
            if *take {
                format!("`{}.take(usize::MAX)`", one_line(receiver))
            } else {
                format!("`{}.skip(0)`", one_line(receiver))
            }
        }
        SiteKind::EndianSwap { op, receiver, .. } => {
            format!("`{}.{}()`", one_line(receiver), op.swapped().name())
        }
        SiteKind::BitCount { op, receiver, .. } => {
            format!("`{}.{}()`", one_line(receiver), op.swapped().name())
        }
        SiteKind::LoopCtrl { is_break } => {
            if *is_break {
                "`continue`".to_string()
            } else {
                "`break`".to_string()
            }
        }
        SiteKind::LiteralSet {
            receiver,
            argument,
            elements,
            ..
        } => match m {
            Mutation::DropLiteralElem(idx) => {
                if let Some((elem, _, _)) = elements.get(idx as usize) {
                    format!(
                        "without `{}` in `{}.contains({})`",
                        one_line(elem),
                        one_line(receiver),
                        one_line(argument)
                    )
                } else {
                    format!(
                        "without a literal element in `{}.contains({})`",
                        one_line(receiver),
                        one_line(argument)
                    )
                }
            }
            _ => format!(
                "without a literal element in `{}.contains({})`",
                one_line(receiver),
                one_line(argument)
            ),
        },
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
                Mutation::Shift(d) => {
                    let display = literal_display_kind(&lhs, &rhs, *literal);
                    match literal {
                        LiteralSide::Rhs(k) => {
                            format!("`{lhs} {sym} {}`", format_shifted_literal(k + d, display))
                        }
                        // k op rhs + d  <=>  k - d op rhs
                        LiteralSide::Lhs(k) => {
                            format!("`{} {sym} {rhs}`", format_shifted_literal(k - d, display))
                        }
                        LiteralSide::None => match (op, d) {
                            (CmpOp::Ge, 1) => format!("`{lhs} > {rhs}`"),
                            (CmpOp::Gt, -1) => format!("`{lhs} >= {rhs}`"),
                            (CmpOp::Le, -1) => format!("`{lhs} < {rhs}`"),
                            (CmpOp::Lt, 1) => format!("`{lhs} <= {rhs}`"),
                            // Meant as arithmetic on whole numbers: in the operands' own
                            // type the addition could overflow, the mutant that was run cannot.
                            // tmt:ignore a shift of 0 is never planned, so `>=` is the same
                            (_, d) if d > 0 => {
                                format!("`{lhs} {sym} {rhs} + {d}` (without overflow)")
                            }
                            (_, d) => format!("`{lhs} {sym} {rhs} - {}` (without overflow)", -d),
                        },
                    }
                }
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LiteralDisplay {
    Int,
    Byte,
    Char,
}

fn literal_display_kind(lhs: &str, rhs: &str, literal: LiteralSide) -> LiteralDisplay {
    let text = match literal {
        LiteralSide::Rhs(_) => rhs,
        LiteralSide::Lhs(_) => lhs,
        LiteralSide::None => return LiteralDisplay::Int,
    };
    let Ok(mut expr) = syn::parse_str::<syn::Expr>(text) else {
        return LiteralDisplay::Int;
    };
    loop {
        match expr {
            syn::Expr::Paren(inner) => expr = *inner.expr,
            syn::Expr::Group(inner) => expr = *inner.expr,
            _ => break,
        }
    }
    match expr {
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Byte(_),
            ..
        }) => LiteralDisplay::Byte,
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Char(_),
            ..
        }) => LiteralDisplay::Char,
        _ => LiteralDisplay::Int,
    }
}

fn format_byte_lit(b: u8) -> String {
    match b {
        b'\'' => "b'\\''".to_string(),
        b'\\' => "b'\\\\'".to_string(),
        b'\n' => "b'\\n'".to_string(),
        b'\r' => "b'\\r'".to_string(),
        b'\t' => "b'\\t'".to_string(),
        b'\0' => "b'\\0'".to_string(),
        0x20..=0x7e => format!("b'{}'", b as char),
        _ => format!("b'\\x{b:02x}'"),
    }
}

fn ord_to_char(ord: i128) -> Option<char> {
    let u = u32::try_from(ord).ok()?;
    let scalar = if u < 0xD800 {
        u
    } else if u <= 0x10_D7FF {
        u + 0x800
    } else {
        return None;
    };
    char::from_u32(scalar)
}

fn format_shifted_literal(v: i128, display: LiteralDisplay) -> String {
    match display {
        LiteralDisplay::Byte => u8::try_from(v)
            .map(format_byte_lit)
            .unwrap_or_else(|_| v.to_string()),
        LiteralDisplay::Char => ord_to_char(v)
            .map(|c| format!("{c:?}"))
            .unwrap_or_else(|| v.to_string()),
        LiteralDisplay::Int => v.to_string(),
    }
}

fn format_literal_value(v: i128, display: LiteralDisplay) -> String {
    match display {
        LiteralDisplay::Byte => u8::try_from(v)
            .map(|b| format!("{} ({b})", format_byte_lit(b)))
            .unwrap_or_else(|_| v.to_string()),
        LiteralDisplay::Char => ord_to_char(v)
            .map(|c| format!("{c:?} ({})", c as u32))
            .unwrap_or_else(|| v.to_string()),
        LiteralDisplay::Int => v.to_string(),
    }
}

/// The operand values that give `lhs - rhs = d`, in words.
fn input_for(lhs: &str, rhs: &str, literal: LiteralSide, d: i128) -> String {
    let display = literal_display_kind(lhs, rhs, literal);
    match (literal, d) {
        (LiteralSide::Rhs(k), d) => {
            format!("`{lhs}` = {}", format_literal_value(k + d, display))
        }
        (LiteralSide::Lhs(k), d) => {
            format!("`{rhs}` = {}", format_literal_value(k - d, display))
        }
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
    let display = literal_display_kind(lhs, rhs, literal);
    let ds: Vec<i128> = [obs.max_neg, obs.eq.then_some(0), obs.min_pos]
        .into_iter()
        .flatten()
        .collect();
    let text = match literal {
        LiteralSide::Rhs(k) => {
            let values: Vec<String> = ds
                .iter()
                .map(|d| format_literal_value(k + d, display))
                .collect();
            format!("`{lhs}` = {}", values.join(", "))
        }
        LiteralSide::Lhs(k) => {
            let mut values: Vec<i128> = ds.iter().map(|d| k - d).collect();
            values.sort();
            let values: Vec<String> = values
                .iter()
                .map(|v| format_literal_value(*v, display))
                .collect();
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
            same,
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
            // Without the shifts that are no mutants: the expression cannot be a
            // value that tells them from the original.
            (((max_shift - min_shift) - (same.1 - same.0)) as u128, add)
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

/// Input gap at `v.first()` / `v.last()`: no test gave more than one element.
pub fn slice_end_gap_finding(file: &str, site: &Site, verified: bool) -> Option<Finding> {
    let SiteKind::SliceEnd { receiver, .. } = &site.kind else {
        return None;
    };
    Some(Finding::InputGap {
        at: location(file, site),
        survivors: vec![describe(site, Mutation::OtherEnd)],
        survivor_count: 1,
        add_tests: vec![format!(
            "`{}` with two or more elements, the first different from the last",
            one_line(receiver)
        )],
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

/// Input gap at `s.starts_with(p)` / `s.ends_with(p)`: no test passed a string containing `p`
/// away from the prefix/suffix position.
pub fn str_affix_gap_finding(file: &str, site: &Site, verified: bool) -> Option<Finding> {
    let SiteKind::StrAffix {
        starts,
        receiver,
        pattern,
        ..
    } = &site.kind
    else {
        return None;
    };
    let where_else = if *starts { "start" } else { "end" };
    Some(Finding::InputGap {
        at: location(file, site),
        survivors: vec![describe(site, Mutation::StrContains)],
        survivor_count: 1,
        add_tests: vec![format!(
            "`{}` containing `{}` somewhere other than the {where_else}",
            one_line(receiver),
            one_line(pattern)
        )],
        observed: None,
        verified,
    })
}

/// Input gap at `s.replace(a, b)` / `s.replacen(a, b, 1)`: no test had `a` occur more than once.
pub fn str_replace_gap_finding(file: &str, site: &Site, verified: bool) -> Option<Finding> {
    let SiteKind::StrReplace { receiver, from, .. } = &site.kind else {
        return None;
    };
    Some(Finding::InputGap {
        at: location(file, site),
        survivors: vec![describe(site, Mutation::StrReplaceMultiplicity)],
        survivor_count: 1,
        add_tests: vec![format!(
            "`{}` containing `{}` more than once",
            one_line(receiver),
            one_line(from)
        )],
        observed: None,
        verified,
    })
}

/// Input gap at `x.clamp(lo, hi)`: no test crossed the dropped bound.
pub fn clamp_gap_finding(
    file: &str,
    site: &Site,
    mutation: Mutation,
    verified: bool,
) -> Option<Finding> {
    let SiteKind::Clamp {
        receiver,
        low,
        high,
        ..
    } = &site.kind
    else {
        return None;
    };
    let advice = if mutation == Mutation::ClampDropLower {
        format!(
            "`{}` below the lower clamp bound `{}`",
            one_line(receiver),
            one_line(low)
        )
    } else {
        format!(
            "`{}` above the upper clamp bound `{}`",
            one_line(receiver),
            one_line(high)
        )
    };
    Some(Finding::InputGap {
        at: location(file, site),
        survivors: vec![describe(site, mutation)],
        survivor_count: 1,
        add_tests: vec![advice],
        observed: None,
        verified,
    })
}

/// Input gap at `x.floor()` / `.ceil()` / `.round()` / `.trunc()`: no test distinguished the rounding mode.
pub fn rounding_gap_finding(
    file: &str,
    site: &Site,
    mutation: Mutation,
    verified: bool,
) -> Option<Finding> {
    let SiteKind::Rounding { op, receiver, .. } = &site.kind else {
        return None;
    };
    let alt = if mutation == Mutation::RoundAlt2 {
        op.alt2()
    } else {
        op.alt1()
    };
    Some(Finding::InputGap {
        at: location(file, site),
        survivors: vec![describe(site, mutation)],
        survivor_count: 1,
        add_tests: vec![format!(
            "`{}` that is {}",
            one_line(receiver),
            op.differs_for(alt)
        )],
        observed: None,
        verified,
    })
}

/// Input gap at `a.saturating_{add,sub,mul}(b)` / `a.wrapping_{add,sub,mul}(b)`:
/// no test produced a different result under the swapped arithmetic mode.
pub fn saturating_gap_finding(file: &str, site: &Site, verified: bool) -> Option<Finding> {
    let SiteKind::SaturatingArith {
        op,
        receiver,
        argument,
        ..
    } = &site.kind
    else {
        return None;
    };
    Some(Finding::InputGap {
        at: location(file, site),
        survivors: vec![describe(site, Mutation::SaturatingSwap)],
        survivor_count: 1,
        add_tests: vec![format!(
            "`{}` and `{}` where `{}` differs from `{}` ({})",
            one_line(receiver),
            one_line(argument),
            op.name(),
            op.swapped().name(),
            op.overflow_phrase()
        )],
        observed: None,
        verified,
    })
}

/// Input gap at `s.trim()` / `.trim_start()` / `.trim_end()`: no test had whitespace on the dropped side.
pub fn str_trim_gap_finding(
    file: &str,
    site: &Site,
    mutation: Mutation,
    verified: bool,
) -> Option<Finding> {
    let SiteKind::StrTrim { op, receiver, .. } = &site.kind else {
        return None;
    };
    let alt = if mutation == Mutation::TrimAlt2 {
        op.alt2().unwrap_or_else(|| op.alt1())
    } else {
        op.alt1()
    };
    Some(Finding::InputGap {
        at: location(file, site),
        survivors: vec![describe(site, mutation)],
        survivor_count: 1,
        add_tests: vec![format!(
            "`{}` with {}",
            one_line(receiver),
            op.whitespace_phrase(alt)
        )],
        observed: None,
        verified,
    })
}

/// Input gap at `a.eq_ignore_ascii_case(b)`: no test matched only when ignoring ASCII case.
pub fn str_case_eq_gap_finding(file: &str, site: &Site, verified: bool) -> Option<Finding> {
    let SiteKind::StrCaseEq {
        receiver, argument, ..
    } = &site.kind
    else {
        return None;
    };
    Some(Finding::InputGap {
        at: location(file, site),
        survivors: vec![describe(site, Mutation::StrExactEq)],
        survivor_count: 1,
        add_tests: vec![format!(
            "`{}` and `{}` that match only when ignoring ASCII case",
            one_line(receiver),
            one_line(argument)
        )],
        observed: None,
        verified,
    })
}

/// Input gap at `x.abs()`: no test passed a negative value.
pub fn abs_gap_finding(file: &str, site: &Site, verified: bool) -> Option<Finding> {
    let SiteKind::Abs { receiver, .. } = &site.kind else {
        return None;
    };
    Some(Finding::InputGap {
        at: location(file, site),
        survivors: vec![describe(site, Mutation::DropAbs)],
        survivor_count: 1,
        add_tests: vec![format!("negative `{}`", one_line(receiver))],
        observed: None,
        verified,
    })
}

/// Input gap at `iter.take(n)` / `iter.skip(n)`: count was always `usize::MAX` / `0`.
pub fn iter_limit_gap_finding(file: &str, site: &Site, verified: bool) -> Option<Finding> {
    let SiteKind::IterLimit { take, count, .. } = &site.kind else {
        return None;
    };
    let advice = if *take {
        format!(
            "`{}` smaller than `usize::MAX` with more elements than `{}`",
            one_line(count),
            one_line(count)
        )
    } else {
        format!("non-zero `{}`", one_line(count))
    };
    Some(Finding::InputGap {
        at: location(file, site),
        survivors: vec![describe(site, Mutation::IterLimitDrop)],
        survivor_count: 1,
        add_tests: vec![advice],
        observed: None,
        verified,
    })
}

/// Input gap at `x.to_be_bytes()` / `to_le_bytes()` / `to_be()` / `to_le()`:
/// every observed value was byte-palindromic (`x == x.swap_bytes()`).
pub fn endian_gap_finding(file: &str, site: &Site, verified: bool) -> Option<Finding> {
    let SiteKind::EndianSwap { receiver, .. } = &site.kind else {
        return None;
    };
    Some(Finding::InputGap {
        at: location(file, site),
        survivors: vec![describe(site, Mutation::EndianSwap)],
        survivor_count: 1,
        add_tests: vec![format!(
            "`{}` with asymmetric bytes (e.g. `0x0102`)",
            one_line(receiver)
        )],
        observed: None,
        verified,
    })
}

/// Input gap at `x.leading_zeros()` / `trailing_zeros()` / `leading_ones()` / `trailing_ones()`:
/// every observed value had equal leading and trailing counts.
pub fn bit_count_gap_finding(file: &str, site: &Site, verified: bool) -> Option<Finding> {
    let SiteKind::BitCount { op, receiver, .. } = &site.kind else {
        return None;
    };
    Some(Finding::InputGap {
        at: location(file, site),
        survivors: vec![describe(site, Mutation::BitCountSwap)],
        survivor_count: 1,
        add_tests: vec![format!(
            "`{}` where `{}` differs from `{}`",
            one_line(receiver),
            op.name(),
            op.swapped().name()
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

/// Input gap at a `(a..b).contains(x)` / `(a..=b).contains(x)` site where no test passed
/// an input equal to the upper endpoint `b` (with `a <= b`), so flipping `..` <-> `..=`
/// cannot change the result for any test.
pub fn range_contains_gap_finding(file: &str, site: &Site, verified: bool) -> Option<Finding> {
    let SiteKind::RangeContains {
        lhs, rhs, argument, ..
    } = &site.kind
    else {
        return None;
    };
    Some(Finding::InputGap {
        at: location(file, site),
        survivors: vec![describe(site, Mutation::RangeEnd)],
        survivor_count: 1,
        add_tests: vec![format!(
            "a case where `{}` equals `{}` (with `{} <= {}`)",
            one_line(argument),
            one_line(rhs),
            one_line(lhs),
            one_line(rhs)
        )],
        observed: None,
        verified,
    })
}

/// Input gap at a `[lit1, lit2, ...].contains(&x)` site where no test matched one of the
/// literal elements, so removing that element cannot change the result for any test.
pub fn literal_set_gap_finding(
    file: &str,
    site: &Site,
    elem_idx: u8,
    verified: bool,
) -> Option<Finding> {
    let SiteKind::LiteralSet {
        argument, elements, ..
    } = &site.kind
    else {
        return None;
    };
    let (elem, _, _) = elements.get(elem_idx as usize)?;
    Some(Finding::InputGap {
        at: location(file, site),
        survivors: vec![describe(site, Mutation::DropLiteralElem(elem_idx))],
        survivor_count: 1,
        add_tests: vec![format!(
            "a case where `{}` matches `{}`",
            one_line(argument),
            one_line(elem)
        )],
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
        (ArithOp::BitOr, ArithOp::BitXor) | (ArithOp::BitXor, ArithOp::BitOr) => format!(
            "operands where `{lhs}` and `{rhs}` share at least one set bit (if their bits are disjoint by construction, `{}` and `{}` are equivalent)",
            op.symbol(),
            other.symbol()
        ),
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

/// The report as SARIF 2.1.0, with file names relative to the repository root.
pub fn render_sarif(report: &Report) -> String {
    serde_json::to_string_pretty(&sarif_document(vec![sarif_run(report)]))
        .expect("JSON values serialise")
}

/// The CLI's workspace SARIF has one run per member, the analysed ones in report order
/// and then those whose analysis could not run (`failed`: package and what went wrong).
/// A member that failed is a run that says so: a document without it would read as a
/// package with nothing to report. Each run has an id of its own, by which a service
/// that takes several runs of one tool keeps them apart.
#[doc(hidden)]
pub fn workspace_sarif(reports: &[Report], failed: &[(String, String)]) -> impl std::fmt::Display {
    let category = |package: &str| serde_json::json!({ "id": format!("tmt/{package}/") });
    let mut runs: Vec<serde_json::Value> = reports
        .iter()
        .map(|report| {
            let mut run = sarif_run(report);
            run["properties"]["package"] = serde_json::json!(report.package);
            run["automationDetails"] = category(&report.package);
            run["invocations"] = serde_json::json!([{ "executionSuccessful": true }]);
            run
        })
        .collect();
    for (package, error) in failed {
        runs.push(serde_json::json!({
            "tool": sarif_tool(),
            "automationDetails": category(package),
            "invocations": [{
                "executionSuccessful": false,
                "toolExecutionNotifications": [{
                    "level": "error",
                    "message": { "text": error },
                }],
            }],
            "properties": { "package": package },
            "results": [],
        }));
    }
    sarif_document(runs)
}

/// A path as the URI reference SARIF wants: `/` between its parts, and in the parts
/// every byte that is not plainly allowed in one (a space, `#`, `%`, what is not ASCII)
/// written as `%XX`.
fn sarif_uri(prefix: &str, file: &str) -> String {
    sarif_uri_for(prefix, file, cfg!(windows))
}

fn sarif_uri_for(prefix: &str, file: &str, backslash_is_separator: bool) -> String {
    let mut uri = String::new();
    for byte in prefix.bytes().chain(file.bytes()) {
        let byte = if backslash_is_separator && byte == b'\\' {
            b'/'
        } else {
            byte
        };
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                uri.push(byte as char)
            }
            _ => uri.push_str(&format!("%{byte:02X}")),
        }
    }
    uri
}

fn sarif_tool() -> serde_json::Value {
    serde_json::json!({ "driver": {
        "name": "tmt",
        "rules": [
            { "id": "input-gap", "shortDescription": { "text": "No test input tells this mutant from the original" } },
            { "id": "assertion-gap", "shortDescription": { "text": "Tests run the mutated code and still pass" } },
            { "id": "survivor", "shortDescription": { "text": "A mutant built on its own passes every test; why is not known" } },
            { "id": "unstable-prediction", "shortDescription": { "text": "A predicted survivor failed when re-run; tests may be non-deterministic" } },
            { "id": "weakened-test", "shortDescription": { "text": "Tests since the base revision stopped catching a mutant in unchanged code" } },
            { "id": "audit-disagreement", "shortDescription": { "text": "Audit found a mismatch between selective/probe results and full/source execution" } }
        ]
    } })
}

fn sarif_document(runs: Vec<serde_json::Value>) -> serde_json::Value {
    let mut document = serde_json::json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
    });
    document["runs"] = serde_json::Value::Array(runs);
    document
}

fn sarif_run(report: &Report) -> serde_json::Value {
    let prefix = report.analysis.package_path.as_str();
    let mut results: Vec<serde_json::Value> = report
        .findings
        .iter()
        .map(|finding| {
            let (kind, at) = finding_parts(finding);
            let rule = match kind {
                "INPUT GAP" => "input-gap",
                "ASSERTION GAP" => "assertion-gap",
                _ => "survivor",
            };
            let text = format!("`{}`: {}", at.code, finding_body(finding, false).join("; "));
            serde_json::json!({
                "ruleId": rule,
                "level": "warning",
                "message": { "text": text },
                // What the service tells a finding of one run from that of another by.
                "partialFingerprints": { "tmtFindingId/v2": at.id },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": sarif_uri(prefix, &at.file) },
                        "region": { "startLine": at.line, "startColumn": at.column }
                    }
                }]
            })
        })
        .collect();
    for at in &report.unstable {
        results.push(serde_json::json!({
            "ruleId": "unstable-prediction",
            "level": "error",
            "message": {
                "text": format!(
                    "`{}`: trace predicted a survivor, but the re-run failed (tests may not be deterministic)",
                    at.code
                )
            },
            "locations": [{
                "physicalLocation": {
                    "artifactLocation": { "uri": sarif_uri(prefix, &at.file) },
                    "region": { "startLine": at.line, "startColumn": at.column }
                }
            }]
        }));
    }
    for at in &report.weakened {
        results.push(serde_json::json!({
            "ruleId": "weakened-test",
            "level": "warning",
            "message": {
                "text": format!(
                    "`{}`: mutant in unchanged code now survives after test changes",
                    at.code
                )
            },
            "locations": [{
                "physicalLocation": {
                    "artifactLocation": { "uri": sarif_uri(prefix, &at.file) },
                    "region": { "startLine": at.line, "startColumn": at.column }
                }
            }]
        }));
    }
    if let Some(audit) = &report.audit {
        for at in &audit.disagreements {
            results.push(serde_json::json!({
                "ruleId": "audit-disagreement",
                "level": "error",
                "message": {
                    "text": format!(
                        "`{}`: survived selective tests, but failed when audited against all tests",
                        at.code
                    )
                },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": sarif_uri(prefix, &at.file) },
                        "region": { "startLine": at.line, "startColumn": at.column }
                    }
                }]
            }));
        }
        for diff in &audit.source_disagreements {
            results.push(serde_json::json!({
                "ruleId": "audit-disagreement",
                "level": "error",
                "message": {
                    "text": format!(
                        "`{}` ({}): {} as a probe, {} built from source",
                        diff.at.code, diff.mutant, diff.as_probe, diff.built
                    )
                },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": sarif_uri(prefix, &diff.at.file) },
                        "region": { "startLine": diff.at.line, "startColumn": diff.at.column }
                    }
                }]
            }));
        }
        for diff in &audit.trace_differences {
            let text = if diff.rerun_failed && diff.records == 0 {
                format!(
                    "audit trace re-run for `{}` did not pass (tests may not be deterministic)",
                    diff.binary
                )
            } else if diff.rerun_failed {
                format!(
                    "audit trace re-run for `{}` did not pass and differed in {} records",
                    diff.binary, diff.records
                )
            } else {
                format!(
                    "audit trace for `{}` differed between two runs ({} records)",
                    diff.binary, diff.records
                )
            };
            results.push(serde_json::json!({
                "ruleId": "audit-disagreement",
                "level": "error",
                "message": { "text": text }
            }));
        }
    }
    let mut properties = serde_json::json!({
        "analysis": report.analysis,
        "evaluation": report.evaluation,
        "skipped": report.skipped,
    });
    if let Some(audit) = &report.audit {
        properties["audit"] = serde_json::json!(audit);
    }
    let mut run = serde_json::json!({
        "tool": sarif_tool(),
        // What was looked at: a run of a part says nothing about the rest.
        "properties": properties,
    });
    run["results"] = serde_json::Value::Array(results);
    run
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

/// The lines of one finding, without its header. When `sanitize` is true, each untrusted
/// field is sanitized before being joined so an incomplete escape sequence in one field
/// cannot swallow later fields on the same line.
fn finding_body(finding: &Finding, sanitize: bool) -> Vec<String> {
    let clean = |s: &str| {
        if sanitize {
            sanitize_inline(s)
        } else {
            s.to_string()
        }
    };
    let clean_list = |items: &[String]| {
        if sanitize {
            items
                .iter()
                .map(|s| sanitize_inline(s))
                .collect::<Vec<_>>()
                .join(", ")
        } else {
            items.join(", ")
        }
    };
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
                clean_list(survivors)
            ));
            lines.extend(observed.iter().map(|s| clean(s)));
            lines.extend(
                add_tests
                    .iter()
                    .map(|test| format!("add a test with {}", clean(test))),
            );
        }
        Finding::AssertionGap {
            survivors,
            tests,
            effects,
            effects_of,
            witness,
            ..
        } => {
            lines.push(format!("passes every test as: {}", clean_list(survivors)));
            let shown_tests: Vec<String> = tests.iter().map(|t| clean(t)).collect();
            lines.push(format!(
                "these tests run the changed code and still pass: {}",
                list(&shown_tests, 5)
            ));
            if effects.is_empty() {
                lines.push("add an assertion on what this code is responsible for".to_string());
            } else {
                // Which of several mutants was followed, so that what is said of "the
                // mutant" below can be tried.
                if let (Some(followed), true) = (effects_of, survivors.len() > 1) {
                    lines.push(format!(
                        "followed one of them through the tests: {}",
                        clean(followed)
                    ));
                }
                lines.extend(effects.iter().map(|s| clean(s)));
            }
            if let Some(witness) = witness {
                lines.extend(witness_lines(witness, sanitize));
            }
        }
        Finding::Survivor {
            survivors,
            change_since: Some(revision),
            ..
        } => {
            lines.push(format!("passes every test {}", clean_list(survivors)));
            lines.push(format!(
                "no test tells this change from the code as it was at {}: add one that fails without it",
                clean(short_revision(revision))
            ));
        }
        Finding::Survivor {
            survivors,
            witness,
            advice,
            ..
        } => {
            lines.push(format!("passes every test as: {}", clean_list(survivors)));
            if let Some(witness) = witness {
                lines.extend(witness_lines(witness, sanitize));
            } else if let Some(advice) = advice {
                lines.push(clean(advice));
            } else {
                lines.push(
                    "built on its own, where nothing can be recorded: either no test runs this code, or one does and does not check what it gives"
                        .to_string(),
                );
            }
        }
    }
    lines
}

fn witness_lines(witness: &VerifiedWitness, sanitize: bool) -> Vec<String> {
    let clean = |s: &str| {
        if sanitize {
            sanitize_inline(s)
        } else {
            s.to_string()
        }
    };
    let call = clean(&witness.call);
    let orig = clean(&witness.original);
    let mut_val = clean(&witness.mutant);
    let assertion = clean(&witness.assertion);
    if let Some(test) = &witness.in_test {
        let test = clean(test);
        // The test may make the call more than once, with another result each time.
        let place = witness
            .in_test_line
            .zip(witness.in_test_column)
            .map(|(line, column)| format!("line {line}, column {column}"));
        let (on, of) = place.map_or_else(
            || (String::new(), "that call".to_string()),
            |place| (format!(" at {place}"), format!("the call at {place}")),
        );
        return vec![
            format!(
                "verified on `{call}` where `{test}` calls it{on}: original returned {orig}, mutant returned {mut_val}",
            ),
            format!("in `{test}`, compare the result of {of} with this value: `{assertion}`"),
        ];
    }
    let mut lines = vec![format!(
        "verified on `{call}`: original returned {orig}, mutant returned {mut_val}",
    )];
    if let Some(killed) = witness.individually_killed {
        lines.push(format!(
            "the selected tests detected all {killed} component changes in isolation"
        ));
    }
    lines.extend(witness.guidance.iter().map(|s| clean(s)));
    lines.push(format!("add a test with `{assertion}`"));
    lines
}

fn finding_parts(finding: &Finding) -> (&'static str, &Location) {
    match finding {
        Finding::InputGap { at, .. } => ("INPUT GAP", at),
        Finding::AssertionGap { at, .. } => ("ASSERTION GAP", at),
        Finding::Survivor {
            at,
            change_since: Some(_),
            ..
        } => ("UNTESTED CHANGE", at),
        Finding::Survivor { at, .. } => ("SURVIVOR", at),
    }
}

/// Strips 7-bit and 8-bit ANSI/VT terminal escape sequences (CSI, OSC, DCS, SOS, PM, APC,
/// and nF/Fp/Fe ESC sequences), C0/C1/DEL control characters, and Unicode bidirectional
/// override/isolate controls from untrusted project text before writing to a terminal.
/// When `preserve_newlines` is false, `\n`, `\r`, and `\t` are collapsed to spaces so
/// untrusted fields cannot forge new lines in text reports.
fn sanitize_terminal_text(text: &str, preserve_newlines: bool) -> String {
    fn consume_csi(chars: &mut std::iter::Peekable<std::str::Chars<'_>>) {
        while let Some(&next) = chars.peek() {
            if (' '..='?').contains(&next) {
                chars.next();
            } else if ('@'..='~').contains(&next) {
                chars.next();
                break;
            } else {
                // Incomplete/malformed CSI: stop before consuming unrelated text or newlines.
                break;
            }
        }
    }
    fn consume_st_terminated(
        chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
        allow_bel: bool,
    ) {
        while let Some(next) = chars.next() {
            if (allow_bel && next == '\x07') || next == '\u{009c}' {
                break;
            }
            if next == '\x1b' && chars.peek() == Some(&'\\') {
                chars.next();
                break;
            }
        }
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' {
            match chars.peek().copied() {
                // 7-bit CSI: ESC [ <param/interm 0x20-0x3F>* <final 0x40-0x7E>
                Some('[') => {
                    chars.next();
                    consume_csi(&mut chars);
                }
                // 7-bit OSC: ESC ] ... (BEL | ESC \ | C1 ST)
                Some(']') => {
                    chars.next();
                    consume_st_terminated(&mut chars, true);
                }
                // 7-bit DCS / SOS / PM / APC: ESC (P | X | ^ | _) ... (ESC \ | C1 ST)
                Some('P' | 'X' | '^' | '_') => {
                    chars.next();
                    consume_st_terminated(&mut chars, false);
                }
                // nF sequences (e.g., ESC ( B charset designation): 0x20-0x2F intermediates + 0x30-0x7E final.
                Some(next) if (' '..='/').contains(&next) => {
                    while let Some(&c) = chars.peek() {
                        if (' '..='/').contains(&c) {
                            chars.next();
                        } else if ('0'..='~').contains(&c) {
                            chars.next();
                            break;
                        } else {
                            break;
                        }
                    }
                }
                // Single-final ESC sequence (e.g., ESC c, ESC 7, ESC =).
                Some(next) if ('0'..='~').contains(&next) => {
                    chars.next();
                }
                _ => {}
            }
            continue;
        }
        // 8-bit C1 CSI (\u{009B}), OSC (\u{009D}), DCS (\u{0090}), SOS (\u{0098}), PM (\u{009E}), APC (\u{009F}).
        if ch == '\u{009b}' {
            consume_csi(&mut chars);
            continue;
        }
        if ch == '\u{009d}' {
            consume_st_terminated(&mut chars, true);
            continue;
        }
        if matches!(ch, '\u{0090}' | '\u{0098}' | '\u{009e}' | '\u{009f}') {
            consume_st_terminated(&mut chars, false);
            continue;
        }
        // Unicode BiDi override/isolate controls (Trojan Source / terminal spoofing).
        if matches!(
            ch,
            '\u{061c}'
                | '\u{200e}'
                | '\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2066}'..='\u{2069}'
        ) {
            continue;
        }
        if ch == '\n' && preserve_newlines {
            out.push('\n');
            continue;
        }
        if matches!(ch, '\n' | '\r' | '\t') {
            out.push(' ');
            continue;
        }
        if ch.is_control() {
            continue;
        }
        out.push(ch);
    }
    out
}

/// Sanitizes an untrusted single-line string for terminal output (no escape sequences, no
/// control characters, no injected line breaks).
#[doc(hidden)]
pub fn sanitize_inline(text: &str) -> String {
    sanitize_terminal_text(text, false)
}

/// Sanitizes multi-line terminal output while preserving intentional `\n` line breaks.
#[doc(hidden)]
pub fn sanitize_terminal(text: &str) -> String {
    sanitize_terminal_text(text, true)
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
                at.column,
                sanitize_inline(&at.code)
            ));
        } else {
            if previous.is_some() {
                out.push('\n');
            }
            out.push_str(&format!(
                "{kind}  {}:{}:{}\n    {}\n",
                sanitize_inline(&at.file),
                at.line,
                at.column,
                sanitize_inline(&at.code)
            ));
        }
        for line in finding_body(finding, true) {
            out.push_str(&format!("  {line}\n"));
        }
        if report.weakened.iter().any(|before| {
            (before.file.as_str(), before.line, before.column)
                == (at.file.as_str(), at.line, at.column)
        }) {
            out.push_str(&format!(
                "  the tests as they were at {} found this out: a change to the tests lets it through\n",
                sanitize_inline(short_revision(
                    report.analysis.since.as_deref().unwrap_or("the revision")
                ))
            ));
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
                sanitize_inline(&at.file),
                at.line,
                at.column,
                sanitize_inline(&at.code)
            ));
        }
    }
    for at in &report.unstable {
        out.push_str(&format!(
            "UNSTABLE  {}:{}:{}\n    {}\n  the trace predicted a survivor but the re-run failed: the tests are not deterministic, or do something the trace cannot see\n\n",
            sanitize_inline(&at.file),
            at.line,
            at.column,
            sanitize_inline(&at.code)
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
        sanitize_inline(&report.package),
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
    if !report.timeout_rechecks.is_empty() {
        let rechecks = &report.timeout_rechecks;
        out.push_str(&format!(
            ", {} parallel timeouts ({} confirmed, {} recovered, {} unconfirmed)",
            rechecks.first_pass, rechecks.confirmed, rechecks.recovered, rechecks.unconfirmed
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
            "audit: {} of {} surviving mutants run against all tests, {} failed one they had not been run against",
            audit.sampled,
            audit.survivors,
            audit.disagreements.len()
        ));
        if audit.not_audited > 0 {
            out.push_str(&format!(", {} could not be judged", audit.not_audited));
        }
        out.push('\n');
        for at in &audit.disagreements {
            out.push_str(&format!(
                "  {}:{}:{}  {}\n",
                sanitize_inline(&at.file),
                at.line,
                at.column,
                sanitize_inline(&at.code)
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
                sanitize_inline(&differs.at.file),
                differs.at.line,
                differs.at.column,
                sanitize_inline(&differs.mutant),
                sanitize_inline(&differs.as_probe),
                sanitize_inline(&differs.built)
            ));
        }
        if audit.trace_differences.is_empty() {
            out.push_str("audit: recorded once more, the tests did the same\n");
        }
        for binary in &audit.trace_differences {
            let name = sanitize_inline(&binary.binary);
            if binary.rerun_failed && binary.records == 0 {
                out.push_str(&format!(
                    "audit: recorded once more, `{name}` did not pass: its tests do not always do the same\n"
                ));
            } else if binary.rerun_failed {
                out.push_str(&format!(
                    "audit: recorded once more, `{name}` did not pass and differs in {} records: its tests do not always do the same\n",
                    binary.records
                ));
            } else {
                out.push_str(&format!(
                    "audit: recorded once more, `{name}` differs in {} records: its tests do not always do the same\n",
                    binary.records
                ));
            }
        }
    }
    if report.mutants_not_compiled > 0 {
        out.push_str(&format!(
            "({} mutants not evaluated: their code is not part of this build (a `cfg` or a feature that is off, or a file of another target); other --features may build it)\n",
            report.mutants_not_compiled
        ));
    }
    let not_evaluated = report
        .mutants_not_evaluated
        .saturating_sub(report.mutants_not_compiled);
    if not_evaluated > 0 {
        out.push_str(&format!(
            "({not_evaluated} mutants not evaluated: a range that ends at the largest value of its type cannot be made to include it, or a run that could not be judged)\n"
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
    if report.survivor_breakdown.unreported() > 0 {
        let breakdown = &report.survivor_breakdown;
        let mut parts = vec![];
        if breakdown.redundant_constant > 0 {
            parts.push(format!(
                "{} redundant constant",
                breakdown.redundant_constant
            ));
        }
        if breakdown.preexisting_unchanged > 0 {
            parts.push(format!(
                "{} preexisting in unchanged code",
                breakdown.preexisting_unchanged
            ));
        }
        if breakdown.unchanged_not_compared > 0 {
            parts.push(format!(
                "{} in unchanged code (earlier tests could not be evaluated)",
                breakdown.unchanged_not_compared
            ));
        }
        if breakdown.unconfirmed_gap > 0 {
            parts.push(format!(
                "{} from unconfirmed gaps",
                breakdown.unconfirmed_gap
            ));
        }
        out.push_str(&format!(
            "({} surviving mutants not reported as findings: {})\n",
            breakdown.unreported(),
            parts.join(", ")
        ));
    }
    if report.evaluation == Some(Evaluation::Partial) {
        out.push_str("(partial evaluation: some mutants were not built or evaluated)\n");
    }
    if report.evaluation == Some(Evaluation::NoMutantsExecuted) {
        if report.unreached.is_empty()
            && report.mutants_not_evaluated == 0
            && report.mutants_not_built == 0
            && (report.unviable_mutants > 0 || report.skipped_sites > 0)
        {
            out.push_str("No mutants executed. See any skipped entries in --json.\n");
        } else {
            out.push_str("No mutants executed. See any skipped entries in --json. For a subprocess that clears the probe environment, try --source-only.\n");
        }
    }
    if !show_unreached && !report.unreached.is_empty() {
        out.push_str("(--show-unreached lists the code no test executes)\n");
    }
    sanitize_terminal(&out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::discover;

    #[test]
    fn evaluation_distinguishes_no_sites_no_mutants_partial_and_completed_work() {
        let mut report = Report::default();
        report.record_evaluation();
        assert_eq!(report.evaluation, Some(Evaluation::NoSites));
        report.sites = 1;
        report.record_evaluation();
        assert_eq!(report.evaluation, Some(Evaluation::NoMutantsExecuted));
        assert!(render_text(&report, false).contains("No mutants executed."));
        assert!(render_text(&report, false).contains("--source-only"));
        // When every candidate was skipped as unviable/uncompilable with no unreached,
        // unbuilt, or unevaluated sites, suggesting --source-only would not help.
        report.unviable_mutants = 1;
        let unbuildable_text = render_text(&report, false);
        assert!(unbuildable_text.contains("No mutants executed."));
        assert!(
            !unbuildable_text.contains("--source-only"),
            "{unbuildable_text}"
        );
        // Mixed with unevaluated or unbuilt candidates keeps the --source-only hint.
        report.mutants_not_evaluated = 1;
        assert!(render_text(&report, false).contains("--source-only"));
        report.mutants_not_evaluated = 0;
        report.mutants_not_built = 1;
        assert!(render_text(&report, false).contains("--source-only"));
        report.mutants_not_built = 0;
        report.unviable_mutants = 0;
        report.mutants_run = 1;
        report.mutants_killed = 1;
        report.record_evaluation();
        assert_eq!(report.evaluation, None);
        assert!(report.counts_reconcile());
        assert!(!render_text(&report, false).contains("No mutants executed."));
        assert!(!render_text(&report, false).contains("partial evaluation"));

        report.mutants_not_built = 3;
        report.record_evaluation();
        assert_eq!(report.evaluation, Some(Evaluation::Partial));
        assert!(render_text(&report, false).contains("partial evaluation"));
    }

    #[test]
    fn evaluation_pict_table() {
        let table = include_str!("../tests/data/report_evaluation.pict.tsv");
        let mut lines = table.lines();
        let header = lines.next().expect("header");
        assert_eq!(
            header,
            "sites\tmutants_run\tskipped_entries\tskipped_sites\tunparseable_source\tmutants_not_built\tmutants_not_evaluated\tunconfirmed_timeouts\tsource_only_timeout\thas_gaps\tsource_only"
        );
        let dummy = || Location {
            file: "src/lib.rs".into(),
            line: 1,
            column: 1,
            code: "x".into(),
            ..Location::default()
        };
        let mut rows = 0;
        for line in lines.filter(|line| !line.trim().is_empty()) {
            rows += 1;
            let cols: Vec<&str> = line.split('\t').collect();
            assert_eq!(cols.len(), 11, "row: {line}");
            let skipped = if cols[4] == "true" {
                vec![SkippedSite::new(
                    dummy(),
                    (1, 2),
                    SkipReason::UnparseableSource,
                    Some("parse error".into()),
                )]
            } else if cols[2] == "nonempty" {
                vec![SkippedSite::new(
                    dummy(),
                    (1, 2),
                    SkipReason::ProbeReplacedBySourceRebuild,
                    None,
                )]
            } else {
                vec![]
            };
            let source_only_timeout = cols[8] == "true";
            let mut report = Report {
                sites: usize::from(cols[0] == "nonzero"),
                mutants_run: usize::from(cols[1] == "nonzero"),
                mutants_killed: usize::from(cols[1] == "nonzero"),
                mutants_timed_out: usize::from(source_only_timeout),
                skipped,
                skipped_sites: usize::from(cols[3] == "nonzero"),
                mutants_not_built: usize::from(cols[5] == "nonzero"),
                mutants_not_evaluated: usize::from(cols[6] == "nonzero"),
                timeout_rechecks: TimeoutRechecks {
                    first_pass: usize::from(cols[7] == "nonzero"),
                    unconfirmed: usize::from(cols[7] == "nonzero"),
                    ..TimeoutRechecks::default()
                },
                analysis: Analysis {
                    source_only: cols[10] == "true",
                    ..Analysis::default()
                },
                ..Report::default()
            };
            report.record_evaluation();
            let expected = match cols[1] {
                "zero" => {
                    if cols[0] == "zero"
                        && cols[2] == "empty"
                        && cols[5] == "zero"
                        && cols[6] == "zero"
                    {
                        Some(Evaluation::NoSites)
                    } else {
                        Some(Evaluation::NoMutantsExecuted)
                    }
                }
                "nonzero" => {
                    if cols[3] == "nonzero"
                        || cols[4] == "true"
                        || cols[5] == "nonzero"
                        || cols[6] == "nonzero"
                        || cols[7] == "nonzero"
                        || source_only_timeout
                    {
                        Some(Evaluation::Partial)
                    } else {
                        None
                    }
                }
                other => panic!("unexpected mutants_run value: {other}"),
            };
            assert_eq!(report.evaluation, expected, "row: {line}");
            assert!(report.counts_reconcile(), "row: {line}");
            let text = render_text(&report, false);
            assert_eq!(
                text.contains("partial evaluation"),
                expected == Some(Evaluation::Partial),
                "row: {line}\n{text}"
            );
        }
        assert!(rows >= 11, "expected PICT table rows, got {rows}");
    }

    #[test]
    fn survivor_breakdown_reconciles_and_renders_unreported_reasons() {
        let mut report = Report {
            package: "demo".into(),
            sites: 5,
            mutants_run: 11,
            mutants_killed: 4,
            mutants_timed_out: 1,
            mutants_survived: 7,
            survivor_breakdown: SurvivorBreakdown {
                reported: 1,
                suppressed: 1,
                redundant_constant: 2,
                preexisting_unchanged: 1,
                unchanged_not_compared: 1,
                unconfirmed_gap: 1,
            },
            ..Report::default()
        };
        assert!(report.counts_reconcile());
        report.record_evaluation();
        assert_eq!(report.evaluation, Some(Evaluation::Partial));
        let text = render_text(&report, false);
        assert!(
            text.contains(
                "(5 surviving mutants not reported as findings: 2 redundant constant, 1 preexisting in unchanged code, 1 in unchanged code (earlier tests could not be evaluated), 1 from unconfirmed gaps)"
            ),
            "{text}"
        );

        report.survivor_breakdown.reported = 0;
        assert!(!report.counts_reconcile());
        report.survivor_breakdown.reported = 1;
        report.mutants_timed_out = 5;
        assert!(!report.counts_reconcile());

        // Overflow in breakdown addition must not panic or wrap to a false match.
        let overflow = Report {
            mutants_run: 0,
            mutants_killed: 0,
            mutants_survived: 0,
            survivor_breakdown: SurvivorBreakdown {
                reported: usize::MAX,
                suppressed: 1,
                ..SurvivorBreakdown::default()
            },
            ..Report::default()
        };
        assert!(!overflow.counts_reconcile());
    }

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
            local_types: vec![],
            shadowed_prims: vec![],
            cast_probes: vec![],
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
            same: (0, 0),
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
                local_types: vec![],
                shadowed_prims: vec![],
                cast_probes: vec![],
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
                needs_space: false,
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
                index: None,
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
                index: None,
            },
            ..assign.clone()
        };
        assert_eq!(describe(&closed, Mutation::RangeEnd), "`1..n`");
        let arm = Site {
            kind: SiteKind::Arm {
                pattern: "Kind::Update { key, .. }".into(),
                guard: crate::instrument::ArmGuard::None(0),
                removal: (0, 0),
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

        let end = Site {
            kind: SiteKind::SliceEnd {
                first: false,
                receiver: "self.items".into(),
                call: (0, 0),
                method: (0, 0),
            },
            ..assign.clone()
        };
        assert_eq!(describe(&end, Mutation::OtherEnd), "`self.items.first()`");
        let Some(Finding::InputGap { add_tests, .. }) = slice_end_gap_finding("f.rs", &end, true)
        else {
            panic!()
        };
        assert_eq!(
            add_tests[0],
            "`self.items` with two or more elements, the first different from the last"
        );
        assert_eq!(slice_end_gap_finding("f.rs", &clamp, true), None);

        let rem = site_of("fn f(n: u32, k: u32) -> u32 { n % k }");
        assert_eq!(describe(&rem, Mutation::ArithSwap), "`n / k`");
        assert_eq!(
            logic_gap_finding("f.rs", &add, LogicGap::RhsNeverDecides, true),
            None
        );
    }

    #[test]
    fn workspace_sarif_matches_the_individual_runs() {
        use serde_json::{Value, json};

        let report = Report {
            package: "zebra".into(),
            analysis: Analysis {
                package_path: "crates/zebra/".into(),
                ..Analysis::default()
            },
            findings: vec![Finding::Survivor {
                at: Location {
                    file: r#"src/"é".rs"#.into(),
                    line: 4,
                    column: 1,
                    id: "zebra|4".into(),
                    ..Location::default()
                },
                survivors: vec!["delete".into()],
                witness: None,
                change_since: None,
                advice: None,
            }],
            ..Report::default()
        };
        let empty = Report {
            package: "ant".into(),
            ..Report::default()
        };
        for reports in [
            &[][..],
            std::slice::from_ref(&report),
            &[report.clone(), empty],
        ] {
            let runs: Vec<_> = reports
                .iter()
                .map(|report| {
                    let mut single: Value = serde_json::from_str(&render_sarif(report)).unwrap();
                    let mut run = single["runs"][0].take();
                    assert!(run["properties"].get("package").is_none());
                    run["properties"]["package"] = json!(report.package);
                    // What a run of a workspace has beside that of a package alone.
                    run["automationDetails"] = json!({ "id": format!("tmt/{}/", report.package) });
                    run["invocations"] = json!([{ "executionSuccessful": true }]);
                    run
                })
                .collect();
            let old = json!({
                "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
                "version": "2.1.0",
                "runs": runs,
            });
            let workspace: Value =
                serde_json::from_str(&format!("{:#}", workspace_sarif(reports, &[]))).unwrap();
            assert_eq!(workspace, old);
        }
    }

    #[test]
    fn a_package_that_could_not_be_analysed_is_a_run_that_says_so() {
        use serde_json::Value;

        let analysed = Report {
            package: "ant".into(),
            ..Report::default()
        };
        let failed = [(
            "bee".to_string(),
            "the tests of `bee` fail: \"quoted\"\n".to_string(),
        )];
        let document: Value = serde_json::from_str(&format!(
            "{:#}",
            workspace_sarif(std::slice::from_ref(&analysed), &failed)
        ))
        .unwrap();
        let runs = document["runs"].as_array().unwrap();
        assert_eq!(runs.len(), 2);
        // Each run is told from the others, and the one that failed is not a clean one.
        assert_eq!(runs[0]["automationDetails"]["id"], "tmt/ant/");
        assert_eq!(runs[0]["invocations"][0]["executionSuccessful"], true);
        assert_eq!(runs[1]["automationDetails"]["id"], "tmt/bee/");
        assert_eq!(runs[1]["invocations"][0]["executionSuccessful"], false);
        assert_eq!(
            runs[1]["invocations"][0]["toolExecutionNotifications"][0]["message"]["text"],
            failed[0].1
        );
        assert_eq!(runs[1]["results"].as_array().unwrap().len(), 0);
        assert_eq!(runs[1]["tool"], runs[0]["tool"]);
        assert_eq!(runs[1]["properties"]["package"], "bee");
        // Only failures: still a document, with nothing passed off as analysed.
        let document: Value =
            serde_json::from_str(&format!("{:#}", workspace_sarif(&[], &failed))).unwrap();
        assert_eq!(document["runs"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn a_path_is_written_as_a_uri() {
        assert_eq!(sarif_uri("", "src/lib.rs"), "src/lib.rs");
        assert_eq!(
            sarif_uri("crates/a-b_c/", "src/x~1.rs"),
            "crates/a-b_c/src/x~1.rs"
        );
        // What a URI gives a meaning of its own, or does not allow, is written as bytes.
        assert_eq!(
            sarif_uri("my crate/", "a#b%c?.rs"),
            "my%20crate/a%23b%25c%3F.rs"
        );
        assert_eq!(sarif_uri("", "src/é.rs"), "src/%C3%A9.rs");
        assert_eq!(sarif_uri("", r#"a\b"c.rs"#), "a%5Cb%22c.rs");
        let report = Report {
            analysis: Analysis {
                package_path: "my crate/".into(),
                ..Analysis::default()
            },
            findings: vec![Finding::Survivor {
                at: Location {
                    file: "src/a b.rs".into(),
                    line: 1,
                    column: 1,
                    ..Location::default()
                },
                survivors: vec![],
                witness: None,
                change_since: None,
                advice: None,
            }],
            ..Report::default()
        };
        let sarif: serde_json::Value = serde_json::from_str(&render_sarif(&report)).unwrap();
        assert_eq!(
            sarif["runs"][0]["results"][0]["locations"][0]["physicalLocation"]["artifactLocation"]
                ["uri"],
            "my%20crate/src/a%20b.rs"
        );
        assert_eq!(sarif_uri_for("", r#"a\b"c.rs"#, false), "a%5Cb%22c.rs");
        assert_eq!(
            sarif_uri_for(r"crates\core\", r"src\lib.rs", true),
            "crates/core/src/lib.rs"
        );
    }

    #[test]
    fn findings_sort_by_path_and_position_in_input_order_for_ties() {
        let mut report = Report {
            findings: [
                ("z.rs", 1, 1, "1"),
                ("é.rs", 1, 1, "2"),
                ("a.rs", 2, 1, "3"),
                ("a.rs", 1, 3, "4"),
                ("a.rs", 1, 3, "5"),
                ("a.rs", 1, 1, "6"),
            ]
            .into_iter()
            .map(|(file, line, column, id)| Finding::Survivor {
                at: Location {
                    file: file.into(),
                    line,
                    column,
                    id: id.into(),
                    ..Location::default()
                },
                survivors: vec![],
                witness: None,
                change_since: None,
                advice: None,
            })
            .collect(),
            ..Report::default()
        };
        report.sort_findings();
        let ids: Vec<_> = report
            .findings
            .iter()
            .map(|finding| finding.at().id.as_str())
            .collect();
        assert_eq!(ids, ["6", "4", "5", "3", "1", "2"]);
    }

    #[test]
    fn collapsed_code_matches_the_previous_format() {
        // Pairwise combinations of character set, whitespace and normalized width.
        for (charset, spacing, width) in [
            ("ascii", "none", 17),
            ("ascii", "split", 100),
            ("ascii", "surrounded", 101),
            ("multibyte", "split", 101),
            ("ascii", "none", 4096),
            ("multibyte", "none", 97),
            ("multibyte", "surrounded", 0),
            ("multibyte", "surrounded", 17),
            ("multibyte", "surrounded", 4096),
            ("ascii", "split", 17),
            ("multibyte", "none", 100),
            ("multibyte", "none", 101),
            ("ascii", "surrounded", 97),
            ("ascii", "surrounded", 100),
            ("multibyte", "split", 97),
            ("ascii", "none", 0),
            ("multibyte", "split", 4096),
        ] {
            let glyph = if charset == "ascii" { "x" } else { "🦀" };
            let code = match spacing {
                "none" => glyph.repeat(width),
                "surrounded" => format!("\t{}\u{2003}\r\n", glyph.repeat(width)),
                _ => format!(
                    "{}\u{2003}\r\n {}",
                    glyph.repeat(width / 2),
                    glyph.repeat(width - width / 2 - 1),
                ),
            };
            let normalized = code.split_whitespace().collect::<Vec<_>>().join(" ");
            let expected = if normalized.chars().count() > 100 {
                format!("{}...", normalized.chars().take(97).collect::<String>())
            } else {
                normalized
            };
            assert_eq!(one_line(&code), expected, "{charset} {spacing} {width}");
        }

        // The last visible character can itself be the space between two words.
        let code = format!("{}\n{}", "x".repeat(96), "y".repeat(4096));
        assert_eq!(one_line(&code), format!("{} ...", "x".repeat(96)));
        let code = format!("{}\n{}", "x".repeat(100), " ".repeat(4096));
        assert_eq!(one_line(&code), "x".repeat(100));
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
    fn timeout_rechecks_are_in_the_json_only_when_there_were_any() {
        let none = serde_json::to_value(Report::default()).unwrap();
        assert!(none.get("timeout_rechecks").is_none(), "{none}");
        let report = Report {
            timeout_rechecks: TimeoutRechecks {
                first_pass: 3,
                confirmed: 1,
                recovered: 2,
                unconfirmed: 0,
            },
            ..Report::default()
        };
        assert_eq!(
            serde_json::to_value(&report).unwrap()["timeout_rechecks"],
            serde_json::json!({"first_pass": 3, "confirmed": 1, "recovered": 2, "unconfirmed": 0})
        );
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
                same: (0, 0),
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
                    witness: None,
                },
                Finding::AssertionGap {
                    at: below,
                    survivors: vec!["delete `s.reset();`".into()],
                    tests: vec!["t".into()],
                    effects: vec![
                        "with the mutant, `t` no longer runs `s.count = 0;` (src/lib.rs:12)".into(),
                    ],
                    effects_of: None,
                    witness: None,
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
        assert_eq!(rules.len(), 6);
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
            witness: None,
            change_since: None,
            advice: None,
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
            schema_version: SCHEMA_VERSION,
            mutants: None,
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
            mutants_not_compiled: 0,
            mutants_run: 3,
            mutants_killed: 2,
            mutants_timed_out: 0,
            mutants_survived: 1,
            survivor_breakdown: SurvivorBreakdown {
                reported: 1,
                ..SurvivorBreakdown::default()
            },
            timeout_rechecks: TimeoutRechecks::default(),
            suppressed: 0,
            findings: vec![Finding::AssertionGap {
                at: at.clone(),
                survivors: vec!["delete `s.check()?;`".into()],
                tests: (0..7).map(|i| format!("tests::t{i}")).collect(),
                effects: vec![],
                effects_of: None,
                witness: None,
            }],
            unreached: vec![at.clone()],
            unstable: vec![at],
            weakened: vec![],
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
            witness: None,
        };
        // Of several mutants one was followed: the report says which.
        let body = finding_body(&gap(&["`a <= b`", "`a == b`"], Some("`a <= b`")), false);
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
            let body = finding_body(&single, false);
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
            rerun_failed: false,
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
                "\n(1 mutants not evaluated: a range that ends at the largest value of its type cannot be made to include it, or a run that could not be judged)\n"
            ),
            "{}",
            render_text(&at_top, false)
        );
        // Each reason is said for the mutants it holds for, and only when it holds for some.
        let not_built = Report {
            mutants_not_evaluated: 5,
            mutants_not_compiled: 5,
            ..Report::default()
        };
        let text = render_text(&not_built, false);
        assert!(
            text.ends_with("\n(5 mutants not evaluated: their code is not part of this build (a `cfg` or a feature that is off, or a file of another target); other --features may build it)\n"),
            "{text}"
        );
        assert!(!text.contains("largest value"), "{text}");
        let both = Report {
            mutants_not_evaluated: 7,
            mutants_not_compiled: 5,
            ..Report::default()
        };
        let text = render_text(&both, false);
        assert!(
            text.contains("(5 mutants not evaluated: their code"),
            "{text}"
        );
        assert!(
            text.contains("\n(2 mutants not evaluated: a range"),
            "{text}"
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
            witness: None,
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

    #[test]
    fn audit_reports_unjudged_all_tests_reruns_in_text_and_json() {
        let mut report = Report {
            package: "demo".into(),
            audit: Some(Audit {
                sampled: 3,
                survivors: 3,
                not_audited: 2,
                ..Audit::default()
            }),
            ..Report::default()
        };
        let text = render_text(&report, false);
        assert!(
            text.contains(
                "audit: 3 of 3 surviving mutants run against all tests, 0 failed one they had not been run against, 2 could not be judged\n"
            ),
            "{text}"
        );
        let value = serde_json::to_value(&report).unwrap();
        assert_eq!(value["audit"]["not_audited"], 2);

        report.audit.as_mut().unwrap().not_audited = 0;
        let clean_text = render_text(&report, false);
        assert!(!clean_text.contains("could not be judged"), "{clean_text}");
        let clean_value = serde_json::to_value(&report).unwrap();
        assert!(clean_value["audit"].get("not_audited").is_none());
    }

    #[test]
    fn sarif_emits_unstable_predictions() {
        let report = Report {
            package: "demo".into(),
            unstable: vec![Location {
                file: "src/lib.rs".into(),
                line: 12,
                column: 5,
                code: "age >= 18".into(),
                ..Location::default()
            }],
            ..Report::default()
        };
        let sarif: serde_json::Value = serde_json::from_str(&render_sarif(&report)).unwrap();
        let results = sarif["runs"][0]["results"].as_array().unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["ruleId"], "unstable-prediction");
        assert_eq!(results[0]["level"], "error");
        assert_eq!(
            results[0]["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
            "src/lib.rs"
        );
    }

    #[test]
    fn sarif_emits_weakened_and_audit_disagreements_and_has_gaps() {
        let loc = Location {
            file: "src/lib.rs".into(),
            line: 20,
            column: 9,
            code: "x > 0".into(),
            ..Location::default()
        };
        let report = Report {
            package: "demo".into(),
            weakened: vec![loc.clone()],
            audit: Some(Audit {
                survivors: 1,
                sampled: 1,
                disagreements: vec![loc.clone()],
                not_audited: 0,
                rebuilt: 1,
                compared: 1,
                not_compared: 0,
                source_disagreements: vec![SourceDisagreement {
                    at: loc,
                    mutant: "`x >= 0`".into(),
                    as_probe: "passes".into(),
                    built: "fails".into(),
                }],
                trace_differences: vec![
                    TraceDifference {
                        binary: "unit".into(),
                        records: 3,
                        rerun_failed: false,
                    },
                    TraceDifference {
                        binary: "flaky".into(),
                        records: 0,
                        rerun_failed: true,
                    },
                    TraceDifference {
                        binary: "flaky-diff".into(),
                        records: 2,
                        rerun_failed: true,
                    },
                ],
            }),
            ..Report::default()
        };
        assert!(report.has_gaps());
        let text = render_text(&report, false);
        assert!(
            text.contains("`flaky` did not pass: its tests do not always do the same"),
            "{text}"
        );
        assert!(
            text.contains(
                "`flaky-diff` did not pass and differs in 2 records: its tests do not always do the same"
            ),
            "{text}"
        );
        let sarif: serde_json::Value = serde_json::from_str(&render_sarif(&report)).unwrap();
        let results = sarif["runs"][0]["results"].as_array().unwrap();
        let rules: Vec<_> = results
            .iter()
            .map(|r| (r["ruleId"].as_str().unwrap(), r["level"].as_str().unwrap()))
            .collect();
        assert_eq!(
            rules,
            [
                ("weakened-test", "warning"),
                ("audit-disagreement", "error"),
                ("audit-disagreement", "error"),
                ("audit-disagreement", "error"),
                ("audit-disagreement", "error"),
                ("audit-disagreement", "error"),
            ]
        );
        assert!(
            results[4]["message"]["text"]
                .as_str()
                .unwrap()
                .contains("did not pass"),
            "{}",
            results[4]
        );
        assert!(
            results[5]["message"]["text"]
                .as_str()
                .unwrap()
                .contains("did not pass and differed in 2 records"),
            "{}",
            results[5]
        );
        assert!(sarif["runs"][0]["properties"].get("audit").is_some());
    }

    #[test]
    fn render_text_sanitizes_terminal_escape_sequences_bidi_and_injected_newlines() {
        let report = Report {
            package: "pkg\x1b[31mred\x1b[0m\nFAKE".into(),
            sites: 1,
            findings: vec![Finding::AssertionGap {
                at: Location {
                    file: "src/\x1b]52;c;evil\x07lib.rs".into(),
                    line: 10,
                    column: 5,
                    code: "x\u{202e} == 1\r\nFAKE LINE".into(),
                    ..Location::default()
                },
                survivors: vec!["`x \x1b[2J!= 1`".into()],
                tests: vec!["tests::\u{009b}31mspoof".into()],
                effects: vec![],
                effects_of: None,
                witness: None,
            }],
            ..Report::default()
        };
        assert_eq!(report.schema_version, SCHEMA_VERSION);
        let json: serde_json::Value = serde_json::to_value(&report).unwrap();
        assert_eq!(json["schema_version"], SCHEMA_VERSION);
        // JSON remains lossless.
        assert!(json["package"].as_str().unwrap().contains('\x1b'));

        let text = render_text(&report, false);
        assert!(!text.contains('\x1b'), "{text:?}");
        assert!(!text.contains('\x07'), "{text:?}");
        assert!(!text.contains('\u{009b}'), "{text:?}");
        assert!(!text.contains('\u{202e}'), "{text:?}");
        assert!(!text.contains('\r'), "{text:?}");
        // Injected newlines in untrusted fields cannot start a forged line.
        assert!(!text.lines().any(|l| l.starts_with("FAKE")), "{text:?}");
        assert!(text.contains("src/lib.rs:10:5"), "{text:?}");
        assert!(text.contains("pkgred FAKE:"), "{text:?}");

        // C1 OSC/DCS, C1 ST terminator, nF ESC ( B, and incomplete OSC do not leak payloads
        // or swallow subsequent fields on the same line.
        assert_eq!(sanitize_inline("a\u{009d}0;title\u{009c}b"), "ab");
        assert_eq!(sanitize_inline("a\u{0090}dcs-payload\u{009c}b"), "ab");
        assert_eq!(sanitize_inline("a\x1b]0;title\u{009c}b"), "ab");
        assert_eq!(sanitize_inline("a\x1b(Bb"), "ab");
        let report_unclosed = Report {
            package: "p".into(),
            sites: 1,
            findings: vec![Finding::AssertionGap {
                at: Location {
                    file: "src/lib.rs".into(),
                    line: 1,
                    column: 1,
                    code: "x == 1".into(),
                    ..Location::default()
                },
                survivors: vec!["`a\x1b]0;unterminated".into(), "`b`".into()],
                tests: vec!["t".into()],
                effects: vec![],
                effects_of: None,
                witness: None,
            }],
            ..Report::default()
        };
        let unclosed_text = render_text(&report_unclosed, false);
        assert!(unclosed_text.contains("`a, `b`"), "{unclosed_text:?}");
    }

    #[test]
    fn bitor_bitxor_input_gap_advice_mentions_shared_bits_and_disjoint_equivalence() {
        let found = crate::instrument::discover("fn f(a: u8, b: u8) -> u8 { a | b }", 0).unwrap();
        let site = found
            .sites
            .iter()
            .find(|s| matches!(s.kind, SiteKind::Arith { .. }))
            .expect("arith site");
        let Some(Finding::InputGap { add_tests, .. }) =
            arith_gap_finding("src/lib.rs", site, Mutation::ArithSwap2, true)
        else {
            panic!("expected InputGap for | -> ^");
        };
        assert_eq!(add_tests.len(), 1);
        assert!(
            add_tests[0].contains("share at least one set bit")
                && add_tests[0].contains("disjoint by construction"),
            "{add_tests:?}"
        );
    }

    #[test]
    fn byte_and_char_literal_nearest_values_match_pict_table() {
        let table = include_str!("../tests/data/literal-nearest.pict.tsv");
        let mut rows = 0;
        for line in table.lines().skip(1).filter(|l| !l.trim().is_empty()) {
            let cols: Vec<&str> = line.split('\t').collect();
            assert_eq!(cols.len(), 4, "unexpected row: {line}");
            let (lit_kind, lit_side, cmp_op, obs_kind) = (cols[0], cols[1], cols[2], cols[3]);
            let (lit_text, lit_val): (&str, Option<i128>) = match lit_kind {
                "byte_ascii" => ("b'A'", Some(65)),
                "byte_escape" => ("b'\\n'", Some(10)),
                "byte_non_ascii" => ("b'\\xfe'", Some(254)),
                "char_ascii" => ("'A'", Some(65)),
                "char_unicode" => ("'あ'", Some('あ' as u32 as i128)),
                "int" => ("65", Some(65)),
                "none" => ("other", None),
                other => panic!("unexpected LitKind: {other}"),
            };
            let (lhs, rhs, literal) = match (lit_val, lit_side) {
                (Some(k), "rhs") => ("tag", lit_text, LiteralSide::Rhs(k)),
                (Some(k), "lhs") => (lit_text, "tag", LiteralSide::Lhs(k)),
                (None, _) => ("tag", "other", LiteralSide::None),
                other => panic!("unexpected LitSide: {other:?}"),
            };
            let obs = match obs_kind {
                "pos_only" => Obs::of_d(46),
                "neg_only" => Obs::of_d(-1),
                "both_sides" => {
                    let mut o = Obs::of_d(-2);
                    o.merge(&Obs::of_d(1));
                    o
                }
                "eq_only" => Obs::of_d(0),
                other => panic!("unexpected ObsKind: {other}"),
            };
            let observed = observed_values(lhs, rhs, literal, &obs).expect("valued obs");
            match lit_kind {
                "byte_ascii" => {
                    if lit_side == "rhs" && obs_kind == "pos_only" {
                        assert_eq!(
                            observed,
                            "nearest values the tests passed: `tag` = b'o' (111)"
                        );
                    } else {
                        assert!(
                            observed.contains("b'") && observed.contains('('),
                            "{observed}"
                        );
                    }
                }
                "byte_escape" => {
                    if lit_side == "rhs" && obs_kind == "eq_only" {
                        assert_eq!(
                            observed,
                            "nearest values the tests passed: `tag` = b'\\n' (10)"
                        );
                    } else {
                        assert!(
                            observed.contains("b'") && observed.contains('('),
                            "{observed}"
                        );
                    }
                }
                "byte_non_ascii" => {
                    if lit_side == "rhs" && obs_kind == "pos_only" {
                        // 254 + 46 = 300 > u8::MAX: falls back cleanly to plain integer.
                        assert_eq!(observed, "nearest values the tests passed: `tag` = 300");
                    } else {
                        assert!(observed.contains("b'\\x"), "{observed}");
                    }
                }
                "char_ascii" => {
                    if lit_side == "rhs" && obs_kind == "pos_only" {
                        assert_eq!(
                            observed,
                            "nearest values the tests passed: `tag` = 'o' (111)"
                        );
                    } else {
                        assert!(
                            observed.contains('\'') && !observed.contains("b'"),
                            "{observed}"
                        );
                    }
                }
                "char_unicode" => {
                    assert!(
                        observed.contains('\'') && !observed.contains("b'"),
                        "{observed}"
                    );
                }
                "int" => {
                    assert!(!observed.contains('\''), "{observed}");
                }
                "none" => {
                    assert!(observed.contains("`tag` - `other` = "), "{observed}");
                }
                _ => unreachable!(),
            }

            // Also check input_for and Shift description for ordering ops on both literal sides.
            if lit_kind == "byte_ascii" {
                let expected_input = "`tag` = b'A' (65)";
                assert_eq!(input_for(lhs, rhs, literal, 0), expected_input);
                if cmp_op != "eq" {
                    let op_sym = if cmp_op == "ge" { ">=" } else { "<" };
                    let expr = if lit_side == "rhs" {
                        format!("tag {op_sym} b'A'")
                    } else {
                        format!("b'A' {op_sym} tag")
                    };
                    let site = site_of(&format!("fn f(tag: u8) -> bool {{ {expr} }}"));
                    let expected = if lit_side == "rhs" {
                        format!("`tag {op_sym} b'B'`")
                    } else {
                        format!("`b'@' {op_sym} tag`")
                    };
                    assert_eq!(describe(&site, Mutation::Shift(1)), expected);
                }
            }
            if lit_kind == "char_ascii" {
                let expected_input = "`tag` = 'A' (65)";
                assert_eq!(input_for(lhs, rhs, literal, 0), expected_input);
                if cmp_op != "eq" {
                    let op_sym = if cmp_op == "ge" { ">=" } else { "<" };
                    let expr = if lit_side == "rhs" {
                        format!("tag {op_sym} 'A'")
                    } else {
                        format!("'A' {op_sym} tag")
                    };
                    let site = site_of(&format!("fn f(tag: char) -> bool {{ {expr} }}"));
                    let expected = if lit_side == "rhs" {
                        format!("`tag {op_sym} 'B'`")
                    } else {
                        format!("`'@' {op_sym} tag`")
                    };
                    assert_eq!(describe(&site, Mutation::Shift(1)), expected);
                }
            }
            rows += 1;
        }
        assert!(rows >= 25, "expected full PICT table, got {rows} rows");

        // Ordinal 0xD800 maps cleanly to '\u{e000}' (57344), skipping surrogates;
        // out-of-range ordinals fall back to plain integers without panicking.
        assert_eq!(
            format_literal_value(0xd800, LiteralDisplay::Char),
            "'\\u{e000}' (57344)"
        );
        assert_eq!(
            format_literal_value(0x10_d800, LiteralDisplay::Char),
            "1103872"
        );
    }
}
