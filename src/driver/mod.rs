//! Runs the whole pipeline on a cargo package:
//! copy -> instrument -> build once -> trace run -> plan -> run the infected mutants.
//!
//! The user's tree is never modified: everything happens in a copy under
//! `<target dir>/tmt/`.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};

use crate::checkpoint::{Checkpoint, Phase};
use crate::instrument::{self, RUNTIME_MOD, Rendered, Site, SiteKind};
use crate::output_fixture::{self, Fixture};
use crate::plan::{self, Mutation};
use crate::propagation::Change;
use crate::report::{self, Finding, Report};
use crate::scope::{self};
use crate::trace::Trace;
use crate::work_lock::WorkLock;
use crate::{git_index, leftovers, progress, reverts, witness};

#[derive(Clone)]
pub struct Options {
    /// Directory of the package (or workspace) to analyse.
    pub dir: PathBuf,
    pub package: Option<String>,
    /// Return source candidates without running a build.
    pub list: bool,
    /// Run every member's tests against the selected package.
    pub test_workspace: bool,
    /// Re-run the tests against one survivor per input gap to confirm the prediction.
    pub verify: bool,
    /// Where to put the copy and its build. Default: `<target dir>/tmt`.
    pub work_dir: Option<PathBuf>,
    /// Progress lines on stderr.
    pub progress: bool,
    /// Analyse only code changed since this git revision (plus untracked files).
    pub since: Option<String>,
    /// Analyse only these lines: `FILE:FROM-TO`, with `FILE` relative to the package.
    pub lines: Vec<String>,
    /// Findings listed in this file are not reported.
    pub baseline: Option<PathBuf>,
    /// Write the findings of this run to this file, as a baseline for later runs.
    pub write_baseline: Option<PathBuf>,
    /// Other workspace packages whose tests are run too.
    pub also_test: Vec<String>,
    /// Run the tests of every workspace package that depends on the analysed one.
    pub dependents: bool,
    /// Trace surviving mutants to say what they change in the tests.
    pub effects: bool,
    /// Build and run, one by one, the mutants that cannot be switched at run time.
    pub rebuild: bool,
    /// Rust expressions used as the payload of a Result error mutation.
    pub error_values: Vec<String>,
    pub source_only: bool,
    pub literal_operators: bool,
    pub shard: Option<report::Shard>,
    pub nextest: Option<report::Nextest>,
    pub test_selection: TestSelection,
    /// Optimisation level to build the tests with, instead of the profile's own.
    pub opt_level: Option<u8>,
    /// Override the time allowed for each test runner under a mutant.
    pub timeout_seconds: Option<u64>,
    pub baseline_jobs: usize,
    pub serial_timeout_rechecks: bool,
    pub wrapper_grace: Duration,
    /// Run this many surviving mutants against all tests and record the tests twice.
    pub audit: Option<usize>,
    /// Cargo's choice of features and of the lock file, the same for every cargo command.
    pub cargo: CargoConfig,
    /// How many mutants to run at the same time.
    pub jobs: usize,
    /// Copy every file of the workspace, including the ones git ignores.
    pub all_files: bool,
    pub git_index: bool,
    pub copy_fixtures: Vec<PathBuf>,
    pub copy_fixture_excludes: Vec<PathBuf>,
    pub output_fixtures: Vec<Fixture>,
    pub checkpoint: bool,
    /// Also run the doc tests (slow: rustdoc recompiles them for every mutant they reach).
    pub doc: bool,
}

#[derive(Clone, Default)]
pub struct TestSelection {
    pub isolate: bool,
    pub targets: Vec<String>,
    pub include: Vec<String>,
    pub exclude: Vec<String>,
    pub include_ignored: bool,
    pub wrapper: Option<PathBuf>,
}

impl TestSelection {
    fn enabled(&self) -> bool {
        self.isolate
            || !self.targets.is_empty()
            || !self.include.is_empty()
            || !self.exclude.is_empty()
            || self.include_ignored
            || self.wrapper.is_some()
    }
}

pub use crate::report::CargoConfig;

impl CargoConfig {
    /// The arguments that say the same to `cargo metadata`, `cargo test` and rustdoc.
    pub fn flags(&self) -> Vec<String> {
        let mut flags = vec![];
        if !self.features.is_empty() {
            flags.extend(["--features".to_string(), self.features.join(",")]);
        }
        for (on, flag) in [
            (self.all_features, "--all-features"),
            (self.no_default_features, "--no-default-features"),
            (self.locked, "--locked"),
        ] {
            if on {
                flags.push(flag.to_string());
            }
        }
        flags
    }
}

/// How far from an error to look for the probe that caused it (a probe can break type
/// inference a few lines further down).
const NEARBY_LINES: usize = 6;
/// File that marks a directory as tmt's own working directory.
const WORK_MARKER: &str = ".tmt-work-dir";
/// Name under which the crate root re-declares `std` for the runtime module.
const STD_ALIAS: &str = "__tmt_std";
/// More rebuilds than this means the blame logic is not converging.
const MAX_REPAIR_ROUNDS: usize = 40;
/// Above this many tests, run the whole binary instead of passing names.
const MAX_TEST_FILTERS: usize = 200;

mod cargo;
mod runner;
mod scope_blame;
mod session;
mod witness_runner;
mod workcopy;

use cargo::*;
use runner::*;
use scope_blame::*;
use session::*;
use witness_runner::*;
use workcopy::*;

struct SourceFile {
    /// Path relative to the package directory, for reports.
    display: String,
    /// Path inside the working copy.
    path: PathBuf,
    owners: BTreeSet<PathBuf>,
    original: String,
    lines: scope::LineIndex,
    sites: Vec<Site>,
    /// Sites whose mutants are built one by one.
    rebuild_sites: Vec<Site>,
    /// Where every site of the file is (line, column, code), also those a run of a part
    /// leaves out: findings are numbered by these.
    places: Vec<(usize, usize, String)>,
    rendered: Option<Rendered>,
}

fn write_rendered(
    file: &mut SourceFile,
    state: &WorkCopy,
    suffix: &str,
    disabled: &HashSet<u32>,
    newly_disabled: &HashSet<u32>,
) -> Result<()> {
    if file.rendered.is_none()
        || file
            .sites
            .iter()
            .any(|site| newly_disabled.contains(&site.id))
    {
        file.rendered = Some(instrument::render(&file.original, &file.sites, &|id| {
            !disabled.contains(&id)
        }));
    }
    let rendered = file.rendered.as_ref().expect("rendered before writing");
    if suffix.is_empty() {
        state.write(&file.path, &rendered.text)
    } else {
        state.write(&file.path, &format!("{}{suffix}", rendered.text))
    }
}

#[derive(Clone, Copy)]
struct WorkspaceShard {
    offset: usize,
    total: usize,
    member_total: usize,
}

struct CatalogueSnapshot {
    path: PathBuf,
    ready: std::cell::Cell<bool>,
    _lock: WorkLock,
}

struct WorkspaceBatch<'a> {
    metadata: &'a Metadata,
    work: &'a Path,
    catalogue: Option<CatalogueSnapshot>,
}

/// Collect the original aggregate report for callers that want it.
pub fn run_packages(options: &Options, names: &[String]) -> Result<Vec<(String, Result<Report>)>> {
    let mut reports = vec![];
    run_packages_each(options, names, |name, result| {
        reports.push((name, result));
        Ok(())
    })?;
    Ok(reports)
}

/// Emit each completed package before starting the next one.
pub fn run_packages_each(
    options: &Options,
    names: &[String],
    mut completed: impl FnMut(String, Result<Report>) -> Result<()>,
) -> Result<()> {
    if options.baseline.is_some() || options.write_baseline.is_some() {
        bail!("baseline files require a single package");
    }
    let metadata = cargo_metadata(&options.dir, &options.cargo)?;
    let all: BTreeSet<_> = metadata.packages.iter().map(|p| p.name.clone()).collect();
    let selected = if names.is_empty() {
        all.clone()
    } else {
        names.iter().cloned().collect()
    };
    if let Some(unknown) = selected.difference(&all).next() {
        bail!("no package `{unknown}` in the workspace");
    }
    let base = options
        .work_dir
        .clone()
        .unwrap_or_else(|| metadata.target_directory.join("tmt-workspace"));
    fs::create_dir_all(&base).with_context(|| format!("creating {}", base.display()))?;
    let base = base.canonicalize()?;
    if metadata.workspace_root.canonicalize()?.starts_with(&base) {
        bail!(
            "the work directory {} contains the workspace",
            base.display()
        );
    }
    let catalogue = if options.list || options.shard.is_some() {
        let path = work_directory(&base, ".catalogue")?;
        Some(CatalogueSnapshot {
            _lock: WorkLock::take(&path)?,
            path,
            ready: std::cell::Cell::new(false),
        })
    } else {
        None
    };
    let batch = WorkspaceBatch {
        metadata: &metadata,
        work: &base,
        catalogue,
    };
    let member_options = selected.into_iter().map(|name| {
        let mut member = options.clone();
        member.package = Some(name.clone());
        member.work_dir = Some(base.join(&name));
        (name, member)
    });
    let Some(shard) = options.shard else {
        for (name, member) in member_options {
            let result = run_package(&member, Some(&batch), None);
            completed(name, result)?;
        }
        return Ok(());
    };
    if !options.source_only || shard.index >= shard.count {
        bail!("shards require --source-only and a valid shard index");
    }
    // A shard needs the counts of preceding members. For an executing batch
    // retain counts only: the entire catalogue need not stay in memory.
    let catalogues: Vec<_> = member_options
        .map(|(name, member)| {
            let mut preview = member.clone();
            preview.list = true;
            preview.shard = None;
            preview.progress = false;
            let catalogue = run_package(&preview, Some(&batch), None).map(|mut report| {
                let count = report.mutants.as_ref().map_or(0, Vec::len);
                if !member.list {
                    report.mutants = None;
                }
                (count, report)
            });
            (name, member, catalogue)
        })
        .collect();
    let total = catalogues
        .iter()
        .filter_map(|(_, _, catalogue)| catalogue.as_ref().ok())
        .map(|(count, _)| count)
        .sum();
    let mut offset = 0;
    for (name, member, catalogue) in catalogues {
        let result = catalogue.and_then(|(member_total, mut report)| {
            let range = WorkspaceShard {
                offset,
                total,
                member_total,
            };
            offset += member_total;
            if member.list {
                report.analysis.shard = Some(shard);
                let mut index = range.offset;
                report
                    .mutants
                    .as_mut()
                    .expect("source catalogue")
                    .retain(|_| {
                        let keep = shard.contains(index, total);
                        index += 1;
                        keep
                    });
                Ok(report)
            } else {
                run_package(&member, Some(&batch), Some(range))
            }
        });
        completed(name, result)?;
    }
    Ok(())
}

/// Says at the end of a package, however it ends, what its runs left behind.
struct LeftoverNote(bool);

impl Drop for LeftoverNote {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        let stopped = leftovers::STOPPED.swap(0, Ordering::Relaxed);
        let not_stopped = leftovers::NOT_STOPPED.swap(0, Ordering::Relaxed);
        if let Some(text) = leftover_note(stopped, not_stopped)
            && self.0
        {
            eprintln!("tmt: {text}");
        }
    }
}

fn leftover_note(stopped: usize, not_stopped: usize) -> Option<String> {
    let stopped = (stopped > 0).then(|| {
        format!("stopped {stopped} processes that the tests had left running after their run")
    });
    let not_stopped = (not_stopped > 0).then(|| {
        format!(
            "after {not_stopped} runs something the tests started may still be running \
             (it could not be found or stopped)"
        )
    });
    match (stopped, not_stopped) {
        (None, None) => None,
        (Some(one), None) | (None, Some(one)) => Some(one),
        (Some(stopped), Some(not_stopped)) => Some(format!("{stopped}; {not_stopped}")),
    }
}

pub fn run(options: &Options) -> Result<Report> {
    run_package(options, None, None)
}

