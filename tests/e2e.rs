//! Runs the `tmt` binary on the fixture crates.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::fixture_command;
use serde_json::Value;

/// Copies a fixture into this test's own directory so tests can edit it and run in parallel.
fn fixture(name: &str, test: &str) -> PathBuf {
    let from = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name);
    let to = Path::new(env!("CARGO_TARGET_TMPDIR")).join(test);
    let _ = fs::remove_dir_all(&to);
    copy(&from, &to);
    to
}

fn copy(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name();
        if name == "target" || name == "Cargo.lock" {
            continue;
        }
        if entry.file_type().unwrap().is_dir() {
            copy(&entry.path(), &to.join(name));
        } else {
            fs::copy(entry.path(), to.join(name)).unwrap();
        }
    }
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

fn tmt(dir: &Path, args: &[&str]) -> Run {
    let output = fixture_command(env!("CARGO_BIN_EXE_cargo-tmt"))
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    Run {
        code: output.status.code().unwrap(),
        stdout: String::from_utf8(output.stdout).unwrap(),
        stderr: String::from_utf8(output.stderr).unwrap(),
    }
}

fn json(dir: &Path) -> (i32, Value) {
    let run = tmt(dir, &["--json", "--quiet"]);
    let value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("not JSON ({e}):\n{}\n{}", run.stdout, run.stderr));
    (run.code, value)
}

/// A report without what it says about how it was asked for.
fn but_how(mut report: Value) -> Value {
    report.as_object_mut().unwrap().remove("analysis");
    report
}

fn after_confirmed_timeouts(mut report: Value, count: usize) -> Value {
    let rechecks = report.as_object_mut().unwrap().remove("timeout_rechecks");
    assert_eq!(
        rechecks,
        Some(serde_json::json!({
            "first_pass": count,
            "confirmed": count,
            "recovered": 0,
            "unconfirmed": 0,
        }))
    );
    report
}

fn findings<'a>(report: &'a Value, kind: &str) -> Vec<&'a Value> {
    report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["kind"] == kind)
        .collect()
}

fn strings(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect()
}

fn append(path: &Path, text: &str) {
    let mut source = fs::read_to_string(path).unwrap();
    source.push_str(text);
    fs::write(path, source).unwrap();
}

#[test]
fn unpinned_boundary_is_an_input_gap_until_the_boundary_is_tested() {
    let dir = fixture("eligibility", "eligibility");
    let before = fs::read_to_string(dir.join("src/lib.rs")).unwrap();
    let (code, report) = json(&dir);
    assert_eq!(code, 2, "{report:#}");
    let gaps = findings(&report, "input_gap");
    assert_eq!(gaps.len(), 1, "{report:#}");
    assert_eq!(
        report["findings"].as_array().unwrap().len(),
        1,
        "{report:#}"
    );
    let gap = gaps[0];
    assert_eq!(gap["file"], "src/lib.rs");
    assert_eq!(gap["line"], 4);
    assert_eq!(gap["code"], "age >= 18");
    assert_eq!(strings(&gap["survivors"]), ["`age >= 19`", "`age >= 20`"]);
    assert_eq!(
        strings(&gap["add_tests"]),
        ["`age` = 18 (the original gives true)"]
    );
    assert_eq!(gap["verified"], true);
    assert_eq!(
        fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
        before,
        "the user's source is untouched"
    );

    // The text report says the same thing.
    let text = tmt(&dir, &[]);
    assert_eq!(text.code, 2);
    assert!(
        text.stdout.contains("INPUT GAP  src/lib.rs:4:5"),
        "{}",
        text.stdout
    );
    assert!(
        text.stdout.contains("add a test with `age` = 18"),
        "{}",
        text.stdout
    );
    assert!(text.stderr.contains("tracing"), "{}", text.stderr);

    // Take the advice.
    append(
        &dir.join("src/lib.rs"),
        "\n#[test]\nfn eighteen_is_accepted() {\n    assert!(is_eligible(18));\n}\n",
    );
    let (code, report) = json(&dir);
    assert_eq!(code, 0, "{report:#}");
    assert_eq!(
        report["findings"].as_array().unwrap().len(),
        0,
        "{report:#}"
    );
    assert_eq!(
        report["mutants_run"], report["mutants_killed"],
        "{report:#}"
    );
}

#[test]
fn missing_assertion_is_an_assertion_gap_until_the_state_is_checked() {
    let dir = fixture("batch-store", "batch-store");
    let (code, report) = json(&dir);
    assert_eq!(code, 2, "{report:#}");
    let gaps = findings(&report, "assertion_gap");
    assert_eq!(gaps.len(), 2, "{report:#}");
    assert_eq!(gaps[0]["code"], "self.check_preconditions(batch)?;");
    assert_eq!(
        strings(&gaps[0]["survivors"]),
        ["delete `self.check_preconditions(batch)?;`"]
    );
    // The check that always passes is the same hole seen from the function.
    assert_eq!(
        strings(&gaps[1]["survivors"]),
        ["the body of `check_preconditions` replaced by `Ok(())`"]
    );
    assert!(
        strings(&gaps[0]["tests"])
            .contains(&"tests::commit_fails_when_an_update_target_is_missing"),
        "{report:#}"
    );

    // Assert that a failed batch leaves nothing behind.
    append(
        &dir.join("src/lib.rs"),
        r#"
#[test]
fn failed_batch_writes_nothing() {
    let mut store = Store::default();
    let batch = [
        Write::Set { key: "a".into(), value: "1".into() },
        Write::Update { key: "b".into(), value: "2".into() },
    ];
    assert!(store.commit(&batch).is_err());
    assert_eq!(store.get("a"), None);
}
"#,
    );
    let (code, report) = json(&dir);
    assert_eq!(code, 0, "{report:#}");
    assert_eq!(
        report["findings"].as_array().unwrap().len(),
        0,
        "{report:#}"
    );
}

