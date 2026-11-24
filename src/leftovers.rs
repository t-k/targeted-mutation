//! Stops the processes that a run of the tests left running.
//!
//! A timeout stops the process group of the tests, and so does the end of a run. A
//! process that left the group (a new session, a group of its own) is not in it, and once
//! its parent is gone nothing says whose it was. A mutant makes this common: the test
//! that would have stopped its helper fails, or is stopped, before it gets there.
//!
//! So every run is given an open file that it passes on, as an open file is passed on,
//! to whatever it starts: through `fork` and `exec`, into other groups and sessions, and
//! past the end of the parent. The file is locked through that one open description, and
//! the lock lasts as long as any process has it. After the run, a lock that is still held
//! means that somebody is left, and whoever has the file open is stopped.
//!
//! What this does not see: a process that closes the files it was given (a daemon that
//! detaches properly, a child of Python's `subprocess`, which closes them by default).

use std::collections::{BTreeSet, VecDeque};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Processes stopped after their run was over, in this process so far.
pub(crate) static STOPPED: AtomicUsize = AtomicUsize::new(0);
/// Runs after which somebody was still left and could not be stopped or found.
pub(crate) static NOT_STOPPED: AtomicUsize = AtomicUsize::new(0);

/// One run is started at a time. Between `fork` and `exec` a child has a copy of every
/// file of this process, the file of another run included; it must be gone before that
/// other run is over and asks who has its file.
static STARTING: Mutex<()> = Mutex::new(());
/// Direct child PIDs of active `Watch` runs, so a subreaper/PID-1 process can tell its
/// still-running sibling children apart from reparented orphans.
static ACTIVE_CHILDREN: Mutex<BTreeSet<u32>> = Mutex::new(BTreeSet::new());
/// Stopped orphans under PID 1/subreaper whose reparenting took longer than `REAP_WAIT`,
/// carried over to subsequent sweeps so only verified stopped targets are ever reaped.
#[cfg(unix)]
static PENDING_REAP: Mutex<VecDeque<StoppedTarget>> = Mutex::new(VecDeque::new());

/// How long those who have the file are stopped again and again before giving up.
const PATIENCE: Duration = Duration::from_secs(5);
/// A copy held by a child of this process that has not reached its `exec` goes by itself.
const SETTLE: Duration = Duration::from_millis(50);

/// What a sweep did.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Swept {
    /// Processes that were sent the signal that cannot be refused.
    pub(crate) stopped: usize,
    /// The file is still held, or who holds it could not be found out.
    pub(crate) left: bool,
}

/// The file of one run.
pub(crate) struct Watch {
    path: PathBuf,
    started: bool,
    child_pid: Option<u32>,
}

impl Watch {
    /// `path` is an existing file that nothing else uses.
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            started: false,
            child_pid: None,
        }
    }

    fn clear_active_child(&mut self) {
        if let Some(pid) = self.child_pid.take() {
            ACTIVE_CHILDREN
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&pid);
        }
    }

    /// Starts the command with the file open and locked.
    pub(crate) fn spawn(&mut self, command: &mut Command) -> io::Result<Child> {
        let _one_at_a_time = STARTING.lock().unwrap_or_else(|e| e.into_inner());
        #[cfg(unix)]
        let file = {
            use std::os::fd::AsRawFd;
            use std::os::unix::process::CommandExt;

            // Without the file the run is as it was before: nothing is watched.
            let file = File::open(&self.path)
                .and_then(|file| crate::work_lock::lock_open_file(&file).map(|()| file))
                .ok();
            if let Some(file) = &file {
                let fd = file.as_raw_fd();
                // SAFETY: the hook only clears a flag of a descriptor, which is
                // async-signal-safe, and the file is open until the child exists.
                unsafe { command.pre_exec(move || crate::work_lock::inherit_lock(fd)) };
                self.started = true;
            }
            file
        };
        let child = command.spawn();
        if let Ok(ref spawned) = child {
            let pid = spawned.id();
            self.child_pid = Some(pid);
            ACTIVE_CHILDREN
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(pid);
        }
        // This process lets go: from here on only the run and what it started hold it.
        #[cfg(unix)]
        drop(file);
        // A command that did not start has passed the file on to nobody: the sweep finds
        // the lock free.
        child
    }

    /// Stops whoever still has the file, unless they are to be left, and removes it.
    pub(crate) fn finish(mut self, leave: bool) -> Swept {
        self.clear_active_child();
        // tmt:ignore a run that was not given the file holds no lock: the sweep finds it free at once
        let swept = if self.started && !leave {
            sweep(&self.path)
        } else {
            Swept::default()
        };
        let _ = fs::remove_file(&self.path);
        swept
    }
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.clear_active_child();
    }
}

