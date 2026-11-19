//! Runs the `tmt` binary on the fixture crates.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    let output = Command::new(env!("CARGO_BIN_EXE_tmt"))
        .arg(dir)
        .args(args)
        .env_remove("TMT_TRACE")
        .env_remove("TMT_MUTANT")
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
    assert_eq!(gaps.len(), 1, "{report:#}");
    assert_eq!(gaps[0]["code"], "self.check_preconditions(batch)?;");
    assert_eq!(
        strings(&gaps[0]["survivors"]),
        ["delete `self.check_preconditions(batch)?;`"]
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
    let (code, report) = json(&dir);
    assert_eq!(code, 2, "{report:#}");
    let all = report["findings"].as_array().unwrap();
    let at = |code: &str, kind: &str| {
        all.iter()
            .find(|f| f["code"] == code && f["kind"] == kind)
            .unwrap_or_else(|| panic!("no {kind} at `{code}`: {report:#}"))
    };

    // `let value; value = ...;` and a diverging call cannot be guarded: skipped, not fatal.
    assert_eq!(report["skipped_sites"], 2, "{report:#}");
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
    // Floats carry no values: only "never equal" can be said.
    assert_eq!(
        strings(&at("celsius > 30.5", "input_gap")["survivors"]),
        ["`celsius >= 30.5`"]
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
    // `const fn` bodies and test modules are not sites.
    assert!(!codes.contains(&"a < 10"), "{report:#}");
    let unreached: Vec<&str> = report["unreached"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["code"].as_str().unwrap())
        .collect();
    assert_eq!(unreached, ["x == 7"]);
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
        "\n#[test]\nfn broken() {\n    assert!(is_eligible(1));\n}\n",
    );
    let run = tmt(&dir, &["--quiet"]);
    assert_eq!(run.code, 1, "{}", run.stdout);
    assert!(
        run.stderr.contains("do not pass on the unmutated code"),
        "{}",
        run.stderr
    );
    assert_eq!(run.stdout, "");
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
fn endless_loop_mutant_is_killed_by_the_timeout() {
    let dir = fixture("looping", "looping");
    let started = std::time::Instant::now();
    let (code, report) = json(&dir);
    assert_eq!(code, 0, "{report:#}");
    assert_eq!(report["mutants_run"], 2, "{report:#}");
    assert_eq!(report["mutants_killed"], 2, "{report:#}");
    assert_eq!(report["mutants_timed_out"], 1, "{report:#}");
    assert!(started.elapsed() >= std::time::Duration::from_secs(5), "the timeout was waited for");
    assert!(started.elapsed() < std::time::Duration::from_secs(120), "and the mutant was stopped");
}

#[test]
fn no_std_crate_and_custom_work_dir() {
    let dir = fixture("no-std", "no-std");
    let work = Path::new(env!("CARGO_TARGET_TMPDIR")).join("no-std-work");
    let _ = fs::remove_dir_all(&work);
    let run = tmt(&dir, &["--json", "--quiet", "--work-dir", work.to_str().unwrap()]);
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
    assert_eq!(code, 0, "{report:#}");
    assert_eq!(report["package"], "ws-app");
    assert_eq!(report["sites"], 1, "only the member's own code is mutated: {report:#}");

    let ambiguous = tmt(&ws, &["--quiet"]);
    assert_eq!(ambiguous.code, 1);
    assert!(ambiguous.stderr.contains("--package"), "{}", ambiguous.stderr);
    let unknown = tmt(&ws, &["--quiet", "--package", "nope"]);
    assert_eq!(unknown.code, 1);
}
