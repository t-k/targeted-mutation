//! Diff scope resolution, compile-error site blaming, domain inference, and audit verdicts.

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::instrument::{self, LiteralSide};
use crate::scope::Scope;

use super::*;

/// Whether re-running a surviving mutant against all tests proved the selective run missed a kill.
/// An unevaluated run or an unconfirmed parallel timeout is not evidence of disagreement.
pub(super) fn audit_all_tests_disagrees(again: &JobResult) -> bool {
    again.survived.is_none() && !again.unable && !again.unconfirmed_timeout
}

/// Compares a probe job verdict with its source-built twin.
/// Returns `None` when either side cannot be judged, or `Some(disagrees)`.
pub(super) fn audit_source_comparison(probe: &JobResult, built: Option<Outcome>) -> Option<bool> {
    if probe.unable || probe.unconfirmed_timeout {
        return None;
    }
    let built = built.filter(|outcome| *outcome != Outcome::Unable)?;
    let as_probe = probe.survived.is_some();
    Some((built == Outcome::Pass) != as_probe)
}

/// The lines to analyse, from `--since` and `--lines`; `None` for everything. File names
/// are relative to the workspace root.
pub(super) fn requested_scope(
    options: &Options,
    workspace: &Path,
    package_dir: &Path,
    copy: &Path,
) -> Result<Option<Scope>> {
    if options.since.is_none() && options.lines.is_empty() {
        return Ok(None);
    }
    // `--lines` names files the way the report does: relative to the package.
    let prefix = package_dir.strip_prefix(copy).unwrap_or(Path::new(""));
    let lines: Vec<String> = options
        .lines
        .iter()
        .map(|spec| normalize(&prefix.join(spec)).to_string_lossy().into_owned())
        .collect();
    let mut scope = Scope::parse_lines(&lines)?;
    if let Some(revision) = &options.since {
        scope.merge(Scope::from_diff(&since_diff(workspace, revision)?));
        // `-z`: the names as they are, not quoted.
        for file in git_output(
            workspace,
            &["ls-files", "--others", "--exclude-standard", "-z"],
        )?
        .split('\0')
        .filter(|file| !file.is_empty())
        {
            scope.add_whole_file(file);
        }
    }
    Ok(Some(scope))
}

pub(super) fn git_output(workspace: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(workspace)
        .args(args)
        .output()
        .context("running git")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// What changed in the workspace since a revision, as `git diff -U0` says it.
pub(super) fn since_diff(workspace: &Path, revision: &str) -> Result<String> {
    // `--relative` gives paths from the workspace root even inside a larger repository.
    git_output(
        workspace,
        &[
            "diff",
            "-U0",
            "--inter-hunk-context=0",
            "--no-color",
            "--no-ext-diff",
            "--relative",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            revision,
            "--",
        ],
    )
}

/// The sites to disable for one compile error, from the most to the least specific guess:
/// the innermost site at a primary span, at a secondary span, any site on the line of a
/// primary span, and finally every site of the file.
pub(super) fn blame_error(
    error: &CompileError,
    files: &[SourceFile],
    copy: &Path,
    disabled: &HashSet<u32>,
) -> Vec<u32> {
    let enabled = |id: u32| !disabled.contains(&id);
    let file_of = |span: &ErrorSpan| {
        let path = copy.join(&span.file);
        files.iter().find(|f| f.path == path)
    };
    let innermost = |spans: &[ErrorSpan]| -> Vec<u32> {
        let mut ids = vec![];
        for span in spans {
            if let Some(file) = file_of(span) {
                let rendered = file.rendered.as_ref().expect("rendered before building");
                // Inside the text of a probe, that probe is the one that does not fit.
                match rendered
                    .inserted_by(span.byte_start)
                    .filter(|id| enabled(*id))
                {
                    Some(id) => ids.push(id),
                    None => ids.extend(instrument::blame(
                        &file.sites,
                        &enabled,
                        rendered.to_original(span.byte_start),
                    )),
                }
            }
        }
        ids
    };
    let widen = |lines: usize| -> Vec<u32> {
        let mut ids = vec![];
        for span in &error.primary {
            if let Some(file) = file_of(span) {
                ids.extend(
                    file.sites
                        .iter()
                        .filter(|s| enabled(s.id) && s.line.abs_diff(span.line) <= lines)
                        .map(|s| s.id),
                );
            }
        }
        ids
    };
    [&error.primary, &error.secondary]
        .into_iter()
        .map(|spans| innermost(spans))
        .chain([0, NEARBY_LINES, usize::MAX].into_iter().map(widen))
        .find(|ids| !ids.is_empty())
        .unwrap_or_default()
}

/// Range of `D = lhs - rhs` that the operand types allow, when one side is a literal.
pub(super) fn domain_of(
    literal: LiteralSide,
    bounds: Option<&crate::trace::TypeBounds>,
) -> Option<(i128, i128)> {
    let bounds = bounds?;
    // A literal past the values of the type it is compared with is another number
    // there (`256i8` is 0 where that is allowed): nothing is known of the difference.
    let within = |k: i128, (low, high): (i128, i128)| (low <= k && k <= high).then_some(k);
    match literal {
        LiteralSide::Rhs(k) => {
            let k = within(k, bounds.lhs)?;
            Some((bounds.lhs.0.checked_sub(k)?, bounds.lhs.1.checked_sub(k)?))
        }
        LiteralSide::Lhs(k) => {
            let k = within(k, bounds.rhs)?;
            Some((k.checked_sub(bounds.rhs.1)?, k.checked_sub(bounds.rhs.0)?))
        }
        LiteralSide::None => None,
    }
}
