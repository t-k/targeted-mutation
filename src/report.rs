//! Turns planner results into findings a person can act on: which wrong implementation
//! survives, and which input or assertion would catch it.

use serde::Serialize;

use crate::instrument::{LiteralSide, Site, SiteKind};
use crate::plan::{CmpOp, InputGap, Mutation, Obs};

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Location {
    pub file: String,
    pub line: usize,
    pub column: usize,
    pub code: String,
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
    },
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Report {
    pub package: String,
    pub sites: usize,
    /// Sites that could not be instrumented (the probe did not compile there).
    pub skipped_sites: usize,
    pub mutants_run: usize,
    pub mutants_killed: usize,
    /// Killed mutants that were stopped by the timeout (typically an endless loop).
    pub mutants_timed_out: usize,
    pub findings: Vec<Finding>,
    /// Code no test executes.
    pub unreached: Vec<Location>,
    /// Survivors predicted from the trace that a re-run killed: the tests are not deterministic.
    pub unstable: Vec<Location>,
}

impl Report {
    pub fn has_gaps(&self) -> bool {
        !self.findings.is_empty()
    }
}

pub fn location(file: &str, site: &Site) -> Location {
    Location {
        file: file.to_string(),
        line: site.line,
        column: site.column,
        code: one_line(&site.text),
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

/// Source-level form of a mutant.
pub fn describe(site: &Site, m: Mutation) -> String {
    let SiteKind::Cmp {
        op,
        lhs,
        rhs,
        literal,
        ..
    } = &site.kind
    else {
        return format!("delete `{}`", one_line(&site.text));
    };
    let (lhs, rhs) = (one_line(lhs), one_line(rhs));
    let sym = op.symbol();
    match m {
        Mutation::Const(c) => format!("`{c}`"),
        Mutation::Negate => format!("`!({lhs} {sym} {rhs})`"),
        Mutation::Op(op2) => format!("`{lhs} {} {rhs}`", op2.symbol()),
        Mutation::Delete => format!("delete `{}`", one_line(&site.text)),
        Mutation::Shift(d) => match literal {
            LiteralSide::Rhs(k) => format!("`{lhs} {sym} {}`", k + d),
            // k op rhs + d  <=>  k - d op rhs
            LiteralSide::Lhs(k) => format!("`{} {sym} {rhs}`", k - d),
            LiteralSide::None => match (op, d) {
                (CmpOp::Ge, 1) => format!("`{lhs} > {rhs}`"),
                (CmpOp::Gt, -1) => format!("`{lhs} >= {rhs}`"),
                (CmpOp::Le, -1) => format!("`{lhs} < {rhs}`"),
                (CmpOp::Lt, 1) => format!("`{lhs} <= {rhs}`"),
                (_, d) if d > 0 => format!("`{lhs} {sym} {rhs} + {d}`"),
                (_, d) => format!("`{lhs} {sym} {rhs} - {}`", -d),
            },
        },
    }
}

/// The operand values that give `lhs - rhs = d`, in words.
fn input_for(lhs: &str, rhs: &str, literal: LiteralSide, d: i128) -> String {
    match (literal, d) {
        (LiteralSide::Rhs(k), d) => format!("`{lhs}` = {}", k + d),
        (LiteralSide::Lhs(k), d) => format!("`{rhs}` = {}", k - d),
        (LiteralSide::None, 0) => format!("`{lhs}` equal to `{rhs}`"),
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
    let (survivors, survivor_count, add_tests) = match gap {
        InputGap::Unreached => return None,
        InputGap::OneSided { always } => (
            vec![describe(site, Mutation::Const(*always))],
            1,
            vec![format!(
                "an input that makes `{}` {}",
                one_line(&site.text),
                !always
            )],
        ),
        InputGap::EqualNeverObserved { flipped } => (
            vec![describe(site, Mutation::Op(*flipped))],
            1,
            vec![format!(
                "`{lhs_text}` equal to `{rhs_text}` (the original gives {}, the mutant {})",
                op.eval_d(0),
                flipped.eval_d(0)
            )],
        ),
        InputGap::Boundary {
            min_shift,
            max_shift,
            missing,
        } => {
            // Nearest mutants first: +1, -1, +2, -2, ...
            let mut shifts: Vec<i128> = (1..=3)
                .flat_map(|n| [n, -n])
                .filter(|s| min_shift <= s && s <= max_shift)
                .collect();
            shifts.truncate(3);
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
            (
                shifts
                    .into_iter()
                    .map(|s| describe(site, Mutation::Shift(s)))
                    .collect(),
                (max_shift - min_shift) as u128,
                add,
            )
        }
    };
    Some(Finding::InputGap {
        at: location(file, site),
        survivors,
        survivor_count,
        add_tests,
        observed: observed_values(&lhs_text, &rhs_text, *literal, obs),
        verified,
    })
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

pub fn render_text(report: &Report, show_unreached: bool) -> String {
    let mut out = String::new();
    for finding in &report.findings {
        match finding {
            Finding::InputGap {
                at,
                survivors,
                survivor_count,
                add_tests,
                observed,
                verified,
            } => {
                out.push_str(&format!(
                    "INPUT GAP  {}:{}:{}\n    {}\n",
                    at.file, at.line, at.column, at.code
                ));
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
                out.push_str(&format!(
                    "  passes every test as: {}{more}  [{note}]\n",
                    survivors.join(", ")
                ));
                if let Some(observed) = observed {
                    out.push_str(&format!("  {observed}\n"));
                }
                for test in add_tests {
                    out.push_str(&format!("  add a test with {test}\n"));
                }
            }
            Finding::AssertionGap {
                at,
                survivors,
                tests,
            } => {
                out.push_str(&format!(
                    "ASSERTION GAP  {}:{}:{}\n    {}\n",
                    at.file, at.line, at.column, at.code
                ));
                out.push_str(&format!(
                    "  passes every test as: {}\n",
                    survivors.join(", ")
                ));
                out.push_str(&format!(
                    "  these tests run the changed code and still pass: {}\n",
                    list(tests, 5)
                ));
                out.push_str("  add an assertion on what this code is responsible for\n");
            }
        }
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
            "UNSTABLE  {}:{}:{}\n    {}\n  the trace predicted a survivor but a re-run failed: the tests are not deterministic\n\n",
            at.file, at.line, at.column, at.code
        ));
    }
    let input = report
        .findings
        .iter()
        .filter(|f| matches!(f, Finding::InputGap { .. }))
        .count();
    let assertion = report.findings.len() - input;
    out.push_str(&format!(
        "{}: {} sites, {} input gaps, {} assertion gaps, {} mutants run ({} killed), {} sites unreached",
        report.package,
        report.sites,
        input,
        assertion,
        report.mutants_run,
        report.mutants_killed,
        report.unreached.len()
    ));
    if report.skipped_sites > 0 {
        out.push_str(&format!(
            ", {} sites skipped (probe did not compile)",
            report.skipped_sites
        ));
    }
    out.push('\n');
    if !show_unreached && !report.unreached.is_empty() {
        out.push_str("(--show-unreached lists the code no test executes)\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::instrument::discover;

    fn site_of(src: &str) -> Site {
        discover(src, 0).unwrap().sites.into_iter().next().unwrap()
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
        assert_eq!(describe(&none, Mutation::Shift(2)), "`a >= b + 2`");
        assert_eq!(describe(&none, Mutation::Shift(-1)), "`a >= b - 1`");
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
                    code: "age >= 18".into()
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
        let gap = InputGap::EqualNeverObserved { flipped: CmpOp::Le };
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
        };
        let report = Report {
            package: "p".into(),
            sites: 2,
            mutants_run: 1,
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
                    at,
                    survivors: vec!["`!(a >= b)`".into()],
                    tests: vec!["t".into()],
                },
            ],
            ..Report::default()
        };
        assert_eq!(
            render_text(&report, false),
            "INPUT GAP  src/lib.rs:3:9\n    a >= b\n\
             \x20 passes every test as: `a > b` (+3 more)  [from the trace, not re-run]\n\
             \x20 nearest values the tests passed: `a` - `b` = -1, 4\n\
             \x20 add a test with `a` equal to `b` (the original gives true)\n\
             \n\
             INPUT GAP  src/lib.rs:3:9\n    a >= b\n\
             \x20 passes every test as: `true`  [re-run: tests pass]\n\
             \n\
             ASSERTION GAP  src/lib.rs:3:9\n    a >= b\n\
             \x20 passes every test as: `!(a >= b)`\n\
             \x20 these tests run the changed code and still pass: t\n\
             \x20 add an assertion on what this code is responsible for\n\
             \n\
             p: 2 sites, 2 input gaps, 1 assertion gaps, 1 mutants run (0 killed), 0 sites unreached\n"
        );
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
        };
        let report = Report {
            package: "p".into(),
            sites: 4,
            skipped_sites: 1,
            mutants_run: 3,
            mutants_killed: 2,
            mutants_timed_out: 0,
            findings: vec![Finding::AssertionGap {
                at: at.clone(),
                survivors: vec!["delete `s.check()?;`".into()],
                tests: (0..7).map(|i| format!("tests::t{i}")).collect(),
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
        assert!(text.contains("--show-unreached"), "{text}");
        assert!(render_text(&report, true).contains("UNREACHED  src/lib.rs:3:9"));
        assert!(!Report::default().has_gaps());
    }
}
