mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::fixture_command;
use serde_json::Value;

fn package(name: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    if path.exists() {
        fs::remove_dir_all(&path).unwrap();
    }
    fs::create_dir_all(path.join("src")).unwrap();
    write(
        &path,
        "Cargo.toml",
        "[package]\nname = \"campaign-example\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n",
    );
    write(
        &path,
        "src/lib.rs",
        "pub fn value() -> u32 { 9 }\n#[test] fn exact_value() { assert_eq!(value(), 9); }\n",
    );
    path
}

fn write(path: &Path, relative: &str, content: &str) {
    let path = path.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn command(path: &Path, args: &[&str]) -> Command {
    let mut command = fixture_command(env!("CARGO_BIN_EXE_cargo-tmt"));
    command.arg(path).args(args);
    command
}

fn json(run: &Output, code: i32) -> Value {
    assert_eq!(
        run.status.code(),
        Some(code),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    serde_json::from_slice(&run.stdout).unwrap()
}

#[cfg(unix)]
fn wrapper(path: &Path, contents: &str) {
    use std::os::unix::fs::PermissionsExt;
    write(path, "wrap", contents);
    fs::set_permissions(path.join("wrap"), fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn checkpoint_reuses_only_the_same_campaign_context() {
    let path = package("checkpoint-cli");
    let args = [
        "--source-only",
        "--lines",
        "src/lib.rs:1",
        "--checkpoint",
        "--quiet",
        "--json",
    ];
    let run = || command(&path, &args).output().unwrap();
    let first = json(&run(), 0);
    let checkpoint_path = first["analysis"]["checkpoint"].as_str().unwrap();
    let events = || {
        fs::read_to_string(checkpoint_path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap())
            .collect::<Vec<_>>()
    };
    let recorded = events();
    assert_eq!(recorded.last().unwrap()["event"], "campaign_completed");
    assert!(
        recorded
            .iter()
            .any(|row| row["event"] == "outcome" && row["phase"] == "source")
    );

    // Simulate interruption after candidate outcomes but before the final campaign marker.
    let mut incomplete = fs::read_to_string(checkpoint_path).unwrap();
    let last = incomplete.trim_end().rfind('\n').unwrap() + 1;
    incomplete.truncate(last);
    incomplete.push_str("{\"event\":\"campaign_com");
    fs::write(checkpoint_path, incomplete).unwrap();
    assert_eq!(json(&run(), 0), first);
    assert!(events().iter().any(|row| row["event"] == "phase_resumed"
        && row["phase"] == "source"
        && row["reused"].as_u64().unwrap() == 2));

    let before = events().len();
    write(
        &path,
        "src/lib.rs",
        "pub fn value() -> u32 { 8 }\n#[test] fn exact_value() { assert_eq!(value(), 8); }\n",
    );
    assert_eq!(json(&run(), 0)["mutants_killed"], 2);
    let changed = events();
    assert!(
        changed[before..]
            .iter()
            .any(|row| row["event"] == "phase_started" && row["phase"] == "source")
    );
    assert!(
        !changed[before..]
            .iter()
            .any(|row| row["event"] == "phase_resumed")
    );
}

#[test]
fn cargo_output_assets_work_in_a_child_with_a_cleared_environment() {
    let path = package("output-fixture-cli");
    write(&path, "assets/message.txt", "isolated asset");
    write(
        &path,
        "src/bin/helper.rs",
        r#"
fn main() {
    let exe = std::env::current_exe().unwrap();
    print!("{}", std::fs::read_to_string(exe.parent().unwrap().join("runtime/message.txt")).unwrap());
}
"#,
    );
    write(
        &path,
        "tests/child.rs",
        r#"
#[test]
fn child_reads_asset() {
    let child = std::process::Command::new(env!("CARGO_BIN_EXE_helper")).env_clear().output().unwrap();
    assert!(child.status.success(), "{:?}", child.stderr);
    assert_eq!(child.stdout, b"isolated asset");
    assert_eq!(campaign_example::value(), 9);
}
"#,
    );
    let mut args = vec![
        "--source-only",
        "--test-target",
        "child",
        "--lines",
        "src/lib.rs:1",
        "--quiet",
        "--json",
    ];
    let missing = command(&path, &args).output().unwrap();
    assert_eq!(missing.status.code(), Some(1));
    args.extend(["--copy-output-fixture", "assets=debug/runtime"]);
    let report = json(&command(&path, &args).output().unwrap(), 0);
    assert_eq!(report["mutants_killed"], 2);
    assert_eq!(
        report["analysis"]["output_fixtures"][0]["to"],
        "debug/runtime"
    );
}

#[cfg(unix)]
#[test]
fn explicit_fixture_exclusion_only_omits_a_confined_entry() {
    let path = package("dangling-fixture-cli");
    write(&path, "vendor/module.txt", "sdk");
    fs::create_dir_all(path.join("vendor/.bin")).unwrap();
    std::os::unix::fs::symlink("../missing", path.join("vendor/.bin/glob")).unwrap();
    let mut args = vec!["--list", "--quiet", "--json", "--copy-fixture", "vendor"];
    let strict = command(&path, &args).output().unwrap();
    assert_eq!(strict.status.code(), Some(1));
    args.extend(["--copy-fixture-exclude", "vendor/.bin/glob"]);
    let report = json(&command(&path, &args).output().unwrap(), 0);
    assert_eq!(
        report["analysis"]["copy_fixture_excludes"][0],
        "vendor/.bin/glob"
    );
    assert_eq!(
        fs::read_to_string(path.join("target/tmt/src/vendor/module.txt")).unwrap(),
        "sdk"
    );
    assert!(!path.join("target/tmt/src/vendor/.bin/glob").exists());
    fs::remove_file(path.join("vendor/.bin/glob")).unwrap();
    std::os::unix::fs::symlink("../../../missing", path.join("vendor/.bin/glob")).unwrap();
    let outside = command(&path, &args).output().unwrap();
    assert_eq!(outside.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&outside.stderr).contains("outside the workspace"));
}

#[cfg(unix)]
#[test]
fn wrapper_can_clean_up_after_mutant_timeout_or_use_force_only() {
    let path = package("wrapper-grace-cli");
    write(
        &path,
        "src/lib.rs",
        r#"pub fn value() -> bool { true }
#[test]
fn running() {
    if !value() { std::thread::sleep(std::time::Duration::from_secs(10)); }
}
"#,
    );
    wrapper(
        &path,
        r#"#!/bin/sh
set -eu
entry=$(mktemp -d "$FIXTURE_SCRATCH/wrapper.XXXXXX")
trap 'rmdir "$entry"' EXIT
trap 'exit 143' TERM
"$@" &
wait "$!"
"#,
    );
    let scratch = path.join("target/cleanup");
    fs::create_dir_all(&scratch).unwrap();
    for (grace, entries) in [("2", 0), ("0", 1)] {
        let args = [
            "--source-only",
            "--lines",
            "src/lib.rs:1",
            "--test-wrapper",
            "wrap",
            "--wrapper-grace",
            grace,
            "--timeout",
            "1",
            "--quiet",
            "--json",
        ];
        let run = command(&path, &args)
            .env("FIXTURE_SCRATCH", &scratch)
            .output()
            .unwrap();
        let report = json(&run, 3);
        assert_eq!(report["mutants_timed_out"], 1);
        assert_eq!(fs::read_dir(&scratch).unwrap().count(), entries);
    }
}

#[cfg(unix)]
#[test]
fn isolated_baselines_use_the_opt_in_worker_count_and_keep_order() {
    let path = package("baseline-jobs-cli");
    write(
        &path,
        "src/lib.rs",
        r#"pub fn value() -> u32 { 9 }
#[test] fn aa() { assert_eq!(value(), 9); }
#[test] fn bb() { assert_eq!(value(), 9); }
"#,
    );
    wrapper(
        &path,
        r#"#!/bin/sh
set -eu
if [ -n "${TMT_TRACE:-}" ]; then
    touch "$FIXTURE_SCRATCH/$TMT_TEST"
    attempts=0
    until [ -f "$FIXTURE_SCRATCH/aa" ] && [ -f "$FIXTURE_SCRATCH/bb" ]; do
        attempts=$((attempts + 1))
        [ "$attempts" -lt 100 ] || exit 91
        sleep 0.02
    done
fi
exec "$@"
"#,
    );
    let scratch = path.join("target/baselines");
    fs::create_dir_all(&scratch).unwrap();
    let args = [
        "--json",
        "--lines",
        "src/lib.rs:1",
        "--test-wrapper",
        "wrap",
        "--baseline-jobs",
        "2",
    ];
    let run = command(&path, &args)
        .env("FIXTURE_SCRATCH", &scratch)
        .output()
        .unwrap();
    let report = json(&run, 0);
    let tests = report["analysis"]["test_selection"]["tests"]
        .as_array()
        .unwrap();
    assert!(tests[0].as_str().unwrap().ends_with("::aa"));
    assert!(tests[1].as_str().unwrap().ends_with("::bb"));
    assert!(String::from_utf8_lossy(&run.stderr).contains("baselines: 2/2 completed"));
}

#[test]
fn stage_external_supports_monorepo_parent_relative_paths_and_skips_repair_when_missing() {
    let root = Path::new(env!("CARGO_TARGET_TMPDIR")).join("monorepo-parent-relative");
    if root.exists() {
        fs::remove_dir_all(&root).unwrap();
    }
    let pkg = root.join("apps/api");
    fs::create_dir_all(pkg.join("src")).unwrap();
    write(&root, "proto/contract.txt", "v1-contract");
    write(&root, "testdata/cases.txt", "42");
    write(
        &pkg,
        "Cargo.toml",
        "[package]\nname = \"api\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n",
    );
    write(
        &pkg,
        "build.rs",
        r#"fn main() {
    let text = std::fs::read_to_string("../../proto/contract.txt").expect("proto missing");
    println!("cargo:rustc-env=CONTRACT_VER={}", text.trim());
}
"#,
    );
    write(
        &pkg,
        "src/lib.rs",
        r#"const CONTRACT: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../proto/contract.txt"));
const CASE: &str = include_str!("../../../testdata/cases.txt");

pub fn is_valid(n: u32) -> bool {
    n >= 4
}

#[test]
fn checks_boundary() {
    assert_eq!(CONTRACT.trim(), "v1-contract");
    assert_eq!(CASE.trim(), "42");
    assert!(!is_valid(3));
    assert!(is_valid(4));
    assert_eq!(env!("CONTRACT_VER"), "v1-contract");
}
"#,
    );

    // Without --stage-external, build.rs fails or include_str! fails without a pointless repair rebuild.
    let missing = command(&pkg, &["--lines", "src/lib.rs:4-6", "--json"])
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(1));
    let missing_err = String::from_utf8_lossy(&missing.stderr);
    assert!(
        !missing_err.contains("rebuilding without the sites that failed to compile"),
        "should not trigger repair rebuild: {missing_err}"
    );

    // Also verify missing include_str! alone (when build.rs succeeds) skips repair rebuild.
    let only_include_missing = command(
        &pkg,
        &[
            "--stage-external",
            "../../proto",
            "--lines",
            "src/lib.rs:4-6",
            "--json",
        ],
    )
    .output()
    .unwrap();
    assert_eq!(only_include_missing.status.code(), Some(1));
    let include_err = String::from_utf8_lossy(&only_include_missing.stderr);
    assert!(
        include_err.contains("unrelated to instrumentation"),
        "{include_err}"
    );
    assert!(
        !include_err.contains("rebuilding without the sites that failed to compile"),
        "include_str! failure must not trigger repair rebuild: {include_err}"
    );

    // With both external parent paths staged (shorthand + explicit mapping), build and tests succeed.
    let ok = command(
        &pkg,
        &[
            "--stage-external",
            "../../proto",
            "--stage-external",
            &format!("{}=../../testdata", root.join("testdata").display()),
            "--lines",
            "src/lib.rs:4-6",
            "--quiet",
            "--json",
        ],
    )
    .output()
    .unwrap();
    let report = json(&ok, 0);
    assert_eq!(
        report["analysis"]["stage_external"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn lib_flag_test_filter_and_selection_hints_work_end_to_end() {
    let path = package("test-filter-and-lib");
    write(
        &path,
        "Cargo.toml",
        "[package]\nname = \"my-api\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n",
    );
    write(
        &path,
        "src/lib.rs",
        r#"pub mod pagination {
    pub fn has_next(page: u32, total: u32) -> bool {
        page < total
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn first_page_has_next() {
            assert!(has_next(1, 2));
        }

        #[test]
        fn last_page_stops() {
            assert!(!has_next(2, 2));
        }
    }
}

#[cfg(test)]
mod unrelated {
    #[test]
    fn other_test() {
        assert_eq!(1 + 1, 2);
    }
}
"#,
    );
    write(&path, "src/main.rs", "fn main() {}\n");
    write(
        &path,
        "tests/smoke.rs",
        "#[test]\nfn smoke_checks() { assert!(my_api::pagination::has_next(1, 2)); assert!(!my_api::pagination::has_next(2, 2)); }\n",
    );

    // 1. --lib + --test-filter selects only pagination module unit tests.
    let filtered = command(
        &path,
        &[
            "--lib",
            "--test-filter",
            "pagination::tests",
            "--lines",
            "src/lib.rs:2-4",
            "--quiet",
            "--json",
        ],
    )
    .output()
    .unwrap();
    let report = json(&filtered, 0);
    let selected = report["analysis"]["test_selection"]["tests"]
        .as_array()
        .unwrap();
    assert_eq!(selected.len(), 2, "{selected:?}");
    assert!(
        selected
            .iter()
            .all(|t| t.as_str().unwrap().contains("pagination::tests"))
    );

    // 2. --test smoke runs only the integration test target.
    let integration = command(
        &path,
        &[
            "--test",
            "smoke",
            "--lines",
            "src/lib.rs:2-4",
            "--quiet",
            "--json",
        ],
    )
    .output()
    .unwrap();
    let int_report = json(&integration, 0);
    assert_eq!(
        int_report["analysis"]["test_selection"]["tests"],
        serde_json::json!(["my-api::test::smoke::smoke_checks"])
    );

    // 3. --test-target my-api matches the empty bin target and prints a --lib hint.
    let bin_only = command(
        &path,
        &[
            "--test-target",
            "my-api",
            "--lines",
            "src/lib.rs:2-4",
            "--quiet",
        ],
    )
    .output()
    .unwrap();
    assert_eq!(bin_only.status.code(), Some(1));
    let bin_err = String::from_utf8_lossy(&bin_only.stderr);
    assert!(
        bin_err.contains("--lib") && bin_err.contains("my_api"),
        "{bin_err}"
    );

    // 4. --include-test with a module prefix fails with a --test-filter hint.
    let prefix = command(
        &path,
        &[
            "--lib",
            "--include-test",
            "pagination::tests",
            "--lines",
            "src/lib.rs:2-4",
            "--quiet",
        ],
    )
    .output()
    .unwrap();
    assert_eq!(prefix.status.code(), Some(1));
    let prefix_err = String::from_utf8_lossy(&prefix.stderr);
    assert!(
        prefix_err.contains("--test-filter pagination::tests"),
        "{prefix_err}"
    );
}

#[test]
fn repair_cache_avoids_second_rebuild_on_repeat_run_and_invalidates_on_edit() {
    let path = package("repair-cache-e2e");
    // `late_init` triggers a definite-initialization error when instrumented with a probe,
    // forcing a repair rebuild on the first run.
    write(
        &path,
        "src/lib.rs",
        r#"pub fn late_init(flag: bool) -> u32 {
    let value;
    if flag {
        value = 1;
    } else {
        value = 2;
    }
    value
}

pub fn check(n: u32) -> bool {
    n >= 4
}

#[test]
fn covers_both() {
    assert_eq!(late_init(true), 1);
    assert_eq!(late_init(false), 2);
    assert!(!check(3));
    assert!(check(4));
}
"#,
    );

    let first = command(&path, &["--json"]).output().unwrap();
    let first_report = json(&first, 0);
    let first_stderr = String::from_utf8_lossy(&first.stderr);
    assert!(
        first_stderr.contains("rebuilding without the sites that failed to compile"),
        "expected repair rebuild on first run: {first_stderr}"
    );
    assert!(
        path.join("target/tmt/.tmt-repair-cache.json").is_file(),
        "repair cache file should be written after successful build"
    );

    // Second identical run must reuse the cached exclusion and skip the repair rebuild.
    let second = command(&path, &["--json"]).output().unwrap();
    let second_report = json(&second, 0);
    let second_stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        second_stderr.contains("reusing") && second_stderr.contains("cached repair exclusion"),
        "expected cached repair reuse message: {second_stderr}"
    );
    assert!(
        !second_stderr.contains("rebuilding without the sites that failed to compile"),
        "repeat run should not trigger a repair rebuild: {second_stderr}"
    );
    assert_eq!(first_report["skipped"], second_report["skipped"]);

    // Editing the source invalidates the cache so repair runs again cleanly.
    write(
        &path,
        "src/lib.rs",
        r#"pub fn late_init(flag: bool) -> u32 {
    let value;
    if flag {
        value = 10;
    } else {
        value = 20;
    }
    value
}

pub fn check(n: u32) -> bool {
    n >= 4
}

#[test]
fn covers_both() {
    assert_eq!(late_init(true), 10);
    assert_eq!(late_init(false), 20);
    assert!(!check(3));
    assert!(check(4));
}
"#,
    );
    let edited = command(&path, &["--json"]).output().unwrap();
    let _ = json(&edited, 0);
    let edited_stderr = String::from_utf8_lossy(&edited.stderr);
    assert!(
        !edited_stderr.contains("cached repair exclusion"),
        "source edit must invalidate repair cache: {edited_stderr}"
    );
    assert!(
        edited_stderr.contains("rebuilding without the sites that failed to compile"),
        "source edit should re-run repair: {edited_stderr}"
    );
}

