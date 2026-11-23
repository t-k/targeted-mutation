//! Which lines of which files to analyse (`--since`, `--lines`).

use std::collections::BTreeMap;

use anyhow::{Result, bail};

/// Line ranges per file. Paths are relative to the workspace root, with `/` separators.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Scope {
    files: BTreeMap<String, Vec<(usize, usize)>>,
}

impl Scope {
    /// Parses `FILE`, `FILE:LINE` or `FILE:FROM-TO` (1-based, inclusive).
    pub fn parse_lines(specs: &[String]) -> Result<Scope> {
        let mut scope = Scope::default();
        for spec in specs {
            if spec.starts_with(':') {
                bail!("not a line range: `{spec}` (expected FILE:FROM-TO)");
            }
            let (file, range) = match spec.rsplit_once(':') {
                Some((file, lines))
                    // `file` is not empty: a spec that starts with `:` was refused above.
                    if lines.starts_with(|c: char| c.is_ascii_digit()) =>
                {
                    let (from, to) = lines.split_once('-').unwrap_or((lines, lines));
                    let parse = |text: &str| text.parse::<usize>().ok().filter(|n| *n > 0);
                    match (parse(from), parse(to)) {
                        (Some(from), Some(to)) if from <= to => (file, (from, to)),
                        _ => bail!("not a line range: `{spec}` (expected FILE:FROM-TO)"),
                    }
                }
                _ => (spec.as_str(), (1, usize::MAX)),
            };
            if file.is_empty() {
                bail!("not a line range: `{spec}` (expected FILE:FROM-TO)");
            }
            scope.add(file, range);
        }
        Ok(scope)
    }

    /// Reads the output of `git diff -U0`: the lines each hunk adds or changes. A hunk that
    /// only deletes marks the lines on either side of the deletion, because the code that
    /// is left there now behaves differently.
    pub fn from_diff(diff: &str) -> Scope {
        let mut scope = Scope::default();
        let mut file: Option<String> = None;
        for line in diff.lines() {
            if let Some(path) = line.strip_prefix("+++ ") {
                // `+++ b/path`, or `+++ /dev/null` for a deleted file.
                file = new_path(path);
            } else if line.starts_with("@@ ")
                && let Some(file) = &file
                && let Some(added) = line.split(' ').find_map(|part| part.strip_prefix('+'))
            {
                let (start, count) = added.split_once(',').unwrap_or((added, "1"));
                let (Ok(start), Ok(count)) = (start.parse::<usize>(), count.parse::<usize>())
                else {
                    continue;
                };
                let range = if count == 0 {
                    // `+N,0`: lines were removed after line N.
                    (start.max(1), start + 1)
                } else {
                    (start, start + count - 1)
                };
                scope.add(file, range);
            }
        }
        scope
    }

    pub fn add(&mut self, file: &str, range: (usize, usize)) {
        self.files
            .entry(file.replace('\\', "/"))
            .or_default()
            .push(range);
    }

    pub fn add_whole_file(&mut self, file: &str) {
        self.add(file, (1, usize::MAX));
    }

