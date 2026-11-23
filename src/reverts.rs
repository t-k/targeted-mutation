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
    /// Where the change is in the file as it is now, and what is put there to undo
    /// it: the old lines, with the lines of the tests as they are now.
    pub range: (usize, usize),
    pub put_back: String,
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
    /// The program and its tests in lines that cannot be told apart: undone as a whole
    /// it would put the old tests back too, which says nothing of the tests as they are.
    Mixed,
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
        let touches = |ranges: &[(usize, usize)], from: usize, to: usize| {
            ranges.iter().any(|(start, end)| *start < to && from < *end)
        };
        // The code of each side without the blank lines around it, which belong to
        // nothing.
        let trimmed = |text: &str, at: usize| {
            let spacing = text.len() - text.trim_start().len();
            (at + spacing, at + text.trim_end().len())
        };
        let undone_parsed = instrument::parse_source(&undone).ok();
        let (new_from, new_to) = trimmed(&current[from..to], from);
        let (old_from, old_to) = trimmed(&hunk.old, from);
        let tests_then = undone_parsed
            .as_ref()
            .map(|parsed| instrument::test_only_ranges(&undone, parsed));
        let new_empty = new_from >= new_to;
        let old_empty = old_from >= old_to;
        if new_empty && old_empty {
            kind(Kind::Nothing);
            continue;
        }
        // Each side: all of it code of the tests, some of it, or none.
        let new_all = new_empty || inside(&tests_now, new_from, new_to);
        let new_some = !new_empty && touches(&tests_now, new_from, new_to);
        let (old_all, old_some) = match &tests_then {
            _ if old_empty => (true, false),
            Some(tests) => (
                inside(tests, old_from, old_to),
                touches(tests, old_from, old_to),
            ),
            // Old lines that are not Rust alone: where they go says what they are.
            None => {
                let around =
                    inside(&tests_now, from, to) || (new_empty && inside(&tests_now, from, from));
                (around, around)
            }
        };
        if new_all && old_all {
            kind(Kind::Tests);
            continue;
        }
        let mut undone = undone;
        let mut undone_parsed = undone_parsed;
        if new_some || old_some {
            // Line for line the tests can be kept as they are and the program put
            // back; otherwise the two cannot be told apart.
            let Some(split) = keep_tests(current, hunk, from, to, &tests_now) else {
                kind(Kind::Mixed);
                continue;
            };
            if instrument::same_tokens(current, &split) == Some(true) {
                kind(Kind::Nothing);
                continue;
            }
            undone_parsed = instrument::parse_source(&split).ok();
            undone = split;
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
        // What stands between the text before the change and the text after it.
        let put_back = undone[from..undone.len() - (current.len() - to)].to_string();
        kind(Kind::Program(Change {
            hunk: hunk.clone(),
            line,
            code: crate::report::one_line(code_line.trim()),
            undone: undone_parsed.map(|_| undone),
            range: (from, to),
            put_back,
        }));
    }
    found
}

/// `current` with the lines of a hunk put back that are not code of the tests, when the
/// hunk has as many old lines as new ones: each old line then stands for the new line
/// at its place. `None` when it has not, or no line of it is the program's.
fn keep_tests(
    current: &str,
    hunk: &Hunk,
    from: usize,
    to: usize,
    tests_now: &[(usize, usize)],
) -> Option<String> {
    let new_lines: Vec<&str> = current[from..to].split_inclusive('\n').collect();
    let old_lines: Vec<&str> = hunk.old.split_inclusive('\n').collect();
    if new_lines.len() != old_lines.len() || new_lines.is_empty() {
        return None;
    }
    let mut out = String::from(&current[..from]);
    let mut at = from;
    let mut put_back = 0;
    for (new, old) in new_lines.iter().zip(&old_lines) {
        let code = new.trim();
        let start = at + (new.len() - new.trim_start().len());
        let of_tests = code.is_empty()
            || tests_now
                .iter()
                .any(|(first, last)| *first <= start && start + code.len() <= *last);
        if of_tests {
            out.push_str(new);
        } else {
            out.push_str(old);
            put_back += 1;
        }
        at += new.len();
    }
    out.push_str(&current[to..]);
    (put_back > 0).then_some(out)
}

