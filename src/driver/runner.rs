//! Test binary execution, timeouts, process group cleanup, baseline timing, and worker pool.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::propagation::{self};
use crate::trace::Trace;
use crate::work_lock::WorkLock;
use crate::{leftovers, progress, witness};

use super::*;

/// How a group of tests is started.
#[derive(Clone)]
pub(super) enum Runner {
    /// A libtest executable.
    Exe(PathBuf),
    Isolated {
        exe: PathBuf,
        test: String,
        ignored: bool,
        wrapper: Option<PathBuf>,
    },
    Nextest {
        copy: PathBuf,
        target_dir: PathBuf,
        profile: Option<String>,
        opt_level: Option<u8>,
        flags: Vec<String>,
    },
    /// The package's doc tests, through `cargo test --doc` in the working copy. rustdoc
    /// compiles them on every run, and they cannot be selected by name.
    Doc {
        copy: PathBuf,
        target_dir: PathBuf,
        package: String,
        opt_level: Option<u8>,
        flags: Vec<String>,
    },
}

impl Runner {
    pub(super) fn executable(&self) -> Option<&Path> {
        match self {
            Self::Exe(exe) | Self::Isolated { exe, .. } => Some(exe),
            Self::Doc { .. } | Self::Nextest { .. } => None,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum BaselineTiming {
    Completed(Duration),
    StillRunning(Duration),
}

pub(super) fn explicit_timeout_conflict(
    timeout: Option<Duration>,
    baseline: Duration,
    selected_count: Option<usize>,
    test_count: usize,
) -> Option<Duration> {
    let entire_runner = selected_count.is_none_or(|count| count == test_count);
    timeout.filter(|&limit| entire_runner && baseline >= limit)
}

pub(super) type FilteredTimings =
    std::sync::Mutex<BTreeMap<(usize, Vec<String>), std::sync::Arc<std::sync::OnceLock<Duration>>>>;

pub(super) struct TestBinary {
    pub(super) name: String,
    /// Directory the tests run in: their package's, as under `cargo test`.
    pub(super) cwd: PathBuf,
    /// Where the marks of its runs are made: a directory of this analysis alone.
    pub(super) marks: PathBuf,
    pub(super) git_ceiling: PathBuf,
    pub(super) work_lock: Option<Arc<WorkLock>>,
    /// The tests the binary contains, by the name libtest selects them with.
    pub(super) tests: BTreeSet<String>,
    pub(super) runner: Runner,
    /// How long the tests take when nothing is recorded: what a mutant is measured against.
    pub(super) baseline: Duration,
    /// A comparable measurement, with trace recording disabled.
    pub(super) untraced_baseline: Option<BaselineTiming>,
    pub(super) timeout_diagnostic: std::sync::Once,
    /// How long they took while recording.
    pub(super) traced: Duration,
    pub(super) timeout: Option<Duration>,
    pub(super) wrapper_grace: Duration,
    pub(super) report_slow: bool,
    pub(super) filtered_timings: FilteredTimings,
    pub(super) trace: Trace,
    pub(super) effects_baseline: std::sync::OnceLock<propagation::BaselineIndex>,
}

impl TestBinary {
    pub(super) fn mutant_limit(&self, baseline: Duration) -> Duration {
        self.timeout.unwrap_or_else(|| mutant_timeout(baseline))
    }

    pub(super) fn warn_timeout_conflict(&self, filter: Option<&[String]>) {
        // The initial timing can include tracing or tests outside this filter.
        let Some(limit) = explicit_timeout_conflict(
            self.timeout,
            self.baseline,
            filter.map(<[String]>::len),
            self.tests.len(),
        ) else {
            return;
        };
        self.timeout_diagnostic.call_once(|| {
            let timing = self.untraced_baseline.or_else(|| {
                let started = Instant::now();
                // Bound the check by the original traced run. If it times
                // out, exclude time spent stopping it from the lower bound.
                match run_binary(self, None, &[], self.traced, RunOutput::Quiet, 1) {
                    Ok(Outcome::Pass) => Some(BaselineTiming::Completed(started.elapsed())),
                    Ok(Outcome::Timeout) => Some(BaselineTiming::StillRunning(self.traced)),
                    _ => None,
                }
            });
            let (elapsed, message) = match timing {
                Some(BaselineTiming::Completed(elapsed)) => (elapsed, "took"),
                Some(BaselineTiming::StillRunning(elapsed)) => (elapsed, "was still running after"),
                None => return,
            };
            if elapsed >= limit {
                eprintln!(
                    "tmt: warning: unmutated baseline for `{}` {message} {:.3}s, meeting or exceeding --timeout {}",
                    crate::report::sanitize_inline(&self.name),
                    elapsed.as_secs_f64(),
                    limit.as_secs()
                );
            }
        });
    }

    pub(super) fn filtered_limit(
        &self,
        filter: Option<&[String]>,
        traced: bool,
        sharing: usize,
    ) -> Duration {
        let baseline = if traced { self.traced } else { self.baseline };
        let Some(filter) =
            filter.filter(|names| !names.is_empty() && names.len() < self.tests.len())
        else {
            return self.mutant_limit(baseline);
        };
        if self.timeout.is_some() || baseline < SLOW_TRACE || !matches!(self.runner, Runner::Exe(_))
        {
            return self.mutant_limit(baseline);
        }
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        let threads = test_threads(cores, sharing);
        let timed = {
            let mut groups = self
                .filtered_timings
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            groups
                .entry((threads, filter.to_vec()))
                .or_default()
                .clone()
        };
        let timing = *timed.get_or_init(|| {
            let untraced = self.baseline;
            let started = Instant::now();
            // One measured group is shared by all jobs with this exact filter.
            // A failing or inconclusive timing run keeps the conservative limit.
            match run_binary(
                self,
                Some(filter),
                &[],
                untraced,
                RunOutput::Quiet,
                sharing.max(1),
            ) {
                Ok(Outcome::Pass) => started.elapsed(),
                _ => untraced,
            }
        });
        self.mutant_limit(if traced {
            timing.max(
                timing.mul_f64(self.traced.as_secs_f64() / self.baseline.as_secs_f64().max(0.001)),
            )
        } else {
            timing
        })
    }
}

#[derive(Debug, PartialEq, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Outcome {
    Pass,
    Fail,
    Timeout,
    /// The run gave up: it could not go on as the mutant it was started with.
    Unable,
}

impl From<Outcome> for progress::Outcome {
    fn from(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Pass => Self::Survived,
            Outcome::Fail => Self::Failed,
            Outcome::Timeout => Self::TimedOut,
            Outcome::Unable => Self::Unable,
        }
    }
}

/// Tests to pass to the binary, or `None` for "run everything".
///
/// A name that is not one of the binary's tests comes from a thread the test named
/// itself; passing it to libtest would select nothing and "pass". Such observations
/// belong to some unknown test, like those of unnamed threads.
pub(super) fn test_filter(
    tests: &BTreeSet<String>,
    known: &BTreeSet<String>,
) -> Option<Vec<String>> {
    if tests.len() > MAX_TEST_FILTERS || !tests.iter().all(|t| known.contains(t)) {
        None
    } else {
        Some(tests.iter().cloned().collect())
    }
}

/// Names of the tests in libtest's `--list --format terse` output.
pub(super) fn parse_test_list(stdout: &str) -> BTreeSet<String> {
    stdout
        .lines()
        .filter_map(|line| line.strip_suffix(": test"))
        .map(str::to_string)
        .collect()
}

/// What makes cargo build the tests (and what they depend on) at an optimisation level.
pub(super) fn list_tests(
    exe: &Path,
    cwd: &Path,
    work: &Path,
    ignored: bool,
    strict: bool,
    lock: Option<&WorkLock>,
) -> Result<BTreeSet<String>> {
    let mut command = Command::new(exe);
    isolate_copy_command(&mut command, work);
    if let Some(lock) = lock {
        lock.retain_for(&mut command);
    }
    command.args(["--list", "--format", "terse"]);
    if ignored {
        command.arg("--ignored");
    }
    let listing = command
        .current_dir(cwd)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("listing tests of {}", exe.display()))?;
    if !listing.status.success() {
        if !strict {
            return Ok(BTreeSet::new());
        }
        bail!(
            "listing tests of {} failed: {}",
            exe.display(),
            String::from_utf8_lossy(&listing.stderr)
        );
    }
    Ok(parse_test_list(&String::from_utf8_lossy(&listing.stdout)))
}