    pub fn merge(&mut self, other: Scope) {
        for (file, ranges) in other.files {
            self.files.entry(file).or_default().extend(ranges);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn files(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// Whether lines `first..=last` of `file` touch the scope.
    pub fn touches(&self, file: &str, first: usize, last: usize) -> bool {
        let ranges = self.files.get(file).or_else(|| {
            if file.contains('\\') {
                self.files.get(&file.replace('\\', "/"))
            } else {
                None
            }
        });
        ranges.is_some_and(|ranges| {
            ranges
                .iter()
                .any(|(from, to)| first <= *to && *from <= last)
        })
    }
}

/// One hunk of `git diff -U0`: where the new lines are, and the lines that were there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// Path relative to the workspace root, with `/` separators.
    pub file: String,
    /// The first new line (1-based); with no new lines, the line the old ones came after.
    pub start: usize,
    /// How many new lines there are.
    pub count: usize,
    /// The old lines, each with its line end (the last one without, if the old file
    /// ended without one there).
    pub old: String,
}

/// The hunks of the files that are still there. The lines of a hunk are counted as its
/// header says, so a removed line that starts with `--` is not taken for a file header.
pub fn hunks(diff: &str) -> Vec<Hunk> {
    let mut found = Vec::new();
    let mut file: Option<String> = None;
    let mut lines = diff.split_inclusive('\n').peekable();
    while let Some(line) = lines.next() {
        let line = line.strip_suffix('\n').unwrap_or(line);
        if let Some(path) = line.strip_prefix("+++ ") {
            file = new_path(path);
            continue;
        }
        let Some((old_count, start, count)) = hunk_header(line) else {
            continue;
        };
        let mut old = String::new();
        let (mut old_left, mut new_left) = (old_count, count);
        while old_left + new_left > 0 {
            let Some(body) = lines.next() else { break };
            match body.as_bytes().first() {
                Some(b'-') if old_left > 0 => {
                    old.push_str(&body[1..]);
                    old_left -= 1;
                }
                Some(b'+') if new_left > 0 => new_left -= 1,
                _ => break,
            }
            if lines.peek().is_some_and(|next| next.starts_with('\\')) {
                // `\ No newline at end of file`, about the line just read.
                lines.next();
                if body.starts_with('-') {
                    old.truncate(old.trim_end_matches(['\n', '\r']).len());
                }
            }
        }
        if let Some(file) = &file {
            found.push(Hunk {
                file: file.replace('\\', "/"),
                start,
                count,
                old,
            });
        }
    }
    found
}

/// `@@ -a,b +c,d @@`: the count of old lines, the first new line and the count of new ones.
fn hunk_header(line: &str) -> Option<(usize, usize, usize)> {
    let mut parts = line.strip_prefix("@@ -")?.split(' ');
    let pair = |text: &str| -> Option<(usize, usize)> {
        let (start, count) = text.split_once(',').unwrap_or((text, "1"));
        Some((start.parse().ok()?, count.parse().ok()?))
    };
    let (_, old_count) = pair(parts.next()?)?;
    let (start, count) = pair(parts.next()?.strip_prefix('+')?)?;
    Some((old_count, start, count))
}

/// The path of a `+++ ` line: `None` for `/dev/null`.
fn new_path(path: &str) -> Option<String> {
    // Git quotes a path with bytes outside printable ASCII, and puts a tab after one
    // with a space.
    let path = match unquote(path) {
        Some(path) => path,
        None => path.strip_suffix('\t').unwrap_or(path).to_string(),
    };
    path.strip_prefix("b/").map(str::to_string)
}

/// `text` with the hunks undone: the old lines where the new ones are. The hunks are of
/// this text and do not overlap. `None` when one of them does not fit the text.
pub fn undo(text: &str, hunks: &[&Hunk]) -> Option<String> {
    let starts: Vec<usize> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(at, _)| at + 1))
        .collect();
    // The offset where line `n` (1-based) starts; past the last line, the end.
    let start_of = |line: usize| starts.get(line - 1).copied().unwrap_or(text.len());
    let mut ordered: Vec<&Hunk> = hunks.to_vec();
    ordered.sort_by_key(|hunk| (hunk.start, hunk.count));
    let mut out = String::with_capacity(text.len());
    let mut done = 0;
    for hunk in ordered {
        let (from, to) = if hunk.count == 0 {
            let at = start_of(hunk.start + 1);
            (at, at)
        } else {
            (start_of(hunk.start), start_of(hunk.start + hunk.count))
        };
        if from < done || to > text.len() || (hunk.count > 0 && from == to) {
            return None;
        }
        out.push_str(&text[done..from]);
        // Old lines put after a last line that has no line end need one before them.
        if from == to && from == text.len() && !text.is_empty() && !text.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&hunk.old);
        done = to;
    }
    out.push_str(&text[done..]);
    Some(out)
}

