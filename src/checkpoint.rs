//! Durable outcomes for one campaign, under the caller's work-directory lock.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

const VERSION: u32 = 1;
const HEADER: &[u8] = b"{\"event\":\"format\",\"version\":1}\n";

#[derive(Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
enum Event {
    Format {
        version: u32,
    },
    CampaignStarted {
        context: String,
        metadata: Value,
    },
    PhaseStarted {
        phase: String,
        context: String,
        candidates: Vec<String>,
    },
    PhaseResumed {
        phase: String,
        reused: usize,
        pending: usize,
    },
    Outcome {
        phase: String,
        candidate: usize,
        key: String,
        outcome: Value,
    },
    PhaseCompleted {
        phase: String,
    },
    CampaignCompleted,
}

#[derive(Default)]
struct State {
    campaign: Option<String>,
    epoch: usize,
    active: BTreeSet<String>,
    complete: bool,
    phases: BTreeMap<String, PhaseState>,
}

struct PhaseState {
    campaign: String,
    context: String,
    keys: Vec<String>,
    outcomes: Vec<Option<Value>>,
    complete: bool,
}

impl PhaseState {
    fn pending(&self) -> usize {
        self.outcomes
            .iter()
            .filter(|outcome| outcome.is_none())
            .count()
    }

    fn key(&self, index: usize) -> Result<&str> {
        self.keys
            .get(index)
            .map(String::as_str)
            .with_context(|| format!("checkpoint has no candidate {index}"))
    }
}

impl State {
    fn running(&self) -> Result<&str> {
        ensure!(!self.complete, "checkpoint campaign has already completed");
        self.campaign
            .as_deref()
            .context("checkpoint campaign has not started")
    }

    fn phase(&self, name: &str) -> Result<&PhaseState> {
        ensure!(
            self.active.contains(name),
            "checkpoint phase {name:?} has not started"
        );
        Ok(&self.phases[name])
    }

    fn phase_mut(&mut self, name: &str) -> Result<&mut PhaseState> {
        self.running()?;
        ensure!(
            self.active.contains(name),
            "checkpoint phase {name:?} has not started"
        );
        let phase = self
            .phases
            .get_mut(name)
            .expect("active phase was recorded");
        ensure!(
            !phase.complete,
            "checkpoint phase {name:?} has already completed"
        );
        Ok(phase)
    }

    fn apply(&mut self, event: Event) -> Result<()> {
        match event {
            Event::Format { .. } => bail!("duplicate checkpoint format record"),
            Event::CampaignStarted { context, .. } => {
                self.campaign = Some(context);
                self.epoch += 1;
                self.active.clear();
                self.complete = false;
            }
            Event::PhaseStarted {
                phase,
                context,
                candidates,
            } => {
                let campaign = self.running()?.to_owned();
                ensure!(
                    !self.active.contains(&phase),
                    "duplicate checkpoint phase {phase:?}"
                );
                self.active.insert(phase.clone());
                self.phases.insert(
                    phase,
                    PhaseState {
                        campaign,
                        context,
                        outcomes: vec![None; candidates.len()],
                        keys: candidates,
                        complete: false,
                    },
                );
            }
            Event::PhaseResumed {
                phase,
                reused,
                pending,
            } => {
                let campaign = self.running()?.to_owned();
                ensure!(
                    !self.active.contains(&phase),
                    "duplicate checkpoint phase {phase:?}"
                );
                let previous = self
                    .phases
                    .get_mut(&phase)
                    .with_context(|| format!("checkpoint cannot resume unknown phase {phase:?}"))?;
                ensure!(
                    previous.campaign == campaign
                        && previous.pending() == pending
                        && previous.keys.len() - pending == reused,
                    "checkpoint resume does not match phase {phase:?}"
                );
                previous.complete = false;
                self.active.insert(phase);
            }
            Event::Outcome {
                phase,
                candidate,
                key,
                outcome,
            } => {
                let phase = self.phase_mut(&phase)?;
                ensure!(
                    phase.key(candidate)? == key,
                    "checkpoint candidate key does not match"
                );
                ensure!(
                    phase.outcomes[candidate].is_none(),
                    "duplicate checkpoint outcome"
                );
                phase.outcomes[candidate] = Some(outcome);
            }
            Event::PhaseCompleted { phase } => {
                let phase = self.phase_mut(&phase)?;
                ensure!(
                    phase.pending() == 0,
                    "checkpoint phase still has pending candidates"
                );
                phase.complete = true;
            }
            Event::CampaignCompleted => {
                self.running()?;
                ensure!(
                    self.active.iter().all(|name| self.phases[name].complete),
                    "checkpoint campaign still has unfinished phases"
                );
                self.complete = true;
            }
        }
        Ok(())
    }
}