#[test]
fn awkward_constructs() {
    let dir = fixture("kitchen-sink", "kitchen-sink");
    let run = tmt(&dir, &["--json", "--quiet", "--rebuild"]);
    let (code, report) = (
        run.code,
        serde_json::from_str::<Value>(&run.stdout).unwrap(),
    );
    assert_eq!(code, 2, "{report:#}");
    let all = report["findings"].as_array().unwrap();
    let at = |code: &str, kind: &str| {
        all.iter()
            .find(|f| f["code"] == code && f["kind"] == kind)
            .unwrap_or_else(|| panic!("no {kind} at `{code}`: {report:#}"))
    };

    // Both deletions fail as probes but are attempted as source edits.
    assert_eq!(report["skipped_sites"], 0, "{report:#}");
    let repaired: Vec<_> = report["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|site| site["reason"] == "probe_replaced_by_source_rebuild")
        .collect();
    assert_eq!(repaired.len(), 2, "{report:#}");
    for line in [64, 104] {
        assert!(
            repaired.iter().any(|site| site["file"] == "src/lib.rs"
                && site["line"] == line
                && site["end_line"].as_u64().unwrap() >= line
                && !site["diagnostic"].as_str().unwrap().is_empty()),
            "{report:#}"
        );
    }
    assert!(
        report["unviable_mutants"].as_u64().unwrap() >= 2,
        "{report:#}"
    );
    assert!(
        !all.iter()
            .any(|f| f["code"].as_str().unwrap().contains("fail_hard")),
        "{report:#}"
    );

    // Integers: the boundary and the inputs that pin it.
    let clamp = at("value > 100", "input_gap");
    assert_eq!(
        strings(&clamp["add_tests"]),
        [
            "`value` = 100 (the original gives false)",
            "`value` = 101 (the original gives true)"
        ]
    );
    // Floats: never equal, and any threshold between the two observed values survives.
    let float = at("celsius > 30.5", "input_gap");
    assert_eq!(
        strings(&float["survivors"]),
        [
            "`celsius >= 30.5`",
            "`celsius > 35.25`",
            "`celsius > 20.25`"
        ]
    );
    assert_eq!(
        float["observed"],
        "nearest values the tests passed: `celsius` = 10.0, 40.0; any threshold between them passes"
    );
    // A condition that is never true in the tests.
    assert_eq!(
        strings(&at("n == 0", "input_gap")["survivors"]),
        ["`false`"]
    );
    // Observed from a spawned thread, where the test name is unknown.
    assert_eq!(
        strings(&at("x > 5", "input_gap")["survivors"])[0],
        "`x > 6`"
    );
    // Statements the tests execute but do not check, in unit and integration tests.
    at("self.total += 1;", "assertion_gap");
    assert_eq!(
        strings(&at("ensure_positive(n)?;", "assertion_gap")["tests"]),
        ["parses_numbers"]
    );

    // Known limit: `max_of` returns the same value for `a >= b` and `a > b` when the two
    // are equal, so this mutant is equivalent, and it is reported all the same.
    assert_eq!(
        strings(&at("a >= b", "assertion_gap")["survivors"]),
        ["`a > b`"]
    );

    let codes: Vec<&str> = all.iter().map(|f| f["code"].as_str().unwrap()).collect();
    // `x >= 0` on an unsigned value cannot be false: an equivalent mutant, not a gap.
    assert!(!codes.contains(&"x >= 0"), "{report:#}");
    // Fully tested code is quiet.
    for quiet in [
        "a < b",
        "**x < limit",
        "i < items.len()",
        "items[i] == stop",
        "a == *b",
        "items.len() > 0",
    ] {
        assert!(
            !codes.contains(&quiet),
            "`{quiet}` should have no finding: {report:#}"
        );
    }
    // No probe can be put in a `const fn`: its mutants are built one by one. The tests
    // only ask for a number below 10.
    let in_const = |code: &str| -> Vec<&str> {
        all.iter()
            .filter(|f| f["code"] == code)
            .flat_map(|f| strings(&f["survivors"]))
            .collect()
    };
    assert_eq!(in_const("a < 10"), ["`a <= 10`"], "{report:#}");
    let kinds: Vec<&str> = all
        .iter()
        .filter(|f| f["code"] == "a < 10")
        .map(|f| f["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["survivor"], "built on its own: {report:#}");
    assert_eq!(
        in_const("const fn const_limit(a: u32) -> bool"),
        ["the body of `const_limit` replaced by `true`"],
        "{report:#}"
    );
    // A compound assignment to more than a variable is built too: the test counts hits.
    assert!(
        !codes.contains(&"*self.hits.entry(key.to_string()).or_insert(0) += 1"),
        "{report:#}"
    );
    let unreached: Vec<&str> = report["unreached"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["code"].as_str().unwrap())
        .collect();
    // `total` is a getter no test calls: nothing to probe in it, but a body to replace.
    assert_eq!(
        unreached,
        [
            "fn total(&self) -> u32",
            "fn never_called(x: u8) -> bool",
            "x == 7"
        ]
    );
    // `unsigned_floor` is `x >= 0` on an unsigned value: constant for a reason no test can
    // change, so neither the comparison nor the function is reported.
    assert!(
        !codes.contains(&"fn unsigned_floor(x: u32) -> bool"),
        "{report:#}"
    );

    // The verdicts match after the extra timeout rechecks.
    let parallel = tmt(&dir, &["--json", "--quiet", "--rebuild", "--jobs", "4"]);
    let parallel: Value = serde_json::from_str(&parallel.stdout).unwrap();
    assert_eq!(after_confirmed_timeouts(parallel, 2), report);
    assert_eq!(
        report["unstable"].as_array().unwrap().len(),
        0,
        "{report:#}"
    );

    let text = tmt(&dir, &["--show-unreached", "--quiet", "--no-verify"]);
    assert!(
        text.stdout.contains("UNREACHED  src/lib.rs:"),
        "{}",
        text.stdout
    );
    assert!(
        text.stdout.contains("[from the trace, not re-run]"),
        "{}",
        text.stdout
    );
    assert_eq!(text.stderr, "");
}

#[test]
fn failing_tests_stop_the_analysis() {
    let dir = fixture("eligibility", "failing-baseline");
    append(
        &dir.join("src/lib.rs"),
        "\n#[test]\nfn broken() {\n    assert!(is_eligible(1), \"baseline diagnostic sentinel\");\n}\n",
    );
    let work = dir.join("target/tmt");
    fs::create_dir_all(&work).unwrap();
    let sentinel = work.join("baseline-0.log");
    fs::write(&sentinel, "operator's file").unwrap();
    let run = tmt(&dir, &["--json", "--quiet"]);
    assert_eq!(run.code, 1, "{}", run.stdout);
    assert!(
        run.stderr.contains("do not pass on the unmutated code"),
        "{}",
        run.stderr
    );
    assert!(run.stderr.contains("test broken"), "{}", run.stderr);
    assert!(
        run.stderr.contains("baseline diagnostic sentinel"),
        "{}",
        run.stderr
    );
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "operator's file");
    assert_eq!(run.stdout, "");
    let no_sites = tmt(&dir, &["--json", "--quiet", "--lines", "README.md"]);
    assert_eq!(no_sites.code, 0, "{}", no_sites.stderr);
    let report: Value = serde_json::from_str(&no_sites.stdout).unwrap();
    assert_eq!(report["evaluation"], "no_sites");
}

#[test]
fn broken_source_stops_the_analysis() {
    let dir = fixture("eligibility", "broken-source");
    append(
        &dir.join("src/lib.rs"),
        "\npub fn broken() -> u32 { \"text\" }\n",
    );
    let run = tmt(&dir, &["--quiet"]);
    assert_eq!(run.code, 1, "{}", run.stdout);
    assert!(run.stderr.contains("does not build"), "{}", run.stderr);

    let missing = tmt(Path::new("/nonexistent/tmt-fixture"), &["--quiet"]);
    assert_eq!(missing.code, 1);
    assert!(
        missing.stderr.contains("no such directory"),
        "{}",
        missing.stderr
    );
}

#[test]
fn source_timeout_reports_incomplete_analysis_to_ci() {
    let dir = fixture("looping", "source-loop-timeout");
    // Long enough that, on a busy machine, only the mutants that never end run out of it.
    let run = tmt(
        &dir,
        &["--source-only", "--timeout", "4", "--quiet", "--json"],
    );
    assert_eq!(run.code, 3, "{}", run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(report["mutants_timed_out"], 2, "{report:#}");
    assert!(
        report["findings"].as_array().unwrap().is_empty(),
        "{report:#}"
    );
    assert_eq!(report["analysis"]["timeout_seconds"], 4, "{report:#}");
}

#[test]
fn endless_loop_mutant_is_killed_by_the_timeout() {
    let dir = fixture("looping", "looping");
    let started = std::time::Instant::now();
    let (code, report) = json(&dir);
    assert_eq!(code, 0, "{report:#}");
    // Deleting `i += 1` loops forever, and so does `i *= 1`; `i -= 1`, the negated
    // condition and the two constant results fail quickly.
    assert_eq!(report["mutants_run"], 6, "{report:#}");
    assert_eq!(report["mutants_killed"], 6, "{report:#}");
    assert_eq!(report["mutants_timed_out"], 2, "{report:#}");
    assert!(
        started.elapsed() >= std::time::Duration::from_secs(5),
        "the timeout was waited for"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(120),
        "and the mutant was stopped"
    );
    // Run three at a time, a mutant that ran out of time is run once more on its own:
    // the two that never end run out of time again, with the same final verdicts.
    let run = tmt(&dir, &["--json", "--quiet", "--jobs", "3"]);
    let together: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(after_confirmed_timeouts(together, 2), report);
}

#[test]
fn no_std_crate_and_custom_work_dir() {
    let dir = fixture("no-std", "no-std");
    let work = Path::new(env!("CARGO_TARGET_TMPDIR")).join("no-std-work");
    let _ = fs::remove_dir_all(&work);
    let run = tmt(
        &dir,
        &["--json", "--quiet", "--work-dir", work.to_str().unwrap()],
    );
    assert_eq!(run.code, 2, "{}{}", run.stdout, run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    let gaps = findings(&report, "input_gap");
    assert_eq!(gaps.len(), 1, "{report:#}");
    assert_eq!(gaps[0]["code"], "raw > 255");
    assert_eq!(report["skipped_sites"], 0, "{report:#}");
    // Everything the run produced is in the work directory, nothing in the package.
    assert!(work.join("src/src/lib.rs").exists());
    assert!(work.join("target").exists());
    assert!(!dir.join("target").exists());
}

#[test]
fn file_scopes_keep_catalogue_diagnostics_and_candidates() {
    for row in include_str!("data/scoped-files.pict.tsv").lines().skip(1) {
        let columns: Vec<_> = row.split('\t').collect();
        let [scope, peer, copy] = columns.as_slice() else {
            panic!("{row}")
        };
        let dir = fixture("eligibility", &format!("file-scope-{scope}-{peer}-{copy}"));
        let declaration = if *peer == "disabled" {
            "#[cfg(test)] mod leaf;"
        } else {
            "mod leaf;"
        };
        let code = "pub fn allowed(n: u8) -> bool { n >= 10 }";
        fs::write(dir.join("src/lib.rs"), format!("{declaration}\n{code}\n")).unwrap();
        fs::write(
            dir.join("src/leaf.rs"),
            if *peer == "malformed" {
                "fn invalid("
            } else {
                code
            },
        )
        .unwrap();

        if *copy == "warm" {
            let warm = tmt(&dir, &["--list", "--json", "--quiet"]);
            assert_eq!(warm.code, 0, "{row}: {}", warm.stderr);
        }
        let mut args = vec!["--list", "--json", "--quiet"];
        if *scope != "leaf" {
            args.extend(["--lines", "src/lib.rs"]);
        }
        if *scope != "root" {
            args.extend(["--lines", "src/leaf.rs"]);
        }
        let run = tmt(&dir, &args);
        assert_eq!(run.code, 0, "{row}: {}", run.stderr);
        let report: Value = serde_json::from_str(&run.stdout).unwrap();
        let candidates: std::collections::BTreeSet<_> = report["mutants"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["edits"][0]["file"].as_str().unwrap())
            .collect();
        assert_eq!(candidates.contains("src/lib.rs"), *scope != "leaf", "{row}");
        assert_eq!(
            candidates.contains("src/leaf.rs"),
            *scope != "root" && *peer == "valid",
            "{row}"
        );
        assert_eq!(
            report["skipped"].as_array().map_or(0, Vec::len),
            usize::from(*scope != "root" && *peer == "malformed"),
            "{row}"
        );
    }
}

#[test]
fn root_source_mutants_keep_the_runtime_only_for_probes_in_other_files() {
    for peer in [false, true] {
        let dir = fixture("eligibility", &format!("source-root-runtime-{peer}"));
        let work = dir.join("target/source-root");
        let mut source = r#"
pub const fn const_rule(value: u8) -> bool { value >= 10 }
pub fn probe_rule(value: u8) -> bool { value >= 10 }
#[test]
fn checks() {
    for rule in [const_rule, probe_rule] {
        assert!(!rule(9));
        assert!(rule(10));
        assert!(rule(11));
    }
}
"#
        .to_owned();
        if peer {
            source.push_str(
                "mod peer;\n#[test] fn checks_peer() { assert!(peer::probe_rule(10)); }\n",
            );
            fs::write(
                dir.join("src/peer.rs"),
                "pub fn probe_rule(value: u8) -> bool { value >= 10 }",
            )
            .unwrap();
        }
        fs::write(dir.join("src/lib.rs"), source).unwrap();
        fs::write(
            dir.join("build.rs"),
            r#"
use std::io::Write as _;
fn main() {
    println!("cargo:rerun-if-changed=src/lib.rs");
    let source = std::fs::read_to_string("src/lib.rs").unwrap();
    let has_probes = source.contains("crate::__tmt_rt::");
    let has_runtime = source.lines().any(|line| line.trim() == "mod __tmt_rt;");
    let mut log = std::fs::OpenOptions::new().create(true).append(true)
        .open("../root-probe-log").unwrap();
    writeln!(log, "{has_probes} {has_runtime}").unwrap();
}
"#,
        )
        .unwrap();
        let run = tmt(
            &dir,
            &[
                "--rebuild",
                "--no-effects",
                "--quiet",
                "--json",
                "--work-dir",
                work.to_str().unwrap(),
            ],
        );
        assert!(matches!(run.code, 0 | 2), "{}\n{}", run.stderr, run.stdout);
        let log = fs::read_to_string(work.join("root-probe-log")).unwrap();
        assert!(log.lines().any(|line| line == "true true"), "{log}");
        assert!(
            log.lines().any(|line| line == format!("false {peer}")),
            "{log}"
        );
        assert!(
            !log.lines().any(|line| line == format!("false {}", !peer)),
            "{log}"
        );
        let restored = fs::read_to_string(work.join("src/src/lib.rs")).unwrap();
        assert!(restored.contains("crate::__tmt_rt::") && restored.contains("mod __tmt_rt;"));
    }
}

#[test]
fn a_scoped_campaign_numbers_only_retained_probes() {
    let dir = fixture("eligibility", "compact-probe-ids");
    let work = dir.join("target/scoped");
    let mut lib = "mod other;\n".to_string();
    for n in 0..96 {
        lib.push_str(&format!(
            "pub fn earlier_{n}(x: u32) -> bool {{ x >= 10 }}\n"
        ));
    }
    lib.push_str("pub fn selected(x: u32) -> bool { x >= 10 }\n");
    lib.push_str(
        "#[test] fn selected_boundaries() { for pred in [selected, other::selected] { \
         assert!(!pred(9)); assert!(pred(10)); assert!(pred(11)); } }\n",
    );
    fs::write(dir.join("src/lib.rs"), lib).unwrap();
    fs::write(
        dir.join("src/other.rs"),
        "pub fn selected(x: u32) -> bool { x >= 10 }\n",
    )
    .unwrap();
    let run = tmt(
        &dir,
        &[
            "--quiet",
            "--json",
            "--work-dir",
            work.to_str().unwrap(),
            "--lines",
            "src/lib.rs:98",
            "--lines",
            "src/other.rs:1",
        ],
    );
    assert_eq!(run.code, 0, "{}\n{}", run.stderr, run.stdout);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    let sites = report["sites"].as_u64().unwrap();
    assert!(report["mutants_run"].as_u64().unwrap() > 0);
    assert_eq!(report["mutants_run"], report["mutants_killed"]);
    let ids: Vec<u64> = ["lib.rs", "other.rs"]
        .into_iter()
        .map(|file| {
            let source = fs::read_to_string(work.join("src/src").join(file)).unwrap();
            let (_, probe) = source.split_once("__tmt_rt::ord(").expect(&source);
            let (id, _) = probe.split_once("u32,").expect(probe);
            id.parse().unwrap()
        })
        .collect();
    assert_ne!(ids[0], ids[1]);
    assert!(ids.iter().all(|&id| id < sites), "{ids:?} >= {sites}");
}

#[test]
fn all_production_targets_follow_modules_and_report_unbuilt_targets() {
    let dir = fixture("eligibility", "all-production-targets");
    let manifest = dir.join("Cargo.toml");
    append(
        &manifest,
        r#"
[lib]
name = "engine"
path = "src/engine.rs"

[[bin]]
name = "cli-tool"
path = "src/cli_entry.rs"

[[bin]]
name = "gated"
path = "gated/entry.rs"
required-features = ["optional"]

[features]
optional = []
"#,
    );
    let write = |path: &str, content: &str| {
        let dest = dir.join(path);
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(dest, content).unwrap();
    };
    let marker = r#"
pub fn marker(n: u32) -> u32 { n + 1 }
#[test] fn checks_marker() { assert_eq!(marker(2), 3); }
"#;
    let shared = r#"
mod shared_direct;
mod shared_nested { mod leaf; }
#[path = "../common/shared.rs"] mod shared_explicit;
"#;
    write(
        "src/engine.rs",
        &format!(
            r#"{marker}{shared} mod primary_nested {{ mod leaf; }} #[path = "../common/primary.rs"] mod primary_explicit; #[cfg(test)] mod decoy;"#
        ),
    );
    write(
        "src/cli_entry.rs",
        &format!(
            r#"{marker}{shared} fn main() {{ println!("{{}}:{{}}", marker(2), secondary_nested::leaf::marker(2)); }} mod secondary_nested {{ pub(super) mod leaf; }} #[cfg(feature = "optional")] mod secondary_inactive; #[path = "../common/secondary.rs"] mod secondary_explicit;"#
        ),
    );
    write(
        "gated/entry.rs",
        &format!(
            r#"{marker} fn main() {{}} mod nested {{ mod leaf; }} #[path = "../common/gated.rs"] mod explicit;"#
        ),
    );
    let active = [
        ("src/engine.rs", false),
        ("src/primary_nested/leaf.rs", false),
        ("common/primary.rs", false),
        ("src/cli_entry.rs", true),
        ("src/secondary_nested/leaf.rs", true),
        ("src/secondary_inactive.rs", true),
        ("common/secondary.rs", true),
        ("gated/entry.rs", true),
        ("gated/nested/leaf.rs", true),
        ("common/gated.rs", true),
        ("src/shared_direct.rs", true),
        ("src/shared_nested/leaf.rs", true),
        ("common/shared.rs", true),
    ];
    for (path, _) in &active {
        if !path.ends_with("engine.rs")
            && !path.ends_with("cli_entry.rs")
            && !path.ends_with("entry.rs")
        {
            write(path, marker);
        }
    }
    write("src/secondary_inactive.rs", marker);
    write("src/orphan.rs", marker);
    write("src/decoy.rs", marker);
    write("src/lib.rs", marker);
    let run = tmt(&dir, &["--list", "--json", "--quiet"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let catalog: Value = serde_json::from_str(&run.stdout).unwrap();
    let additions: Vec<_> = catalog["mutants"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|entry| entry["what"] == "+" && entry["into"] == "-")
        .map(|entry| {
            (
                entry["edits"][0]["file"].as_str().unwrap(),
                entry["requires_rebuild"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(additions.len(), active.len(), "{additions:?}");
    for expected in active {
        assert!(additions.contains(&expected), "{expected:?}: {additions:?}");
    }
    write(
        "tests/process.rs",
        r#"
#[test]
fn checks_binary() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_cli-tool")).output().unwrap();
    assert!(output.status.success());
    assert_eq!(output.stdout, b"3:3\n");
}
"#,
    );
    let run = tmt(
        &dir,
        &[
            "--source-only",
            "--json",
            "--quiet",
            "--test-target",
            "process",
            "--lines",
            "src/cli_entry.rs:2",
        ],
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(report["mutants_run"], 4, "{report:#}");
    assert_eq!(report["mutants_killed"], 4, "{report:#}");
    for (file, code, killed, unevaluated) in [
        ("src/secondary_nested/leaf.rs", 0, 4, 0),
        ("src/secondary_inactive.rs", 3, 0, 4),
    ] {
        let run = tmt(
            &dir,
            &[
                "--source-only",
                "--json",
                "--quiet",
                "--test-target",
                "process",
                "--lines",
                file,
            ],
        );
        assert_eq!(run.code, code, "{}{}", run.stderr, run.stdout);
        let report: Value = serde_json::from_str(&run.stdout).unwrap();
        assert_eq!(report["mutants_killed"], killed, "{report:#}");
        assert_eq!(report["mutants_not_evaluated"], unevaluated, "{report:#}");
        assert!(
            report["findings"].as_array().unwrap().is_empty(),
            "{report:#}"
        );
    }

    for feature in [false, true] {
        let mut args = vec![
            "--rebuild",
            "--json",
            "--quiet",
            "--lines",
            "gated/nested/leaf.rs",
        ];
        if feature {
            args.extend(["--features", "optional"]);
        }
        let run = tmt(&dir, &args);
        assert_eq!(run.code, if feature { 0 } else { 3 }, "{}", run.stderr);
        let report: Value = serde_json::from_str(&run.stdout).unwrap();
        assert_eq!(
            report["mutants_run"],
            if feature { 4 } else { 0 },
            "{report:#}"
        );
        assert_eq!(
            report["mutants_not_evaluated"],
            if feature { 0 } else { 4 },
            "{report:#}"
        );
        assert_eq!(
            report["mutants_killed"],
            if feature { 4 } else { 0 },
            "{report:#}"
        );
        assert!(
            report["findings"].as_array().unwrap().is_empty(),
            "{report:#}"
        );
    }
}

#[test]
fn source_only_distinguishes_inactive_modules_from_survivors() {
    let dir = fixture("eligibility", "conditional-source-module");
    append(&dir.join("Cargo.toml"), "\n[features]\noptional = []\n");
    append(
        &dir.join("src/lib.rs"),
        r#"
#[cfg(feature = "optional")]
#[cfg_attr(feature = "optional", path = "optional impl.rs")]
mod optional;
#[cfg(feature = "optional")]
#[test]
fn checks_optional() { assert_eq!(optional::add(1, 2), 3); }

#[cfg_attr(test, path = "test_engine.rs")]
mod engine;
include!("included.rs");
#[test]
fn checks_included() { assert_eq!(included(1, 2), 3); }
"#,
    );
    fs::write(
        dir.join("src/optional impl.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    fs::write(
        dir.join("src/included.rs"),
        "pub fn included(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    fs::create_dir(dir.join("tests")).unwrap();
    fs::write(
        dir.join("tests/fixture.rs"),
        r#"
#[test]
fn can_read_disabled_source_as_text() {
    assert!(include_str!("../src/optional impl.rs").contains("pub fn add"));
}
"#,
    )
    .unwrap();
    for file in ["engine.rs", "test_engine.rs"] {
        fs::write(dir.join("src").join(file), "pub fn value() -> u32 { 4 }\n").unwrap();
    }
    let run = tmt(&dir, &["--source-only", "--list", "--quiet", "--json"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let catalogue: Value = serde_json::from_str(&run.stdout).unwrap();
    let mutants = catalogue["mutants"].as_array().unwrap();
    for file in ["src/optional impl.rs", "src/included.rs"] {
        assert!(
            mutants
                .iter()
                .any(|entry| entry["edits"][0]["file"] == file),
            "{catalogue:#}"
        );
    }
    assert!(
        mutants
            .iter()
            .any(|entry| entry["edits"][0]["file"] == "src/engine.rs")
    );
    assert!(
        !mutants
            .iter()
            .any(|entry| entry["edits"][0]["file"] == "src/test_engine.rs")
    );
    let baseline = [
        "--source-only",
        "--quiet",
        "--json",
        "--lines",
        "src/optional impl.rs",
    ];
    let run = tmt(&dir, &baseline);
    assert_eq!(run.code, 3, "{}{}", run.stderr, run.stdout);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(report["mutants_not_evaluated"], 5, "{report:#}");
    assert!(
        report["findings"].as_array().unwrap().is_empty(),
        "{report:#}"
    );
    let run = tmt(&dir, &[&baseline[..], &["--features", "optional"]].concat());
    assert_eq!(run.code, 0, "{}{}", run.stderr, run.stdout);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(report["mutants_killed"], 5, "{report:#}");
    assert_eq!(report["mutants_not_evaluated"], 0, "{report:#}");
}

#[test]
fn a_library_root_directly_included_in_a_test_still_builds() {
    let dir = fixture("eligibility", "test-includes-library-root");
    fs::create_dir(dir.join("tests")).unwrap();
    fs::write(
        dir.join("tests/direct.rs"),
        r#"
#[path = "../src/lib.rs"]
mod api;

#[test]
fn checks_library() {
    assert_eq!(api::add(1, 2), 3);
}
"#,
    )
    .unwrap();
    fs::write(
        dir.join("src/lib.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    let run = tmt(&dir, &["--rebuild", "--quiet", "--json"]);
    assert_eq!(run.code, 0, "{}{}", run.stderr, run.stdout);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(report["mutants_killed"], 5, "{report:#}");
}

#[test]
fn member_relative_module_paths_can_be_reused_in_line_filters() {
    let root = fixture("multi", "member-relative-module");
    let ws = root.join("ws");
    append(
        &ws.join("core/src/lib.rs"),
        "\n#[path = \"../../shared.rs\"] mod shared;\n",
    );
    fs::write(
        ws.join("shared.rs"),
        "pub fn add(a: i32, b: i32) -> i32 { a + b }\n",
    )
    .unwrap();
    let run = tmt(
        &ws,
        &[
            "--package",
            "ws-core",
            "--list",
            "--json",
            "--quiet",
            "--lines",
            "../shared.rs:1",
        ],
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let catalogue: Value = serde_json::from_str(&run.stdout).unwrap();
    let mutants = catalogue["mutants"].as_array().unwrap();
    assert!(!mutants.is_empty(), "{catalogue:#}");
    assert!(
        mutants
            .iter()
            .all(|mutant| mutant["edits"][0]["file"] == "../shared.rs")
    );
}

#[test]
fn source_only_can_use_doctests_of_other_workspace_members() {
    let root = fixture("multi", "other-member-doctest");
    let ws = root.join("ws");
    append(
        &ws.join("core/src/lib.rs"),
        "\npub fn only_doc(n: i32) -> i32 { n + 1 }\n",
    );
    append(
        &ws.join("app/src/lib.rs"),
        r#"
/// ```
/// assert_eq!(ws_core::only_doc(7), 8);
/// ```
pub fn doc_checks_the_core() {}
"#,
    );
    let source = fs::read_to_string(ws.join("core/src/lib.rs")).unwrap();
    let line = source.lines().count();
    let run = tmt(
        &ws,
        &[
            "--package",
            "ws-core",
            "--test-workspace",
            "--doc",
            "--source-only",
            "--quiet",
            "--json",
            "--lines",
            &format!("src/lib.rs:{line}"),
        ],
    );
    assert_eq!(run.code, 0, "{}{}", run.stderr, run.stdout);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert!(report["mutants_killed"].as_u64().unwrap() > 0, "{report:#}");
    assert_eq!(
        report["mutants_killed"], report["mutants_run"],
        "{report:#}"
    );
}

#[test]
fn workspace_member_by_name_and_by_directory() {
    // `ws/app` depends on `../../outside`, which is not part of the workspace.
    let root = fixture("multi", "multi");
    let ws = root.join("ws");

    let run = tmt(&ws, &["--json", "--quiet", "--package", "ws-core"]);
    assert_eq!(run.code, 2, "{}{}", run.stdout, run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(report["package"], "ws-core");
    assert_eq!(findings(&report, "input_gap")[0]["code"], "size > 1000");
    assert_eq!(findings(&report, "input_gap")[0]["file"], "src/lib.rs");

    let (code, report) = json(&ws.join("app"));
    assert_eq!(report["package"], "ws-app");
    assert_eq!(
        report["sites"], 4,
        "only the member's own code is mutated: {report:#}"
    );
    // The dependency outside the workspace always answers true, so it never decides.
    assert_eq!(code, 2, "{report:#}");
    let gaps = findings(&report, "input_gap");
    assert_eq!(gaps.len(), 1, "{report:#}");
    assert_eq!(
        gaps[0]["code"],
        "ws_core::is_large(size) && outside::enabled()"
    );
    assert_eq!(
        strings(&gaps[0]["survivors"]),
        ["`ws_core::is_large(size) && { outside::enabled(); true }`"]
    );

    let ambiguous = tmt(&ws, &["--quiet"]);
    assert_eq!(ambiguous.code, 1);
    assert!(
        ambiguous.stderr.contains("--package"),
        "{}",
        ambiguous.stderr
    );
    let unknown = tmt(&ws, &["--quiet", "--package", "nope"]);
    assert_eq!(unknown.code, 1);
}

#[test]
fn workspace_and_catalog_use_the_same_package_selection() {
    let root = fixture("multi", "compatibility-cli");
    let workspace = root.join("ws");
    let work = workspace.join("analysis");
    // Full strength-three table from cli.pict.
    for selector in [
        &["-p", "ws-core"][..],
        &["--workspace"][..],
        &["-p", "ws-core", "-p", "ws-app"][..],
    ] {
        for json in [false, true] {
            for list in [true, false] {
                let mut args = vec!["--quiet", "--work-dir", work.to_str().unwrap()];
                args.extend(selector);
                args.extend(json.then_some("--json"));
                args.extend(list.then_some("--list"));
                let run = tmt(&workspace, &args);
                assert_eq!(
                    run.code,
                    if list { 0 } else { 2 },
                    "{args:?}: {}",
                    run.stderr
                );
                assert!(run.stdout.contains("ws-core"), "{args:?}: {}", run.stdout);
                assert_eq!(run.stdout.contains("ws-app"), selector.len() != 2);
                assert!(!run.stdout.lines().any(|line| line.starts_with("outside:")));
                if !json {
                    continue;
                }
                let data: Value = serde_json::from_str(&run.stdout).unwrap();
                if selector.len() != 2 {
                    assert_eq!(data["packages"].as_array().unwrap().len(), 2, "{data:#}");
                    assert_eq!(data["packages"][0]["package"], "ws-app");
                    assert_eq!(data["packages"][1]["package"], "ws-core");
                } else {
                    assert_eq!(data["package"], "ws-core");
                }
            }
        }
    }
    let run = tmt(
        &workspace,
        &["--quiet", "--workspace", "--test-workspace", "--json"],
    );
    assert_eq!(run.code, 2, "{}", run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    for entry in report["packages"].as_array().unwrap() {
        let tests = strings(&entry["analysis"]["tests_of"]);
        assert!(tests.contains(&"ws-app") && tests.contains(&"ws-core"));
    }
}

#[test]
fn workspace_failure_keeps_other_reports_and_has_a_distinct_exit_status() {
    let dir = fixture("multi", "workspace-failed-member").join("ws");
    append(
        &dir.join("app/src/lib.rs"),
        "\ncompile_error!(\"broken member\");\n",
    );
    for format in ["--json", "--sarif"] {
        let run = tmt(&dir, &["--workspace", "--quiet", format]);
        assert_eq!(run.code, 1, "{}", run.stderr);
        assert!(run.stderr.contains("broken member"), "{}", run.stderr);
        let value: Value = serde_json::from_str(&run.stdout).unwrap();
        if format == "--json" {
            assert_eq!(value["errors"][0]["package"], "ws-app");
            assert_eq!(value["packages"][0]["package"], "ws-core");
        } else {
            // The member that could not be analysed is a run too, and says that it failed:
            // a document without it would read as a package with nothing to report.
            let runs = value["runs"].as_array().unwrap();
            assert_eq!(runs.len(), 2);
            assert!(!runs[0]["results"].as_array().unwrap().is_empty());
            assert_eq!(runs[0]["automationDetails"]["id"], "tmt/ws-core/");
            assert_eq!(runs[0]["invocations"][0]["executionSuccessful"], true);
            assert_eq!(runs[1]["automationDetails"]["id"], "tmt/ws-app/");
            assert_eq!(runs[1]["invocations"][0]["executionSuccessful"], false);
            let said =
                runs[1]["invocations"][0]["toolExecutionNotifications"][0]["message"]["text"]
                    .as_str()
                    .unwrap();
            assert!(said.contains("broken member"), "{said}");
            assert!(runs[1]["results"].as_array().unwrap().is_empty());
        }
    }
}

#[test]
fn source_only_can_use_the_literal_operator_edit_used_by_cargo_mutants() {
    let dir = fixture("eligibility", "literal-operator");
    let original = "pub fn answer() -> u32 { 8 - 2 * 3 }\n\
                    #[test] fn checks_answer() { assert_ne!(answer(), 9); }\n";
    fs::write(dir.join("src/lib.rs"), original).unwrap();
    let mut kills = vec![];
    for literal in [false, true] {
        let mut args = vec!["--source-only", "--quiet", "--json"];
        args.extend(literal.then_some("--literal-operators"));
        args.push("--list");
        let run = tmt(&dir, &args);
        assert_eq!(run.code, 0, "{}", run.stderr);
        let list: Value = serde_json::from_str(&run.stdout).unwrap();
        let edit = &list["mutants"]
            .as_array()
            .unwrap()
            .iter()
            .find(|candidate| candidate["what"] == "*" && candidate["into"] == "+")
            .unwrap()["edits"][0];
        let start = edit["start"].as_u64().unwrap() as usize;
        let end = edit["end"].as_u64().unwrap() as usize;
        let changed = format!(
            "{}{}{}",
            &original[..start],
            edit["replacement"].as_str().unwrap(),
            &original[end..]
        );
        let expected = if literal { "8 - 2 + 3" } else { "8 - (2 + 3)" };
        assert!(changed.contains(expected), "{changed}");
        args.pop();
        let run = tmt(&dir, &args);
        assert_eq!(run.code, 2, "{}", run.stderr);
        let report: Value = serde_json::from_str(&run.stdout).unwrap();
        kills.push(report["mutants_killed"].as_u64().unwrap());
    }
    assert_eq!(kills, [0, 1]);
}

#[cfg(unix)]
#[test]
fn source_only_nextest_uses_the_chosen_profile() {
    let installed = Command::new("cargo")
        .args(["nextest", "--version"])
        .output()
        .is_ok_and(|output| output.status.success());
    if !installed {
        eprintln!("nextest executable is unavailable");
        return;
    }
    let dir = fixture("eligibility", "nextest-profile");
    fs::write(
        dir.join("src/lib.rs"),
        r#"pub fn answer() -> u32 { const { 6 * 7 } }
#[test] fn good() { assert_eq!(answer(), 42); }
#[test] fn excluded() { panic!("must not be run by the selected profile"); }
"#,
    )
    .unwrap();
    fs::create_dir(dir.join(".config")).unwrap();
    fs::write(
        dir.join(".config/nextest.toml"),
        "[profile.tmt]\ndefault-filter = 'test(=good)'\n\
         [profile.empty]\ndefault-filter = 'test(=missing)'\n",
    )
    .unwrap();
    let common = [
        "--source-only",
        "--test-tool",
        "nextest",
        "--quiet",
        "--json",
    ];
    let run = tmt(
        &dir,
        &[common.as_slice(), &["--nextest-profile", "tmt"]].concat(),
    );
    assert_eq!(run.code, 0, "{}", run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(report["analysis"]["nextest"]["profile"], "tmt");
    assert_eq!(report["mutants_run"], 4);
    assert_eq!(report["mutants_killed"], 4);
    let run = tmt(
        &dir,
        &[common.as_slice(), &["--nextest-profile", "empty"]].concat(),
    );
    assert_eq!(run.code, 1, "{}", run.stderr);
    assert!(run.stderr.contains("nextest"), "{}", run.stderr);
    assert!(run.stdout.is_empty());
}

#[test]
fn source_shards_partition_the_catalog_and_the_executed_mutants() {
    let dir = fixture("eligibility", "source-shards");
    fs::write(
        dir.join("src/lib.rs"),
        "pub fn answer() -> u32 { const { 6 * 7 } }\n\
         #[test] fn right_answer() { assert_eq!(answer(), 42); }\n",
    )
    .unwrap();
    let base = ["--source-only", "--json", "--quiet"];
    let catalog = tmt(&dir, &[base.as_slice(), &["--list"]].concat());
    let catalog: Value = serde_json::from_str(&catalog.stdout).unwrap();
    let expected: Vec<_> = catalog["mutants"]
        .as_array()
        .unwrap()
        .iter()
        .map(Value::to_string)
        .collect();
    assert_eq!(expected.len(), 4);
    // Full strength-three table from shard.pict.
    for method in ["slice", "round-robin"] {
        for total in [2, 5] {
            let mut seen = vec![];
            for index in 0..total {
                let shard = format!("{index}/{total}");
                let args = [base.as_slice(), &["--shard", &shard, "--sharding", method]].concat();
                let list = tmt(&dir, &[args.as_slice(), &["--list"]].concat());
                assert_eq!(list.code, 0, "{}", list.stderr);
                let entries: Value = serde_json::from_str(&list.stdout).unwrap();
                let own = entries["mutants"].as_array().unwrap();
                let chunk = expected.len().div_ceil(total);
                let actual: Vec<_> = own.iter().map(Value::to_string).collect();
                let assigned: Vec<_> = expected
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| {
                        if method == "slice" {
                            i / chunk == index
                        } else {
                            i % total == index
                        }
                    })
                    .map(|(_, entry)| entry.clone())
                    .collect();
                assert_eq!(actual, assigned, "{method} {shard}");
                seen.extend(own.iter().map(Value::to_string));
                let run = tmt(&dir, &args);
                assert_eq!(run.code, 0, "{}", run.stderr);
                let report: Value = serde_json::from_str(&run.stdout).unwrap();
                assert_eq!(report["mutants_run"], own.len(), "{shard}: {report:#}");
                assert_eq!(report["mutants_killed"], own.len(), "{shard}: {report:#}");
            }
            seen.sort();
            let mut all = expected.clone();
            all.sort();
            assert_eq!(seen, all, "{method} {total}");
        }
    }
    let invalid = tmt(&dir, &["--source-only", "--shard", "2/2"]);
    assert_ne!(invalid.code, 0);
    assert!(invalid.stderr.contains("shard index"), "{}", invalid.stderr);
}

#[test]
fn workspace_shards_partition_one_combined_catalogue() {
    let dir = fixture("multi", "workspace-shards").join("ws");
    let candidates = |extra: &[&str]| {
        let args = [
            &[
                "--workspace",
                "--source-only",
                "--list",
                "--json",
                "--quiet",
            ][..],
            extra,
        ]
        .concat();
        let run = tmt(&dir, &args);
        assert_eq!(run.code, 0, "{}", run.stderr);
        let report: Value = serde_json::from_str(&run.stdout).unwrap();
        report["packages"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|package| {
                package["mutants"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|m| (package["package"].to_string(), m.to_string()))
            })
            .collect::<Vec<_>>()
    };
    let all = candidates(&[]);
    assert!(all.iter().any(|(package, _)| package.contains("ws-app")));
    assert!(all.iter().any(|(package, _)| package.contains("ws-core")));
    let chunk = all.len().div_ceil(2);
    assert_eq!(candidates(&["--shard", "0/2"]), all[..chunk]);
    assert_eq!(candidates(&["--shard", "1/2"]), all[chunk..]);
}

#[test]
fn source_catalog_shows_actual_const_edits_without_building() {
    let dir = fixture("eligibility", "source-catalog-const");
    let path = dir.join("src/lib.rs");
    let clean = r#"pub fn answer() -> u32 {
    const { 6 * 7 }
}

#[test]
fn checks_answer() { assert_eq!(answer(), 42); }
"#;
    fs::write(
        &path,
        format!("{clean}compile_error!(\"list must not build\");\n"),
    )
    .unwrap();
    let run = tmt(&dir, &["--list", "--json", "--quiet"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let data: Value = serde_json::from_str(&run.stdout).unwrap();
    let actual: Vec<_> = data["mutants"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|candidate| candidate["what"] == "*")
        .map(|candidate| (candidate["into"].as_str().unwrap(), &candidate["edits"][0]))
        .collect();
    assert_eq!(actual.len(), 2, "{data:#}");
    for (operator, edit) in actual {
        assert!(["+", "/"].contains(&operator));
        assert_eq!(edit["file"], "src/lib.rs");
        assert_eq!(edit["line"], 2);
        let start = edit["start"].as_u64().unwrap() as usize;
        let end = edit["end"].as_u64().unwrap() as usize;
        assert!(clean[start..end].contains('*'));
    }

    fs::write(&path, clean).unwrap();
    let run = tmt(&dir, &["--source-only", "--json", "--quiet"]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(report["mutants_run"], 4, "{report:#}");
    assert_eq!(report["mutants_killed"], 4, "{report:#}");
}

#[test]
fn boolean_and_arithmetic_operators_and_macro_arguments() {
    let dir = fixture("operators", "operators");
    let (code, report) = json(&dir);
    assert_eq!(code, 2, "{report:#}");
    assert_eq!(
        report["skipped_sites"], 0,
        "every probe compiles: {report:#}"
    );
    assert_eq!(
        report["unstable"].as_array().unwrap().len(),
        0,
        "{report:#}"
    );
    let all = report["findings"].as_array().unwrap();
    let gap = |code: &str| {
        all.iter()
            .find(|f| f["code"] == code && f["kind"] == "input_gap")
            .unwrap_or_else(|| panic!("no input gap at `{code}`: {report:#}"))
    };

    // The right operand of `||` never decides.
    let or = gap("is_owner || is_admin");
    assert_eq!(
        strings(&or["survivors"]),
        ["`is_owner || { is_admin; false }`"]
    );
    assert_eq!(
        strings(&or["add_tests"]),
        ["a case where `is_owner` is false and `is_admin` is true"]
    );
    // `+` and `-` agree when the right operand is always 0; `*` and `/` when it is always 1.
    assert_eq!(
        strings(&gap("amount + fee")["survivors"]),
        ["`amount - fee`"]
    );
    assert_eq!(
        strings(&gap("amount + fee")["add_tests"]),
        ["`fee` other than 0"]
    );
    assert_eq!(
        strings(&gap("value * factor")["survivors"]),
        ["`value / factor`"]
    );

    // A compound assignment whose right side is always 0, and a function that only ever
    // returned false.
    assert_eq!(
        strings(&gap("*position += step")["survivors"]),
        ["`*position -= step`"]
    );
    let ret = gap("fn must_wait(ready: bool) -> bool");
    assert_eq!(
        strings(&ret["survivors"]),
        ["`must_wait` returning `false`"]
    );
    assert_eq!(
        strings(&ret["add_tests"]),
        ["a case where `must_wait` returns true"]
    );

    let codes: Vec<&str> = all.iter().map(|f| f["code"].as_str().unwrap()).collect();
    // Killed: the bit test (`|` instead of `&`) and the negation (`ready` instead of `!ready`).
    for quiet in [
        "bits & flag",
        "bits & flag != 0",
        "!ready",
        "fn has_flag(bits: u32, flag: u32) -> bool",
    ] {
        assert!(
            !codes.contains(&quiet),
            "`{quiet}` should have no finding: {report:#}"
        );
    }
    // Killed or fully pinned: the range check, the area, and the code inside `vec!` / `format!`.
    for quiet in [
        "lo <= x && x <= hi",
        "lo <= x",
        "x <= hi",
        "width * height",
        "n > 0",
        "n % 2 == 0",
        "n % 2",
        "index + 1",
    ] {
        assert!(
            !codes.contains(&quiet),
            "`{quiet}` should have no finding: {report:#}"
        );
    }
    // Not numbers: neither a finding nor "unreached".
    for opaque in ["name.to_owned() + \"!\"", "a + b"] {
        assert!(!codes.contains(&opaque), "{report:#}");
    }
    assert_eq!(
        report["unreached"].as_array().unwrap().len(),
        0,
        "{report:#}"
    );

    // Take the advice for the three input gaps above.
    append(
        &dir.join("src/lib.rs"),
        r#"
#[test]
fn closes_the_gaps() {
    assert!(can_edit(false, true));
    assert_eq!(total_with_fee(100, 5), 105);
        assert_eq!(scale(2.5, 2.0), 5.0);
    let mut position = 7;
    advance(&mut position, 3);
    assert_eq!(position, 10);
    assert!(must_wait(false));
}
"#,
    );
    let (_, report) = json(&dir);
    let codes: Vec<&str> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["code"].as_str().unwrap())
        .collect();
    for closed in [
        "is_owner || is_admin",
        "amount + fee",
        "value * factor",
        "*position += step",
        "fn must_wait(ready: bool) -> bool",
    ] {
        assert!(
            !codes.contains(&closed),
            "`{closed}` should be closed: {report:#}"
        );
    }
}

#[test]
fn doc_tests_are_used_on_request() {
    let dir = fixture("doc-tested", "doc-tested");
    // Without --doc nothing executes the code.
    let (code, report) = json(&dir);
    assert_eq!(code, 3, "{report:#}");
    assert_eq!(
        report["unreached"].as_array().unwrap().len(),
        5,
        "{report:#}"
    );
    assert_eq!(report["mutants_run"], 0, "{report:#}");

    let run = tmt(&dir, &["--json", "--quiet", "--doc"]);
    assert_eq!(run.code, 2, "{}{}", run.stdout, run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(
        report["unreached"].as_array().unwrap().len(),
        0,
        "{report:#}"
    );
    // `double(2) == 4` kills `x / 2` but not `x + 2`; `is_even` is only ever called with
    // an even number.
    let gaps = findings(&report, "input_gap");
    // The function is that one expression, so "it always returns true" is not repeated.
    let gap_codes: Vec<&str> = gaps.iter().map(|g| g["code"].as_str().unwrap()).collect();
    assert_eq!(gap_codes, ["x * 2", "x % 2 == 0"], "{report:#}");
    assert_eq!(strings(&gaps[0]["survivors"]), ["`x + 2`"]);
    assert_eq!(strings(&gaps[1]["survivors"]), ["`true`"]);
    assert_eq!(findings(&report, "assertion_gap").len(), 0, "{report:#}");
    assert!(
        report["mutants_killed"].as_u64().unwrap() >= 2,
        "{report:#}"
    );
}

#[test]
fn files_ignored_by_git_stay_out_of_the_working_copy() {
    let dir = fixture("eligibility", "git-ignored");
    let git = |args: &[&str]| {
        let status = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(args)
            .output()
            .unwrap()
            .status;
        assert!(status.success(), "git {args:?}");
    };
    git(&["init", "-q"]);
    fs::write(dir.join(".gitignore"), "target/\nsecrets/\n*.local\n").unwrap();
    fs::create_dir_all(dir.join("secrets")).unwrap();
    fs::write(dir.join("secrets/api-key.txt"), "not for copying").unwrap();
    fs::write(dir.join("notes.local"), "not for copying").unwrap();
    fs::write(dir.join("untracked.txt"), "new file, not ignored").unwrap();

    let copy = dir.join("target/tmt/src");
    let (code, report) = json(&dir);
    assert_eq!(code, 2, "{report:#}");
    assert!(copy.join("src/lib.rs").exists());
    assert!(
        copy.join("untracked.txt").exists(),
        "untracked but not ignored files are part of the build"
    );
    assert!(!copy.join("secrets").exists());
    assert!(!copy.join("notes.local").exists());
    assert!(!copy.join(".git").exists());

    // On request everything is copied (except .git and build output).
    let run = tmt(&dir, &["--json", "--quiet", "--all-files"]);
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(copy.join("secrets/api-key.txt").exists());
    assert!(copy.join("notes.local").exists());
    assert!(!copy.join(".git").exists());
    assert!(!copy.join("target").exists());
}

#[test]
fn cargo_intermediates_stay_inside_the_work_directory() {
    let dir = fixture("doc-tested", "cargo-build-dir");
    let work = dir.join("mutation");
    let ordinary = dir.join("ordinary-build");
    let output = fixture_command(env!("CARGO_BIN_EXE_cargo-tmt"))
        .arg(&dir)
        .args(["--json", "--quiet", "--doc", "--rebuild", "--work-dir"])
        .arg(&work)
        .env("CARGO_BUILD_BUILD_DIR", &ordinary)
        .output()
        .unwrap();
    assert!(
        matches!(output.status.code(), Some(0 | 2)),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(work.join("target/debug").is_dir());
    assert!(
        !ordinary.exists(),
        "mutation build wrote {}",
        ordinary.display()
    );
}

#[test]
fn observations_on_a_named_thread_are_not_mistaken_for_a_test() {
    let dir = fixture("eligibility", "named-thread");
    // The boundary value is only passed on a thread the test names itself. `--exact worker`
    // would select no test at all and "pass"; the whole binary has to run instead.
    append(
        &dir.join("src/lib.rs"),
        r#"
#[test]
fn boundary_on_a_named_thread() {
    let handle = std::thread::Builder::new()
        .name("worker".into())
        .spawn(|| is_eligible(18))
        .unwrap();
    assert!(handle.join().unwrap());
}
"#,
    );
    let (code, report) = json(&dir);
    assert_eq!(code, 0, "{report:#}");
    assert_eq!(
        report["findings"].as_array().unwrap().len(),
        0,
        "{report:#}"
    );
    assert_eq!(
        report["mutants_run"], report["mutants_killed"],
        "{report:#}"
    );
}

#[test]
fn work_dir_must_be_safe_to_delete() {
    let dir = fixture("eligibility", "work-dir-safety");
    let source = fs::read_to_string(dir.join("src/lib.rs")).unwrap();

    // The package itself, or a directory above it: its `src` is the user's code.
    for work in [dir.clone(), dir.parent().unwrap().to_path_buf()] {
        let run = tmt(&dir, &["--quiet", "--work-dir", work.to_str().unwrap()]);
        assert_eq!(run.code, 1, "{}", run.stdout);
        assert!(
            run.stderr.contains("contains the workspace"),
            "{}",
            run.stderr
        );
        assert_eq!(fs::read_to_string(dir.join("src/lib.rs")).unwrap(), source);
    }

    // Someone else's directory that happens to have a `src`.
    let other = Path::new(env!("CARGO_TARGET_TMPDIR")).join("work-dir-safety-other");
    let _ = fs::remove_dir_all(&other);
    fs::create_dir_all(other.join("src")).unwrap();
    fs::write(other.join("src/precious.txt"), "keep me").unwrap();
    let run = tmt(&dir, &["--quiet", "--work-dir", other.to_str().unwrap()]);
    assert_eq!(run.code, 1, "{}", run.stdout);
    assert!(run.stderr.contains("refusing to delete"), "{}", run.stderr);
    assert_eq!(
        fs::read_to_string(other.join("src/precious.txt")).unwrap(),
        "keep me"
    );

    // A directory of its own works, and can be reused.
    let own = Path::new(env!("CARGO_TARGET_TMPDIR")).join("work-dir-safety-own");
    let _ = fs::remove_dir_all(&own);
    for _ in 0..2 {
        let run = tmt(&dir, &["--quiet", "--work-dir", own.to_str().unwrap()]);
        assert_eq!(run.code, 2, "{}", run.stderr);
    }
}

#[cfg(unix)]
#[test]
fn symbolic_links_are_never_written_through() {
    let dir = fixture("eligibility", "symlinks");
    let outside = Path::new(env!("CARGO_TARGET_TMPDIR")).join("symlinks-outside");
    let _ = fs::remove_dir_all(&outside);
    fs::create_dir_all(outside.join("dir")).unwrap();
    let shared = "pub fn limit(x: u32) -> bool {\n    x > 3\n}\n";
    fs::write(outside.join("shared.rs"), shared).unwrap();
    fs::write(
        outside.join("dir/inner.rs"),
        "pub fn inner(x: u32) -> bool { x > 4 }\n",
    )
    .unwrap();

    // A source file and a directory that are links to places outside the package, a link
    // to a directory inside it, and a link that points back at its own parent.
    use std::os::unix::fs::symlink;
    symlink(outside.join("shared.rs"), dir.join("src/shared.rs")).unwrap();
    symlink(outside.join("dir"), dir.join("src/outside_dir")).unwrap();
    fs::create_dir_all(dir.join("data")).unwrap();
    fs::write(dir.join("data/note.txt"), "data").unwrap();
    symlink(dir.join("data"), dir.join("data_link")).unwrap();
    symlink(&dir, dir.join("data/loop")).unwrap();
    append(
        &dir.join("src/lib.rs"),
        "\nmod shared;\n#[test]\nfn uses_shared() {\n    assert!(shared::limit(9));\n    assert!(!shared::limit(1));\n}\n",
    );

    let (code, report) = json(&dir);
    assert_eq!(code, 2, "{report:#}");
    // The linked file is analysed as part of the crate, in the copy.
    let codes: Vec<&str> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["code"].as_str().unwrap())
        .collect();
    assert!(codes.contains(&"x > 3"), "{report:#}");
    // Nothing outside the copy changed.
    assert_eq!(
        fs::read_to_string(outside.join("shared.rs")).unwrap(),
        shared
    );
    assert_eq!(
        fs::read_to_string(outside.join("dir/inner.rs")).unwrap(),
        "pub fn inner(x: u32) -> bool { x > 4 }\n"
    );
    // The copy holds plain files, no links.
    let copy = dir.join("target/tmt/src");
    assert!(
        fs::symlink_metadata(copy.join("src/shared.rs"))
            .unwrap()
            .file_type()
            .is_file()
    );
    assert!(
        !copy.join("src/outside_dir").exists(),
        "links leaving the workspace are not followed"
    );
    assert!(
        copy.join("data_link/note.txt").exists(),
        "links inside the workspace are copied"
    );
    assert!(
        !copy.join("data/loop").exists(),
        "a link to its own ancestor is not followed"
    );
}

#[test]
fn probes_do_not_change_what_the_code_does() {
    // The fixture's tests check how often user code runs, in which order, and how long
    // temporaries live. They are part of the baseline run on the instrumented crate, so a
    // probe that changed any of it would stop the analysis.
    let dir = fixture("semantics", "semantics");
    let run = tmt(&dir, &["--json", "--quiet"]);
    assert_ne!(run.code, 1, "{}", run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(report["skipped_sites"], 0, "{report:#}");
    assert_eq!(
        report["unstable"].as_array().unwrap().len(),
        0,
        "{report:#}"
    );
    let codes: Vec<&str> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["code"].as_str().unwrap())
        .collect();
    // The argument of the local `vec!` and the indexed `+=` are not probed.
    assert!(!codes.iter().any(|c| c.contains("a < 3")), "{report:#}");
    assert!(!codes.iter().any(|c| c.contains("slots[")), "{report:#}");
    // A `first` / `last` that is not the one of a slice is not taken for it: with one
    // element the two would be the same, and these are not.
    assert!(
        !codes
            .iter()
            .any(|c| *c == "values.first()" || *c == "values.last()"),
        "{report:#}"
    );
}

fn git_output(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

#[test]
fn only_changed_code_is_analysed_on_request() {
    let dir = fixture("kitchen-sink", "since");
    fs::write(dir.join(".gitignore"), "target/\nCargo.lock\n").unwrap();
    git_output(&dir, &["init", "-q"]);
    git_output(&dir, &["add", "-A"]);
    // A tree is a revision too: the state of the files as staged just now.
    let base = git_output(&dir, &["write-tree"]);

    // Nothing changed: nothing to analyse.
    let run = tmt(&dir, &["--json", "--quiet", "--since", &base]);
    assert_eq!(run.code, 0, "{}", run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(report["sites"], 0, "{report:#}");
    assert_eq!(report["mutants_run"], 0, "{report:#}");

    // Change one function and add an untracked file.
    let source = fs::read_to_string(dir.join("src/lib.rs")).unwrap();
    fs::write(
        dir.join("src/lib.rs"),
        source.replace("celsius > 30.5", "celsius > 31.5"),
    )
    .unwrap();
    fs::write(
        dir.join("src/extra.rs"),
        "pub fn tiny(x: u8) -> bool {\n    x > 200\n}\n",
    )
    .unwrap();
    append(&dir.join("src/lib.rs"), "\npub mod extra;\n");

    let run = tmt(&dir, &["--json", "--quiet", "--since", &base]);
    assert_eq!(run.code, 2, "{}", run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    let codes: Vec<&str> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["code"].as_str().unwrap())
        .collect();
    assert_eq!(codes, ["celsius > 31.5"], "{report:#}");
    // The changed comparison, its function (its result and the two bodies it can be
    // replaced by), and the same four of the new file.
    assert_eq!(report["sites"], 8, "{report:#}");
    let unreached: Vec<&str> = report["unreached"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["file"].as_str().unwrap())
        .collect();
    assert_eq!(unreached, ["src/extra.rs", "src/extra.rs"]);

    for setting in [["diff.noprefix", "true"], ["diff.dstPrefix", "custom/"]] {
        git_output(&dir, &["config", setting[0], setting[1]]);
        let actual = tmt(&dir, &["--json", "--quiet", "--since", &base]);
        let actual: Value = serde_json::from_str(&actual.stdout).unwrap();
        assert_eq!(actual, report, "{setting:?}");
        git_output(&dir, &["config", "--unset", setting[0]]);
    }

    // The same selection by hand.
    let run = tmt(
        &dir,
        &[
            "--json",
            "--quiet",
            "--lines",
            "src/lib.rs:24-26",
            "--lines",
            "src/extra.rs",
        ],
    );
    let by_lines: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(by_lines["analysis"]["lines"][1], "src/extra.rs");
    assert_eq!(
        report["analysis"]["since"].as_str().map(str::len),
        Some(base.len())
    );
    // The lines alone do not say what was there before: with a revision, the changes
    // undone are mutants that a build of their own would run (the changed comparison,
    // and the declaration of the new module).
    let mut since = but_how(report);
    assert_eq!(
        since["mutants_not_built"].as_u64().unwrap(),
        by_lines["mutants_not_built"].as_u64().unwrap() + 2
    );
    assert_eq!(since["evaluation"], "partial");
    since.as_object_mut().unwrap().remove("evaluation");
    since["mutants_not_built"] = by_lines["mutants_not_built"].clone();
    assert_eq!(but_how(by_lines), since);
    let bad = tmt(&dir, &["--quiet", "--lines", "src/lib.rs:9-2"]);
    assert_eq!(bad.code, 1);
    assert!(bad.stderr.contains("not a line range"), "{}", bad.stderr);
    let unknown = tmt(&dir, &["--quiet", "--since", "no-such-revision"]);
    assert_eq!(unknown.code, 1);
}

#[test]
fn changed_files_are_found_whatever_their_names() {
    let dir = fixture("eligibility", "since-names");
    fs::write(dir.join(".gitignore"), "target/\nCargo.lock\n").unwrap();
    let module = |name: &str| {
        format!(
            "pub fn {name}(n: u32) -> bool {{\n    n >= 100\n}}\n\n\
             #[test]\nfn far_from_the_limit() {{\n    assert!({name}(500));\n    assert!(!{name}(5));\n}}\n"
        )
    };
    // A name git quotes (bytes that are not ASCII), one it follows with a tab (a space).
    fs::write(dir.join("src/価格.rs"), module("priced")).unwrap();
    fs::write(dir.join("src/two words.rs"), module("worded")).unwrap();
    // Both: quoted, and the tab after the closing quote.
    fs::write(dir.join("src/価格 表.rs"), module("tabled")).unwrap();
    append(
        &dir.join("src/lib.rs"),
        "\n#[path = \"価格.rs\"]\npub mod price;\n#[path = \"two words.rs\"]\npub mod words;\n\
         #[path = \"価格 表.rs\"]\npub mod table;\n",
    );
    git_output(&dir, &["init", "-q"]);
    git_output(&dir, &["add", "-A"]);
    let base = git_output(&dir, &["write-tree"]);
    for file in ["src/価格.rs", "src/two words.rs", "src/価格 表.rs"] {
        let path = dir.join(file);
        let source = fs::read_to_string(&path).unwrap();
        fs::write(&path, source.replace("n >= 100", "n >= 200")).unwrap();
    }
    for quote_path in ["true", "false"] {
        git_output(&dir, &["config", "core.quotePath", quote_path]);
        let run = tmt(&dir, &["--json", "--quiet", "--since", &base]);
        assert_eq!(run.code, 2, "{}{}", run.stdout, run.stderr);
        let report: Value = serde_json::from_str(&run.stdout).unwrap();
        let mut files: Vec<&str> = report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["code"] == "n >= 200")
            .map(|f| f["file"].as_str().unwrap())
            .collect();
        files.sort_unstable();
        assert_eq!(
            files,
            ["src/two words.rs", "src/価格 表.rs", "src/価格.rs"],
            "{report:#}"
        );
    }
    // Not yet known to git: the whole file.
    fs::write(dir.join("src/新 しい.rs"), module("fresh")).unwrap();
    append(
        &dir.join("src/lib.rs"),
        "\n#[path = \"新 しい.rs\"]\npub mod fresh;\n",
    );
    let run = tmt(&dir, &["--json", "--quiet", "--since", &base]);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["file"] == "src/新 しい.rs" && f["code"] == "n >= 100"),
        "{report:#}"
    );
}

#[test]
fn crates_of_earlier_editions_are_analysed() {
    for edition in ["2015", "2018"] {
        let name = format!("edition-{edition}");
        let dir = fixture(&name, &name);
        for mode in [&[][..], &["--source-only"][..]] {
            let mut args = vec!["--json", "--quiet"];
            args.extend(mode);
            let run = tmt(&dir, &args);
            assert_eq!(
                run.code, 2,
                "{edition} {mode:?}: {}{}",
                run.stdout, run.stderr
            );
            let report: Value = serde_json::from_str(&run.stdout).unwrap();
            let codes: Vec<&str> = report["findings"]
                .as_array()
                .unwrap()
                .iter()
                .map(|f| f["code"].as_str().unwrap())
                .collect();
            // The shift is among what was switched while running: its checked form is
            // what did not build before the 2021 edition.
            assert!(codes.contains(&"n >= 18"), "{edition} {mode:?}: {report:#}");
            assert!(report["mutants_run"].as_u64().unwrap() > 5, "{report:#}");
            assert!(report["skipped"].is_null(), "{report:#}");
        }
    }
}

#[test]
fn a_change_the_tests_do_not_tell_from_the_old_code_is_found() {
    let dir = fixture("reverting", "reverting");
    fs::write(dir.join(".gitignore"), "target/\nCargo.lock\n").unwrap();
    git_output(&dir, &["init", "-q"]);
    git_output(&dir, &["add", "-A"]);
    let base = git_output(&dir, &["write-tree"]);
    let edit = |file: &str, from: &str, to: &str| {
        let path = dir.join(file);
        let source = fs::read_to_string(&path).unwrap();
        assert!(source.contains(from), "{file}: {from}");
        fs::write(&path, source.replace(from, to)).unwrap();
    };
    // The age of a session is now counted from its last use: no test has a session that
    // was used after it was made, so the old code passes them all.
    edit(
        "src/lib.rs",
        "    let _ = used;\n    now - created < 30",
        "    now - used < 30 && created <= used",
    );
    // Not the program: the documentation, and a test.
    edit(
        "src/lib.rs",
        "/// Whether a session is still good.",
        "/// Says if it is.",
    );
    edit(
        "src/lib.rs",
        "assert!(valid(0, 0, 10));",
        "assert!(valid(0, 0, 11));",
    );
    // A limit that moved, with a test that says so.
    edit("src/limits.rs", "n.min(10)", "n.min(20)");
    edit(
        "src/limits.rs",
        "        assert_eq!(cap(3), 3);\n",
        "        assert_eq!(cap(3), 3);\n        assert_eq!(cap(15), 15);\n",
    );
    // Two changes that belong together: the function that is new does not go alone.
    edit(
        "src/steps.rs",
        "pub fn label(n: u32) -> u32 {\n    n\n}",
        "fn zero() -> u32 {\n    0\n}\n\npub fn label(n: u32) -> u32 {\n    n + zero()\n}",
    );
    let changes = |report: &Value| -> Vec<(String, u64, String, String)> {
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| !f["change_since"].is_null())
            .map(|f| {
                assert_eq!(f["change_since"], base.as_str(), "{f:#}");
                assert_eq!(f["kind"], "survivor", "{f:#}");
                (
                    f["file"].as_str().unwrap().to_string(),
                    f["line"].as_u64().unwrap(),
                    f["code"].as_str().unwrap().to_string(),
                    f["survivors"][0].as_str().unwrap().to_string(),
                )
            })
            .collect()
    };

    // Built once: the changes are counted among what a build of their own would run.
    let run = tmt(&dir, &["--json", "--quiet", "--since", &base]);
    let quick: Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}{}", run.stdout, run.stderr));
    assert_eq!(changes(&quick), [], "{quick:#}");
    assert!(
        quick["mutants_not_built"].as_u64().unwrap() >= 4,
        "{quick:#}"
    );

    for mode in ["--rebuild", "--source-only"] {
        let run = tmt(&dir, &["--json", "--quiet", mode, "--since", &base]);
        assert_eq!(run.code, 2, "{mode}: {}{}", run.stdout, run.stderr);
        let report: Value = serde_json::from_str(&run.stdout).unwrap();
        let s = |text: &str| text.to_string();
        assert_eq!(
            changes(&report),
            [
                (
                    s("src/lib.rs"),
                    9,
                    s("now - used < 30 && created <= used"),
                    s("with the code as it was: `let _ = used; now - created < 30`")
                ),
                (
                    s("src/steps.rs"),
                    1,
                    s("fn zero() -> u32 {"),
                    s("with all 2 changes to this file undone")
                ),
                (
                    s("src/steps.rs"),
                    6,
                    s("n + zero()"),
                    s("with the code as it was: `n`")
                ),
            ],
            "{mode}: {report:#}"
        );
        // The function alone does not go (one that does not build); the limit is told.
        assert!(
            report["unviable_mutants"].as_u64().unwrap() >= 1,
            "{report:#}"
        );
        // The working copy is as it was: a second run says the same.
        let again = tmt(&dir, &["--json", "--quiet", mode, "--since", &base]);
        let again: Value = serde_json::from_str(&again.stdout).unwrap();
        assert_eq!(changes(&again), changes(&report), "{mode}");
    }

    let text = tmt(&dir, &["--quiet", "--rebuild", "--since", &base]);
    let short = &base[..12];
    assert!(
        text.stdout.contains(&format!(
            "UNTESTED CHANGE  src/lib.rs:9:1\n    now - used < 30 && created <= used\n  \
             passes every test with the code as it was: `let _ = used; now - created < 30`\n  \
             no test tells this change from the code as it was at {short}: add one that fails without it\n"
        )),
        "{}",
        text.stdout
    );

    // A limit and its test changed in lines next to each other are one hunk: the old
    // limit is tried with the test as it is now, which finds it out. And a constant
    // that changed where nothing else can be mutated is tried too.
    append(
        &dir.join("src/lib.rs"),
        "\npub const DEFAULT_LIMIT: u32 = 41;\n\npub fn limit() -> u32 { 41 }\n\
         #[test] fn the_limit() { assert_eq!(limit(), 41); assert!(DEFAULT_LIMIT > 0); }\n\n\
         fn helper() -> u32 { 1 }\n\npub fn uses() -> u32 { helper() }\n\n\
         #[test] fn used() { assert_eq!(uses(), 1); }\n",
    );
    git_output(&dir, &["add", "-A"]);
    let with_limit = git_output(&dir, &["write-tree"]);
    edit(
        "src/lib.rs",
        "pub fn limit() -> u32 { 41 }",
        "pub fn limit() -> u32 { 42 }",
    );
    edit(
        "src/lib.rs",
        "assert_eq!(limit(), 41);",
        "assert_eq!(limit(), 42);",
    );
    edit(
        "src/lib.rs",
        "DEFAULT_LIMIT: u32 = 41;",
        "DEFAULT_LIMIT: u32 = 42;",
    );
    // A helper under another name: its definition and its use do not build one without
    // the other, so all changes of the file are undone together. The tests stay as
    // they are there too, and find the old limit out.
    edit(
        "src/lib.rs",
        "fn helper() -> u32 { 1 }",
        "fn one() -> u32 { 1 }",
    );
    edit("src/lib.rs", "{ helper() }", "{ one() }");
    for mode in ["--rebuild", "--source-only"] {
        let run = tmt(&dir, &["--json", "--quiet", mode, "--since", &with_limit]);
        let report: Value = serde_json::from_str(&run.stdout)
            .unwrap_or_else(|e| panic!("{e}: {}{}", run.stdout, run.stderr));
        let undone: Vec<String> = report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| !f["change_since"].is_null())
            .map(|f| f["code"].as_str().unwrap().to_string())
            .collect();
        // The limit is tested (41 is not 42); the constant is only said to be positive.
        assert_eq!(
            undone,
            ["pub const DEFAULT_LIMIT: u32 = 42;"],
            "{mode}: {report:#}"
        );
        assert_eq!(run.code, 2, "{mode}: {report:#}");
    }
    // Only the constant changed, where no site is: its old value is tried all the same.
    edit(
        "src/lib.rs",
        "pub fn limit() -> u32 { 42 }",
        "pub fn limit() -> u32 { 41 }",
    );
    edit(
        "src/lib.rs",
        "assert_eq!(limit(), 42);",
        "assert_eq!(limit(), 41);",
    );
    edit(
        "src/lib.rs",
        "fn one() -> u32 { 1 }",
        "fn helper() -> u32 { 1 }",
    );
    edit("src/lib.rs", "{ one() }", "{ helper() }");
    let run = tmt(
        &dir,
        &["--json", "--quiet", "--rebuild", "--since", &with_limit],
    );
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(run.code, 2, "{report:#}");
    assert!(report["evaluation"].is_null(), "{report:#}");
    assert_eq!(
        report["findings"].as_array().unwrap().len(),
        1,
        "{report:#}"
    );
    assert_eq!(report["findings"][0]["change_since"], with_limit.as_str());
    edit(
        "src/lib.rs",
        "DEFAULT_LIMIT: u32 = 42;",
        "DEFAULT_LIMIT: u32 = 41;",
    );

    // With a session used after it was made, the change is told from the old code.
    edit(
        "src/lib.rs",
        "assert!(!valid(0, 0, 40));",
        "assert!(!valid(0, 0, 40));\n        assert!(valid(0, 90, 100));",
    );
    let run = tmt(&dir, &["--json", "--quiet", "--rebuild", "--since", &base]);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert!(
        changes(&report)
            .iter()
            .all(|(file, ..)| file == "src/steps.rs"),
        "{report:#}"
    );
}

#[test]
fn rounding_too_early_is_told_by_an_amount_that_is_not_round() {
    let dir = fixture("rounding", "rounding");
    // Switched while running, every mutant is found out: the tests look thorough.
    let (code, quick) = json(&dir);
    assert_eq!(code, 0, "{quick:#}");
    assert_eq!(quick["findings"], serde_json::json!([]), "{quick:#}");

    let run = tmt(&dir, &["--json", "--quiet", "--rebuild"]);
    assert_eq!(run.code, 2, "{}{}", run.stdout, run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    let findings = report["findings"].as_array().unwrap();
    assert_eq!(findings.len(), 1, "{report:#}");
    let finding = &findings[0];
    assert_eq!(finding["line"], 5, "{finding:#}");
    assert_eq!(
        finding["survivors"],
        serde_json::json!([
            "`u64::from(cents) / 10_000 * u64::from(bps)` (divide before multiplying)"
        ]),
    );
    // 80 is the least remainder that 125 basis points show: 80 * 125 = 10000.
    let witness = &finding["witness"];
    assert_eq!(witness["call"], "fee(10080, 125)", "{finding:#}");
    assert_eq!(witness["original"], "126", "{finding:#}");
    assert_eq!(witness["mutant"], "125", "{finding:#}");
    assert_eq!(witness["assertion"], "assert_eq!(fee(10080, 125), 126);");
    assert_eq!(
        witness["guidance"],
        "The tests pass with the division by 10000 done first; this `cents` is not a multiple of it."
    );

    // With the assertion it asks for, nothing is left.
    let path = dir.join("src/lib.rs");
    let source = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        source.replace(
            "assert_eq!(fee(20_000, 250), 500);",
            "assert_eq!(fee(20_000, 250), 500);\n        assert_eq!(fee(10080, 125), 126);",
        ),
    )
    .unwrap();
    let run = tmt(&dir, &["--json", "--quiet", "--rebuild"]);
    assert_eq!(run.code, 0, "{}{}", run.stdout, run.stderr);
}

#[test]
fn a_key_without_one_of_its_parts_passes_tests_that_never_mix_them() {
    let dir = fixture("keyed", "keyed");
    // Both languages with both pages, and every mutant switched while running is found
    // out: the tests look thorough.
    let (code, quick) = json(&dir);
    assert_eq!(code, 0, "{quick:#}");

    let run = tmt(&dir, &["--json", "--quiet", "--rebuild"]);
    assert_eq!(run.code, 2, "{}{}", run.stdout, run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    let findings = report["findings"].as_array().unwrap();
    // Without the page two texts of one store collide, which the tests see; without
    // the language nothing does, as no store has two languages.
    assert_eq!(findings.len(), 1, "{report:#}");
    assert_eq!(
        findings[0]["survivors"],
        serde_json::json!([r#"`format!("/{slug}")` (without `language`)"#]),
    );
    assert_eq!(findings[0]["line"], 7);
    // A value one test puts, read with the language of the other: what was run to
    // see the two keys differ, and the lines to add.
    let witness = &findings[0]["witness"];
    assert_eq!(
        witness["call"], "labels.get(\"en\", \"home\")",
        "{report:#}"
    );
    assert_eq!(witness["original"], "None", "{report:#}");
    assert_eq!(witness["mutant"], "Some(\"こんにちは\")", "{report:#}");
    assert_eq!(
        witness["assertion"],
        "let mut labels = Labels::default(); labels.put(\"ja\", \"home\", \"こんにちは\"); assert_eq!(labels.get(\"en\", \"home\"), None);"
    );
    assert_eq!(
        witness["guidance"],
        "The tests pass without `language` in the value; here \"ja\" and \"en\" meet in one place."
    );

    // One language put, the other asked for: the two lines that tell.
    let path = dir.join("src/lib.rs");
    let source = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        source.replace(
            "        assert_eq!(labels.get(\"ja\", \"home\"), Some(\"こんにちは\"));\n",
            "        assert_eq!(labels.get(\"ja\", \"home\"), Some(\"こんにちは\"));\n        \
             assert_eq!(labels.get(\"en\", \"home\"), None);\n",
        ),
    )
    .unwrap();
    let run = tmt(&dir, &["--json", "--quiet", "--rebuild"]);
    assert_eq!(run.code, 0, "{}{}", run.stdout, run.stderr);
}

#[test]
fn a_boundary_value_no_input_gives_is_not_asked_for() {
    let dir = fixture("stepped", "stepped");
    let (code, report) = json(&dir);
    assert_eq!(code, 2, "{report:#}");
    // 16 and 24 blocks of eight have nothing between them, and neither have the bits
    // 0100 and 1000 of the mask 1100; a last digit is under ten whatever the number
    // is. Only the comparison tested far from its limit has a gap, and the values to
    // add are ones a count of blocks gives.
    let findings: Vec<&Value> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["kind"] == "input_gap")
        .collect();
    // A type with a multiplication of its own is not a count of eights: its gap is
    // asked for as any other.
    assert_eq!(findings.len(), 2, "{report:#}");
    assert_eq!(findings[1]["code"], "rate * 8 > 20", "{report:#}");
    assert_eq!(
        findings[1]["add_tests"],
        serde_json::json!([
            "`rate * 8` = 20 (the original gives false)",
            "`rate * 8` = 21 (the original gives true)"
        ]),
    );
    let finding = findings[0];
    assert_eq!(finding["line"], 11, "{finding:#}");
    assert_eq!(
        finding["add_tests"],
        serde_json::json!([
            "`blocks * 8` = 16 (the original gives false)",
            "`blocks * 8` = 24 (the original gives true)"
        ]),
    );
    // The thresholds named are ones those values find out: `> 21` is `> 20` for
    // every multiple of eight.
    assert_eq!(
        finding["survivors"],
        serde_json::json!([
            "`blocks * 8 > 24`",
            "`blocks * 8 > 15`",
            "`blocks * 8 > 25`"
        ]),
    );
    assert_eq!(finding["survivor_count"], 24, "{finding:#}");
    assert_eq!(finding["verified"], true, "{finding:#}");

    // With the two values it asks for (2 and 3 blocks), nothing is left.
    let path = dir.join("src/lib.rs");
    let source = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        source.replace(
            "assert!(is_long(5));",
            "assert!(is_long(5));\n        assert!(!is_long(2));\n        assert!(is_long(3));\n        \
             assert!(!is_rated(Eighths(20)));\n        assert!(is_rated(Eighths(21)));",
        ),
    )
    .unwrap();
    let (_, report) = json(&dir);
    assert!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .all(|f| f["kind"] != "input_gap"),
        "{report:#}"
    );
}

#[test]
fn a_distinguishing_test_is_found_in_a_file_that_is_a_module() {
    // The same code as the fixtures have at the top of the library, one file down:
    // the test that shows the difference is put into that file, under its module.
    for (fixture_name, line, call, assertion) in [
        (
            "batch-store",
            33,
            "store.get(\"a\")",
            "assert_eq!(store.get(\"a\"), None);",
        ),
        (
            "rounding",
            5,
            "fee(10080, 125)",
            "assert_eq!(fee(10080, 125), 126);",
        ),
    ] {
        let dir = fixture(fixture_name, &format!("{fixture_name}-module"));
        fs::rename(dir.join("src/lib.rs"), dir.join("src/inner.rs")).unwrap();
        fs::write(dir.join("src/lib.rs"), "pub mod inner;\n").unwrap();
        let lines = format!("src/inner.rs:{line}");
        let run = tmt(&dir, &["--json", "--quiet", "--rebuild", "--lines", &lines]);
        assert_eq!(run.code, 2, "{fixture_name}: {}{}", run.stdout, run.stderr);
        let report: Value = serde_json::from_str(&run.stdout).unwrap();
        let witnesses: Vec<&Value> = report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| !f["witness"].is_null())
            .collect();
        assert_eq!(witnesses.len(), 1, "{fixture_name}: {report:#}");
        let finding = witnesses[0];
        assert_eq!(finding["file"], "src/inner.rs", "{finding:#}");
        assert_eq!(finding["line"], line, "{finding:#}");
        assert_eq!(finding["witness"]["call"], call, "{finding:#}");
        assert!(
            finding["witness"]["assertion"]
                .as_str()
                .unwrap()
                .ends_with(assertion),
            "{finding:#}"
        );
        // The working copy is left as it was: the same again.
        let again = tmt(&dir, &["--json", "--quiet", "--rebuild", "--lines", &lines]);
        let again: Value = serde_json::from_str(&again.stdout).unwrap();
        assert_eq!(again["findings"], report["findings"], "{fixture_name}");
    }
}

#[test]
fn what_changed_tests_no_longer_find_out_is_theirs() {
    let dir = fixture("weakening", "weakening");
    fs::write(dir.join(".gitignore"), "target/\nCargo.lock\n").unwrap();
    git_output(&dir, &["init", "-q"]);
    git_output(&dir, &["add", "-A"]);
    let base = git_output(&dir, &["write-tree"]);
    let path = dir.join("src/lib.rs");
    let original = fs::read_to_string(&path).unwrap();
    let edit = |from: &str, to: &str| {
        let source = fs::read_to_string(&path).unwrap();
        assert!(source.contains(from), "{from}");
        fs::write(&path, source.replace(from, to)).unwrap();
    };
    let run = || {
        let run = tmt(&dir, &["--json", "--quiet", "--since", &base]);
        let report: Value = serde_json::from_str(&run.stdout)
            .unwrap_or_else(|e| panic!("{e}: {}{}", run.stdout, run.stderr));
        let found: Vec<(u64, String)> = report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| {
                (
                    f["line"].as_u64().unwrap(),
                    f["code"].as_str().unwrap().to_string(),
                )
            })
            .collect();
        let weakened: Vec<u64> = report["weakened"]
            .as_array()
            .map(|all| all.iter().map(|at| at["line"].as_u64().unwrap()).collect())
            .unwrap_or_default();
        (run.code, found, weakened, report)
    };

    // An assertion that says less, and one that is gone: no line of the program
    // changed, and what the tests found out before is found in the whole file. The gap
    // of `is_senior` was there before and is not this change's.
    edit(
        "        assert_eq!(settle(10, true), Status::Cancelled);\n",
        "        assert!(settle(10, true).is_terminal());\n",
    );
    edit("        assert!(is_eligible(18));\n", "");
    let (code, found, weakened, report) = run();
    assert_eq!(code, 2, "{report:#}");
    assert_eq!(
        found,
        [
            (19, "refund || amount == 0".to_string()),
            (27, "age >= 18".to_string())
        ],
        "{report:#}"
    );
    assert_eq!(weakened, [19, 27], "{report:#}");
    assert!(
        report["unreached"].as_array().unwrap().is_empty(),
        "{report:#}"
    );
    let text = tmt(&dir, &["--quiet", "--since", &base]);
    let said = format!(
        "  the tests as they were at {} found this out: a change to the tests lets it through\n",
        &base[..12]
    );
    assert_eq!(text.stdout.matches(&said).count(), 2, "{}", text.stdout);

    // Tests that say the same in other words, or more, let nothing new through.
    fs::write(&path, &original).unwrap();
    edit(
        "        assert_eq!(settle(10, true), Status::Cancelled);\n",
        "        assert_eq!(Status::Cancelled, settle(10, true));\n        assert!(settle(10, true).is_terminal());\n",
    );
    let (code, found, weakened, report) = run();
    assert_eq!((code, found.len(), weakened.len()), (0, 0, 0), "{report:#}");

    // Tests that fail as they were (the program changed with them) say nothing of what
    // they found out: only the changed line is looked at.
    fs::write(&path, &original).unwrap();
    edit("age >= 18", "age >= 21");
    edit("assert!(is_eligible(18));", "assert!(!is_eligible(18));");
    edit("assert!(is_eligible(20));", "assert!(is_eligible(21));");
    let (_, found, weakened, report) = run();
    assert!(weakened.is_empty(), "{report:#}");
    assert!(found.iter().all(|(line, _)| *line == 27), "{report:#}");
}

#[test]
fn known_findings_can_be_suppressed() {
    let dir = fixture("eligibility", "suppress");
    let baseline = dir.join("tmt-baseline.json");
    let baseline_arg = baseline.to_str().unwrap();

    // Record what is there today.
    let run = tmt(
        &dir,
        &["--json", "--quiet", "--write-baseline", baseline_arg],
    );
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(fs::read_to_string(&baseline).unwrap().contains("age >= 18"));

    // With the baseline the known gap is neither reported nor a failure.
    let run = tmt(&dir, &["--json", "--quiet", "--baseline", baseline_arg]);
    assert_eq!(run.code, 0, "{}", run.stdout);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(report["findings"].as_array().unwrap().len(), 0);
    assert_eq!(report["suppressed"], 1);
    let text = tmt(&dir, &["--quiet", "--baseline", baseline_arg]);
    assert!(
        text.stdout.contains("1 findings suppressed"),
        "{}",
        text.stdout
    );

    // Code that moves keeps its entry; a new gap is still reported.
    let source = fs::read_to_string(dir.join("src/lib.rs")).unwrap();
    fs::write(
        dir.join("src/lib.rs"),
        format!("// moved down\n\n{source}\npub fn is_senior(age: u32) -> bool {{\n    age >= 65\n}}\n\n#[test]\nfn seniors() {{\n    assert!(is_senior(70));\n    assert!(!is_senior(30));\n}}\n"),
    )
    .unwrap();
    let run = tmt(&dir, &["--json", "--quiet", "--baseline", baseline_arg]);
    assert_eq!(run.code, 2, "{}", run.stdout);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    let codes: Vec<&str> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["code"].as_str().unwrap())
        .collect();
    assert_eq!(codes, ["age >= 65"], "{report:#}");
    assert_eq!(report["suppressed"], 1);

    // A comment on the line (or the line above) silences the rest.
    let source = fs::read_to_string(dir.join("src/lib.rs")).unwrap();
    fs::write(
        dir.join("src/lib.rs"),
        source.replace(
            "    age >= 65",
            "    // tmt:ignore the pension age is checked by the caller's tests\n    age >= 65",
        ),
    )
    .unwrap();
    let run = tmt(&dir, &["--json", "--quiet", "--baseline", baseline_arg]);
    assert_eq!(run.code, 0, "{}", run.stdout);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(report["suppressed"], 2);

    let missing = tmt(
        &dir,
        &["--quiet", "--baseline", "/nonexistent/baseline.json"],
    );
    assert_eq!(missing.code, 1);
}

#[test]
fn tests_of_dependent_packages_can_close_a_gap() {
    let root = fixture("multi", "dependents");
    let ws = root.join("ws");
    // On its own tests the core package leaves its boundary open.
    let alone = tmt(&ws, &["--json", "--quiet", "--package", "ws-core"]);
    assert_eq!(alone.code, 2, "{}", alone.stderr);

    // The dependent package tests the boundary.
    for flags in [&["--dependents"][..], &["--also-test", "ws-app"][..]] {
        let mut args = vec!["--json", "--quiet", "--package", "ws-core"];
        args.extend_from_slice(flags);
        let run = tmt(&ws, &args);
        assert_eq!(run.code, 0, "{flags:?}: {}{}", run.stdout, run.stderr);
        let report: Value = serde_json::from_str(&run.stdout).unwrap();
        assert_eq!(
            report["findings"].as_array().unwrap().len(),
            0,
            "{report:#}"
        );
        assert_eq!(
            report["sites"], 4,
            "still only the core package's code: {report:#}"
        );
        assert!(
            report["mutants_killed"].as_u64().unwrap() >= 2,
            "{report:#}"
        );
    }
    let unknown = tmt(
        &ws,
        &["--quiet", "--package", "ws-core", "--also-test", "nope"],
    );
    assert_eq!(unknown.code, 1);
    assert!(unknown.stderr.contains("--also-test"), "{}", unknown.stderr);
}

#[test]
fn surviving_mutants_are_traced_to_what_they_change() {
    let dir = fixture("batch-store", "effects");
    let (_, report) = json(&dir);
    let gap = findings(&report, "assertion_gap")[0];
    let effects = strings(&gap["effects"]);
    // Without the precondition check the failing batch gets as far as writing a document.
    assert!(
        effects.iter().any(
            |e| e.contains("commit_fails_when_an_update_target_is_missing")
                && e.contains("now runs `self.docs.insert(key.clone(), value.clone());`")
        ),
        "{effects:?}"
    );
    let plain = tmt(&dir, &["--json", "--quiet", "--no-effects"]);
    let plain: Value = serde_json::from_str(&plain.stdout).unwrap();
    assert_eq!(
        findings(&plain, "assertion_gap")[0]["effects"]
            .as_array()
            .unwrap()
            .len(),
        0
    );

    // A mutant whose effect the tests never look at.
    let sink = fixture("kitchen-sink", "effects-sink");
    let (_, report) = json(&sink);
    let total = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["code"] == "self.total += 1;" && f["kind"] == "assertion_gap")
        .unwrap();
    assert_eq!(
        strings(&total["effects"]),
        [
            "with the mutant, these tests still reach the same code with the same outcomes, as far as that is recorded: they do not use its result, it is the same for their inputs, or what they assert holds for both"
        ]
    );

    let sarif = tmt(&dir, &["--sarif", "--quiet"]);
    assert_eq!(sarif.code, 2);
    let sarif: Value = serde_json::from_str(&sarif.stdout).unwrap();
    assert_eq!(sarif["runs"][0]["results"][0]["ruleId"], "assertion-gap");
}

#[test]
fn ranges_min_max_match_arms_and_option_results() {
    let dir = fixture("shapes", "shapes");
    let (code, report) = json(&dir);
    assert_eq!(code, 2, "{report:#}");
    assert_eq!(report["skipped_sites"], 0, "{report:#}");
    let all = report["findings"].as_array().unwrap();
    let at = |code: &str, kind: &str| {
        all.iter()
            .find(|f| f["code"] == code && f["kind"] == kind)
            .unwrap_or_else(|| panic!("no {kind} at `{code}`: {report:#}"))
    };
    // One element too many adds a zero: the test cannot tell.
    assert_eq!(
        strings(&at("0..n", "assertion_gap")["survivors"]),
        ["`0..=n`"]
    );
    // `min` and `max` agree when the operands are equal.
    let clamp = at("level.min(cap)", "input_gap");
    assert_eq!(strings(&clamp["survivors"]), ["`level.max(cap)`"]);
    assert_eq!(
        strings(&clamp["add_tests"]),
        ["`level` different from `cap`"]
    );
    // Never `Some` in the tests.
    let find = at("fn first_even(values: &[u32]) -> Option<u32>", "input_gap");
    assert_eq!(
        strings(&find["survivors"]),
        ["`first_even` returning `None`"]
    );
    assert_eq!(
        strings(&find["add_tests"]),
        ["a case where `first_even` returns `Some`"]
    );
    // Without the arm the value falls through to "many", which is not "none" either.
    assert_eq!(
        strings(&at("few if few < 10 => ..", "assertion_gap")["survivors"]),
        ["remove the arm `few if few < 10 => ..`"]
    );

    let codes: Vec<&str> = all.iter().map(|f| f["code"].as_str().unwrap()).collect();
    // Killed: `max` with different operands, a `Some` that becomes `None`, the `0` arm.
    for quiet in [
        "level.max(floor)",
        "fn position_of(values: &[u32], wanted: u32) -> Option<usize>",
        "0 => ..",
    ] {
        assert!(
            !codes.contains(&quiet),
            "`{quiet}` should have no finding: {report:#}"
        );
    }

    append(
        &dir.join("src/lib.rs"),
        r#"
#[test]
fn closes_the_gaps() {
    assert_eq!(sum_first(&[1, 2, 9], 2), 3);
    assert_eq!(clamp_level(9, 5), 5);
    assert_eq!(first_even(&[1, 4]), Some(4));
    assert_eq!(bucket(3), "few");
}
"#,
    );
    let (_, report) = json(&dir);
    let codes: Vec<&str> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["code"].as_str().unwrap())
        .collect();
    for closed in [
        "0..n",
        "level.min(cap)",
        "fn first_even(values: &[u32]) -> Option<u32>",
        "few if few < 10 => ..",
    ] {
        assert!(
            !codes.contains(&closed),
            "`{closed}` should be closed: {report:#}"
        );
    }
}

#[test]
fn first_and_last_match_are_distinguished_by_a_verified_test() {
    for pinned in [false, true] {
        for library in [false, true] {
            let dir = fixture("shapes", &format!("position-witness-{pinned}-{library}"));
            let target = if library { "shapes" } else { "selected" };
            let source = if library {
                dir.join("src/lib.rs")
            } else {
                fs::create_dir(dir.join("tests")).unwrap();
                let test_path = dir.join("tests/selected.rs");
                fs::write(&test_path, "use shapes::position_of;\n").unwrap();
                test_path
            };
            if !library || pinned {
                append(
                    &source,
                    "\n#[test]\nfn searches() {\n    assert_eq!(position_of(&[4, 5], 5), Some(1));\n    assert_eq!(position_of(&[4, 5], 6), None);\n",
                );
                if pinned {
                    append(
                        &source,
                        "    assert_eq!(position_of(&[5, 5], 5), Some(0));\n",
                    );
                }
                append(&source, "}\n");
            }
            let original = fs::read_to_string(dir.join("src/lib.rs")).unwrap();
            for mode in [None, Some("--rebuild"), Some("--source-only")] {
                let mut args = vec![
                    "--json",
                    "--quiet",
                    "--lines",
                    "src/lib.rs:28-29",
                    "--test-target",
                    target,
                ];
                args.extend(mode);
                let run = tmt(&dir, &args);
                assert_ne!(run.code, 1, "{args:?}: {}", run.stderr);
                let report: Value = serde_json::from_str(&run.stdout).unwrap();
                let expected = mode.is_some() && !pinned;
                let survivor = findings(&report, "survivor").into_iter().find(|finding| {
                    strings(&finding["survivors"]).contains(&"replace `position` with `rposition`")
                });
                assert_eq!(
                    survivor.is_some(),
                    expected,
                    "{args:?} {pinned}: {report:#}"
                );
                if let Some(finding) = survivor {
                    assert_eq!(finding["item"], "position_of");
                    if library {
                        assert_eq!(
                            finding["witness"],
                            serde_json::json!({
                                "call": "position_of(&[5, 5], 5)",
                                "original": "Some(0)",
                                "mutant": "Some(1)",
                                "assertion": "assert_eq!(position_of(&[5, 5], 5), Some(0));"
                            })
                        );
                    } else {
                        assert!(finding.get("witness").is_none(), "{finding:#}");
                    }
                }
                assert_eq!(
                    fs::read_to_string(dir.join("src/lib.rs")).unwrap(),
                    original
                );
            }
        }
    }

    let dir = fixture("shapes", "position-witness-formats");
    let args = ["--quiet", "--rebuild", "--lines", "src/lib.rs:28-29"];
    for format in [None, Some("--sarif")] {
        let flags: Vec<_> = args.into_iter().chain(format).collect();
        let run = tmt(&dir, &flags);
        assert_eq!(run.code, 2, "{}", run.stderr);
        assert!(
            run.stdout.contains("original returned Some(0)"),
            "{}",
            run.stdout
        );
        assert!(
            run.stdout.contains("mutant returned Some(1)"),
            "{}",
            run.stdout
        );
        assert!(
            run.stdout
                .contains("assert_eq!(position_of(&[5, 5], 5), Some(0));"),
            "{}",
            run.stdout
        );
    }

    let outside = tmt(
        &dir,
        &["--json", "--quiet", "--rebuild", "--lines", "src/lib.rs:4"],
    );
    let report: Value = serde_json::from_str(&outside.stdout).unwrap();
    assert!(!outside.stdout.contains("rposition"), "{report:#}");
}

#[test]
fn position_witness_honors_predicate_scope_and_works_without_the_std_prelude() {
    for (name, no_std) in [("edition-2015", false), ("no-std", true)] {
        let dir = fixture("shapes", &format!("position-witness-{name}"));
        let path = dir.join("src/lib.rs");
        let src = fs::read_to_string(&path).unwrap().replace(
            ".position(|v| *v == wanted)",
            ".position(\n        |v| *v == wanted,\n    )",
        );
        let src = if no_std {
            format!("#![no_std]\n{src}")
        } else {
            src
        };
        fs::write(path, src).unwrap();
        if !no_std {
            let path = dir.join("Cargo.toml");
            let manifest = fs::read_to_string(&path).unwrap();
            assert!(manifest.contains("\"2021\""));
            fs::write(path, manifest.replace("\"2021\"", "\"2015\"")).unwrap();
        }
        let predicate = if no_std {
            "src/lib.rs:31"
        } else {
            "src/lib.rs:30"
        };
        let mode = if no_std { "--rebuild" } else { "--source-only" };
        let run = tmt(&dir, &["--json", "--quiet", mode, "--lines", predicate]);
        assert_eq!(run.code, 2, "{name}: {}", run.stderr);
        let report: Value = serde_json::from_str(&run.stdout).unwrap();
        let witness = findings(&report, "survivor")
            .into_iter()
            .find(|f| f["code"] == "position");
        assert_eq!(
            witness.map(|f| &f["witness"]["original"]),
            Some(&serde_json::json!("Some(0)")),
            "{name}: {report:#}"
        );
    }
}

#[cfg(unix)]
#[test]
fn a_failed_position_observation_leaves_a_survivor_and_restores_the_copy() {
    use std::os::unix::fs::PermissionsExt;

    let dir = fixture("shapes", "position-witness-retry");
    let wrapper = dir.join("wrapper.sh");
    fs::write(
        &wrapper,
        r#"#!/bin/sh
if [ -n "${TMT_WITNESS_OUTPUT:-}" ]; then
    printf 'Some(42)\n' >> "$TMT_WITNESS_OUTPUT"
    exit 1
fi
exec "$@"
"#,
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let at = [
        "--json",
        "--quiet",
        "--rebuild",
        "--lines",
        "src/lib.rs:28-29",
        "--test-target",
        "shapes",
    ];
    let first = tmt(
        &dir,
        &[at.as_slice(), &["--test-wrapper", "wrapper.sh"]].concat(),
    );
    assert_eq!(first.code, 2, "{}", first.stderr);
    let report: Value = serde_json::from_str(&first.stdout).unwrap();
    let result = findings(&report, "survivor")
        .into_iter()
        .find(|finding| finding["code"] == "position")
        .unwrap();
    assert!(result.get("witness").is_none(), "{result:#}");
    let copied = fs::read_to_string(dir.join("target/tmt/src/src/lib.rs")).unwrap();
    assert!(!copied.contains("__tmt_witness"));
    assert!(!copied.contains("rposition"));

    let retry = tmt(&dir, &at);
    assert_eq!(retry.code, 2, "{}", retry.stderr);
    let report: Value = serde_json::from_str(&retry.stdout).unwrap();
    let result = findings(&report, "survivor")
        .into_iter()
        .find(|finding| finding["code"] == "position")
        .unwrap();
    assert_eq!(result["witness"]["mutant"], "Some(1)", "{result:#}");
}

#[test]
fn verified_witnesses_respect_rebuild_mode_and_existing_tests() {
    for rule in ["mask", "range", "pair"] {
        for pinned in [false, true] {
            let crate_name = if rule == "mask" {
                "operators"
            } else {
                "shapes"
            };
            let dir = fixture(crate_name, &format!("verified-witness-{rule}-{pinned}"));
            let source = dir.join("src/lib.rs");
            let (lines, at, expected) = match rule {
                "mask" => {
                    if pinned {
                        append(
                            &source,
                            "\n#[test] fn partial_mask() { assert!(has_flag(0b01, 0b11)); }\n",
                        );
                    }
                    ("src/lib.rs:72-74", "has_flag", ("true", "false"))
                }
                "range" => {
                    if pinned {
                        let input = fs::read_to_string(&source).unwrap();
                        fs::write(&source, input.replace("[1, 2, 0]", "[1, 2, 1]")).unwrap();
                    }
                    ("src/lib.rs:4-9", "sum_first", ("3", "4"))
                }
                "pair" => {
                    fs::write(
                        &source,
                        r#"pub fn encode(value: u16) -> [u8; 2] { value.to_le_bytes() }
pub fn decode(bytes: [u8; 2]) -> u16 { u16::from_le_bytes(bytes) }
#[test]
fn round_trip() { assert_eq!(decode(encode(0x0102)), 0x0102); }
"#,
                    )
                    .unwrap();
                    if pinned {
                        append(
                            &source,
                            "#[test] fn wire_format() { assert_eq!(encode(0x0102), [2, 1]); }\n",
                        );
                    }
                    ("src/lib.rs:1-2", "encode", ("[2, 1]", "[1, 2]"))
                }
                _ => unreachable!(),
            };
            let before = fs::read_to_string(&source).unwrap();
            for mode in [None, Some("--rebuild"), Some("--source-only")] {
                let args: Vec<_> = ["--json", "--quiet", "--lines", lines]
                    .into_iter()
                    .chain(mode)
                    .collect();
                let run = tmt(&dir, &args);
                assert!(
                    matches!(run.code, 0 | 2 | 3),
                    "{rule} {args:?}: {}",
                    run.stderr
                );
                let report: Value = serde_json::from_str(&run.stdout).unwrap();
                let matching = report["findings"].as_array().unwrap().iter().find(|f| {
                    f["item"] == at
                        && f["witness"]["original"] == expected.0
                        && f["witness"]["mutant"] == expected.1
                });
                assert_eq!(
                    matching.is_some(),
                    mode.is_some() && !pinned,
                    "{rule} {pinned} {args:?}: {report:#}"
                );
                if let Some(finding) = matching {
                    if rule == "range" {
                        assert_eq!(finding["witness"]["call"], "sum_first(&[1, 2, 1], 2)");
                        assert!(
                            finding["witness"]["guidance"]
                                .as_str()
                                .unwrap()
                                .contains("[1, 2, 0]")
                        );
                    }
                    if rule == "pair" {
                        assert_eq!(finding["witness"]["individually_killed"], 2);
                        assert!(
                            finding["witness"]["assertion"]
                                .as_str()
                                .unwrap()
                                .contains("encode(")
                        );
                        let survivor = strings(&finding["survivors"]).join(" ");
                        assert!(
                            survivor.contains("to_be_bytes") && survivor.contains("from_be_bytes")
                        );
                    }
                }
                assert_eq!(fs::read_to_string(&source).unwrap(), before);
            }
        }
    }
}

#[test]
fn range_witnesses_preserve_the_range_and_check_the_existing_assertion() {
    for grouped in [false, true] {
        for keeps_expectation in [false, true] {
            let dir = fixture(
                "shapes",
                &format!("range-witness-{grouped}-{keeps_expectation}"),
            );
            let path = dir.join("src/lib.rs");
            let mut source = fs::read_to_string(&path).unwrap();
            let survivor = if grouped {
                source = source.replace("for i in 0..n {", "for i in (0)..(n) {");
                "`(0)..=(n)`"
            } else {
                "`0..=n`"
            };
            if !keeps_expectation {
                source = source.replace("let mut total = 0;", "let mut total = values[n];");
            }
            fs::write(&path, source).unwrap();
            for mode in ["--rebuild", "--source-only"] {
                let run = tmt(
                    &dir,
                    &["--json", "--quiet", "--lines", "src/lib.rs:4-9", mode],
                );
                assert_eq!(run.code, 2, "{mode}: {}", run.stderr);
                let report: Value = serde_json::from_str(&run.stdout).unwrap();
                let finding = report["findings"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|finding| {
                        finding["item"] == "sum_first"
                            && strings(&finding["survivors"]).contains(&survivor)
                    })
                    .unwrap_or_else(|| panic!("{survivor} {mode}: {report:#}"));
                let expected = if keeps_expectation { "3" } else { "4" };
                let witness = &finding["witness"];
                assert_eq!(
                    witness["original"], expected,
                    "{survivor} {mode}: {finding:#}"
                );
                assert_eq!(
                    witness["assertion"],
                    format!("assert_eq!(sum_first(&[1, 2, 1], 2), {expected});")
                );
                assert_eq!(
                    witness.get("guidance").is_some(),
                    keeps_expectation,
                    "{finding:#}"
                );
            }
        }
    }
}

#[test]
fn unicode_arguments_fields_and_sort_keys_get_distinguishing_tests() {
    let dir = fixture("distinguishing", "verified-witness-relations");
    let run = tmt(&dir, &["--json", "--quiet", "--rebuild"]);
    assert_eq!(run.code, 2, "{}", run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    let mut suggestions = Vec::new();
    for item in ["chars", "expiry", "field_expiry", "order"] {
        let finding = findings(&report, "survivor")
            .into_iter()
            .find(|f| f["item"] == item && f.get("witness").is_some());
        let finding = finding.unwrap_or_else(|| panic!("no witness for {item}: {report:#}"));
        let witness = &finding["witness"];
        assert_ne!(witness["original"], witness["mutant"]);
        assert!(
            witness["assertion"]
                .as_str()
                .unwrap()
                .contains("assert_eq!")
        );
        if item == "chars" {
            assert_eq!(witness["original"], "1");
            assert_eq!(witness["mutant"], "2");
        }
        suggestions.push((
            item,
            strings(&finding["survivors"])[0],
            witness["assertion"].as_str().unwrap(),
        ));
    }
    for (item, _, assertion) in &suggestions {
        append(
            &dir.join("src/lib.rs"),
            &format!("\n#[test] fn check_{item}() {{ {assertion} }}\n"),
        );
    }
    let run = tmt(&dir, &["--json", "--quiet", "--rebuild"]);
    assert!(matches!(run.code, 0 | 2), "{}", run.stderr);
    let closed: Value = serde_json::from_str(&run.stdout).unwrap();
    for (item, mutation, _) in suggestions {
        assert!(
            !findings(&closed, "survivor")
                .iter()
                .any(|f| { f["item"] == item && strings(&f["survivors"]).contains(&mutation) }),
            "{item} {mutation}: {closed:#}"
        );
    }
}

#[test]
fn a_weak_assertion_gets_the_value_its_call_returns() {
    let dir = fixture("shapes", "stronger-assertion");
    let run = tmt(&dir, &["--json", "--quiet", "--rebuild"]);
    assert_eq!(run.code, 2, "{}", run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    // `assert_ne!(bucket(3), "none")` has the input; only what it compares with is weak.
    let finding = findings(&report, "assertion_gap")
        .into_iter()
        .find(|f| f["code"] == "few if few < 10 => ..")
        .unwrap_or_else(|| panic!("{report:#}"));
    assert_eq!(finding["witness"]["call"], "bucket(3)", "{finding:#}");
    assert_eq!(finding["witness"]["original"], "\"few\"", "{finding:#}");
    assert_eq!(finding["witness"]["mutant"], "\"many\"", "{finding:#}");
    assert_eq!(
        finding["witness"]["assertion"],
        "assert_eq!(bucket(3), \"few\");"
    );
    assert_eq!(
        finding["witness"]["in_test"], "tests::buckets",
        "{finding:#}"
    );
    assert_eq!(finding["witness"]["in_test_line"], 67, "{finding:#}");
    assert_eq!(finding["witness"]["in_test_column"], 20, "{finding:#}");
    // A finding that asks for an input is not given a call of the tests: they all
    // return what the original returns.
    for gap in findings(&report, "input_gap") {
        assert!(gap.get("witness").is_none(), "{gap:#}");
    }
    let text = tmt(
        &dir,
        &["--quiet", "--rebuild", "--lines", "src/lib.rs:36-40"],
    );
    assert!(
        text.stdout.contains(
            "  verified on `bucket(3)` where `tests::buckets` calls it at line 67, column 20: original returned \"few\", mutant returned \"many\"\n  \
             in `tests::buckets`, compare the result of the call at line 67, column 20 with this value: `assert_eq!(bucket(3), \"few\");`"
        ),
        "{}",
        text.stdout
    );

    // The proposed line closes the finding.
    append(
        &dir.join("src/lib.rs"),
        "\n#[test] fn pinned() { assert_eq!(bucket(3), \"few\"); }\n",
    );
    let run = tmt(&dir, &["--json", "--quiet", "--rebuild"]);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert!(
        !report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["code"] == "few if few < 10 => .."),
        "{report:#}"
    );
}

#[test]
fn a_stronger_assertion_has_the_value_the_call_has_where_the_test_makes_it() {
    let dir = fixture("eligibility", "stronger-assertion-in-place");
    fs::write(
        dir.join("src/lib.rs"),
        r#"use std::cell::Cell;
thread_local! { static MODE: Cell<i32> = const { Cell::new(1) }; }
pub fn enable(mode: i32) -> bool {
    MODE.with(|m| m.set(mode));
    true
}
pub fn f(n: i32) -> i32 {
    n + MODE.with(|m| m.get())
}

#[test]
fn never_gets_there() {
    let off = true;
    assert!(off || f(9) != 0);
}

#[test]
fn stateful() {
    assert_eq!(enable(2), f(3) > 0);
    assert!(enable(1));
}

pub fn timeout() -> u64 {
    60 * 1000
}
pub fn place(n: u32) -> u32 {
    let _ = n
        + 1;
    line!()
}

#[test]
fn weak() {
    assert!(timeout() > 0);
    assert!(place(3) > 0);
}
"#,
    )
    .unwrap();
    let run = tmt(&dir, &["--json", "--quiet", "--rebuild"]);
    assert_eq!(run.code, 2, "{}", run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    let finding = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["code"] == "n + MODE.with(|m| m.get())" && f.get("witness").is_some())
        .unwrap_or_else(|| panic!("{report:#}"));
    // The call the test never gets to says nothing. The other is looked at after the
    // mode is set, in the same statement, as the test does it: 3 + 2, not 3 + 1.
    assert_eq!(finding["witness"]["call"], "f(3)", "{finding:#}");
    assert_eq!(finding["witness"]["original"], "5", "{finding:#}");
    assert_eq!(finding["witness"]["assertion"], "assert_eq!(f(3), 5);");
    // The value is the one the call has in that test, and the report says so.
    assert_eq!(finding["witness"]["in_test"], "stateful", "{finding:#}");
    assert_eq!(finding["witness"]["in_test_line"], 19, "{finding:#}");
    // A product of two numbers is mutated by building it only: its mutant that passes
    // is looked at like any other.
    let product = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["code"] == "60 * 1000" && f.get("witness").is_some())
        .unwrap_or_else(|| panic!("{report:#}"));
    assert_eq!(product["witness"]["call"], "timeout()", "{product:#}");
    assert_eq!(product["witness"]["original"], "60000", "{product:#}");
    assert_eq!(product["witness"]["in_test"], "weak", "{product:#}");
    // A mutant keeps the lines of the source: an operator with a line end beside it
    // does not move the `line!()` after it, and nothing tells the two results apart.
    let moved: Vec<_> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["item"] == "place")
        .collect();
    assert!(moved.len() > 1, "{report:#}");
    for finding in moved {
        if finding["code"] == "fn place(n: u32) -> u32" {
            // The body replaced by a number does return another: the line is the
            // one `line!()` is on in the source.
            assert_eq!(finding["witness"]["original"], "29", "{finding:#}");
        } else {
            assert!(finding.get("witness").is_none(), "{finding:#}");
        }
    }

    // Put where the test makes the call, the assertion holds and closes the finding.
    let path = dir.join("src/lib.rs");
    let source = fs::read_to_string(&path).unwrap();
    let pinned = source.replace(
        "assert_eq!(enable(2), f(3) > 0);",
        "assert!(enable(2));\n    assert_eq!(f(3), 5);",
    );
    assert_ne!(pinned, source);
    fs::write(&path, pinned).unwrap();
    let run = tmt(&dir, &["--json", "--quiet", "--rebuild"]);
    assert_ne!(run.code, 1, "{}", run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert!(
        !report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["code"] == "n + MODE.with(|m| m.get())" && f["kind"] != "input_gap"),
        "{report:#}"
    );
}

#[test]
fn a_failing_batch_gets_an_assertion_on_its_persisted_state() {
    let dir = fixture("batch-store", "verified-witness-after-error");
    let args = ["--json", "--quiet", "--rebuild", "--lines", "src/lib.rs:33"];
    let run = tmt(&dir, &args);
    assert_eq!(run.code, 2, "{}", run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    let finding = findings(&report, "assertion_gap")
        .into_iter()
        .find(|f| f["code"] == "self.check_preconditions(batch)?;")
        .unwrap();
    assert_eq!(finding["witness"]["original"], "None", "{finding:#}");
    assert_eq!(finding["witness"]["mutant"], "Some(\"1\")", "{finding:#}");
    assert_eq!(
        finding["witness"]["assertion"],
        "assert_eq!(store.get(\"a\"), None);"
    );

    let success_only = tmt(
        &dir,
        &[
            args.as_slice(),
            &[
                "--include-test",
                "tests::batch_writes_are_visible_after_commit",
            ],
        ]
        .concat(),
    );
    assert!(
        matches!(success_only.code, 0 | 2),
        "{}",
        success_only.stderr
    );
    assert!(
        !success_only.stdout.contains("\"witness\""),
        "{}",
        success_only.stdout
    );

    let path = dir.join("src/lib.rs");
    let input = fs::read_to_string(&path).unwrap();
    fs::write(&path, input.replace("assert_eq!(result, Err(CommitError::NotFound(\"b\".into())));", "assert_eq!(result, Err(CommitError::NotFound(\"b\".into())));\n        assert_eq!(store.get(\"a\"), None);")).unwrap();
    let closed = tmt(&dir, &args);
    assert!(matches!(closed.code, 0 | 2), "{}", closed.stderr);
    assert!(
        !closed
            .stdout
            .contains("delete `self.check_preconditions(batch)?;`"),
        "{}",
        closed.stdout
    );
}

#[test]
fn a_debug_value_with_the_wrong_assertion_type_is_not_suggested() {
    let dir = fixture("batch-store", "verified-witness-debug-type");
    let path = dir.join("src/lib.rs");
    let source = fs::read_to_string(&path).unwrap()
        .replace("#[derive(Default)]\n", "")
        .replace("impl Store {", "impl Default for Store {\n    fn default() -> Self { Self { docs: BTreeMap::from([(String::from(\"a\"), String::from(\"old\"))]) } }\n}\nimpl Store {")
        .replace("Option<&str>", "Option<&String>")
        .replace("self.docs.get(key).map(String::as_str)", "self.docs.get(key)")
        .replace("Some(\"2\")", "Some(&String::from(\"2\"))");
    fs::write(&path, source).unwrap();
    let run = tmt(&dir, &["--json", "--quiet", "--rebuild"]);
    assert_eq!(run.code, 2, "{}", run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    let finding = findings(&report, "assertion_gap")
        .into_iter()
        .find(|f| f["code"] == "self.check_preconditions(batch)?;")
        .unwrap();
    assert!(finding.get("witness").is_none(), "{finding:#}");
}

#[test]
fn distinguishing_witness_checks_actual_equality_in_both_variants() {
    let template = r#"
pub struct Observed(usize);

impl std::fmt::Debug for Observed {
    fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, fmt)
    }
}

impl PartialEq<usize> for Observed {
    fn eq(&self, expected: &usize) -> bool {
        COMPARISON
    }
}
"#;
    for (label, equality) in [
        ("mutant-equal", "*expected == 0 || self.0 == *expected"),
        ("original-unequal", "*expected != 0 && self.0 == *expected"),
    ] {
        let dir = fixture("batch-store", &format!("witness-{label}"));
        let path = dir.join("src/lib.rs");
        let source = fs::read_to_string(&path)
            .unwrap()
            .replace(
                "impl Store {",
                &format!(
                    "{}\nimpl Store {{",
                    template.replace("COMPARISON", equality)
                ),
            )
            .replace("Option<&str>", "Observed")
            .replace(
                "self.docs.get(key).map(String::as_str)",
                "Observed(usize::from(self.docs.contains_key(key)))",
            )
            .replace(
                "assert_eq!(store.get(\"a\"), Some(\"2\"));",
                "assert_eq!(store.get(\"a\"), 1);",
            );
        fs::write(&path, source).unwrap();
        let run = tmt(&dir, &["--json", "--quiet", "--rebuild"]);
        assert_eq!(run.code, 2, "{label}: {}", run.stderr);
        let report: Value = serde_json::from_str(&run.stdout).unwrap();
        let finding = findings(&report, "assertion_gap")
            .into_iter()
            .find(|f| f["code"] == "self.check_preconditions(batch)?;")
            .unwrap();
        assert!(finding.get("witness").is_none(), "{label}: {finding:#}");
    }
}

#[test]
fn paired_witnesses_respect_decoder_scope_test_selection_and_skip_attributes() {
    let dir = fixture("shapes", "verified-witness-selected-pair");
    let path = dir.join("src/lib.rs");
    let mut source = String::from(
        "pub fn encode(value: u16) -> [u8; 2] { value.to_le_bytes() }\npub fn decode(bytes: [u8; 2]) -> u16\n{\n    u16::from_le_bytes(bytes)\n}\n",
    );
    for number in 0..5 {
        source.push_str(&format!(
            "#[test] fn earlier_{number}() {{ assert_eq!(decode(encode(0x0102)), 0x0102); }}\n"
        ));
    }
    source.push_str("#[test] fn selected() { assert_eq!(decode(encode(0x0102)), 0x0102); }\n");
    fs::write(&path, &source).unwrap();
    let selected = tmt(
        &dir,
        &[
            "--json",
            "--quiet",
            "--rebuild",
            "--include-test",
            "selected",
            "--lines",
            "src/lib.rs:2",
        ],
    );
    assert_eq!(selected.code, 2, "{}", selected.stderr);
    let report: Value = serde_json::from_str(&selected.stdout).unwrap();
    assert!(
        findings(&report, "survivor")
            .iter()
            .any(|f| f["witness"]["individually_killed"] == 2),
        "{report:#}"
    );

    for attribute in ["#[cfg(test)]", "#[cfg_attr(any(), mutants::skip)]"] {
        fs::write(&path, format!("{attribute}\n{source}")).unwrap();
        let rejected = tmt(&dir, &["--json", "--quiet", "--rebuild"]);
        assert!(matches!(rejected.code, 0 | 2 | 3), "{}", rejected.stderr);
        assert!(
            !rejected.stdout.contains("reverse the byte order together"),
            "{}",
            rejected.stdout
        );
    }
}

#[test]
fn shared_witness_builds_fall_back_to_individual_inputs_on_a_name_conflict() {
    let dir = fixture("distinguishing", "witness-batch-fallback");
    append(
        &dir.join("src/lib.rs"),
        "\nmod helper;\n#[test] fn uses_other_module() { assert!(helper::is_large(2)); }\n",
    );
    fs::write(
        dir.join("src/helper.rs"),
        "pub fn is_large(value: u8) -> bool { value > 1 }\n",
    )
    .unwrap();
    let args = [
        "--rebuild",
        "--json",
        "--lines",
        "src/lib.rs:5",
        "--lines",
        "src/lib.rs:9",
        "--lines",
        "src/helper.rs",
    ];
    let run = tmt(&dir, &args);
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(
        run.stderr
            .contains("sharing a build for 2 original witness inputs"),
        "{}",
        run.stderr
    );
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f.get("witness").is_some())
            .count(),
        2
    );

    append(
        &dir.join("src/lib.rs"),
        "\n#[cfg(test)]\nfn __tmt_witness_0() {}\n",
    );
    let run = tmt(&dir, &args);
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(
        run.stderr.contains("shared witness build failed"),
        "{}",
        run.stderr
    );
    let fallback: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(fallback, report);
}

#[test]
fn an_inconclusive_shared_witness_retries_the_original_test_name() {
    let dir = fixture("shapes", "witness-batch-test-name");
    let source = dir.join("src/lib.rs");
    let old = fs::read_to_string(&source).unwrap();
    let body = old.find("pub fn sum_first").unwrap();
    let brace = body + old[body..].find('{').unwrap() + 1;
    let text = format!(
        r#"{} if std::env::args().any(|arg| arg.starts_with("__tmt_witness_")) {{ return 0; }} {}"#,
        &old[..brace],
        &old[brace..]
    );
    fs::write(source, text).unwrap();
    let run = tmt(&dir, &["--json", "--rebuild"]);
    assert_eq!(run.code, 2, "{}", run.stderr);
    assert!(
        run.stderr
            // The range, the search, and the call of `bucket` for each of its two findings.
            .contains("sharing a build for 4 original witness inputs"),
        "{}",
        run.stderr
    );
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    let range = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["code"] == "0..n")
        .unwrap();
    assert_eq!(range["witness"]["original"], "3", "{range:#}");
    assert_eq!(range["witness"]["mutant"], "4", "{range:#}");
}

#[test]
fn configured_result_errors_are_mutated_in_every_execution_mode() {
    let dir = fixture("genres", "configured-errors");
    let source = dir.join("src/lib.rs");
    // Pairwise table from errors.pict, including the seeded default and three execution paths.
    let rows = [
        ("direct", 0, None),
        ("direct", 1, None),
        ("direct", 1, Some("--rebuild")),
        ("direct", 1, Some("--source-only")),
        ("iterator", 1, Some("--rebuild")),
        ("iterator", 1, Some("--source-only")),
        ("other", 1, None),
        ("alias", 1, Some("--source-only")),
        ("nested", 0, Some("--rebuild")),
        ("iterator", 0, None),
        ("other", 2, Some("--source-only")),
        ("nested", 1, Some("--source-only")),
        ("alias", 0, Some("--rebuild")),
        ("iterator", 2, Some("--rebuild")),
        ("nested", 2, None),
        ("direct", 2, Some("--source-only")),
        ("alias", 2, None),
        ("other", 0, Some("--source-only")),
        ("other", 1, Some("--rebuild")),
    ];
    for (shape, errors, mode) in rows {
        let (signature, result) = match shape {
            "direct" => ("core::result::Result<u32, String>", "Ok(n)"),
            "nested" => ("Option<Result<u32, String>>", "Some(Ok(n))"),
            "alias" => ("alias::Result", "Ok(())"),
            "iterator" => (
                "impl Iterator<Item = Result<u32, String>>",
                "core::iter::once(Ok(n))",
            ),
            "other" => ("u32", "n"),
            _ => unreachable!(),
        };
        let body = if shape == "alias" {
            "let _ = n; Ok(())"
        } else {
            result
        };
        let (call, expected) = if shape == "iterator" {
            ("parse(n).collect::<Vec<_>>()", "vec![Ok(n)]")
        } else {
            ("parse(n)", result)
        };
        fs::write(
            &source,
            format!(
                "mod alias {{ pub type Result = core::result::Result<(), String>; }}\n\
                 pub fn parse(n: u32) -> {signature} {{ {body} }}\n\
                 #[test]\nfn accepts_success() {{\n    let n = 5;\n    assert_eq!({call}, {expected});\n}}\n",
            ),
        )
        .unwrap();
        let mut args = vec!["--quiet", "--json", "--lines", "src/lib.rs:2"];
        args.extend(mode);
        for expression in [r#""mutated".into()"#, "String::from(7_u32)"]
            .into_iter()
            .take(errors)
        {
            args.extend(["--error", expression]);
        }
        let run = tmt(&dir, &args);
        let report: Value = serde_json::from_str(&run.stdout)
            .unwrap_or_else(|err| panic!("{args:?}: {err}: {}", run.stderr));
        let base = match shape {
            "nested" if mode == Some("--source-only") => 3,
            "nested" => 4,
            "alias" => 1,
            "iterator" => 3,
            _ => 2,
        };
        if shape == "iterator" && mode.is_none() {
            assert_eq!(report["mutants_not_built"], base + errors, "{report:#}");
            continue;
        }
        let configured = if shape == "other" { 0 } else { errors };
        assert_eq!(
            report["mutants_run"],
            base + configured.min(1),
            "{shape} {args:?} {report:#}"
        );
        assert_eq!(
            report["unviable_mutants"],
            configured.saturating_sub(1),
            "{shape} {args:?} {report:#}"
        );
        assert_eq!(
            report["mutants_killed"],
            if shape == "alias" {
                configured.min(1)
            } else {
                base + configured.min(1)
            },
            "{shape} {args:?} {report:#}"
        );
        if errors > 0 {
            assert_eq!(
                report["analysis"]["error_values"].as_array().unwrap().len(),
                errors
            );
        } else {
            assert!(
                report["analysis"].get("error_values").is_none(),
                "{report:#}"
            );
        }
    }
    // One line of Rust, with nothing in it that is not to be seen.
    for unseen in ["E::new(\n1)", "E::new(1)\t// note", "E::new(1)\u{1b}"] {
        let refused = tmt(&dir, &["--error", unseen, "--quiet"]);
        assert_eq!(refused.code, 1, "{unseen:?}: {}", refused.stderr);
        assert!(
            refused.stderr.contains("without control characters"),
            "{unseen:?}: {}",
            refused.stderr
        );
    }
    let invalid = tmt(&dir, &["--error", "broken(", "--quiet"]);
    assert_eq!(invalid.code, 1);
    assert!(
        invalid.stderr.contains("invalid --error expression"),
        "{}",
        invalid.stderr
    );
}

#[test]
fn source_mutant_must_still_build_the_production_library() {
    let dir = fixture("eligibility", "source-mutant-production-lib");
    fs::write(
        dir.join("src/lib.rs"),
        r#"
        #[cfg(test)]
        pub type Code = u8;
        #[cfg(not(test))]
        pub struct Code;
        pub fn check() -> Result<(), Code> { Ok(()) }
        #[test]
        fn succeeds() { assert!(check().is_ok()); }
        "#,
    )
    .unwrap();
    let run = tmt(
        &dir,
        &["--quiet", "--json", "--source-only", "--error", "7"],
    );
    let report: Value =
        serde_json::from_str(&run.stdout).unwrap_or_else(|err| panic!("{err}: {}", run.stderr));
    assert_eq!(report["unviable_mutants"], 1, "{report:#}");
    assert_eq!(report["mutants_run"], 0, "{report:#}");
}

#[test]
fn the_mutants_of_cargo_mutants() {
    let dir = fixture("genres", "genres");
    let run = tmt(&dir, &["--json", "--quiet", "--rebuild"]);
    let (code, report) = (
        run.code,
        serde_json::from_str::<Value>(&run.stdout).unwrap(),
    );
    assert_eq!(code, 2, "{report:#}");
    assert_eq!(report["skipped_sites"], 0, "{report:#}");
    assert_eq!(report["mutants_not_built"], 0, "{report:#}");
    // `Default::default()` for a type without a default is not a mutant: `Token`, and
    // the three functions that return a `Below`.
    assert_eq!(report["unviable_mutants"], 4, "{report:#}");
    let all = report["findings"].as_array().unwrap();
    let survivors = |code: &str, kind: &str| -> Vec<&str> {
        all.iter()
            .filter(|f| f["code"] == code && f["kind"] == kind)
            .flat_map(|f| strings(&f["survivors"]))
            .collect()
    };
    // A function's body replaced by a value: the tests only look at one result.
    assert_eq!(
        survivors("fn opposite(x: i32) -> i32", "assertion_gap"),
        ["the body of `opposite` replaced by `0`"]
    );
    assert_eq!(
        survivors("fn freezing() -> i32", "assertion_gap"),
        [
            "the body of `freezing` replaced by `1`",
            "the body of `freezing` replaced by `-1`"
        ]
    );
    assert_eq!(
        survivors("fn label(n: u32) -> String", "assertion_gap"),
        ["the body of `label` replaced by `\"xyzzy\".into()`"]
    );
    assert_eq!(
        survivors(
            "fn longest<'a>(words: &[&'a str]) -> Option<&'a str>",
            "assertion_gap"
        ),
        [
            "the body of `longest` replaced by `Some(\"\")`",
            "the body of `longest` replaced by `Some(\"xyzzy\")`"
        ]
    );
    assert_eq!(
        survivors(
            "fn parse(text: &str) -> Result<u32, String>",
            "assertion_gap"
        ),
        [
            "the body of `parse` replaced by `Ok(0)`",
            "the body of `parse` replaced by `Ok(1)`"
        ]
    );
    assert_eq!(
        survivors("fn pair(n: u32) -> (u32, bool)", "assertion_gap"),
        ["the body of `pair` replaced by `(1, false)`"]
    );
    assert_eq!(
        survivors(
            "fn record(log: &mut Vec<String>, line: &str)",
            "assertion_gap"
        ),
        ["the body of `record` replaced by `()`"]
    );
    // The map is looked into, so every replacement of it fails.
    assert!(
        survivors(
            "fn lengths(words: &[&str]) -> HashMap<String, usize>",
            "assertion_gap"
        )
        .is_empty(),
        "{report:#}"
    );
    // A dropped minus: `opposite` is only called with 0, `freezing` only checked against 0.
    assert_eq!(survivors("-x", "assertion_gap"), ["`x`"]);
    assert_eq!(survivors("-5", "assertion_gap"), ["`5`"]);
    // The second replacement of `*` is `+`, which 0 tells apart; `/` it does not.
    assert_eq!(survivors("x * 3", "input_gap"), ["`x / 3`"]);
    // A guard that is true whenever the tests get there. It is said once, at the
    // comparison the guard consists of.
    assert_eq!(survivors("*side > 10", "input_gap"), ["`true`"]);
    // A field left to the base: the default depth is the 0 the test asks for.
    assert_eq!(
        survivors("depth", "assertion_gap"),
        ["without the field `depth` (it is then `(Options { ..Default::default() }).depth`)"]
    );
    let codes: Vec<&str> = all.iter().map(|f| f["code"].as_str().unwrap()).collect();
    for killed in [
        // The other fields are checked, and so are the guards of `sign`.
        "strict: true",
        "name: name.to_string()",
        "n < 0 => ..",
        // Marked with `mutants::skip`, and return types nothing else can stand in for.
        "x + 1",
        "fn untouched(x: i32) -> i32",
        "fn evens(limit: u32) -> impl Iterator<Item = u32>",
        "fn token(n: u32) -> Token",
    ] {
        assert!(!codes.contains(&killed), "`{killed}`: {report:#}");
    }
    assert_eq!(report["unreached"].as_array().unwrap().len(), 1);

    // A minus that changes the type: its probe does not compile, so the mutant is built
    // on its own instead of the site being skipped.
    // Nothing was recorded for it: it is a survivor, with no claim about the cause.
    assert_eq!(survivors("-above", "survivor"), ["`above`"]);
    assert!(survivors("-above", "assertion_gap").is_empty());
    let built = all.iter().find(|f| f["code"] == "-above").unwrap();
    assert!(built.get("tests").is_none(), "{built:#}");

    // An iterator of another type cannot be returned beside the body's own, so the
    // three replacements of `evens` are built one by one. Unless asked for they are
    // left out, and the report says how many there are.
    // The range `evens` filters is not run through by a `for`: with its end taken in
    // it is another type, and that mutant is built as well. It passes: no test asks
    // for the limit itself.
    assert_eq!(survivors("0..limit", "survivor").len(), 1, "{report:#}");
    let (_, quick) = json(&dir);
    assert_eq!(quick["mutants_not_built"], 5, "{quick:#}");
    assert_eq!(
        report["mutants_run"].as_u64().unwrap(),
        quick["mutants_run"].as_u64().unwrap() + 5,
        "{quick:#}"
    );
    assert_eq!(
        report["mutants_killed"].as_u64().unwrap(),
        quick["mutants_killed"].as_u64().unwrap() + 3,
        "{quick:#}"
    );
    // Left out, the site with the probe that did not compile is a skipped one.
    assert_eq!(quick["skipped_sites"], 1, "{quick:#}");
    assert!(
        quick["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|site| site["reason"] == "requires_source_rebuild"),
        "{quick:#}"
    );
    assert_eq!(
        quick["findings"].as_array().unwrap().len() + 2,
        all.len(),
        "{quick:#}"
    );
}

#[test]
fn the_last_round_of_a_loop_is_the_nearest_value() {
    let dir = fixture("countdown", "countdown");
    let (code, report) = json(&dir);
    assert_eq!(code, 2, "{report:#}");
    let gaps = findings(&report, "input_gap");
    assert_eq!(gaps.len(), 1, "{report:#}");
    // A distance that only comes closer is not written each time it does, but when the
    // thread ends: -11 is the last round of the thread the test started, 50 the last of
    // a test's own.
    assert_eq!(
        gaps[0]["observed"],
        "nearest values the tests passed: `i` - `limit` = -11, 50"
    );
    // Optimised code observes the same and gives the same report.
    let run = tmt(&dir, &["--json", "--quiet", "--opt-level", "2"]);
    let optimised: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(optimised["analysis"]["opt_level"], 2);
    assert_eq!(but_how(optimised), but_how(report));
    let bad = tmt(&dir, &["--quiet", "--opt-level", "4"]);
    assert_eq!(bad.code, 1, "{}", bad.stderr);
    assert!(bad.stderr.contains("--opt-level"), "{}", bad.stderr);
}

#[test]
fn a_right_operand_that_leaves_the_function_was_still_executed() {
    let dir = fixture("diverging", "diverging");
    let (code, report) = json(&dir);
    assert_eq!(code, 2, "{report:#}");
    // `positive(n)?` returns the error: the right operand never gives a value. The
    // expression is not unreached for that, and `||` in its place is run and found out
    // (it would not get as far as the error).
    let unreached: Vec<&str> = report["unreached"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["code"].as_str().unwrap())
        .collect();
    assert_eq!(unreached, ["n > 0"], "{report:#}");
    let survivors: Vec<&str> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["code"] == "flag && positive(n)?")
        .flat_map(|f| strings(&f["survivors"]))
        .collect();
    // What does survive are the two that still call `positive`.
    assert_eq!(
        survivors,
        [
            "`flag && { positive(n)?; true }`",
            "`{ flag; positive(n)? }`"
        ],
        "{report:#}"
    );
}

#[test]
fn the_features_asked_for_are_the_ones_analysed() {
    let dir = fixture("featured", "featured");
    let run = |args: &[&str]| -> (Vec<String>, Vec<String>, Value) {
        let mut all = vec!["--json", "--quiet"];
        all.extend(args);
        let run = tmt(&dir, &all);
        let report: Value = serde_json::from_str(&run.stdout)
            .unwrap_or_else(|e| panic!("{e}: {}{}", run.stdout, run.stderr));
        let codes = |list: &Value| -> Vec<String> {
            list.as_array()
                .unwrap()
                .iter()
                .map(|item| item["code"].as_str().unwrap().to_string())
                .collect()
        };
        (
            codes(&report["findings"]),
            codes(&report["unreached"]),
            report,
        )
    };
    // By default `extra` is not compiled: nothing runs it.
    let (findings, unreached, _) = run(&[]);
    assert!(findings.is_empty(), "{findings:?}");
    assert!(unreached.contains(&"n > 10".to_string()), "{unreached:?}");
    // With the feature its test runs, and is found to be far from the limit.
    let (findings, unreached, report) = run(&["--features", "extra"]);
    assert!(findings.contains(&"n > 10".to_string()), "{findings:?}");
    assert!(!unreached.contains(&"n > 10".to_string()), "{unreached:?}");
    assert_eq!(report["analysis"]["cargo"]["features"][0], "extra");
    // Without the default features `basic` is gone.
    let (_, unreached, report) = run(&["--no-default-features", "--features", "extra"]);
    assert!(unreached.contains(&"n > 5".to_string()), "{unreached:?}");
    assert_eq!(report["analysis"]["cargo"]["no_default_features"], true);
    let (findings, unreached, _) = run(&["--all-features"]);
    assert!(findings.contains(&"n > 10".to_string()), "{findings:?}");
    assert!(!unreached.contains(&"n > 5".to_string()), "{unreached:?}");
    // A feature the package does not have is cargo's error, passed on.
    let bad = tmt(&dir, &["--quiet", "--features", "nope"]);
    assert_eq!(bad.code, 1, "{}", bad.stderr);
    assert!(bad.stderr.contains("nope"), "{}", bad.stderr);
}

#[test]
fn code_a_feature_leaves_out_is_not_a_survivor_when_built_one_by_one() {
    let dir = fixture("featured", "featured-source");
    let run = tmt(&dir, &["--json", "--quiet", "--source-only"]);
    let report: Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}{}", run.stdout, run.stderr));
    // `extra` is off: the mutants of its functions change nothing that is built, also
    // the one that divides first, which is found by another rule.
    let findings = report["findings"].as_array().unwrap();
    assert!(
        findings
            .iter()
            .all(|f| f["code"] != "n > 10" && f["line"] != 16),
        "{report:#}"
    );
    assert!(
        report["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s["line"] == 16 && s["code"] == "cents * parts / 100"),
        "{report:#}"
    );
    let skipped: Vec<&Value> = report["skipped"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["reason"] == "source_not_built")
        .collect();
    assert!(!skipped.is_empty(), "{report:#}");
    assert!(
        skipped
            .iter()
            .all(|s| [9, 10, 15, 16].contains(&s["line"].as_u64().unwrap())
                && s["diagnostic"] == "under a `cfg` that the features of this build turn off"),
        "{report:#}"
    );
    let left_out = report["mutants_not_compiled"].as_u64().unwrap();
    assert!(left_out > 0, "{report:#}");
    assert_eq!(report["mutants_not_evaluated"], left_out, "{report:#}");
    // What is built is judged as before: `basic` is tested at its limit.
    assert!(report["mutants_killed"].as_u64().unwrap() > 0, "{report:#}");
    let text = tmt(&dir, &["--quiet", "--source-only"]);
    assert!(
        text.stdout.contains(&format!(
            "({left_out} mutants not evaluated: their code is not part of this build"
        )),
        "{}",
        text.stdout
    );
    assert!(!text.stdout.contains("largest value"), "{}", text.stdout);
    // A change to code the feature leaves out is not a change of this build: undone,
    // it is the same program, which says nothing of the tests.
    fs::write(dir.join(".gitignore"), "target/\nCargo.lock\n").unwrap();
    git_output(&dir, &["init", "-q"]);
    git_output(&dir, &["add", "-A"]);
    let base = git_output(&dir, &["write-tree"]);
    let path = dir.join("src/lib.rs");
    let source = fs::read_to_string(&path).unwrap();
    fs::write(&path, source.replace("n > 10", "n > 11")).unwrap();
    let run = tmt(&dir, &["--json", "--quiet", "--rebuild", "--since", &base]);
    let since: Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}{}", run.stdout, run.stderr));
    assert_eq!(since["findings"], serde_json::json!([]), "{since:#}");
    assert!(
        since["mutants_not_compiled"].as_u64().unwrap() >= 1,
        "{since:#}"
    );
    let run = tmt(
        &dir,
        &[
            "--json",
            "--quiet",
            "--rebuild",
            "--since",
            &base,
            "--features",
            "extra",
        ],
    );
    let since: Value = serde_json::from_str(&run.stdout).unwrap();
    // With the feature it is a change, and one the far-off test does not tell.
    assert!(
        since["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| !f["change_since"].is_null() && f["code"] == "n > 11"),
        "{since:#}"
    );
    fs::write(&path, &source).unwrap();

    // An item on one line that the feature leaves out is no change of this build
    // either. But the `cfg` itself is: with it reworded the function gives 2 and not
    // 9, which a test that only asks for more than nothing does not tell.
    append(
        &path,
        "\n#[cfg(feature = \"extra\")]\npub const LIMIT: u32 = 41;\n\n\
         pub fn step() -> u32 {\n    #[cfg(feature = \"basic\")]\n    return 9;\n    \
         #[allow(unreachable_code)]\n    2\n}\n\n\
         #[test]\nfn steps_forward() {\n    assert!(step() > 0);\n}\n",
    );
    git_output(&dir, &["add", "-A"]);
    let base = git_output(&dir, &["write-tree"]);
    let with_step = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        with_step.replace("LIMIT: u32 = 41;", "LIMIT: u32 = 42;"),
    )
    .unwrap();
    let run = tmt(&dir, &["--json", "--quiet", "--rebuild", "--since", &base]);
    let since: Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}{}", run.stdout, run.stderr));
    assert_eq!(since["findings"], serde_json::json!([]), "{since:#}");
    assert_eq!(since["mutants_not_compiled"], 1, "{since:#}");
    fs::write(
        &path,
        with_step.replace(
            "    #[cfg(feature = \"basic\")]\n    return 9;",
            "    #[cfg(feature = \"extra\")]\n    return 9;",
        ),
    )
    .unwrap();
    let run = tmt(&dir, &["--json", "--quiet", "--rebuild", "--since", &base]);
    assert_eq!(run.code, 2, "{}{}", run.stdout, run.stderr);
    let since: Value = serde_json::from_str(&run.stdout).unwrap();
    let undone: Vec<&Value> = since["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| !f["change_since"].is_null())
        .collect();
    assert_eq!(undone.len(), 1, "{since:#}");
    assert_eq!(
        undone[0]["code"], "#[cfg(feature = \"extra\")]",
        "{since:#}"
    );
    assert_eq!(
        undone[0]["survivors"][0],
        "with the code as it was: `#[cfg(feature = \"basic\")]`"
    );
    // A `cfg` reworded together with the code under it: that code is left out now and
    // with its own change undone, but not with the `cfg` undone. Neither goes back
    // alone; all together they are the program as it was, which the test passes too.
    fs::write(&path, &source).unwrap();
    append(
        &path,
        "\n#[cfg(feature = \"basic\")]\nfn old() -> u32 {\n    1\n}\n\n\
         pub fn value() -> u32 {\n    old()\n}\n\n\
         #[test]\nfn positive() {\n    assert!(value() > 0);\n}\n",
    );
    git_output(&dir, &["add", "-A"]);
    let base = git_output(&dir, &["write-tree"]);
    let with_value = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        with_value
            .replace(
                "#[cfg(feature = \"basic\")]\nfn old() -> u32 {\n    1\n}",
                "#[cfg(feature = \"extra\")]\nfn old() -> u32 {\n    missing()\n}",
            )
            .replace("    old()\n", "    2\n"),
    )
    .unwrap();
    let run = tmt(&dir, &["--json", "--quiet", "--rebuild", "--since", &base]);
    let since: Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}{}", run.stdout, run.stderr));
    let undone: Vec<&str> = since["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| !f["change_since"].is_null())
        .map(|f| f["survivors"][0].as_str().unwrap())
        .collect();
    assert_eq!(
        undone,
        ["with all 3 changes to this file undone"],
        "{since:#}"
    );
    assert_eq!(run.code, 2, "{since:#}");
    // Two `cfg` on one statement, both reworded: either alone leaves it out as it is
    // now, both together put it back. And next to a change the tests find out, a
    // change of code that is left out asks for nothing more.
    fs::write(&path, &source).unwrap();
    append(
        &path,
        "\npub fn twice() -> u32 {\n    #[cfg(feature = \"basic\")]\n    #[allow(unreachable_code)]\n    \
         #[cfg(feature = \"basic\")]\n    return 9;\n    #[allow(unreachable_code)]\n    2\n}\n\n\
         #[test]\nfn twice_is_something() {\n    assert!(twice() > 0);\n}\n",
    );
    git_output(&dir, &["add", "-A"]);
    let base = git_output(&dir, &["write-tree"]);
    let with_twice = fs::read_to_string(&path).unwrap();
    let undone_of = |text: String| -> (i32, Vec<String>) {
        fs::write(&path, text).unwrap();
        let run = tmt(&dir, &["--json", "--quiet", "--rebuild", "--since", &base]);
        let report: Value = serde_json::from_str(&run.stdout)
            .unwrap_or_else(|e| panic!("{e}: {}{}", run.stdout, run.stderr));
        let undone = report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| !f["change_since"].is_null())
            .map(|f| f["survivors"][0].as_str().unwrap().to_string())
            .collect();
        (run.code, undone)
    };
    assert_eq!(
        undone_of(with_twice.replace(
            "    #[cfg(feature = \"basic\")]\n    #[allow(unreachable_code)]\n    #[cfg(feature = \"basic\")]\n    return 9;",
            "    #[cfg(feature = \"extra\")]\n    #[allow(unreachable_code)]\n    #[cfg(feature = \"extra\")]\n    return 9;",
        )),
        (2, vec!["with all 2 changes to this file undone".to_string()])
    );
    assert_eq!(
        undone_of(
            with_twice
                .replace("n > 5", "n > 6")
                .replace("assert!(!super::basic(5));", "assert!(!super::basic(6));")
                .replace("assert!(super::basic(6));", "assert!(super::basic(7));")
                .replace("n > 10", "n > 11")
        )
        .1,
        Vec::<String>::new()
    );
    fs::write(&path, &source).unwrap();

    // With the feature the same code is judged, and found far from its limit.
    let run = tmt(
        &dir,
        &["--json", "--quiet", "--source-only", "--features", "extra"],
    );
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(report["mutants_not_compiled"], 0, "{report:#}");
    assert!(
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["code"] == "n > 10"),
        "{report:#}"
    );
}

#[test]
fn a_finding_is_known_by_where_it_is() {
    let repository = Path::new(env!("CARGO_TARGET_TMPDIR")).join("twins-repository");
    let _ = fs::remove_dir_all(&repository);
    let dir = repository.join("twins");
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/twins");
    copy(&source, &dir);
    fs::write(repository.join(".gitignore"), "target/\n").unwrap();
    git_output(&repository, &["init", "-q"]);
    let ids = |report: &Value| -> Vec<String> {
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["id"].as_str().unwrap().to_string())
            .collect()
    };
    let (code, report) = json(&dir);
    assert_eq!(code, 2, "{report:#}");
    // The same code four times: told apart by the function, and within one by the order.
    assert_eq!(
        ids(&report),
        [
            "input_gap|src/lib.rs|first|a > b|0",
            "input_gap|src/lib.rs|second|a > b|0",
            "input_gap|src/lib.rs|Pair::both|a > b|0",
            "input_gap|src/lib.rs|Pair::both|a > b|1",
        ],
        "{report:#}"
    );
    assert_eq!(report["findings"][2]["item"], "Pair::both");

    // SARIF carries the name, and the path from the top of the repository.
    let sarif = tmt(&dir, &["--sarif", "--quiet"]);
    let sarif: Value = serde_json::from_str(&sarif.stdout).unwrap();
    let result = &sarif["runs"][0]["results"][3];
    assert_eq!(
        result["partialFingerprints"]["tmtFindingId/v2"],
        "input_gap|src/lib.rs|Pair::both|a > b|1"
    );
    let uri = result["locations"][0]["physicalLocation"]["artifactLocation"]["uri"]
        .as_str()
        .unwrap();
    assert_eq!(uri, "twins/src/lib.rs");

    // Known findings stay known when the code moves and a new twin appears; only the
    // new one is reported.
    let baseline = dir.join("known.json");
    let baseline_arg = baseline.to_str().unwrap();
    tmt(&dir, &["--quiet", "--write-baseline", baseline_arg]);
    let written: Value = serde_json::from_str(&fs::read_to_string(&baseline).unwrap()).unwrap();
    assert_eq!(written["version"], 2);
    let source = fs::read_to_string(dir.join("src/lib.rs")).unwrap();
    let moved = source.replace(
        "pub fn first(",
        "pub fn zeroth(a: u32, b: u32) -> bool {\n    a > b\n}\n\n\npub fn first(",
    );
    let moved = moved.replace(
        "assert!(first(9, 1));",
        "assert!(zeroth(9, 1));\n        assert!(!zeroth(1, 9));\n        assert!(first(9, 1));",
    );
    fs::write(dir.join("src/lib.rs"), moved).unwrap();
    let run = tmt(&dir, &["--json", "--quiet", "--baseline", baseline_arg]);
    let after: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(
        ids(&after),
        ["input_gap|src/lib.rs|zeroth|a > b|0"],
        "{after:#}"
    );
    assert_eq!(after["suppressed"], 4, "{after:#}");
}

#[test]
fn an_audit_runs_survivors_against_all_tests() {
    let dir = fixture("namesake", "namesake");
    // A thread with the name of another test: what it sees is taken for that test's,
    // and the mutants are run against the wrong one. They seem to survive.
    let (code, report) = json(&dir);
    assert_eq!(code, 2, "{report:#}");
    assert_eq!(findings(&report, "assertion_gap").len(), 3, "{report:#}");
    assert!(report.get("audit").is_none(), "{report:#}");

    // Run against every test, four of them fail the test that does the work.
    let run = tmt(&dir, &["--json", "--quiet", "--audit", "100"]);
    let audited: Value = serde_json::from_str(&run.stdout).unwrap();
    let audit = &audited["audit"];
    assert_eq!(audit["survivors"], 7, "{audited:#}");
    assert_eq!(audit["sampled"], 7, "{audited:#}");
    assert_eq!(
        audit["disagreements"].as_array().unwrap().len(),
        4,
        "{audited:#}"
    );
    assert_eq!(audit["trace_differences"].as_array().unwrap().len(), 0);
    // The verdict of all tests counts: the false findings are gone, the true one stays.
    assert_eq!(findings(&audited, "assertion_gap").len(), 0, "{audited:#}");
    assert_eq!(findings(&audited, "input_gap").len(), 1, "{audited:#}");
    assert_eq!(
        audited["mutants_killed"].as_u64().unwrap(),
        report["mutants_killed"].as_u64().unwrap() + 4
    );

    // A smaller sample is taken evenly, and is the same every time.
    let sample = |n: &str| -> Value {
        let run = tmt(&dir, &["--json", "--quiet", "--audit", n]);
        serde_json::from_str::<Value>(&run.stdout).unwrap()["audit"].clone()
    };
    let two = sample("2");
    assert_eq!(two["sampled"], 2, "{two:#}");
    assert_eq!(two, sample("2"));
    assert_eq!(sample("0")["sampled"], 0);
    // Without the re-run of predicted survivors there is nothing to stand on.
    let bad = tmt(&dir, &["--quiet", "--audit", "1", "--no-verify"]);
    assert!(bad.stderr.contains("--no-verify"), "{}", bad.stderr);
}

#[test]
fn mutants_are_run_as_what_they_are() {
    let dir = fixture("revisit", "revisit");
    let (code, report) = json(&dir);
    // The tests pass under the probes at all: `Level` has a `max` of its own (never above
    // nine), and that one is still the one called.
    assert_eq!(code, 2, "{report:#}");
    let survivors = |code: &str| -> Vec<&str> {
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["code"] == code && f["kind"] == "assertion_gap")
            .flat_map(|f| strings(&f["survivors"]))
            .collect()
    };
    // `>=` and `>` in place of `<` answer alike for the one value the test starts with.
    // Both are run: the second time round they see different values, and `>` passes.
    assert_eq!(survivors("x < 1"), ["`x > 1`"], "{report:#}");
    // `from..=to` cannot be made of `from..to` where `to` is the largest value there is:
    // the mutant is not run and not reported as passing.
    // The same where the tests only get to the largest value with the mutant on
    // (`last_count`): that run is given up, not taken for a pass.
    assert_eq!(report["mutants_not_evaluated"], 2, "{report:#}");
    assert!(survivors("from..to").is_empty(), "{report:#}");
    assert!(survivors("end..end").is_empty(), "{report:#}");
    // `Gauge` cannot be copied and says when it is dropped; its `max` only looks at it.
    // The probe borrows it as the call did (the test would see it dropped otherwise),
    // and it is a probe: nothing was left out for not compiling.
    assert_eq!(report["skipped_sites"], 0, "{report:#}");
    // A float can be what is not a number, for which `y < 0.0` is false and "not
    // `y >= 0.0`" true. The test only gets there once the comparison has answered
    // otherwise, so nothing recorded shows it; `<` is run all the same, and passes.
    assert_eq!(survivors("y >= 0.0"), ["`y < 0.0`"], "{report:#}");
    let text = tmt(&dir, &["--quiet"]).stdout;
    assert!(text.contains("(2 mutants not evaluated: a range"), "{text}");

    // A range that is looked at instead of run through (`takes_its_end_in` asks for its
    // `end_bound`) is no probe at all: `0..end + 1` would hold the numbers of `0..=end`
    // and still not be it. It waits for a build with the two that were not evaluated.
    // So do the two `0..2` of the loops: two literals have no type a probe could take.
    assert_eq!(report["mutants_not_built"], 5, "{report:#}");
    assert!(survivors("0u8..end").is_empty(), "{report:#}");
    // Built from the source, the three are what they say, and each fails its test. The
    // two loops run a third time without the tests noticing: those two pass.
    let run = tmt(&dir, &["--json", "--quiet", "--rebuild"]);
    let built: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(built["mutants_not_evaluated"], 0, "{built:#}");
    assert_eq!(built["mutants_not_built"], 0, "{built:#}");
    for (count, more) in [("mutants_run", 5), ("mutants_killed", 3)] {
        assert_eq!(
            built[count].as_u64().unwrap(),
            report[count].as_u64().unwrap() + more,
            "{count}: {built:#}"
        );
    }
    let passed: Vec<&str> = built["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["kind"] == "survivor")
        .map(|f| f["code"].as_str().unwrap())
        .collect();
    assert_eq!(passed, ["0..2", "0..2"], "{built:#}");
    assert_eq!(
        built["findings"].as_array().unwrap().len(),
        report["findings"].as_array().unwrap().len() + 2,
        "{built:#}"
    );
}

#[test]
fn a_part_of_a_file_numbers_its_findings_as_the_whole_does() {
    let dir = fixture("twins", "twins-part");
    // Only the line of the second of the two comparisons in `Pair::both`.
    let run = tmt(&dir, &["--json", "--quiet", "--lines", "src/lib.rs:16"]);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    let ids: Vec<&str> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["id"].as_str().unwrap())
        .collect();
    // It is still the second: a baseline of the whole file knows it as that.
    assert_eq!(
        ids,
        ["input_gap|src/lib.rs|Pair::both|a > b|1"],
        "{report:#}"
    );
}

/// What a probe does and what the same change does in the source agree, mutant by mutant.
fn probes_agree_with_the_source(names: &[&str]) {
    for name in names {
        let dir = fixture(name, &format!("audit-{name}"));
        let run = tmt(&dir, &["--json", "--quiet", "--audit", "1000"]);
        let report: Value = serde_json::from_str(&run.stdout)
            .unwrap_or_else(|e| panic!("{name}: {e}: {}{}", run.stdout, run.stderr));
        let audit = &report["audit"];
        let count = |what: &str| audit[what].as_u64().unwrap();
        assert!(count("rebuilt") >= 10, "{name}: {audit:#}");
        // Every one of them is accounted for, and most could be compared.
        assert_eq!(
            count("compared") + count("not_compared"),
            count("rebuilt"),
            "{name}"
        );
        assert!(
            count("compared") * 10 >= count("rebuilt") * 9,
            "{name}: {audit:#}"
        );
        assert_eq!(
            audit["source_disagreements"].as_array().unwrap().len(),
            0,
            "{name}: {audit:#}"
        );
        assert_eq!(
            audit["disagreements"].as_array().unwrap().len(),
            0,
            "{name}: {audit:#}"
        );
        // The built ones are only compared: the counts are those of a run without them.
        let (_, plain) = json(&dir);
        assert_eq!(report["mutants_run"], plain["mutants_run"], "{name}");
        assert_eq!(report["findings"], plain["findings"], "{name}");
    }
}

/// The same on the fixtures made of the constructs a probe has to be careful with. It
/// builds some two hundred mutants: `cargo test -- --ignored`.
#[test]
#[ignore = "builds every mutant of four fixtures; takes minutes"]
fn probes_agree_with_the_source_on_the_larger_fixtures() {
    probes_agree_with_the_source(&["operators", "semantics", "shapes", "kitchen-sink"]);
}

#[test]
fn an_audit_builds_mutants_from_the_source_and_compares() {
    probes_agree_with_the_source(&["revisit"]);
    // One of `revisit` cannot be compared: the range whose probe gives up at the largest
    // value. Built from the source it is found out, which the probe could not show.
    let dir = fixture("revisit", "audit-revisit-counts");
    let run = tmt(&dir, &["--json", "--quiet", "--audit", "1000"]);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(report["audit"]["not_compared"], 1, "{report:#}");

    // A guard that does something: taken for `true` as a probe it is still evaluated,
    // replaced by `true` in the source it is not, and the test notices.
    let dir = fixture("guarded", "audit-guarded");
    let run = tmt(&dir, &["--json", "--quiet", "--audit", "1000"]);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    let differ = report["audit"]["source_disagreements"].as_array().unwrap();
    assert_eq!(differ.len(), 1, "{report:#}");
    assert_eq!(differ[0]["code"], "note(asked)");
    assert_eq!(
        differ[0]["mutant"],
        "the guard `note(asked)` replaced by `true`"
    );
    assert_eq!(
        (differ[0]["as_probe"].as_str(), differ[0]["built"].as_str()),
        (Some("passes"), Some("fails"))
    );
    let text = tmt(&dir, &["--quiet", "--audit", "1000"]).stdout;
    assert!(
        text.contains("the guard `note(asked)` replaced by `true`: passes as a probe, fails built"),
        "{text}"
    );
}

#[test]
fn a_run_given_up_inside_a_program_a_test_started_is_not_a_failed_test() {
    let dir = fixture("spawned", "spawned");
    // The range is only run by the program the test starts, and the test only asks
    // whether that program ends well. With the end of the range taken in, the program
    // gets to a range no probe can make and stops; the test fails for it. That is not
    // the mutant found out: it is the mutant not run.
    let (_, report) = json(&dir);
    assert_eq!(report["mutants_not_evaluated"], 1, "{report:#}");
    let at_the_range = |report: &Value| -> Vec<String> {
        report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["code"] == "end..end")
            .map(|f| f["kind"].as_str().unwrap().to_string())
            .collect()
    };
    assert!(at_the_range(&report).is_empty(), "{report:#}");
    // Built from the source it is run for what it is, and passes: the program ends well
    // whatever it prints.
    let run = tmt(&dir, &["--json", "--quiet", "--rebuild"]);
    let built: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(built["mutants_not_evaluated"], 0, "{built:#}");
    assert_eq!(at_the_range(&built), ["survivor"], "{built:#}");
    assert_eq!(
        built["mutants_killed"], report["mutants_killed"],
        "{built:#}"
    );

    // A test that ends the test program itself, with the very status the runtime uses,
    // has found its mutant out: the mark tells the two apart, not the status.
    let at_the_product: Vec<&Value> = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["code"] == "x * 2")
        .collect();
    assert!(at_the_product.is_empty(), "{report:#}");

    // The mark is found wherever the temporary directory is said to be, also where that
    // is said from the directory the run was started in.
    let output = fixture_command(env!("CARGO_BIN_EXE_cargo-tmt"))
        .arg(".")
        .args(["--json", "--quiet"])
        .current_dir(&dir)
        .env("TMPDIR", ".")
        .output()
        .unwrap();
    let relative: Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("{e}: {}", String::from_utf8_lossy(&output.stderr)));
    assert_eq!(relative["mutants_not_evaluated"], 1, "{relative:#}");
    assert_eq!(
        relative["mutants_killed"], report["mutants_killed"],
        "{relative:#}"
    );
}

#[test]
fn started_by_cargo_as_one_of_its_subcommands() {
    let dir = fixture("eligibility", "as-subcommand");
    // `cargo tmt` finds `cargo-tmt` on the path and gives it its own name first.
    let exe = Path::new(env!("CARGO_BIN_EXE_cargo-tmt"));
    let mut path = vec![exe.parent().unwrap().to_path_buf()];
    path.extend(std::env::split_paths(&std::env::var_os("PATH").unwrap()));
    let output = fixture_command("cargo")
        .args(["tmt", "--json", "--quiet"])
        .current_dir(&dir)
        .env("PATH", std::env::join_paths(path).unwrap())
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["package"], "eligibility");
    assert_eq!(findings(&report, "input_gap").len(), 1, "{report:#}");
    // Started by its own name it is the same, and it says how it is called.
    let (_, direct) = json(&dir);
    assert_eq!(direct["findings"], report["findings"]);
    let help = fixture_command(exe).arg("--help").output().unwrap();
    assert!(
        String::from_utf8_lossy(&help.stdout).contains("Usage: cargo tmt"),
        "{}",
        String::from_utf8_lossy(&help.stdout)
    );
}

