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
                file = path.strip_prefix("b/").map(str::to_string);
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
        self.files
            .get(&file.replace('\\', "/"))
            .is_some_and(|ranges| {
                ranges
                    .iter()
                    .any(|(from, to)| first <= *to && *from <= last)
            })
    }
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
        let rest = &source[start..];
        let within = rest
            .char_indices()
            .nth(column.checked_sub(1)?)
            .map_or(rest.len(), |(offset, _)| offset);
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
                if offset <= source.len() && source.is_char_boundary(offset) {
                    let prefix = &source[..offset];
                    let column = prefix.rsplit('\n').next().unwrap().chars().count() + 1;
                    assert_eq!(
                        lines.position(&source, offset),
                        (line_of(&source, offset), column),
                        "{row}"
                    );
                }
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
    fn lines_of_offsets() {
        let source = "a\nbc\n\nd";
        assert_eq!(
            [0, 1, 2, 4, 5, 6, 7, 99].map(|o| line_of(source, o)),
            [1, 1, 2, 2, 3, 4, 4, 4]
        );
    }
}