/// Counts a sweep for the summary of the campaign.
pub(crate) fn count(swept: &Swept) {
    STOPPED.fetch_add(swept.stopped, Ordering::Relaxed);
    if swept.left {
        NOT_STOPPED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Whether leftovers are to be left alone: `TMT_LEAVE_PROCESSES` is set to something.
pub(crate) fn leave() -> bool {
    asked_to_leave(std::env::var_os("TMT_LEAVE_PROCESSES"))
}

fn asked_to_leave(setting: Option<std::ffi::OsString>) -> bool {
    setting.is_some_and(|value| !value.is_empty())
}

/// How long to wait after sweeping for stopped descendants to be reparented and reaped.
const REAP_WAIT: Duration = Duration::from_millis(200);

#[cfg(unix)]
struct StoppedTarget {
    pid: u32,
    start: Option<String>,
    #[cfg(target_os = "linux")]
    pidfd: Option<std::os::fd::OwnedFd>,
}

#[cfg(unix)]
fn sweep(path: &Path) -> Swept {
    let mut swept = Swept::default();
    let mut stopped = std::collections::BTreeSet::new();
    let mut reparented_stopped: Vec<StoppedTarget> = Vec::new();
    let deadline = Instant::now() + PATIENCE;
    let mut settled = false;
    let me = std::process::id();
    let subreaper = is_subreaper(me);
    if subreaper {
        let mut carried = PENDING_REAP.lock().unwrap_or_else(|e| e.into_inner());
        reap_stopped_queue(&mut carried);
    }
    loop {
        match held(path) {
            Ok(true) => {}
            Ok(false) => break,
            Err(_) => {
                swept.left = true;
                break;
            }
        }
        // tmt:ignore the pause only spares a listing while a copy is on its way out: who is stopped is the same
        if !settled {
            // tmt:ignore as above
            std::thread::sleep(SETTLE);
            settled = true;
            continue;
        }
        #[cfg(not(target_os = "linux"))]
        let before_ps = ps_snapshot();
        let Ok(holders) = holders(path) else {
            swept.left = true;
            break;
        };
        #[cfg(not(target_os = "linux"))]
        let after_ps = ps_snapshot();
        #[cfg(not(target_os = "linux"))]
        let candidates: Vec<u32> = holders
            .into_iter()
            .filter(|&pid| {
                let parent = after_ps.get(&pid).map(|info| info.ppid);
                let active_child = if subreaper && parent == Some(me) {
                    let _spawn = STARTING.lock().unwrap_or_else(|e| e.into_inner());
                    ACTIVE_CHILDREN
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .contains(&pid)
                } else {
                    false
                };
                is_left_candidate(pid, me, parent, active_child, subreaper)
            })
            .collect();
        #[cfg(not(target_os = "linux"))]
        let verified_pids = verified_snapshot_pids(&candidates, &before_ps, &after_ps);
        let mut stopped_in_pass = false;
        #[cfg(target_os = "linux")]
        for pid in holders {
            let parent = parent_of(pid);
            let active_child = if subreaper && parent == Some(me) {
                // Synchronize with `Watch::spawn` so a sibling child that just forked is
                // registered in `ACTIVE_CHILDREN` before deciding whether it is an orphan.
                let _spawn = STARTING.lock().unwrap_or_else(|e| e.into_inner());
                ACTIVE_CHILDREN
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains(&pid)
            } else {
                false
            };
            if !is_left_candidate(pid, me, parent, active_child, subreaper) {
                continue;
            }
            if let Some(target) = stop_verified(pid, path) {
                stopped_in_pass = true;
                stopped.insert(pid);
                if subreaper {
                    // Under PID 1 or a subreaper, a stopped descendant whose own parent
                    // is also being stopped may be reparented to `me` after this check.
                    reparented_stopped.push(target);
                }
            }
        }
        #[cfg(not(target_os = "linux"))]
        for pid in verified_pids {
            if stop(pid) {
                stopped_in_pass = true;
                stopped.insert(pid);
                if subreaper {
                    reparented_stopped.push(StoppedTarget {
                        pid,
                        start: after_ps.get(&pid).map(|info| info.start.clone()),
                    });
                }
            }
        }
        reap_stopped(&mut reparented_stopped);
        // tmt:ignore giving up needs a holder that cannot be stopped, which a test cannot make
        if Instant::now() >= deadline {
            if stopped_in_pass {
                // If we just killed holders right as the deadline elapsed (e.g. slow `lsof`),
                // allow the kernel a brief moment to close their FDs before reporting `left`.
                let settle_deadline = Instant::now() + Duration::from_millis(200);
                loop {
                    match held(path) {
                        Ok(false) => break,
                        Ok(true) if Instant::now() < settle_deadline => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        _ => {
                            swept.left = true;
                            break;
                        }
                    }
                }
            } else {
                swept.left = true;
            }
            break;
        }
        // tmt:ignore without the pause the loop only looks more often
        std::thread::sleep(Duration::from_millis(20));
    }
    // Reap stopped orphans under PID 1/subreaper with a short bounded wait so an
    // unrelated parent that closed the watched file cannot stall the sweep for 5s.
    let reap_deadline = (Instant::now() + REAP_WAIT).min(deadline);
    while !reparented_stopped.is_empty() && Instant::now() < reap_deadline {
        reap_stopped(&mut reparented_stopped);
        if !reparented_stopped.is_empty() {
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    if subreaper && !reparented_stopped.is_empty() {
        let mut carried = PENDING_REAP.lock().unwrap_or_else(|e| e.into_inner());
        reap_stopped_queue(&mut carried);
        const MAX_PENDING_REAP: usize = 256;
        for target in reparented_stopped {
            if carried.len() >= MAX_PENDING_REAP {
                carried.pop_front();
            }
            carried.push_back(target);
        }
    }
    swept.stopped = stopped.len();
    swept
}

/// Decides whether a non-pidfd candidate may be stopped: both start-time snapshots must
/// exist and match, and the process must still hold the watched file between them.
#[cfg(any(test, unix))]
fn can_stop_verified(before: Option<&str>, still_holds: bool, after: Option<&str>) -> bool {
    matches!((before, after), (Some(a), Some(b)) if still_holds && a == b)
}

#[cfg(unix)]
fn reap_stopped(targets: &mut Vec<StoppedTarget>) {
    targets.retain(|target| !reap_target(target));
}

#[cfg(unix)]
fn reap_stopped_queue(targets: &mut VecDeque<StoppedTarget>) {
    targets.retain(|target| !reap_target(target));
}

/// On Linux 5.3, `pidfd_open` exists while `waitid(P_PIDFD, ...)` was added in Linux 5.4
/// and returns `EINVAL` on 5.3; fall back to synchronized `start_token` + `waitpid`.
#[cfg(any(test, target_os = "linux"))]
fn should_fallback_from_waitid_pidfd(errno: Option<i32>) -> bool {
    errno == Some(22 /* EINVAL */)
}

/// Reaps a single stopped orphan without ever touching active sibling runner children.
/// On Linux with a pinned `pidfd`, uses `waitid(P_PIDFD, ...)` to prevent PID-reuse races.
/// Otherwise synchronizes with `STARTING` + `ACTIVE_CHILDREN` + `start_token` before `waitpid`.
#[cfg(unix)]
fn reap_target(target: &StoppedTarget) -> bool {
    #[cfg(target_os = "linux")]
    if let Some(pidfd) = &target.pidfd {
        use std::os::fd::AsRawFd as _;
        const P_PIDFD: std::ffi::c_uint = 3;
        const WNOHANG: std::ffi::c_int = 1;
        const WEXITED: std::ffi::c_int = 4;
        #[repr(C)]
        struct SigInfo {
            si_signo: std::ffi::c_int,
            si_errno: std::ffi::c_int,
            si_code: std::ffi::c_int,
            _pad: [u64; 15],
        }
        unsafe extern "C" {
            fn waitid(
                idtype: std::ffi::c_uint,
                id: std::ffi::c_uint,
                infop: *mut SigInfo,
                options: std::ffi::c_int,
            ) -> std::ffi::c_int;
        }
        let mut info = SigInfo {
            si_signo: 0,
            si_errno: 0,
            si_code: 0,
            _pad: [0; 15],
        };
        // SAFETY: `waitid(P_PIDFD, fd, &mut info, WEXITED | WNOHANG)` targets only the
        // process pinned by `pidfd` and never blocks.
        let rc = unsafe {
            waitid(
                P_PIDFD,
                pidfd.as_raw_fd() as std::ffi::c_uint,
                &mut info,
                WEXITED | WNOHANG,
            )
        };
        if rc == 0 {
            // `si_signo != 0` means the process was reaped; `0` means still alive.
            return info.si_signo != 0;
        }
        let errno = io::Error::last_os_error().raw_os_error();
        if errno == Some(4 /* EINTR */) {
            return false;
        }
        if !should_fallback_from_waitid_pidfd(errno) {
            // `ECHILD`: either reaped by another parent or not yet reparented to `me`.
            return parent_of(target.pid).is_none();
        }
        // Linux 5.3 (`EINVAL`): fall through to the synchronized `start_token` + `waitpid` path.
    }

    unsafe extern "C" {
        fn waitpid(
            pid: std::ffi::c_int,
            status: *mut std::ffi::c_int,
            options: std::ffi::c_int,
        ) -> std::ffi::c_int;
    }
    const WNOHANG: std::ffi::c_int = 1;
    let Ok(raw_pid) = std::ffi::c_int::try_from(target.pid) else {
        return true;
    };
    if raw_pid <= 1 {
        return true;
    }
    // Synchronize with `Watch::spawn` and verify process identity while holding `STARTING`
    // so a newly spawned runner reusing the same PID cannot be reaped before registration.
    let _spawn = STARTING.lock().unwrap_or_else(|e| e.into_inner());
    if ACTIVE_CHILDREN
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&target.pid)
    {
        return true;
    }
    let Some(expected) = target.start.as_deref() else {
        // Without a verified start token, fail closed rather than calling `waitpid` by raw PID.
        return true;
    };
    if start_token(target.pid).as_deref() != Some(expected) {
        // Either already gone/reaped or PID was recycled for a different process.
        return true;
    }
    let mut status: std::ffi::c_int = 0;
    // SAFETY: `waitpid` with a verified `pid > 1` and `WNOHANG` never blocks and never
    // reaps any active runner child.
    let rc = unsafe { waitpid(raw_pid, &mut status, WNOHANG) };
    if rc > 0 {
        return true;
    }
    if rc == 0 {
        return false;
    }
    let errno = io::Error::last_os_error().raw_os_error();
    if errno == Some(4 /* EINTR */) {
        return false;
    }
    parent_of(target.pid).is_none()
}

/// Only fall back from `pidfd_open` to PID-based signalling when the kernel does not
/// implement `pidfd_open` (`ENOSYS`). Other errors (`EMFILE`, `EPERM`, `ESRCH`) fail
/// closed rather than re-opening a PID-reuse race.
#[cfg(any(test, target_os = "linux"))]
fn should_fallback_from_pidfd(errno: Option<i32>) -> bool {
    errno == Some(38 /* ENOSYS */)
}

/// Stops `pid` only after re-verifying that the exact same process still holds `path`.
/// On Linux, pins the process with `pidfd_open` before verifying file ownership and
/// signals via `pidfd_send_signal` to eliminate PID-reuse races.
#[cfg(target_os = "linux")]
fn stop_verified(pid: u32, path: &Path) -> Option<StoppedTarget> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
    use std::os::unix::fs::MetadataExt as _;

    const SYS_PIDFD_OPEN: std::ffi::c_long = 434;
    const SYS_PIDFD_SEND_SIGNAL: std::ffi::c_long = 424;
    unsafe extern "C" {
        fn syscall(num: std::ffi::c_long, ...) -> std::ffi::c_long;
    }
    let wanted_meta = fs::metadata(path).ok()?;
    let wanted = (wanted_meta.dev(), wanted_meta.ino());
    let raw_pid = std::ffi::c_int::try_from(pid).ok()?;
    if raw_pid <= 1 {
        return None;
    }
    // SAFETY: `pidfd_open(pid, 0)` takes two integers and returns a new fd or -1.
    let raw_fd = unsafe { syscall(SYS_PIDFD_OPEN, raw_pid, 0 as std::ffi::c_uint) };
    if raw_fd >= 0 {
        // SAFETY: `raw_fd` is a newly created valid file descriptor owned by this scope.
        let pidfd = unsafe { OwnedFd::from_raw_fd(raw_fd as std::os::fd::RawFd) };
        if !holds_path_linux(pid, wanted) {
            return None;
        }
        // Capture `start_token` before signaling so Linux 5.3 (`waitid(P_PIDFD)` -> `EINVAL`)
        // can safely fall back to synchronized `waitpid` without PID-reuse races.
        let start = start_token(pid);
        // SAFETY: `pidfd_send_signal` targets the pinned process description; if the
        // process exited and the PID was recycled, the kernel returns `ESRCH`.
        let rc = unsafe {
            syscall(
                SYS_PIDFD_SEND_SIGNAL,
                pidfd.as_raw_fd(),
                9 as std::ffi::c_int,
                std::ptr::null::<std::ffi::c_void>(),
                0 as std::ffi::c_uint,
            )
        };
        return (rc == 0).then_some(StoppedTarget {
            pid,
            start,
            pidfd: Some(pidfd),
        });
    }
    // Fallback strictly for kernels older than 5.3 without `pidfd_open` (`ENOSYS`).
    if !should_fallback_from_pidfd(io::Error::last_os_error().raw_os_error()) {
        return None;
    }
    let before = start_token(pid);
    let still_holds = before.is_some() && holds_path_linux(pid, wanted);
    let after = if still_holds { start_token(pid) } else { None };
    if can_stop_verified(before.as_deref(), still_holds, after.as_deref()) && stop(pid) {
        Some(StoppedTarget {
            pid,
            start: before,
            pidfd: None,
        })
    } else {
        None
    }
}

/// Selects candidate PIDs that are verified to belong to the watched holders across a
/// `before_ps -> lsof -> after_ps` bracket:
/// 1. Any candidate present in both snapshots with an identical start timestamp, and
/// 2. Any newly spawned candidate (absent from `before_ps`, present in `after_ps`) whose
///    parent chain in `after_ps` descends from a verified holder from (1).
#[cfg(any(test, all(unix, not(target_os = "linux"))))]
fn verified_snapshot_pids(
    candidates: &[u32],
    before_ps: &std::collections::HashMap<u32, PsEntry>,
    after_ps: &std::collections::HashMap<u32, PsEntry>,
) -> Vec<u32> {
    let verified: BTreeSet<u32> = candidates
        .iter()
        .copied()
        .filter(|pid| {
            can_stop_verified(
                before_ps.get(pid).map(|info| info.start.as_str()),
                true,
                after_ps.get(pid).map(|info| info.start.as_str()),
            )
        })
        .collect();
    let mut selected: Vec<u32> = verified.iter().copied().collect();
    for &pid in candidates {
        if verified.contains(&pid) || before_ps.contains_key(&pid) {
            continue;
        }
        let mut current = pid;
        for _ in 0..32 {
            let Some(parent) = after_ps.get(&current).map(|info| info.ppid) else {
                break;
            };
            if parent == current || parent <= 1 {
                break;
            }
            if verified.contains(&parent) {
                selected.push(pid);
                break;
            }
            current = parent;
        }
    }
    selected
}

#[cfg(any(test, all(unix, not(target_os = "linux"))))]
#[derive(Debug, PartialEq, Eq)]
struct PsEntry {
    ppid: u32,
    start: String,
}

#[cfg(all(unix, not(target_os = "linux")))]
fn ps_snapshot() -> std::collections::HashMap<u32, PsEntry> {
    let Ok(listed) = Command::new("ps")
        .env("LC_ALL", "C")
        .args(["-eo", "pid=,ppid=,lstart="])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return std::collections::HashMap::new();
    };
    parse_ps_snapshot(&String::from_utf8_lossy(&listed.stdout))
}

#[cfg(any(test, all(unix, not(target_os = "linux"))))]
fn parse_ps_snapshot(listing: &str) -> std::collections::HashMap<u32, PsEntry> {
    let mut table = std::collections::HashMap::new();
    for line in listing.lines() {
        let trimmed = line.trim();
        let Some((pid_str, rest)) = trimmed.split_once(char::is_whitespace) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some((ppid_str, start_str)) = rest.split_once(char::is_whitespace) else {
            continue;
        };
        let start = start_str.trim();
        if let (Ok(pid), Ok(ppid)) = (pid_str.parse::<u32>(), ppid_str.parse::<u32>())
            && !start.is_empty()
        {
            table.insert(
                pid,
                PsEntry {
                    ppid,
                    start: start.to_owned(),
                },
            );
        }
    }
    table
}

#[cfg(target_os = "linux")]
fn holds_path_linux(pid: u32, wanted: (u64, u64)) -> bool {
    use std::os::unix::fs::MetadataExt as _;

    let Ok(files) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return false;
    };
    files.flatten().any(|file| {
        let entry = file.path();
        if let Ok(target) = fs::read_link(&entry)
            && !target.is_absolute()
        {
            return false;
        }
        fs::metadata(&entry).is_ok_and(|open| (open.dev(), open.ino()) == wanted)
    })
}

