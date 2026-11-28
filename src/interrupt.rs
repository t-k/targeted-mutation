//! Cooperative SIGINT handling so active test workers are stopped and reaped
//! before `cargo tmt` exits.

use std::fmt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Bounded grace period given to owned child process groups after SIGTERM on
/// the first SIGINT before escalating to SIGKILL.
pub const INTERRUPT_GRACE: Duration = Duration::from_secs(2);

/// Error returned when execution stops due to SIGINT.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Interrupted;

impl fmt::Display for Interrupted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("interrupted by SIGINT")
    }
}

impl std::error::Error for Interrupted {}

static INTERRUPT_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Returns true if `err` (or any cause in its chain) is [`Interrupted`].
pub fn is_interrupted_err(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| cause.is::<Interrupted>())
}

/// Returns true once at least one SIGINT has been received.
pub fn is_interrupted() -> bool {
    INTERRUPT_COUNT.load(Ordering::Acquire) > 0
}

/// Returns true if a second (or subsequent) SIGINT requested immediate SIGKILL
/// escalation without waiting out the grace period.
pub fn force_kill_requested() -> bool {
    INTERRUPT_COUNT.load(Ordering::Acquire) >= 2
}

/// Returns `Err(Interrupted)` if a SIGINT has been received.
pub fn check() -> anyhow::Result<()> {
    if is_interrupted() {
        Err(Interrupted.into())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
mod unix {
    use std::ffi::c_int;
    use std::os::fd::RawFd;
    use std::sync::Once;
    use std::sync::atomic::{AtomicI32, Ordering};

    use super::INTERRUPT_COUNT;

    const SIGINT: c_int = 2;
    const F_SETFD: c_int = 2;
    const FD_CLOEXEC: c_int = 1;
    const F_GETFL: c_int = 3;
    const F_SETFL: c_int = 4;

    #[cfg(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly"
    ))]
    const O_NONBLOCK: c_int = 0x0004;

    #[cfg(all(
        target_os = "linux",
        any(
            target_arch = "x86_64",
            target_arch = "x86",
            target_arch = "aarch64",
            target_arch = "riscv64"
        )
    ))]
    const O_NONBLOCK: c_int = 0o4000;

    #[cfg(not(any(
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd",
        target_os = "dragonfly",
        all(
            target_os = "linux",
            any(
                target_arch = "x86_64",
                target_arch = "x86",
                target_arch = "aarch64",
                target_arch = "riscv64"
            )
        )
    )))]
    const O_NONBLOCK: c_int = 0;

    static INSTALL: Once = Once::new();
    static WAKE_READ_FD: AtomicI32 = AtomicI32::new(-1);
    static WAKE_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

    unsafe extern "C" {
        fn pipe(fds: *mut c_int) -> c_int;
        fn close(fd: c_int) -> c_int;
        fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
        fn write(fd: c_int, buf: *const std::ffi::c_void, count: usize) -> isize;
        fn signal(sig: c_int, handler: extern "C" fn(c_int)) -> usize;
        fn siginterrupt(sig: c_int, flag: c_int) -> c_int;
    }

    extern "C" fn handle_sigint(_sig: c_int) {
        INTERRUPT_COUNT.fetch_add(1, Ordering::SeqCst);
        let fd = WAKE_WRITE_FD.load(Ordering::Acquire);
        if fd >= 0 {
            let byte: u8 = 1;
            // SAFETY: `write(2)` on a non-blocking pipe fd with a 1-byte stack
            // buffer is async-signal-safe per POSIX.
            let _ = unsafe { write(fd, (&byte as *const u8).cast(), 1) };
        }
    }

    pub(super) fn install() {
        INSTALL.call_once(|| {
            if O_NONBLOCK != 0 {
                let mut fds = [-1 as c_int; 2];
                // SAFETY: `fds` is a valid 2-element array for `pipe(2)`.
                if unsafe { pipe(fds.as_mut_ptr()) } == 0 {
                    let mut ok = true;
                    for &fd in &fds {
                        // SAFETY: `fd` is a valid open pipe descriptor.
                        if unsafe { fcntl(fd, F_SETFD, FD_CLOEXEC) } < 0 {
                            ok = false;
                        }
                        let flags = unsafe { fcntl(fd, F_GETFL) };
                        if flags < 0 || unsafe { fcntl(fd, F_SETFL, flags | O_NONBLOCK) } < 0 {
                            ok = false;
                        }
                    }
                    if ok {
                        WAKE_READ_FD.store(fds[0], Ordering::Release);
                        WAKE_WRITE_FD.store(fds[1], Ordering::Release);
                    } else {
                        unsafe {
                            let _ = close(fds[0]);
                            let _ = close(fds[1]);
                        }
                    }
                }
            }
            // SAFETY: `handle_sigint` only performs async-signal-safe operations
            // (atomic RMW and non-blocking 1-byte `write(2)`).
            unsafe {
                let _ = signal(SIGINT, handle_sigint);
                let _ = siginterrupt(SIGINT, 1);
            }
        });
    }

    pub(super) fn wake_read_fd() -> Option<RawFd> {
        let fd = WAKE_READ_FD.load(Ordering::Acquire);
        (fd >= 0).then_some(fd)
    }
}

/// Installs the process-wide SIGINT handler. Safe to call multiple times.
pub fn install() {
    #[cfg(unix)]
    unix::install();
}

/// Returns the non-blocking self-pipe read descriptor woken on SIGINT, if installed.
#[cfg(unix)]
pub(crate) fn wake_read_fd() -> Option<std::os::fd::RawFd> {
    unix::wake_read_fd()
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::{Mutex, MutexGuard};

    use super::*;

    static LOCK: Mutex<()> = Mutex::new(());

    pub(crate) struct Guard {
        _guard: MutexGuard<'static, ()>,
    }

    impl Guard {
        pub(crate) fn set(count: usize) -> Self {
            let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
            INTERRUPT_COUNT.store(count, Ordering::SeqCst);
            Self { _guard: guard }
        }
    }

    impl Drop for Guard {
        fn drop(&mut self) {
            INTERRUPT_COUNT.store(0, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupt_flags_and_error_detection_follow_signal_count() {
        let _guard = test_support::Guard::set(0);
        assert!(!is_interrupted());
        assert!(!force_kill_requested());
        assert!(check().is_ok());

        INTERRUPT_COUNT.store(1, Ordering::SeqCst);
        assert!(is_interrupted());
        assert!(!force_kill_requested());
        let err = check().unwrap_err();
        assert!(is_interrupted_err(&err));
        let wrapped = err.context("running baseline");
        assert!(is_interrupted_err(&wrapped));
        assert!(!is_interrupted_err(&anyhow::anyhow!("other error")));

        INTERRUPT_COUNT.store(2, Ordering::SeqCst);
        assert!(is_interrupted());
        assert!(force_kill_requested());
    }
}