/// The changes of one file to try undone, in the order of the file. Left out: a hunk that
/// changes no token (comments, documentation, spacing), and one that only touches code
/// of the tests. `current` is the file as it is now, and the hunks are of it.
pub(crate) fn changes(current: &str, hunks: &[Hunk]) -> Vec<Change> {
    classify(current, hunks)
        .into_iter()
        .filter_map(|(_, kind)| match kind {
            Kind::Program(change) => Some(change),
            Kind::Tests | Kind::Mixed | Kind::Nothing => None,
        })
        .collect()
}

/// How many hunks of a file change the program and its tests in lines that cannot be
/// told apart: nothing is said of them.
pub(crate) fn mixed(current: &str, hunks: &[Hunk]) -> usize {
    classify(current, hunks)
        .iter()
        .filter(|(_, kind)| matches!(kind, Kind::Mixed))
        .count()
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
    // Each as it is undone alone: the tests stay as they are, here too.
    let mut ordered: Vec<&Change> = changes.iter().collect();
    ordered.sort_by_key(|change| std::cmp::Reverse(change.range));
    let mut undone = current.to_string();
    let mut before = current.len();
    for change in ordered {
        let (from, to) = change.range;
        if to > before {
            return None;
        }
        undone.replace_range(from..to, &change.put_back);
        before = from;
    }
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

    /// A limit and the test of it on the lines next to it: one hunk for git.
    const NEXT_TO: &str = "\
pub fn limit() -> u32 { 41 }
#[test] fn the_limit() { assert_eq!(limit(), 41); }
";

    #[test]
    fn the_program_is_put_back_and_the_tests_are_kept() {
        let new = NEXT_TO.replace("41", "42");
        let found = between(NEXT_TO, &new);
        assert_eq!(found.len(), 1, "{found:?}");
        // The old limit with the test as it is now: that is what the tests are asked.
        let wanted = NEXT_TO.replace("limit(), 41", "limit(), 42");
        assert_eq!(found[0].undone.as_deref(), Some(wanted.as_str()));
        // Undone together with others, the test is kept as well.
        assert_eq!(all_undone(&new, &found).as_deref(), Some(wanted.as_str()));
        let renamed = NEXT_TO.to_string()
            + "\npub fn kept() {}\n\nfn helper() -> u32 { 1 }\n\npub fn uses() -> u32 { helper() }\n";
        let new = renamed.replace("41", "42").replace("helper", "one");
        let found = between(&renamed, &new);
        // The definition and the use do not build one without the other: all three
        // changes go back together, and the test stays.
        assert_eq!(found.len(), 3, "{found:?}");
        assert_eq!(
            all_undone(&new, &found).as_deref(),
            Some(renamed.replace("limit(), 41", "limit(), 42").as_str())
        );
        // The test first and the program after it: the same.
        let (first, second) = NEXT_TO.split_once('\n').unwrap();
        let old = format!("{second}{first}\n");
        let new = old.replace("41", "42");
        let found = between(&old, &new);
        assert_eq!(
            found[0].undone.as_deref(),
            Some(old.replace("limit(), 41", "limit(), 42").as_str())
        );
    }

    #[test]
    fn what_cannot_be_told_apart_is_not_tried() {
        // The same change with a line more on one side: the old lines no longer stand
        // for the new ones one by one.
        let old = NEXT_TO.to_string();
        let new = old.replace("{ 41 }", "{ 42 }").replace(
            "{ assert_eq!(limit(), 41); }",
            "{\n    assert_eq!(limit(), 42); }",
        );
        let dir_hunks = |old: &str, new: &str| {
            let dir = std::env::temp_dir().join(format!(
                "tmt-mixed-{}-{:?}",
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
        let hunks = dir_hunks(&old, &new);
        assert_eq!(hunks.len(), 1, "{hunks:?}");
        assert_eq!(changes(&new, &hunks), []);
        assert_eq!(test_hunks(&new, &hunks), []);
        assert_eq!(mixed(&new, &hunks), 1);
        // And the other way round.
        let back = dir_hunks(&new, &old);
        assert_eq!(changes(&old, &back), []);
        assert_eq!(mixed(&old, &back), 1);
        // A change of the program alone, or of the tests alone, is not mixed.
        let program = old.replace("{ 41 }", "{ 42 }");
        assert_eq!(mixed(&program, &dir_hunks(&old, &program)), 0);
        assert_eq!(changes(&program, &dir_hunks(&old, &program)).len(), 1);
        let tests = old.replace("limit(), 41", "limit(), 40 + 1");
        assert_eq!(mixed(&tests, &dir_hunks(&old, &tests)), 0);
        assert_eq!(test_hunks(&tests, &dir_hunks(&old, &tests)).len(), 1);
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