/// Whether a process that has the file is one the run left.
///
/// Not this process, and not a child of it: such a child is another command of this
/// process on its way from `fork` to `exec`, where it lets go of the file by itself. (The
/// run itself was a child too, and has been collected by now.) When this process is PID 1
/// or a subreaper, reparented orphans also report `parent == me`, so active sibling
/// children are excluded via `ACTIVE_CHILDREN` instead of skipping every `parent == me`.
/// A process whose parent cannot be told has just ended.
#[cfg(test)]
fn is_left(pid: u32, me: u32, parent: Option<u32>) -> bool {
    is_left_candidate(pid, me, parent, false, false)
}

fn is_left_candidate(
    pid: u32,
    me: u32,
    parent: Option<u32>,
    active_child: bool,
    subreaper: bool,
) -> bool {
    pid != me && parent.is_some_and(|parent| parent != me || (subreaper && !active_child))
}

#[cfg(target_os = "linux")]
fn is_subreaper(me: u32) -> bool {
    if me == 1 {
        return true;
    }
    const PR_GET_CHILD_SUBREAPER: std::ffi::c_int = 37;
    unsafe extern "C" {
        fn prctl(
            option: std::ffi::c_int,
            arg2: *mut std::ffi::c_int,
            arg3: std::ffi::c_ulong,
            arg4: std::ffi::c_ulong,
            arg5: std::ffi::c_ulong,
        ) -> std::ffi::c_int;
    }
    let mut flag: std::ffi::c_int = 0;
    // SAFETY: `PR_GET_CHILD_SUBREAPER` writes a single `c_int` to `flag`.
    unsafe { prctl(PR_GET_CHILD_SUBREAPER, &mut flag, 0, 0, 0) == 0 && flag != 0 }
}