/// Overflow checks and debug assertions stay as the profile has them.
pub(super) fn profile_env(opt_level: Option<u8>) -> Vec<(&'static str, String)> {
    opt_level.map_or(vec![], |level| {
        vec![
            ("CARGO_PROFILE_DEV_OPT_LEVEL", level.to_string()),
            ("CARGO_PROFILE_TEST_OPT_LEVEL", level.to_string()),
        ]
    })
}

/// A trace run longer than this is followed by a run without recording, for the timing.
pub(super) const SLOW_TRACE: Duration = Duration::from_secs(2);

pub(super) fn mutant_timeout(baseline: Duration) -> Duration {
    Duration::from_secs(5) + baseline * 5
}

pub(super) const TEST_POLL_INITIAL: Duration = Duration::from_micros(250);
pub(super) const TEST_POLL_CAP: Duration = Duration::from_millis(5);

/// Short-running tests often finish before the old polling interval. Back off for
/// longer runs so they do not pay for frequent exit and failure-output checks.
pub(super) fn next_test_poll(delay: &mut Duration, remaining: Duration) -> Duration {
    let wait = (*delay).min(remaining);
    *delay = (*delay * 2).min(TEST_POLL_CAP);
    wait
}

/// The test threads of one test binary when `sharing` of them run at the same time: its
/// share of the cores. Never fewer than two, because the harness runs the tests of a
/// single thread on the main thread, where they have no name to be told apart by.
pub(super) fn test_threads(cores: usize, sharing: usize) -> usize {
    (cores / sharing.max(1)).max(2)
}