#[test]
fn workspace_catalogue_has_one_snapshot_and_streamed_reports() {
    let path = package("catalogue-stream");
    write(
        &path,
        "Cargo.toml",
        "[workspace]\nmembers = [\"a\", \"b\"]\nresolver = \"2\"\n",
    );
    for name in ["a", "b"] {
        write(
            &path,
            &format!("{name}/Cargo.toml"),
            &format!("[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n"),
        );
        write(
            &path,
            &format!("{name}/src/lib.rs"),
            "pub fn value() -> u32 { 9 }\n#[test] fn checks() { assert_eq!(value(), 9); }\n",
        );
    }
    let run = command(&path, &["--workspace", "--list", "--jsonl"])
        .output()
        .unwrap();
    assert!(
        run.status.success(),
        "{}",
        String::from_utf8_lossy(&run.stderr)
    );
    let lines = String::from_utf8(run.stdout).unwrap();
    let records: Vec<Value> = lines
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), 2);
    for (record, name) in records.iter().zip(["a", "b"]) {
        assert_eq!(record["package"], name);
        assert_eq!(
            record["report"]["mutants"][0]["edits"][0]["file"],
            "src/lib.rs"
        );
    }
    let work = path.join("target/tmt-workspace");
    assert!(work.join(".catalogue/src/a/src/lib.rs").exists());
    assert!(!work.join("a").exists() && !work.join("b").exists());

    write(
        &path,
        "b/src/lib.rs",
        "compile_error!(\"second member failed\");\n",
    );
    let run = command(&path, &["--workspace", "--jsonl", "--quiet"])
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(1));
    let output = String::from_utf8(run.stdout).unwrap();
    let records: Vec<Value> = output
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0]["report"]["package"], "a");
    assert!(
        records[1]["error"]
            .as_str()
            .unwrap()
            .contains("second member failed")
    );
}

