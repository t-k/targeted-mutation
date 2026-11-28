mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use common::fixture_command;
use serde_json::Value;

fn write(path: &Path, relative: &str, content: &str) {
    let path = path.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn temp_crate(name: &str, lib_rs: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_TARGET_TMPDIR")).join(name);
    if path.exists() {
        fs::remove_dir_all(&path).unwrap();
    }
    fs::create_dir_all(path.join("src")).unwrap();
    write(
        &path,
        "Cargo.toml",
        &format!(
            "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2021\"\n[workspace]\n"
        ),
    );
    write(&path, "src/lib.rs", lib_rs);
    path
}

fn tmt_command(path: &Path, args: &[&str]) -> Command {
    let mut command = fixture_command(env!("CARGO_BIN_EXE_cargo-tmt"));
    command.arg(path).args(args);
    command
}

#[cfg(unix)]
fn send_signal(pid: u32, sig: i32) -> i32 {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid as i32, sig) }
}

#[cfg(unix)]
fn process_alive(pid: u32) -> bool {
    const EPERM: i32 = 1;
    let rc = send_signal(pid, 0);
    if rc == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(EPERM)
}

#[cfg(unix)]
fn wait_for_pid_file(path: &Path, timeout: Duration) -> u32 {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if let Ok(text) = fs::read_to_string(path)
            && let Ok(pid) = text.trim().parse::<u32>()
            && pid > 1
        {
            return pid;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "timed out waiting for active worker PID file {}",
        path.display()
    );
}

#[cfg(unix)]
fn run_sigint_reap_scenario(name: &str, ignore_sigterm: bool) {
    let marker = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("{name}-worker.pid"));
    let _ = fs::remove_file(&marker);
    let ignore_term_code = if ignore_sigterm {
        r#"
            unsafe extern "C" {
                fn signal(sig: i32, handler: usize) -> usize;
            }
            // SIGTERM = 15, SIG_IGN = 1
            unsafe { signal(15, 1) };
        "#
    } else {
        ""
    };
    let lib_rs = format!(
        r#"
pub fn positive(x: i32) -> bool {{
    x > 0
}}

#[test]
fn blocks_under_mutant_only() {{
    if positive(1) {{
        return;
    }}
    {ignore_term_code}
    let tmp = "{marker}.tmp";
    std::fs::write(tmp, format!("{{}}\n", std::process::id())).unwrap();
    std::fs::rename(tmp, "{marker}").unwrap();
    std::thread::sleep(std::time::Duration::from_secs(60));
}}
"#,
        marker = marker.display(),
        ignore_term_code = ignore_term_code,
    );
    let pkg = temp_crate(name, &lib_rs);
    let work = pkg.join("work");
    let work_str = work.to_str().unwrap();

    let mut child = tmt_command(
        &pkg,
        &[
            "--no-config",
            "--no-effects",
            "--work-dir",
            work_str,
            "--timeout",
            "60",
            "--json",
        ],
    )
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();

    let worker_pid = wait_for_pid_file(&marker, Duration::from_secs(45));
    assert!(
        process_alive(worker_pid),
        "expected mutant test worker {worker_pid} to be alive before SIGINT"
    );

    // While the worker is alive, mutual exclusion on the work directory must hold.
    let contended = tmt_command(&pkg, &["--no-config", "--work-dir", work_str, "--json"])
        .output()
        .unwrap();
    assert!(
        !contended.status.success(),
        "second run must be rejected while work directory is locked"
    );
    let contended_err = String::from_utf8_lossy(&contended.stderr);
    assert!(
        contended_err.contains("another tmt run is using"),
        "expected lock contention message, got: {contended_err}"
    );

    let sigint_sent = Instant::now();
    assert_eq!(send_signal(child.id(), 2), 0, "sending SIGINT to cargo-tmt");

    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if sigint_sent.elapsed() > Duration::from_secs(15) {
            let _ = child.kill();
            let _ = child.wait();
            if process_alive(worker_pid) {
                let _ = send_signal(worker_pid, 9);
            }
            panic!("cargo-tmt did not exit within 15s after SIGINT");
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let elapsed = sigint_sent.elapsed();
    assert_eq!(
        status.code(),
        Some(130),
        "expected exit code 130 on SIGINT, got {status:?}"
    );
    if !ignore_sigterm {
        assert!(
            elapsed < Duration::from_millis(1800),
            "SIGTERM-obeying worker should exit promptly before grace expires (took {elapsed:?})"
        );
    }

    // The owned test subprocess must already be gone (reaped before CLI exit).
    assert!(
        !process_alive(worker_pid),
        "mutant worker {worker_pid} survived after cargo-tmt exited"
    );

    // A follow-on invocation must now acquire the work directory cleanly.
    let follow_on = tmt_command(&pkg, &["--no-config", "--work-dir", work_str, "--list"])
        .output()
        .unwrap();
    assert!(
        follow_on.status.success(),
        "follow-on invocation failed to acquire work directory: {}",
        String::from_utf8_lossy(&follow_on.stderr)
    );
    let _ = fs::remove_file(&marker);
}