struct Log {
    state: State,
    file: File,
    write_failed: bool,
}

impl Log {
    fn append(&mut self, event: Event) -> Result<()> {
        ensure!(!self.write_failed, "a previous checkpoint write failed");
        let mut line = serde_json::to_vec(&event)?;
        line.push(b'\n');
        self.state.apply(event)?;
        let written = self
            .file
            .write_all(&line)
            .and_then(|()| self.file.sync_data());
        self.write_failed = written.is_err();
        written.context("persisting checkpoint record")
    }
}

/// Serializes progress from all worker threads. The caller holds the work-directory
/// lock for this file's entire lifetime, including recovery on open.
pub(crate) struct Checkpoint(Mutex<Log>);

pub(crate) struct Phase<'a> {
    checkpoint: &'a Checkpoint,
    phase: String,
    epoch: usize,
}

impl Checkpoint {
    pub(crate) fn open(path: &Path) -> Result<Self> {
        let load = || -> Result<Self> {
            match fs::symlink_metadata(path) {
                Ok(metadata) => check_file(&metadata)?,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                Err(err) => return Err(err.into()),
            }
            let file = File::options()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)?;
            check_file(&file.metadata()?)?;
            check_file(&fs::symlink_metadata(path)?)?;
            let state = recover(&file)?;
            Ok(Self(Mutex::new(Log {
                state,
                file,
                write_failed: false,
            })))
        };
        load().with_context(|| format!("opening checkpoint {}", path.display()))
    }

    fn lock(&self) -> Result<MutexGuard<'_, Log>> {
        let log = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("checkpoint writer panicked"))?;
        ensure!(!log.write_failed, "a previous checkpoint write failed");
        Ok(log)
    }

    pub(crate) fn start(&self, context: &str, metadata: Value) -> Result<()> {
        self.lock()?.append(Event::CampaignStarted {
            context: context.to_owned(),
            metadata,
        })
    }

    pub(crate) fn begin(&self, phase: &str, context: &str, keys: Vec<String>) -> Result<Phase<'_>> {
        let mut log = self.lock()?;
        let campaign = log.state.running()?;
        let previous = log.state.phases.get(phase).filter(|previous| {
            previous.campaign == campaign && previous.context == context && previous.keys == keys
        });
        let event = if let Some(previous) = previous {
            let pending = previous.pending();
            Event::PhaseResumed {
                phase: phase.to_owned(),
                reused: keys.len() - pending,
                pending,
            }
        } else {
            Event::PhaseStarted {
                phase: phase.to_owned(),
                context: context.to_owned(),
                candidates: keys,
            }
        };
        log.append(event)?;
        Ok(Phase {
            checkpoint: self,
            phase: phase.to_owned(),
            epoch: log.state.epoch,
        })
    }

    pub(crate) fn finish(&self) -> Result<()> {
        self.lock()?.append(Event::CampaignCompleted)
    }
}

impl Phase<'_> {
    fn lock(&self) -> Result<MutexGuard<'_, Log>> {
        let log = self.checkpoint.lock()?;
        ensure!(
            log.state.epoch == self.epoch,
            "checkpoint phase belongs to a previous campaign"
        );
        Ok(log)
    }

    pub(crate) fn get<T: DeserializeOwned>(&self, index: usize) -> Result<Option<T>> {
        let log = self.lock()?;
        let phase = log.state.phase(&self.phase)?;
        phase.key(index)?;
        phase.outcomes[index]
            .as_ref()
            .map(|value| serde_json::from_value(value.clone()))
            .transpose()
            .with_context(|| format!("decoding checkpoint outcome for candidate {index}"))
    }

    pub(crate) fn record(&self, index: usize, outcome: &impl Serialize) -> Result<()> {
        let mut log = self.lock()?;
        let key = log.state.phase(&self.phase)?.key(index)?.to_owned();
        log.append(Event::Outcome {
            phase: self.phase.clone(),
            candidate: index,
            key,
            outcome: serde_json::to_value(outcome)?,
        })
    }

    pub(crate) fn finish(&self) -> Result<()> {
        self.lock()?.append(Event::PhaseCompleted {
            phase: self.phase.clone(),
        })
    }
}

fn check_file(metadata: &fs::Metadata) -> Result<()> {
    ensure!(
        metadata.is_file(),
        "checkpoint must be a regular file, not a symbolic link"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(metadata.nlink() == 1, "checkpoint must not have hard links");
    }
    Ok(())
}