#[test]
fn checkpoint_cannot_reuse_a_different_test_selection_and_completes_an_empty_scope() {
    let path = package("checkpoint-selection");
    write(
        &path,
        "src/lib.rs",
        "pub fn value() -> u32 { 9 }\n#[test] fn detects() { assert_eq!(value(), 9); }\n#[test] fn accepts() { assert!(value() < 10); }\n",
    );
    let args = [
        "--source-only",
        "--checkpoint",
        "--quiet",
        "--json",
        "--include-test",
    ];
    let detected = json(
        &command(
            &path,
            &[&args[..], &["detects", "--lines", "src/lib.rs:1"]].concat(),
        )
        .output()
        .unwrap(),
        0,
    );
    let survived = json(
        &command(
            &path,
            &[&args[..], &["accepts", "--lines", "src/lib.rs:1"]].concat(),
        )
        .output()
        .unwrap(),
        2,
    );
    assert_eq!(detected["mutants_killed"], 2);
    assert_eq!(survived["mutants_killed"], 0);
    let empty = json(
        &command(
            &path,
            &[&args[..], &["accepts", "--lines", "src/lib.rs:99"]].concat(),
        )
        .output()
        .unwrap(),
        0,
    );
    assert_eq!(empty["evaluation"], "no_sites");
    let log = fs::read_to_string(empty["analysis"]["checkpoint"].as_str().unwrap()).unwrap();
    let events: Vec<Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events[events.len() - 2]["event"], "campaign_started");
    assert_eq!(events.last().unwrap()["event"], "campaign_completed");
    assert!(!events.iter().any(|row| row["event"] == "phase_resumed"));
}

