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
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-tmt"))
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

/// A report without what it says about how it was asked for.
fn but_how(mut report: Value) -> Value {
    report.as_object_mut().unwrap().remove("analysis");
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

    // `let value; value = ...;` and a diverging call cannot be guarded: skipped, not fatal.
    assert_eq!(report["skipped_sites"], 2, "{report:#}");
    let skipped = report["skipped"].as_array().unwrap();
    assert!(
        skipped
            .iter()
            .any(|site| site["reason"] == "probe_compile_error"
                && site["file"] == "src/lib.rs"
                && site["line"].as_u64().unwrap() > 0
                && site["end_line"].as_u64().unwrap() >= site["line"].as_u64().unwrap()
                && !site["diagnostic"].as_str().unwrap().is_empty()),
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

    // Running mutants in parallel gives the same report.
    let parallel = tmt(&dir, &["--json", "--quiet", "--rebuild", "--jobs", "4"]);
    let parallel: Value = serde_json::from_str(&parallel.stdout).unwrap();
    assert_eq!(parallel, report);
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
    // the two that never end run out of time again, and the report is the same.
    let run = tmt(&dir, &["--json", "--quiet", "--jobs", "3"]);
    let together: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(together, report);
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
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-tmt"))
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
    assert_eq!(but_how(by_lines), but_how(report));
    let bad = tmt(&dir, &["--quiet", "--lines", "src/lib.rs:9-2"]);
    assert_eq!(bad.code, 1);
    assert!(bad.stderr.contains("not a line range"), "{}", bad.stderr);
    let unknown = tmt(&dir, &["--quiet", "--since", "no-such-revision"]);
    assert_eq!(unknown.code, 1);
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
            "with the mutant, these tests still reach the same code with the same outcomes: they do not use its result, or it is the same for their inputs"
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
            .contains("sharing a build for 2 original witness inputs"),
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
    assert_eq!(bad.code, 2, "{}", bad.stderr);
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
    let output = Command::new(env!("CARGO_BIN_EXE_cargo-tmt"))
        .arg(".")
        .args(["--json", "--quiet"])
        .current_dir(&dir)
        .env("TMPDIR", ".")
        .env_remove("TMT_TRACE")
        .env_remove("TMT_MUTANT")
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
    let output = Command::new("cargo")
        .args(["tmt", "--json", "--quiet"])
        .current_dir(&dir)
        .env("PATH", std::env::join_paths(path).unwrap())
        .env_remove("TMT_TRACE")
        .env_remove("TMT_MUTANT")
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
    let help = Command::new(exe).arg("--help").output().unwrap();
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
