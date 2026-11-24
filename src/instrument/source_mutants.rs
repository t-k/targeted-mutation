//! Source-level mutant edits for rebuild and audit modes.

use syn::spanned::Spanned;

use crate::plan::CmpOp;

use super::discover::written_bool;
use super::model::{ArmGuard, Site, SiteKind};

/// A mutant as a change of the source text: `replacement` in place of `start..end`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SourceMutant {
    pub start: usize,
    pub end: usize,
    pub replacement: String,
    /// What is replaced and by what, in the words of cargo-mutants: (`<`, `==`),
    /// (`-`, ``) for a deleted minus, (`guard`, `true`), (`body`, `0`).
    pub what: String,
    pub into: String,
}

/// The mutants of a site as changes of the source, for the sites that are mutated by
/// building them. The replacements of an operator are those of cargo-mutants. Sites that
/// only exist as probes (a result replaced after the body ran, the other of `min` /
/// `max`) have none.
/// A strict/non-strict boundary replacement for runs without operand observations.
pub(crate) fn source_boundary_mutant(site: &Site) -> Option<SourceMutant> {
    let SiteKind::Cmp { op, gap, .. } = &site.kind else {
        return None;
    };
    let replacement = match op {
        CmpOp::Le => "<",
        CmpOp::Ge => ">",
        _ => return None,
    };
    Some(SourceMutant {
        start: gap.0,
        end: gap.1,
        replacement: format!(" {replacement} "),
        what: op.symbol().to_string(),
        into: replacement.to_string(),
    })
}

/// Gives each mutant the line ends of what it replaces: the lines after it are the lines
/// they were, so `line!()` and the place of a panic say the same in the mutant.
pub fn keep_line_ends(source: &str, mutants: &mut [SourceMutant]) {
    for mutant in mutants {
        let Some(replaced) = source.get(mutant.start..mutant.end) else {
            continue;
        };
        let lines = replaced.matches('\n').count();
        let kept = mutant.replacement.matches('\n').count();
        mutant
            .replacement
            .push_str(&"\n".repeat(lines.saturating_sub(kept)));
    }
}

