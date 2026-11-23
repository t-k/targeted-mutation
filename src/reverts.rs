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
    /// Set by who asks: the change is of code the build leaves out now and undone. It
    /// is not tried alone, and still goes back with the others.
    pub outside: bool,
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

impl Change {
    /// Whether undoing the change gives the same program for the build: then there is
    /// nothing to try. `off_now` are the ranges the build leaves out of the file as it
    /// is, `off_then` those of the file with this change undone (`None` when that is
    /// not known: the change is then tried). A change of what decides it (a `cfg` put
    /// on, taken off or reworded) leaves other code out before than after.
    pub fn outside_the_build(
        &self,
        current: &str,
        off_now: &[(usize, usize)],
        off_then: Option<&[(usize, usize)]>,
    ) -> bool {
        match (&self.undone, off_then) {
            (Some(undone), Some(off_then)) => same_build(current, off_now, undone, off_then),
            _ => false,
        }
    }
}

/// Whether two texts of a file are the same program for a build that leaves these
/// ranges of each out: the same tokens in what is left. Where what is left asks where
/// it is in the file, any other text is another program.
pub(crate) fn same_build(
    now: &str,
    off_now: &[(usize, usize)],
    then: &str,
    off_then: &[(usize, usize)],
) -> bool {
    let built = |text: &str, off: &[(usize, usize)]| {
        let mut ranges = off.to_vec();
        ranges.sort_unstable();
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for (from, to) in ranges {
            if from >= at && to <= text.len() {
                out.push_str(&text[at..from]);
                out.push(' ');
                at = to;
            }
        }
        out.push_str(&text[at..]);
        out
    };
    let (built_now, built_then) = (built(now, off_now), built(then, off_then));
    if instrument::same_tokens(&built_now, &built_then) != Some(true) {
        return false;
    }
    // Code that asks where it is can tell a line more above it, or a column more
    // before it on its line. `line!` and `column!` say where they are themselves: those
    // have to be where they were. The location of a caller can be that of any call:
    // then every token that is kept has to be.
    let asked = asks_where_it_is(&built_now).max(asks_where_it_is(&built_then));
    if asked == Asks::Nothing {
        return true;
    }
    matches!(
        (places(now, off_now, asked), places(then, off_then, asked)),
        (Some(a), Some(b)) if a == b
    )
}

/// What code asks of its own place in the file.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Asks {
    Nothing,
    /// `line!` or `column!`: where the macro is written.
    ItsOwnPlace,
    /// `#[track_caller]` or `Location::caller`: where something is called from.
    APlaceOfACall,
}

type Trees = std::iter::Peekable<proc_macro2::token_stream::IntoIter>;

fn next_is(trees: &mut Trees, mark: char) -> bool {
    matches!(trees.peek(), Some(proc_macro2::TokenTree::Punct(p)) if p.as_char() == mark)
}

/// The line and column of the tokens of a text that are not in the ranges left out and
/// whose place may be asked for: every one (documentation aside), or the `line!` and
/// `column!` alone. `None` when the text does not lex.
fn places(text: &str, off: &[(usize, usize)], asked: Asks) -> Option<Vec<(usize, usize)>> {
    fn note(span: proc_macro2::Span, off: &[(usize, usize)], out: &mut Vec<(usize, usize)>) {
        let range = span.byte_range();
        if !off
            .iter()
            .any(|(from, to)| *from <= range.start && range.end <= *to)
        {
            out.push((span.start().line, span.start().column));
        }
    }
    fn walk(
        stream: proc_macro2::TokenStream,
        off: &[(usize, usize)],
        all: bool,
        out: &mut Vec<(usize, usize)>,
    ) {
        let mut trees: Trees = stream.into_iter().peekable();
        while let Some(tree) = trees.next() {
            match tree {
                proc_macro2::TokenTree::Punct(hash) if hash.as_char() == '#' => {
                    // Documentation, as a comment or written out, outer or inner, is
                    // no place of the program (as it is no token of it).
                    let mut ahead = trees.clone();
                    if next_is(&mut ahead, '!') {
                        ahead.next();
                    }
                    if matches!(ahead.peek(), Some(proc_macro2::TokenTree::Group(group))
                        if group.delimiter() == proc_macro2::Delimiter::Bracket
                            && matches!(group.stream().into_iter().next(),
                                Some(proc_macro2::TokenTree::Ident(name)) if name == "doc"))
                    {
                        ahead.next();
                        trees = ahead;
                        continue;
                    }
                    if all {
                        note(hash.span(), off, out);
                    }
                }
                proc_macro2::TokenTree::Group(group) => {
                    if all {
                        note(group.span_open(), off, out);
                    }
                    walk(group.stream(), off, all, out);
                    if all {
                        note(group.span_close(), off, out);
                    }
                }
                proc_macro2::TokenTree::Ident(name) => {
                    let asks_here =
                        (name == "line" || name == "column") && next_is(&mut trees, '!');
                    if all || asks_here {
                        note(name.span(), off, out);
                    }
                }
                other => {
                    if all {
                        note(other.span(), off, out);
                    }
                }
            }
        }
    }
    let mut out = vec![];
    walk(
        text.parse::<proc_macro2::TokenStream>().ok()?,
        off,
        asked == Asks::APlaceOfACall,
        &mut out,
    );
    Some(out)
}