#[test]
fn a_child_without_probe_environment_can_be_tested_from_source() {
    let dir = fixture("spawned", "env-clear");
    let child_test = dir.join("tests/cli.rs");
    fs::write(
        dir.join("src/main.rs"),
        r#"fn main() {
    let n: u32 = std::env::args().nth(1).unwrap().parse().unwrap();
    println!("{}", n <= 4);
}
"#,
    )
    .unwrap();
    fs::write(
        child_test,
        r#"#[test]
fn cli_boundary() {
    for (arg, want) in [("4", "true\n"), ("5", "false\n")] {
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_spawned"))
            .env_clear()
            .arg(arg)
            .output()
            .unwrap();
        assert!(result.status.success());
        assert_eq!(result.stdout, want.as_bytes());
    }
}
"#,
    )
    .unwrap();
    // A binary-only crate makes src/main.rs the mutation target.
    fs::remove_file(dir.join("src/lib.rs")).unwrap();
    fs::remove_file(dir.join("tests/exit.rs")).unwrap();
    let args = ["--json", "--quiet", "--lines", "src/main.rs:3"];
    let probe = tmt(&dir, &args);
    assert_eq!(probe.code, 3, "{}", probe.stderr);
    let report: Value = serde_json::from_str(&probe.stdout).unwrap();
    assert_eq!(report["evaluation"], "no_mutants_executed", "{report:#}");
    let text = tmt(&dir, &["--quiet", "--lines", "src/main.rs:3"]);
    assert_eq!(text.code, 3, "{}", text.stderr);
    assert!(
        text.stdout.contains("No mutants executed."),
        "{}",
        text.stdout
    );
    let rebuilt = tmt(&dir, &[args.as_slice(), &["--source-only"]].concat());
    assert_ne!(rebuilt.code, 1, "{}", rebuilt.stderr);
    let report: Value = serde_json::from_str(&rebuilt.stdout).unwrap();
    assert!(
        report["mutants_killed"].as_u64().unwrap() >= 1,
        "{report:#}"
    );
    assert_eq!(report["analysis"]["source_only"], true);
    assert_eq!(report["unreached"].as_array().unwrap().len(), 0);
    let test = dir.join("tests/cli.rs");
    let text = fs::read_to_string(&test).unwrap();
    fs::write(test, text.replace(r#"("4", "true"#, r#"("3", "true"#)).unwrap();
    let rebuilt = tmt(&dir, &[args.as_slice(), &["--source-only"]].concat());
    assert_eq!(rebuilt.code, 2, "{}", rebuilt.stderr);
    assert!(rebuilt.stdout.contains("n < 4"), "{}", rebuilt.stdout);
}

#[test]
fn test_target_build_scope_applies_to_probes_and_source_rebuilds() {
    for integration in [true, false] {
        for multiple in [false, true] {
            let dir = fixture("spawned", &format!("target-scope-{integration}-{multiple}"));
            fs::remove_file(dir.join("tests/exit.rs")).unwrap();
            fs::remove_file(dir.join("tests/cli.rs")).unwrap();
            let (target, lines, test) = if integration {
                fs::remove_file(dir.join("src/lib.rs")).unwrap();
                fs::write(
                    dir.join("src/main.rs"),
                    r#"fn main() {
    let n: u32 = std::env::args().nth(1).unwrap().parse().unwrap();
    println!("{}", n <= 4);
}
"#,
                )
                .unwrap();
                fs::write(
                    dir.join("tests/arguments.rs"),
                    r#"#[test]
fn arguments() {
    for (arg, want) in [("4", "true\n"), ("5", "false\n")] {
        let result = std::process::Command::new(env!("CARGO_BIN_EXE_spawned"))
            .arg(arg).output().unwrap();
        assert!(result.status.success());
        assert_eq!(result.stdout, want.as_bytes());
    }
}
"#,
                )
                .unwrap();
                (
                    "arguments",
                    "src/main.rs:3",
                    "spawned::test::arguments::arguments",
                )
            } else {
                fs::remove_file(dir.join("src/main.rs")).unwrap();
                fs::write(
                    dir.join("src/lib.rs"),
                    r#"pub fn allowed(n: u32) -> bool {
    n <= 4
}

fn late_init(flag: bool) -> u32 {
    let value;
    value = if flag { 1 } else { 2 };
    value
}

#[test]
fn boundary() {
    assert!(allowed(4));
    assert!(!allowed(5));
    assert_eq!(late_init(true), 1);
}
"#,
                )
                .unwrap();
                ("spawned", "src/lib.rs:2", "spawned::lib::spawned::boundary")
            };
            let package_wide = !integration && multiple;
            if !package_wide {
                fs::write(
                    dir.join("tests/unselected.rs"),
                    "compile_error!(\"unselected artifact build sentinel\");\n",
                )
                .unwrap();
            }
            let mut args = vec![
                "--json",
                "--quiet",
                "--lines",
                lines,
                "--test-target",
                target,
            ];
            if multiple {
                append(&dir.join("Cargo.toml"), "members = [\"other\"]\n");
                fs::create_dir_all(dir.join("other/src")).unwrap();
                fs::write(
                    dir.join("other/Cargo.toml"),
                    "[package]\nname = \"other\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
                )
                .unwrap();
                fs::write(
                    dir.join("other/src/lib.rs"),
                    if integration {
                        "compile_error!(\"unselected package sentinel\");\n"
                    } else {
                        "#[test]\nfn unselected() { panic!(\"unselected test ran\"); }\n"
                    },
                )
                .unwrap();
                args.extend(["--also-test", "other"]);
            }
            for source_only in [false, true] {
                let mut args = args.clone();
                if source_only {
                    args.push("--source-only");
                }
                let repairs_probes = !integration && !multiple && !source_only;
                if repairs_probes {
                    args.extend(["--lines", "src/lib.rs:7"]);
                }
                let run = tmt(&dir, &args);
                assert_ne!(run.code, 1, "{args:?}: {}", run.stderr);
                let report: Value = serde_json::from_str(&run.stdout).unwrap();
                let selection = &report["analysis"]["test_selection"];
                assert_eq!(
                    selection["build_scope"],
                    if package_wide {
                        "package_wide"
                    } else {
                        "selected_targets"
                    },
                    "{args:?}: {report:#}"
                );
                assert_eq!(strings(&selection["tests"]), [test]);
                assert!(
                    report["mutants_killed"].as_u64().unwrap() >= 2,
                    "{report:#}"
                );
                if repairs_probes {
                    assert!(
                        report["skipped"].as_array().unwrap().iter().any(|site| {
                            site["reason"] == "probe_compile_error" && site["line"] == 7
                        }),
                        "{report:#}"
                    );
                }
            }
            if package_wide {
                fs::write(
                    dir.join("tests/unselected.rs"),
                    "compile_error!(\"fallback sentinel\");\n",
                )
                .unwrap();
                let run = tmt(&dir, &args);
                assert_eq!(run.code, 1, "{}", run.stderr);
                assert!(run.stderr.contains("package-wide"), "{}", run.stderr);
                assert!(run.stderr.contains("fallback sentinel"), "{}", run.stderr);
            }
        }
    }
}

