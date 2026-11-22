use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use anyhow::{Context, Result, bail};

/// Holds the work directory's lock until the OS closes the file handle.
pub(crate) struct WorkLock {
    _file: Arc<File>,
}

impl WorkLock {
    /// Keep the lock across exec in a child that uses the working copy. A
    /// command that has been configured but not started keeps the file alive
    /// so it cannot accidentally inherit a reused descriptor.
    #[cfg(unix)]
    pub(crate) fn retain_for(&self, command: &mut Command) {
        use std::os::fd::AsRawFd;
        use std::os::unix::process::CommandExt;

        let file = Arc::clone(&self._file);
        // SAFETY: the child's inherited File is still open, and the hook only
        // borrows its descriptor and performs async-signal-safe fcntl calls.
        unsafe { command.pre_exec(move || inherit_lock(file.as_raw_fd())) };
    }

    #[cfg(not(unix))]
    pub(crate) fn retain_for(&self, _: &mut Command) {
        // The exclusive Windows handle is owned by the parent.
    }

    pub(crate) fn take(work: &Path) -> Result<Self> {
        let path = lock_directory(work)?.join("owner");
        match acquire(&path) {
            Ok(file) => Ok(Self {
                _file: Arc::new(file),
            }),
            Err(err) if contended(&err) => {
                bail!("another tmt run is using {}", work.display())
            }
            Err(err) => Err(err).with_context(|| format!("locking {}", path.display())),
        }
    }
}

/// A directory at the old sentinel's path makes the two protocols exclude
/// one another. Creating the directory is atomic. If stopped before opening
/// its owner file, the next run can still initialize it.
fn lock_directory(work: &Path) -> Result<PathBuf> {
    let path = work.join(".tmt-lock");
    match fs::create_dir(&path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let existing = fs::symlink_metadata(&path)
                .with_context(|| format!("inspecting {}", path.display()))?;
            if !is_link(&existing) && existing.is_file() {
                bail!(
                    "{} is a legacy TMT lock file. Choose a fresh --work-dir, or remove \
                     the legacy file after all older TMT runs using this directory have ended",
                    path.display()
                );
            }
            if is_link(&existing) || !existing.is_dir() {
                bail!("{} must be a real lock directory", path.display());
            }
        }
        Err(error) => return Err(error).with_context(|| format!("creating {}", path.display())),
    }
    Ok(path)
}

#[cfg(unix)]
fn inherit_lock(fd: std::os::fd::RawFd) -> io::Result<()> {
    use std::os::raw::c_int;

    unsafe extern "C" {
        fn fcntl(fd: c_int, command: c_int, ...) -> c_int;
    }
    const F_GETFD: c_int = 1;
    const F_SETFD: c_int = 2;
    const FD_CLOEXEC: c_int = 1;

    // SAFETY: the borrowed descriptor belongs to the command's open lock.
    let flags = unsafe { fcntl(fd, F_GETFD) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: F_SETFD accepts one c_int argument. This updates only the
    // descriptor in the child, leaving other spawned commands unaffected.
    if unsafe { fcntl(fd, F_SETFD, flags & !FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn acquire(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x00200000;
        // No other handle may open the file for read, write or delete.
        options
            .share_mode(0)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }

    let file = match options.create_new(true).open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => {
            require_regular(&fs::symlink_metadata(path)?)?;
            options.create_new(false).open(path)?
        }
        Err(err) => return Err(err),
    };
    let opened = file.metadata()?;
    require_regular(&opened)?;
    lock_open_file(&file)?;

    // Do not unlink this path on release: another contender may already have
    // opened this inode. Also check that it wasn't replaced during acquisition.
    let named = fs::symlink_metadata(path)?;
    require_regular(&named)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (opened.dev(), opened.ino()) != (named.dev(), named.ino()) {
            return Err(io::Error::other(
                "lock file was replaced during acquisition",
            ));
        }
    }
    Ok(file)
}

fn require_regular(metadata: &fs::Metadata) -> io::Result<()> {
    if metadata.is_file() && !is_link(metadata) {
        Ok(())
    } else {
        Err(io::Error::other("lock path must be a regular file"))
    }
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return true;
        }
    }
    metadata.file_type().is_symlink()
}

