//! Findings the user has already looked at: `// tmt:ignore` comments and baseline files.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};

use crate::report::Finding;

/// What identifies a finding across runs: its kind, file, code and surviving mutants.
/// Line numbers are left out, so code that merely moves keeps its fingerprint.
pub fn fingerprint(finding: &Finding) -> String {
    // A mutant built on its own was an assertion gap when fingerprints were written.
    let kind = match finding {
        Finding::InputGap { .. } => "input_gap",
        Finding::AssertionGap { .. } | Finding::Survivor { .. } => "assertion_gap",
    };
    let (at, survivors) = (finding.at(), finding.survivors());
    format!(
        "{kind}\t{}\t{}\t{}",
        at.file,
        at.code,
        survivors.join(" | ")
    )
}

/// The findings a baseline file lists.
#[derive(Debug, PartialEq)]
pub enum Baseline {
    /// As written before findings had names of their own: by [`fingerprint`].
    Fingerprints(BTreeSet<String>),
    /// By the name of the finding (`report::finding_id`).
    Ids(BTreeSet<String>),
}

impl Baseline {
    pub fn knows(&self, finding: &Finding) -> bool {
        let at = finding.at();
        match self {
            Baseline::Fingerprints(prints) => prints.contains(&fingerprint(finding)),
            Baseline::Ids(ids) => !at.id.is_empty() && ids.contains(&at.id),
        }
    }
}

/// The version of the file [`write`] writes.
const VERSION: u64 = 2;

/// Reads a baseline: the object [`write`] writes, or the list of fingerprints earlier
/// versions wrote.
pub fn load(path: &Path) -> Result<Baseline> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    parse(&text).with_context(|| format!("{} is not a tmt baseline", path.display()))
}

fn parse(text: &str) -> Result<Baseline> {
    let strings = |value: &serde_json::Value| -> Result<BTreeSet<String>> {
        Ok(serde_json::from_value(value.clone())?)
    };
    let value: serde_json::Value = serde_json::from_str(text)?;
    if value.is_array() {
        return Ok(Baseline::Fingerprints(strings(&value)?));
    }
    match value["version"].as_u64() {
        Some(VERSION) => Ok(Baseline::Ids(strings(&value["findings"])?)),
        Some(other) => anyhow::bail!("version {other} of the format is not known to this tmt"),
        None => anyhow::bail!("neither a list nor an object with a version"),
    }
}

pub fn write(path: &Path, findings: &[Finding]) -> Result<()> {
    let text = render(findings);
    std::fs::write(path, format!("{text}\n")).with_context(|| format!("writing {}", path.display()))
}

fn render(findings: &[Finding]) -> String {
    let ids: BTreeSet<&str> = findings
        .iter()
        .map(|finding| {
            let at = finding.at();
            at.id.as_str()
        })
        .collect();
    let file = serde_json::json!({ "version": VERSION, "findings": ids });
    serde_json::to_string_pretty(&file).expect("strings serialise")
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
                ..Location::default()
            },
            survivors: survivors.iter().map(|s| s.to_string()).collect(),
            tests: vec!["tests::t".into()],
            effects: vec![],
            effects_of: None,
        }
    }

    #[test]
    fn a_baseline_knows_findings_by_name_and_old_ones_by_fingerprint() {
        let named = |id: &str, survivors: &[&str]| {
            let mut finding = gap(10, "a > b", survivors);
            let Finding::AssertionGap { at, .. } = &mut finding else {
                unreachable!()
            };
            at.id = id.to_string();
            finding
        };
        let one = named("assertion_gap|src/lib.rs|f|a > b|0", &["`a >= b`"]);
        let other = named("assertion_gap|src/lib.rs|g|a > b|0", &["`a >= b`"]);
        let written = render(std::slice::from_ref(&one));
        let baseline = parse(&written).unwrap();
        assert!(baseline.knows(&one));
        // The same code in another function is another finding.
        assert!(!baseline.knows(&other));
        // Other words for the mutants, or one more of them, and it is still the one.
        assert!(baseline.knows(&named(
            "assertion_gap|src/lib.rs|f|a > b|0",
            &["`a >= b`", "the negation"]
        )));
        // A finding without a name is not known by having none.
        assert!(
            !parse(&render(&[gap(1, "x", &[])]))
                .unwrap()
                .knows(&gap(2, "y", &[]))
        );
        // A file of an earlier version lists fingerprints, and still works as it did.
        let old = serde_json::to_string(&[fingerprint(&one)]).unwrap();
        let baseline = parse(&old).unwrap();
        assert!(matches!(baseline, Baseline::Fingerprints(_)));
        assert!(baseline.knows(&one) && baseline.knows(&other));
        assert!(!baseline.knows(&named("x", &["`a >= b`", "the negation"])));
        // Anything else is refused, with a later version said to be one.
        let later = parse(r#"{"version": 3, "findings": []}"#).unwrap_err();
        assert!(later.to_string().contains("version 3"), "{later}");
        for bad in [
            "{}",
            "3",
            r#"{"version": 2, "findings": [1]}"#,
            r#"["a", 1]"#,
        ] {
            assert!(parse(bad).is_err(), "{bad}");
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
                ..Location::default()
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
        let mut findings = [
            gap(1, "a > b", &["`a >= b`"]),
            gap(2, "x();", &["delete `x();`"]),
        ];
        for (index, finding) in findings.iter_mut().enumerate() {
            let Finding::AssertionGap { at, .. } = finding else {
                unreachable!()
            };
            at.id = format!("id-{index}");
        }
        write(&path, &findings).unwrap();
        let loaded = load(&path).unwrap();
        let ids: BTreeSet<String> = ["id-0", "id-1"].iter().map(|id| id.to_string()).collect();
        assert_eq!(loaded, Baseline::Ids(ids));
        assert!(findings.iter().all(|f| loaded.knows(f)));
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
    let z = 3;
    let w = 4; // tmt:ignore: with a colon
    // tmt:ignored is another word
    // not tmt:ignore here? it is: tmt:ignore must start the comment
    let url = \"http://tmt:ignore\";
}
";
        let ignored = ignored_lines(source);
        assert_eq!(
            ignored.keys().copied().collect::<Vec<_>>(),
            [2, 3, 4, 5, 7, 8]
        );
        assert_eq!(ignored[&7], "with a colon");
        assert_eq!(ignored[&2], "equivalent: both branches return the same");
        assert_eq!(ignored[&3], "equivalent: both branches return the same");
        assert_eq!(ignored[&4], "");
        assert!(ignored_lines("").is_empty());
    }
}
