//! A change undone as a mutant (`--since` with mutants built one by one): the code as it
//! was before, put back where the new code is. Tests that still pass do not tell the
//! change from what was there.

use crate::instrument;
use crate::scope::{self, Hunk};

/// One hunk of a file that is worth undoing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Change {
    pub hunk: Hunk,
    /// The first line of the change in the new text (1-based).
    pub line: usize,
    /// The new code of the change, or the line the old code was taken from before.
    pub code: String,
    /// The file with this change alone undone; `None` when that is not Rust any more.
    pub undone: Option<String>,
}

impl Change {
    /// What the mutant is, in the words of a finding.
    pub fn describe(&self) -> String {
        let old = self.hunk.old.trim();
        let new_lines = self.hunk.count;
        match (old.is_empty(), new_lines) {
            (true, 1) => "without this line (it is new)".to_string(),
            (true, _) => format!("without these {new_lines} lines (they are new)"),
            (false, 0) => format!(
                "with the code that was removed here: `{}`",
                crate::report::one_line(old)
            ),
            (false, _) => format!(
                "with the code as it was: `{}`",
                crate::report::one_line(old)
            ),
        }
    }
}

/// What a hunk of a file changes.
enum Kind {
    /// The program: worth trying undone.
    Program(Change),
    /// Code of the tests alone.
    Tests,
    /// No token (comments, documentation, spacing), or nothing that fits the text.
    Nothing,
}

fn classify(current: &str, hunks: &[Hunk]) -> Vec<(Hunk, Kind)> {
    let Ok(parsed) = instrument::parse_source(current) else {
        return vec![];
    };
    let tests_now = instrument::test_only_ranges(current, &parsed);
    let lines = current.matches('\n').count() + 1;
    let mut ordered: Vec<&Hunk> = hunks.iter().collect();
    ordered.sort_by_key(|hunk| (hunk.start, hunk.count));
    let mut found = vec![];
    for hunk in ordered {
        let mut kind = |kind| found.push((hunk.clone(), kind));
        let Some(undone) = scope::undo(current, &[hunk]) else {
            kind(Kind::Nothing);
            continue;
        };
        if instrument::same_tokens(current, &undone) == Some(true) {
            kind(Kind::Nothing);
            continue;
        }
        // Where the new lines are, and where the old ones are in the undone text.
        let (from, to) = scope::range_of(current, hunk);
        let inside = |ranges: &[(usize, usize)], from: usize, to: usize| {
            ranges
                .iter()
                .any(|(start, end)| *start <= from && to <= *end)
        };
        let undone_parsed = instrument::parse_source(&undone).ok();
        let new_is_tests = if from == to {
            // Nothing new: the place alone says nothing, unless the old code cannot.
            undone_parsed.is_some() || inside(&tests_now, from, to)
        } else {
            // Blank lines around the code belong to nothing.
            let text = &current[from..to];
            let spacing = text.len() - text.trim_start().len();
            inside(&tests_now, from + spacing, from + text.trim_end().len())
        };
        let old_is_tests = match (&undone_parsed, hunk.old.trim().is_empty()) {
            (_, true) => true,
            (Some(parsed), false) => {
                let tests_then = instrument::test_only_ranges(&undone, parsed);
                let spacing = hunk.old.len() - hunk.old.trim_start().len();
                inside(
                    &tests_then,
                    from + spacing,
                    from + hunk.old.trim_end().len(),
                )
            }
            (None, false) => true,
        };
        if from == to && hunk.old.trim().is_empty() {
            kind(Kind::Nothing);
            continue;
        }
        if new_is_tests && old_is_tests {
            kind(Kind::Tests);
            continue;
        }
        let line = if hunk.count == 0 {
            (hunk.start + 1).min(lines)
        } else {
            hunk.start
        };
        let code_line = current[scope::range_of(
            current,
            &Hunk {
                start: line,
                count: 1,
                ..hunk.clone()
            },
        )
        .0..]
            .lines()
            .find(|text| !text.trim().is_empty())
            .unwrap_or("");
        kind(Kind::Program(Change {
            hunk: hunk.clone(),
            line,
            code: crate::report::one_line(code_line.trim()),
            undone: undone_parsed.map(|_| undone),
        }));
    }
    found
}

