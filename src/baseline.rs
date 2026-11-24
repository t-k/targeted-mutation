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
    let mut value: serde_json::Value = serde_json::from_str(text)?;
    if value.is_array() {
        return Ok(Baseline::Fingerprints(serde_json::from_value(value)?));
    }
    match value["version"].as_u64() {
        Some(VERSION) => Ok(Baseline::Ids(serde_json::from_value(
            value["findings"].take(),
        )?)),
        Some(other) => anyhow::bail!("version {other} of the format is not known to this tmt"),
        None => anyhow::bail!("neither a list nor an object with a version"),
    }
}

pub fn write(path: &Path, findings: &[Finding]) -> Result<()> {
    let mut text = render(findings);
    text.push('\n');
    std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))
}

fn render(findings: &[Finding]) -> String {
    #[derive(serde::Serialize)]
    struct BaselineFile<'a> {
        findings: BTreeSet<&'a str>,
        version: u64,
    }

    let ids: BTreeSet<&str> = findings
        .iter()
        .map(|finding| {
            let at = finding.at();
            at.id.as_str()
        })
        .collect();
    serde_json::to_string_pretty(&BaselineFile {
        findings: ids,
        version: VERSION,
    })
    .expect("strings serialise")
}

/// Lines covered by a `// tmt:ignore` comment, with the reason given after it. A
/// standalone comment line covers its own line and the next one (so it can sit above the
/// code); an end-of-line comment covers only its own line.
pub fn ignored_lines(source: &str) -> BTreeMap<usize, String> {
    let mut ignored = BTreeMap::new();
    let bytes = source.as_bytes();
    let mut i = 0;
    let mut line = 1usize;
    let mut line_start = 0usize;
    let mut block_depth = 0usize;
    while i < bytes.len() {
        if block_depth > 0 {
            if bytes[i] == b'/' && bytes.get(i + 1) == Some(&b'*') {
                block_depth += 1;
                i += 2;
            } else if bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/') {
                block_depth -= 1;
                i += 2;
            } else {
                if bytes[i] == b'\n' {
                    line += 1;
                    line_start = i + 1;
                }
                i += 1;
            }
            continue;
        }
        match bytes[i] {
            b'\n' => {
                line += 1;
                line_start = i + 1;
                i += 1;
            }
            b'/' if bytes.get(i + 1) == Some(&b'/') => {
                let comment_start = i + 2;
                let mut end = comment_start;
                while end < bytes.len() && bytes[end] != b'\n' {
                    end += 1;
                }
                let comment = &source[comment_start..end];
                if let Some(rest) = comment.trim_start().strip_prefix("tmt:ignore")
                    && (rest.is_empty()
                        || rest.starts_with(|c: char| c.is_whitespace() || c == ':'))
                {
                    let reason = rest.trim_start_matches(':').trim().to_string();
                    let standalone = source[line_start..i].trim().is_empty();
                    ignored.insert(line, reason.clone());
                    if standalone {
                        ignored.entry(line + 1).or_insert(reason);
                    }
                }
                i = end;
            }
            b'/' if bytes.get(i + 1) == Some(&b'*') => {
                block_depth = 1;
                i += 2;
            }
            // Raw strings: `r"..."`, `r#"..."#`, `br#"..."#`, `cr#"..."#`.
            b'r' if raw_string_hashes(bytes, i).is_some() => {
                let hashes = raw_string_hashes(bytes, i).unwrap();
                i += 1 + hashes + 1;
                while i < bytes.len() {
                    if bytes[i] == b'\n' {
                        line += 1;
                        line_start = i + 1;
                    }
                    if bytes[i] == b'"'
                        && bytes
                            .get(i + 1..i + 1 + hashes)
                            .is_some_and(|s| s.iter().all(|b| *b == b'#'))
                    {
                        i += 1 + hashes;
                        break;
                    }
                    i += 1;
                }
            }
            b'"' => {
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\\' {
                        if bytes.get(i + 1) == Some(&b'\n') {
                            line += 1;
                            line_start = i + 2;
                        }
                        i = (i + 2).min(bytes.len());
                        continue;
                    }
                    if bytes[i] == b'\n' {
                        line += 1;
                        line_start = i + 1;
                    }
                    if bytes[i] == b'"' {
                        i += 1;
                        break;
                    }
                    i += 1;
                }
            }
            // Character literals (`'"'`, `'\\'`, `'\''`, `'a'`); lifetimes (`'a`) have no closing `'`.
            b'\'' => {
                if bytes.get(i + 1) == Some(&b'\\') && bytes.get(i + 2).is_some_and(|&b| b != b'\n')
                {
                    // Escape char literal such as `'\''`, `'\"'`, `'\\'`, `'\x7f'`, `'\u{...}'`.
                    // Start after the escaped byte so `'\''` does not close on the escaped quote.
                    let mut j = i + 3;
                    while j < bytes.len() && bytes[j] != b'\n' && bytes[j] != b'\'' {
                        j += 1;
                    }
                    if bytes.get(j) == Some(&b'\'') {
                        i = j + 1;
                    } else {
                        i += 1;
                    }
                } else if let Some(ch) = source[i + 1..].chars().next()
                    && bytes.get(i + 1 + ch.len_utf8()) == Some(&b'\'')
                {
                    i += 1 + ch.len_utf8() + 1;
                } else {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    ignored
}

fn raw_string_hashes(bytes: &[u8], r_pos: usize) -> Option<usize> {
    // Reject identifier suffixes like `str"..."` (except `br"` / `cr"`).
    if r_pos > 0 {
        let prev = bytes[r_pos - 1];
        let is_prefix =
            (prev == b'b' || prev == b'c') && (r_pos == 1 || !is_ident_byte(bytes[r_pos - 2]));
        if is_ident_byte(prev) && !is_prefix {
            return None;
        }
    }
    let mut hashes = 0;
    while bytes.get(r_pos + 1 + hashes) == Some(&b'#') {
        hashes += 1;
    }
    (bytes.get(r_pos + 1 + hashes) == Some(&b'"')).then_some(hashes)
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
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
            witness: None,
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
            r#"{"version": 2}"#,
            r#"{"version": 2, "findings": null}"#,
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
        let mut findings = vec![
            gap(1, "a > b", &["`a >= b`"]),
            gap(2, "x();", &["delete `x();`"]),
        ];
        for (index, finding) in findings.iter_mut().enumerate() {
            let Finding::AssertionGap { at, .. } = finding else {
                unreachable!()
            };
            at.id = format!("id-{index}-é\n");
        }
        findings.reverse();
        findings.push(findings[0].clone());
        write(&path, &findings).unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "{\n  \"findings\": [\n    \"id-0-é\\n\",\n    \"id-1-é\\n\"\n  ],\n  \"version\": 2\n}\n"
        );
        let loaded = load(&path).unwrap();
        let ids: BTreeSet<String> = ["id-0-é\n", "id-1-é\n"].map(str::to_string).into();
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
    let url = \"http://tmt:ignore reason\";
    let ok = \"https://example.com\"; // tmt:ignore after url
    let raw = r#\"// tmt:ignore inside raw string\"#;
    /* // tmt:ignore inside block comment */
    let ch = '\"'; // tmt:ignore after quote char
    let quotes = ['\\'', '\"'];
    let fake = \"// tmt:ignore fake after escaped quote\";
    let real = 5; // tmt:ignore after escaped quote
}
";
        let ignored = ignored_lines(source);
        assert_eq!(
            ignored.keys().copied().collect::<Vec<_>>(),
            [2, 3, 4, 7, 11, 14, 17]
        );
        assert_eq!(ignored[&7], "with a colon");
        assert_eq!(ignored[&2], "equivalent: both branches return the same");
        assert_eq!(ignored[&3], "equivalent: both branches return the same");
        assert_eq!(ignored[&4], "");
        assert_eq!(ignored[&11], "after url");
        assert_eq!(ignored[&14], "after quote char");
        assert_eq!(ignored[&17], "after escaped quote");
        assert!(ignored_lines("").is_empty());
    }
}