/// Whether code uses its own place in the file: `line!`, `column!`, the location of a
/// caller (`#[track_caller]`, `Location::caller`). A macro of another file that does is
/// not seen.
fn asks_where_it_is(text: &str) -> Asks {
    fn scan(stream: proc_macro2::TokenStream, found: &mut Asks) {
        let mut trees: Trees = stream.into_iter().peekable();
        while let Some(tree) = trees.next() {
            match tree {
                proc_macro2::TokenTree::Ident(name) => {
                    if name == "track_caller" {
                        *found = Asks::APlaceOfACall;
                    } else if name == "Location" && next_is(&mut trees, ':') {
                        // `Location::caller`: a type of that name alone asks nothing.
                        let mut ahead = trees.clone();
                        ahead.next();
                        ahead.next();
                        if matches!(ahead.next(), Some(proc_macro2::TokenTree::Ident(name)) if name == "caller")
                        {
                            *found = Asks::APlaceOfACall;
                        }
                    } else if (name == "line" || name == "column") && next_is(&mut trees, '!') {
                        *found = (*found).max(Asks::ItsOwnPlace);
                    }
                }
                proc_macro2::TokenTree::Group(group) => scan(group.stream(), found),
                _ => {}
            }
        }
    }
    // What does not lex is not known not to.
    let Ok(stream) = text.parse::<proc_macro2::TokenStream>() else {
        return Asks::APlaceOfACall;
    };
    let mut found = Asks::Nothing;
    scan(stream, &mut found);
    found
}