#[test]
fn test_target_with_required_features_is_built_when_they_are_enabled() {
    let dir = fixture("eligibility", "target-required-features");
    fs::create_dir_all(dir.join("tests")).unwrap();
    append(
        &dir.join("Cargo.toml"),
        r#"
[features]
default = ["extra"]
extra = []

[[test]]
name = "-chosen"
path = "tests/chosen.rs"
required-features = ["extra"]
"#,
    );
    fs::write(
        dir.join("tests/chosen.rs"),
        "#[test]\nfn boundary() { assert!(eligibility::is_eligible(18)); }\n",
    )
    .unwrap();
    let args = ["--json", "--quiet", "--test-target=-chosen"];
    let disabled = tmt(
        &dir,
        &[args.as_slice(), &["--no-default-features"]].concat(),
    );
    assert_eq!(disabled.code, 1, "{}", disabled.stderr);
    assert!(
        disabled.stderr.contains("no tests matched"),
        "{}",
        disabled.stderr
    );

    fs::write(
        dir.join("tests/unselected.rs"),
        "compile_error!(\"unselected\");\n",
    )
    .unwrap();
    for features in [
        &[][..],
        &["--no-default-features", "--features", "extra"],
        &["--no-default-features", "--features", "eligibility/extra"],
        &["--all-features"],
    ] {
        let run = tmt(&dir, &[args.as_slice(), features].concat());
        assert_ne!(run.code, 1, "{features:?}: {}", run.stderr);
        let report: Value = serde_json::from_str(&run.stdout).unwrap();
        let selection = &report["analysis"]["test_selection"];
        assert_eq!(selection["build_scope"], "selected_targets", "{report:#}");
        assert_eq!(
            strings(&selection["tests"]),
            ["eligibility::test::-chosen::boundary"]
        );
    }
}