#[cfg(unix)]
#[test]
fn sigint_reaps_active_mutant_worker_ignoring_sigterm_and_releases_work_lock() {
    run_sigint_reap_scenario("sigint-reap-ignore-term", true);
}

#[cfg(unix)]
#[test]
fn sigint_reaps_active_mutant_worker_obeying_sigterm_promptly() {
    run_sigint_reap_scenario("sigint-reap-obey-term", false);
}

#[test]
fn baseline_timeout_reports_structured_diagnostic_and_forbids_partial_trace() {
    let lib_rs = r#"
pub fn check(x: i32) -> bool {
    x > 0
}

#[test]
fn fast_test() {
    assert!(check(1));
    assert!(!check(0));
}

#[test]
fn slow_test() {
    std::thread::sleep(std::time::Duration::from_secs(3));
    assert!(check(5));
}
"#;
    let pkg = temp_crate("baseline-timeout-diag", lib_rs);
    let work = pkg.join("work");
    let work_str = work.to_str().unwrap();

    // Explicit --baseline-timeout 1 should fail the baseline stage on slow_test,
    // emit structured rerun diagnostics, and never report a partial baseline as complete.
    let out = tmt_command(
        &pkg,
        &[
            "--no-config",
            "--lib",
            "--no-effects",
            "--baseline-timeout",
            "1",
            "--timeout",
            "10",
            "--work-dir",
            work_str,
            "--json",
        ],
    )
    .output()
    .unwrap();
    assert_eq!(
        out.status.code(),
        Some(1),
        "expected exit 1 on baseline timeout"
    );
    assert!(
        out.stdout.is_empty(),
        "partial baseline must never produce a report on stdout: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    for expected in [
        "baseline timeout",
        "baseline_trace",
        "slow_test",
        "budget 1s",
        "partial baseline",
        "baseline-timeout.json",
        "--include-test slow_test",
        "--exclude-test slow_test",
    ] {
        assert!(
            stderr.contains(expected),
            "stderr missing `{expected}`:\n{stderr}"
        );
    }

    let diag_path = work.join("baseline-timeout.json");
    let diag: Value = serde_json::from_str(&fs::read_to_string(&diag_path).unwrap()).unwrap();
    assert_eq!(diag["schema_version"], 1);
    assert_eq!(diag["package"], "baseline-timeout-diag");
    assert_eq!(diag["stage"], "baseline_trace");
    assert_eq!(diag["test"], "slow_test");
    assert_eq!(diag["budget_seconds"], 1);
    assert_eq!(diag["partial_baseline"], true);
    assert_eq!(diag["total_baselines"], 2);
    assert!(
        diag["elapsed_seconds"].as_f64().unwrap() >= 0.9,
        "unexpected elapsed_seconds: {diag:#}"
    );
    assert_eq!(
        diag["rerun_args"],
        serde_json::json!(["--include-test", "slow_test"])
    );
    assert_eq!(
        diag["exclude_args"],
        serde_json::json!(["--exclude-test", "slow_test"])
    );

    // Using the emitted exclude_args lets the remaining tests complete cleanly.
    let exclude_args: Vec<String> = diag["exclude_args"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let mut rerun_args = vec![
        "--no-config".to_string(),
        "--lib".to_string(),
        "--no-effects".to_string(),
        "--baseline-timeout".to_string(),
        "1".to_string(),
        "--work-dir".to_string(),
        work_str.to_string(),
        "--json".to_string(),
    ];
    rerun_args.extend(exclude_args);
    let rerun_refs: Vec<&str> = rerun_args.iter().map(String::as_str).collect();
    let rerun = tmt_command(&pkg, &rerun_refs).output().unwrap();
    assert_eq!(
        rerun.status.code(),
        Some(0),
        "rerun excluding slow_test should pass: {}",
        String::from_utf8_lossy(&rerun.stderr)
    );
    let report: Value = serde_json::from_slice(&rerun.stdout).unwrap();
    assert_eq!(report["analysis"]["baseline_timeout_seconds"], 1);
    assert!(
        !diag_path.exists(),
        "stale baseline-timeout.json should be removed on a successful run"
    );

    // Config file `baseline_timeout = 1` triggers the same failure; CLI override lifts it.
    write(&pkg, "tmt.toml", "baseline_timeout = 1\nlib = true\n");
    let cfg_fail = tmt_command(&pkg, &["--no-effects", "--work-dir", work_str, "--json"])
        .output()
        .unwrap();
    assert_eq!(cfg_fail.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&cfg_fail.stderr).contains("budget 1s"),
        "expected config baseline_timeout to apply: {}",
        String::from_utf8_lossy(&cfg_fail.stderr)
    );

    let cli_override = tmt_command(
        &pkg,
        &[
            "--no-effects",
            "--baseline-timeout",
            "10",
            "--work-dir",
            work_str,
            "--json",
        ],
    )
    .output()
    .unwrap();
    assert_eq!(
        cli_override.status.code(),
        Some(0),
        "CLI --baseline-timeout 10 should override tmt.toml: {}",
        String::from_utf8_lossy(&cli_override.stderr)
    );
    let override_report: Value = serde_json::from_slice(&cli_override.stdout).unwrap();
    assert_eq!(override_report["analysis"]["baseline_timeout_seconds"], 10);

    // Source-only mode reports stage `baseline_untraced`.
    let source_only = tmt_command(
        &pkg,
        &[
            "--no-config",
            "--source-only",
            "--lib",
            "--baseline-timeout",
            "1",
            "--work-dir",
            work_str,
            "--json",
        ],
    )
    .output()
    .unwrap();
    assert_eq!(source_only.status.code(), Some(1));
    let source_diag: Value =
        serde_json::from_str(&fs::read_to_string(&diag_path).unwrap()).unwrap();
    assert_eq!(source_diag["stage"], "baseline_untraced");
    assert_eq!(source_diag["partial_baseline"], true);
}

