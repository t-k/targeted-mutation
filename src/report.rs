//! Turns planner results into findings a person can act on: which wrong implementation
//! survives, and which input or assertion would catch it.

use serde::Serialize;

use crate::instrument::{ArithOp, LiteralSide, Site, SiteKind};
use crate::plan::{CmpOp, InputGap, LogicGap, Mutation, Obs};

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
        SiteKind::Not { operand, .. } => format!("`{}`", one_line(operand)),
        SiteKind::Return { name } => match m {
            Mutation::Const(value) => format!("`{name}` returning `{value}`"),
            _ => format!("`{name}` returning something else"),
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
            format!(
                "`{} {}{eq} {}`",
                one_line(lhs),
                op.swapped().symbol(),
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
                        (_, d) if d > 0 => format!("`{lhs} {sym} {rhs} + {d}`"),
                        (_, d) => format!("`{lhs} {sym} {rhs} - {}`", -d),
                    },
                },
                Mutation::ShiftF(d) => match (float_literal(&lhs), float_literal(&rhs)) {
                    (None, Some(k)) => format!("`{lhs} {sym} {}`", show_float(k + d)),
                    (Some(k), None) => format!("`{} {sym} {rhs}`", show_float(k - d)),
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
    let original = format!("`{lhs_text} {} {rhs_text}`", op.symbol());
    let mut survivors: Vec<String> = gap
        .survivors()
        .into_iter()
        .map(|m| describe(site, m))
        .collect();
    // A float shift too small to move the literal is not a different program.
    survivors.retain(|s| *s != original);
    let literal_value = float_literal(&lhs_text).or(float_literal(&rhs_text));
    let moved: Vec<String> = gap
        .survivors()
        .into_iter()
        .filter(|m| match (m, literal_value) {
            (Mutation::ShiftF(d), Some(k)) => k + d != k && k - d != k,
            _ => true,
        })
        .map(|m| describe(site, m))
        .collect();
    survivors.retain(|s| moved.contains(s));
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

/// Input gap at a `-> bool` function that returned the same value in every test.
pub fn return_gap_finding(
    file: &str,
    site: &Site,
    always: bool,
    verified: bool,
) -> Option<Finding> {
    let SiteKind::Return { name } = &site.kind else {
        return None;
    };
    Some(Finding::InputGap {
        at: location(file, site),
        survivors: vec![describe(site, Mutation::Const(always))],
        survivor_count: 1,
        add_tests: vec![format!("a case where `{name}` returns {}", !always)],
        observed: None,
        verified,
    })
}

/// Input gap at an arithmetic operator: the swapped operator computes the same value on
/// every pair of operands the tests pass.
pub fn arith_gap_finding(file: &str, site: &Site, verified: bool) -> Option<Finding> {
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
    let add = match op {
        ArithOp::Add | ArithOp::Sub => format!("`{rhs}` other than 0"),
        _ => format!(
            "operands for which `{lhs} {}{eq} {rhs}` and `{lhs} {}{eq} {rhs}` differ",
            op.symbol(),
            op.swapped().symbol()
        ),
    };
    Some(Finding::InputGap {
        at: location(file, site),
        survivors: vec![describe(site, Mutation::ArithSwap)],
        survivor_count: 1,
        add_tests: vec![add],
        observed: None,
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
            .find(|s| !matches!(s.kind, SiteKind::Return { .. }))
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
        assert_eq!(arith_gap_finding("f.rs", &and, true), None);

        let add = site_of("fn f(total: u32, fee: u32) -> u32 { total + fee }");
        assert_eq!(describe(&add, Mutation::ArithSwap), "`total - fee`");
        let Some(Finding::InputGap {
            survivors,
            add_tests,
            ..
        }) = arith_gap_finding("f.rs", &add, true)
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
        }) = arith_gap_finding("f.rs", &mul, true)
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
        let Some(Finding::InputGap { add_tests, .. }) = arith_gap_finding("f.rs", &assign, true)
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
        let Some(Finding::InputGap { add_tests, .. }) = arith_gap_finding("f.rs", &shift, true)
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
}