#[cfg(unix)]
#[test]
fn selected_tests_run_in_their_own_process_and_use_a_wrapper() {
    use std::os::unix::fs::PermissionsExt;

    let dir = fixture("eligibility", "isolated");
    fs::create_dir_all(dir.join("tests")).unwrap();
    fs::write(
        dir.join("tests/selected.rs"),
        r#"#[test]
fn selected() {
    assert_eq!(std::env::var("WRAPPED").as_deref(), Ok("yes"));
    let handle = std::thread::Builder::new().name("excluded".into())
        .spawn(|| assert!(eligibility::is_eligible(20))).unwrap();
    handle.join().unwrap();
}
#[test]
fn excluded() {
    panic!("this test must not run");
}
#[test]
#[ignore]
fn extra() {
    assert!(!eligibility::is_eligible(17));
}
"#,
    )
    .unwrap();
    let wrapper = dir.join("wrapper.sh");
    fs::write(&wrapper, "#!/bin/sh\nexport WRAPPED=yes\nexec \"$@\"\n").unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let run = tmt(
        &dir,
        &[
            "--json",
            "--quiet",
            "--test-target",
            "selected",
            "--exclude-test",
            "excluded",
            "--include-ignored",
            "--test-wrapper",
            "wrapper.sh",
        ],
    );
    assert_ne!(run.code, 1, "{}", run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    let selection = &report["analysis"]["test_selection"];
    assert_eq!(selection["wrapper"], "wrapper.sh");
    assert_eq!(
        selection["tests"],
        serde_json::json!([
            "eligibility::test::selected::extra",
            "eligibility::test::selected::selected"
        ])
    );
    assert!(report["mutants_run"].as_u64().unwrap() > 0, "{report:#}");

    let missing = tmt(&dir, &["--quiet", "--include-test", "missing"]);
    assert_eq!(missing.code, 1);
    assert!(
        missing.stderr.contains("no tests matched"),
        "{}",
        missing.stderr
    );
}

#[test]
fn requested_git_index_and_ignored_fixtures_stay_in_the_copy() {
    let dir = fixture("eligibility", "requested-copy");
    fs::write(dir.join(".gitignore"), "target/\nnode_modules/\n").unwrap();
    fs::write(dir.join("tracked-but-deleted"), "tracked").unwrap();
    append(
        &dir.join("src/lib.rs"),
        r#"
#[test]
fn git_index_is_available() {
    let output = std::process::Command::new("git")
        .args(["ls-files", "--error-unmatch", "Cargo.toml", "tracked-but-deleted"])
        .output().unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(std::fs::read_to_string("node_modules/sdk/data.txt").unwrap(), "fixture");
    assert!(!std::path::Path::new("node_modules/unrequested").exists());
}
"#,
    );
    git_output(&dir, &["init", "-q"]);
    git_output(&dir, &["add", "-A"]);
    fs::remove_file(dir.join("tracked-but-deleted")).unwrap();
    fs::create_dir_all(dir.join("node_modules/sdk/empty")).unwrap();
    fs::write(dir.join("node_modules/sdk/data.txt"), "fixture").unwrap();
    fs::write(dir.join("node_modules/unrequested"), "exclude me").unwrap();
    let work = dir.with_file_name("requested-work");
    let work_arg = work.to_str().unwrap();
    let args = ["--json", "--quiet", "--work-dir", work_arg];
    let good = tmt(
        &dir,
        &[
            args.as_slice(),
            &["--git-index", "--copy-fixture", "node_modules/sdk"],
        ]
        .concat(),
    );
    let _: Value = serde_json::from_str(&good.stdout)
        .unwrap_or_else(|error| panic!("{error}: {}", good.stderr));
    assert!(work.join("src/.git/index").is_file());
    assert!(work.join("src/node_modules/sdk/empty").is_dir());
    assert!(!work.join("src/node_modules/unrequested").exists());
    let bad = tmt(&dir, &args);
    assert_eq!(bad.code, 1, "{}", bad.stderr);
    assert!(!work.join("src/.git").exists());
    assert!(!work.join("src/node_modules").exists());
    assert!(dir.join(".git/index").is_file());
}

#[test]
fn progress_stays_on_stderr_and_respects_quiet() {
    let dir = fixture("eligibility", "mutant-progress");
    let run = tmt(&dir, &["--json", "--rebuild"]);
    assert_ne!(run.code, 1, "{}", run.stderr);
    let _: Value = serde_json::from_str(&run.stdout).unwrap();
    assert!(run.stderr.contains("mutants: ") && run.stderr.contains("completed ("));
    assert!(
        run.stderr.contains("failed")
            && run.stderr.contains("timed out")
            && run.stderr.contains("survived")
    );
    let quiet = tmt(&dir, &["--json", "--rebuild", "--quiet"]);
    assert_eq!(quiet.stderr, "");
    assert_eq!(quiet.stdout, run.stdout);
}

#[test]
fn a_custom_harness_can_reject_listing_in_default_mode() {
    let dir = fixture("eligibility", "custom-harness");
    append(
        &dir.join("Cargo.toml"),
        "\n[[test]]\nname = \"custom\"\nharness = false\n",
    );
    fs::create_dir_all(dir.join("tests")).unwrap();
    fs::write(
        dir.join("tests/custom.rs"),
        r#"
fn main() {
    if std::env::args().any(|arg| arg == "--list") { std::process::exit(2); }
    assert!(eligibility::is_eligible(20));
}
"#,
    )
    .unwrap();
    let run = tmt(&dir, &["--json", "--quiet"]);
    assert_ne!(run.code, 1, "{}", run.stderr);
    let _: Value = serde_json::from_str(&run.stdout).unwrap();
}

#[test]
fn an_unparseable_selection_is_not_a_successful_empty_selection() {
    let dir = fixture("eligibility", "skipped-bom");
    let source = dir.join("src/lib.rs");
    let old = fs::read_to_string(&source).unwrap();
    fs::write(source, format!("\u{feff}{old}")).unwrap();
    let run = tmt(&dir, &["--lines", "src/lib.rs", "--json", "--quiet"]);
    assert_eq!(run.code, 3, "{}\n{}", run.stdout, run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(report["evaluation"], "no_mutants_executed");
    assert_eq!(report["skipped"][0]["reason"], "unparseable_source");
}

/// A child made by `fork` has the locks of the runtime as they were at that instant,
/// perhaps held by a thread it does not have. What it runs before `exec` records nothing
/// (and waits for nothing): it is reported as not reached, while a mutant still acts there.
#[cfg(unix)]
#[test]
fn code_between_fork_and_exec_is_not_traced() {
    let dir = fixture("forking", "forking");
    let (code, report) = json(&dir);
    assert_eq!(code, 0, "{report:#}");
    assert_eq!(report["skipped_sites"], 0, "{report:#}");
    assert_eq!(findings(&report, "survivor").len(), 0, "{report:#}");
    let unreached: Vec<&str> = report["unreached"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["code"].as_str().unwrap())
        .collect();
    assert_eq!(unreached, ["level > HIGHEST"], "{report:#}");
    assert_eq!(
        report["mutants_run"], report["mutants_killed"],
        "{report:#}"
    );
}

/// A mutant makes a test fail before it stops the helper it started in a process group
/// of its own. The run passes a locked file on to whatever it starts, and whoever still
/// has it after the run is stopped.
#[cfg(unix)]
#[test]
fn what_a_failed_test_left_running_in_another_group_is_stopped() {
    let dir = fixture("leaving", "leaving");
    let list = dir.join("helpers.txt");
    let output = fixture_command(env!("CARGO_BIN_EXE_cargo-tmt"))
        .arg(&dir)
        .arg("--json")
        .env("LEAVING_PIDS", &list)
        .output()
        .unwrap();
    let stderr = String::from_utf8(output.stderr).unwrap();
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(0), "{report:#}\n{stderr}");
    let killed = report["mutants_killed"].as_u64().unwrap();
    assert!(killed > 0, "{report:#}");
    assert_eq!(report["mutants_run"], killed, "{report:#}");

    let helpers: Vec<String> = fs::read_to_string(&list)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    // The unmutated runs stop their helper themselves; every mutant leaves one.
    assert!(helpers.len() as u64 > killed, "{helpers:?}");
    let running: Vec<&String> = helpers
        .iter()
        .filter(|pid| {
            let listed = std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", pid.as_str()])
                .output()
                .unwrap();
            let state = String::from_utf8_lossy(&listed.stdout);
            !state.trim().is_empty() && !state.trim().starts_with('Z')
        })
        .collect();
    for pid in &running {
        let _ = std::process::Command::new("kill")
            .args(["-9", pid.as_str()])
            .status();
    }
    assert!(running.is_empty(), "left running: {running:?}\n{stderr}");
    let note = stderr
        .lines()
        .find(|line| line.starts_with("tmt: stopped "))
        .unwrap_or_else(|| panic!("{stderr}"));
    assert!(
        note.ends_with(" processes that the tests had left running after their run"),
        "{note}"
    );
    let stopped: u64 = note.split(' ').nth(2).unwrap().parse().unwrap();
    assert!(stopped >= killed, "{note}");
}

/// One function and one test for a row of `tests/data/slice_ends.pict.tsv`: the call, on
/// a line of its own, and what is expected there.
struct EndRow {
    line: usize,
    receiver: &'static str,
    other: &'static str,
    expected: Option<&'static str>,
    /// An assertion on a sequence of two different elements.
    closing: String,
    label: String,
}

/// A call of a row's function with these elements.
type Call = Box<dyn Fn(&str) -> String>;

fn slice_end_crate(table: &str) -> (String, Vec<EndRow>) {
    let mut source = String::from(
        "pub struct Held { pub items: Vec<u32> }\n\
         pub struct Own(pub Vec<u32>);\n\
         impl Own {\n\
         pub fn first(&self) -> Option<u32> { self.0.get(0).copied() }\n\
         pub fn last(&self) -> Option<u32> { self.0.iter().next_back().copied() }\n\
         }\n",
    );
    let mut tests = String::new();
    let mut rows = vec![];
    for (n, row) in table.lines().skip(1).enumerate() {
        let cells: Vec<&str> = row.split('\t').collect();
        let [method, receiver, seen] = cells[..] else {
            panic!("{row}")
        };
        let other = if method == "first" { "last" } else { "first" };
        let inputs: &[(&[u32], Option<u32>)] = match (seen, method) {
            ("none", _) => &[],
            ("empty", _) => &[(&[], None)],
            ("one", _) => &[(&[7], Some(7))],
            ("empty_and_one", _) => &[(&[], None), (&[7], Some(7))],
            ("many", "first") => &[(&[10, 20], Some(10))],
            ("many", _) => &[(&[10, 20], Some(20))],
            ("many_same_ends", _) => &[(&[5, 5], Some(5))],
            _ => panic!("{row}"),
        };
        let length = if seen == "one" { 1 } else { 2 };
        // (the receiver as written, the function's head, its body, how a test calls it)
        let (written, head, body, call): (&str, String, String, Call) = match receiver {
            "slice" => (
                "v",
                format!("pub fn row_{n}(v: &[u32]) -> Option<u32> {{"),
                format!("v.{method}().copied()"),
                Box::new(move |items| format!("row_{n}(&[{items}])")),
            ),
            "vec" => (
                "v",
                format!("pub fn row_{n}(v: Vec<u32>) -> Option<u32> {{"),
                format!("v.{method}().copied()"),
                Box::new(move |items| format!("row_{n}(vec![{items}])")),
            ),
            "array" => (
                "v",
                format!("pub fn row_{n}(v: [u32; {length}]) -> Option<u32> {{"),
                format!("v.{method}().copied()"),
                Box::new(move |items| format!("row_{n}([{items}])")),
            ),
            "set" => (
                "v",
                format!("pub fn row_{n}(v: &std::collections::BTreeSet<u32>) -> Option<u32> {{"),
                format!("v.{method}().copied()"),
                Box::new(move |items| {
                    format!("row_{n}(&std::collections::BTreeSet::from([{items}]))")
                }),
            ),
            "field" => (
                "self.items",
                format!("impl Held {{ pub fn row_{n}(&self) -> Option<u32> {{"),
                format!("self.items.{method}().copied() }}"),
                Box::new(move |items| format!("Held {{ items: vec![{items}] }}.row_{n}()")),
            ),
            "mut_ref" => (
                "v",
                format!("pub fn row_{n}(v: &mut Vec<u32>) -> Option<u32> {{"),
                format!("v.{method}().copied()"),
                Box::new(move |items| format!("row_{n}(&mut vec![{items}])")),
            ),
            "own_type" => (
                "v",
                format!("pub fn row_{n}(v: &Own) -> Option<u32> {{"),
                format!("v.{method}()"),
                Box::new(move |items| format!("row_{n}(&Own(vec![{items}]))")),
            ),
            "iterator" => (
                "v.iter()",
                format!("pub fn row_{n}(v: &[u32]) -> Option<u32> {{"),
                format!("v.iter().{method}().copied()"),
                Box::new(move |items| format!("row_{n}(&[{items}])")),
            ),
            _ => panic!("{row}"),
        };
        source.push_str(&head);
        source.push('\n');
        let line = source.lines().count() + 1;
        source.push_str(&body);
        source.push_str("\n}\n");
        if !inputs.is_empty() {
            tests.push_str(&format!("#[test]\nfn test_row_{n}() {{\n"));
            for (items, result) in inputs {
                let items: Vec<String> = items.iter().map(u32::to_string).collect();
                tests.push_str(&format!(
                    "assert_eq!({}, {result:?});\n",
                    call(&items.join(", "))
                ));
            }
            tests.push_str("}\n");
        }
        // A sequence of this crate's own, an iterator and a `&mut` are left alone, and an
        // array of one element has only one end.
        let plain = matches!(receiver, "slice" | "vec" | "set" | "field" | "array");
        let expected = match seen {
            "empty" | "one" | "empty_and_one" if plain && receiver != "array" => Some("input_gap"),
            "many_same_ends" if plain => Some("assertion_gap"),
            _ => None,
        };
        rows.push(EndRow {
            line,
            receiver: written,
            other,
            expected,
            closing: format!(
                "assert_eq!({}, Some({}));\n",
                call("10, 20"),
                if method == "first" { 10 } else { 20 }
            ),
            label: format!("row {n} ({row})"),
        });
    }
    source.push_str(&tests);
    (source, rows)
}

#[test]
fn slice_end_table() {
    let table = include_str!("data/slice_ends.pict.tsv");
    let (source, rows) = slice_end_crate(table);
    let dir = fixture("eligibility", "slice-ends");
    fs::write(dir.join("src/lib.rs"), &source).unwrap();
    let (code, report) = json(&dir);
    assert_eq!(code, 2, "{report:#}");
    assert_eq!(report["skipped_sites"], 0, "{report:#}");
    assert_eq!(
        report["unstable"].as_array().unwrap().len(),
        0,
        "{report:#}"
    );
    let all = report["findings"].as_array().unwrap();
    for row in &rows {
        let found: Vec<&Value> = all.iter().filter(|f| f["line"] == row.line).collect();
        let kinds: Vec<&str> = found.iter().map(|f| f["kind"].as_str().unwrap()).collect();
        let expected: Vec<&str> = row.expected.into_iter().collect();
        assert_eq!(kinds, expected, "{}: {found:#?}", row.label);
        let Some(finding) = found.first() else {
            continue;
        };
        assert_eq!(
            strings(&finding["survivors"]),
            [format!("`{}.{}()`", row.receiver, row.other)],
            "{}",
            row.label
        );
        if row.expected == Some("input_gap") {
            assert_eq!(
                strings(&finding["add_tests"]),
                [format!(
                    "`{}` with two or more elements, the first different from the last",
                    row.receiver
                )],
                "{}",
                row.label
            );
            assert_eq!(finding["verified"], true, "{}", row.label);
        }
    }

    // Written into the source, the other end passes the same tests.
    let gap = rows
        .iter()
        .find(|row| row.expected == Some("input_gap"))
        .unwrap();
    let lines = format!("src/lib.rs:{}", gap.line);
    let built = tmt(
        &dir,
        &["--json", "--quiet", "--source-only", "--lines", &lines],
    );
    let built: Value = serde_json::from_str(&built.stdout).unwrap();
    let survivors = findings(&built, "survivor");
    assert_eq!(survivors.len(), 1, "{built:#}");
    assert_eq!(survivors[0]["line"], gap.line, "{built:#}");
    assert_eq!(
        strings(&survivors[0]["survivors"]),
        [format!("`{}.{}()`", gap.receiver, gap.other)],
        "{built:#}"
    );

    // A test with two different ends closes every input gap.
    let closing: String = rows
        .iter()
        .filter(|row| row.expected == Some("input_gap"))
        .map(|row| row.closing.as_str())
        .collect();
    append(
        &dir.join("src/lib.rs"),
        &format!("#[test]\nfn closes_the_gaps() {{\n{closing}}}\n"),
    );
    let (_, report) = json(&dir);
    for row in rows.iter().filter(|row| row.expected == Some("input_gap")) {
        let left: Vec<&Value> = report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|f| f["line"] == row.line)
            .collect();
        assert!(left.is_empty(), "{}: {left:#?}", row.label);
    }
}