/// The changes of one file to try undone, in the order of the file. Left out: a hunk that
/// changes no token (comments, documentation, spacing), and one that only touches code
/// of the tests. `current` is the file as it is now, and the hunks are of it.
pub(crate) fn changes(current: &str, hunks: &[Hunk]) -> Vec<Change> {
    classify(current, hunks)
        .into_iter()
        .filter_map(|(_, kind)| match kind {
            Kind::Program(change) => Some(change),
            Kind::Tests | Kind::Nothing => None,
        })
        .collect()
}

/// The hunks of a file that change its tests and nothing else.
pub(crate) fn test_hunks(current: &str, hunks: &[Hunk]) -> Vec<Hunk> {
    classify(current, hunks)
        .into_iter()
        .filter(|(_, kind)| matches!(kind, Kind::Tests))
        .map(|(hunk, _)| hunk)
        .collect()
}

/// The file with all of these changes undone; `None` when that is not Rust.
pub(crate) fn all_undone(current: &str, changes: &[Change]) -> Option<String> {
    let hunks: Vec<&Hunk> = changes.iter().map(|change| &change.hunk).collect();
    let undone = scope::undo(current, &hunks)?;
    instrument::parse_source(&undone).ok().map(|_| undone)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The changes between two texts, as git sees them.
    fn between(old: &str, new: &str) -> Vec<Change> {
        let dir = std::env::temp_dir().join(format!(
            "tmt-changes-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("old"), old).unwrap();
        std::fs::write(dir.join("new"), new).unwrap();
        let output = std::process::Command::new("git")
            .current_dir(&dir)
            .args(["diff", "--no-index", "-U0", "--no-color", "--no-ext-diff"])
            .args(["--src-prefix=a/", "--dst-prefix=b/", "old", "new"])
            .output()
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        changes(
            new,
            &scope::hunks(&String::from_utf8(output.stdout).unwrap()),
        )
    }

    const OLD: &str = "\
/// Whether a session is still good.
pub fn valid(created: u64, used: u64, now: u64) -> bool {
    // thirty minutes
    now - created < 30
}

pub fn label(n: u32) -> u32 {
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh() {
        assert!(valid(0, 0, 10));
    }
}
";

    #[test]
    fn a_change_of_the_program_is_tried_undone() {
        let new = OLD.replace("now - created < 30", "now - used < 30");
        let found = between(OLD, &new);
        assert_eq!(found.len(), 1, "{found:?}");
        let change = &found[0];
        assert_eq!(change.line, 4);
        assert_eq!(change.code, "now - used < 30");
        assert_eq!(change.undone.as_deref(), Some(OLD));
        assert_eq!(
            change.describe(),
            "with the code as it was: `now - created < 30`"
        );
        assert_eq!(all_undone(&new, &found).as_deref(), Some(OLD));
    }

    #[test]
    fn what_changes_no_token_or_only_the_tests_is_left_out() {
        for new in [
            // A comment, the documentation, the spacing.
            OLD.replace("// thirty minutes", "// half an hour"),
            OLD.replace("/// Whether a session is still good.", "/// Says if it is."),
            OLD.replace("    n\n", "        n\n"),
            OLD.replace("    // thirty minutes\n", ""),
            OLD.replace("pub fn label", "\n\npub fn label"),
            // A test that is new, changed, or gone, and a helper of the tests.
            OLD.replace("valid(0, 0, 10)", "valid(0, 0, 20)"),
            OLD.replace(
                "    #[test]\n    fn fresh() {",
                "    #[test]\n    fn other() {\n        assert!(!valid(0, 0, 40));\n    }\n\n    #[test]\n    fn fresh() {",
            ),
            OLD.replace(
                "    #[test]\n    fn fresh() {\n        assert!(valid(0, 0, 10));\n    }\n",
                "",
            ),
            OLD.replace("    use super::*;\n", "    use super::*;\n\n    fn ten() -> u64 { 10 }\n"),
            format!("{OLD}\n#[test]\nfn outside() {{\n    assert!(valid(1, 1, 2));\n}}\n"),
        ] {
            assert_ne!(new, OLD);
            assert_eq!(between(OLD, &new), [], "forwards: {new}");
            // The other way round: the same holds for what was there before.
            assert_eq!(between(&new, OLD), [], "backwards: {new}");
        }
    }

    #[test]
    fn the_changes_of_the_tests_are_told_from_the_rest() {
        let weaker = OLD.replace("assert!(valid(0, 0, 10));", "let _ = valid(0, 0, 10);");
        let both = weaker.replace("now - created < 30", "now - used < 30");
        let diff = |old: &str, new: &str| {
            let dir = std::env::temp_dir().join(format!(
                "tmt-test-hunks-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("old"), old).unwrap();
            std::fs::write(dir.join("new"), new).unwrap();
            let output = std::process::Command::new("git")
                .current_dir(&dir)
                .args(["diff", "--no-index", "-U0", "--no-color", "--no-ext-diff"])
                .args(["--src-prefix=a/", "--dst-prefix=b/", "old", "new"])
                .output()
                .unwrap();
            std::fs::remove_dir_all(&dir).unwrap();
            scope::hunks(&String::from_utf8(output.stdout).unwrap())
        };
        let hunks = diff(OLD, &both);
        assert_eq!(hunks.len(), 2);
        let of_tests = test_hunks(&both, &hunks);
        assert_eq!(of_tests.len(), 1, "{of_tests:?}");
        assert_eq!(of_tests[0].old.trim(), "assert!(valid(0, 0, 10));");
        assert_eq!(changes(&both, &hunks).len(), 1);
        // With the tests' change undone the program's change stays.
        let hunk_refs: Vec<&Hunk> = of_tests.iter().collect();
        assert_eq!(
            scope::undo(&both, &hunk_refs).unwrap(),
            OLD.replace("now - created < 30", "now - used < 30")
        );
        // A comment in the tests is no change of them.
        let commented = OLD.replace(
            "        assert!(valid",
            "        // sure\n        assert!(valid",
        );
        assert_eq!(test_hunks(&commented, &diff(OLD, &commented)), []);
        assert_eq!(test_hunks("fn f( {", &hunks), []);
    }

    #[test]
    fn code_that_is_new_or_gone_is_described_as_such() {
        let new = OLD.replace("    n\n", "    let n = n + 1;\n    n\n");
        let found = between(OLD, &new);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].describe(), "without this line (it is new)");
        assert_eq!(
            (found[0].line, found[0].code.as_str()),
            (8, "let n = n + 1;")
        );
        let back = between(&new, OLD);
        assert_eq!(
            back[0].describe(),
            "with the code that was removed here: `let n = n + 1;`"
        );
        assert_eq!((back[0].line, back[0].code.as_str()), (8, "n"));
        let two = OLD.replace("    n\n", "    let n = n + 1;\n    let n = n * 2;\n    n\n");
        assert_eq!(
            between(OLD, &two)[0].describe(),
            "without these 2 lines (they are new)"
        );
    }

    #[test]
    fn a_change_that_is_not_rust_alone_is_kept_for_the_whole() {
        // The parameter and its use change together: neither alone is a program that
        // parses differently, but one alone does not build. A brace that moves does not
        // even parse alone.
        let new = OLD.replace("    n\n}", "    if n > 1 {\n    n\n    }\n}");
        let found = between(OLD, &new);
        assert!(found.len() >= 2, "{found:?}");
        assert!(
            found.iter().any(|change| change.undone.is_none()),
            "{found:?}"
        );
        assert_eq!(all_undone(&new, &found).as_deref(), Some(OLD));
        // A file that is not Rust now has nothing to try.
        assert_eq!(changes("fn f( {", &[]), []);
    }
}