#[cfg(unix)]
#[test]
fn failed_serial_baseline_stops_before_the_next_test() {
    let path = package("baseline-failure-cli");
    write(
        &path,
        "src/lib.rs",
        r#"pub fn value() -> u32 { 9 }
#[test] fn first() { assert_eq!(value(), 9); }
#[test] fn second() { assert_eq!(value(), 9); }
"#,
    );
    wrapper(
        &path,
        r#"#!/bin/sh
touch "$FIXTURE_SCRATCH/$TMT_TEST"
exit 91
"#,
    );
    let scratch = path.join("target/fail-fast");
    fs::create_dir_all(&scratch).unwrap();
    let output = command(&path, &["--json", "--test-wrapper", "wrap"])
        .env("FIXTURE_SCRATCH", &scratch)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(scratch.join("first").exists());
    assert!(!scratch.join("second").exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("::first"));
}

#[cfg(unix)]
#[test]
fn spanless_cargo_linker_error_survives_the_cli_failure() {
    let path = package("linker-diagnostic-cli");
    let tool = path.join("target/tools");
    fs::create_dir_all(&tool).unwrap();
    wrapper(
        &tool,
        r#"#!/bin/sh
if [ "$1" = test ]; then
    printf '%s\n' '{"reason":"compiler-message","message":{"level":"error","message":"linking with cc failed","rendered":"ld: library not found for -liconv","spans":[]}}'
    printf '%s\n' 'error: could not compile campaign-example' >&2
    exit 101
fi
exec "$FIXTURE_CARGO" "$@"
"#,
    );
    fs::rename(tool.join("wrap"), tool.join("cargo")).unwrap();
    let path_var = std::env::join_paths(
        std::iter::once(tool).chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let run = command(&path, &["--quiet", "--json"])
        .env("FIXTURE_CARGO", env!("CARGO"))
        .env("PATH", path_var)
        .output()
        .unwrap();
    assert_eq!(run.status.code(), Some(1));
    assert!(run.stdout.is_empty());
    let diagnostic = String::from_utf8_lossy(&run.stderr);
    assert!(
        diagnostic.contains("ld: library not found for -liconv"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("could not compile campaign-example"),
        "{diagnostic}"
    );
}

#[cfg(unix)]
#[test]
fn active_test_child_protects_work_copy_after_tmt_parent_exits() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};

    struct Release(PathBuf);
    impl Drop for Release {
        fn drop(&mut self) {
            let _ = fs::write(&self.0, b"");
        }
    }

    let path = package("interrupted-parent-cli");
    let gate = path.join("target/gate");
    fs::create_dir_all(&gate).unwrap();
    let release = Release(gate.join("release"));
    wrapper(
        &path,
        r#"#!/bin/sh
set -eu
touch "$FIXTURE_SCRATCH/ready"
left=9000
until [ -f "$FIXTURE_SCRATCH/release" ]; do
    left=$((left - 1))
    [ "$left" -gt 0 ] || exit 91
    sleep 0.02
done
exec "$@"
"#,
    );
    let mut child = command(&path, &["--quiet", "--test-wrapper", "wrap"])
        .env("FIXTURE_SCRATCH", &gate)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // The fixture is built before its tests start: on a busy machine that takes a while.
    // The waits end as soon as what they wait for is there, so they cost nothing when
    // it is quick.
    let began = Instant::now();
    while !gate.join("ready").exists() && began.elapsed() < Duration::from_secs(150) {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("tmt exited before the test started: {status}");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .unwrap()
            .success()
    );
    assert!(!child.wait().unwrap().success());
    assert!(gate.join("ready").exists());
    let list = || command(&path, &["--quiet", "--list"]).output().unwrap();
    let blocked = list();
    assert_eq!(
        blocked.status.code(),
        Some(1),
        "{}",
        String::from_utf8_lossy(&blocked.stderr)
    );
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("another tmt run"));
    drop(release);
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let result = list();
        if result.status.success() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn timeout_recheck_policy_distinguishes_parallel_and_serial_verdicts() {
    let path = package("timeout-recheck-cli");
    write(
        &path,
        "src/lib.rs",
        r#"pub fn value() -> bool { true }
#[test]
fn hangs_on_false() {
    if !value() { std::thread::sleep(std::time::Duration::from_secs(10)); }
}
"#,
    );
    let args = [
        "--json",
        "--lines",
        "src/lib.rs:1",
        "--jobs",
        "2",
        "--timeout",
        "1",
        "--timeout-recheck",
    ];
    let none = command(&path, &[&args[..], &["none"]].concat())
        .output()
        .unwrap();
    let unconfirmed = json(&none, 3);
    let count = unconfirmed["timeout_rechecks"]["first_pass"]
        .as_u64()
        .unwrap();
    assert!(count > 0, "{unconfirmed:#}");
    assert_eq!(unconfirmed["timeout_rechecks"]["unconfirmed"], count);
    assert_eq!(unconfirmed["mutants_killed"], 0);

    let serial = command(&path, &[&args[..], &["serial"]].concat())
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&serial.stderr).contains("serial timeout rechecks"),
        "{}",
        String::from_utf8_lossy(&serial.stderr)
    );
    let confirmed: Value = serde_json::from_slice(&serial.stdout).unwrap();
    assert_eq!(confirmed["timeout_rechecks"]["confirmed"], count);
    assert_eq!(confirmed["timeout_rechecks"]["unconfirmed"], 0);
    assert_eq!(confirmed["mutants_killed"], count);
}

