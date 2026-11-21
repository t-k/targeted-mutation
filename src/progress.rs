use std::sync::{Mutex, mpsc};
use std::thread;
use std::time::Duration;

const UPDATE_INTERVAL: Duration = Duration::from_secs(5);

pub(crate) enum Outcome {
    Failed,
    TimedOut,
    Survived,
    Unable,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Counts {
    failed: usize,
    timed_out: usize,
    survived: usize,
    unable: usize,
}

impl Counts {
    fn completed(self) -> usize {
        self.failed + self.timed_out + self.survived + self.unable
    }
}

pub(crate) struct Progress(Option<Mutex<Counts>>);

impl Progress {
    pub(crate) fn complete(&self, outcome: Outcome) {
        if let Some(counts) = &self.0 {
            let mut counts = counts.lock().unwrap();
            let counter = match outcome {
                Outcome::Failed => &mut counts.failed,
                Outcome::TimedOut => &mut counts.timed_out,
                Outcome::Survived => &mut counts.survived,
                Outcome::Unable => &mut counts.unable,
            };
            *counter += 1;
        }
    }
}

pub(crate) fn track<T>(
    enabled: bool,
    label: &str,
    total: usize,
    run: impl FnOnce(&Progress) -> T,
) -> T {
    track_with_reporter(enabled, total, UPDATE_INTERVAL, run, |counts| {
        eprintln!(
            "tmt: {label}: {}/{total} completed ({} failed, {} timed out, {} survived, {} unevaluated)",
            counts.completed(),
            counts.failed,
            counts.timed_out,
            counts.survived,
            counts.unable,
        );
    })
}

pub(crate) fn track_baselines<T>(
    enabled: bool,
    total: usize,
    run: impl FnOnce(&Progress) -> T,
) -> T {
    track_with_reporter(enabled, total, UPDATE_INTERVAL, run, |counts| {
        eprintln!(
            "tmt: baselines: {}/{total} completed ({} failed)",
            counts.completed(),
            counts.failed
        );
    })
}

pub(crate) fn track_timeout_rechecks<T>(
    enabled: bool,
    total: usize,
    run: impl FnOnce(&Progress) -> T,
) -> T {
    track_with_reporter(enabled, total, UPDATE_INTERVAL, run, |counts| {
        eprintln!(
            "tmt: serial timeout rechecks: {}/{} completed ({} pending, {} confirmed, {} recovered, {} unevaluated)",
            counts.completed(),
            total,
            total - counts.completed(),
            counts.timed_out,
            counts.survived,
            counts.unable
        );
    })
}

fn track_with_reporter<T>(
    enabled: bool,
    total: usize,
    interval: Duration,
    run: impl FnOnce(&Progress) -> T,
    mut emit: impl FnMut(Counts) + Send,
) -> T {
    if !enabled || total == 0 {
        return run(&Progress(None));
    }

    let progress = Progress(Some(Mutex::default()));
    let counts = progress.0.as_ref().unwrap();
    thread::scope(|scope| {
        let (_active, finished) = mpsc::channel::<()>();
        scope.spawn(move || {
            let mut report = || {
                let snapshot = *counts.lock().unwrap();
                emit(snapshot);
            };
            while finished.recv_timeout(interval) == Err(mpsc::RecvTimeoutError::Timeout) {
                report();
            }
            report();
        });
        // The sender drops before the scope joins the reporter, including on unwind.
        run(&progress)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic;
    use std::time::Duration;

    #[test]
    fn heartbeat_reports_no_completions_while_work_is_running() {
        let (emit, seen) = std::sync::mpsc::channel();
        let result = track_with_reporter(
            true,
            1,
            Duration::from_millis(5),
            |progress| {
                assert_eq!(
                    seen.recv_timeout(Duration::from_secs(2)).unwrap(),
                    Counts::default()
                );
                progress.complete(Outcome::Survived);
                42
            },
            |counts| emit.send(counts).unwrap(),
        );
        assert_eq!(result, 42);
        assert_eq!(seen.try_iter().last().unwrap().survived, 1);
    }

    #[test]
    fn final_counts_only_include_completed_outcomes() {
        let (emit, seen) = std::sync::mpsc::channel();
        let result = track_with_reporter(
            true,
            5,
            Duration::from_secs(60),
            |progress| {
                for outcome in [
                    Outcome::Failed,
                    Outcome::TimedOut,
                    Outcome::Survived,
                    Outcome::Unable,
                ] {
                    progress.complete(outcome);
                }
                Err::<(), _>("campaign error")
            },
            |counts| emit.send(counts).unwrap(),
        );
        assert_eq!(result, Err("campaign error"));
        let counts = seen.try_recv().unwrap();
        assert_eq!(
            (
                counts.failed,
                counts.timed_out,
                counts.survived,
                counts.unable
            ),
            (1, 1, 1, 1)
        );
        assert_eq!(counts.completed(), 4);
        assert!(seen.try_recv().is_err());
    }

    #[test]
    fn quiet_and_empty_campaigns_do_not_report() {
        for (enabled, total) in [(false, 1), (true, 0)] {
            track_with_reporter(
                enabled,
                total,
                Duration::ZERO,
                |progress| progress.complete(Outcome::Survived),
                |_| panic!("an inactive campaign must not report"),
            );
        }
        assert_eq!(track(false, "mutation test", 1, |_| 42), 42);
    }

    #[test]
    fn reporting_stops_when_the_campaign_panics() {
        let (emit, seen) = std::sync::mpsc::channel();
        let result = panic::catch_unwind(|| {
            track_with_reporter(
                true,
                1,
                Duration::from_secs(60),
                |progress| {
                    progress.complete(Outcome::Failed);
                    panic!("campaign panic");
                },
                |counts| emit.send(counts).unwrap(),
            );
        });
        assert!(result.is_err());
        assert_eq!(seen.try_iter().last().unwrap().failed, 1);
    }
}