#[cfg(all(unix, not(target_os = "linux")))]
fn is_subreaper(me: u32) -> bool {
    me == 1
}

#[cfg(target_os = "linux")]
fn start_token(pid: u32) -> Option<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    start_in_stat(&stat).map(str::to_owned)
}

#[cfg(any(test, target_os = "linux"))]
fn start_in_stat(stat: &str) -> Option<&str> {
    let after_name = &stat[stat.rfind(')')? + 1..];
    // Field 22 in `/proc/<pid>/stat` (starttime) is index 19 after `state` (field 3).
    after_name.split_whitespace().nth(19)
}

#[cfg(all(unix, not(target_os = "linux")))]
fn start_token(pid: u32) -> Option<String> {
    let listed = Command::new("ps")
        .env("LC_ALL", "C")
        .args(["-o", "lstart=", "-p", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&listed.stdout).trim().to_owned();
    (!text.is_empty()).then_some(text)
}

#[cfg(target_os = "linux")]
fn parent_of(pid: u32) -> Option<u32> {
    // "pid (name) state ppid ...", where the name may contain anything but ends at the
    // last parenthesis.
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    parent_in_stat(&stat)
}

#[cfg(any(test, target_os = "linux"))]
fn parent_in_stat(stat: &str) -> Option<u32> {
    let after_name = &stat[stat.rfind(')')? + 1..];
    after_name.split_whitespace().nth(1)?.parse().ok()
}

#[cfg(all(unix, not(target_os = "linux")))]
fn parent_of(pid: u32) -> Option<u32> {
    let listed = Command::new("ps")
        .args(["-o", "ppid=", "-p", &pid.to_string()])
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    String::from_utf8_lossy(&listed.stdout).trim().parse().ok()
}

#[cfg(not(unix))]
fn sweep(_: &Path) -> Swept {
    Swept::default()
}

/// Whether some process has the file open through the description that was locked.
#[cfg(unix)]
fn held(path: &Path) -> io::Result<bool> {
    let file = File::open(path)?;
    match crate::work_lock::lock_open_file(&file) {
        Ok(()) => Ok(false),
        // tmt:ignore a lock that fails for another reason on a file that was just opened cannot be provoked in a test
        Err(error) if crate::work_lock::contended(&error) => Ok(true),
        Err(error) => Err(error),
    }
}

/// The processes that have the file open.
#[cfg(target_os = "linux")]
fn holders(path: &Path) -> io::Result<Vec<u32>> {
    use std::os::unix::fs::MetadataExt;

    let wanted = fs::metadata(path)?;
    let wanted = (wanted.dev(), wanted.ino());
    let mut pids = vec![];
    for process in fs::read_dir("/proc")?.flatten() {
        let Some(pid) = process.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        // A process of somebody else, or one that has just ended, cannot be read: it is
        // not one of the run's.
        let Ok(files) = fs::read_dir(process.path().join("fd")) else {
            continue;
        };
        let has = files.flatten().any(|file| {
            let entry = file.path();
            // Skip pseudo-descriptors (`socket:[...]`, `pipe:[...]`, `anon_inode:[...]`)
            // without discarding renamed or hard-linked paths to the same inode.
            if let Ok(target) = fs::read_link(&entry)
                && !target.is_absolute()
            {
                return false;
            }
            fs::metadata(&entry).is_ok_and(|open| (open.dev(), open.ino()) == wanted)
        });
        if has {
            pids.push(pid);
        }
    }
    Ok(pids)
}

/// The processes that have the file open, as `lsof` lists them.
#[cfg(all(unix, not(target_os = "linux")))]
fn holders(path: &Path) -> io::Result<Vec<u32>> {
    let mut last = io::Error::from(io::ErrorKind::NotFound);
    for lsof in ["lsof", "/usr/sbin/lsof", "/usr/bin/lsof"] {
        match Command::new(lsof)
            .args(["-n", "-P", "-w", "-t", "--"])
            .arg(path)
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
        {
            // It ends with a failure when nobody has the file: its output is what counts.
            Ok(output) => return Ok(pids(&String::from_utf8_lossy(&output.stdout))),
            Err(error) => last = error,
        }
    }
    Err(last)
}

/// The process numbers in the lines of a listing; anything else in it is no process.
#[cfg(any(test, all(unix, not(target_os = "linux"))))]
fn pids(listing: &str) -> Vec<u32> {
    listing
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .filter(|pid| *pid > 1)
        .collect()
}

#[cfg(unix)]
fn stop(pid: u32) -> bool {
    unsafe extern "C" {
        fn kill(pid: std::ffi::c_int, signal: std::ffi::c_int) -> std::ffi::c_int;
    }
    // Never 0, -1 or a negative number, which name groups: one process at a time.
    // tmt:ignore a test cannot ask to stop process 1 or 0 (every process of the group): the guard is what keeps that from ever happening
    if let Ok(pid) = std::ffi::c_int::try_from(pid)
        && pid > 1
    {
        // SAFETY: a plain system call with two integers.
        return unsafe { kill(pid, 9) == 0 };
    }
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::Read;
    use std::process::Stdio;

    static NEXT: AtomicUsize = AtomicUsize::new(0);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "tmt-leftovers-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn file(&self) -> PathBuf {
            let path = self.0.join("held");
            fs::write(&path, "").unwrap();
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// A shell that leaves a process in a group of its own, which ignores the polite
    /// signal and ends by itself after a minute, prints its number and ends.
    fn leaving() -> Command {
        let mut command = Command::new("sh");
        command
            .args([
                "-c",
                "set -m; (trap '' TERM HUP; i=0; while [ $i -lt 60 ]; do sleep 1; i=$((i+1)); done) \
                 >/dev/null 2>&1 & echo $!",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        command
    }

    fn left_by(mut child: Child) -> u32 {
        let mut said = String::new();
        child
            .stdout
            .take()
            .unwrap()
            .read_to_string(&mut said)
            .unwrap();
        assert!(child.wait().unwrap().success());
        said.trim().parse().unwrap()
    }

    fn alive(pid: u32) -> bool {
        // A process that was stopped and not yet collected is listed as a zombie.
        let listed = Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        let state = String::from_utf8_lossy(&listed.stdout);
        !state.trim().is_empty() && !state.trim().starts_with('Z')
    }

    fn gone(pid: u32) -> bool {
        let deadline = Instant::now() + Duration::from_secs(10);
        while alive(pid) {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        true
    }

    struct Reaper(u32);

    impl Drop for Reaper {
        fn drop(&mut self) {
            stop(self.0);
        }
    }

    #[test]
    fn what_a_run_left_in_another_group_is_stopped_after_it() {
        let scratch = Scratch::new();
        let path = scratch.file();
        let mut watch = Watch::new(path.clone());
        let left = Reaper(left_by(watch.spawn(&mut leaving()).unwrap()));
        assert!(alive(left.0));
        assert!(held(&path).unwrap());

        let swept = watch.finish(false);
        assert!(!swept.left, "{swept:?}");
        // The loop of the shell, and perhaps the `sleep` it was waiting for.
        assert!((1..=2).contains(&swept.stopped), "{swept:?}");
        assert!(gone(left.0));
        assert!(!path.exists());
    }

    #[test]
    fn a_run_that_left_nothing_is_not_swept() {
        let scratch = Scratch::new();
        let path = scratch.file();
        let mut watch = Watch::new(path.clone());
        let mut child = watch
            .spawn(Command::new("true").stdin(Stdio::null()))
            .unwrap();
        assert!(child.wait().unwrap().success());
        assert_eq!(watch.finish(false), Swept::default());
        assert!(!path.exists());
    }

    #[test]
    fn only_those_with_the_file_of_the_run_are_stopped() {
        let scratch = Scratch::new();
        let path = scratch.file();
        // Started the same way, without the file: somebody else's.
        let other = Reaper(left_by(leaving().spawn().unwrap()));
        let mut watch = Watch::new(path);
        let left = Reaper(left_by(watch.spawn(&mut leaving()).unwrap()));

        let swept = watch.finish(false);
        assert!(!swept.left, "{swept:?}");
        assert!(gone(left.0));
        assert!(alive(other.0));
    }

    #[test]
    fn leftovers_are_left_when_asked_and_a_run_that_did_not_start_has_none() {
        let scratch = Scratch::new();
        let path = scratch.file();
        let mut watch = Watch::new(path.clone());
        let left = Reaper(left_by(watch.spawn(&mut leaving()).unwrap()));
        assert_eq!(watch.finish(true), Swept::default());
        assert!(alive(left.0));
        assert!(!path.exists());

        let path = scratch.file();
        let mut watch = Watch::new(path.clone());
        assert!(
            watch
                .spawn(&mut Command::new(scratch.0.join("no such program")))
                .is_err()
        );
        assert_eq!(watch.finish(false), Swept::default());
        assert!(!path.exists());
    }

    #[test]
    fn a_file_that_is_gone_is_reported_as_not_swept() {
        let scratch = Scratch::new();
        let path = scratch.file();
        let mut watch = Watch::new(path.clone());
        let left = Reaper(left_by(watch.spawn(&mut leaving()).unwrap()));
        fs::remove_file(&path).unwrap();
        let swept = watch.finish(false);
        assert_eq!(
            swept,
            Swept {
                stopped: 0,
                left: true
            }
        );
        assert!(alive(left.0));
    }

    #[test]
    fn a_child_of_this_process_and_this_process_are_never_left_by_a_run() {
        assert!(is_left(40, 7, Some(1)));
        assert!(is_left(40, 7, Some(39)));
        // On its way from `fork` to `exec`.
        assert!(!is_left(40, 7, Some(7)));
        assert!(!is_left(7, 7, Some(1)));
        // Ended between the listing and the look.
        assert!(!is_left(40, 7, None));
        assert_eq!(
            parent_of(std::process::id()),
            Some(std::os::unix::process::parent_id())
        );
        assert_eq!(parent_of(u32::MAX - 7), None);
    }

    #[test]
    fn the_parent_is_read_after_a_name_that_may_contain_anything() {
        assert_eq!(parent_in_stat("12 (sh) S 7 12 12 0"), Some(7));
        assert_eq!(parent_in_stat("12 (a) b) 3 (x) R 1 12"), Some(1));
        assert_eq!(parent_in_stat("12 (sh"), None);
        assert_eq!(parent_in_stat("12 (sh) S"), None);
    }

    #[test]
    fn leftovers_are_left_only_when_the_setting_says_something() {
        assert!(!asked_to_leave(None));
        assert!(!asked_to_leave(Some("".into())));
        assert!(asked_to_leave(Some("1".into())));
        assert!(asked_to_leave(Some("no".into())));
    }

    #[test]
    fn a_sweep_is_counted_for_the_summary() {
        // Other tests of this program sweep too: the counts only grow.
        let (stopped, not_stopped) = (
            STOPPED.load(Ordering::Relaxed),
            NOT_STOPPED.load(Ordering::Relaxed),
        );
        count(&Swept {
            stopped: 3,
            left: true,
        });
        assert!(STOPPED.load(Ordering::Relaxed) >= stopped + 3);
        assert!(NOT_STOPPED.load(Ordering::Relaxed) > not_stopped);
        let not_stopped = NOT_STOPPED.load(Ordering::Relaxed);
        count(&Swept::default());
        assert_eq!(NOT_STOPPED.load(Ordering::Relaxed), not_stopped);
    }

    #[test]
    fn a_listing_gives_process_numbers_and_nothing_that_names_a_group() {
        assert_eq!(
            pids("120\n 77 \n\nlsof: warning\n0\n1\n2\n-5\n4000000000\n"),
            [120, 77, 2, 4000000000]
        );
        assert!(pids("").is_empty());
    }

    #[test]
    fn subreaper_stops_reparented_orphans_but_not_active_children() {
        // Under PID 1 or a subreaper, an orphan reparented to `me` (not an active child)
        // is swept, while an active sibling child is never stopped.
        assert!(is_left_candidate(40, 1, Some(1), false, true));
        assert!(!is_left_candidate(40, 1, Some(1), true, true));
        assert!(!is_left_candidate(1, 1, Some(0), false, true));
        assert!(!is_left_candidate(40, 7, Some(7), false, false));
        assert!(is_left_candidate(40, 7, Some(7), false, true));
    }

    #[test]
    fn start_token_parses_stat_and_matches_live_process() {
        let stat = "12 (a) b) S 7 12 12 0 -1 4194304 1 0 0 0 0 0 0 0 20 0 1 0 987654 0 0";
        assert_eq!(start_in_stat(stat), Some("987654"));
        assert_eq!(start_in_stat("12 (sh) S 7"), None);
        assert!(start_token(std::process::id()).is_some());
        assert_eq!(start_token(u32::MAX - 7), None);
    }

    #[test]
    fn verified_stop_rejects_recycled_or_missing_tokens_and_reaps_stopped_child() {
        assert!(can_stop_verified(Some("t1"), true, Some("t1")));
        assert!(!can_stop_verified(Some("t1"), true, Some("t2")));
        assert!(!can_stop_verified(Some("t1"), false, Some("t1")));
        assert!(!can_stop_verified(None, true, Some("t1")));
        assert!(!can_stop_verified(Some("t1"), true, None));
        assert!(!can_stop_verified(None, true, None));
        assert!(should_fallback_from_pidfd(Some(38)));
        assert!(!should_fallback_from_pidfd(Some(24)));
        assert!(!should_fallback_from_pidfd(Some(1)));
        assert!(!should_fallback_from_pidfd(None));
        assert!(should_fallback_from_waitid_pidfd(Some(22)));
        assert!(!should_fallback_from_waitid_pidfd(Some(10)));
        assert!(!should_fallback_from_waitid_pidfd(None));
        assert!(!stop(u32::MAX - 7));

        let child = Command::new("sleep")
            .arg("5")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let start = start_token(pid);
        assert!(start.is_some());
        // Missing start token fails closed without reaping.
        assert!(reap_target(&StoppedTarget {
            pid,
            start: None,
            #[cfg(target_os = "linux")]
            pidfd: None,
        }));
        assert!(alive(pid));
        assert!(stop(pid));
        let mut pending = vec![StoppedTarget {
            pid,
            start,
            #[cfg(target_os = "linux")]
            pidfd: None,
        }];
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pending.is_empty() && Instant::now() < deadline {
            reap_stopped(&mut pending);
            if !pending.is_empty() {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        assert!(pending.is_empty(), "expected stopped child to be reaped");
        std::mem::forget(child);
    }

    #[test]
    fn ps_snapshot_parses_pid_ppid_and_start_timestamp() {
        let table = parse_ps_snapshot(
            "  12   7 Tue Nov 24 18:07:25 2026\n\
             invalid line\n\
             42   1   \n\
             99 12 火 11/24 18:07:25 2026\n",
        );
        assert_eq!(
            table.get(&12),
            Some(&PsEntry {
                ppid: 7,
                start: "Tue Nov 24 18:07:25 2026".to_owned(),
            })
        );
        assert_eq!(
            table.get(&99),
            Some(&PsEntry {
                ppid: 12,
                start: "火 11/24 18:07:25 2026".to_owned(),
            })
        );
        assert!(!table.contains_key(&42));

        let before = parse_ps_snapshot(
            "100 1 Tue Nov 24 18:00:00 2026\n\
             200 1 Tue Nov 24 18:00:00 2026\n",
        );
        let after = parse_ps_snapshot(
            "100 1 Tue Nov 24 18:00:00 2026\n\
             101 100 Tue Nov 24 18:00:03 2026\n\
             200 1 Tue Nov 24 18:00:04 2026\n\
             300 1 Tue Nov 24 18:00:03 2026\n",
        );
        // 100 matches both snapshots; 101 was spawned by 100 during lsof;
        // 200 changed start time (recycled PID); 300 is an unrelated new process.
        assert_eq!(
            verified_snapshot_pids(&[100, 101, 200, 300], &before, &after),
            vec![100, 101]
        );
    }
}