#[test]
fn checkpoint_accounts_for_effective_build_script_environment() {
    let path = package("checkpoint-build-script");
    write(
        &path,
        "build.rs",
        r#"
fn main() {
    let mode = std::env::var("FIXTURE_MODE_FILE").unwrap();
    println!("cargo:rerun-if-changed={mode}");
    println!("cargo:rustc-env=FIXTURE_MODE={}", std::fs::read_to_string(mode).unwrap());
}
"#,
    );
    write(
        &path,
        "src/lib.rs",
        r#"pub fn value() -> u32 { 9 }
#[test] fn verifies() {
    if env!("FIXTURE_MODE") == "strict" { assert_eq!(value(), 9); }
    else { assert!(value() < 10); }
}
"#,
    );
    write(&path, "target/mode", "strict");
    let args = [
        "--source-only",
        "--checkpoint",
        "--lines",
        "src/lib.rs:1",
        "--quiet",
        "--json",
    ];
    let run = || {
        command(&path, &args)
            .env("FIXTURE_MODE_FILE", path.join("target/mode"))
            .output()
            .unwrap()
    };
    assert_eq!(json(&run(), 0)["mutants_killed"], 2);
    write(&path, "target/mode", "loose");
    assert_eq!(json(&run(), 2)["mutants_killed"], 0);
}