fn recover(mut file: &File) -> Result<State> {
    let mut state = State::default();
    file.rewind()?;
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let mut offset = 0;
    let mut row = 1;
    loop {
        line.clear();
        let bytes = reader.read_until(b'\n', &mut line)?;
        if bytes == 0 {
            break;
        }
        if !line.ends_with(b"\n") {
            ensure!(
                offset > 0 || HEADER.starts_with(&line),
                "file is not a TMT checkpoint"
            );
            file.set_len(offset)?;
            file.sync_data()?;
            break;
        }
        let event = serde_json::from_slice(&line)
            .with_context(|| format!("reading checkpoint record {row}"))?;
        if offset == 0 {
            ensure!(
                matches!(event, Event::Format { version: VERSION }),
                "unsupported TMT checkpoint format"
            );
        } else {
            state
                .apply(event)
                .with_context(|| format!("replaying checkpoint record {row}"))?;
        }
        offset += bytes as u64;
        row += 1;
    }
    drop(reader);
    file.seek(SeekFrom::End(0))?;
    if offset == 0 {
        file.write_all(HEADER)?;
        file.sync_data()?;
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;
    use std::io::Write;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Scratch(PathBuf);

    impl Scratch {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("tmt-checkpoint-test-{}-{n}", std::process::id()));
            fs::create_dir(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> PathBuf {
            self.0.join("checkpoint.jsonl")
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn keys(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    fn events(path: &Path) -> Vec<Value> {
        fs::read_to_string(path)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    #[test]
    fn checkpoint_reuses_only_the_identical_ordered_campaign() {
        // The generated strength-three matrix includes the mandatory seed rows.
        const CASES: &[(&str, &str, &str)] = &[
            ("Empty", "Exact", "Serial"),
            ("Torn", "Exact", "Parallel"),
            ("Complete", "Inventory", "Parallel"),
            ("Complete", "Phase", "Parallel"),
            ("Partial", "Campaign", "Serial"),
            ("Partial", "Phase", "Serial"),
            ("Complete", "Exact", "Parallel"),
            ("Partial", "Exact", "Serial"),
            ("Torn", "Phase", "Parallel"),
            ("Partial", "Inventory", "Parallel"),
            ("Torn", "Exact", "Serial"),
            ("Empty", "Exact", "Parallel"),
            ("Torn", "Campaign", "Parallel"),
            ("Partial", "Inventory", "Serial"),
            ("Partial", "Campaign", "Parallel"),
            ("Complete", "Inventory", "Serial"),
            ("Partial", "Exact", "Parallel"),
            ("Complete", "Exact", "Serial"),
            ("Torn", "Inventory", "Serial"),
            ("Partial", "Phase", "Parallel"),
            ("Torn", "Campaign", "Serial"),
            ("Torn", "Inventory", "Parallel"),
            ("Torn", "Phase", "Serial"),
            ("Complete", "Campaign", "Serial"),
            ("Complete", "Campaign", "Parallel"),
            ("Complete", "Phase", "Serial"),
        ];
        for &(history, identity, workers) in CASES {
            let dir = Scratch::new();
            let path = dir.path();
            if history != "Empty" {
                let log = Checkpoint::open(&path).unwrap();
                log.start("settings", json!({"package":"demo"})).unwrap();
                let phase = log.begin("probe", "trace", keys(&["a", "b", "c"])).unwrap();
                phase.record(0, &1).unwrap();
                if history == "Complete" {
                    phase.record(1, &2).unwrap();
                    phase.record(2, &3).unwrap();
                    phase.finish().unwrap();
                    log.finish().unwrap();
                }
            }
            if history == "Torn" {
                fs::OpenOptions::new()
                    .append(true)
                    .open(&path)
                    .unwrap()
                    .write_all(br#"{"event":"outcom"#)
                    .unwrap();
            }
            let log = Checkpoint::open(&path).unwrap();
            let campaign = if identity == "Campaign" {
                "other"
            } else {
                "settings"
            };
            let context = if identity == "Phase" {
                "other"
            } else {
                "trace"
            };
            let candidates = if identity == "Inventory" {
                keys(&["b", "a", "c"])
            } else {
                keys(&["a", "b", "c"])
            };
            log.start(campaign, json!({"package":"demo"})).unwrap();
            let phase = log.begin("probe", context, candidates).unwrap();
            let reused = match (identity, history) {
                ("Exact", "Complete") => 3,
                ("Exact", "Partial" | "Torn") => 1,
                _ => 0,
            };
            assert_eq!(
                phase.get::<usize>(0).unwrap(),
                (reused > 0).then_some(1),
                "{history} {identity}"
            );
            assert_eq!(
                phase.get::<usize>(1).unwrap(),
                (reused == 3).then_some(2),
                "{history} {identity}"
            );
            assert!(
                events(&path).last().unwrap()["event"] != "campaign_completed",
                "{history} {identity}"
            );
            if workers == "Serial" {
                for index in reused..3 {
                    phase.record(index, &(index + 1)).unwrap();
                }
            } else {
                std::thread::scope(|scope| {
                    for index in reused..3 {
                        let phase = &phase;
                        scope.spawn(move || phase.record(index, &(index + 1)).unwrap());
                    }
                });
            }
            let outcomes: Vec<_> = (0..3).map(|i| phase.get::<usize>(i).unwrap()).collect();
            assert_eq!(outcomes, [Some(1), Some(2), Some(3)]);
            assert!(log.finish().is_err());
            phase.finish().unwrap();
            log.finish().unwrap();
            assert_eq!(events(&path).last().unwrap()["event"], "campaign_completed");
        }
    }

    #[test]
    fn checkpoint_rejects_invalid_transitions_without_losing_valid_results() {
        let dir = Scratch::new();
        let path = dir.path();
        let log = Checkpoint::open(&path).unwrap();
        assert!(log.begin("probe", "trace", keys(&["a"])).is_err());
        log.start("settings", Value::Null).unwrap();
        let phase = log.begin("probe", "trace", keys(&["a"])).unwrap();
        assert!(phase.finish().is_err());
        assert!(phase.record(1, &1).is_err());
        phase.record(0, &17).unwrap();
        assert!(phase.record(0, &3).is_err());
        assert_eq!(phase.get::<u8>(0).unwrap(), Some(17));
        assert!(log.begin("probe", "trace", keys(&["a"])).is_err());
        phase.finish().unwrap();
        assert!(phase.record(0, &3).is_err());
        log.finish().unwrap();

        log.start("settings", Value::Null).unwrap();
        let new_phase = log.begin("probe", "trace", keys(&["a"])).unwrap();
        assert!(phase.get::<u8>(0).is_err());
        assert_eq!(new_phase.get::<u8>(0).unwrap(), Some(17));
    }

    #[test]
    fn checkpoint_reports_the_later_phase_pending_after_resuming() {
        let dir = Scratch::new();
        let path = dir.path();
        let candidates = || keys(&["a", "b"]);
        {
            let log = Checkpoint::open(&path).unwrap();
            log.start("settings", Value::Null).unwrap();
            let probe = log.begin("probe", "build", candidates()).unwrap();
            probe.record(0, &10).unwrap();
            probe.record(1, &11).unwrap();
            probe.finish().unwrap();
            let source = log.begin("source", "build", candidates()).unwrap();
            source.record(1, &21).unwrap();
        }
        let log = Checkpoint::open(&path).unwrap();
        log.start("settings", Value::Null).unwrap();
        let probe = log.begin("probe", "build", candidates()).unwrap();
        probe.finish().unwrap();
        let source = log.begin("source", "build", candidates()).unwrap();
        assert_eq!(probe.get::<u8>(0).unwrap(), Some(10));
        assert_eq!(probe.get::<u8>(1).unwrap(), Some(11));
        assert_eq!(source.get::<u8>(0).unwrap(), None);
        assert_eq!(source.get::<u8>(1).unwrap(), Some(21));
        assert_eq!(
            events(&path).last().unwrap(),
            &json!({"event":"phase_resumed", "phase":"source", "reused":1, "pending":1})
        );
        assert!(log.finish().is_err());
        source.record(0, &20).unwrap();
        source.finish().unwrap();
        log.finish().unwrap();
    }

    #[test]
    fn checkpoint_can_recover_an_interrupted_format_header() {
        let dir = Scratch::new();
        let path = dir.path();
        fs::write(&path, &HEADER[..HEADER.len() - 2]).unwrap();
        let log = Checkpoint::open(&path).unwrap();
        log.start("settings", Value::Null).unwrap();
        assert_eq!(
            events(&path).first().unwrap(),
            &json!({"event":"format", "version":VERSION})
        );
    }

    #[test]
    fn checkpoint_preserves_other_files_and_complete_invalid_rows() {
        let dir = Scratch::new();
        let path = dir.path();
        for invalid in [
            b"unrelated file".as_slice(),
            b"{\"event\":\"format\",\"version\":2}\n",
            b"{\"event\":\"format\",\"version\":1}\ninvalid\n",
        ] {
            fs::write(&path, invalid).unwrap();
            assert!(Checkpoint::open(&path).is_err());
            assert_eq!(fs::read(&path).unwrap(), invalid);
        }
    }

    #[test]
    #[cfg(unix)]
    fn checkpoint_rejects_symbolic_and_hard_links() {
        let dir = Scratch::new();
        let path = dir.path();
        let outside = dir.0.join("keep");
        fs::write(&outside, b"unchanged").unwrap();
        std::os::unix::fs::symlink(&outside, &path).unwrap();
        assert!(Checkpoint::open(&path).is_err());
        fs::remove_file(&path).unwrap();
        fs::hard_link(&outside, &path).unwrap();
        assert!(Checkpoint::open(&path).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"unchanged");
    }
}