#[test]
fn value_aware_method_rules_find_input_gaps_and_rebuild_literal_sets() {
    let dir = fixture("eligibility", "value-aware-methods");
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"value-aware\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("src/lib.rs"),
        r#"
pub fn has_prefix(s: &str) -> bool {
    s.starts_with("api/")
}

pub fn normalize_sep(s: &str) -> String {
    s.replace("-", "_")
}

pub fn bounded(x: i32) -> i32 {
    x.clamp(0, 100)
}

pub fn whole(x: f64) -> f64 {
    x.floor()
}

pub fn all_positive(xs: &[i32]) -> bool {
    xs.iter().all(|&x| x > 0)
}

pub fn reuse_iter_after_all(xs: &[i32]) -> Option<i32> {
    let mut it = xs.iter().copied();
    let _ = it.all(|x| x > 0);
    it.next()
}

pub fn check_ref_iter(it: &mut std::slice::Iter<'_, i32>) -> bool {
    it.all(|&x| x > 0)
}

pub fn reuse_copy_iter_after_any() -> Option<i32> {
    #[derive(Clone, Copy)]
    struct Pair(u8);
    impl Iterator for Pair {
        type Item = i32;
        fn next(&mut self) -> Option<i32> {
            let cur = self.0;
            if cur >= 2 {
                return None;
            }
            self.0 += 1;
            Some(cur as i32)
        }
    }
    let mut it = Pair(0);
    let _ = it.any(|x| x == 0);
    it.next()
}

pub fn clamp_panic_guard(x: i32, lo: i32, hi: i32) -> i32 {
    x.clamp(lo, hi)
}

pub fn allowed_method(m: &str) -> bool {
    ["GET", "POST", "PUT"].contains(&m)
}

pub struct Custom(pub i32);
impl Custom {
    pub fn starts_with(&self, n: i32) -> bool { self.0 == n }
    pub fn replace(&self, a: i32, b: i32) -> i32 { self.0 + a + b }
    pub fn floor(&self) -> i32 { self.0 }
    pub fn clamp(&self, lo: i32, hi: i32) -> i32 { self.0 + lo + hi }
    pub fn all(&mut self, f: impl Fn(i32) -> bool) -> bool { f(self.0) }
}

pub fn uses_custom(c: &mut Custom) -> bool {
    c.starts_with(1)
        && c.replace(1, 2) == 4
        && c.floor() == 1
        && c.clamp(0, 0) == 1
        && c.all(|x| x == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_basics_without_edge_cases() {
        assert!(has_prefix("api/users"));
        assert!(!has_prefix("health"));
        assert_eq!(normalize_sep("a-b"), "a_b");
        assert_eq!(bounded(50), 50);
        assert_eq!(whole(2.0), 2.0);
        assert!(all_positive(&[1, 2, 3]));
        assert_eq!(reuse_iter_after_all(&[1, -1, 5]), Some(5));
        assert!(check_ref_iter(&mut [1, 2].iter()));
        assert_eq!(reuse_copy_iter_after_any(), Some(1));
        assert!(allowed_method("GET"));
        assert!(allowed_method("POST"));
        assert!(!allowed_method("DELETE"));
        assert!(uses_custom(&mut Custom(1)));
    }

    #[test]
    #[should_panic]
    fn reversed_clamp_panics() {
        let _ = clamp_panic_guard(5, 10, 0);
    }
}
"#,
    )
    .unwrap();

    let (code, report) = json(&dir);
    assert_eq!(code, 2, "{report:#}");
    // Custom types without alternate methods compile cleanly without needing probe skips,
    // and are treated as opaque (never reported as unreached).
    assert_eq!(report["skipped_sites"].as_u64(), Some(0), "{report:#}");
    assert_eq!(
        report["unreached"].as_array().map(Vec::len),
        Some(0),
        "{report:#}"
    );
    let input_gaps = findings(&report, "input_gap");
    let has_gap = |needle: &str| {
        input_gaps
            .iter()
            .any(|f| strings(&f["survivors"]).iter().any(|s| s.contains(needle)))
    };
    assert!(has_gap("s.contains(\"api/\")"), "{report:#}");
    assert!(has_gap("s.replacen(\"-\", \"_\", 1)"), "{report:#}");
    assert!(has_gap("x.min(100)"), "{report:#}");
    assert!(has_gap("x.max(0)"), "{report:#}");
    assert!(has_gap("x.trunc()"), "{report:#}");
    assert!(has_gap("x.ceil()"), "{report:#}");

    // Reversed-bounds clamp panic test must kill both clamp mutants (neither input_gap nor unreached).
    assert!(
        !report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["code"].as_str() == Some("x.clamp(lo, hi)")),
        "{report:#}"
    );

    let assertion_gaps = findings(&report, "assertion_gap");
    assert!(
        assertion_gaps.iter().any(|f| strings(&f["survivors"])
            .iter()
            .any(|s| s.contains("xs.iter().any(|&x| x > 0)"))),
        "{report:#}"
    );

    // Custom non-std methods must not produce false findings or fail compilation.
    assert!(
        !report["findings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| matches!(
                f["code"].as_str().unwrap_or(""),
                "c.starts_with(1)"
                    | "c.replace(1, 2)"
                    | "c.floor()"
                    | "c.clamp(0, 0)"
                    | "c.all(|x| x == 1)"
            )),
        "{report:#}"
    );

    // With --rebuild, dropping "PUT" from `["GET", "POST", "PUT"].contains(&m)` survives
    // and names the removed element.
    let run = tmt(
        &dir,
        &["--json", "--quiet", "--rebuild", "--lines", "src/lib.rs:56"],
    );
    assert_eq!(run.code, 2, "{}", run.stderr);
    let rebuilt: Value = serde_json::from_str(&run.stdout).unwrap();
    let survivors = findings(&rebuilt, "survivor");
    assert_eq!(survivors.len(), 1, "{rebuilt:#}");
    assert!(
        strings(&survivors[0]["survivors"])
            .iter()
            .any(|s| s.contains("without `\"PUT\"`")),
        "{rebuilt:#}"
    );
}