#[test]
fn explicit_timeout_warns_once_for_the_selected_slow_test() {
    let path = package("explicit-timeout-slow-baseline");
    write(
        &path,
        "src/lib.rs",
        r#"pub fn is_positive(value: i32) -> bool { value > 0 }
#[test]
fn slow_test() {
    std::thread::sleep(std::time::Duration::from_millis(1_300));
    assert!(is_positive(1));
    assert!(!is_positive(0));
}
#[test]
fn fast_test() {
    assert!(is_positive(1));
    assert!(!is_positive(0));
}
"#,
    );
    let args = [
        "--json",
        "--jobs",
        "8",
        "--no-effects",
        "--timeout-recheck",
        "none",
        "--include-test",
        "slow_test",
        "--timeout",
        "1",
    ];
    let run = command(&path, &args).output().unwrap();
    let report = json(&run, 3);
    assert_eq!(report["analysis"]["timeout_seconds"], 1);
    assert!(report["timeout_rechecks"]["unconfirmed"].as_u64().unwrap() > 1);
    let stderr = String::from_utf8_lossy(&run.stderr);
    let mut warnings = stderr.lines().filter(|line| line.contains("warning:"));
    let warning = warnings
        .next()
        .unwrap_or_else(|| panic!("missing warning:\n{stderr}"));
    assert!(
        warning.contains("slow_test") && warning.contains("--timeout 1"),
        "{warning}"
    );
    assert!(
        warning.contains("took ") || warning.contains("still running after "),
        "{warning}"
    );
    assert!(warnings.next().is_none(), "{stderr}");
    assert!(stderr.find(warning).unwrap() < stderr.find("waiting for mutant tests").unwrap());

    let run = command(
        &path,
        &[
            "--json",
            "--quiet",
            "--no-effects",
            "--include-test",
            "fast_test",
            "--timeout",
            "1",
        ],
    )
    .output()
    .unwrap();
    let report = json(&run, 0);
    assert_eq!(report["mutants_timed_out"], 0);
    assert!(!String::from_utf8_lossy(&run.stderr).contains("warning:"));
}

