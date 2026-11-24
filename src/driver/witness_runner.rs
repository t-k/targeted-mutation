//! Execution and shared-build verification of distinguishing witness tests.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::instrument::{self, Site};
use crate::report::{self};
use crate::trace::Trace;
use crate::witness;

use super::*;

pub(super) fn observe_witness(
    baseline: &TestBinary,
    exe: PathBuf,
    wrapper: Option<&Path>,
    scenario: &witness::Scenario,
    name: &str,
    compares: bool,
    in_root: bool,
) -> Result<Option<witness::Observation>> {
    use std::io::Read as _;
    let test_name = scenario.test_name(name);
    // In a file that is a module the test is under the module's name, which the
    // binary knows: the one test that ends with the name is it.
    let test_name = if in_root {
        test_name
    } else {
        let inner = format!("::{test_name}");
        let listed = list_tests(
            &exe,
            &baseline.cwd,
            &baseline.git_ceiling,
            false,
            false,
            baseline.work_lock.as_deref(),
        )?;
        let mut named = listed.into_iter().filter(|test| test.ends_with(&inner));
        match (named.next(), named.next()) {
            (Some(only), None) => only,
            _ => return Ok(None),
        }
    };
    let test = TestBinary {
        name: test_name.clone(),
        cwd: baseline.cwd.clone(),
        marks: baseline.marks.clone(),
        git_ceiling: baseline.git_ceiling.clone(),
        work_lock: baseline.work_lock.clone(),
        tests: BTreeSet::new(),
        runner: Runner::Isolated {
            exe,
            test: test_name,
            ignored: false,
            wrapper: wrapper.map(Path::to_path_buf),
        },
        baseline: baseline.baseline,
        untraced_baseline: None,
        timeout_diagnostic: std::sync::Once::new(),
        traced: Duration::ZERO,
        timeout: baseline.timeout,
        wrapper_grace: baseline.wrapper_grace,
        report_slow: false,
        filtered_timings: Default::default(),
        trace: Trace::default(),
        effects_baseline: std::sync::OnceLock::new(),
    };
    static RUNS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let path = reserve_file(&test.marks, "witness", &RUNS)?;
    let result = (|| {
        let Some(output) = path.to_str() else {
            return Ok(None);
        };
        if run_binary(
            &test,
            None,
            &[(witness::OUTPUT_ENV, output)],
            test.mutant_limit(test.baseline),
            RunOutput::Quiet,
            1,
        )? != Outcome::Pass
        {
            return Ok(None);
        }
        let mut text = String::new();
        const MAX_OBSERVATION: u64 = 2048;
        fs::File::open(&path)?
            .take(MAX_OBSERVATION + 1)
            .read_to_string(&mut text)?;
        Ok((text.len() <= MAX_OBSERVATION as usize)
            .then(|| scenario.observation(&text, compares))
            .flatten())
    })();
    release_mark(&path)?;
    result
}

pub(super) fn candidate_edit(
    file: &SourceFile,
    start: usize,
    end: usize,
    replacement: String,
) -> report::CandidateEdit {
    let (line, column) = file.lines.position(&file.original, start);
    report::CandidateEdit {
        file: file.display.clone(),
        line,
        column,
        start,
        end,
        before: file.original[start..end].to_string(),
        replacement,
    }
}

/// For every mutant of the source in a function that a test calls with a result it does
/// not pin down: that mutant with those calls. Each is only tried where the mutant is a
/// finding already; the calls are then run on both to see whether one tells them apart.
pub(super) fn stronger_assertions<'a>(
    original: &str,
    parsed: &syn::File,
    sites: impl Iterator<Item = &'a Site> + Clone,
) -> Vec<witness::Candidate> {
    let mut candidates = vec![];
    for function in witness::unpinned_calls(original, parsed) {
        let inside = sites
            .clone()
            .filter(|site| function.item.start <= site.start && site.end <= function.item.end);
        for site in inside {
            let lines = original
                .get(site.start..site.end)
                .map_or(0, |text| text.matches('\n').count());
            let mut mutants = instrument::source_mutants(site);
            mutants.extend(instrument::source_boundary_mutant(site));
            instrument::keep_line_ends(original, &mut mutants);
            for mutant in mutants {
                // A comparison is planned in other words than it is written in: the
                // finding may name the negation where the source says `!=`.
                for mutation in mutations_like(site, &mutant, false) {
                    candidates.push(witness::Candidate {
                        description: report::describe(site, mutation),
                        edits: vec![witness::Edit {
                            span: mutant.start..mutant.end,
                            replacement: mutant.replacement.clone(),
                        }],
                        scope: site.line..=site.line + lines,
                        signature_lines: vec![function.signature_line],
                        existing: true,
                        advice: None,
                        file: Default::default(),
                        scenarios: function.scenarios.clone(),
                        finding_at: Some((site.line, site.column)),
                    });
                }
            }
        }
    }
    candidates
}

