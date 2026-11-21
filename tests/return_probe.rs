use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;
use targeted_mutation::instrument::{SiteKind, discover};

#[test]
fn a_result_probe_requires_every_function_exit_to_pass_through_it() {
    for row in include_str!("data/return-probe.pict.tsv").lines().skip(1) {
        let fields: Vec<&str> = row.trim_end().split('\t').collect();
        let [result, callable, path] = fields.as_slice() else {
            panic!("bad PICT row: {row}");
        };
        let (ty, tail, early) = match *result {
            "Bool" => ("bool", "true", "false"),
            "Option" => ("Option<u8>", "Some(1)", "None"),
            other => panic!("unknown result: {other}"),
        };
        let prefix = match *path {
            "Fallthrough" => String::new(),
            "Return" => format!("if flag {{ return {early}; }}"),
            "Question" => "let _ = None::<()>?;".to_owned(),
            "Macro" => "maybe_return!(flag);".to_owned(),
            "Closure" if *result == "Option" => "let _ = || Some(None::<()>?);".to_owned(),
            "Closure" => "let _ = || -> bool { return false; };".to_owned(),
            "Async" => "let _ = async { return false; };".to_owned(),
            "NestedItem" => "fn inner() -> bool { return false; }".to_owned(),
            "InlineConst" => "let _ = const { stringify!(return false) };".to_owned(),
            other => panic!("unknown path: {other}"),
        };
        let method = format!("fn check(flag: bool) -> {ty} {{ {prefix} {tail} }}");
        let source = match *callable {
            "Free" => method,
            "Method" => format!("struct S; impl S {{ {method} }}"),
            "Default" => format!("trait T {{ {method} }}"),
            other => panic!("unknown callable: {other}"),
        };
        let sites = discover(&source, 0).unwrap().sites;
        let has_result_probe = sites
            .iter()
            .any(|site| matches!(&site.kind, SiteKind::Return { name, .. } if name == "check"));
        assert_eq!(
            has_result_probe,
            !matches!(*path, "Return" | "Question" | "Macro"),
            "{row}: {source}",
        );
        assert!(
            sites
                .iter()
                .any(|site| matches!(&site.kind, SiteKind::Body { name, .. } if name == "check")),
            "missing body replacement: {source}",
        );
    }
}

#[test]
fn already_asserted_early_results_are_not_reported_as_input_gaps() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("return-probe");
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"return-probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n",
    )
    .unwrap();
    fs::write(
        dir.join("src/lib.rs"),
        r#"
pub fn accepts(flag: bool) -> bool {
    if !flag {
        return false;
    }
    true
}

pub fn maybe(flag: bool) -> Option<u8> {
    let _ = Some(()).filter(|_| flag)?;
    Some(1)
}

#[test]
fn known_true_and_false_are_both_asserted() {
    assert!(accepts(true));
    assert!(!accepts(false));
    assert_eq!(maybe(true), Some(1));
    assert_eq!(maybe(false), None);
}
"#,
    )
    .unwrap();
    for mode in [None, Some("--source-only")] {
        let run = Command::new(env!("CARGO_BIN_EXE_cargo-tmt"))
            .arg(&dir)
            .args(["--no-effects", "--quiet", "--json"])
            .args(mode)
            .output()
            .unwrap();
        let report: Value = serde_json::from_slice(&run.stdout)
            .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&run.stderr)));
        assert_eq!(run.status.code(), Some(0), "{mode:?}: {report:#}");
        assert_eq!(report["findings"], serde_json::json!([]));
        assert_eq!(report["mutants_killed"], report["mutants_run"]);
        assert!(
            report["mutants_killed"].as_u64().unwrap() >= 5,
            "{report:#}"
        );
    }
}