#[test]
fn since_subhunk_undo_catches_untested_condition_inside_tested_hunk() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("since-subhunk-undo");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"since-subhunk\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        dir.join("src/lib.rs"),
        r#"
pub fn allow(is_member: bool, _is_active: bool, _is_verified: bool) -> bool {
    is_member
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_member_and_active() {
        assert!(allow(true, true, true));
        assert!(!allow(false, true, true));
    }
}
"#,
    )
    .unwrap();

    let git = |args: &[&str]| {
        let out = Command::new("git")
            .current_dir(&dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.name", "tmt-test"]);
    git(&["config", "user.email", "tmt-test@example.com"]);
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "initial"]);

    // Add TWO conditions (`&& is_active && is_verified`), but tests only check `is_active`.
    // Reverting the whole hunk fails tests (killed by `is_active`), while the AST sub-hunk
    // removing only `is_verified` survives as an UNTESTED CHANGE.
    fs::write(
        dir.join("src/lib.rs"),
        r#"
pub fn allow(is_member: bool, is_active: bool, is_verified: bool) -> bool {
    is_member && is_active && is_verified
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_member_and_active() {
        assert!(allow(true, true, true));
        assert!(!allow(false, true, true));
        assert!(!allow(true, false, true));
    }
}
"#,
    )
    .unwrap();

    let run = tmt(
        &dir,
        &["--json", "--quiet", "--source-only", "--since", "HEAD"],
    );
    assert_eq!(run.code, 2, "{}", run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    let untested: Vec<_> = findings(&report, "survivor")
        .into_iter()
        .filter(|f| f["change_since"].is_string())
        .collect();
    assert!(
        untested.iter().any(|f| strings(&f["survivors"])
            .iter()
            .any(|s| s.contains("without added condition `is_verified`"))),
        "{report:#}"
    );
    // Whole hunk and `is_active` sub-hunk are killed by `assert!(!allow(true, false, true))`.
    assert!(
        !untested.iter().any(|f| strings(&f["survivors"])
            .iter()
            .any(|s| s.contains("without added condition `is_active`"))),
        "{report:#}"
    );
}

