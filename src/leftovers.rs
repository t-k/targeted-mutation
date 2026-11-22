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
}

impl Watch {
    /// `path` is an existing file that nothing else uses.
    pub(crate) fn new(path: PathBuf) -> Self {
        Self {
            path,
            started: false,
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
        // This process lets go: from here on only the run and what it started hold it.
        #[cfg(unix)]
        drop(file);
        // A command that did not start has passed the file on to nobody: the sweep finds
        // the lock free.
        child
    }

    /// Stops whoever still has the file, unless they are to be left, and removes it.
    pub(crate) fn finish(self, leave: bool) -> Swept {
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

#[cfg(unix)]
fn sweep(path: &Path) -> Swept {
    let mut swept = Swept::default();
    let mut stopped = std::collections::BTreeSet::new();
    let deadline = Instant::now() + PATIENCE;
    let mut settled = false;
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
        let Ok(holders) = holders(path) else {
            swept.left = true;
            break;
        };
        for pid in holders {
            if is_left(pid, std::process::id(), parent_of(pid)) {
                stop(pid);
                stopped.insert(pid);
            }
        }
        // tmt:ignore giving up needs a holder that cannot be stopped, which a test cannot make
        if Instant::now() >= deadline {
            swept.left = true;
            break;
        }
        // tmt:ignore without the pause the loop only looks more often
        std::thread::sleep(Duration::from_millis(20));
    }
    swept.stopped = stopped.len();
    swept
}

/// Whether a process that has the file is one the run left.
///
/// Not this process, and not a child of it: such a child is another command of this
/// process on its way from `fork` to `exec`, where it lets go of the file by itself. (The
/// run itself was a child too, and has been collected by now.) A process whose parent
/// cannot be told has just ended.
fn is_left(pid: u32, me: u32, parent: Option<u32>) -> bool {
    pid != me && parent.is_some_and(|parent| parent != me)
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
            fs::metadata(file.path()).is_ok_and(|open| (open.dev(), open.ino()) == wanted)
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
            .arg("-t")
            .arg("--")
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
fn stop(pid: u32) {
    unsafe extern "C" {
        fn kill(pid: std::ffi::c_int, signal: std::ffi::c_int) -> std::ffi::c_int;
    }
    // Never 0, -1 or a negative number, which name groups: one process at a time.
    // tmt:ignore a test cannot ask to stop process 1 or 0 (every process of the group): the guard is what keeps that from ever happening
    if let Ok(pid) = std::ffi::c_int::try_from(pid)
        && pid > 1
    {
        // SAFETY: a plain system call with two integers.
        unsafe { kill(pid, 9) };
    }
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
}