fn run_package(
    options: &Options,
    batch: Option<&WorkspaceBatch<'_>>,
    workspace_shard: Option<WorkspaceShard>,
) -> Result<Report> {
    let _leftovers = LeftoverNote(options.progress);
    if let Some(shard) = options.shard
        && (!options.source_only || shard.index >= shard.count)
    {
        bail!("shards require --source-only and a valid shard index");
    }
    let mut seen_errors = HashSet::new();
    let mut error_values = vec![];
    for expression in &options.error_values {
        let expression = expression.trim();
        // The text is written into the code of probes, each on the line of its site: it
        // is one line, and has nothing in it that is not to be seen.
        if expression.chars().any(char::is_control) {
            bail!(
                "invalid --error expression {expression:?}: it has to be on one line, \
                 without control characters"
            );
        }
        let parsed = syn::parse_str::<syn::Expr>(expression)
            .with_context(|| format!("invalid --error expression {expression:?}"))?;
        if seen_errors.insert(quote::ToTokens::to_token_stream(&parsed).to_string()) {
            error_values.push(expression);
        }
    }
    let selection = &options.test_selection;
    if options.nextest.is_some() {
        if !cfg!(unix) {
            bail!("the nextest runner is available on Unix");
        }
        if !options.source_only || options.doc || selection.enabled() {
            bail!(
                "--test-tool nextest requires --source-only; select tests through the nextest profile"
            );
        }
    }
    if options.baseline_jobs > 1 && !selection.enabled() {
        bail!("--baseline-jobs requires isolated tests or a test wrapper or selection");
    }
    if options.doc && selection.enabled() {
        bail!("--doc cannot be combined with isolated test selection");
    }
    let note = |text: &str| {
        if options.progress {
            eprintln!("tmt: {text}");
        }
    };
    let dir = options
        .dir
        .canonicalize()
        .with_context(|| format!("no such directory: {}", options.dir.display()))?;

    let local_metadata;
    let metadata = if let Some(batch) = batch {
        batch.metadata
    } else {
        local_metadata = cargo_metadata(&dir, &options.cargo)?;
        &local_metadata
    };
    let catalogue = batch
        .and_then(|batch| batch.catalogue.as_ref())
        .filter(|_| options.list);
    let package = match &options.package {
        Some(name) => metadata.packages.iter().find(|p| &p.name == name),
        None => metadata
            .packages
            .iter()
            .find(|p| p.manifest_path.parent() == Some(dir.as_path()))
            .or(match &metadata.packages[..] {
                [only] => Some(only),
                _ => None,
            }),
    }
    .ok_or_else(|| anyhow!("cannot tell which package to analyse; pass --package <name>"))?;
    // The packages whose tests are built and run: the analysed one, and on request others
    // of the workspace that use it.
    let mut test_packages: Vec<&Package> = vec![package];
    for name in &options.also_test {
        let other = metadata
            .packages
            .iter()
            .find(|p| &p.name == name)
            .ok_or_else(|| anyhow!("--also-test: no package `{name}` in the workspace"))?;
        if !test_packages.iter().any(|p| p.name == other.name) {
            test_packages.push(other);
        }
    }
    if options.test_workspace {
        test_packages.extend(
            metadata
                .packages
                .iter()
                .filter(|other| {
                    !test_packages
                        .iter()
                        .any(|selected| selected.name == other.name)
                })
                .collect::<Vec<_>>(),
        );
    }
    if options.dependents {
        for name in dependents_of(&package.name, &metadata.packages) {
            if let Some(other) = metadata.packages.iter().find(|p| p.name == name)
                && !test_packages.iter().any(|p| p.name == other.name)
            {
                test_packages.push(other);
            }
        }
    }
    let is_kind = |t: &Target, kinds: &[&str]| t.kind.iter().any(|k| kinds.contains(&k.as_str()));
    let root_target = package
        .targets
        .iter()
        .find(|t| {
            is_kind(
                t,
                &["lib", "rlib", "cdylib", "staticlib", "dylib", "proc-macro"],
            )
        })
        .or_else(|| package.targets.iter().find(|t| is_kind(t, &["bin"])))
        .ok_or_else(|| anyhow!("package `{}` has no lib or bin target", package.name))?;

    // 2. Copy the workspace.
    let workspace = metadata.workspace_root.canonicalize()?;
    let requested_work = catalogue
        .map(|catalogue| &catalogue.path)
        .or(options.work_dir.as_ref());
    let work = match requested_work {
        Some(dir) => {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            dir.canonicalize()?
        }
        None => metadata.target_directory.join("tmt"),
    };
    // The copy is brought up to date on every run, and files that are no longer wanted
    // are deleted from it. Make sure it is ours to delete from.
    if workspace.starts_with(&work) {
        bail!(
            "the work directory {} contains the workspace; choose a directory of its own",
            work.display()
        );
    }
    fs::create_dir_all(&work).with_context(|| format!("creating {}", work.display()))?;
    let work = work.canonicalize()?;
    let work_lock = if catalogue.is_none() {
        Some(Arc::new(WorkLock::take(&work)?))
    } else {
        None
    };
    let copy = work.join("src");
    let marker = work.join(WORK_MARKER);
    if copy.exists() && !marker.exists() {
        bail!(
            "{} exists and was not created by tmt; refusing to delete it. \
                 Choose another --work-dir or remove it yourself.",
            copy.display()
        );
    }
    fs::write(
        &marker,
        "Working directory of tmt (targeted-mutation). Safe to delete.\n",
    )?;
    let target_dir = metadata.target_directory.clone();
    let forbidden = |path: &Path| {
        path == target_dir
            || path == work
            || batch.is_some_and(|batch| path == batch.work)
            || path.file_name().is_some_and(|n| n == ".git")
            || (path.parent() == Some(workspace.as_path())
                && path.file_name().is_some_and(|n| n == "target"))
    };
    let state = WorkCopy::open(&work, &copy);
    let fixtures = options
        .copy_fixtures
        .iter()
        .map(|path| fixture_relative(path))
        .collect::<Result<Vec<_>>>()?;
    let excluded = fixture_exclusions(&workspace, &fixtures, &options.copy_fixture_excludes)?;
    let fixture_forbidden = |path: &Path| forbidden(path) || excluded.contains(path);
    let skip = |path: &Path| {
        fixture_forbidden(path) || path.file_name().is_some_and(|n| n == "node_modules")
    };
    if options.progress && !options.list {
        for path in &excluded {
            note(&format!(
                "excluded fixture {}",
                path.strip_prefix(&workspace).unwrap_or(path).display()
            ));
        }
    }
    if catalogue.is_none_or(|catalogue| !catalogue.ready.get()) {
        work_directory(&work, "src")?;
        let visible = if options.all_files {
            None
        } else {
            git_visible_files(&workspace)?
        };
        match visible {
            // Only what git would not ignore: build output, caches, local notes and secrets
            // that are kept out of the repository stay out of the copy too.
            Some(files) => {
                let copied = copy_git_visible(&workspace, &copy, files, &workspace, &skip, &state)?;
                note(&format!(
                    "copied {copied} files (ignored files are left out)"
                ));
            }
            None => copy_tree(&workspace, &copy, &workspace, &skip, &state)?,
        }
        for relative in &fixtures {
            copy_fixture_entry(
                &workspace.join(relative),
                &copy.join(relative),
                &workspace,
                &fixture_forbidden,
                &state,
                &mut BTreeSet::new(),
            )?;
        }
        prune(&copy, &state)?;
        if options.git_index {
            git_index::snapshot(&workspace, &copy)?;
        }
        if !options.list {
            // Relative path dependencies that leave the workspace would point nowhere from the copy.
            let mut manifests: Vec<&Path> = metadata
                .packages
                .iter()
                .map(|p| p.manifest_path.as_path())
                .collect();
            let root_manifest = metadata.workspace_root.join("Cargo.toml");
            manifests.push(&root_manifest);
            for manifest in manifests {
                let Ok(canonical) = manifest.canonicalize() else {
                    continue;
                };
                let (Some(manifest_dir), Ok(relative)) =
                    (canonical.parent(), canonical.strip_prefix(&workspace))
                else {
                    continue;
                };
                let text = fs::read_to_string(&canonical)?;
                if let Some(rewritten) =
                    absolutize_path_dependencies(&text, manifest_dir, &workspace, &copy)?
                {
                    state.write(&copy.join(relative), &rewritten)?;
                }
            }
        }
        if let Some(catalogue) = catalogue {
            state.settle(&workspace)?;
            catalogue.ready.set(true);
        }
    }

    let in_copy = |path: &Path| -> Result<PathBuf> {
        let canonical = path.canonicalize()?;
        let relative = canonical
            .strip_prefix(&workspace)
            .with_context(|| format!("{} is outside the workspace", path.display()))?;
        Ok(copy.join(relative))
    };
    let package_dir = in_copy(
        package
            .manifest_path
            .parent()
            .expect("manifest has a parent"),
    )?;
    let root_file = in_copy(&root_target.src_path)?;
    let source_dir = root_file
        .parent()
        .expect("source file has a parent")
        .to_path_buf();

    // 3. Walk all production roots. Other Cargo targets can include a production
    // module by path, so record them as consumers when deciding what can be probed.
    let mut roots = BTreeMap::new();
    for owner in &metadata.packages {
        for target in &owner.targets {
            let production = owner.name == package.name && target.is_production();
            if let Ok(path) = in_copy(&target.src_path)
                && path.is_file()
            {
                let selected = roots.entry(path).or_insert(false);
                *selected |= production;
            }
        }
    }
    let paths = source_graph(roots, &copy, |path| state.original(path, &workspace))?;
    let scope = requested_scope(options, &workspace, &package_dir, &copy)?;
    let mut skipped = vec![];
    let mut files = vec![];
    let mut next_id = 0u32;
    let want_witnesses = (options.list || options.rebuild || options.source_only)
        && root_target.test_build_arg() == Some(("--lib", None));
    let mut witness_candidates = vec![];
    for (path, owners) in paths {
        let Some(original) = state.original(&path, &workspace)? else {
            continue; // left by an earlier run; gone or rewritten before the build
        };
        let selected_file = scope.as_ref().is_none_or(|scope| {
            let relative = path.strip_prefix(&copy).unwrap_or(&path).to_string_lossy();
            scope.touches(&relative, 1, usize::MAX)
        });
        if !selected_file && path != root_file {
            continue;
        }
        let display = relative_source(&path, &package_dir).display().to_string();
        match instrument::parse_source(&original) {
            Ok(parsed) => {
                let mut found =
                    instrument::discover_parsed(&original, &parsed, next_id, &error_values);
                // A test can be put into a file that the library alone builds.
                if want_witnesses
                    && selected_file
                    && owners.consumers.len() == 1
                    && owners.consumers.contains(&root_file)
                {
                    witness_candidates.extend(
                        witness::discover_parsed(&original, &parsed, !options.list)
                            .into_iter()
                            .map(|mut candidate| {
                                candidate.file = path.clone();
                                candidate
                            }),
                    );
                    // With `--literal-operators` the mutant that was built is not the
                    // one these candidates write.
                    if !options.list && !options.literal_operators {
                        witness_candidates.extend(
                            // Also for what is mutated by building it only (the
                            // product of two literals).
                            stronger_assertions(
                                &original,
                                &parsed,
                                found.sites.iter().chain(&found.rebuild_sites),
                            )
                            .into_iter()
                            .map(|mut candidate| {
                                candidate.file = path.clone();
                                candidate
                            }),
                        );
                    }
                }
                next_id += found.sites.len() as u32;
                if owners.consumers.len() != 1 || !owners.consumers.contains(&root_file) {
                    found.rebuild_sites.append(&mut found.sites);
                }
                let places = if options.list {
                    vec![]
                } else {
                    found
                        .sites
                        .iter()
                        .chain(&found.rebuild_sites)
                        .map(|site| (site.line, site.column, report::one_line(&site.text)))
                        .collect()
                };
                files.push(SourceFile {
                    display,
                    path,
                    owners: owners.production,
                    lines: scope::LineIndex::new(&original),
                    original,
                    places,
                    sites: found.sites,
                    rebuild_sites: found.rebuild_sites,
                    rendered: None,
                });
            }
            Err(err) => {
                if selected_file {
                    let diagnostic = format!("{err:#}");
                    note(&format!("skipping {display}: {diagnostic}"));
                    let end_line = scope::line_of(&original, original.len());
                    let end_column = original.rsplit('\n').next().unwrap_or("").chars().count() + 1;
                    skipped.push(report::SkippedSite::new(
                        report::Location {
                            file: display,
                            line: 1,
                            column: 1,
                            ..Default::default()
                        },
                        (end_line, end_column),
                        report::SkipReason::UnparseableSource,
                        Some(diagnostic),
                    ));
                }
            }
        }
    }

    // Where the tests of a file changed and nothing else says so, the whole program of
    // the file is looked at: what they found out before and let through now is theirs.
    let since_hunks = match options.since.as_deref() {
        Some(revision) if !options.list && options.shard.is_none() => {
            scope::hunks(&since_diff(&workspace, revision)?)
        }
        _ => vec![],
    };
    let mut tests_changed: Vec<(PathBuf, Vec<scope::Hunk>)> = vec![];
    // Files with a change that is tried undone: they are looked at even with no site.
    let mut undo_paths: HashSet<PathBuf> = HashSet::new();
    let mut beyond_places: HashSet<(PathBuf, usize, usize)> = HashSet::new();
    // Only the requested lines, if any were requested.
    if let Some(scope) = &scope {
        for file in &mut files {
            let relative = file
                .path
                .strip_prefix(&copy)
                .unwrap_or(&file.path)
                .to_string_lossy()
                .into_owned();
            let in_scope = |site: &Site| {
                scope.touches(&relative, file.lines.line_of(site.start), file.lines.line_of(site.end))
                    // A function is in scope by its signature line too.
                    || scope.touches(&relative, site.line, site.line)
            };
            let of_file: Vec<scope::Hunk> = since_hunks
                .iter()
                .filter(|hunk| hunk.file == relative)
                .cloned()
                .collect();
            if (options.rebuild || options.source_only)
                && (!reverts::changes(&file.original, &of_file).is_empty()
                    || reverts::mixed(&file.original, &of_file) > 0)
            {
                undo_paths.insert(file.path.clone());
            }
            let of_tests = reverts::test_hunks(&file.original, &of_file);
            if of_tests.is_empty() {
                file.sites.retain(in_scope);
            } else {
                beyond_places.extend(
                    file.sites
                        .iter()
                        .filter(|site| !in_scope(site))
                        .map(|site| (file.path.clone(), site.start, site.end)),
                );
                tests_changed.push((file.path.clone(), of_tests));
            }
            file.rebuild_sites.retain(in_scope);
        }
        files.retain(|f| {
            !f.sites.is_empty()
                || !f.rebuild_sites.is_empty()
                || f.path == root_file
                || witness_candidates.iter().any(|c| c.file == f.path)
                || undo_paths.contains(&f.path)
        });
    }
    if !options.list {
        for candidate in witness_candidates.iter().filter(|c| !c.existing) {
            if let Some(file) = files.iter_mut().find(|file| file.path == candidate.file) {
                let at = witness_location(file, candidate);
                file.places.push((at.line, at.column, at.code));
            }
        }
    }
    witness_candidates.retain(|candidate| {
        let Some(file) = files.iter().find(|file| file.path == candidate.file) else {
            return false;
        };
        let relative = file
            .path
            .strip_prefix(&copy)
            .unwrap_or(&file.path)
            .to_string_lossy();
        scope.as_ref().is_none_or(|scope| {
            let site_in_scope = if candidate.edits.len() == 1 {
                scope.touches(&relative, *candidate.scope.start(), *candidate.scope.end())
            } else {
                candidate.edits.iter().any(|edit| {
                    scope.touches(
                        &relative,
                        file.lines.line_of(edit.span.start),
                        file.lines.line_of(edit.span.end),
                    )
                })
            };
            site_in_scope
                || candidate
                    .signature_lines
                    .iter()
                    .any(|&line| scope.touches(&relative, line, line))
        })
    });
    let total_sites: usize = files
        .iter()
        .map(|f| f.sites.len() + f.rebuild_sites.len())
        .sum::<usize>()
        + witness_candidates
            .iter()
            .filter(|candidate| !candidate.existing)
            .count();
    note(&format!("{} sites in {} files", total_sites, files.len()));
    if options.source_only {
        for file in &mut files {
            file.rebuild_sites.append(&mut file.sites);
        }
    }

    let source_mutants = |file: &SourceFile, site: &Site, include_boundary: bool| {
        let mut mutants = instrument::source_mutants(site);
        if include_boundary {
            mutants.extend(instrument::source_boundary_mutant(site));
        }
        if options.literal_operators {
            instrument::literal_operator_mutants(&file.original, site, &mut mutants);
        }
        instrument::keep_line_ends(&file.original, &mut mutants);
        mutants
    };
    let counts = options.shard.map(|_| {
        let regular: usize = files
            .iter()
            .map(|file| {
                file.rebuild_sites
                    .iter()
                    .map(|site| source_mutants(file, site, true).len())
                    .sum::<usize>()
            })
            .sum();
        let total = regular
            + witness_candidates
                .iter()
                .filter(|candidate| !candidate.existing)
                .count();
        (regular, total)
    });
    if let (Some(range), Some((_, local_total))) = (workspace_shard, counts)
        && range.member_total != local_total
    {
        bail!("package catalogue changed while preparing its workspace shard");
    }
    let in_shard = |index| match (options.shard, counts, workspace_shard) {
        (Some(shard), _, Some(range)) => shard.contains(range.offset + index, range.total),
        (Some(shard), Some((_, total)), _) => shard.contains(index, total),
        _ => true,
    };
    if let Some((mut index, _)) = counts {
        witness_candidates.retain(|candidate| {
            if candidate.existing {
                return true;
            }
            let selected = in_shard(index);
            index += 1;
            selected
        });
    }
    let empty_shard = counts.is_some_and(|(_, total)| !(0..total).any(in_shard));

    let package_path = package.manifest_path.parent().unwrap_or(Path::new("."));
    let mut report_path = repository_prefix(package_path);
    if report_path.is_empty()
        && batch.is_some()
        && let Ok(relative) = package_path.strip_prefix(&workspace)
    {
        report_path = workspace_report_prefix(relative);
    }
    let mut report = Report {
        package: package.name.clone(),
        sites: if empty_shard { 0 } else { total_sites },
        analysis: report::Analysis {
            package_path: report_path,
            since: options.since.clone(),
            lines: options.lines.clone(),
            tests_of: test_packages.iter().map(|p| p.name.clone()).collect(),
            cargo: options.cargo.clone(),
            opt_level: options.opt_level,
            timeout_seconds: options.timeout_seconds,
            baseline_jobs: (options.baseline_jobs > 1).then_some(options.baseline_jobs),
            timeout_recheck: if options.serial_timeout_rechecks {
                "serial"
            } else {
                "none"
            },
            wrapper_grace_seconds: selection
                .wrapper
                .as_ref()
                .map(|_| options.wrapper_grace.as_secs()),
            rebuild: options.rebuild || options.source_only,
            verified: options.verify && !options.source_only,
            doc_tests: options.doc,
            source_only: options.source_only,
            literal_operators: options.literal_operators,
            shard: options.shard,
            nextest: options.nextest.clone(),
            error_values: error_values
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            test_selection: None,
            git_index: options.git_index,
            copy_fixtures: options.copy_fixtures.clone(),
            copy_fixture_excludes: options.copy_fixture_excludes.clone(),
            output_fixtures: options.output_fixtures.clone(),
            checkpoint: (options.checkpoint && !options.list)
                .then(|| work.join("checkpoint.jsonl")),
        },
        skipped,
        ..Report::default()
    };
    if options.list {
        if catalogue.is_none() {
            state.settle(&workspace)?;
        }
        let mut candidates = vec![];
        let mut index = 0;
        for file in &files {
            let sites = file
                .sites
                .iter()
                .map(|site| (site, false))
                .chain(file.rebuild_sites.iter().map(|site| (site, true)));
            for (site, requires_rebuild) in sites {
                for mutant in source_mutants(file, site, true) {
                    if in_shard(index) {
                        candidates.push(report::Candidate {
                            what: mutant.what,
                            into: mutant.into,
                            requires_rebuild,
                            edits: vec![candidate_edit(
                                file,
                                mutant.start,
                                mutant.end,
                                mutant.replacement,
                            )],
                        });
                    }
                    index += 1;
                }
            }
        }
        candidates.extend(
            witness_candidates
                .iter()
                .filter(|candidate| !candidate.existing)
                .filter_map(|candidate| {
                    let file = files.iter().find(|file| file.path == candidate.file)?;
                    Some(report::Candidate {
                        what: "witness".into(),
                        into: candidate.description.clone(),
                        requires_rebuild: true,
                        edits: candidate
                            .edits
                            .iter()
                            .map(|edit| {
                                candidate_edit(
                                    file,
                                    edit.span.start,
                                    edit.span.end,
                                    edit.replacement.clone(),
                                )
                            })
                            .collect(),
                    })
                }),
        );
        report.mutants = Some(candidates);
        return Ok(report);
    }
    if empty_shard || (scope.is_some() && total_sites == 0 && undo_paths.is_empty()) {
        // Restore files changed by a previous run before reporting a selection with no work.
        state.settle(&workspace)?;
        if let Some(path) = &options.baseline {
            crate::baseline::load(path)?;
        }
        if let Some(path) = &options.write_baseline {
            crate::baseline::write(path, &[])?;
        }
        report.record_evaluation();
        if let Some(path) = report.analysis.checkpoint.as_deref() {
            let checkpoint = Checkpoint::open(path)?;
            let settings =
                serde_json::json!({"analysis": &report.analysis, "package": &package.name});
            let identity = crate::campaign_identity::compute(&copy, &settings, [], false)?;
            checkpoint.start(&identity, settings)?;
            checkpoint.finish()?;
        }
        return Ok(report);
    }

    // The injected runtime indexes its per-thread arrays by probe ID. Number the
    // retained probes together so a narrow scope does not allocate slots for omitted sites.
    for (id, site) in files
        .iter_mut()
        .flat_map(|file| &mut file.sites)
        .enumerate()
    {
        site.id = id as u32;
    }
    // The sites outside of what changed, looked at for the sake of the tests that did.
    let beyond: HashSet<u32> = files
        .iter()
        .flat_map(|file| {
            file.sites
                .iter()
                .filter(|site| beyond_places.contains(&(file.path.clone(), site.start, site.end)))
                .map(|site| site.id)
        })
        .collect();

    let root_source = state
        .original(&root_file, &workspace)?
        .with_context(|| format!("{} is missing", root_target.src_path.display()))?;
    let root_suffix = if options.source_only || files.iter().all(|file| file.sites.is_empty()) {
        String::new()
    } else {
        state.write(
            &source_dir.join(format!("{RUNTIME_MOD}.rs")),
            crate::RUNTIME_SOURCE,
        )?;
        let std_alias = format!("extern crate std as {STD_ALIAS};");
        if root_source.contains(&std_alias) {
            format!("\n#[doc(hidden)]\nmod {RUNTIME_MOD};\n")
        } else {
            format!("\n#[doc(hidden)]\n{std_alias}\n#[doc(hidden)]\nmod {RUNTIME_MOD};\n")
        }
    };

    // 4. Build, disabling the sites whose probe does not compile (formal/tla/RepairLoop.tla).
    let mut disabled: HashSet<u32> = HashSet::new();
    let mut repairs = BTreeMap::new();
    let build_target = work_directory(&work, "target")?;
    let target_flags = test_build_flags(
        &test_packages,
        &selection.targets,
        &workspace,
        &options.cargo,
    );
    let build_scope = if target_flags.is_some() {
        report::TestBuildScope::SelectedTargets
    } else {
        report::TestBuildScope::PackageWide
    };
    let build_note = if !selection.targets.is_empty() && target_flags.is_none() {
        "Cargo is using a package-wide build because the selected test targets cannot be expressed exactly.\n"
    } else {
        ""
    };
    if !build_note.is_empty() {
        note(build_note.trim_end());
    }
    // An observation uses only the library's unit-test artifact. The baseline and
    // source mutants still need the normal library build to check cfg(not(test)).
    let can_build_only_observer = !options.doc
        && target_flags.is_none()
        && test_packages.len() == 1
        && package.targets.len() == 1
        && root_target.test
        && root_target.test_build_arg() == Some(("--lib", None));
    let build_test_binaries = |observer: bool| {
        let mut command = Command::new("cargo");
        isolate_copy_command(&mut command, &work);
        if let Some(lock) = &work_lock {
            lock.retain_for(&mut command);
        }
        command
            .args(["test", "--no-run", "--message-format=json"])
            .args(test_packages.iter().flat_map(|p| ["-p", p.name.as_str()]))
            .args(target_flags.as_deref().unwrap_or_default())
            .args((observer && can_build_only_observer).then_some("--lib"))
            .args(options.cargo.flags())
            .current_dir(&copy)
            .env("CARGO_TARGET_DIR", &build_target)
            .env("CARGO_BUILD_BUILD_DIR", &build_target)
            .envs(profile_env(options.opt_level))
            .stderr(Stdio::piped())
            .output()
            .context("running cargo test --no-run")
    };
    let cargo_build = || build_test_binaries(false);
    let mut rounds = 0;
    let mut newly_disabled = HashSet::new();
    let build = loop {
        for file in &mut files {
            let suffix = if file.path == root_file {
                root_suffix.as_str()
            } else {
                ""
            };
            write_rendered(file, &state, suffix, &disabled, &newly_disabled)?;
        }
        if !files.iter().any(|f| f.path == root_file) {
            state.write(&root_file, &format!("{root_source}{root_suffix}"))?;
        }
        if rounds == 0 {
            state.settle(&workspace)?;
        }
        note(if rounds == 0 {
            "building the instrumented crate"
        } else {
            "rebuilding without the sites that failed to compile"
        });
        let output = cargo_build()?;
        let names: Vec<&str> = test_packages.iter().map(|p| p.name.as_str()).collect();
        let build = parse_build_output(&String::from_utf8_lossy(&output.stdout), &names);
        if output.status.success() {
            break build;
        }
        let diagnostics = || {
            [
                build.diagnostics.join("\n"),
                String::from_utf8_lossy(&output.stderr).into_owned(),
            ]
            .into_iter()
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n")
        };
        if build.errors.is_empty() {
            bail!("the build failed:\n{build_note}{}", diagnostics());
        }
        rounds += 1;
        let before = disabled.len();
        let mut blamed_now: HashSet<u32> = HashSet::new();
        for error in &build.errors {
            let blamed = blame_error(error, &files, &copy, &disabled);
            if std::env::var_os("TMT_DEBUG").is_some() {
                eprintln!("tmt: debug: blaming {blamed:?} for\n{}", error.rendered);
            }
            for id in blamed {
                repairs
                    .entry(id)
                    .or_insert_with(|| error.rendered.lines().next().unwrap_or("").to_string());
                blamed_now.insert(id);
            }
        }
        disabled.extend(blamed_now.iter().copied());
        newly_disabled = blamed_now;
        if disabled.len() == before || rounds > MAX_REPAIR_ROUNDS {
            bail!(
                "the crate does not build after instrumentation.\n\
                 {build_note}Does `cargo test --no-run` pass on the original for the same targets?\n{}",
                diagnostics()
            );
        }
    };
    let built_sources = if options.rebuild || options.source_only || options.audit.is_some() {
        // A feature can also be turned on behind cargo's back: by compiler flags (in
        // the environment or a configuration file) and by a build script.
        // Every variable of flags for the compiler: also those of one target
        // (`CARGO_TARGET_<TRIPLE>_RUSTFLAGS`).
        let behind_cargo = features_from_flags(
            std::env::vars_os()
                .filter(|(name, _)| name.to_string_lossy().ends_with("RUSTFLAGS"))
                .map(|(_, value)| Some(value)),
        ) || build.build_scripts.iter().any(|script| {
            script["cfgs"].as_array().is_some_and(|cfgs| {
                cfgs.iter()
                    .any(|cfg| cfg.as_str().is_some_and(|cfg| cfg.contains("feature")))
            })
        }) || configuration_names_a_feature(
            &copy,
            std::env::var_os("CARGO_HOME").map(PathBuf::from),
        );
        BuiltSources::from_artifacts(&build.artifacts, &build.features, !behind_cargo, &copy)
    } else {
        BuiltSources::default()
    };
    output_fixture::stage(&workspace, &work, &build_target, &options.output_fixtures)?;
    let checkpoint_path = report.analysis.checkpoint.as_deref();
    let checkpoint = checkpoint_path.map(Checkpoint::open).transpose()?;
    let doc_packages = test_packages
        .iter()
        .filter(|p| options.doc && p.targets.iter().any(|target| target.doctest));
    if build.test_binaries.is_empty() && doc_packages.clone().next().is_none() {
        bail!("{build_note}`{}` has no test binaries", package.name);
    }

    // 5. Trace run: what does each test observe?
    let mut session = Session {
        progress: options.progress,
        serial_timeout_rechecks: options.serial_timeout_rechecks,
        package_dir,
        binaries: vec![],
    };
    let wrapper = selection
        .wrapper
        .as_ref()
        .map(|path| {
            let relative = wrapper_relative(path)?;
            let copied = copy.join(&relative);
            if !copied.is_file() {
                bail!(
                    "test wrapper is missing from the work copy: {}",
                    copied.display()
                );
            }
            Ok(copied)
        })
        .transpose()?;
    let mut runners: Vec<(String, Runner, PathBuf)> = vec![];
    let root_test_exe = |built: &BuildOutput| {
        built
            .test_binaries
            .iter()
            .find(|(name, _, owner, kind)| {
                *name == root_target.name && *owner == package.name && *kind == root_target.kind
            })
            .map(|(_, exe, _, _)| exe.clone())
    };
    let root_exe = (!witness_candidates.is_empty())
        .then(|| root_test_exe(&build))
        .flatten();
    if let Some(nextest) = &options.nextest {
        let mut flags: Vec<_> = test_packages
            .iter()
            .flat_map(|p| ["-p".to_owned(), p.name.clone()])
            .collect();
        flags.extend(options.cargo.flags());
        runners.push((
            "nextest".to_owned(),
            Runner::Nextest {
                copy: copy.clone(),
                target_dir: build_target.clone(),
                profile: nextest.profile.clone(),
                opt_level: options.opt_level,
                flags,
            },
            session.package_dir.clone(),
        ));
    } else {
        for (name, exe, owner, kind) in build.test_binaries {
            if !selection.targets.is_empty() && !selection.targets.contains(&name) {
                continue;
            }
            let owner_package = test_packages
                .iter()
                .find(|p| p.name == owner)
                .expect("binaries are filtered by these packages");
            let cwd = in_copy(
                owner_package
                    .manifest_path
                    .parent()
                    .expect("manifest has a parent"),
            )?;
            if selection.enabled() {
                let all = list_tests(&exe, &cwd, &work, false, true, work_lock.as_deref())?;
                let ignored = list_tests(&exe, &cwd, &work, true, true, work_lock.as_deref())?;
                for test in all {
                    let was_ignored = ignored.contains(&test);
                    if (was_ignored && !selection.include_ignored)
                        || (!selection.include.is_empty() && !selection.include.contains(&test))
                        || selection.exclude.contains(&test)
                    {
                        continue;
                    }
                    runners.push((
                        format!("{owner}::{}::{name}::{test}", kind.join("+")),
                        Runner::Isolated {
                            exe: exe.clone(),
                            test,
                            ignored: was_ignored,
                            wrapper: wrapper.clone(),
                        },
                        cwd.clone(),
                    ));
                }
                continue;
            }
            // Tests of other packages are named after their package.
            let name = if owner == package.name {
                name
            } else {
                format!("{owner}::{name}")
            };
            runners.push((name, Runner::Exe(exe), cwd));
        }
    }
    if selection.enabled() && runners.is_empty() {
        bail!("{build_note}no tests matched the requested selection");
    }
    let selected_tests = selection.enabled().then(|| report::TestSelection {
        build_scope,
        wrapper: selection
            .wrapper
            .as_ref()
            .map(|path| path.display().to_string()),
        tests: runners.iter().map(|(name, _, _)| name.clone()).collect(),
    });
    for owner in doc_packages {
        let doc = Runner::Doc {
            copy: copy.clone(),
            target_dir: build_target.clone(),
            package: owner.name.clone(),
            opt_level: options.opt_level,
            flags: options.cargo.flags(),
        };
        let name = if owner.name == package.name {
            "doc tests".to_owned()
        } else {
            format!("{}::doc tests", owner.name)
        };
        let cwd = in_copy(owner.manifest_path.parent().expect("manifest has a parent"))?;
        runners.push((name, doc, cwd));
    }
    report.analysis.test_selection = selected_tests;
    let identity = if checkpoint.is_some() {
        let mut build_inputs: Vec<_> = build
            .build_scripts
            .iter()
            .map(serde_json::Value::to_string)
            .collect();
        build_inputs.sort();
        // Build script outputs can contain secrets. Only their digest enters
        // the checkpoint, never the Cargo event payload.
        let settings = serde_json::json!({
            "build_scripts": build_inputs,
            "analysis": &report.analysis, "package": &package.name, "jobs": options.jobs,
            "verify": options.verify, "audit": options.audit, "effects": options.effects,
            "all_files": options.all_files, "build_scope": build_scope,
            "runners": runners.iter().map(|(name, runner, cwd)| (name, runner.executable(), cwd)).collect::<Vec<_>>()
        });
        let mut inputs = BTreeSet::new();
        for (root, artifacts) in &build.artifacts {
            inputs.insert(root.clone());
            if let Some(dependencies) = artifacts
                .iter()
                .map(|artifact| artifact_inputs(artifact, root, &copy))
                .find(|deps| !deps.is_empty())
            {
                inputs.extend(dependencies);
            } else {
                // An unfamiliar Cargo output layout cannot hide changed
                // compilation inputs behind a stable artifact name.
                inputs.extend(artifacts.iter().cloned());
            }
        }
        inputs.extend(
            build
                .build_scripts
                .iter()
                .filter_map(|script| script.get("out_dir")?.as_str().map(PathBuf::from)),
        );
        // The snapshot is already included. The paths outside it include
        // dependency sources, generated build inputs, and staged fixtures.
        inputs.retain(|path| !path.starts_with(&copy));
        inputs.extend(
            options
                .output_fixtures
                .iter()
                .map(|fixture| build_target.join(&fixture.to)),
        );
        Some(crate::campaign_identity::compute(
            &copy,
            &settings,
            inputs,
            options.nextest.is_some(),
        )?)
    } else {
        None
    };
    // The marks by which a run says that it gave up are made where nothing else makes
    // files: the working directory is this analysis's alone while it is locked. A
    // directory shared with others (the temporary one, between containers that count
    // their processes alike) could hand one run's mark to another.
    let marks = marks_dir(&work)?;
    let baseline_jobs = if selection.enabled() {
        options.baseline_jobs
    } else {
        1
    };
    if options.progress {
        eprintln!(
            "tmt: baselines: 0/{} completed ({} workers)",
            runners.len(),
            baseline_jobs
        );
    }

    session.binaries = progress::track_baselines(options.progress, runners.len(), |progress| {
        run_pool(runners.len(), baseline_jobs, |slot| {
            let index = slot;
            let (name, runner, cwd) = runners[index].clone();
            let result = (|| {
                let trace_path = work.join(format!("trace-{index}.tsv"));
                let _ = fs::remove_file(&trace_path);
                let mut bin = TestBinary {
                    name,
                    cwd,
                    marks: marks.clone(),
                    git_ceiling: work.clone(),
                    work_lock: work_lock.clone(),
                    tests: BTreeSet::new(),
                    runner,
                    baseline: Duration::ZERO,
                    untraced_baseline: None,
                    timeout_diagnostic: std::sync::Once::new(),
                    traced: Duration::ZERO,
                    timeout: options.timeout_seconds.map(Duration::from_secs),
                    wrapper_grace: options.wrapper_grace,
                    report_slow: options.progress,
                    filtered_timings: Default::default(),
                    trace: Trace::default(),
                    effects_baseline: std::sync::OnceLock::new(),
                };
                session.note(&format!("tracing `{}`", bin.name));
                let started = Instant::now();
                let trace_env = trace_path.to_string_lossy().into_owned();
                let trace = [("TMT_TRACE", trace_env.as_str())];
                run_baseline(
                    &bin,
                    if options.source_only { &[] } else { &trace },
                    Duration::from_secs(3600),
                    &work.join(format!("baseline-{index}.log")),
                    false,
                )?;
                bin.traced = started.elapsed();
                bin.baseline = bin.traced;
                if options.source_only {
                    bin.untraced_baseline = Some(BaselineTiming::Completed(bin.baseline));
                }
                // Recording slows code in loops down a lot, and the time limit of every mutant
                // is a multiple of this: where it matters, time the tests as the mutants run them.
                if !options.source_only && bin.traced > SLOW_TRACE {
                    let started = Instant::now();
                    let passed = run_baseline(
                        &bin,
                        &[],
                        bin.traced,
                        &work.join(format!("baseline-{index}-untraced.log")),
                        true,
                    )?;
                    let elapsed = started.elapsed();
                    if passed {
                        bin.untraced_baseline = Some(BaselineTiming::Completed(elapsed));
                        bin.baseline = elapsed;
                    } else {
                        bin.untraced_baseline = Some(BaselineTiming::StillRunning(bin.traced));
                    }
                }
                // No file means no probe was reached. Anything else unreadable or malformed means
                // the evidence is incomplete, and conclusions drawn from it would be wrong.
                let text = match fs::read_to_string(&trace_path) {
                    Ok(text) => text,
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
                    Err(err) => bail!("cannot read the trace of `{}`: {err}", bin.name),
                };
                bin.trace = Trace::parse(&text);
                if bin.trace.malformed_lines > 0 {
                    bail!(
                        "the trace of `{}` has {} malformed lines; not drawing conclusions from it",
                        bin.name,
                        bin.trace.malformed_lines
                    );
                }
                if let Runner::Exe(exe) = &bin.runner {
                    bin.tests =
                        list_tests(exe, &bin.cwd, &work, false, false, work_lock.as_deref())?;
                }
                Ok(bin)
            })();
            progress.complete(if result.is_ok() {
                progress::Outcome::Survived
            } else {
                progress::Outcome::Failed
            });
            result
        })
    })?;

    // 6. Plan every site, run the mutants, and put the findings together.
    for file in &files {
        for site in &file.sites {
            if disabled.contains(&site.id) {
                let fallback = options.rebuild
                    && !matches!(site.kind, SiteKind::Body { .. })
                    && !instrument::source_mutants(site).is_empty();
                match site.kind {
                    SiteKind::Body { .. } => report.unviable_mutants += 1,
                    _ if fallback => {}
                    _ => report.skipped_sites += 1,
                }
                report.skipped.push(report::SkippedSite::new(
                    report::location(&file.display, site),
                    file.lines.position(&file.original, site.end),
                    if fallback {
                        report::SkipReason::ProbeReplacedBySourceRebuild
                    } else {
                        report::SkipReason::ProbeCompileError
                    },
                    repairs.get(&site.id).cloned(),
                ));
            }
        }
        if !options.rebuild && !options.source_only {
            report.skipped.extend(file.rebuild_sites.iter().map(|site| {
                report::SkippedSite::new(
                    report::location(&file.display, site),
                    file.lines.position(&file.original, site.end),
                    report::SkipReason::RequiresSourceRebuild,
                    None,
                )
            }));
        }
    }
    if let (Some(checkpoint), Some(identity)) = (&checkpoint, &identity) {
        checkpoint.start(
            identity,
            serde_json::json!({
                "package": &package.name, "analysis": &report.analysis,
                "runners": session.binaries.iter().map(|bin| &bin.name).collect::<Vec<_>>(),
                "jobs": options.jobs, "tool_version": env!("CARGO_PKG_VERSION")
            }),
        )?;
    }
    let phase = |name: &str, keys: Vec<String>| {
        checkpoint
            .as_ref()
            .zip(identity.as_deref())
            .map(|(checkpoint, identity)| checkpoint.begin(name, identity, keys))
            .transpose()
    };
    let probe_phase = |name: &str, jobs: &[Job]| -> Result<Option<Phase<'_>>> {
        if checkpoint.is_none() {
            return Ok(None);
        }
        let locations: BTreeMap<_, _> = files
            .iter()
            .flat_map(|file| {
                file.sites
                    .iter()
                    .map(|site| (site.id, (&file.display, site.line, site.column, &site.text)))
            })
            .collect();
        let keys = jobs
            .iter()
            .map(|job| {
                let tests = job
                    .tests
                    .iter()
                    .map(|(bin, selected)| (&session.binaries[*bin].name, selected))
                    .collect::<Vec<_>>();
                serde_json::to_string(&(locations.get(&job.site), job.mutation.encode(), tests))
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        phase(name, keys)
    };
    let scratch = work.clone();
    let mut jobs: Vec<Job> = vec![];
    // Sites whose mutant could not be run as a probe for what it is. With `--rebuild`
    // they are built from the source instead; without, they are counted.
    let mut not_evaluated: HashSet<u32> = HashSet::new();
    let mut work: Vec<SiteWork> = vec![];
    // `(index of the finding, job of its first surviving mutant)`.
    let mut effect_jobs: Vec<(usize, usize)> = vec![];
    for file in &files {
        for site in file.sites.iter().filter(|s| !disabled.contains(&s.id)) {
            // An operand of a comparison that is an arithmetic site of its own, run on
            // primitive numbers in the tests.
            let primitive = |from: usize, to: usize| {
                let Some(mut text) = file.original.get(from..to) else {
                    return false;
                };
                let mut start = from;
                loop {
                    let trimmed = text.trim_start();
                    start += text.len() - trimmed.len();
                    text = trimmed.trim_end();
                    match text
                        .strip_prefix('(')
                        .and_then(|inner| inner.strip_suffix(')'))
                    {
                        Some(inner) => {
                            start += 1;
                            text = inner;
                        }
                        None => break,
                    }
                }
                file.sites.iter().any(|other| {
                    matches!(other.kind, SiteKind::Arith { .. })
                        && other.start == start
                        && other.end == start + text.len()
                        && !disabled.contains(&other.id)
                        && {
                            let seen =
                                session.merged(|t| t.arith.get(&other.id), plan::ArithObs::merge);
                            !seen.opaque
                                && (seen.same || seen.differs || seen.same2 || seen.differs2)
                        }
                })
            };
            let plan = match session.plan(&file.display, site, options.verify, &primitive) {
                // Code that did not change is not said to be unreached here: that takes
                // a run of the whole.
                Planned::Unreached if beyond.contains(&site.id) => continue,
                Planned::Unreached => {
                    // A function is one place, however many sites it is.
                    let at = report::location(&file.display, site);
                    if report.unreached.last() != Some(&at) {
                        report.unreached.push(at);
                    }
                    continue;
                }
                Planned::Nothing => continue,
                Planned::NotEvaluated => {
                    not_evaluated.insert(site.id);
                    continue;
                }
                Planned::Plan(plan) => plan,
            };
            let mut add =
                |mutation: Mutation, tests_of: &dyn Fn(&TestBinary) -> BTreeSet<String>| {
                    jobs.push(session.job(site.id, mutation, tests_of));
                    jobs.len() - 1
                };
            let gaps = plan
                .gaps
                .into_iter()
                .map(
                    |PlannedGap {
                         finding,
                         survivors,
                         constant_expr,
                     }| {
                        // The trace says no test can notice. Confirm by running every test
                        // that reaches the site against the listed survivors.
                        let confirm = if options.verify {
                            survivors
                                .into_iter()
                                .map(|m| add(m, &*plan.reaching))
                                .collect()
                        } else {
                            vec![]
                        };
                        (finding, confirm, constant_expr)
                    },
                )
                .collect();
            let infected = &plan.infected;
            let runs = plan
                .run
                .iter()
                .map(|m| (*m, add(*m, &|bin| infected(bin, *m))))
                .collect();
            work.push(SiteWork {
                file: &file.display,
                site,
                gaps,
                constant: plan.constant,
                cannot_differ: plan.cannot_differ,
                runs,
            });
        }
    }
    session.note(&format!(
        "running {} mutants on {} thread(s)",
        jobs.len(),
        options.jobs.max(1)
    ));
    let probe_checkpoint = probe_phase("probe", &jobs)?;
    let mut results =
        session.run_jobs(&jobs, options.jobs, "mutants", probe_checkpoint.as_ref())?;
    for result in &results {
        report.timeout_rechecks.include(result);
    }

    // 6b. On request, check what the run rests on: that the tests chosen for a mutant
    // are the ones that could fail with it, and that the tests do the same twice.
    if let Some(wanted) = options.audit {
        let survivors: Vec<usize> = (0..jobs.len())
            .filter(|job| results[*job].survived.is_some())
            .collect();
        let chosen: Vec<usize> = spread(survivors.len(), wanted)
            .into_iter()
            .map(|nth| survivors[nth])
            .collect();
        session.note(&format!(
            "audit: running {} of {} surviving mutants against all tests",
            chosen.len(),
            survivors.len()
        ));
        let against_all: Vec<Job> = chosen
            .iter()
            .map(|job| Job {
                site: jobs[*job].site,
                mutation: jobs[*job].mutation,
                tests: (0..session.binaries.len()).map(|bin| (bin, None)).collect(),
            })
            .collect();
        let mut audit = report::Audit {
            sampled: chosen.len(),
            survivors: survivors.len(),
            ..report::Audit::default()
        };
        let audit_checkpoint = probe_phase("audit", &against_all)?;
        for (job, again) in chosen.iter().zip(session.run_jobs(
            &against_all,
            options.jobs,
            "audit mutants",
            audit_checkpoint.as_ref(),
        )?) {
            report.timeout_rechecks.include(&again);
            if again.unable || again.unconfirmed_timeout {
                audit.not_audited += 1;
            } else if audit_all_tests_disagrees(&again) {
                // The verdict of all tests is the one that counts.
                results[*job] = again;
                let site = files
                    .iter()
                    .find_map(|f| Some((f, f.sites.iter().find(|s| s.id == jobs[*job].site)?)));
                if let Some((file, site)) = site {
                    audit
                        .disagreements
                        .push(report::location(&file.display, site));
                }
            }
        }
        session.note("audit: recording the tests once more");
        for (index, bin) in session.binaries.iter().enumerate() {
            let path = scratch.join(format!("trace-{index}-audit.tsv"));
            let _ = fs::remove_file(&path);
            let trace_env = path.to_string_lossy().into_owned();
            let limit = Duration::from_secs(3600);
            let outcome = run_binary(
                bin,
                None,
                &[("TMT_TRACE", &trace_env)],
                limit,
                RunOutput::Quiet,
                1,
            )?;
            let again = Trace::parse(&fs::read_to_string(&path).unwrap_or_default());
            let _ = fs::remove_file(&path);
            let records = bin.trace.differences(&again);
            if outcome != Outcome::Pass || records > 0 {
                audit.trace_differences.push(report::TraceDifference {
                    binary: bin.name.clone(),
                    records,
                });
            }
        }
        report.audit = Some(audit);
    }
    // 6c. In code that did not change, a mutant that passes says something about this
    // change only if the tests as they were found it out. They are built once, with
    // the program as it is, and run against those mutants.
    let mut found_out_before: HashSet<usize> = HashSet::new();
    let asked: Vec<usize> = work
        .iter()
        .filter(|item| beyond.contains(&item.site.id))
        .flat_map(|item| {
            item.gaps
                .iter()
                .flat_map(|(_, confirm, _)| confirm.iter().copied())
                .chain(item.runs.iter().map(|(_, job)| *job))
        })
        .filter(|job| results[*job].survived.is_some())
        .collect();
    if !asked.is_empty() {
        let revision = report::short_revision(options.since.as_deref().unwrap_or_default());
        session.note(&format!(
            "running {} surviving mutants against the tests as they were at {revision}",
            asked.len()
        ));
        let suffix = |file: &SourceFile| {
            if file.path == root_file {
                root_suffix.as_str()
            } else {
                ""
            }
        };
        // The instrumented files with the old lines of their tests.
        let earlier: Vec<(&SourceFile, String)> = tests_changed
            .iter()
            .filter_map(|(path, hunks)| {
                let file = files.iter().find(|file| file.path == *path)?;
                let rendered = file.rendered.as_ref()?;
                let mut places = hunks
                    .iter()
                    .map(|hunk| {
                        let (from, to) = scope::range_of(&file.original, hunk);
                        Some((rendered.copied(from, to)?, hunk.old.as_str()))
                    })
                    .collect::<Option<Vec<_>>>()?;
                places.sort_by_key(|((from, _), _)| std::cmp::Reverse(*from));
                let mut text = rendered.text.clone();
                for ((from, to), old) in places {
                    text.replace_range(from..to, old);
                }
                Some((file, text))
            })
            .collect();
        let attempted = (|| -> Result<Option<Vec<JobResult>>> {
            for (file, text) in &earlier {
                state.write(&file.path, &format!("{text}{}", suffix(file)))?;
            }
            if !cargo_build()?.status.success() {
                session.note(
                    "the tests as they were do not build with the code as it is: what they found out is not known",
                );
                return Ok(None);
            }
            // They have to pass as they are, or a failure says nothing of a mutant.
            let all: Vec<&TestBinary> = session.binaries.iter().collect();
            if run_source_tests(&all)? != Outcome::Pass {
                session.note(
                    "the tests as they were fail on the code as it is: what they found out is not known",
                );
                return Ok(None);
            }
            let against_all: Vec<Job> = asked
                .iter()
                .map(|job| Job {
                    site: jobs[*job].site,
                    mutation: jobs[*job].mutation,
                    tests: (0..session.binaries.len()).map(|bin| (bin, None)).collect(),
                })
                .collect();
            session
                .run_jobs(&against_all, options.jobs, "mutants, earlier tests", None)
                .map(Some)
        })();
        // The files and the binaries as they were, whatever went wrong.
        for (file, _) in &earlier {
            let rendered = file.rendered.as_ref().expect("rendered before building");
            state.write(&file.path, &format!("{}{}", rendered.text, suffix(file)))?;
        }
        if !cargo_build()?.status.success() {
            bail!("the crate does not build once its tests are put back as they are");
        }
        if let Some(again) = attempted? {
            for (job, result) in asked.iter().zip(again) {
                if result.survived.is_none() && !result.unable && !result.unconfirmed_timeout {
                    found_out_before.insert(*job);
                }
            }
        }
    }
    let unable = results
        .iter()
        .filter(|r| r.unable || r.unconfirmed_timeout)
        .count();
    not_evaluated.extend(
        jobs.iter()
            .zip(&results)
            .filter(|(_, result)| result.unable || result.unconfirmed_timeout)
            .map(|(job, _)| job.site),
    );
    if !options.rebuild {
        report.mutants_not_evaluated = not_evaluated.len();
    }
    report.mutants_run = results.len() - unable;
    report.mutants_killed = results.iter().filter(|r| r.survived.is_none()).count() - unable;
    report.mutants_timed_out = results
        .iter()
        .filter(|r| r.timed_out && !r.unconfirmed_timeout)
        .count();

    // Expressions known to be constant in the tests: comparisons with a confirmed
    // one-sided gap or that cannot differ at all, and `&&` / `||` with a single value.
    let confirmed = |w: &SiteWork| {
        w.gaps
            .iter()
            .all(|(_, confirm, _)| confirm.iter().all(|j| results[*j].survived.is_some()))
    };
    let constants: BTreeSet<(&str, usize, usize)> = work
        .iter()
        .filter(|w| w.cannot_differ || (w.constant && confirmed(w)))
        .map(|w| (w.file, w.site.start, w.site.end))
        .collect();
    for item in &work {
        let at = report::location(item.file, item.site);
        for (finding, confirm, constant_expr) in &item.gaps {
            // Already said by the site of the expression itself (or, for a comparison that
            // cannot differ, not worth saying at all).
            let repeats = constant_expr
                .is_some_and(|(start, end)| constants.contains(&(item.file, start, end)));
            if beyond.contains(&item.site.id) {
                // Said only when the tests found it out before their change.
                let confirmed = confirm.iter().all(|j| results[*j].survived.is_some());
                if confirmed && !repeats && confirm.iter().any(|j| found_out_before.contains(j)) {
                    report.weakened.push(finding.at().clone());
                    report.findings.push(finding.clone());
                }
                continue;
            }
            match Prediction::of(confirm.iter().map(|j| &results[*j])) {
                Prediction::Refuted => report.unstable.push(at.clone()),
                Prediction::Confirmed if !repeats => report.findings.push(finding.clone()),
                // Not evaluated (counted above), or said elsewhere.
                Prediction::Confirmed | Prediction::Unknown => {}
            }
        }
        // A body that is one expression, constant in the tests: replacing the body by
        // that constant was said at the expression (or cannot be told apart at all).
        if let SiteKind::Body {
            sole_expr: Some((start, end)),
            ..
        } = item.site.kind
            && constants.contains(&(item.file, start, end))
        {
            continue;
        }
        let mut survivors = vec![];
        let mut passing_tests = BTreeSet::new();
        for (mutation, job) in &item.runs {
            if beyond.contains(&item.site.id) && !found_out_before.contains(job) {
                continue;
            }
            if let Some(tests) = &results[*job].survived {
                survivors.push(report::describe(item.site, *mutation));
                passing_tests.extend(tests.iter().cloned());
            }
        }
        if !survivors.is_empty() {
            // The first survivor is traced below to see what it changes.
            let first = item.runs.iter().find(|(_, job)| {
                results[*job].survived.is_some()
                    && (!beyond.contains(&item.site.id) || found_out_before.contains(job))
            });
            if beyond.contains(&item.site.id) {
                report.weakened.push(at.clone());
            }
            effect_jobs.push((report.findings.len(), first.expect("a survivor").1));
            report.findings.push(Finding::AssertionGap {
                at,
                survivors,
                tests: passing_tests.into_iter().collect(),
                effects: vec![],
                effects_of: None,
                witness: None,
            });
        }
    }

    // 7. What do the surviving mutants change in the tests that let them through?
    if options.effects && !effect_jobs.is_empty() {
        session.note(&format!("tracing {} surviving mutants", effect_jobs.len()));
        let sites: BTreeMap<u32, (&str, &Site)> = files
            .iter()
            .flat_map(|f| f.sites.iter().map(move |s| (s.id, (f.display.as_str(), s))))
            .collect();
        let traced = run_pool(effect_jobs.len(), options.jobs, |index| {
            session.trace_job(
                &jobs[effect_jobs[index].1],
                &scratch,
                options.jobs.clamp(1, effect_jobs.len()),
            )
        })?;
        for ((finding, job), mut changes) in effect_jobs.iter().zip(traced) {
            // What happens inside the mutated code itself is not an effect of it: deleting
            // a statement trivially stops the expressions in it from running.
            if let Some((mutated_file, mutated)) = sites.get(&jobs[*job].site) {
                for found in changes.values_mut() {
                    found.retain(|change| {
                        sites.get(&change.site()).is_none_or(|(file, site)| {
                            file != mutated_file
                                || site.end <= mutated.start
                                || mutated.end <= site.start
                        })
                    });
                }
                changes.retain(|_, found| !found.is_empty());
            }
            if let Finding::AssertionGap {
                effects,
                effects_of,
                ..
            } = &mut report.findings[*finding]
            {
                *effects = describe_effects(&changes, &sites);
                *effects_of = sites
                    .get(&jobs[*job].site)
                    .map(|(_, site)| report::describe(site, jobs[*job].mutation));
            }
        }
    }

    // 7b. What a probe cannot be put in is mutated in the source, built and run: code in
    // a `const fn`, and the sites whose probe did not compile.
    // The last of the four: for an audit, the job that ran the same mutant as a probe.
    let mut built: Vec<(&SourceFile, &Site, instrument::SourceMutant, Option<usize>)> = vec![];
    let build_regular_mutants = options.rebuild || options.source_only;
    let mut index = 0;
    for file in &files {
        // Tried again as the source says them: what did not compile as a probe, and
        // what a probe could not be (a range that ends at the top of its type).
        let retried = file.sites.iter().filter(|site| {
            (disabled.contains(&site.id) && !matches!(site.kind, SiteKind::Body { .. }))
                || not_evaluated.contains(&site.id)
        });
        for site in file.rebuild_sites.iter().chain(retried) {
            let mutants = source_mutants(file, site, true);
            if build_regular_mutants {
                built.extend(mutants.into_iter().filter_map(|mutant| {
                    let selected = in_shard(index);
                    index += 1;
                    selected.then_some((file, site, mutant, None))
                }));
            } else {
                report.mutants_not_built += mutants.len();
            }
        }
    }
    // An audit also builds some of the mutants that were run as probes, from the source
    // as a person would write them, to see whether a probe and the real thing agree.
    if let (Some(wanted), Some(audit)) = (options.audit, report.audit.as_mut()) {
        let mut twins = vec![];
        for file in &files {
            for site in file.sites.iter().filter(|s| !disabled.contains(&s.id)) {
                // Only integers are known to always have an order.
                let integers = session.binaries.iter().all(|bin| {
                    let seen = bin.trace.merged(site.id);
                    !seen.reached() || seen.valued()
                });
                let candidates = jobs_for_site(&jobs, site.id);
                let job_of = |mutation: Mutation| {
                    candidates
                        .clone()
                        .find(|&index| jobs[index].mutation == mutation)
                };
                let mut compared = vec![];
                let mut mutants = instrument::source_mutants(site);
                instrument::keep_line_ends(&file.original, &mut mutants);
                for mutant in mutants {
                    // The first of the mutations that are this change and that was run.
                    let job = mutations_like(site, &mutant, integers)
                        .into_iter()
                        .find_map(job_of);
                    if let Some(job) = job {
                        compared.push(job);
                        twins.push((file, site, mutant, Some(job)));
                    }
                }
                // A negated comparison that no mutant of the source stands for (on
                // floats `<` is not "not `>=`") is compared with the negation written out.
                if let (SiteKind::Cmp { op, .. }, Some(job)) =
                    (&site.kind, job_of(Mutation::Negate))
                    && !compared.contains(&job)
                {
                    let negated = instrument::SourceMutant {
                        start: site.start,
                        end: site.end,
                        replacement: format!("!({})", site.text),
                        what: op.symbol().to_string(),
                        into: "!".to_string(),
                    };
                    twins.push((file, site, negated, Some(job)));
                }
            }
        }
        let chosen = spread(twins.len(), wanted);
        audit.rebuilt = chosen.len();
        let mut twins: Vec<Option<_>> = twins.into_iter().map(Some).collect();
        built.extend(chosen.into_iter().filter_map(|nth| twins[nth].take()));
    }
    let mut by_speed: Vec<&TestBinary> = session.binaries.iter().collect();
    by_speed.sort_by_key(|bin| bin.baseline);
    // A root replacement removes all of its probes. Other files may still use the runtime.
    let root_mutant_suffix = if files.iter().any(|file| {
        file.path != root_file
            && file
                .rendered
                .as_ref()
                .is_some_and(|rendered| rendered.text != file.original)
    }) {
        root_suffix.as_str()
    } else {
        ""
    };
    if !built.is_empty() {
        if options.timeout_seconds.is_some()
            && built
                .iter()
                .any(|(file, _, _, _)| built_sources.contains(file))
        {
            for bin in &by_speed {
                bin.warn_timeout_conflict(None);
            }
        }
        let source_checkpoint = if checkpoint.is_some() {
            let keys = built
                .iter()
                .map(|(file, _, mutation, twin)| {
                    serde_json::to_string(&(
                        &file.display,
                        mutation.start,
                        mutation.end,
                        &mutation.replacement,
                        twin,
                    ))
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            phase("source", keys)?
        } else {
            None
        };
        session.note(&format!(
            "building {} mutants one by one (code a probe cannot be put in, or to compare with the probe)",
            built.len()
        ));
        let instrumented = |file: &SourceFile, text: &str, suffix: &str| {
            if file.path == root_file {
                format!("{text}{suffix}")
            } else {
                text.to_string()
            }
        };
        // All tests are run against each of them; the quick ones first, since one
        // failure is enough.
        let mut skipped_targets = BTreeSet::new();
        // Per file, the code its builds leave out.
        let mut left_out: BTreeMap<&PathBuf, Vec<(usize, usize)>> = BTreeMap::new();
        let mut survivors: Vec<(report::Location, Vec<String>)> = vec![];
        progress::track(
            options.progress,
            "source mutants",
            built.len(),
            |progress| -> Result<()> {
                for (index, (file, site, mutant, twin)) in built.iter().enumerate() {
                    let cached = source_checkpoint
                        .as_ref()
                        .map(|phase| phase.get::<Option<Outcome>>(index))
                        .transpose()?
                        .flatten();
                    let left_out = left_out
                        .entry(&file.path)
                        .or_insert_with(|| built_sources.left_out(file))
                        .iter()
                        .any(|(from, to)| *from <= site.start && site.end <= *to);
                    if left_out || !built_sources.contains(file) {
                        if let Some(phase) = source_checkpoint.as_ref().filter(|_| cached.is_none())
                        {
                            phase.record(index, &Some(Outcome::Unable))?;
                        }
                        if twin.is_none() {
                            report.mutants_not_evaluated += 1;
                            report.mutants_not_compiled += 1;
                            if skipped_targets.insert((&file.path, site.start, site.end)) {
                                report.skipped.push(report::SkippedSite::new(
                                    report::location(&file.display, site),
                                    file.lines.position(&file.original, site.end),
                                    report::SkipReason::SourceNotBuilt,
                                    left_out.then(|| {
                                        "under a `cfg` that the features of this build turn off"
                                            .to_string()
                                    }),
                                ));
                            }
                        } else if let Some(audit) = report.audit.as_mut() {
                            audit.not_compared += 1;
                        }
                        progress.complete(progress::Outcome::Unable);
                        continue;
                    }
                    let outcome = if let Some(cached) = cached {
                        cached
                    } else {
                        let mutated = format!(
                            "{}{}{}",
                            &file.original[..mutant.start],
                            mutant.replacement,
                            &file.original[mutant.end..]
                        );
                        // `None`: it does not build. Otherwise what stopped it, if anything did.
                        let attempt = || -> Result<Option<Outcome>> {
                            state.write(
                                &file.path,
                                &instrumented(file, &mutated, root_mutant_suffix),
                            )?;
                            match source_build(&cargo_build()?) {
                                SourceBuild::Built => run_source_tests(&by_speed).map(Some),
                                SourceBuild::Refused => Ok(None),
                                // Nothing is known of the mutant: not one that does not
                                // compile, and not one to remember as such.
                                SourceBuild::Failed(why) => {
                                    session.note(&format!(
                                        "a build failed without the compiler refusing the code; its mutant is not evaluated: {why}"
                                    ));
                                    Ok(Some(Outcome::Unable))
                                }
                            }
                        };
                        let attempted = attempt();
                        // The file as it was, before another one is changed and whatever went wrong.
                        let rendered = file.rendered.as_ref().expect("rendered before building");
                        state.write(
                            &file.path,
                            &instrumented(file, &rendered.text, &root_suffix),
                        )?;
                        let outcome = attempted?;
                        // A run that could not be judged is tried again when the campaign
                        // is taken up: only what was established is kept.
                        if let Some(phase) = &source_checkpoint
                            && outcome != Some(Outcome::Unable)
                        {
                            phase.record(index, &outcome)?;
                        }
                        outcome
                    };
                    progress.complete(outcome.map_or(progress::Outcome::Unable, Into::into));
                    if let Some(job) = twin {
                        // Only compared: the probe's run is the one that is counted and reported.
                        let as_probe = results[*job].survived.is_some();
                        let Some(audit) = report.audit.as_mut() else {
                            continue;
                        };
                        // What does not build, what could not be evaluated, and an unconfirmed
                        // parallel timeout say nothing about agreement between probe and source.
                        match audit_source_comparison(&results[*job], outcome) {
                            Some(disagrees) => {
                                audit.compared += 1;
                                if disagrees {
                                    let verdict =
                                        |passes: bool| if passes { "passes" } else { "fails" };
                                    audit.source_disagreements.push(report::SourceDisagreement {
                                        at: report::location(&file.display, site),
                                        mutant: report::describe(site, mutation_of(site, mutant)),
                                        as_probe: verdict(as_probe).to_string(),
                                        built: verdict(!as_probe).to_string(),
                                    });
                                }
                            }
                            None => audit.not_compared += 1,
                        }
                        continue;
                    }
                    match outcome {
                        None => report.unviable_mutants += 1,
                        Some(Outcome::Unable) => report.mutants_not_evaluated += 1,
                        Some(outcome) => {
                            report.mutants_run += 1;
                            report.mutants_killed += (outcome != Outcome::Pass) as usize;
                            report.mutants_timed_out += (outcome == Outcome::Timeout) as usize;
                        }
                    }
                    let survived = outcome == Some(Outcome::Pass);
                    if survived {
                        let at = report::location(&file.display, site);
                        // Nothing but the line ends of what was there.
                        let removed = mutant.replacement.trim().is_empty();
                        let text = match (&site.kind, removed) {
                            (SiteKind::Field { .. }, true) => {
                                format!("without the field `{}`", report::one_line(&site.text))
                            }
                            _ => report::describe(site, mutation_of(site, mutant)),
                        };
                        match survivors.last_mut() {
                            Some((last, texts)) if *last == at => texts.push(text),
                            _ => survivors.push((at, vec![text])),
                        }
                    }
                }
                Ok(())
            },
        )?;
        if let Some(phase) = &source_checkpoint {
            phase.finish_if_complete()?;
        }
        report.findings.extend(
            survivors
                .into_iter()
                .map(|(at, survivors)| Finding::Survivor {
                    at,
                    survivors,
                    witness: None,
                    change_since: None,
                    advice: None,
                }),
        );
    }

    // Each change since the revision, undone: do the tests tell it from what was there?
    if let Some(revision) = options
        .since
        .as_deref()
        .filter(|_| !options.list && options.shard.is_none())
    {
        let hunks = scope::hunks(&since_diff(&workspace, revision)?);
        let not_built_changes = std::cell::Cell::new(0usize);
        let changed: Vec<(&SourceFile, Vec<reverts::Change>)> = files
            .iter()
            .filter(|file| built_sources.contains(file) || !options.source_only && !options.rebuild)
            .filter_map(|file| {
                let relative = file
                    .path
                    .strip_prefix(&copy)
                    .ok()?
                    .to_string_lossy()
                    .replace('\\', "/");
                let of_file: Vec<_> = hunks
                    .iter()
                    .filter(|hunk| hunk.file == relative)
                    .cloned()
                    .collect();
                let mut changes = reverts::changes(&file.original, &of_file);
                // Code this build leaves out (a feature that is off) is the same
                // program with its change undone: nothing to try.
                if options.rebuild || options.source_only {
                    for change in &mut changes {
                        // The same program undone, in every build: a `cfg` that
                        // changed may turn code on or off.
                        change.outside = change.undone.as_deref().is_some_and(|undone| {
                            built_sources.same_program(file, &file.original, undone)
                        });
                    }
                    not_built_changes.set(
                        not_built_changes.get()
                            + changes.iter().filter(|change| change.outside).count(),
                    );
                }
                (!changes.is_empty()).then_some((file, changes))
            })
            .collect();
        let not_built_changes = not_built_changes.get();
        report.mutants_not_evaluated += not_built_changes;
        report.mutants_not_compiled += not_built_changes;
        // Those are not tried alone, and still go back with the others.
        let total: usize = changed
            .iter()
            .map(|(_, changes)| changes.iter().filter(|change| !change.outside).count())
            .sum();
        // A change of the program and its tests in lines that cannot be told apart
        // would be undone with the tests: nothing is said of it.
        let mixed: usize = files
            .iter()
            .map(|file| {
                let relative = file
                    .path
                    .strip_prefix(&copy)
                    .unwrap_or(&file.path)
                    .to_string_lossy()
                    .replace('\\', "/");
                let of_file: Vec<_> = hunks
                    .iter()
                    .filter(|hunk| hunk.file == relative)
                    .cloned()
                    .collect();
                reverts::mixed(&file.original, &of_file)
            })
            .sum();
        if mixed > 0 && (options.rebuild || options.source_only) {
            report.mutants_not_evaluated += mixed;
            session.note(&format!(
                "{mixed} changes are of the program and its tests in the same lines: they are not tried undone"
            ));
        }
        if !(options.rebuild || options.source_only) {
            report.mutants_not_built += total;
        } else if total > 0 || changed.iter().any(|(_, changes)| changes.len() > 1) {
            // Also with nothing to try alone: several changes of code that is left out
            // may be a change of the build together.
            session.note(&format!(
                "building {total} changes undone, one by one (the code as it was at {})",
                report::short_revision(revision)
            ));
            let write = |file: &SourceFile, text: &str, suffix: &str| -> Result<()> {
                let suffix = if file.path == root_file { suffix } else { "" };
                state.write(&file.path, &format!("{text}{suffix}"))
            };
            // `None`: it does not build.
            let undone = |edits: &[(&SourceFile, &str)]| -> Result<Option<Outcome>> {
                let attempted = (|| -> Result<Option<Outcome>> {
                    for (file, text) in edits {
                        write(file, text, root_mutant_suffix)?;
                    }
                    Ok(match source_build(&cargo_build()?) {
                        SourceBuild::Built => Some(run_source_tests(&by_speed)?),
                        SourceBuild::Refused => None,
                        SourceBuild::Failed(why) => {
                            session.note(&format!(
                                "a build failed without the compiler refusing the code; the change undone is not evaluated: {why}"
                            ));
                            Some(Outcome::Unable)
                        }
                    })
                })();
                // The files as they were, whatever went wrong.
                for (file, _) in edits {
                    let rendered = file.rendered.as_ref().expect("rendered before building");
                    write(file, &rendered.text, &root_suffix)?;
                }
                attempted
            };
            let mut judged = |outcome: Option<Outcome>, at: report::Location, what: String| {
                match outcome {
                    None => report.unviable_mutants += 1,
                    Some(Outcome::Unable) => report.mutants_not_evaluated += 1,
                    Some(outcome) => {
                        report.mutants_run += 1;
                        report.mutants_killed += (outcome != Outcome::Pass) as usize;
                        report.mutants_timed_out += (outcome == Outcome::Timeout) as usize;
                    }
                }
                if outcome == Some(Outcome::Pass) {
                    report.findings.push(Finding::Survivor {
                        at,
                        survivors: vec![what],
                        witness: None,
                        advice: None,
                        change_since: Some(revision.to_string()),
                    });
                }
            };
            let place = |file: &SourceFile, change: &reverts::Change| report::Location {
                file: file.display.clone(),
                line: change.line,
                column: 1,
                code: change.code.clone(),
                ..Default::default()
            };
            // A change that does not build alone may be one part of several that belong
            // together: then all of the file's are undone at once, and at last all of all.
            let mut stuck_files = 0;
            for (file, changes) in &changed {
                let mut stuck = false;
                let alone = changes.iter().filter(|change| !change.outside).count();
                for change in changes.iter().filter(|change| !change.outside) {
                    let outcome = match &change.undone {
                        Some(text) => undone(&[(file, text.as_str())])?,
                        None => None,
                    };
                    stuck |= outcome.is_none();
                    judged(outcome, place(file, change), change.describe());
                }
                // All together: where one does not build alone, and where what is left
                // out now is built once the others are undone (a `cfg` that changed
                // along with the code under it), which shows as another program with
                // those changes undone too than without them.
                let whole = reverts::all_undone(&file.original, changes);
                let same = |a: &str, b: &str| built_sources.same_program(file, a, b);
                let tried: Vec<reverts::Change> = changes
                    .iter()
                    .filter(|change| !change.outside)
                    .cloned()
                    .collect();
                let left_out_matter = alone < changes.len()
                    && match (&whole, reverts::all_undone(&file.original, &tried)) {
                        (Some(all), Some(without)) => !same(all, &without),
                        _ => true,
                    };
                // And never a program that is the one that is there.
                let another = whole
                    .as_deref()
                    .is_none_or(|all| !same(all, &file.original));
                if changes.len() > 1 && another && (stuck || left_out_matter) {
                    let outcome = match &whole {
                        Some(text) => undone(&[(file, text.as_str())])?,
                        None => None,
                    };
                    stuck = outcome.is_none();
                    judged(
                        outcome,
                        place(file, &changes[0]),
                        format!("with all {} changes to this file undone", changes.len()),
                    );
                }
                stuck_files += usize::from(stuck);
            }
            // All files together: where a file's changes do not build even together,
            // and where code that is left out changed, which may count only with the
            // changes of another file undone.
            let with_left_out = changed
                .iter()
                .any(|(_, changes)| changes.iter().any(|change| change.outside));
            if (stuck_files > 0 || with_left_out) && changed.len() > 1 {
                let whole: Option<Vec<String>> = changed
                    .iter()
                    .map(|(file, changes)| reverts::all_undone(&file.original, changes))
                    .collect();
                let same =
                    |file: &SourceFile, a: &str, b: &str| built_sources.same_program(file, a, b);
                // Never the program that is there: in some file it has to be another.
                let another = whole.as_ref().is_none_or(|texts| {
                    changed
                        .iter()
                        .zip(texts)
                        .any(|((file, _), text)| !same(file, text, &file.original))
                });
                // And what is left out has to count: another program with it undone
                // too than with the other changes alone.
                let left_out_matter = whole.as_ref().is_none_or(|texts| {
                    changed.iter().zip(texts).any(|((file, changes), text)| {
                        let tried: Vec<reverts::Change> = changes
                            .iter()
                            .filter(|change| !change.outside)
                            .cloned()
                            .collect();
                        reverts::all_undone(&file.original, &tried)
                            .is_none_or(|without| !same(file, text, &without))
                    })
                });
                if another && (stuck_files > 0 || (with_left_out && left_out_matter)) {
                    let outcome = match &whole {
                        Some(texts) => {
                            let edits: Vec<(&SourceFile, &str)> = changed
                                .iter()
                                .zip(texts)
                                .map(|((file, _), text)| (*file, text.as_str()))
                                .collect();
                            undone(&edits)?
                        }
                        None => None,
                    };
                    let (file, changes) = &changed[0];
                    judged(
                        outcome,
                        place(file, &changes[0]),
                        format!("with every change undone, in all {} files", changed.len()),
                    );
                }
            }
        }
    }

    // File by file: the test that shows a difference is put at the end of the file the
    // candidate is of, where the names of the file are in scope.
    let mut witness_files: Vec<&PathBuf> = vec![];
    for candidate in &witness_candidates {
        if !witness_files.contains(&&candidate.file) {
            witness_files.push(&candidate.file);
        }
    }
    for witness_file in witness_files {
        let in_root = *witness_file == root_file;
        let witness_candidates: Vec<&witness::Candidate> = witness_candidates
            .iter()
            .filter(|candidate| candidate.file == *witness_file)
            .collect();
        let file = files
            .iter()
            .find(|file| file.path == *witness_file)
            .expect("witness candidates are of files that are analysed");
        let rendered = file.rendered.as_ref().expect("rendered before building");
        let left_out_here = built_sources.left_out(file);
        let restore = if in_root {
            format!("{}{root_suffix}", rendered.text)
        } else {
            rendered.text.clone()
        };
        let library_runners: Vec<_> = session
            .binaries
            .iter()
            .filter(|bin| {
                root_exe
                    .as_deref()
                    .is_some_and(|exe| bin.runner.executable() == Some(exe))
            })
            .collect();
        let ignored = library_runners
            .iter()
            .find_map(|bin| match &bin.runner {
                Runner::Exe(exe) => Some(list_tests(
                    exe,
                    &bin.cwd,
                    &bin.git_ceiling,
                    true,
                    false,
                    bin.work_lock.as_deref(),
                )),
                _ => None,
            })
            .transpose()?
            .unwrap_or_default();
        let runner_for = |scenario: &witness::Scenario| {
            library_runners.iter().copied().find(|bin| {
                scenario
                    .source_test
                    .as_ref()
                    .is_none_or(|required| match &bin.runner {
                        // In a file that is a module, the test's name comes after the
                        // module's.
                        Runner::Exe(_) if !in_root => {
                            let inner = format!("::{required}");
                            bin.tests
                                .iter()
                                .any(|test| test.ends_with(&inner) && !ignored.contains(test))
                        }
                        Runner::Exe(_) => {
                            bin.tests.contains(required) && !ignored.contains(required)
                        }
                        Runner::Isolated { test, .. } if !in_root => {
                            test.ends_with(&format!("::{required}"))
                        }
                        Runner::Isolated { test, .. } => test == required,
                        Runner::Doc { .. } | Runner::Nextest { .. } => false,
                    })
            })
        };
        let write_edits = |edits: &[witness::Edit]| -> Result<bool> {
            let Some(source) = witness::render_edits(&file.original, edits, None) else {
                return Ok(false);
            };
            let suffix = if in_root { root_mutant_suffix } else { "" };
            state.write(&file.path, &format!("{source}{suffix}"))?;
            Ok(true)
        };
        let suite = |edits: &[witness::Edit]| -> Result<Option<Outcome>> {
            if !write_edits(edits)? || !cargo_build()?.status.success() {
                return Ok(None);
            }
            run_source_tests(&by_speed).map(Some)
        };
        let compile_observer = |edits: &[witness::Edit]| -> Result<Option<PathBuf>> {
            if !write_edits(edits)? {
                return Ok(None);
            }
            let build = build_test_binaries(true)?;
            if !build.status.success() {
                return Ok(None);
            }
            let artifacts = parse_build_output(
                &String::from_utf8_lossy(&build.stdout),
                &[package.name.as_str()],
            );
            Ok(root_test_exe(&artifacts))
        };
        let observe = |scenario: &witness::Scenario,
                       baseline: &TestBinary,
                       edits: &[witness::Edit],
                       expected: Option<&str>|
         -> Result<Option<witness::Observation>> {
            let test = scenario.test_edit(&file.original, witness::TEST_NAME, expected);
            let edits: Vec<_> = edits.iter().cloned().chain([test]).collect();
            let Some(exe) = compile_observer(&edits)? else {
                return Ok(None);
            };
            observe_witness(
                baseline,
                exe,
                wrapper.as_deref(),
                scenario,
                witness::TEST_NAME,
                expected.is_some(),
                in_root,
            )
        };
        let witness_checkpoint = if checkpoint.is_some() {
            let keys = witness_candidates
                .iter()
                .map(|candidate| {
                    let edits = candidate
                        .edits
                        .iter()
                        .map(|edit| (&edit.span, &edit.replacement))
                        .collect::<Vec<_>>();
                    serde_json::to_string(&(&candidate.description, edits))
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let name = if in_root {
                "witness".to_string()
            } else {
                format!("witness:{}", file.display)
            };
            phase(&name, keys)?
        } else {
            None
        };
        let mut survivors = vec![];
        let mut claimed = BTreeSet::new();
        progress::track(
            options.progress,
            "source witness mutants",
            witness_candidates.len(),
            |progress| -> Result<()> {
                for (index, candidate) in witness_candidates.iter().copied().enumerate() {
                    let cached = witness_checkpoint
                        .as_ref()
                        .map(|phase| phase.get::<WitnessTrial>(index))
                        .transpose()?
                        .flatten();
                    let at = witness_location(file, candidate);
                    // Code this build leaves out has no mutant to run.
                    if !built_sources.contains(file)
                        || candidate.edits.iter().any(|edit| {
                            left_out_here
                                .iter()
                                .any(|(from, to)| *from <= edit.span.start && edit.span.end <= *to)
                        })
                    {
                        if let Some(phase) =
                            witness_checkpoint.as_ref().filter(|_| cached.is_none())
                        {
                            phase.record(index, &WitnessTrial::Inapplicable)?;
                        }
                        if !candidate.existing {
                            report.mutants_not_evaluated += 1;
                            report.mutants_not_compiled += 1;
                            let span = &candidate.edits[0].span;
                            report.skipped.push(report::SkippedSite::new(
                                at,
                                file.lines.position(&file.original, span.end),
                                report::SkipReason::SourceNotBuilt,
                                Some(
                                    "under a `cfg` that the features of this build turn off"
                                        .to_string(),
                                ),
                            ));
                        }
                        progress.complete(progress::Outcome::Unable);
                        continue;
                    }
                    let existing = candidate
                        .existing
                        .then(|| {
                            report.findings.iter().position(|finding| {
                                let location = finding.at();
                                location.file == at.file
                                    && location.line == at.line
                                    && location.column == at.column
                                    && finding.survivors().contains(&candidate.description)
                                    && (candidate.finding_at.is_none()
                                        || !matches!(finding, Finding::InputGap { .. }))
                            })
                        })
                        .flatten();
                    // One stronger assertion is enough for a finding, but wait until a
                    // witness actually verifies before claiming it so later surviving
                    // mutants on the same finding still get a turn if an earlier one fails.
                    let existing = existing
                        .filter(|index| candidate.finding_at.is_none() || !claimed.contains(index));
                    let scenarios: Vec<_> = candidate
                        .scenarios
                        .iter()
                        .filter_map(|scenario| {
                            runner_for(scenario).map(|runner| (scenario, runner))
                        })
                        .take(4)
                        .collect();
                    let paired = candidate.edits.len() > 1;
                    if (candidate.existing && (existing.is_none() || scenarios.is_empty()))
                        || (paired && scenarios.is_empty())
                    {
                        if let Some(phase) =
                            witness_checkpoint.as_ref().filter(|_| cached.is_none())
                        {
                            phase.record(index, &WitnessTrial::Inapplicable)?;
                        }
                        progress.complete(progress::Outcome::Unable);
                        continue;
                    }
                    // A compound survivor is interesting only if each component fails on
                    // its own. Test the source edits without any generated witness tests.
                    let trial = if let Some(cached) = cached {
                        cached
                    } else {
                        let attempted = (|| -> Result<WitnessTrial> {
                            if paired {
                                for edit in &candidate.edits {
                                    if suite(std::slice::from_ref(edit))? != Some(Outcome::Fail) {
                                        return Ok(WitnessTrial::Inapplicable);
                                    }
                                }
                            }
                            Ok(suite(&candidate.edits)?
                                .map_or(WitnessTrial::Unviable, WitnessTrial::Evaluated))
                        })();
                        state.write(&file.path, &restore)?;
                        let trial = attempted?;
                        if let Some(phase) = &witness_checkpoint {
                            phase.record(index, &trial)?;
                        }
                        trial
                    };
                    let outcome = match trial {
                        WitnessTrial::Inapplicable => {
                            progress.complete(progress::Outcome::Unable);
                            continue;
                        }
                        WitnessTrial::Unviable => {
                            progress.complete(progress::Outcome::Unable);
                            report.unviable_mutants += usize::from(!candidate.existing);
                            continue;
                        }
                        WitnessTrial::Evaluated(outcome) => outcome,
                    };
                    progress.complete(outcome.into());
                    if !candidate.existing {
                        report.mutants_run += 1;
                        report.mutants_killed += usize::from(outcome != Outcome::Pass);
                        report.mutants_timed_out += usize::from(outcome == Outcome::Timeout);
                    }
                    if outcome == Outcome::Pass {
                        survivors.push(WitnessSurvivor {
                            candidate,
                            at,
                            existing,
                            scenarios,
                        });
                    }
                }
                Ok(())
            },
        )?;

        if let Some(phase) = &witness_checkpoint {
            phase.finish()?;
        }
        let originals = shared_witnesses(
            &file.original,
            &survivors,
            wrapper.as_deref(),
            in_root,
            compile_observer,
            note,
        );
        if !survivors.is_empty() {
            state.write(&file.path, &restore)?;
        }
        let originals = originals?;

        progress::track(
            options.progress,
            "distinguishing tests",
            survivors.len(),
            |progress| -> Result<()> {
                for (survivor, original) in survivors.into_iter().zip(originals) {
                    if survivor.existing.is_some_and(|index| {
                        survivor.candidate.finding_at.is_some() && claimed.contains(&index)
                    }) {
                        progress.complete(progress::Outcome::Unable);
                        continue;
                    }
                    let mut evidence = None;
                    let mut original = original;
                    for (scenario, baseline) in survivor.scenarios {
                        let shared = original.take();
                        let distinction = (|| -> Result<_> {
                            if let Some(shared) = shared {
                                // Keep the harness and test name identical on both sides.
                                let edits: Vec<_> = survivor
                                    .candidate
                                    .edits
                                    .iter()
                                    .chain(shared.tests.iter())
                                    .cloned()
                                    .collect();
                                if let Some(exe) = compile_observer(&edits)?
                                    && let Some(mutant) = observe_witness(
                                        baseline,
                                        exe,
                                        wrapper.as_deref(),
                                        scenario,
                                        &shared.name,
                                        true,
                                        in_root,
                                    )
                                    .ok()
                                    .flatten()
                                    && mutant.equals_expected == Some(false)
                                    // Re-check the preserved original binary after the mutant
                                    // run to reject monotonic external-state drift without rebuilding.
                                    && observe_witness(
                                        baseline,
                                        shared.original_exe.path.clone(),
                                        wrapper.as_deref(),
                                        scenario,
                                        &shared.name,
                                        true,
                                        in_root,
                                    )
                                    .ok()
                                    .flatten()
                                    .is_some_and(|rechecked| {
                                        rechecked.equals_expected == Some(true)
                                            && rechecked.value == shared.value
                                    })
                                    && let Some(verified) =
                                        scenario.verified(shared.value, mutant.value)
                                {
                                    return Ok(Some(verified));
                                }
                                // A combined build can fail or the code can depend on
                                // the harness. Retry the original input in isolation.
                            }
                            let Some(original) = observe(scenario, baseline, &[], None)? else {
                                return Ok(None);
                            };
                            let value = original.value;
                            if !witness::is_expected_literal(&value) {
                                return Ok(None);
                            }
                            let Some(mutant) = observe(
                                scenario,
                                baseline,
                                &survivor.candidate.edits,
                                Some(&value),
                            )?
                            else {
                                return Ok(None);
                            };
                            if mutant.equals_expected != Some(false) || value == mutant.value {
                                return Ok(None);
                            }
                            let Some(checked) = observe(scenario, baseline, &[], Some(&value))?
                            else {
                                return Ok(None);
                            };
                            if checked.equals_expected != Some(true) || checked.value != value {
                                return Ok(None);
                            }
                            Ok(scenario.verified(value, mutant.value))
                        })();
                        match distinction {
                            Ok(Some(mut verified)) => {
                                let edits = survivor.candidate.edits.len();
                                verified.individually_killed = (edits > 1).then_some(edits);
                                evidence = Some(verified);
                                break;
                            }
                            Ok(None) => {}
                            Err(err) => note(&format!(
                                "could not check a distinguishing test for {}: {err:#}",
                                scenario.expression,
                            )),
                        }
                    }
                    state.write(&file.path, &restore)?;
                    progress.complete(Outcome::Pass.into());
                    if let Some(index) = survivor.existing {
                        if let Some(evidence) = evidence {
                            report.findings[index].set_witness(evidence);
                            if survivor.candidate.finding_at.is_some() {
                                claimed.insert(index);
                            }
                        }
                    } else {
                        report.findings.push(Finding::Survivor {
                            at: survivor.at,
                            survivors: vec![survivor.candidate.description.clone()],
                            advice: survivor
                                .candidate
                                .advice
                                .clone()
                                .filter(|_| evidence.is_none()),
                            witness: evidence,
                            change_since: None,
                        });
                    }
                }
                Ok(())
            },
        )?;
    }

    report.record_evaluation();
    report.sort_findings();
    let places: Vec<FilePlaces> = files
        .iter()
        .map(|file| FilePlaces {
            display: &file.display,
            source: &file.original,
            lines: &file.lines,
            places: &file.places,
        })
        .collect();
    name_findings(&mut report.findings, &places);

    // 8. Findings the user has already dealt with.
    let files_with_findings: BTreeSet<&str> = report
        .findings
        .iter()
        .map(|finding| finding.at().file.as_str())
        .collect();
    let ignored: BTreeMap<&str, BTreeMap<usize, String>> = files
        .iter()
        .filter(|file| files_with_findings.contains(file.display.as_str()))
        .map(|f| {
            (
                f.display.as_str(),
                crate::baseline::ignored_lines(&f.original),
            )
        })
        .collect();
    let known = match &options.baseline {
        Some(path) => Some(crate::baseline::load(path)?),
        None => None,
    };
    let before = report.findings.len();
    report.findings.retain(|finding| {
        let at = finding.at();
        !ignored
            .get(at.file.as_str())
            .is_some_and(|lines| lines.contains_key(&at.line))
    });
    if let Some(path) = &options.write_baseline {
        crate::baseline::write(path, &report.findings)?;
    }
    report
        .findings
        .retain(|finding| !known.as_ref().is_some_and(|known| known.knows(finding)));
    report.suppressed = before - report.findings.len();
    if let Some(checkpoint) = &checkpoint {
        checkpoint.finish_if_complete()?;
    }
    Ok(report)
}

fn workspace_report_prefix(relative: &Path) -> String {
    let parts: Vec<_> = relative
        .components()
        .filter_map(|part| match part {
            std::path::Component::Normal(name) => Some(name.to_string_lossy()),
            _ => None,
        })
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!("{}/", parts.join("/"))
    }
}

/// Where a directory is in its git repository: `crates/core/`, or nothing at the top
/// and outside of any. The files of a report are named from the package; this in front
/// of them names them from the repository, as a code-scanning service wants them.
fn repository_prefix(dir: &Path) -> String {
    let shown = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--show-prefix"])
        .stderr(Stdio::null())
        .output();
    match shown {
        Ok(output) if output.status.success() => {
            String::from_utf8_lossy(&output.stdout).trim().to_string()
        }
        _ => String::new(),
    }
}

/// `wanted` of the numbers below `count`, spread evenly and the same every time; all of
/// them where there are no more.
fn spread(count: usize, wanted: usize) -> Vec<usize> {
    let wanted = wanted.min(count);
    (0..wanted).map(|nth| nth * count / wanted).collect()
}

/// What naming the findings of a file needs to know of it.
struct FilePlaces<'a> {
    display: &'a str,
    source: &'a str,
    lines: &'a scope::LineIndex,
    /// Every site of the file as (line, column, code), whatever part of it was analysed.
    places: &'a [(usize, usize, String)],
}

/// Gives every finding the item it is in and its name (`report::finding_id`). Which of
/// several places with the same code in one item a finding is at is counted over the
/// sites of the file, not over the findings: a finding keeps its number when the one
/// before it is closed.
fn name_findings(findings: &mut [Finding], files: &[FilePlaces]) {
    let mut by_file: BTreeMap<String, Vec<&mut Finding>> = BTreeMap::new();
    for finding in findings {
        by_file
            .entry(finding.at().file.clone())
            .or_default()
            .push(finding);
    }
    for file in files {
        let Some(findings) = by_file.remove(file.display) else {
            continue;
        };
        let items = instrument::named_items(file.source);
        let index = instrument::ItemIndex::new(&items);
        let item_at = |line, column| {
            file.lines
                .offset_of(file.source, line, column)
                .map_or("", |offset| index.at(offset))
        };
        let mut positions: BTreeMap<(&str, &str), Vec<(usize, usize)>> = BTreeMap::new();
        for (line, column, code) in file.places {
            positions
                .entry((item_at(*line, *column), code))
                .or_default()
                .push((*line, *column));
        }
        for places in positions.values_mut() {
            places.sort_unstable();
            places.dedup();
        }
        for finding in findings {
            let kind = finding.kind();
            let at = finding.at_mut();
            let item = item_at(at.line, at.column);
            let nth = positions.get(&(item, &at.code)).map_or(0, |places| {
                places.partition_point(|&place| place < (at.line, at.column))
            });
            at.item = item.to_string();
            at.id = report::finding_id(kind, at, nth);
        }
    }
}

/// The mutation that a change of the source text is, to describe it in the same words.
fn mutation_of(site: &Site, mutant: &instrument::SourceMutant) -> Mutation {
    match &site.kind {
        SiteKind::Cmp { .. } => plan::CmpOp::ALL
            .into_iter()
            .find(|op| op.symbol() == mutant.into)
            .map_or(Mutation::Negate, Mutation::Op),
        SiteKind::Arith { op, .. } => {
            if mutant.into.trim_end_matches('=') == op.swapped().symbol() {
                Mutation::ArithSwap
            } else {
                Mutation::ArithSwap2
            }
        }
        SiteKind::Guard { .. } => Mutation::Const(mutant.into == "true"),
        SiteKind::Logic { .. } => Mutation::LogicSwap,
        SiteKind::Not { .. } => Mutation::DropNot,
        SiteKind::Range { .. } => Mutation::RangeEnd,
        SiteKind::SliceEnd { .. } => Mutation::OtherEnd,
        // The other kinds have one mutant of the source: a statement, an arm, a field or
        // a minus that is not there, a body that is another.
        _ => Mutation::Delete,
    }
}

/// The mutations a probe may have run that are the same change as a mutant of the source,
/// its own name first. A comparison is planned in other words than it is written in:
/// `a != b` for `a == b` is run as the negation, and on integers `a <= b` for `a < b` as
/// the bound moved by one.
///
/// `integers`: the operands are integers, which always have an order. For anything
/// else (a float can be NaN) the negation of `>=` is true where `<` is false, and is
/// not the same change.
fn mutations_like(site: &Site, mutant: &instrument::SourceMutant, integers: bool) -> Vec<Mutation> {
    let mut all = vec![mutation_of(site, mutant)];
    if let (SiteKind::Cmp { op, .. }, Mutation::Op(into)) = (&site.kind, all[0]) {
        let an_order_is_needed = matches!(op, plan::CmpOp::Le | plan::CmpOp::Ge);
        if into == op.negated() && (integers || !an_order_is_needed) {
            all.push(Mutation::Negate);
        }
        match (op, into) {
            (plan::CmpOp::Lt, plan::CmpOp::Le) | (plan::CmpOp::Ge, plan::CmpOp::Gt) => {
                all.push(Mutation::Shift(1))
            }
            (plan::CmpOp::Gt, plan::CmpOp::Ge) | (plan::CmpOp::Le, plan::CmpOp::Lt) => {
                all.push(Mutation::Shift(-1))
            }
            _ => {}
        }
    }
    all
}

/// The changes a surviving mutant causes, as up to three sentences. Changes that many
/// tests share are mentioned once.
fn describe_effects(
    changes: &BTreeMap<String, BTreeSet<Change>>,
    sites: &BTreeMap<u32, (&str, &Site)>,
) -> Vec<String> {
    let mut by_change: BTreeMap<&Change, Vec<&str>> = BTreeMap::new();
    for (test, found) in changes {
        for change in found {
            by_change.entry(change).or_default().push(test);
        }
    }
    if by_change.is_empty() {
        return vec![
            "with the mutant, these tests still reach the same code with the same outcomes, as far as that is recorded: they do not use its result, it is the same for their inputs, or what they assert holds for both".to_string(),
        ];
    }
    let mut out = vec![];
    for (change, tests) in by_change.iter().take(3) {
        let Some((file, site)) = sites.get(&change.site()) else {
            continue;
        };
        let code = report::location(file, site).code;
        let at = format!("{file}:{}", site.line);
        let who = match tests[..] {
            ["?"] => "a test".to_string(),
            [one] => format!("`{one}`"),
            [first, ..] => format!("`{first}` (and {} more)", tests.len() - 1),
            [] => continue,
        };
        out.push(match change {
            Change::Returns { value, .. } => {
                format!("with the mutant, {who} gets `{value}` from `{code}` ({at})")
            }
            Change::NowRuns { .. } => format!("with the mutant, {who} now runs `{code}` ({at})"),
            Change::NoLongerRuns { .. } => {
                format!("with the mutant, {who} no longer runs `{code}` ({at})")
            }
            Change::NewOutcome { .. } => {
                format!("with the mutant, {who} takes `{code}` ({at}) a way it did not before")
            }
        });
    }
    if by_change.len() > 3 {
        out.push(format!("(and {} more such changes)", by_change.len() - 3));
    }
    out
}

#[cfg(test)]
mod tests;
