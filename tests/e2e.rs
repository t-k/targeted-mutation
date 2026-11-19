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
    let parallel = tmt(&dir, &["--json", "--quiet", "--jobs", "4"]);
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
    assert_eq!(code, 0, "{report:#}");
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
    assert_eq!(by_lines, report);
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
fn the_mutants_of_cargo_mutants() {
    let dir = fixture("genres", "genres");
    let (code, report) = json(&dir);
    assert_eq!(code, 2, "{report:#}");
    assert_eq!(report["skipped_sites"], 0, "{report:#}");
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
    assert_eq!(survivors("-above", "assertion_gap"), ["`above`"]);
    let built = all.iter().find(|f| f["code"] == "-above").unwrap();
    assert_eq!(built["tests"].as_array().unwrap().len(), 0, "{built:#}");

    // An iterator of another type cannot be returned beside the body's own, so the
    // three replacements of `evens` are built one by one. They can be left out.
    let run = tmt(&dir, &["--json", "--quiet", "--no-rebuild"]);
    let quick: Value = serde_json::from_str(&run.stdout).unwrap();
    assert_eq!(
        report["mutants_run"].as_u64().unwrap(),
        quick["mutants_run"].as_u64().unwrap() + 4,
        "{quick:#}"
    );
    assert_eq!(
        report["mutants_killed"].as_u64().unwrap(),
        quick["mutants_killed"].as_u64().unwrap() + 3,
        "{quick:#}"
    );
    // Left out, the site with the probe that did not compile is a skipped one.
    assert_eq!(quick["skipped_sites"], 1, "{quick:#}");
    assert_eq!(
        quick["findings"].as_array().unwrap().len() + 1,
        all.len(),
        "{quick:#}"
    );
}