/// The path git wrote between double quotes (`"b/src/\344\276\241.rs"`): C escapes, and
/// three octal digits for each byte that is not printable ASCII. `None` for text that is
/// not quoted that way.
fn unquote(text: &str) -> Option<String> {
    let inner = text.strip_prefix('"')?.strip_suffix('"')?;
    let mut bytes = Vec::with_capacity(inner.len());
    let mut rest = inner.bytes();
    while let Some(byte) = rest.next() {
        if byte == b'"' {
            return None;
        }
        if byte != b'\\' {
            bytes.push(byte);
            continue;
        }
        bytes.push(match rest.next()? {
            b'a' => 7,
            b'b' => 8,
            b't' => b'\t',
            b'n' => b'\n',
            b'v' => 11,
            b'f' => 12,
            b'r' => b'\r',
            b'"' => b'"',
            b'\\' => b'\\',
            first @ b'0'..=b'3' => {
                let mut value = u32::from(first - b'0');
                for _ in 0..2 {
                    let digit = rest.next().filter(|digit| (b'0'..=b'7').contains(digit))?;
                    value = value * 8 + u32::from(digit - b'0');
                }
                u8::try_from(value).ok()?
            }
            _ => return None,
        });
    }
    String::from_utf8(bytes).ok()
}

/// Byte offsets of the line starts in one source file.
pub(crate) struct LineIndex(Vec<usize>);

impl LineIndex {
    pub(crate) fn new(source: &str) -> Self {
        Self(
            std::iter::once(0)
                .chain(
                    source
                        .bytes()
                        .enumerate()
                        .filter(|&(_, byte)| byte == b'\n')
                        .map(|(offset, _)| offset + 1),
                )
                .collect(),
        )
    }

    pub(crate) fn line_of(&self, offset: usize) -> usize {
        self.0.partition_point(|&start| start <= offset)
    }

    pub(crate) fn position(&self, source: &str, offset: usize) -> (usize, usize) {
        let offset = if source.is_char_boundary(offset) {
            offset
        } else {
            source.len()
        };
        let line = self.line_of(offset);
        let column = source[self.0[line - 1]..offset].chars().count() + 1;
        (line, column)
    }

    pub(crate) fn offset_of(&self, source: &str, line: usize, column: usize) -> Option<usize> {
        // As in the unindexed lookup, out-of-range lines and columns clamp to EOF.
        let start = self
            .0
            .get(line.checked_sub(1)?)
            .copied()
            .unwrap_or(source.len());
        let column = column.checked_sub(1)?;
        if column == 0 {
            return Some(start);
        }
        let rest = &source[start..];
        let capped = column.min(rest.len());
        let within = if rest.as_bytes()[..capped].is_ascii() {
            capped
        } else {
            rest.char_indices()
                .nth(column)
                .map_or(rest.len(), |(offset, _)| offset)
        };
        Some(start + within)
    }
}

