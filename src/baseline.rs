//! Findings the user has already looked at: `// tmt:ignore` comments and baseline files.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};

use crate::report::Finding;

/// What identifies a finding across runs: its kind, file, code and surviving mutants.
/// Line numbers are left out, so code that merely moves keeps its fingerprint.
pub fn fingerprint(finding: &Finding) -> String {
    let (kind, at, survivors) = match finding {
        Finding::InputGap { at, survivors, .. } => ("input_gap", at, survivors),
        Finding::AssertionGap { at, survivors, .. } => ("assertion_gap", at, survivors),
    };
    format!(
        "{kind}\t{}\t{}\t{}",
        at.file,
        at.code,
        survivors.join(" | ")
    )
}

/// Reads a baseline written by [`write`]: one fingerprint per JSON array element.
pub fn load(path: &Path) -> Result<BTreeSet<String>> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("{} is not a tmt baseline", path.display()))
}

pub fn write(path: &Path, findings: &[Finding]) -> Result<()> {
    let prints: BTreeSet<String> = findings.iter().map(fingerprint).collect();
    let text = serde_json::to_string_pretty(&prints).expect("strings serialise");
    std::fs::write(path, format!("{text}\n")).with_context(|| format!("writing {}", path.display()))
}

/// Lines covered by a `// tmt:ignore` comment, with the reason given after it. The
/// comment covers its own line and the next one, so it can sit above the code.
pub fn ignored_lines(source: &str) -> BTreeMap<usize, String> {
    let mut ignored = BTreeMap::new();
    for (index, line) in source.lines().enumerate() {
        let Some(comment) = line.find("//").map(|at| &line[at + 2..]) else {
            continue;
        };
        let Some(rest) = comment.trim_start().strip_prefix("tmt:ignore") else {
            continue;
        };
        // `tmt:ignored` is some other word, and `"http://tmt:ignore"` is not a comment.
        if !(rest.is_empty() || rest.starts_with(|c: char| c.is_whitespace() || c == ':')) {
            continue;
        }
        let rest = rest.trim_start_matches(':');
        let reason = rest.trim().to_string();
        ignored.insert(index + 1, reason.clone());
        ignored.entry(index + 2).or_insert(reason);
    }
    ignored
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::Location;

    fn gap(line: usize, code: &str, survivors: &[&str]) -> Finding {
        Finding::AssertionGap {
            at: Location {
                file: "src/lib.rs".into(),
                line,
                column: 5,
                code: code.into(),
            },
            survivors: survivors.iter().map(|s| s.to_string()).collect(),
            tests: vec!["tests::t".into()],
            effects: vec![],
        }
    }

    #[test]
    fn fingerprints_ignore_position_but_not_content() {
        let base = fingerprint(&gap(10, "a > b", &["`a >= b`"]));
        assert_eq!(
            base,
            fingerprint(&gap(99, "a > b", &["`a >= b`"])),
            "moved code"
        );
        assert_ne!(base, fingerprint(&gap(10, "a > c", &["`a >= b`"])));
        assert_ne!(
            base,
            fingerprint(&gap(10, "a > b", &["`a >= b`", "`!(a > b)`"])),
            "a new survivor"
        );
        let input = Finding::InputGap {
            at: Location {
                file: "src/lib.rs".into(),
                line: 10,
                column: 5,
                code: "a > b".into(),
            },
            survivors: vec!["`a >= b`".into()],
            survivor_count: 1,
            add_tests: vec![],
            observed: None,
            verified: true,
        };
        assert_ne!(base, fingerprint(&input), "another kind of finding");
    }

    #[test]
    fn baseline_round_trip() {
        let path =
            std::env::temp_dir().join(format!("tmt-baseline-test-{}.json", std::process::id()));
        let findings = [
            gap(1, "a > b", &["`a >= b`"]),
            gap(2, "x();", &["delete `x();`"]),
        ];
        write(&path, &findings).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.len(), 2);
        assert!(findings.iter().all(|f| loaded.contains(&fingerprint(f))));
        std::fs::write(&path, "not json").unwrap();
        assert!(load(&path).is_err());
        std::fs::remove_file(&path).unwrap();
        assert!(load(&path).is_err());
    }

    #[test]
    fn ignore_comments() {
        let source = "\
fn f() {
    // tmt:ignore equivalent: both branches return the same
    if a >= b { a } else { b }
    let x = 1; // tmt:ignore
    let y = 2;
    // tmt:ignored is another word
    // not tmt:ignore here? it is: tmt:ignore must start the comment
    let url = \"http://tmt:ignore\";
}
";
        let ignored = ignored_lines(source);
        assert_eq!(ignored.keys().copied().collect::<Vec<_>>(), [2, 3, 4, 5]);
        assert_eq!(ignored[&2], "equivalent: both branches return the same");
        assert_eq!(ignored[&3], "equivalent: both branches return the same");
        assert_eq!(ignored[&4], "");
        assert!(ignored_lines("").is_empty());
    }
}