pub(super) fn witness_location(
    source: &SourceFile,
    candidate: &witness::Candidate,
) -> report::Location {
    let span = &candidate.edits[0].span;
    let (line, column) = candidate
        .finding_at
        .unwrap_or_else(|| source.lines.position(&source.original, span.start));
    report::Location {
        file: source.display.clone(),
        line,
        column,
        code: report::one_line(&source.original[span.clone()]),
        ..Default::default()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum WitnessTrial {
    Inapplicable,
    Unviable,
    Evaluated(Outcome),
}

pub(super) struct WitnessSurvivor<'a> {
    pub(super) candidate: &'a witness::Candidate,
    pub(super) at: report::Location,
    pub(super) existing: Option<usize>,
    pub(super) scenarios: Vec<(&'a witness::Scenario, &'a TestBinary)>,
}

/// A private copy of the shared original witness binary so compiling a mutant into the
/// same Cargo target directory cannot overwrite the binary needed for post-mutant re-checks.
pub(super) struct SavedExecutable {
    pub(super) path: PathBuf,
}

impl SavedExecutable {
    pub(super) fn copy_from(exe: &Path) -> Result<Self> {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        Self::copy_from_with_counter(exe, &NEXT)
    }

    pub(super) fn copy_from_with_counter(
        exe: &Path,
        next: &std::sync::atomic::AtomicUsize,
    ) -> Result<Self> {
        let parent = exe
            .parent()
            .context("witness executable has no parent directory")?;
        let name = exe
            .file_name()
            .context("witness executable has no file name")?
            .to_string_lossy();
        let mut input = fs::File::open(exe)
            .with_context(|| format!("opening original witness binary {}", exe.display()))?;
        let permissions = input.metadata()?.permissions();
        for _ in 0..1024 {
            let id = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = parent.join(format!("tmt-orig-{}-{id}-{name}", std::process::id()));
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut output) => {
                    // Own the newly created file immediately so any copy/chmod failure removes it.
                    let saved = Self { path };
                    std::io::copy(&mut input, &mut output).with_context(|| {
                        format!(
                            "preserving original witness binary {} at {}",
                            exe.display(),
                            saved.path.display()
                        )
                    })?;
                    output.set_permissions(permissions)?;
                    drop(output);
                    return Ok(saved);
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!("creating preserved witness binary {}", path.display())
                    });
                }
            }
        }
        anyhow::bail!(
            "could not allocate a unique path to preserve witness binary {}",
            exe.display()
        )
    }
}

impl Drop for SavedExecutable {
    fn drop(&mut self) {
        if let Err(err) = fs::remove_file(&self.path)
            && err.kind() == std::io::ErrorKind::PermissionDenied
            && let Ok(mut perms) = fs::metadata(&self.path).map(|m| m.permissions())
        {
            #[allow(clippy::permissions_set_readonly_false)]
            perms.set_readonly(false);
            let _ = fs::set_permissions(&self.path, perms);
            let _ = fs::remove_file(&self.path);
        }
    }
}

pub(super) struct SharedWitness {
    pub(super) value: String,
    pub(super) name: String,
    pub(super) tests: std::rc::Rc<[witness::Edit]>,
    pub(super) original_exe: std::rc::Rc<SavedExecutable>,
}

pub(super) fn shared_witnesses(
    source: &str,
    survivors: &[WitnessSurvivor<'_>],
    wrapper: Option<&Path>,
    in_root: bool,
    compile: impl Fn(&[witness::Edit]) -> Result<Option<PathBuf>>,
    note: impl Fn(&str),
) -> Result<Vec<Option<SharedWitness>>> {
    let mut originals: Vec<_> = (0..survivors.len()).map(|_| None).collect();
    // A custom wrapper may recognize the individual witness test name.
    if wrapper.is_some() {
        return Ok(originals);
    }
    // Compile the first selected input of each survivor together, then compile the
    // typed checks of their values together. No later input is run speculatively.
    let inputs: Vec<_> = survivors
        .iter()
        .enumerate()
        .filter_map(|(index, survivor)| {
            let (scenario, runner) = *survivor.scenarios.first()?;
            Some((
                index,
                scenario,
                runner,
                format!("{}_{index}", witness::TEST_NAME),
            ))
        })
        .collect();
    for batch in inputs.chunks(32).filter(|batch| batch.len() > 1) {
        note(&format!(
            "sharing a build for {} original witness inputs",
            batch.len()
        ));
        let tests: Vec<_> = batch
            .iter()
            .map(|(_, scenario, _, name)| scenario.test_edit(source, name, None))
            .collect();
        let Some(exe) = compile(&tests)? else {
            note("shared witness build failed, trying inputs individually");
            continue;
        };
        let observed: Vec<_> = batch
            .iter()
            .filter_map(|(index, scenario, runner, name)| {
                let value =
                    observe_witness(runner, exe.clone(), wrapper, scenario, name, false, in_root)
                        .ok()??
                        .value;
                witness::is_expected_literal(&value)
                    .then_some((*index, *scenario, *runner, name, value))
            })
            .collect();
        if observed.is_empty() {
            continue;
        }
        let tests: std::rc::Rc<[_]> = observed
            .iter()
            .map(|(_, scenario, _, name, value)| scenario.test_edit(source, name, Some(value)))
            .collect();
        let Some(exe) = compile(&tests)? else {
            note("shared witness build failed, trying inputs individually");
            continue;
        };
        let saved = match SavedExecutable::copy_from(&exe) {
            Ok(saved) => std::rc::Rc::new(saved),
            Err(err) => {
                note(&format!(
                    "could not preserve shared witness binary, trying inputs individually: {err:#}"
                ));
                continue;
            }
        };
        for (index, scenario, runner, name, value) in observed {
            let checked = observe_witness(
                runner,
                saved.path.clone(),
                wrapper,
                scenario,
                name,
                true,
                in_root,
            );
            if checked.is_ok_and(|value_read| {
                value_read
                    .is_some_and(|read| read.equals_expected == Some(true) && read.value == value)
            }) {
                originals[index] = Some(SharedWitness {
                    value,
                    name: name.clone(),
                    tests: tests.clone(),
                    original_exe: saved.clone(),
                });
            }
        }
    }
    Ok(originals)
}