/// The ranges, each to the end of its last line when only a comment follows there: the
/// comment after a test is of the test's line.
fn with_trailing_comments(text: &str, ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges
        .into_iter()
        .map(|(start, end)| {
            let rest = text[end..].lines().next().unwrap_or("");
            let after = rest.trim_start();
            if after.starts_with("//") {
                (start, end + rest.len())
            } else {
                (start, end)
            }
        })
        .collect()
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
    let tests_now = with_trailing_comments(current, instrument::test_only_ranges(current, &parsed));
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
        let tests_then = undone_parsed.as_ref().map(|parsed| {
            with_trailing_comments(&undone, instrument::test_only_ranges(&undone, parsed))
        });
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
            outside: false,
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

    /// Whether the one change between two texts is outside of a build with these
    /// features, now and undone.
    fn outside(old: &str, new: &str, features: &[&str]) -> bool {
        let features: std::collections::BTreeSet<String> =
            features.iter().map(|name| name.to_string()).collect();
        let off = |text: &str| {
            instrument::parse_source(text)
                .map(|parsed| instrument::inactive_ranges(text, &parsed, &features))
                .ok()
        };
        let found = between(old, new);
        assert_eq!(found.len(), 1, "{found:?}");
        let then = found[0].undone.as_deref().and_then(off);
        found[0].outside_the_build(new, &off(new).unwrap(), then.as_deref())
    }

    #[test]
    fn a_change_is_outside_the_build_only_if_it_is_so_undone_too() {
        let code = "\
#[cfg(feature = \"extra\")]
pub fn extra() -> u32 { 41 }

#[cfg(feature = \"extra\")]
pub const LIMIT: u32 = 41;

#[cfg(feature = \"basic\")]
pub fn basic() -> u32 {
    41
}
";
        // In code the feature leaves out, whatever the shape of the item: a function
        // on one line, a constant, the body of a function of several lines.
        for (from, to) in [
            (
                "pub fn extra() -> u32 { 41 }",
                "pub fn extra() -> u32 { 42 }",
            ),
            ("pub const LIMIT: u32 = 41;", "pub const LIMIT: u32 = 42;"),
        ] {
            assert!(outside(code, &code.replace(from, to), &["basic"]), "{to}");
            assert!(
                !outside(code, &code.replace(from, to), &["basic", "extra"]),
                "{to}"
            );
        }
        let body = code.replace("    41\n", "    42\n");
        assert!(!outside(code, &body, &["basic"]));
        assert!(outside(code, &body, &["extra"]));
        // The attribute itself: left out now and built before, or the other way round.
        let moved = code.replace(
            "#[cfg(feature = \"basic\")]\npub fn basic",
            "#[cfg(feature = \"extra\")]\npub fn basic",
        );
        assert!(!outside(code, &moved, &["basic"]));
        assert!(!outside(&moved, code, &["basic"]));
        // Off under either name: nothing changes for this build.
        assert!(outside(code, &moved, &[]));
        // An attribute that is new, and one that is gone.
        let added = code.replace("#[cfg(feature = \"basic\")]\n", "");
        assert!(!outside(&added, code, &["extra"]));
        assert!(!outside(code, &added, &["extra"]));
        // Lines added to code that is left out, and taken from it.
        let longer = code.replace(
            "pub fn extra() -> u32 { 41 }",
            "pub fn extra() -> u32 {\n    let n = 41;\n    n\n}",
        );
        assert!(outside(code, &longer, &["basic"]));
        assert!(outside(&longer, code, &["basic"]));
        // What the build was before is not known: tried.
        let found = between(code, &body);
        assert!(!found[0].outside_the_build(&body, &[(0, body.len())], None));
    }

    #[test]
    fn a_comment_after_a_test_is_of_the_line_of_the_test() {
        let old =
            "pub const X: u32 = 41;\n#[test] fn check() { assert_eq!(X, 41); } // regression\n";
        let new = old.replace("41", "42");
        let found = between(old, &new);
        assert_eq!(found.len(), 1, "{found:?}");
        // The constant goes back, the test and its comment stay.
        assert_eq!(
            found[0].undone.as_deref(),
            Some(new.replace("X: u32 = 42", "X: u32 = 41").as_str())
        );
    }

    #[test]
    fn whole_items_that_are_left_out_change_nothing_and_lines_may() {
        let base = "pub fn kept() -> u32 { 1 }\n";
        let item = "\n#[cfg(feature = \"extra\")]\npub const X: u32 = 42;\n";
        // An item that is left out, new at the end of the file or between others, one
        // or two of them, and the same taken away.
        for added in [
            format!("{base}{item}"),
            format!("{base}{item}{item}"),
            format!("{item}{base}"),
        ] {
            assert!(outside(base, &added, &["basic"]), "{added}");
            assert!(outside(&added, base, &["basic"]), "{added}");
            assert!(!outside(base, &added, &["extra"]), "{added}");
        }
        // With an item that is built among them, it is a change.
        let mixed = format!("{base}{item}pub fn more() {{}}\n");
        assert!(!outside(base, &mixed, &["basic"]));
        // Lines that move what `line!` gives are a change of what is built.
        let counted = "#[cfg(feature = \"extra\")]\nfn off() {\n}\npub const N: u32 = line!();\n";
        let longer = counted.replace("fn off() {\n", "fn off() {\n    let _ = 2;\n");
        assert!(!outside(counted, &longer, &["basic"]));
        assert!(outside(
            &counted.replace("line!()", "4"),
            &longer.replace("line!()", "4"),
            &["basic"]
        ));
        // With every token that is kept where it was, nothing can be told: a number of
        // the same length in code that is left out. One digit more moves nothing either
        // when nothing that is kept comes after it on its line.
        let same = longer.replace("let _ = 2;", "let _ = 3;");
        assert!(outside(&longer, &same, &["basic"]));
        let wider = longer.replace("let _ = 2;", "let _ = 200;");
        assert!(outside(&longer, &wider, &["basic"]));
        assert!(outside(
            &longer.replace("line!()", "4"),
            &same.replace("line!()", "4"),
            &["basic"]
        ));
    }

    #[test]
    fn the_same_build_is_the_same_tokens_in_what_is_built() {
        // Two attributes on one statement, both reworded: either alone leaves it out,
        // both together put it back.
        let old = "fn f() -> u8 {\n    #[cfg(feature = \"basic\")]\n    #[allow(unreachable_code)]\n    #[cfg(feature = \"basic\")]\n    return 1;\n    2\n}\n";
        let new = old.replace("basic", "extra");
        let features: std::collections::BTreeSet<String> = ["basic".to_string()].into();
        let off = |text: &str| {
            instrument::inactive_ranges(text, &instrument::parse_source(text).unwrap(), &features)
        };
        let found = between(old, &new);
        assert_eq!(found.len(), 2, "{found:?}");
        for change in &found {
            let undone = change.undone.as_deref().unwrap();
            assert!(change.outside_the_build(&new, &off(&new), Some(&off(undone))));
        }
        let both = all_undone(&new, &found).unwrap();
        assert_eq!(both, old);
        assert!(!same_build(&new, &off(&new), &both, &off(&both)));
        // An item that is left out, with a comment after it, is no part of the build.
        let base = "pub const N: u8 = 7;\n";
        let with =
            format!("{base}#[cfg(feature = \"extra\")] pub const OFF: u8 = 42; // optional\n");
        assert!(same_build(&with, &off(&with), base, &off(base)));
        // Where code asks for its line, however that is spaced, lines count.
        for asking in [
            "line !()",
            "line!()",
            "column ! ()",
            "std::panic::Location::caller().line()",
        ] {
            let base = format!("pub fn n() -> u32 {{ {asking} }}\n");
            let with = format!("#[cfg(feature = \"extra\")]\npub const OFF: u8 = 42;\n{base}");
            assert!(
                !same_build(&with, &off(&with), &base, &off(&base)),
                "{asking}"
            );
        }
        // A caller's location is asked for somewhere: a change of code that is left
        // out after it, or of the same length, moves no token that is kept.
        let tracked = "#[track_caller]\npub fn checked() {}\n#[cfg(feature = \"extra\")]\nfn off() -> u8 { 41 }\npub fn after() {}\n";
        let edited = tracked.replace("41", "42");
        assert!(same_build(&edited, &off(&edited), tracked, &off(tracked)));
        let longer = tracked.replace("fn off() -> u8 { 41 }", "fn off() -> u8 {\n    41\n}");
        assert!(!same_build(&longer, &off(&longer), tracked, &off(tracked)));
        // On one line, what is left out before it moves the column.
        let base =
            "#[cfg(feature = \"extra\")] const OFF: u32 = 1; pub const N: u32 = column!();\n";
        let with = base.replace("= 1;", "= 100;");
        assert!(!same_build(&with, &off(&with), base, &off(base)));
        // In code that is left out it asks nothing of this build.
        let base = "#[cfg(feature = \"extra\")]\nfn off() -> u32 {\n    line!()\n}\npub fn n() -> u32 { 1 }\n";
        let with = base.replace("    line!()\n", "    let _ = 2;\n    line!()\n");
        assert!(same_build(&with, &off(&with), base, &off(base)));
        assert_eq!(
            asks_where_it_is("#[track_caller] fn f() {}"),
            Asks::APlaceOfACall
        );
        assert_eq!(asks_where_it_is("fn f( {"), Asks::APlaceOfACall);
        assert_eq!(
            asks_where_it_is("fn f() -> u32 { std::panic::Location::caller().line() }"),
            Asks::APlaceOfACall
        );
        assert_eq!(
            asks_where_it_is("const N: u32 = line!() + column ! ();"),
            Asks::ItsOwnPlace
        );
        // Names alone ask nothing: a function `line`, a type `Location`.
        assert_eq!(
            asks_where_it_is(
                "struct Location; fn line() { let column = 1; let _ = !column; let _ = Location; }"
            ),
            Asks::Nothing
        );
        // `line!` above code that is left out stays where it is whatever comes after
        // it: what moves below is not asked for.
        let above = "pub const START: u32 = line!();\n#[cfg(feature = \"extra\")]\nfn off() -> u8 {\n    41\n}\npub fn answer() -> u32 { 7 }\n";
        let longer = above.replace("    41\n", "    let value = 41;\n    value + 1\n");
        assert!(same_build(&longer, &off(&longer), above, &off(above)));
        // Below it, it moves.
        let below = "#[cfg(feature = \"extra\")]\nfn off() -> u8 {\n    41\n}\npub const START: u32 = line!();\n";
        let longer = below.replace("    41\n", "    let value = 41;\n    value + 1\n");
        assert!(!same_build(&longer, &off(&longer), below, &off(below)));
        // Documentation that grows before code that is left out moves nothing that is
        // asked for: written as an attribute of the file or of an item on the line of
        // the code, or as a comment above it.
        for (short, long) in [
            ("#![doc = \"a\"]", "#![doc = \"long\"]"),
            ("#[doc = \"a\"]", "#[doc = \"long\"]"),
        ] {
            let before = format!(
                "{short} #[cfg(feature = \"extra\")] fn off() {{ 1 }}\n#[track_caller]\npub fn n() -> u32 {{ line!() }}\n"
            );
            let after = before.replace(short, long).replace("{ 1 }", "{ 2 }");
            assert!(
                same_build(&after, &off(&after), &before, &off(&before)),
                "{short}"
            );
        }
        let before = "//! Old docs.\n#[cfg(feature = \"extra\")]\nfn off() {}\n#[track_caller]\npub fn n() -> u32 { line!() }\n";
        let after = before.replace("//! Old docs.", "//! Much longer documentation.");
        assert!(same_build(&after, &off(&after), before, &off(before)));
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