fn contended(error: &io::Error) -> bool {
    #[cfg(windows)]
    if error.raw_os_error() == Some(32) {
        // ERROR_SHARING_VIOLATION
        return true;
    }
    error.kind() == io::ErrorKind::WouldBlock
}

#[cfg(unix)]
fn lock_open_file(file: &File) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::raw::c_int;

    unsafe extern "C" {
        fn flock(fd: c_int, operation: c_int) -> c_int;
    }
    const LOCK_EX: c_int = 2;
    const LOCK_NB: c_int = 4;

    loop {
        // SAFETY: the File owns this descriptor throughout the call, and the
        // two flags request a nonblocking exclusive lock.
        if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

#[cfg(windows)]
fn lock_open_file(_: &File) -> io::Result<()> {
    // The exclusive open acquired the lock.
    Ok(())
}

#[cfg(not(any(unix, windows)))]
fn lock_open_file(_: &File) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "work directory locking is unavailable on this platform",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "tmt-work-lock-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&dir).unwrap();
            Self(dir)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct LockChild(Child);

    impl Drop for LockChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    fn seed_owner(work: &Path, contents: &[u8]) {
        let reserved = work.join(".tmt-lock");
        fs::create_dir(&reserved).unwrap();
        fs::write(reserved.join("owner"), contents).unwrap();
    }

    const TEST_DIR: &str = "TMT_WORK_LOCK_UNIT_TEST_DIR";

    #[test]
    fn child_owns_lock_until_stdin_or_process_ends() {
        if let Some(work) = std::env::var_os(TEST_DIR) {
            let work = Path::new(&work);
            let _lock = WorkLock::take(work).unwrap();
            fs::write(work.join("ready"), []).unwrap();
            let _ = std::io::stdin().read_line(&mut String::new()).unwrap();
        }
    }

    fn lock_child(work: &Path) -> LockChild {
        let module = module_path!().split_once("::").unwrap().1;
        let mut child = LockChild(
            Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    &format!("{module}::child_owns_lock_until_stdin_or_process_ends"),
                    "--nocapture",
                ])
                .env(TEST_DIR, work)
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let started = Instant::now();
        while !work.join("ready").is_file() {
            assert_eq!(
                child.0.try_wait().unwrap(),
                None,
                "holder exited before taking lock"
            );
            assert!(
                started.elapsed() < Duration::from_secs(5),
                "holder never took lock"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        child
    }

    /// Takes a lock that this process has just let go of. Tests run on threads of one
    /// process, and a child that another test starts holds a copy of every open file
    /// between its fork and its exec, the lock that was just released among them: for
    /// that moment the lock still looks taken. (A run takes its lock once and keeps it,
    /// so only tests see this.)
    fn take_released(work: &Path) -> WorkLock {
        let patience = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            match WorkLock::take(work) {
                Ok(lock) => return lock,
                Err(err) if std::time::Instant::now() < patience => {
                    assert!(
                        err.to_string().contains("another tmt run is using"),
                        "{err:#}"
                    );
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(err) => panic!("the released lock stayed taken: {err:#}"),
            }
        }
    }

    fn assert_contended(work: &Path) {
        let err = WorkLock::take(work).err().expect("second owner took lock");
        assert!(
            err.to_string().contains("another tmt run is using"),
            "{err:#}"
        );
        assert!(!err.to_string().contains("delete"), "{err:#}");
    }

    #[test]
    fn live_owner_is_exclusive_and_operations_reuse_the_same_file() {
        // All six valid rows of the three-way lock PICT table.
        for (existing_owner, child, killed) in [
            (false, false, false),
            (true, true, true),
            (true, true, false),
            (true, false, false),
            (false, true, true),
            (false, true, false),
        ] {
            let work = Scratch::new();
            let marker = work.0.join(".tmt-lock/owner");
            if existing_owner {
                seed_owner(&work.0, &[]);
            }
            if child {
                let mut holder = lock_child(&work.0);
                assert_contended(&work.0);
                if killed {
                    holder.0.kill().unwrap();
                } else {
                    holder.0.stdin.take().unwrap().write_all(b"done\n").unwrap();
                }
                assert_eq!(holder.0.wait().unwrap().success(), !killed);
            } else {
                let _holder = WorkLock::take(&work.0).unwrap();
                assert_contended(&work.0);
            }
            assert!(marker.is_file(), "release removed the lock file");
            let _next = take_released(&work.0);
            assert_contended(&work.0);
        }
    }

    #[cfg(unix)]
    #[test]
    fn registered_child_keeps_the_lock_until_it_exits() {
        // All eight rows of the child handoff PICT table.
        for (existing_owner, parent_first, killed) in [
            (false, true, true),
            (true, false, false),
            (true, true, true),
            (true, true, false),
            (true, false, true),
            (false, true, false),
            (false, false, true),
            (false, false, false),
        ] {
            let work = Scratch::new();
            if existing_owner {
                seed_owner(&work.0, &[]);
            }
            let lock = WorkLock::take(&work.0).unwrap();
            let mut cmd = Command::new("sh");
            cmd.args(["-c", "read -r line"]).stdin(Stdio::piped());
            lock.retain_for(&mut cmd);
            // A command that was not registered never inherits the lock.
            let unrelated = LockChild(
                Command::new("sh")
                    .args(["-c", "read -r line"])
                    .stdin(Stdio::piped())
                    .spawn()
                    .unwrap(),
            );
            let mut child = LockChild(cmd.spawn().unwrap());
            drop(cmd);
            let mut parent = Some(lock);
            if parent_first {
                parent.take();
            }
            assert_contended(&work.0);

            if killed {
                child.0.kill().unwrap();
            } else {
                child.0.stdin.take().unwrap().write_all(b"done\n").unwrap();
            }
            assert_eq!(child.0.wait().unwrap().success(), !killed);
            if !parent_first {
                assert_contended(&work.0);
            }
            drop(parent);
            let _next = take_released(&work.0);
            drop(unrelated);
        }
    }

    #[test]
    fn leaves_an_existing_regular_file_intact_and_rejects_a_directory() {
        let work = Scratch::new();
        let marker = work.0.join(".tmt-lock/owner");
        fs::create_dir_all(&marker).unwrap();
        assert!(WorkLock::take(&work.0).is_err());
        assert!(marker.is_dir());
        fs::remove_dir(&marker).unwrap();

        fs::write(&marker, "keep this content").unwrap();
        drop(WorkLock::take(&work.0).unwrap());
        assert_eq!(fs::read_to_string(&marker).unwrap(), "keep this content");
    }

    #[test]
    fn legacy_sentinel_requires_migration_and_cannot_race_current_lock() {
        let work = Scratch::new();
        let marker = work.0.join(".tmt-lock");
        let legacy_open = || {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&marker)
        };
        let legacy = legacy_open().unwrap();

        let check_migration = || {
            let error = WorkLock::take(&work.0)
                .err()
                .expect("new process acquired a legacy lock");
            assert!(error.to_string().contains("legacy TMT lock"), "{error:#}");
            assert!(error.to_string().contains("--work-dir"), "{error:#}");
        };
        check_migration();
        drop(legacy);
        check_migration();
        fs::remove_file(&marker).unwrap();

        let current = WorkLock::take(&work.0).unwrap();
        assert!(legacy_open().is_err());
        drop(current);
        assert!(legacy_open().is_err());
    }

    #[test]
    fn recovers_after_interruption_before_owner_file_creation() {
        let work = Scratch::new();
        let reserved = work.0.join(".tmt-lock");
        fs::create_dir(&reserved).unwrap();

        let current = WorkLock::take(&work.0).unwrap();
        assert!(reserved.join("owner").is_file());
        assert_contended(&work.0);
        drop(current);
        drop(take_released(&work.0));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_without_creating_or_changing_their_targets() {
        let work = Scratch::new();
        let marker = work.0.join(".tmt-lock");
        let target = work.0.join("target");
        std::os::unix::fs::symlink(&target, &marker).unwrap();
        assert!(WorkLock::take(&work.0).is_err());
        assert!(!target.exists());
        fs::create_dir(&target).unwrap();
        assert!(WorkLock::take(&work.0).is_err());
        assert!(!target.join("owner").exists());
        fs::remove_dir(&target).unwrap();
        fs::remove_file(&marker).unwrap();

        fs::create_dir(&marker).unwrap();
        std::os::unix::fs::symlink(&target, marker.join("owner")).unwrap();
        fs::write(&target, "outside the reserved path").unwrap();
        assert!(WorkLock::take(&work.0).is_err());
        assert_eq!(
            fs::read_to_string(&target).unwrap(),
            "outside the reserved path"
        );
    }
}