/// 1-based line of the byte at `offset`.
pub fn line_of(source: &str, offset: usize) -> usize {
    source.as_bytes()[..offset.min(source.len())]
        .iter()
        .filter(|b| **b == b'\n')
        .count()
        + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    fn specs(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn line_specs() {
        let scope =
            Scope::parse_lines(&specs(&["src/a.rs:10-20", "src/a.rs:30", "src/b.rs"])).unwrap();
        // Inside, on both boundaries, overlapping, and outside.
        assert!(scope.touches("src/a.rs", 12, 13));
        assert!(scope.touches("src/a.rs", 1, 10));
        assert!(scope.touches("src/a.rs", 20, 25));
        assert!(scope.touches("src/a.rs", 5, 40));
        assert!(scope.touches("src/a.rs", 30, 30));
        assert!(!scope.touches("src/a.rs", 1, 9));
        assert!(!scope.touches("src/a.rs", 21, 29));
        assert!(!scope.touches("src/a.rs", 31, 99));
        assert!(scope.touches("src/b.rs", 7, 7));
        assert!(!scope.touches("src/c.rs", 7, 7));
        assert!(
            scope.touches("src\\b.rs", 1, 1),
            "separators are normalised"
        );
        assert_eq!(scope.files().collect::<Vec<_>>(), ["src/a.rs", "src/b.rs"]);
        assert!(!scope.is_empty());
        assert!(Scope::default().is_empty());

        for bad in [
            "src/a.rs:0",
            "src/a.rs:5-3",
            "src/a.rs:4-3",
            "src/a.rs:1-x",
            ":3",
            "src/a.rs:3-",
        ] {
            assert!(Scope::parse_lines(&specs(&[bad])).is_err(), "{bad}");
        }
        // A Windows drive letter or a name with a colon is a file, not a range.
        let odd = Scope::parse_lines(&specs(&["dir:name/a.rs"])).unwrap();
        assert!(odd.touches("dir:name/a.rs", 1, 1));
    }

    #[test]
    fn diff_hunks() {
        let diff = "\
diff --git a/src/a.rs b/src/a.rs
index 111..222 100644
--- a/src/a.rs
+++ b/src/a.rs
@@ -10,2 +10,3 @@ fn changed() {
-old
-old
+new
+new
+new
@@ -30 +31 @@
-x
+y
@@ -40,3 +40,0 @@ fn deleted_lines() {
-gone
-gone
-gone
@@ -0,0 +1,2 @@
+top
+top
diff --git a/src/gone.rs b/src/gone.rs
deleted file mode 100644
--- a/src/gone.rs
+++ /dev/null
@@ -1,5 +0,0 @@
-all
diff --git a/src/new.rs b/src/new.rs
new file mode 100644
--- /dev/null
+++ b/src/new.rs
@@ -0,0 +1,4 @@
+fresh
";
        let scope = Scope::from_diff(diff);
        // Changed: 10..=12. Single line: 31. Pure deletion after line 40: 40 and 41.
        for (line, expected) in [
            (9, false),
            (10, true),
            (12, true),
            (13, false),
            (31, true),
            (32, false),
            (39, false),
            (40, true),
            (41, true),
            (42, false),
            (1, true),
            (2, true),
            (3, false),
        ] {
            assert_eq!(
                scope.touches("src/a.rs", line, line),
                expected,
                "line {line}"
            );
        }
        assert!(
            !scope.touches("src/gone.rs", 1, 100),
            "a deleted file has no lines left"
        );
        assert!(scope.touches("src/new.rs", 4, 4));
        assert!(!scope.touches("src/new.rs", 5, 5));
        assert_eq!(Scope::from_diff(""), Scope::default());
    }

    #[test]
    fn paths_are_read_as_git_writes_them() {
        // A path with a space is followed by a tab; one with other bytes is quoted, with
        // the prefix inside the quotes and octal for what is not ASCII.
        let diff = "\
--- a/src/two words.rs\t
+++ b/src/two words.rs\t
@@ -3 +3 @@
--- \"a/src/\\344\\276\\241\\346\\240\\274.rs\"
+++ \"b/src/\\344\\276\\241\\346\\240\\274.rs\"
@@ -5 +5 @@
--- \"a/src/a\\\"b\\\\c\\td e.rs\"
+++ \"b/src/a\\\"b\\\\c\\td e.rs\"
@@ -7 +7 @@
--- a/src/価格.rs
+++ b/src/価格.rs
@@ -9 +9 @@
--- \"a/src/gone \\303\\251.rs\"
+++ /dev/null
@@ -1 +0,0 @@
";
        let scope = Scope::from_diff(diff);
        assert_eq!(
            scope.files().collect::<Vec<_>>(),
            [
                // Stored with `/`, as every path is (a Windows separator).
                "src/a\"b/c\td e.rs",
                "src/two words.rs",
                "src/価格.rs"
            ]
        );
        assert!(scope.touches("src/two words.rs", 3, 3));
        // Quoted or not (`core.quotePath`), the same file.
        assert!(scope.touches("src/価格.rs", 5, 5));
        assert!(scope.touches("src/価格.rs", 9, 9));
        assert!(scope.touches("src/a\"b\\c\td e.rs", 7, 7));
    }

    #[test]
    fn a_quoted_path_is_what_its_escapes_spell() {
        for (quoted, path) in [
            (r#""plain""#, Some("plain")),
            (r#""a\tb\nc\\d\"e""#, Some("a\tb\nc\\d\"e")),
            (r#""\a\b\f\r\v""#, Some("\u{7}\u{8}\u{c}\r\u{b}")),
            (r#""\343\201\202\101""#, Some("あA")),
            // Not closed, an escape git does not write, octal cut short, bytes that
            // are no text: nothing is guessed.
            (r#""open"#, None),
            (r#""a\qb""#, None),
            (r#""\34""#, None),
            (r#""\377""#, None),
            (r#""a"b""#, None),
            ("bare", None),
        ] {
            assert_eq!(unquote(quoted).as_deref(), path, "{quoted}");
        }
    }

    /// The old and new text of one row of the table.
    fn revert_case(row: &[&str]) -> (String, String) {
        let [shape, position, old_newline, new_newline, hunks, dashes] = row else {
            panic!("{row:?}")
        };
        let line = |name: &str| match *dashes {
            "plain" => format!("{name}\n"),
            _ => format!("-- {name}\n"),
        };
        let keep = |n: usize| format!("kept {n}\n");
        let change = |old: &mut String, new: &mut String, tag: &str| match *shape {
            "replace" => {
                old.push_str(&line(&format!("old {tag}")));
                new.push_str(&line(&format!("new {tag}")));
            }
            "add" => new.push_str(&line(&format!("new {tag}"))),
            _ => old.push_str(&line(&format!("old {tag}"))),
        };
        let (mut old, mut new) = (String::new(), String::new());
        let both = |old: &mut String, new: &mut String, text: String| {
            old.push_str(&text);
            new.push_str(&text);
        };
        let two = *hunks == "two";
        match *position {
            "first" => {
                change(&mut old, &mut new, "a");
                both(&mut old, &mut new, keep(1) + &keep(2));
                if two {
                    change(&mut old, &mut new, "b");
                    both(&mut old, &mut new, keep(3));
                }
            }
            "middle" => {
                both(&mut old, &mut new, keep(1));
                change(&mut old, &mut new, "a");
                both(&mut old, &mut new, keep(2));
            }
            _ => {
                if two {
                    both(&mut old, &mut new, keep(0));
                    change(&mut old, &mut new, "b");
                }
                both(&mut old, &mut new, keep(1) + &keep(2));
                change(&mut old, &mut new, "a");
            }
        }
        for (text, newline) in [(&mut old, old_newline), (&mut new, new_newline)] {
            if *newline == "no" {
                text.pop();
            }
        }
        (old, new)
    }

    #[test]
    fn undoing_the_hunks_of_a_diff_gives_the_old_text() {
        let dir = std::env::temp_dir().join(format!("tmt-reverts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let table = include_str!("../tests/data/reverts.pict.tsv");
        let mut rows = 0;
        for row in table.lines().skip(1) {
            let row: Vec<&str> = row.split('\t').collect();
            let (old, new) = revert_case(&row);
            std::fs::write(dir.join("old"), &old).unwrap();
            std::fs::write(dir.join("new"), &new).unwrap();
            let output = std::process::Command::new("git")
                .current_dir(&dir)
                .args(["diff", "--no-index", "-U0", "--no-color", "--no-ext-diff"])
                .args(["--src-prefix=a/", "--dst-prefix=b/", "old", "new"])
                .output()
                .unwrap();
            let diff = String::from_utf8(output.stdout).unwrap();
            let found = hunks(&diff);
            // A line end that comes or goes at the end is a hunk of its own, or part of
            // the one that changes the last line.
            let changes = if row[4] == "two" { 2 } else { 1 };
            assert!(
                (changes..=changes + 1).contains(&found.len())
                    || (found.len() == 1 && row[1] == "last"),
                "{row:?}\n{diff}\n{found:?}"
            );
            assert!(found.iter().all(|hunk| hunk.file == "new"), "{found:?}");
            let all: Vec<&Hunk> = found.iter().collect();
            assert_eq!(undo(&new, &all).as_deref(), Some(old.as_str()), "{row:?}\n{diff}");
            // One hunk alone: the old lines there, the rest as it is now.
            if let [first, second] = &found[..] {
                let only_first = undo(&new, &[first]).unwrap();
                let only_second = undo(&new, &[second]).unwrap();
                assert_ne!(only_first, old, "{row:?}");
                assert_ne!(only_first, new, "{row:?}");
                assert_ne!(only_first, only_second, "{row:?}");
            }
            rows += 1;
        }
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(rows, 23);
    }

    #[test]
    fn a_hunk_that_does_not_fit_the_text_is_not_applied() {
        let hunk = |start, count| Hunk {
            file: "f".into(),
            start,
            count,
            old: "old\n".into(),
        };
        assert_eq!(undo("a\nb\n", &[&hunk(2, 1)]).as_deref(), Some("a\nold\n"));
        assert_eq!(undo("a\nb\n", &[&hunk(0, 0)]).as_deref(), Some("old\na\nb\n"));
        assert_eq!(undo("a\nb\n", &[&hunk(2, 0)]).as_deref(), Some("a\nb\nold\n"));
        assert_eq!(undo("a\nb", &[&hunk(2, 0)]).as_deref(), Some("a\nb\nold\n"));
        // Lines the text does not have, and two hunks of the same lines.
        assert_eq!(undo("a\nb\n", &[&hunk(3, 1)]), None);
        assert_eq!(undo("a\nb\n", &[&hunk(1, 2), &hunk(2, 1)]), None);
        assert_eq!(hunks("@@ nonsense @@\n@@ -1 +x @@\n"), []);
        // A hunk before any file is of no file; one of a deleted file has no new lines.
        assert_eq!(hunks("@@ -1 +1 @@\n-a\n+b\n"), []);
        assert_eq!(hunks("--- a/f\n+++ /dev/null\n@@ -1 +0,0 @@\n-a\n"), []);
    }

    #[test]
    fn merging_and_whole_files() {
        let mut scope = Scope::parse_lines(&specs(&["a.rs:1-2"])).unwrap();
        let mut other = Scope::default();
        other.add_whole_file("b.rs");
        other.add("a.rs", (9, 9));
        scope.merge(other);
        assert!(
            scope.touches("a.rs", 9, 9)
                && scope.touches("a.rs", 2, 2)
                && !scope.touches("a.rs", 5, 5)
        );
        assert!(scope.touches("b.rs", 12345, 12345));
    }

    #[test]
    fn indexed_positions_match_the_previous_scans() {
        for row in include_str!("../tests/data/source-positions.pict.tsv")
            .lines()
            .skip(1)
        {
            let fields: Vec<_> = row.split('\t').collect();
            let [text, newline, ending, query] = fields.as_slice() else {
                panic!("invalid fixture row {row}");
            };
            let word = if *text == "unicode" { "é🦀" } else { "xyz" };
            let newline = if *newline == "crlf" { "\r\n" } else { "\n" };
            let source = match *ending {
                "empty" => String::new(),
                "newline" => format!("{word}{newline}{word}{newline}"),
                "none" => format!("{word}{newline}{word}"),
                _ => panic!("invalid ending in {row}"),
            };
            let lines = LineIndex::new(&source);
            for offset in 0..=source.len() + 1 {
                assert_eq!(lines.line_of(offset), line_of(&source, offset), "{row}");
                let prefix = source.get(..offset).unwrap_or(&source);
                let column = prefix.rsplit('\n').next().unwrap().chars().count() + 1;
                assert_eq!(
                    lines.position(&source, offset),
                    (line_of(prefix, prefix.len()), column),
                    "{row}"
                );
            }
            let (line, column) = match *query {
                "first" => (1, 1),
                "boundary" => (2, 2),
                "beyond" => (20, 99),
                "zero" => (1, 0),
                _ => panic!("invalid query in {row}"),
            };
            assert_eq!(
                lines.offset_of(&source, line, column),
                crate::instrument::offset_of(&source, line, column),
                "{row}"
            );
        }
    }

    #[test]
    fn long_ascii_prefix_before_multibyte_columns() {
        let source = format!("start\r\n{}é🦀\n", "a".repeat(64_000));
        let lines = LineIndex::new(&source);
        for column in [0, 1, 64_000, 64_001, 64_002, 64_003, usize::MAX] {
            assert_eq!(
                lines.offset_of(&source, 2, column),
                crate::instrument::offset_of(&source, 2, column)
            );
        }
    }

    #[test]
    fn lines_of_offsets() {
        let source = "a\nbc\n\nd";
        assert_eq!(
            [0, 1, 2, 4, 5, 6, 7, 99].map(|o| line_of(source, o)),
            [1, 1, 2, 2, 3, 4, 4, 4]
        );
    }
}