pub fn source_mutants(site: &Site) -> Vec<SourceMutant> {
    let change =
        |start: usize, end: usize, replacement: String, what: &str, into: &str| SourceMutant {
            start,
            end,
            replacement,
            what: what.to_string(),
            into: into.to_string(),
        };
    // A replacement that binds otherwise than the original keeps the operands it had:
    // `2 * 3` in `8 - 2 * 3` becomes `(2 + 3)`, and `8 - 2 * 3` becomes `(8) / (2 * 3)`.
    let operator = |gap: &(usize, usize), from: String, to: String, binds: std::cmp::Ordering| {
        // The operands as they are written, on either side of the operator.
        let operands = gap
            .0
            .checked_sub(site.start)
            .zip(gap.1.checked_sub(site.start))
            .and_then(|(left, right)| {
                Some((
                    site.text.get(..left)?.trim_end(),
                    site.text.get(right..)?.trim_start(),
                    // The line ends between the operands stay between them: each
                    // operand is on the lines it was on.
                    "\n".repeat(site.text.get(left..right)?.matches('\n').count()),
                ))
            });
        match (binds, operands) {
            (std::cmp::Ordering::Less, Some((left, right, lines))) => change(
                site.start,
                site.end,
                format!("({left} {to} {lines}{right})"),
                &from,
                &to,
            ),
            (std::cmp::Ordering::Greater, Some((left, right, lines))) => change(
                site.start,
                site.end,
                format!("({left}) {to} {lines}({right})"),
                &from,
                &to,
            ),
            _ => change(gap.0, gap.1, format!(" {to} "), &from, &to),
        }
    };
    let same = std::cmp::Ordering::Equal;
    match &site.kind {
        SiteKind::Cmp { op, gap, .. } => {
            let others: &[CmpOp] = match op {
                CmpOp::Lt => &[CmpOp::Eq, CmpOp::Gt, CmpOp::Le],
                CmpOp::Gt => &[CmpOp::Eq, CmpOp::Lt, CmpOp::Ge],
                CmpOp::Le => &[CmpOp::Gt],
                CmpOp::Ge => &[CmpOp::Lt],
                CmpOp::Eq => &[CmpOp::Ne],
                CmpOp::Ne => &[CmpOp::Eq],
            };
            others
                .iter()
                .map(|other| operator(gap, op.symbol().into(), other.symbol().into(), same))
                .collect()
        }
        SiteKind::Logic { and, gap, .. } => {
            // `&&` binds tighter than `||`.
            let (from, to, binds) = if *and {
                ("&&", "||", std::cmp::Ordering::Less)
            } else {
                ("||", "&&", std::cmp::Ordering::Greater)
            };
            vec![operator(gap, from.into(), to.into(), binds)]
        }
        SiteKind::Arith {
            op, assign, gap, ..
        } => {
            let eq = if *assign { "=" } else { "" };
            [Some(op.swapped()), op.second(*assign)]
                .into_iter()
                .flatten()
                .map(|other| {
                    operator(
                        gap,
                        format!("{}{eq}", op.symbol()),
                        format!("{}{eq}", other.symbol()),
                        // An assignment has nothing around it and its operands to itself.
                        if *assign {
                            same
                        } else {
                            other.binding().cmp(&op.binding())
                        },
                    )
                })
                .collect()
        }
        SiteKind::Not {
            bang, needs_space, ..
        } => vec![change(
            bang.0,
            bang.1,
            if *needs_space { " " } else { "" }.to_string(),
            "!",
            "",
        )],
        SiteKind::Neg {
            minus, needs_space, ..
        } => vec![change(
            minus.0,
            minus.1,
            if *needs_space { " " } else { "" }.to_string(),
            "-",
            "",
        )],
        SiteKind::Body { value, .. } => {
            vec![change(
                site.start,
                site.end,
                format!(" {value} "),
                "body",
                value,
            )]
        }
        SiteKind::Arm {
            pattern,
            guard,
            removal,
        } => vec![match guard {
            ArmGuard::None(_) => change(removal.0, removal.1, String::new(), "arm", pattern),
            ArmGuard::Existing(from, to) => change(*from, *to, "false".into(), "arm", pattern),
        }],
        SiteKind::Guard { can_be_false } => [true, false]
            .into_iter()
            .filter(|value| *value || *can_be_false)
            // Not the value the guard is written as.
            .filter(|value| {
                syn::parse_str::<syn::Expr>(&site.text)
                    .ok()
                    .and_then(|guard| written_bool(&guard))
                    != Some(*value)
            })
            .map(|value| {
                change(
                    site.start,
                    site.end,
                    value.to_string(),
                    "guard",
                    &value.to_string(),
                )
            })
            .collect(),
        SiteKind::Field { name, removal, .. } => {
            vec![change(removal.0, removal.1, String::new(), "field", name)]
        }
        SiteKind::Stmt => vec![change(site.start, site.end, String::new(), "statement", "")],
        // The end taken in or left out: the two dots and the `=` are all that changes.
        SiteKind::Range { inclusive, gap, .. } => {
            let (from, to) = if *inclusive {
                ("..=", "..")
            } else {
                ("..", "..=")
            };
            vec![change(gap.0, gap.1, to.to_string(), from, to)]
        }
        // The other method's name in place of the first: whatever is written around
        // it (a line that ends, a comment) stays.
        SiteKind::SliceEnd { first, method, .. } => {
            let (from, to) = if *first {
                ("first", "last")
            } else {
                ("last", "first")
            };
            vec![change(method.0, method.1, to.to_string(), from, to)]
        }
        SiteKind::StrAffix { starts, method, .. } => {
            let from = if *starts { "starts_with" } else { "ends_with" };
            vec![change(
                method.0,
                method.1,
                "contains".into(),
                from,
                "contains",
            )]
        }
        SiteKind::StrReplace {
            replace_all,
            from,
            to,
            method,
            close,
            ..
        } => {
            if *replace_all {
                vec![change(
                    method.0,
                    close.1,
                    format!("replacen({from}, {to}, 1)"),
                    "replace",
                    "replacen(..., 1)",
                )]
            } else {
                vec![change(
                    method.0,
                    close.1,
                    format!("replace({from}, {to})"),
                    "replacen",
                    "replace",
                )]
            }
        }
        SiteKind::Clamp {
            low,
            high,
            call,
            close,
            ..
        } => vec![
            change(
                call.0,
                close.1,
                format!(".min({high})"),
                "clamp",
                "min (drop lower)",
            ),
            change(
                call.0,
                close.1,
                format!(".max({low})"),
                "clamp",
                "max (drop upper)",
            ),
        ],
        SiteKind::Rounding { op, method, .. } => vec![
            change(
                method.0,
                method.1,
                op.alt1().name().into(),
                op.name(),
                op.alt1().name(),
            ),
            change(
                method.0,
                method.1,
                op.alt2().name().into(),
                op.name(),
                op.alt2().name(),
            ),
        ],
        SiteKind::Quantifier { all, method, .. } => {
            let (from, to) = if *all { ("all", "any") } else { ("any", "all") };
            vec![change(method.0, method.1, to.into(), from, to)]
        }
        SiteKind::LiteralSet { elements, .. } => elements
            .iter()
            .map(|(elem, start, end)| change(*start, *end, String::new(), "literal_set", elem))
            .collect(),
        SiteKind::Return { .. } | SiteKind::MinMax { .. } => vec![],
    }
}

/// Replace only the operator tokens, preserving their surrounding source.
pub(crate) fn literal_operator_mutants(source: &str, site: &Site, mutants: &mut [SourceMutant]) {
    let gap = match &site.kind {
        SiteKind::Cmp { gap, .. } | SiteKind::Logic { gap, .. } | SiteKind::Arith { gap, .. } => {
            gap
        }
        _ => return,
    };
    if let Ok(operator) = syn::parse_str::<syn::BinOp>(&source[gap.0..gap.1]) {
        let span = operator.span().byte_range();
        for mutant in mutants {
            mutant.start = gap.0 + span.start;
            mutant.end = gap.0 + span.end;
            mutant.replacement.clone_from(&mutant.into);
        }
    }
}