#[test]
fn parallel_baseline_timeouts_keep_stderr_and_diagnostic_json_consistent() {
    let lib_rs = r#"
pub fn check(x: i32) -> bool {
    x > 0
}

#[test]
fn slow_a() {
    // Reach a probe first, then exceed the 1s budget.
    assert!(check(1));
    std::thread::sleep(std::time::Duration::from_secs(3));
}

#[test]
fn slow_b() {
    assert!(check(2));
    std::thread::sleep(std::time::Duration::from_secs(3));
}
"#;
    let pkg = temp_crate("parallel-baseline-timeout-sync", lib_rs);
    let work = pkg.join("work");
    let work_str = work.to_str().unwrap();

    let out = tmt_command(
        &pkg,
        &[
            "--no-config",
            "--lib",
            "--baseline-jobs",
            "2",
            "--baseline-timeout",
            "1",
            "--no-effects",
            "--work-dir",
            work_str,
            "--json",
        ],
    )
    .output()
    .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(out.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&out.stderr);
    let diag: Value =
        serde_json::from_str(&fs::read_to_string(work.join("baseline-timeout.json")).unwrap())
            .unwrap();
    let diag_test = diag["test"].as_str().unwrap();
    let diag_log = diag["log"].as_str().unwrap();
    assert!(
        stderr.contains(&format!("test `{diag_test}`")),
        "stderr and diagnostic JSON disagreed on test `{diag_test}`:\n{stderr}"
    );
    assert!(
        stderr.contains(diag_log),
        "stderr and diagnostic JSON disagreed on log `{diag_log}`:\n{stderr}"
    );
}