#[test]
fn timeout_warning_does_not_use_the_whole_binary_for_an_automatic_filter() {
    let path = package("explicit-timeout-filtered-baseline");
    write(
        &path,
        "src/lib.rs",
        r#"pub fn is_positive(value: i32) -> bool { value > 0 }
#[test]
fn slow_unrelated_test() {
    std::thread::sleep(std::time::Duration::from_millis(1_300));
}
#[test]
fn relevant_test() {
    assert!(is_positive(1));
    assert!(!is_positive(0));
}
"#,
    );
    let run = command(
        &path,
        &[
            "--json",
            "--quiet",
            "--lines",
            "src/lib.rs:1",
            "--no-effects",
            "--timeout",
            "1",
        ],
    )
    .output()
    .unwrap();
    let report = json(&run, 0);
    assert!(report["mutants_run"].as_u64().unwrap() > 0);
    assert!(!String::from_utf8_lossy(&run.stderr).contains("warning:"));
}

#[test]
fn source_only_timeout_warning_remains_visible_in_quiet_json_mode() {
    let path = package("explicit-timeout-source-baseline");
    write(
        &path,
        "src/lib.rs",
        r#"pub fn value() -> bool { true }
#[test]
fn slow_test() {
    std::thread::sleep(std::time::Duration::from_millis(1_300));
    assert!(value());
}
"#,
    );
    let run = command(
        &path,
        &[
            "--json",
            "--quiet",
            "--source-only",
            "--lines",
            "src/lib.rs:1",
            "--include-test",
            "slow_test",
            "--timeout",
            "1",
        ],
    )
    .output()
    .unwrap();
    let report = json(&run, 3);
    assert_eq!(report["mutants_timed_out"], 1);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.contains("warning:")
            && stderr.contains("slow_test")
            && stderr.contains("--timeout 1"),
        "{stderr}"
    );
}