pub(super) enum RunOutput<'a> {
    Quiet,
    FirstFailure,
    Capture(&'a fs::File),
}

pub(super) fn nextest_outcome(code: Option<i32>) -> Result<Outcome> {
    match code {
        Some(0) => Ok(Outcome::Pass),
        // Other exit codes indicate infrastructure errors or no selected tests.
        Some(100) => Ok(Outcome::Fail),
        _ => bail!("cargo nextest failed outside a test (exit code {code:?})"),
    }
}

pub(super) fn run_binary(
    bin: &TestBinary,
    tests: Option<&[String]>,
    env: &[(&str, &str)],
    timeout: Duration,
    output: RunOutput<'_>,
    sharing: usize,
) -> Result<Outcome> {
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    let threads = test_threads(cores, sharing);
    static RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let unable_mark = reserve_file(&bin.marks, "unable", &RUNS)?;

    let mut cmd = match &bin.runner {
        Runner::Exe(exe) => {
            let mut cmd = Command::new(exe);
            // With one test thread libtest runs tests on the main thread, and the runtime
            // could no longer tell which test it is in.
            cmd.arg(format!("--test-threads={threads}"));
            if let Some(tests) = tests {
                cmd.arg("--exact").args(tests);
            }
            cmd.current_dir(&bin.cwd)
                .env("CARGO_MANIFEST_DIR", &bin.cwd);
            cmd
        }
        Runner::Isolated {
            exe,
            test,
            ignored,
            wrapper,
        } => {
            let mut cmd = if let Some(wrapper) = wrapper {
                let mut cmd = Command::new(wrapper);
                cmd.arg(exe);
                cmd
            } else {
                Command::new(exe)
            };
            cmd.args(["--exact", "--test-threads=1", test]);
            if *ignored {
                cmd.arg("--ignored");
            }
            cmd.current_dir(&bin.cwd)
                .env("CARGO_MANIFEST_DIR", &bin.cwd);
            cmd
        }
        Runner::Nextest {
            copy,
            target_dir,
            profile,
            opt_level,
            flags,
        } => {
            let mut cmd = Command::new("cargo");
            cmd.args(["nextest", "run", "--no-tests=fail"]);
            if let Some(profile) = profile {
                cmd.args(["--profile", profile]);
            }
            cmd.args(flags)
                .current_dir(copy)
                .env("CARGO_TARGET_DIR", target_dir)
                .env("CARGO_BUILD_BUILD_DIR", target_dir)
                .envs(profile_env(*opt_level));
            cmd
        }
        Runner::Doc {
            copy,
            target_dir,
            package,
            opt_level,
            flags,
        } => {
            let mut cmd = Command::new("cargo");
            cmd.args(["test", "--doc", "-p", package])
                .args(flags)
                .current_dir(copy)
                .env("CARGO_TARGET_DIR", target_dir)
                .env("CARGO_BUILD_BUILD_DIR", target_dir)
                .envs(profile_env(*opt_level));
            cmd
        }
    };
    isolate_copy_command(&mut cmd, &bin.git_ceiling);
    if let Some(lock) = &bin.work_lock {
        lock.retain_for(&mut cmd);
    }
    cmd.env_remove("TMT_TRACE")
        .env_remove("TMT_MUTANT")
        .env_remove("TMT_TEST")
        .env_remove(witness::OUTPUT_ENV)
        .env("TMT_UNABLE", &unable_mark)
        .envs(env.iter().copied())
        .stdin(Stdio::null());
    if let Runner::Isolated { test, .. } = &bin.runner {
        cmd.env("TMT_TEST", test);
    }
    let nextest = matches!(bin.runner, Runner::Nextest { .. });
    let mut output_to_watch = None;
    match output {
        RunOutput::Capture(file) => {
            cmd.stdout(file.try_clone()?).stderr(file.try_clone()?);
        }
        RunOutput::FirstFailure if !nextest && !matches!(bin.runner, Runner::Isolated { .. }) => {
            // A regular file has a readable end even if a descendant keeps stdout.
            // Reuse the per-run marker, whose contents are never interpreted by probes.
            let writer = fs::OpenOptions::new().append(true).open(&unable_mark)?;
            output_to_watch = Some(FailureOutput::new(fs::File::open(&unable_mark)?));
            cmd.stdout(writer).stderr(Stdio::null());
        }
        RunOutput::Quiet | RunOutput::FirstFailure => {
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    // Its own process group, so that a timeout can stop whatever the tests started too.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    // What the tests leave running outside their group is found by a file they pass on.
    let mut watch = leftovers::Watch::new(reserve_file(&bin.marks, "held", &RUNS)?);
    let mut child = match watch.spawn(&mut cmd) {
        Ok(child) => child,
        Err(error) => {
            watch.finish(true);
            let _ = release_mark(&unable_mark);
            return Err(error).with_context(|| format!("starting the tests of `{}`", bin.name));
        }
    };
    let group = child.id();
    let mut exit_waiter = ExitWaiter::new(group);
    let kill = |child: &mut std::process::Child| {
        if nextest {
            // Nextest creates a process group per test. Let its signal handler
            // forward SIGTERM and reap those groups before we return.
            stop_nextest(child, Duration::from_secs(12));
            return;
        }
        kill_test_group(group);
        let _ = child.kill();
        let _ = child.wait();
    };
    let started = Instant::now();
    let mut delay = TEST_POLL_INITIAL;
    let mut waiting_reported = false;
    let running_mutant = env.iter().any(|(key, _)| *key == "TMT_MUTANT")
        || matches!(output, RunOutput::FirstFailure);
    let result: Result<Outcome> = (|| loop {
        if let Some(status) = child.try_wait()? {
            if matches!(bin.runner, Runner::Isolated { .. }) || output_to_watch.is_some() {
                kill_test_group(group);
            }
            if nextest {
                return nextest_outcome(status.code());
            }
            return Ok(if status.success() {
                Outcome::Pass
            } else {
                Outcome::Fail
            });
        }
        if output_to_watch
            .as_mut()
            .map(FailureOutput::failed)
            .transpose()?
            .unwrap_or(false)
        {
            kill(&mut child);
            return Ok(Outcome::Fail);
        }
        let elapsed = started.elapsed();
        if bin.report_slow
            && running_mutant
            && !waiting_reported
            && elapsed >= Duration::from_secs(5).min(timeout / 2)
        {
            let selected = tests
                .filter(|names| !names.is_empty())
                .map(|names| {
                    names
                        .iter()
                        .map(|name| crate::report::sanitize_inline(name))
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .unwrap_or_else(|| crate::report::sanitize_inline(&bin.name));
            eprintln!(
                "tmt: waiting for mutant tests `{selected}` (limit {:.2}s)",
                timeout.as_secs_f64()
            );
            waiting_reported = true;
        }
        // tmt:ignore at exactly the limit either is right, and no test can stop the clock there
        if elapsed >= timeout {
            if cfg!(unix)
                && !bin.wrapper_grace.is_zero()
                && matches!(
                    &bin.runner,
                    Runner::Isolated {
                        wrapper: Some(_),
                        ..
                    }
                )
            {
                signal_test_group(group, 15);
                let began = Instant::now();
                while began.elapsed() < bin.wrapper_grace && child.try_wait().ok() == Some(None) {
                    std::thread::sleep(
                        Duration::from_millis(10)
                            .min(bin.wrapper_grace.saturating_sub(began.elapsed())),
                    );
                }
            }
            kill(&mut child);
            return Ok(Outcome::Timeout);
        }
        // tmt:ignore without the pause the loop only looks more often: the verdict is the same
        let remaining = timeout - elapsed;
        let wait = if output_to_watch.is_none() && exit_waiter.is_event_driven() {
            if bin.report_slow && running_mutant && !waiting_reported {
                let slow_at = Duration::from_secs(5).min(timeout / 2);
                remaining.min(
                    slow_at
                        .saturating_sub(elapsed)
                        .max(Duration::from_millis(1)),
                )
            } else {
                remaining
            }
        } else {
            next_test_poll(&mut delay, remaining)
        };
        exit_waiter.wait(wait);
    })();
    // tmt:ignore only Windows needs the file closed before the mark is removed, and these tests are for Unix
    drop(output_to_watch);
    // Stop the runner before sweeping leftovers so a live child is never still holding
    // the watch lock when the sweep begins.
    if result.is_err() {
        kill(&mut child);
        let _ = release_mark(&unable_mark);
    }
    // The run is over, whichever way: what it started and did not stop is stopped now.
    leftovers::count(&watch.finish(leftovers::leave()));
    let outcome = result?;

    // Whatever the tests made of it, a run that left the mark was not the mutant's. The
    // mark alone says so: a status is the tests' to choose, and one of them may well end
    // with the runtime's when it finds a mutant out.
    if release_mark(&unable_mark)? {
        return Ok(Outcome::Unable);
    }
    Ok(outcome)
}

pub(super) struct FailureOutput {
    pub(super) file: fs::File,
    pub(super) line: Vec<u8>,
    pub(super) discarded: bool,
}

/// Event-driven child exit waiter (`kqueue` on macOS, `pidfd_open` + `poll` on Linux,
/// falling back to `std::thread::sleep` on other platforms or unsupported kernels).
struct ExitWaiter {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    fd: Option<std::os::fd::OwnedFd>,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    exited: bool,
}

impl ExitWaiter {
    #[cfg(target_os = "macos")]
    fn new(pid: u32) -> Self {
        use std::os::fd::{FromRawFd, OwnedFd};
        const EVFILT_PROC: i16 = -5;
        const EV_ADD: u16 = 0x0001;
        const EV_ONESHOT: u16 = 0x0010;
        const NOTE_EXIT: u32 = 0x8000_0000;
        const F_SETFD: std::ffi::c_int = 2;
        const FD_CLOEXEC: std::ffi::c_int = 1;
        const ESRCH: i32 = 3;

        #[repr(C)]
        struct Kevent {
            ident: usize,
            filter: i16,
            flags: u16,
            fflags: u32,
            data: isize,
            udata: *mut std::ffi::c_void,
        }
        #[repr(C)]
        struct Timespec {
            tv_sec: isize,
            tv_nsec: isize,
        }
        unsafe extern "C" {
            fn kqueue() -> std::ffi::c_int;
            fn fcntl(fd: std::ffi::c_int, cmd: std::ffi::c_int, ...) -> std::ffi::c_int;
            fn kevent(
                kq: std::ffi::c_int,
                changelist: *const Kevent,
                nchanges: std::ffi::c_int,
                eventlist: *mut Kevent,
                nevents: std::ffi::c_int,
                timeout: *const Timespec,
            ) -> std::ffi::c_int;
        }
        // SAFETY: `kqueue()` takes no arguments and returns a new fd or -1.
        let raw = unsafe { kqueue() };
        if raw < 0 {
            return Self {
                fd: None,
                exited: false,
            };
        }
        // SAFETY: `raw` is a valid newly opened kqueue descriptor owned by `fd`.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // Prevent concurrent child spawns from inheriting the kqueue descriptor.
        unsafe { fcntl(raw, F_SETFD, FD_CLOEXEC) };
        let change = Kevent {
            ident: pid as usize,
            filter: EVFILT_PROC,
            flags: EV_ADD | EV_ONESHOT,
            fflags: NOTE_EXIT,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        let zero = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: `change` and `zero` are valid pointers for a 1-element registration.
        let rc = unsafe { kevent(raw, &change, 1, std::ptr::null_mut(), 0, &zero) };
        if rc == 0 {
            Self {
                fd: Some(fd),
                exited: false,
            }
        } else if std::io::Error::last_os_error().raw_os_error() == Some(ESRCH) {
            Self {
                fd: None,
                exited: true,
            }
        } else {
            Self {
                fd: None,
                exited: false,
            }
        }
    }

    #[cfg(all(
        target_os = "linux",
        any(
            all(target_arch = "x86_64", target_pointer_width = "64"),
            target_arch = "aarch64",
            target_arch = "x86",
            target_arch = "riscv64"
        )
    ))]
    fn new(pid: u32) -> Self {
        use std::os::fd::{FromRawFd, OwnedFd};
        const SYS_PIDFD_OPEN: std::ffi::c_long = 434;
        const ESRCH: i32 = 3;
        unsafe extern "C" {
            fn syscall(num: std::ffi::c_long, ...) -> std::ffi::c_long;
        }
        let Ok(raw_pid) = std::ffi::c_int::try_from(pid) else {
            return Self {
                fd: None,
                exited: false,
            };
        };
        // SAFETY: `pidfd_open(pid, 0)` returns a new O_CLOEXEC fd or -1.
        let raw = unsafe { syscall(SYS_PIDFD_OPEN, raw_pid, 0 as std::ffi::c_uint) };
        if raw >= 0 {
            // SAFETY: `raw` is a newly created valid pidfd owned by this struct.
            let fd = unsafe { OwnedFd::from_raw_fd(raw as std::os::fd::RawFd) };
            Self {
                fd: Some(fd),
                exited: false,
            }
        } else if std::io::Error::last_os_error().raw_os_error() == Some(ESRCH) {
            Self {
                fd: None,
                exited: true,
            }
        } else {
            Self {
                fd: None,
                exited: false,
            }
        }
    }

    #[cfg(not(any(
        target_os = "macos",
        all(
            target_os = "linux",
            any(
                all(target_arch = "x86_64", target_pointer_width = "64"),
                target_arch = "aarch64",
                target_arch = "x86",
                target_arch = "riscv64"
            )
        )
    )))]
    fn new(_pid: u32) -> Self {
        #[cfg(target_os = "linux")]
        {
            Self {
                fd: None,
                exited: false,
            }
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self {}
        }
    }

    fn is_event_driven(&self) -> bool {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            self.fd.is_some() || self.exited
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            false
        }
    }

    #[cfg(target_os = "macos")]
    fn wait(&mut self, duration: Duration) {
        use std::os::fd::AsRawFd;
        if self.exited {
            // `NOTE_EXIT` can fire slightly before `waitpid(WNOHANG)` sees the zombie;
            // yield briefly rather than busy-spinning.
            std::thread::sleep(duration.min(TEST_POLL_INITIAL));
            return;
        }
        let Some(fd) = &self.fd else {
            std::thread::sleep(duration);
            return;
        };
        #[repr(C)]
        struct Kevent {
            ident: usize,
            filter: i16,
            flags: u16,
            fflags: u32,
            data: isize,
            udata: *mut std::ffi::c_void,
        }
        #[repr(C)]
        struct Timespec {
            tv_sec: isize,
            tv_nsec: isize,
        }
        unsafe extern "C" {
            fn kevent(
                kq: std::ffi::c_int,
                changelist: *const Kevent,
                nchanges: std::ffi::c_int,
                eventlist: *mut Kevent,
                nevents: std::ffi::c_int,
                timeout: *const Timespec,
            ) -> std::ffi::c_int;
        }
        let mut ev = Kevent {
            ident: 0,
            filter: 0,
            flags: 0,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        let ts = Timespec {
            tv_sec: duration.as_secs().min(isize::MAX as u64) as isize,
            tv_nsec: duration.subsec_nanos() as isize,
        };
        // SAFETY: `ev` and `ts` are valid stack structs for a 1-event wait.
        let rc = unsafe { kevent(fd.as_raw_fd(), std::ptr::null(), 0, &mut ev, 1, &ts) };
        if rc > 0 {
            self.exited = true;
        }
    }

    #[cfg(target_os = "linux")]
    fn wait(&mut self, duration: Duration) {
        use std::os::fd::AsRawFd;
        if self.exited {
            return;
        }
        let Some(fd) = &self.fd else {
            std::thread::sleep(duration);
            return;
        };
        const POLLIN: std::ffi::c_short = 0x0001;
        #[repr(C)]
        struct PollFd {
            fd: std::ffi::c_int,
            events: std::ffi::c_short,
            revents: std::ffi::c_short,
        }
        unsafe extern "C" {
            fn poll(
                fds: *mut PollFd,
                nfds: std::ffi::c_ulong,
                timeout: std::ffi::c_int,
            ) -> std::ffi::c_int;
        }
        let mut pfd = PollFd {
            fd: fd.as_raw_fd(),
            events: POLLIN,
            revents: 0,
        };
        let ms = duration.as_millis().clamp(1, std::ffi::c_int::MAX as u128) as std::ffi::c_int;
        // SAFETY: `pfd` is a valid 1-element `pollfd` array.
        let rc = unsafe { poll(&mut pfd, 1, ms) };
        if rc > 0 {
            self.exited = true;
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    fn wait(&mut self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

impl FailureOutput {
    pub(super) fn new(file: fs::File) -> Self {
        Self {
            file,
            line: Vec::new(),
            discarded: false,
        }
    }

    pub(super) fn failed(&mut self) -> std::io::Result<bool> {
        use std::io::Read as _;
        const MAX_LINE: usize = 8 * 1024;
        let mut chunk = [0; MAX_LINE];
        let read = self.file.read(&mut chunk)?;
        for part in chunk[..read].split_inclusive(|byte| *byte == b'\n') {
            self.discarded |= self.line.len() + part.len() > MAX_LINE;
            if !self.discarded {
                self.line.extend_from_slice(part);
            }
            if part.ends_with(b"\n") {
                if !self.discarded
                    && is_failure_line(String::from_utf8_lossy(&self.line).trim_end())
                {
                    return Ok(true);
                }
                self.discarded = false;
                self.line.clear();
            }
        }
        Ok(false)
    }
}

pub(super) fn stop_nextest(child: &mut std::process::Child, grace: Duration) {
    let wait = |child: &mut std::process::Child, limit: Duration| {
        let started = Instant::now();
        loop {
            match child.try_wait() {
                // tmt:ignore a runner taken for running is signalled again and collected at the end all the same: what this spares is signals to a group that is gone
                Ok(Some(_)) => return true,
                // tmt:ignore how long a runner that does not end is waited for can only be told by the clock
                Ok(None) if started.elapsed() < limit => {
                    // tmt:ignore without the pause the loop only looks more often
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => return false,
            }
        }
    };
    let group = child.id();
    // Snapshot the owned groups before checking whether the parent has already exited.
    // A wrapper can exit before its independently grouped children are reparented and stopped.
    #[cfg(unix)]
    let mut owned = owned_test_processes(group);
    if wait(child, Duration::ZERO) {
        #[cfg(unix)]
        for surviving in surviving_groups(&owned, &test_processes()) {
            kill_test_group(surviving);
        }
        return;
    }
    signal_test_group(group, 15);
    let mut reaped = wait(child, grace);
    if !reaped {
        // The second signal asks nextest to force its units to stop.
        signal_test_group(group, 15);
        reaped = wait(child, grace.min(Duration::from_secs(1)));
    }
    #[cfg(unix)]
    {
        // tmt:ignore the second look only adds a test started between the two, which no test can place there
        if !reaped {
            // tmt:ignore as above
            owned.extend(owned_test_processes(group));
        }
        for surviving in surviving_groups(&owned, &test_processes()) {
            kill_test_group(surviving);
        }
    }
    if !reaped {
        // tmt:ignore the group of a runner that `ps` listed was stopped just above: this is for when `ps` could not be run
        kill_test_group(group);
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(unix)]
#[derive(Debug, PartialEq)]
pub(super) struct TestProcess {
    pub(super) pid: u32,
    pub(super) parent: u32,
    pub(super) group: u32,
    pub(super) started: String,
}

#[cfg(unix)]
pub(super) fn test_processes() -> Vec<TestProcess> {
    let Ok(processes) = Command::new("ps")
        .args([
            "-e", "-o", "pid=", "-o", "ppid=", "-o", "pgid=", "-o", "lstart=",
        ])
        .output()
    else {
        return vec![];
    };
    String::from_utf8_lossy(&processes.stdout)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some(TestProcess {
                pid: fields.next()?.parse().ok()?,
                parent: fields.next()?.parse().ok()?,
                group: fields.next()?.parse().ok()?,
                started: fields.collect::<Vec<_>>().join(" "),
            })
        })
        .collect()
}

/// The groups of the processes in `owned` that are still among `current`. A process is
/// the same one only with the same start and still in its group: its number alone may
/// by now be another's, and it may have moved to a group that was never the runner's.
#[cfg(unix)]
pub(super) fn surviving_groups(owned: &[TestProcess], current: &[TestProcess]) -> BTreeSet<u32> {
    current
        .iter()
        .filter(|process| {
            owned.iter().any(|before| {
                process.pid == before.pid
                    && process.group == before.group
                    && process.started == before.started
            })
        })
        .map(|process| process.group)
        .collect()
}

#[cfg(unix)]
pub(super) fn owned_test_processes(root: u32) -> Vec<TestProcess> {
    owned_among(test_processes(), root)
}

/// Of the processes in `data`, those `root` started, directly or through others, that
/// are in a group one of them leads.
#[cfg(unix)]
pub(super) fn owned_among(data: Vec<TestProcess>, root: u32) -> Vec<TestProcess> {
    let mut owned = HashSet::from([root]);
    loop {
        let before = owned.len();
        for process in &data {
            if owned.contains(&process.parent) {
                owned.insert(process.pid);
            }
        }
        if owned.len() == before {
            break;
        }
    }
    // Only signal groups that are independently owned, rather than a group a
    // child happened to join. Retain member identities in case its leader exits.
    let groups: HashSet<_> = data
        .iter()
        .filter(|process| process.pid == process.group && owned.contains(&process.pid))
        .map(|process| process.group)
        .collect();
    data.into_iter()
        .filter(|process| owned.contains(&process.pid) && groups.contains(&process.group))
        .collect()
}

pub(super) fn kill_test_group(id: u32) {
    signal_test_group(id, 9);
}

pub(super) fn signal_test_group(id: u32, signal: i32) {
    // Never signal group 0 (this process's own group) or 1 (init).
    if id <= 1 {
        return;
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        unsafe extern "C" {
            fn killpg(group: std::ffi::c_int, signal: std::ffi::c_int) -> std::ffi::c_int;
        }
        if let Ok(group) = std::ffi::c_int::try_from(id)
            && group > 1
        {
            // SAFETY: the argument is the child PID (> 1) used to create its own test group.
            unsafe { killpg(group, signal) };
        }
    }
    #[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
    let _ = Command::new("kill")
        .args([format!("-{signal}"), "--".to_owned(), format!("-{id}")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    #[cfg(not(unix))]
    let _ = (id, signal);
}

/// Keeps diagnostics on disk so a descendant holding stdout open cannot hold up the driver.
pub(super) fn run_baseline(
    bin: &TestBinary,
    env: &[(&str, &str)],
    timeout: Duration,
    path: &Path,
    timing_only: bool,
) -> Result<bool> {
    use std::io::{Read, Seek, SeekFrom};

    let mut suffix = 0;
    let (path, mut log) = loop {
        let candidate = if suffix == 0 {
            path.to_path_buf()
        } else {
            path.with_extension(format!("{suffix}.log"))
        };
        match fs::OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(&candidate)
        {
            Ok(log) => break (candidate, log),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => suffix += 1,
            Err(err) => {
                return Err(err)
                    .with_context(|| format!("creating baseline log {}", candidate.display()));
            }
        }
    };
    let result =
        run_binary(bin, None, env, timeout, RunOutput::Capture(&log), 1).with_context(|| {
            format!(
                "running baseline for `{}`; output: {}",
                bin.name,
                path.display()
            )
        })?;
    if result == Outcome::Pass || (timing_only && result == Outcome::Timeout) {
        drop(log);
        fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        return Ok(result == Outcome::Pass);
    }
    let reason = match result {
        Outcome::Pass => unreachable!("pass handled above"),
        Outcome::Fail => "do not pass on the unmutated code",
        Outcome::Timeout => "timed out on the unmutated code",
        Outcome::Unable => "could not be evaluated on the unmutated code",
    };
    const LIMIT: u64 = 16 * 1024;
    log.seek(SeekFrom::Start(log.metadata()?.len().saturating_sub(LIMIT)))?;
    let mut tail = Vec::new();
    log.take(LIMIT).read_to_end(&mut tail)?;
    bail!(
        "the tests in `{}` {reason}; full output: {}\n{}",
        bin.name,
        path.display(),
        String::from_utf8_lossy(&tail).trim_end()
    );
}

pub(super) fn run_source_tests(binaries: &[&TestBinary]) -> Result<Outcome> {
    for bin in binaries {
        bin.warn_timeout_conflict(None);
        let outcome = run_binary(
            bin,
            None,
            &[],
            bin.mutant_limit(bin.baseline),
            RunOutput::FirstFailure,
            1,
        )?;
        if outcome != Outcome::Pass {
            return Ok(outcome);
        }
    }
    Ok(Outcome::Pass)
}

/// A line of libtest output that reports a failed test.
pub(super) fn is_failure_line(line: &str) -> bool {
    line.starts_with("test ") && line.ends_with(" ... FAILED")
}

/// Calls `work(i)` once for every `i < count`, on up to `workers` threads, and returns the
/// results in index order. The first error wins.
pub(super) fn run_pool<T: Send>(
    count: usize,
    workers: usize,
    work: impl Fn(usize) -> Result<T> + Sync,
) -> Result<Vec<T>> {
    if count == 0 {
        return Ok(vec![]);
    }
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    let done: Vec<Vec<(usize, Result<T>)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers.clamp(1, count))
            .map(|_| {
                scope.spawn(|| {
                    let mut mine = vec![];
                    while !stop.load(Ordering::Acquire) {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        if index >= count {
                            break;
                        }
                        let result = work(index);
                        if result.is_err() {
                            stop.store(true, Ordering::Release);
                        }
                        mine.push((index, result));
                    }
                    mine
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("worker panicked"))
            .collect()
    });
    let mut ordered: Vec<Option<Result<T>>> = (0..count).map(|_| None).collect();
    for (index, result) in done.into_iter().flatten() {
        ordered[index] = Some(result);
    }
    ordered.into_iter().flatten().collect()
}