#[test]
fn stronger_assertions_narrow_struct_fields_and_emit_schema_version() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("struct-field-narrowing");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"struct-narrow\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        dir.join("src/lib.rs"),
        r#"
#[derive(Debug)]
pub struct Quote {
    pub subtotal: i32,
    pub fee: i32,
}

pub fn quote(subtotal: i32, rate: i32) -> Quote {
    Quote {
        subtotal,
        fee: subtotal * rate,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_subtotal_only() {
        let q = quote(200, 3);
        assert_eq!(q.subtotal, 200);
    }
}
"#,
    )
    .unwrap();

    let run = tmt(&dir, &["--format", "json", "--quiet", "--rebuild"]);
    assert_eq!(run.code, 2, "{}", run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(report["schema_version"], 1, "{report:#}");
    let gaps = findings(&report, "assertion_gap");
    let narrowed = gaps.iter().find(|f| {
        f["witness"]["field"].as_str() == Some("fee")
            && f["witness"]["assertion"].as_str() == Some("assert_eq!(quote(200, 3).fee, 600);")
    });
    assert!(narrowed.is_some(), "{report:#}");

    // `--list --format json` and `--format jsonl` also emit `schema_version: 1`.
    let listed = tmt(&dir, &["--list", "--format", "json", "--quiet"]);
    assert_eq!(listed.code, 0, "{}", listed.stderr);
    let listed_json: Value = serde_json::from_str(&listed.stdout).unwrap();
    assert_eq!(listed_json["schema_version"], 1, "{listed_json:#}");

    let jsonl = tmt(&dir, &["--format", "jsonl", "--quiet"]);
    assert_eq!(jsonl.code, 2, "{}", jsonl.stderr);
    let first_line: Value = serde_json::from_str(jsonl.stdout.lines().next().unwrap()).unwrap();
    assert_eq!(first_line["schema_version"], 1, "{first_line:#}");
    assert_eq!(first_line["report"]["schema_version"], 1, "{first_line:#}");
}

fn assert_json_counts_reconcile(report: &Value) {
    let run = report["mutants_run"].as_u64().unwrap();
    let killed = report["mutants_killed"].as_u64().unwrap();
    let timed_out = report["mutants_timed_out"].as_u64().unwrap();
    let survived = report["mutants_survived"].as_u64().unwrap();
    let sb = &report["survivor_breakdown"];
    let sb_sum = sb["reported"].as_u64().unwrap()
        + sb["suppressed"].as_u64().unwrap()
        + sb["redundant_constant"].as_u64().unwrap()
        + sb["preexisting_unchanged"].as_u64().unwrap()
        + sb["unchanged_not_compared"].as_u64().unwrap()
        + sb["unconfirmed_gap"].as_u64().unwrap();
    assert_eq!(
        run,
        killed + survived,
        "run != killed + survived: {report:#}"
    );
    assert!(timed_out <= killed, "timed_out > killed: {report:#}");
    assert_eq!(
        survived, sb_sum,
        "survived != sum(survivor_breakdown): {report:#}"
    );
}

#[test]
fn report_counts_reconcile_and_mark_partial_evaluation() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("report-counts-reconcile");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"reconcile-demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        dir.join("src/lib.rs"),
        r#"
pub const fn doubled(x: i32) -> i32 {
    x + x
}

pub fn always_true_in_tests(x: i32) -> bool {
    x > 0
}

pub fn suppressed_gap(x: i32) -> i32 {
    // tmt:ignore intentional weak test
    x + 1
}

pub fn covered(x: i32) -> i32 {
    x + 2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exercises_functions() {
        assert_eq!(doubled(3), 6);
        assert!(always_true_in_tests(5));
        let _ = suppressed_gap(1);
        assert_eq!(covered(3), 5);
    }
}
"#,
    )
    .unwrap();

    // Without `--rebuild`: `const fn doubled` leaves unbuilt mutants -> `evaluation: "partial"`.
    let quick = tmt(&dir, &["--json", "--quiet", "--no-effects"]);
    assert_eq!(quick.code, 2, "{}", quick.stderr);
    let quick_json: Value = serde_json::from_str(&quick.stdout).unwrap();
    assert_json_counts_reconcile(&quick_json);
    assert_eq!(quick_json["evaluation"], "partial", "{quick_json:#}");
    assert!(
        quick_json["mutants_not_built"].as_u64().unwrap() > 0,
        "{quick_json:#}"
    );
    assert!(
        quick_json["survivor_breakdown"]["suppressed"]
            .as_u64()
            .unwrap()
            > 0,
        "{quick_json:#}"
    );
    assert!(
        quick_json["survivor_breakdown"]["redundant_constant"]
            .as_u64()
            .unwrap()
            > 0,
        "{quick_json:#}"
    );

    // Text output also calls out both the partial evaluation and unreported survivors.
    let quick_text = tmt(&dir, &["--quiet", "--no-effects"]);
    assert!(
        quick_text
            .stdout
            .contains("(partial evaluation: some mutants were not built or evaluated)"),
        "{}",
        quick_text.stdout
    );
    assert!(
        quick_text
            .stdout
            .contains("surviving mutants not reported as findings:"),
        "{}",
        quick_text.stdout
    );

    // `--no-verify` does not run confirmation mutants for predicted input gaps, yet counts still reconcile.
    let no_verify = tmt(&dir, &["--json", "--quiet", "--no-effects", "--no-verify"]);
    let no_verify_json: Value = serde_json::from_str(&no_verify.stdout).unwrap();
    assert_json_counts_reconcile(&no_verify_json);
    assert_eq!(
        no_verify_json["evaluation"], "partial",
        "{no_verify_json:#}"
    );

    // With `--rebuild`: all mutants are built and evaluated -> `evaluation` is omitted (`null`).
    let full = tmt(&dir, &["--json", "--quiet", "--no-effects", "--rebuild"]);
    let full_json: Value = serde_json::from_str(&full.stdout).unwrap();
    assert_json_counts_reconcile(&full_json);
    assert_eq!(full_json["mutants_not_built"], 0, "{full_json:#}");
    assert!(full_json["evaluation"].is_null(), "{full_json:#}");
}

#[test]
fn since_preexisting_survivors_in_unchanged_code_reconcile() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("report-counts-since-preexisting");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"since-reconcile\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let base_src = r#"
pub fn unchanged_weak(x: i32) -> i32 {
    if x > 0 {
        x + 1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weak_check() {
        assert!(unchanged_weak(5) > 0);
        assert_eq!(unchanged_weak(-1), 0);
    }
}
"#;
    fs::write(dir.join("src/lib.rs"), base_src).unwrap();
    fs::write(dir.join(".gitignore"), "target/\nCargo.lock\n").unwrap();
    git_output(&dir, &["init", "-q"]);
    git_output(&dir, &["add", "-A"]);
    let base = git_output(&dir, &["write-tree"]);

    // Modify only the test so `unchanged_weak` is checked as unchanged code (`beyond`).
    let updated_src = base_src.replace(
        "assert_eq!(unchanged_weak(-1), 0);",
        "assert_eq!(unchanged_weak(-1), 0);\n        assert_eq!(unchanged_weak(0), 0);",
    );
    fs::write(dir.join("src/lib.rs"), updated_src).unwrap();

    let run = tmt(
        &dir,
        &["--since", &base, "--json", "--quiet", "--no-effects"],
    );
    assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_json_counts_reconcile(&report);
    assert!(
        report["findings"].as_array().unwrap().is_empty(),
        "{report:#}"
    );
    assert!(
        report["mutants_survived"].as_u64().unwrap() > 0,
        "expected preexisting survivors in unchanged code: {report:#}"
    );
    assert_eq!(
        report["survivor_breakdown"]["preexisting_unchanged"], report["mutants_survived"],
        "{report:#}"
    );
    // Only the test changed, so there are no unbuilt source-revert mutants -> complete evaluation.
    assert!(report["evaluation"].is_null(), "{report:#}");

    // When the earlier tests cannot build against the current code, unchanged survivors
    // must be classified as `unchanged_not_compared` and mark `evaluation: "partial"`.
    let broken_earlier = base_src
        .replace(
            "pub fn unchanged_weak(x: i32) -> i32 {",
            "pub fn helper() -> i32 { 1 }\npub fn unchanged_weak(x: i32) -> i32 {",
        )
        .replace(
            "assert_eq!(unchanged_weak(-1), 0);",
            "assert_eq!(unchanged_weak(-1), 0);\n        assert_eq!(helper(), 1);",
        );
    fs::write(dir.join("src/lib.rs"), &broken_earlier).unwrap();
    git_output(&dir, &["add", "-A"]);
    let with_helper = git_output(&dir, &["write-tree"]);
    // Remove `helper()` and its call so old tests calling `helper()` no longer build.
    fs::write(dir.join("src/lib.rs"), base_src).unwrap();
    let uncompared = tmt(
        &dir,
        &["--since", &with_helper, "--json", "--quiet", "--no-effects"],
    );
    assert_eq!(
        uncompared.code, 0,
        "{}\n{}",
        uncompared.stdout, uncompared.stderr
    );
    let uncompared_json: Value = serde_json::from_str(&uncompared.stdout).unwrap();
    assert_json_counts_reconcile(&uncompared_json);
    assert!(
        uncompared_json["survivor_breakdown"]["unchanged_not_compared"]
            .as_u64()
            .unwrap()
            > 0,
        "{uncompared_json:#}"
    );
    assert_eq!(
        uncompared_json["evaluation"], "partial",
        "{uncompared_json:#}"
    );
}

#[test]
fn since_const_only_change_without_rebuild_reports_unbuilt_and_no_mutants_executed() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("report-counts-since-const-only");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"const-only-since\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let base_src = "pub const LIMIT: u32 = 10;\n#[test]\nfn ok() { assert!(LIMIT > 0); }\n";
    fs::write(dir.join("src/lib.rs"), base_src).unwrap();
    fs::write(dir.join(".gitignore"), "target/\nCargo.lock\n").unwrap();
    git_output(&dir, &["init", "-q"]);
    git_output(&dir, &["add", "-A"]);
    let base = git_output(&dir, &["write-tree"]);

    fs::write(
        dir.join("src/lib.rs"),
        base_src.replace("LIMIT: u32 = 10;", "LIMIT: u32 = 20;"),
    )
    .unwrap();
    let run = tmt(
        &dir,
        &["--since", &base, "--json", "--quiet", "--no-effects"],
    );
    assert_eq!(run.code, 3, "{}\n{}", run.stdout, run.stderr);
    let report: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_json_counts_reconcile(&report);
    assert!(
        report["mutants_not_built"].as_u64().unwrap() > 0,
        "{report:#}"
    );
    assert_eq!(report["evaluation"], "no_mutants_executed", "{report:#}");

    // When a sibling module has a const-only change alongside a probed function change in
    // `lib.rs`, the sibling module's unbuilt revert must still be counted and mark `partial`.
    fs::write(
        dir.join("src/limits.rs"),
        "pub const MAX_ITEMS: u32 = 10;\n",
    )
    .unwrap();
    fs::write(
        dir.join("src/lib.rs"),
        "pub mod limits;\npub fn add(a: i32, b: i32) -> i32 { a + b }\n#[test]\nfn ok() { assert_eq!(add(2, 3), 5); assert_eq!(limits::MAX_ITEMS, 10); }\n",
    )
    .unwrap();
    git_output(&dir, &["add", "-A"]);
    let with_sibling = git_output(&dir, &["write-tree"]);
    fs::write(
        dir.join("src/limits.rs"),
        "pub const MAX_ITEMS: u32 = 20;\n",
    )
    .unwrap();
    fs::write(
        dir.join("src/lib.rs"),
        "pub mod limits;\npub fn add(a: i32, b: i32) -> i32 { a + b + 1 }\n#[test]\nfn ok() { assert_eq!(add(2, 3), 6); assert_eq!(limits::MAX_ITEMS, 20); }\n",
    )
    .unwrap();
    let mixed = tmt(
        &dir,
        &[
            "--since",
            &with_sibling,
            "--json",
            "--quiet",
            "--no-effects",
        ],
    );
    let mixed_json: Value = serde_json::from_str(&mixed.stdout).unwrap();
    assert_json_counts_reconcile(&mixed_json);
    assert!(
        mixed_json["mutants_run"].as_u64().unwrap() > 0,
        "{mixed_json:#}"
    );
    assert!(
        mixed_json["mutants_not_built"].as_u64().unwrap() >= 2,
        "expected unbuilt reverts from both lib.rs and sibling limits.rs: {mixed_json:#}"
    );
    assert_eq!(mixed_json["evaluation"], "partial", "{mixed_json:#}");
}

#[test]
fn unparseable_sibling_module_marks_evaluation_partial_in_json_sarif_and_text() {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join("report-counts-unparseable-partial");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("src")).unwrap();
    fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"unparseable-partial\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        dir.join("src/lib.rs"),
        "pub mod broken;\npub fn add(a: i32, b: i32) -> i32 { a + b }\n#[test]\nfn checks_add() { assert_eq!(add(2, 3), 5); }\n",
    )
    .unwrap();
    // UTF-8 BOM makes syn reject `broken.rs` while rustc still compiles it.
    fs::write(
        dir.join("src/broken.rs"),
        "\u{feff}pub fn ok() -> i32 { 1 }\n",
    )
    .unwrap();

    let json_run = tmt(&dir, &["--json", "--quiet", "--no-effects"]);
    assert_eq!(json_run.code, 0, "{}\n{}", json_run.stdout, json_run.stderr);
    let report: Value = serde_json::from_str(&json_run.stdout).unwrap();
    assert_json_counts_reconcile(&report);
    assert!(report["mutants_run"].as_u64().unwrap() > 0, "{report:#}");
    assert_eq!(report["evaluation"], "partial", "{report:#}");

    let sarif_run = tmt(&dir, &["--sarif", "--quiet", "--no-effects"]);
    let sarif: Value = serde_json::from_str(&sarif_run.stdout).unwrap();
    assert_eq!(
        sarif["runs"][0]["properties"]["evaluation"], "partial",
        "{sarif:#}"
    );

    let text_run = tmt(&dir, &["--quiet", "--no-effects"]);
    assert!(
        text_run
            .stdout
            .contains("(partial evaluation: some mutants were not built or evaluated)"),
        "{}",
        text_run.stdout
    );
}

#[test]
fn config_file_auto_discovery_explicit_and_cli_override() {
    let dir = fixture("eligibility", "config-auto-discovery");
    fs::write(
        dir.join("tmt.toml"),
        "format = \"json\"\nquiet = true\nverify = false\n",
    )
    .unwrap();

    // Auto-discovered `tmt.toml` sets JSON format, quiet stderr, and verify = false.
    let run = tmt(&dir, &[]);
    assert_eq!(run.code, 2, "{}\n{}", run.stdout, run.stderr);
    assert!(
        run.stderr.is_empty(),
        "expected quiet stderr: {}",
        run.stderr
    );
    let report: Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("expected JSON from tmt.toml ({e}):\n{}", run.stdout));
    assert_eq!(report["analysis"]["verified"], false, "{report:#}");

    // CLI `--format text` overrides config `format = "json"`.
    let text_override = tmt(&dir, &["--format", "text"]);
    assert_eq!(
        text_override.code, 2,
        "{}\n{}",
        text_override.stdout, text_override.stderr
    );
    assert!(
        text_override.stdout.contains("INPUT GAP"),
        "expected text report: {}",
        text_override.stdout
    );

    // `--no-config` ignores `tmt.toml`, restoring text format and default verify = true.
    let disabled = tmt(&dir, &["--no-config", "--json", "--quiet"]);
    assert_eq!(disabled.code, 2, "{}\n{}", disabled.stdout, disabled.stderr);
    let disabled_report: Value = serde_json::from_str(&disabled.stdout).unwrap();
    assert_eq!(
        disabled_report["analysis"]["verified"], true,
        "{disabled_report:#}"
    );

    // `--config FILE` loads an explicit file instead of auto-discovered `tmt.toml`.
    let custom = dir.join("custom-tmt.toml");
    fs::write(&custom, "format = \"json\"\nquiet = true\nverify = true\n").unwrap();
    let explicit = tmt(&dir, &["--config", custom.to_str().unwrap()]);
    assert_eq!(explicit.code, 2, "{}\n{}", explicit.stdout, explicit.stderr);
    let explicit_report: Value = serde_json::from_str(&explicit.stdout).unwrap();
    assert_eq!(
        explicit_report["analysis"]["verified"], true,
        "{explicit_report:#}"
    );
}

#[test]
fn config_file_workspace_per_package_override() {
    let dir = fixture("multi", "config-workspace-pkg");
    let ws = dir.join("ws");
    fs::create_dir_all(ws.join(".config")).unwrap();
    fs::write(
        ws.join(".config/tmt.toml"),
        "format = \"json\"\nquiet = true\nverify = true\n\n[package.ws-core]\nverify = false\n",
    )
    .unwrap();

    // Workspace run applies `[package.ws-core]` only to `ws-core`, leaving `ws-app` at `verify = true`.
    let run = tmt(&ws, &["--workspace"]);
    assert!(
        run.code == 0 || run.code == 2,
        "unexpected exit {}:\n{}\n{}",
        run.code,
        run.stdout,
        run.stderr
    );
    let combined: Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("expected workspace JSON ({e}):\n{}", run.stdout));
    let packages = combined["packages"].as_array().unwrap();
    let by_name = |name: &str| {
        packages
            .iter()
            .find(|p| p["package"] == name)
            .unwrap_or_else(|| panic!("missing package {name} in {combined:#}"))
    };
    assert_eq!(by_name("ws-core")["analysis"]["verified"], false);
    assert_eq!(by_name("ws-app")["analysis"]["verified"], true);

    // Running directly inside member `ws/core` auto-discovers `ws/.config/tmt.toml`
    // and applies `[package.ws-core]`.
    let member_run = tmt(&ws.join("core"), &[]);
    assert!(
        member_run.code == 0 || member_run.code == 2,
        "unexpected exit {}:\n{}\n{}",
        member_run.code,
        member_run.stdout,
        member_run.stderr
    );
    let member_report: Value = serde_json::from_str(&member_run.stdout)
        .unwrap_or_else(|e| panic!("expected member JSON ({e}):\n{}", member_run.stdout));
    assert_eq!(member_report["package"], "ws-core");
    assert_eq!(member_report["analysis"]["verified"], false);
}

#[test]
fn config_file_errors_exit_one() {
    let dir = fixture("eligibility", "config-errors");

    // Unknown key in `tmt.toml`.
    fs::write(dir.join("tmt.toml"), "nonexistent_option = 42\n").unwrap();
    let unknown = tmt(&dir, &[]);
    assert_eq!(unknown.code, 1, "{}\n{}", unknown.stdout, unknown.stderr);
    assert!(
        unknown.stderr.contains("unknown field"),
        "expected unknown field error: {}",
        unknown.stderr
    );

    // Duplicate candidates in the same directory.
    fs::write(dir.join("tmt.toml"), "quiet = true\n").unwrap();
    fs::create_dir_all(dir.join(".config")).unwrap();
    fs::write(dir.join(".config/tmt.toml"), "quiet = true\n").unwrap();
    let dup = tmt(&dir, &[]);
    assert_eq!(dup.code, 1, "{}\n{}", dup.stdout, dup.stderr);
    assert!(
        dup.stderr.contains("multiple tmt config files"),
        "expected duplicate error: {}",
        dup.stderr
    );

    // Missing explicit `--config`.
    let missing = tmt(&dir, &["--config", "does-not-exist.toml"]);
    assert_eq!(missing.code, 1, "{}\n{}", missing.stdout, missing.stderr);
}

#[test]
fn config_file_workspace_per_package_source_only_shard() {
    let dir = fixture("multi", "config-workspace-shard");
    let ws = dir.join("ws");
    fs::write(
        ws.join("tmt.toml"),
        "format = \"json\"\nquiet = true\n\n[package.ws-app]\nsource_only = true\n\n[package.ws-core]\nsource_only = true\n",
    )
    .unwrap();

    // Per-package `source_only = true` satisfies `--workspace --shard 0/2` without top-level `source_only`.
    let run = tmt(&ws, &["--workspace", "--list", "--shard", "0/2"]);
    assert_eq!(run.code, 0, "{}\n{}", run.stdout, run.stderr);
    let combined: Value = serde_json::from_str(&run.stdout)
        .unwrap_or_else(|e| panic!("expected workspace JSON ({e}):\n{}", run.stdout));
    let packages = combined["packages"].as_array().unwrap();
    assert_eq!(packages.len(), 2, "{combined:#}");
    for pkg in packages {
        assert_eq!(pkg["analysis"]["source_only"], true, "{pkg:#}");
        assert_eq!(pkg["analysis"]["shard"]["index"], 0, "{pkg:#}");
        assert_eq!(pkg["analysis"]["shard"]["count"], 2, "{pkg:#}");
    }
}

#[test]
fn config_file_workspace_deferred_prerequisites_work_dir_overlap_and_glob_virtual_ws() {
    // 1. Top-level `copy_fixture_exclude` satisfied by per-package `copy_fixture` works under `--workspace`.
    let dir = fixture("multi", "config-ws-deferred");
    let ws = dir.join("ws");
    fs::create_dir_all(ws.join("app/data/tmp")).unwrap();
    fs::write(ws.join("app/data/keep.txt"), "ok").unwrap();
    fs::write(ws.join("app/data/tmp/skip.txt"), "skip").unwrap();
    fs::write(
        ws.join("tmt.toml"),
        "format = \"json\"\nquiet = true\ncopy_fixture_exclude = [\"app/data/tmp\"]\n\n[package.ws-app]\ncopy_fixture = [\"app/data\"]\n\n[package.ws-core]\ncopy_fixture = [\"app/data\"]\n",
    )
    .unwrap();
    let deferred_run = tmt(&ws, &["--workspace", "--list"]);
    assert_eq!(
        deferred_run.code, 0,
        "{}\n{}",
        deferred_run.stdout, deferred_run.stderr
    );

    // 2. Member `work_dir` overlapping shared `.catalogue` is rejected before running.
    let work_base = dir.join("shared-work");
    fs::write(
        ws.join("tmt.toml"),
        format!(
            "format = \"json\"\nquiet = true\nwork_dir = \"{}\"\n\n[package.ws-app]\nwork_dir = \"{}\"\n",
            work_base.display().to_string().replace('\\', "\\\\"),
            work_base
                .join(".catalogue")
                .display()
                .to_string()
                .replace('\\', "\\\\"),
        ),
    )
    .unwrap();
    let overlap_run = tmt(&ws, &["--workspace", "--list"]);
    assert_eq!(
        overlap_run.code, 1,
        "{}\n{}",
        overlap_run.stdout, overlap_run.stderr
    );
    assert!(
        overlap_run.stderr.contains("overlaps"),
        "expected overlap error: {}",
        overlap_run.stderr
    );

    // 3. Single-member virtual workspace with glob `members = ["crates/*"]` applies `[package.<name>]`.
    let glob_ws = Path::new(env!("CARGO_TARGET_TMPDIR")).join("config-glob-virtual-ws");
    let _ = fs::remove_dir_all(&glob_ws);
    fs::create_dir_all(glob_ws.join("crates/only/src")).unwrap();
    fs::write(
        glob_ws.join("Cargo.toml"),
        "[workspace]\nresolver = \"2\"\nmembers = [\"crates/*\"]\n",
    )
    .unwrap();
    fs::write(
        glob_ws.join("crates/only/Cargo.toml"),
        "[package]\nname = \"only-glob\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    fs::write(
        glob_ws.join("crates/only/src/lib.rs"),
        "pub fn ok(x: u32) -> bool { x >= 10 }\n#[test]\nfn t() { assert!(ok(15)); assert!(!ok(5)); }\n",
    )
    .unwrap();
    fs::write(
        glob_ws.join("tmt.toml"),
        "format = \"json\"\nquiet = true\nverify = true\n\n[package.only-glob]\nverify = false\n",
    )
    .unwrap();
    let implicit_run = tmt(&glob_ws, &[]);
    assert_eq!(
        implicit_run.code, 2,
        "{}\n{}",
        implicit_run.stdout, implicit_run.stderr
    );
    let implicit_report: Value = serde_json::from_str(&implicit_run.stdout).unwrap();
    assert_eq!(implicit_report["package"], "only-glob");
    assert_eq!(implicit_report["analysis"]["verified"], false);
}