#[cfg(unix)]
#[test]
fn tracing_overhead_alone_does_not_trigger_the_timeout_warning() {
    let path = package("explicit-timeout-trace-overhead");
    wrapper(
        &path,
        r#"#!/bin/sh
if [ -n "${TMT_TRACE:-}" ]; then
    sleep 1.3
fi
exec "$@"
"#,
    );
    let run = command(
        &path,
        &[
            "--json",
            "--quiet",
            "--no-effects",
            "--test-wrapper",
            "wrap",
            "--timeout",
            "1",
        ],
    )
    .output()
    .unwrap();
    let report = json(&run, 0);
    assert!(report["mutants_run"].as_u64().unwrap() > 0);
    assert!(!String::from_utf8_lossy(&run.stderr).contains("warning:"));
}

#[test]
fn timeout_warning_uses_the_baseline_when_the_filter_names_every_test() {
    let path = package("explicit-timeout-all-test-names");
    write(
        &path,
        "src/lib.rs",
        r#"pub fn is_positive(value: i32) -> bool { value > 0 }
#[test]
fn slow_test() {
    std::thread::sleep(std::time::Duration::from_millis(1_300));
    assert!(is_positive(1));
    assert!(!is_positive(0));
}
"#,
    );
    let run = command(
        &path,
        &[
            "--quiet",
            "--json",
            "--no-effects",
            "--jobs",
            "8",
            "--timeout-recheck",
            "none",
            "--lines",
            "src/lib.rs:1",
            "--timeout",
            "1",
        ],
    )
    .output()
    .unwrap();
    let report = json(&run, 3);
    assert!(report["timeout_rechecks"]["unconfirmed"].as_u64().unwrap() > 1);
    let stderr = String::from_utf8_lossy(&run.stderr);
    assert_eq!(stderr.lines().count(), 1, "{stderr}");
    assert!(
        stderr.contains("warning:")
            && stderr.contains("campaign_example")
            && stderr.contains("--timeout 1"),
        "{stderr}"
    );
}
