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
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::instrument::{self, LiteralSide, RUNTIME_MOD, Rendered, Site, SiteKind};
use crate::plan::{self, Mutation, Obs};
use crate::propagation::{self, Change};
use crate::report::{self, Finding, Report};
use crate::scope::{self, Scope};
use crate::trace::Trace;
use crate::{git_index, progress, witness};

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

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    workspace_root: PathBuf,
    target_directory: PathBuf,
}

#[derive(Deserialize)]
struct Package {
    name: String,
    #[serde(default)]
    dependencies: Vec<Dependency>,
    features: BTreeMap<String, Vec<String>>,
    manifest_path: PathBuf,
    targets: Vec<Target>,
}

impl Package {
    /// Only count features we can establish without resolving the dependency graph.
    fn enabled_local_features(&self, workspace: &Path, cargo: &CargoConfig) -> BTreeSet<&str> {
        if cargo.all_features {
            return self.features.keys().map(String::as_str).collect();
        }
        let mut pending = Vec::new();
        if !cargo.no_default_features {
            pending.push("default");
        }
        // With resolver 1, unqualified CLI features only apply to the current package.
        let at_workspace_root = self.manifest_path.parent() == Some(workspace);
        for feature in cargo
            .features
            .iter()
            .flat_map(|features| features.split(|c: char| c == ',' || c.is_whitespace()))
        {
            match feature.split_once('/') {
                Some((package, name)) if package == self.name => pending.push(name),
                None if at_workspace_root => pending.push(feature),
                _ => {}
            }
        }
        let mut enabled = BTreeSet::new();
        while let Some(feature) = pending.pop() {
            if let Some((name, activates)) = self.features.get_key_value(feature)
                && enabled.insert(name.as_str())
            {
                pending.extend(activates.iter().map(String::as_str));
            }
        }
        enabled
    }
}

#[derive(Deserialize)]
struct Dependency {
    name: String,
}

#[derive(Deserialize)]
struct Target {
    name: String,
    kind: Vec<String>,
    src_path: PathBuf,
    test: bool,
    #[serde(default)]
    doctest: bool,
    #[serde(default, rename = "required-features")]
    required_features: Vec<String>,
}

impl Target {
    fn is_production(&self) -> bool {
        matches!(self.test_build_arg(), Some(("--lib" | "--bin", _)))
    }

    /// The Cargo selector and its optional name. `--lib` selects every package's library.
    fn test_build_arg(&self) -> Option<(&'static str, Option<&str>)> {
        match self.kind.first()?.as_str() {
            "lib" | "rlib" | "cdylib" | "dylib" | "staticlib" | "proc-macro" => {
                Some(("--lib", None))
            }
            "bin" => Some(("--bin", Some(&self.name))),
            "example" => Some(("--example", Some(&self.name))),
            "test" => Some(("--test", Some(&self.name))),
            "bench" => Some(("--bench", Some(&self.name))),
            _ => None,
        }
    }
}

/// Select only test targets that Cargo's flags can express without broadening the selection.
fn test_build_flags(
    packages: &[&Package],
    names: &[String],
    workspace: &Path,
    cargo: &CargoConfig,
) -> Option<Vec<String>> {
    if names.is_empty() {
        return None;
    }
    let selected = |target: &Target| target.test && names.contains(&target.name);
    let mut selectors = BTreeSet::new();
    for package in packages {
        let enabled_features = package.enabled_local_features(workspace, cargo);
        for target in package.targets.iter().filter(|t| selected(t)) {
            let selector = target.test_build_arg()?;
            // Cargo skips gated targets in a default build but errors if named explicitly.
            if target
                .required_features
                .iter()
                .any(|feature| !enabled_features.contains(feature.as_str()))
                || selector
                    .1
                    .is_some_and(|name| name.contains(['*', '?', '[']))
            {
                return None;
            }
            selectors.insert(selector);
        }
    }
    // A selector applies to all the chosen packages and can override `test = false`.
    if selectors.is_empty()
        || packages.iter().flat_map(|p| &p.targets).any(|target| {
            !selected(target)
                && target
                    .test_build_arg()
                    .is_some_and(|selector| selectors.contains(&selector))
        })
    {
        return None;
    }
    Some(
        selectors
            .into_iter()
            .map(|(flag, name)| match name {
                Some(name) => format!("{flag}={name}"),
                None => flag.to_owned(),
            })
            .collect(),
    )
}

/// One line of `cargo --message-format=json`.
#[derive(Deserialize)]
struct CargoMessage {
    reason: String,
    package_id: Option<String>,
    target: Option<ArtifactTarget>,
    profile: Option<Profile>,
    executable: Option<PathBuf>,
    #[serde(default)]
    filenames: Vec<PathBuf>,
    message: Option<Diagnostic>,
}

#[derive(Deserialize)]
struct ArtifactTarget {
    name: String,
    kind: Vec<String>,
    src_path: Option<PathBuf>,
}

#[derive(Deserialize)]
struct Profile {
    test: bool,
}

#[derive(Deserialize)]
struct Diagnostic {
    level: String,
    rendered: Option<String>,
    message: Option<String>,
    spans: Vec<DiagnosticSpan>,
}

#[derive(Deserialize)]
struct DiagnosticSpan {
    file_name: String,
    byte_start: usize,
    line_start: usize,
    is_primary: bool,
}

#[derive(Debug, PartialEq, Clone)]
struct ErrorSpan {
    file: String,
    byte_start: usize,
    line: usize,
}

/// One compiler error. The primary spans say where it was detected, the secondary ones
/// often point at the cause (for "possibly-uninitialized", at the guard we inserted).
#[derive(Debug, PartialEq)]
struct CompileError {
    primary: Vec<ErrorSpan>,
    secondary: Vec<ErrorSpan>,
    rendered: String,
}

#[derive(Debug, Default, PartialEq)]
struct BuildOutput {
    /// `(target name, executable, package, target kind)` of the test binaries.
    test_binaries: Vec<(String, PathBuf, String, Vec<String>)>,
    errors: Vec<CompileError>,
    diagnostics: Vec<String>,
    artifacts: Vec<(PathBuf, Vec<PathBuf>)>,
}

/// The workspace packages that depend on `package`, directly or through others.
fn dependents_of(package: &str, packages: &[Package]) -> Vec<String> {
    let mut found: Vec<String> = vec![];
    let mut frontier = vec![package.to_string()];
    while let Some(current) = frontier.pop() {
        for other in packages {
            if other.name != package
                && !found.contains(&other.name)
                && other.dependencies.iter().any(|d| d.name == current)
            {
                found.push(other.name.clone());
                frontier.push(other.name.clone());
            }
        }
    }
    found.sort();
    found
}

/// Package ids embed the path, and the build runs in a copy, so compare by name.
/// Ids look like `path+file:///dir#name@1.0.0`, `path+file:///dir/name#1.0.0` or `name 1.0.0 (path+...)`.
fn is_package(id: &str, name: &str) -> bool {
    let (path, fragment) = id.split_once('#').unwrap_or((id, ""));
    fragment.split('@').next() == Some(name)
        || path.rsplit('/').next() == Some(name)
        || id.split(' ').next() == Some(name)
}

fn parse_build_output(stdout: &str, packages: &[&str]) -> BuildOutput {
    let mut out = BuildOutput::default();
    for line in stdout.lines() {
        let Ok(msg) = serde_json::from_str::<CargoMessage>(line) else {
            continue;
        };
        match msg.reason.as_str() {
            "compiler-artifact" => {
                let Some(target) = msg.target else { continue };
                if let Some(path) = target.src_path {
                    out.artifacts.push((normalize(&path), msg.filenames));
                }
                if let (Some(exe), Some(profile), Some(id)) =
                    (msg.executable, msg.profile, msg.package_id.as_deref())
                    && profile.test
                    && let Some(package) = packages.iter().find(|name| is_package(id, name))
                {
                    out.test_binaries
                        .push((target.name, exe, package.to_string(), target.kind));
                }
            }
            "compiler-message" => {
                let Some(diag) = msg.message else { continue };
                if diag.level != "error" {
                    continue;
                }
                let rendered = diag.rendered.or(diag.message).unwrap_or_default();
                if !rendered.is_empty() && !out.diagnostics.contains(&rendered) {
                    out.diagnostics.push(rendered.clone());
                }
                let span = |s: &DiagnosticSpan| ErrorSpan {
                    file: s.file_name.clone(),
                    byte_start: s.byte_start,
                    line: s.line_start,
                };
                let error = CompileError {
                    primary: diag
                        .spans
                        .iter()
                        .filter(|s| s.is_primary)
                        .map(span)
                        .collect(),
                    secondary: diag
                        .spans
                        .iter()
                        .filter(|s| !s.is_primary)
                        .map(span)
                        .collect(),
                    rendered,
                };
                // "aborting due to N previous errors" and friends carry no location, and the
                // lib is compiled twice (plain and test), which reports everything twice.
                if !error.primary.is_empty() && !out.errors.contains(&error) {
                    out.errors.push(error);
                }
            }
            _ => {}
        }
    }
    out
}

/// Cargo puts each rustc dep-info file beside an artifact with the crate's stem.
fn dependency_file(artifact: &Path) -> Option<String> {
    let stem = artifact.file_stem()?.to_str()?;
    let unprefixed = match artifact.extension().and_then(|ext| ext.to_str()) {
        Some("a" | "so" | "dylib" | "rlib" | "rmeta") => stem.strip_prefix("lib"),
        _ => None,
    };
    unprefixed
        .and_then(|stem| fs::read_to_string(artifact.with_file_name(format!("{stem}.d"))).ok())
        .or_else(|| fs::read_to_string(artifact.with_extension("d")).ok())
}

fn dep_inputs(text: &str, copy: &Path) -> BTreeSet<PathBuf> {
    // Rustc writes one empty rule per input. This avoids counting output
    // artifacts and excludes Cargo's transitive, promoted dependency file.
    text.lines()
        .filter_map(|line| line.strip_suffix(':'))
        .map(|name| name.replace("\\ ", " ").replace("\\#", "#"))
        .map(|name| normalize(&copy.join(name)))
        .filter(|path| path.starts_with(copy))
        .collect()
}

fn directory_entries(path: &Path) -> impl Iterator<Item = PathBuf> + use<> {
    fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
}

fn same_binary(a: &Path, b: &Path) -> Option<bool> {
    use std::io::{BufRead, BufReader, Read};
    let (a, mut b) = (fs::File::open(a).ok()?, fs::File::open(b).ok()?);
    let (left, right) = (a.metadata().ok()?, b.metadata().ok()?);
    if !left.is_file() || !right.is_file() || left.len() != right.len() {
        return Some(false);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if left.ino() == right.ino() && left.dev() == right.dev() {
            return Some(true);
        }
    }
    let mut a = BufReader::new(a);
    let mut other = [0; 8192];
    loop {
        let bytes = a.fill_buf().ok()?;
        let len = bytes.len();
        if len == 0 {
            return Some(true);
        }
        b.read_exact(&mut other[..len]).ok()?;
        if bytes != &other[..len] {
            return Some(false);
        }
        a.consume(len);
    }
}

fn artifact_inputs(artifact: &Path, root: &Path, copy: &Path) -> BTreeSet<PathBuf> {
    let read = |binary: &Path| {
        dependency_file(binary)
            .map(|text| dep_inputs(&text, copy))
            .filter(|inputs| inputs.contains(root))
    };
    if let Some(inputs) = read(artifact) {
        return inputs;
    }
    // Cargo exposes binaries linked for integration tests at the top of the
    // profile. Match the actual rustc output beside its private dep-info file.
    let Some((profile, stem)) = artifact
        .parent()
        .zip(artifact.file_stem().and_then(|s| s.to_str()))
    else {
        return BTreeSet::new();
    };
    let crate_stem = stem.replace('-', "_");
    let hashed = format!("{crate_stem}-");
    let old = directory_entries(&profile.join("deps")).filter(|path| {
        path.extension() == artifact.extension()
            && path
                .file_stem()
                .and_then(|s| s.to_str())
                .is_some_and(|name| name.starts_with(&hashed))
    });
    let mut private_name = std::ffi::OsString::from(crate_stem);
    if let Some(extension) = artifact.extension() {
        private_name.push(".");
        private_name.push(extension);
    }
    let new = directory_entries(&profile.join("build"))
        .flat_map(|package| directory_entries(&package))
        .map(|unit| unit.join("out").join(&private_name));
    // A cached binary can have identical bytes and different inputs. Require
    // the source to be present in every matching compilation.
    old.chain(new)
        .filter_map(|path| {
            let inputs = read(&path)?;
            (same_binary(artifact, &path) == Some(true)).then_some(inputs)
        })
        .reduce(|left, right| left.intersection(&right).cloned().collect())
        .unwrap_or_default()
}

#[derive(Default)]
struct BuiltSources(BTreeMap<PathBuf, BTreeSet<PathBuf>>);

impl BuiltSources {
    fn from_artifacts(artifacts: &[(PathBuf, Vec<PathBuf>)], copy: &Path) -> Self {
        let mut built = Self::default();
        for (root, artifacts) in artifacts {
            if !root.starts_with(copy) {
                continue;
            }
            let files = built.0.entry(root.clone()).or_default();
            files.insert(root.clone());
            if let Some(inputs) = artifacts
                .iter()
                .map(|file| artifact_inputs(file, root, copy))
                .find(|inputs| !inputs.is_empty())
            {
                files.extend(inputs);
            }
        }
        built
    }

    fn contains(&self, source: &SourceFile) -> bool {
        source.owners.iter().any(|root| {
            self.0
                .get(root)
                .is_some_and(|files| files.contains(&source.path))
        })
    }
}

struct SourceFile {
    /// Path relative to the package directory, for reports.
    display: String,
    /// Path inside the working copy.
    path: PathBuf,
    owners: BTreeSet<PathBuf>,
    original: String,
    sites: Vec<Site>,
    /// Sites whose mutants are built one by one.
    rebuild_sites: Vec<Site>,
    /// Where every site of the file is (line, column, code), also those a run of a part
    /// leaves out: findings are numbered by these.
    places: Vec<(usize, usize, String)>,
    rendered: Option<Rendered>,
}

/// How a group of tests is started.
#[derive(Clone)]
enum Runner {
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
    fn executable(&self) -> Option<&Path> {
        match self {
            Self::Exe(exe) | Self::Isolated { exe, .. } => Some(exe),
            Self::Doc { .. } | Self::Nextest { .. } => None,
        }
    }
}

struct TestBinary {
    name: String,
    /// Directory the tests run in: their package's, as under `cargo test`.
    cwd: PathBuf,
    /// Where the marks of its runs are made: a directory of this analysis alone.
    marks: PathBuf,
    git_ceiling: PathBuf,
    /// The tests the binary contains, by the name libtest selects them with.
    tests: BTreeSet<String>,
    runner: Runner,
    /// How long the tests take when nothing is recorded: what a mutant is measured against.
    baseline: Duration,
    /// How long they took while recording.
    traced: Duration,
    timeout: Option<Duration>,
    wrapper_grace: Duration,
    report_slow: bool,
    filtered_timings:
        std::sync::Mutex<BTreeMap<Vec<String>, std::sync::Arc<std::sync::OnceLock<Duration>>>>,
    trace: Trace,
}

impl TestBinary {
    fn mutant_limit(&self, baseline: Duration) -> Duration {
        self.timeout.unwrap_or_else(|| mutant_timeout(baseline))
    }

    fn filtered_limit(&self, filter: Option<&[String]>, traced: bool) -> Duration {
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
        let timed = {
            let mut groups = self.filtered_timings.lock().unwrap();
            groups.entry(filter.to_vec()).or_default().clone()
        };
        let timing = *timed.get_or_init(|| {
            let untraced = self.baseline;
            let started = Instant::now();
            // One measured group is shared by all jobs with this exact filter.
            // A failing or inconclusive timing run keeps the conservative limit.
            match run_binary(self, Some(filter), &[], untraced, RunOutput::Quiet, 1) {
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

#[derive(Debug, PartialEq, Clone, Copy)]
enum Outcome {
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

fn isolate_copy_command(command: &mut Command, work: &Path) {
    git_index::remove_git_overrides(command);
    command.env("GIT_CEILING_DIRECTORIES", work);
}

/// Keeps two runs from rebuilding the same working copy under each other.
struct WorkLock(PathBuf);

impl WorkLock {
    fn take(work: &Path) -> Result<WorkLock> {
        let path = work.join(".tmt-lock");
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => Ok(WorkLock(path)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => bail!(
                "another tmt run is using {}. If none is running, delete {}.",
                work.display(),
                path.display()
            ),
            Err(err) => Err(err).with_context(|| format!("creating {}", path.display())),
        }
    }
}

impl Drop for WorkLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

/// Name of the log of written files, beside the working copy.
const WRITTEN_LOG: &str = ".tmt-written";

/// The working copy and what the runs did to it (formal/tla/WorkCopy.tla).
///
/// Most of the copy is plain copies of the user's files. The files a run instruments
/// are rewritten, and the log names them. The next run leaves those alone when it brings
/// the copy up to date, renders them again from the user's files, and writes only what
/// came out different: a file that keeps its modification time is not rebuilt by cargo.
/// What the log names and the run did not write again goes back to a plain copy.
struct WorkCopy {
    root: PathBuf,
    log: PathBuf,
    /// Paths of the copy that the current run wants to exist.
    kept: RefCell<HashSet<PathBuf>>,
    /// Named by the log when the run started.
    earlier: HashSet<PathBuf>,
    /// Those of `earlier` that the user's tree still has.
    visited: RefCell<HashSet<PathBuf>>,
    /// Written by this run.
    written: RefCell<HashSet<PathBuf>>,
    /// What the log names now.
    logged: RefCell<HashSet<PathBuf>>,
}

impl WorkCopy {
    fn open(work: &Path, root: &Path) -> WorkCopy {
        let log = work.join(WRITTEN_LOG);
        // The log is appended to and replaced: never through a link to somewhere else.
        for file in [log.clone(), log.with_extension("new")] {
            if fs::symlink_metadata(&file).is_ok_and(|found| found.is_symlink()) {
                let _ = fs::remove_file(&file);
            }
        }
        let earlier: HashSet<PathBuf> = fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .map(Path::new)
            // Nothing but a path below the copy: the entries are deleted and overwritten.
            .filter(|relative| {
                let mut parts = relative.components();
                parts.clone().next().is_some()
                    && parts.all(|part| matches!(part, std::path::Component::Normal(_)))
            })
            .map(|relative| root.join(relative))
            .collect();
        WorkCopy {
            root: root.to_path_buf(),
            log,
            kept: RefCell::default(),
            logged: RefCell::new(earlier.clone()),
            earlier,
            visited: RefCell::default(),
            written: RefCell::default(),
        }
    }

    fn is_kept(&self, path: &Path) -> bool {
        self.kept.borrow().contains(path) || self.earlier.contains(path)
    }

    /// The text of the user's file that `path` of the copy stands for. `None` for what
    /// only an earlier run left there.
    fn original(&self, path: &Path, workspace: &Path) -> Result<Option<String>> {
        let source = if !self.earlier.contains(path) {
            path.to_path_buf() // a plain copy
        } else if !self.visited.borrow().contains(path) {
            return Ok(None);
        } else {
            workspace.join(path.strip_prefix(&self.root).unwrap_or(path))
        };
        fs::read_to_string(&source)
            .map(Some)
            .with_context(|| format!("reading {}", source.display()))
    }

    /// Writes a file of the working copy. Refuses to write anywhere else, and never
    /// through a symbolic link. The log names the file before it changes.
    fn write(&self, path: &Path, content: &str) -> Result<()> {
        if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
            fs::remove_file(path)?;
        }
        let parent = path.parent().and_then(|p| p.canonicalize().ok());
        let inside = match (parent, self.root.canonicalize()) {
            (Some(parent), Ok(root)) => parent.starts_with(root),
            _ => false,
        };
        if !inside {
            bail!(
                "refusing to write outside the working copy: {}",
                path.display()
            );
        }
        self.record(path)?;
        if fs::read_to_string(path).is_ok_and(|existing| existing == content) {
            return Ok(()); // keep the modification time: nothing to rebuild
        }
        fs::write(path, content).with_context(|| format!("writing {}", path.display()))
    }

    fn record(&self, path: &Path) -> Result<()> {
        self.written.borrow_mut().insert(path.to_path_buf());
        if self.logged.borrow().contains(path) {
            return Ok(());
        }
        let line = path
            .strip_prefix(&self.root)
            .ok()
            .and_then(Path::to_str)
            .filter(|text| !text.chars().any(char::is_control));
        let Some(line) = line else {
            bail!(
                "cannot instrument {}: its name cannot be written on a line of the log",
                path.display()
            );
        };
        use std::io::Write as _;
        let mut log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log)
            .with_context(|| format!("opening {}", self.log.display()))?;
        writeln!(log, "{line}").with_context(|| format!("writing {}", self.log.display()))?;
        self.logged.borrow_mut().insert(path.to_path_buf());
        Ok(())
    }

    /// Before the build: what earlier runs wrote and this one did not is a plain copy
    /// again (or gone, like its source), and only then leaves the log.
    fn settle(&self, workspace: &Path) -> Result<()> {
        let written = self.written.borrow();
        for path in self.earlier.difference(&written) {
            if fs::symlink_metadata(path).is_ok() {
                fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
            }
            if self.visited.borrow().contains(path) {
                let source = workspace.join(path.strip_prefix(&self.root).unwrap_or(path));
                copy_as_new(&source, path)?;
            }
        }
        let mut lines: Vec<&str> = written
            .iter()
            .filter_map(|path| path.strip_prefix(&self.root).ok()?.to_str())
            .collect();
        lines.sort_unstable();
        let fresh = self.log.with_extension("new");
        fs::write(
            &fresh,
            lines
                .iter()
                .map(|line| format!("{line}\n"))
                .collect::<String>(),
        )?;
        fs::rename(&fresh, &self.log)?;
        *self.logged.borrow_mut() = written.clone();
        Ok(())
    }
}

/// Makes `dst` a copy of the file `src`, leaving it alone if it already is one. A file
/// that keeps its modification time is not rebuilt by cargo, so a second run only
/// recompiles what changed.
fn sync_file(src: &Path, dst: &Path, state: &WorkCopy) -> Result<()> {
    state.kept.borrow_mut().insert(dst.to_path_buf());
    if state.earlier.contains(dst) {
        state.visited.borrow_mut().insert(dst.to_path_buf());
        if fs::symlink_metadata(dst).is_ok_and(|m| m.is_file()) {
            // Written by an earlier run: this one decides what becomes of it.
            return Ok(());
        }
    }
    // Times say nothing certain: a file put back from an archive is older than the copy
    // of what it replaces. A copy is what holds the same bytes.
    // A link is not a copy, whatever it points to.
    let up_to_date = fs::symlink_metadata(dst).is_ok_and(|to| to.is_file()) && same_bytes(src, dst);
    if up_to_date {
        return Ok(());
    }
    if let Ok(existing) = fs::symlink_metadata(dst) {
        if existing.is_dir() {
            fs::remove_dir_all(dst)?;
        } else {
            fs::remove_file(dst)?;
        }
    }
    copy_as_new(src, dst)
}

/// Whether two files hold the same bytes. One that cannot be read is not the same.
fn same_bytes(a: &Path, b: &Path) -> bool {
    fs::read(a)
        .ok()
        .zip(fs::read(b).ok())
        .is_some_and(|(a, b)| a == b)
}

/// Makes `dir` a directory of the working copy. Whatever else is in the way below the
/// copy's root goes first, a link above all: a link is followed, and what is written
/// "into the copy" would land where it points.
fn real_dir(dir: &Path, state: &WorkCopy) -> Result<()> {
    if let Ok(below) = dir.strip_prefix(&state.root) {
        let mut path = state.root.clone();
        for part in below.components() {
            path.push(part);
            match fs::symlink_metadata(&path) {
                Ok(found) if !found.is_dir() => fs::remove_file(&path)
                    .with_context(|| format!("removing {}", path.display()))?,
                _ => {}
            }
        }
    }
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))
}

/// Copies a file and gives the copy the current time. Some systems copy the modification
/// time along, and a file that changed in the working copy but looks older than the last
/// build would not be rebuilt.
fn copy_as_new(src: &Path, dst: &Path) -> Result<()> {
    fs::copy(src, dst).with_context(|| format!("copying {}", src.display()))?;
    // Opened for reading: the copy of a read-only file is read-only too.
    fs::File::open(dst)
        .and_then(|file| file.set_modified(std::time::SystemTime::now()))
        .with_context(|| format!("touching {}", dst.display()))
}

/// Removes from the working copy whatever the current run did not put there.
fn prune(dir: &Path, state: &WorkCopy) -> Result<bool> {
    let mut empty = true;
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let is_dir = fs::symlink_metadata(&path)?.is_dir();
        if is_dir {
            if prune(&path, state)? && !state.is_kept(&path) {
                fs::remove_dir(&path)?;
            } else {
                empty = false;
            }
        } else if state.is_kept(&path) {
            empty = false;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(empty)
}

/// Copies one file, directory or symbolic link into the working copy.
///
/// The copy never contains a symbolic link: instrumented sources are written into it, and
/// a link would send those writes back to the user's files. A link to a file becomes a
/// plain file with the same content. A link to a directory is followed only when the
/// directory lies inside `workspace` and does not contain the link itself.
fn copy_entry(
    src: &Path,
    dst: &Path,
    workspace: &Path,
    skip: &dyn Fn(&Path) -> bool,
    state: &WorkCopy,
) -> Result<()> {
    let kind = fs::symlink_metadata(src)?.file_type();
    if kind.is_symlink() {
        let Ok(target) = src.canonicalize() else {
            return Ok(()); // dangling
        };
        if target.is_dir() {
            let contains_link = src
                .parent()
                .is_some_and(|dir| dir.canonicalize().is_ok_and(|dir| dir.starts_with(&target)));
            if !target.starts_with(workspace) || contains_link || skip(&target) {
                return Ok(());
            }
            return copy_tree(&target, dst, workspace, skip, state);
        }
        sync_file(&target, dst, state)?;
    } else if kind.is_dir() {
        // Build output and other caches (cargo marks its target directories this way).
        if !src.join("CACHEDIR.TAG").exists() {
            copy_tree(src, dst, workspace, skip, state)?;
        }
    } else {
        sync_file(src, dst, state)?;
    }
    Ok(())
}

fn copy_tree(
    from: &Path,
    to: &Path,
    workspace: &Path,
    skip: &dyn Fn(&Path) -> bool,
    state: &WorkCopy,
) -> Result<()> {
    real_dir(to, state)?;
    for entry in fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let src = entry?.path();
        if skip(&src) {
            continue;
        }
        let Some(name) = src.file_name() else {
            continue;
        };
        copy_entry(&src, &to.join(name), workspace, skip, state)?;
    }
    Ok(())
}

/// An explicitly selected fixture may include ignored directories. Resolve links to
/// their contents inside the workspace, without following them into a parent cycle.
fn copy_fixture_entry(
    from: &Path,
    to: &Path,
    workspace: &Path,
    forbidden: &dyn Fn(&Path) -> bool,
    state: &WorkCopy,
    visiting: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let resolved = from
        .canonicalize()
        .with_context(|| format!("resolving fixture {}", from.display()))?;
    let allowed = |path: &Path| {
        path.starts_with(workspace)
            && path != workspace
            && !path
                .ancestors()
                .take_while(|part| *part != workspace)
                .any(forbidden)
    };
    if !allowed(from) || !allowed(&resolved) {
        bail!(
            "fixture {} resolves to an excluded path: {}",
            from.display(),
            resolved.display()
        );
    }
    let kind = fs::metadata(&resolved)?.file_type();
    real_dir(to.parent().expect("fixture has a parent"), state)?;
    if kind.is_file() {
        return sync_file(&resolved, to, state);
    }
    if !kind.is_dir() || !visiting.insert(resolved.clone()) {
        bail!(
            "fixture {} is a cycle or is not a regular file or directory",
            from.display()
        );
    }
    real_dir(to, state)?;
    state.kept.borrow_mut().insert(to.to_path_buf());
    for entry in fs::read_dir(&resolved)? {
        let entry = entry?;
        let path = entry.path();
        if !forbidden(&path) {
            copy_fixture_entry(
                &path,
                &to.join(entry.file_name()),
                workspace,
                forbidden,
                state,
                visiting,
            )?;
        }
    }
    visiting.remove(&resolved);
    Ok(())
}

/// The files under `dir` that git tracks or would track (not ignored), relative to `dir`.
/// `None` when `dir` is not in a git work tree, or is itself ignored: then git has nothing
/// to say about which files matter.
fn git_visible_files(dir: &Path) -> Result<Option<Vec<PathBuf>>> {
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stderr(Stdio::null())
            .output()
    };
    // No git, not a work tree, or a directory git ignores: nothing to go by.
    let Ok(inside) = git(&["rev-parse", "--is-inside-work-tree"]) else {
        return Ok(None);
    };
    if !inside.status.success() {
        return Ok(None);
    }
    // Exit status 0 means "ignored".
    if git(&["check-ignore", "-q", "."])?.status.success() {
        return Ok(None);
    }
    // From here on a failure must not turn into "copy everything": that would copy the
    // very files the ignore rules keep out.
    let listing = git(&[
        "ls-files",
        "--cached",
        "--others",
        "--exclude-standard",
        "-z",
    ])?;
    if !listing.status.success() {
        bail!(
            "git could not list the files of {}; pass --all-files to copy everything",
            dir.display()
        );
    }
    Ok(Some(
        listing
            .stdout
            .split(|b| *b == 0)
            .filter(|name| !name.is_empty())
            .filter_map(|name| std::str::from_utf8(name).ok())
            .map(PathBuf::from)
            .collect(),
    ))
}

/// Copies the files of `from` that git does not ignore. A submodule is listed by its
/// parent as one directory and has ignore rules of its own, so it is copied the same way.
fn copy_git_visible(
    from: &Path,
    to: &Path,
    files: Vec<PathBuf>,
    workspace: &Path,
    skip: &dyn Fn(&Path) -> bool,
    state: &WorkCopy,
) -> Result<usize> {
    let mut copied = 0;
    for relative in files {
        let (src, dst) = (from.join(&relative), to.join(&relative));
        if skip(&src)
            || src
                .ancestors()
                .any(|a| a != from && a.starts_with(from) && skip(a))
        {
            continue;
        }
        let Ok(kind) = fs::symlink_metadata(&src).map(|m| m.file_type()) else {
            continue; // deleted in the working tree
        };
        if let Some(parent) = dst.parent() {
            real_dir(parent, state)?;
        }
        if kind.is_dir() {
            match git_visible_files(&src)? {
                Some(inner) => {
                    copied += copy_git_visible(&src, &dst, inner, workspace, skip, state)?
                }
                None => copy_tree(&src, &dst, workspace, skip, state)?,
            }
        } else {
            copy_entry(&src, &dst, workspace, skip, state)?;
            copied += 1;
        }
    }
    Ok(copied)
}

#[derive(Default)]
struct ModuleOwners {
    production: BTreeSet<PathBuf>,
    consumers: BTreeSet<PathBuf>,
}

fn source_graph(
    roots: impl IntoIterator<Item = (PathBuf, bool)>,
    copy: &Path,
    state: &WorkCopy,
    workspace: &Path,
) -> Result<BTreeMap<PathBuf, ModuleOwners>> {
    use std::collections::VecDeque;
    let mut files: BTreeMap<PathBuf, ModuleOwners> = BTreeMap::new();
    for (root, production) in roots {
        let directory = root.parent().expect("Cargo target directory").to_path_buf();
        let mut pending = VecDeque::from([(root.clone(), directory, production)]);
        let mut seen = HashSet::new();
        while let Some((path, directory, active)) = pending.pop_front() {
            if !seen.insert((path.clone(), directory.clone(), active)) {
                continue;
            }
            let Some(original) = state.original(&path, workspace)? else {
                continue;
            };
            let record = files.entry(path.clone()).or_default();
            record.consumers.insert(root.clone());
            if active {
                record.production.insert(root.clone());
            }
            for module in crate::modules::external(&original, &path, &directory, active) {
                if let Some(child) = module
                    .paths
                    .iter()
                    .map(|path| normalize(path))
                    .find(|path| path.starts_with(copy) && path.is_file())
                {
                    pending.push_back((child, normalize(&module.directory), module.production));
                }
            }
        }
    }
    files.retain(|_, owners| !owners.production.is_empty());
    Ok(files)
}

/// Resolves `.` and `..` without touching the file system.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

fn relative_source(path: &Path, package: &Path) -> PathBuf {
    for (depth, ancestor) in package.ancestors().enumerate() {
        if let Ok(local) = path.strip_prefix(ancestor) {
            let mut relative: PathBuf = std::iter::repeat_n("..", depth).collect();
            relative.push(local);
            return relative;
        }
    }
    path.to_path_buf()
}

/// Rewrites `path = "../outside"` dependencies of a manifest to absolute paths when they
/// point outside `workspace`. Returns `None` when nothing has to change.
fn absolutize_path_dependencies(
    manifest: &str,
    manifest_dir: &Path,
    workspace: &Path,
    copy: &Path,
) -> Result<Option<String>> {
    use toml_edit::{DocumentMut, Item, TableLike, value};

    fn fix(deps: &mut dyn TableLike, dirs: (&Path, &Path, &Path), changed: &mut bool) {
        let (manifest_dir, workspace, copy) = dirs;
        for (_, dep) in deps.iter_mut() {
            let Some(dep) = dep.as_table_like_mut() else {
                continue;
            };
            let Some(path) = dep.get("path").and_then(Item::as_str) else {
                continue;
            };
            let absolute = Path::new(path).is_absolute();
            let target = normalize(&manifest_dir.join(path));
            let rewritten = match target.strip_prefix(workspace) {
                // Outside the workspace: not copied, so point at the original.
                Err(_) => target,
                // Inside, but spelled as an absolute path: that is the original too, and
                // the build has to use the copy.
                Ok(inside) if absolute => copy.join(inside),
                Ok(_) => continue,
            };
            dep.insert("path", value(rewritten.to_string_lossy().into_owned()));
            *changed = true;
        }
    }
    const SECTIONS: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];
    fn fix_sections(table: &mut dyn TableLike, dirs: (&Path, &Path, &Path), changed: &mut bool) {
        for section in SECTIONS {
            if let Some(deps) = table.get_mut(section).and_then(Item::as_table_like_mut) {
                fix(deps, dirs, changed);
            }
        }
    }

    let mut doc: DocumentMut = manifest.parse().context("parsing Cargo.toml")?;
    let mut changed = false;
    let dirs = (manifest_dir, workspace, copy);
    fix_sections(doc.as_table_mut(), dirs, &mut changed);
    if let Some(ws) = doc.get_mut("workspace").and_then(Item::as_table_like_mut) {
        fix_sections(ws, dirs, &mut changed);
    }
    for per_key in ["target", "patch"] {
        let Some(outer) = doc.get_mut(per_key).and_then(Item::as_table_like_mut) else {
            continue;
        };
        for (_, inner) in outer.iter_mut() {
            let Some(inner) = inner.as_table_like_mut() else {
                continue;
            };
            if per_key == "target" {
                fix_sections(inner, dirs, &mut changed);
            } else {
                fix(inner, dirs, &mut changed);
            }
        }
    }
    Ok(changed.then(|| doc.to_string()))
}

/// Tests to pass to the binary, or `None` for "run everything".
///
/// A name that is not one of the binary's tests comes from a thread the test named
/// itself; passing it to libtest would select nothing and "pass". Such observations
/// belong to some unknown test, like those of unnamed threads.
fn test_filter(tests: &BTreeSet<String>, known: &BTreeSet<String>) -> Option<Vec<String>> {
    if tests.len() > MAX_TEST_FILTERS || !tests.iter().all(|t| known.contains(t)) {
        None
    } else {
        Some(tests.iter().cloned().collect())
    }
}

/// Names of the tests in libtest's `--list --format terse` output.
fn parse_test_list(stdout: &str) -> BTreeSet<String> {
    stdout
        .lines()
        .filter_map(|line| line.strip_suffix(": test"))
        .map(str::to_string)
        .collect()
}

/// What makes cargo build the tests (and what they depend on) at an optimisation level.
fn list_tests(
    exe: &Path,
    cwd: &Path,
    work: &Path,
    ignored: bool,
    strict: bool,
) -> Result<BTreeSet<String>> {
    let mut command = Command::new(exe);
    isolate_copy_command(&mut command, work);
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
fn profile_env(opt_level: Option<u8>) -> Vec<(&'static str, String)> {
    opt_level.map_or(vec![], |level| {
        vec![
            ("CARGO_PROFILE_DEV_OPT_LEVEL", level.to_string()),
            ("CARGO_PROFILE_TEST_OPT_LEVEL", level.to_string()),
        ]
    })
}

/// A trace run longer than this is followed by a run without recording, for the timing.
const SLOW_TRACE: Duration = Duration::from_secs(2);

fn mutant_timeout(baseline: Duration) -> Duration {
    Duration::from_secs(5) + baseline * 5
}

const TEST_POLL_INITIAL: Duration = Duration::from_micros(250);
const TEST_POLL_CAP: Duration = Duration::from_millis(5);

/// Short-running tests often finish before the old polling interval. Back off for
/// longer runs so they do not pay for frequent exit and failure-output checks.
fn next_test_poll(delay: &mut Duration, remaining: Duration) -> Duration {
    let wait = (*delay).min(remaining);
    *delay = (*delay * 2).min(TEST_POLL_CAP);
    wait
}

/// The test threads of one test binary when `sharing` of them run at the same time: its
/// share of the cores. Never fewer than two, because the harness runs the tests of a
/// single thread on the main thread, where they have no name to be told apart by.
fn test_threads(cores: usize, sharing: usize) -> usize {
    (cores / sharing.max(1)).max(2)
}

enum RunOutput<'a> {
    Quiet,
    FirstFailure,
    Capture(&'a fs::File),
}

fn nextest_outcome(code: Option<i32>) -> Result<Outcome> {
    match code {
        Some(0) => Ok(Outcome::Pass),
        // Other exit codes indicate infrastructure errors or no selected tests.
        Some(100) => Ok(Outcome::Fail),
        _ => bail!("cargo nextest failed outside a test (exit code {code:?})"),
    }
}

fn run_binary(
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
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting the tests of `{}`", bin.name))?;
    let group = child.id();
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
                .map(|names| names.join(", "))
                .unwrap_or_else(|| bin.name.clone());
            eprintln!(
                "tmt: waiting for mutant tests `{selected}` (limit {:.2}s)",
                timeout.as_secs_f64()
            );
            waiting_reported = true;
        }
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
        std::thread::sleep(next_test_poll(&mut delay, timeout - elapsed));
    })();
    drop(output_to_watch);
    if result.is_err() {
        kill(&mut child);
        let _ = release_mark(&unable_mark);
    }
    let outcome = result?;

    // Whatever the tests made of it, a run that left the mark was not the mutant's. The
    // mark alone says so: a status is the tests' to choose, and one of them may well end
    // with the runtime's when it finds a mutant out.
    if release_mark(&unable_mark)? {
        return Ok(Outcome::Unable);
    }
    Ok(outcome)
}

struct FailureOutput {
    file: fs::File,
    line: Vec<u8>,
    discarded: bool,
}

impl FailureOutput {
    fn new(file: fs::File) -> Self {
        Self {
            file,
            line: Vec::new(),
            discarded: false,
        }
    }

    fn failed(&mut self) -> std::io::Result<bool> {
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

fn stop_nextest(child: &mut std::process::Child, grace: Duration) {
    let wait = |child: &mut std::process::Child, limit: Duration| {
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) if started.elapsed() < limit => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => return false,
            }
        }
    };
    if wait(child, Duration::ZERO) {
        return;
    }
    let group = child.id();
    // Snapshot the owned groups while the parent exists. A wrapper can exit
    // before its independently grouped children are reparented and stopped.
    #[cfg(unix)]
    let mut owned = owned_test_processes(group);
    signal_test_group(group, 15);
    let mut reaped = wait(child, grace);
    if !reaped {
        // The second signal asks nextest to force its units to stop.
        signal_test_group(group, 15);
        reaped = wait(child, grace.min(Duration::from_secs(1)));
    }
    #[cfg(unix)]
    {
        if !reaped {
            owned.extend(owned_test_processes(group));
        }
        let current = test_processes();
        let surviving_groups: BTreeSet<_> = current
            .iter()
            .filter(|process| {
                owned.iter().any(|before| {
                    process.pid == before.pid
                        && process.group == before.group
                        && process.started == before.started
                })
            })
            .map(|process| process.group)
            .collect();
        for surviving in surviving_groups {
            kill_test_group(surviving);
        }
    }
    if !reaped {
        kill_test_group(group);
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(unix)]
struct TestProcess {
    pid: u32,
    parent: u32,
    group: u32,
    started: String,
}

#[cfg(unix)]
fn test_processes() -> Vec<TestProcess> {
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

#[cfg(unix)]
fn owned_test_processes(root: u32) -> Vec<TestProcess> {
    let data = test_processes();
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

fn kill_test_group(id: u32) {
    signal_test_group(id, 9);
}

fn signal_test_group(id: u32, signal: i32) {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        unsafe extern "C" {
            fn killpg(group: std::ffi::c_int, signal: std::ffi::c_int) -> std::ffi::c_int;
        }
        if let Ok(group) = std::ffi::c_int::try_from(id) {
            // SAFETY: the argument is the child PID used to create its own test group.
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
fn run_baseline(
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

/// The directory of the marks in a working directory: made if it is not there, used if
/// it is one. Anything else of that name (a file, a link) is somebody's, in a directory
/// that was given to work in before it was this tool's: it is left alone and said.
fn marks_dir(work: &Path) -> Result<PathBuf> {
    work_directory(work, "marks")
}

fn work_directory(work: &Path, name: &str) -> Result<PathBuf> {
    let path = work.join(name);
    match fs::symlink_metadata(&path) {
        Ok(found) if found.is_dir() => {}
        Ok(_) => bail!(
            "{} is in the way: the working directory needs a directory of that name, and \
             this is not one. Move it, or choose another --work-dir",
            path.display()
        ),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(&path).with_context(|| format!("creating {}", path.display()))?
        }
        Err(err) => return Err(err).with_context(|| format!("inspecting {}", path.display())),
    }
    Ok(path)
}

/// Makes the file by which a run says that it gave up: there for the run, and gone when
/// the run could not go on as its mutant. The runtime only has to take it away, which
/// it can do on a full disk too. The exit status would say the same, but only that of
/// the tests themselves: a program a test started gives its status to the test, which
/// fails for it.
///
/// The file is new: one of the same name that is there already is left as it is, and
/// another name is tried.
fn reserve_file(dir: &Path, label: &str, runs: &std::sync::atomic::AtomicUsize) -> Result<PathBuf> {
    // The tests run in a directory of their own, and so does what they start: a place
    // given from here (`TMPDIR=.`) has to be the same place there.
    let dir = std::path::absolute(dir).unwrap_or(dir.to_path_buf());
    loop {
        let mark = dir.join(format!(
            "tmt-{label}-{}-{}",
            std::process::id(),
            runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&mark)
        {
            Ok(_) => return Ok(mark),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
            // If it cannot be made, nothing could be told from the run.
            Err(err) => {
                return Err(err).with_context(|| format!("creating {}", mark.display()));
            }
        }
    }
}

fn run_source_tests(binaries: &[&TestBinary]) -> Result<Outcome> {
    for bin in binaries {
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

fn observe_witness(
    baseline: &TestBinary,
    exe: PathBuf,
    wrapper: Option<&Path>,
    scenario: &witness::Scenario,
    name: &str,
    compares: bool,
) -> Result<Option<witness::Observation>> {
    use std::io::Read as _;
    let test_name = scenario.test_name(name);
    let test = TestBinary {
        name: test_name.clone(),
        cwd: baseline.cwd.clone(),
        marks: baseline.marks.clone(),
        git_ceiling: baseline.git_ceiling.clone(),
        tests: BTreeSet::new(),
        runner: Runner::Isolated {
            exe,
            test: test_name,
            ignored: false,
            wrapper: wrapper.map(Path::to_path_buf),
        },
        baseline: baseline.baseline,
        traced: Duration::ZERO,
        timeout: baseline.timeout,
        wrapper_grace: baseline.wrapper_grace,
        report_slow: false,
        filtered_timings: Default::default(),
        trace: Trace::default(),
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

fn candidate_edit(
    file: &SourceFile,
    start: usize,
    end: usize,
    replacement: String,
) -> report::CandidateEdit {
    let prefix = &file.original[..start];
    report::CandidateEdit {
        file: file.display.clone(),
        line: scope::line_of(&file.original, start),
        column: prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1,
        start,
        end,
        before: file.original[start..end].to_string(),
        replacement,
    }
}

fn witness_location(source: &SourceFile, candidate: &witness::Candidate) -> report::Location {
    let span = &candidate.edits[0].span;
    let prefix = &source.original[..span.start];
    let column = prefix.rsplit('\n').next().unwrap_or("").chars().count() + 1;
    report::Location {
        file: source.display.clone(),
        line: scope::line_of(&source.original, span.start),
        column,
        code: report::one_line(&source.original[span.clone()]),
        ..Default::default()
    }
}

enum WitnessTrial {
    Inapplicable,
    Unviable,
    Evaluated(Outcome),
}

struct WitnessSurvivor<'a> {
    candidate: &'a witness::Candidate,
    at: report::Location,
    existing: Option<usize>,
    scenarios: Vec<(&'a witness::Scenario, &'a TestBinary)>,
}

struct SharedWitness {
    value: String,
    name: String,
    tests: std::rc::Rc<[witness::Edit]>,
}

fn shared_witnesses(
    source: &str,
    survivors: &[WitnessSurvivor<'_>],
    wrapper: Option<&Path>,
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
                let value = observe_witness(runner, exe.clone(), wrapper, scenario, name, false)
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
        for (index, scenario, runner, name, value) in observed {
            let checked = observe_witness(runner, exe.clone(), wrapper, scenario, name, true);
            if checked.is_ok_and(|value_read| {
                value_read
                    .is_some_and(|read| read.equals_expected == Some(true) && read.value == value)
            }) {
                originals[index] = Some(SharedWitness {
                    value,
                    name: name.clone(),
                    tests: tests.clone(),
                });
            }
        }
    }
    Ok(originals)
}

/// Takes the mark away after the run and says whether the run had given up (the mark
/// was gone). Anything else that keeps it from being taken away leaves the question
/// open, and is an error rather than an answer.
fn release_mark(mark: &Path) -> Result<bool> {
    match fs::remove_file(mark) {
        Ok(()) => Ok(false),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(err) => Err(err).with_context(|| {
            format!(
                "removing {}: cannot tell whether the tests ran the mutant to the end",
                mark.display()
            )
        }),
    }
}

/// A line of libtest output that reports a failed test.
fn is_failure_line(line: &str) -> bool {
    line.starts_with("test ") && line.ends_with(" ... FAILED")
}

/// The lines to analyse, from `--since` and `--lines`; `None` for everything. File names
/// are relative to the workspace root.
fn requested_scope(
    options: &Options,
    workspace: &Path,
    package_dir: &Path,
    copy: &Path,
) -> Result<Option<Scope>> {
    if options.since.is_none() && options.lines.is_empty() {
        return Ok(None);
    }
    // `--lines` names files the way the report does: relative to the package.
    let prefix = package_dir.strip_prefix(copy).unwrap_or(Path::new(""));
    let lines: Vec<String> = options
        .lines
        .iter()
        .map(|spec| normalize(&prefix.join(spec)).to_string_lossy().into_owned())
        .collect();
    let mut scope = Scope::parse_lines(&lines)?;
    if let Some(revision) = &options.since {
        let git = |args: &[&str]| -> Result<String> {
            let output = Command::new("git")
                .arg("-C")
                .arg(workspace)
                .args(args)
                .output()
                .context("running git")?;
            if !output.status.success() {
                bail!(
                    "git {} failed: {}",
                    args.join(" "),
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        };
        // `--relative` gives paths from the workspace root even inside a larger repository.
        scope.merge(Scope::from_diff(&git(&[
            "diff",
            "-U0",
            "--no-color",
            "--no-ext-diff",
            "--relative",
            "--src-prefix=a/",
            "--dst-prefix=b/",
            revision,
            "--",
        ])?));
        for file in git(&["ls-files", "--others", "--exclude-standard"])?.lines() {
            scope.add_whole_file(file);
        }
    }
    Ok(Some(scope))
}

/// The sites to disable for one compile error, from the most to the least specific guess:
/// the innermost site at a primary span, at a secondary span, any site on the line of a
/// primary span, and finally every site of the file.
fn blame_error(
    error: &CompileError,
    files: &[SourceFile],
    copy: &Path,
    disabled: &HashSet<u32>,
) -> Vec<u32> {
    let enabled = |id: u32| !disabled.contains(&id);
    let file_of = |span: &ErrorSpan| {
        let path = copy.join(&span.file);
        files.iter().find(|f| f.path == path)
    };
    let innermost = |spans: &[ErrorSpan]| -> Vec<u32> {
        let mut ids = vec![];
        for span in spans {
            if let Some(file) = file_of(span) {
                let rendered = file.rendered.as_ref().expect("rendered before building");
                // Inside the text of a probe, that probe is the one that does not fit.
                match rendered
                    .inserted_by(span.byte_start)
                    .filter(|id| enabled(*id))
                {
                    Some(id) => ids.push(id),
                    None => ids.extend(instrument::blame(
                        &file.sites,
                        &enabled,
                        rendered.to_original(span.byte_start),
                    )),
                }
            }
        }
        ids
    };
    let widen = |lines: usize| -> Vec<u32> {
        let mut ids = vec![];
        for span in &error.primary {
            if let Some(file) = file_of(span) {
                ids.extend(
                    file.sites
                        .iter()
                        .filter(|s| enabled(s.id) && s.line.abs_diff(span.line) <= lines)
                        .map(|s| s.id),
                );
            }
        }
        ids
    };
    [
        innermost(&error.primary),
        innermost(&error.secondary),
        widen(0),
        widen(NEARBY_LINES),
        widen(usize::MAX),
    ]
    .into_iter()
    .find(|ids| !ids.is_empty())
    .unwrap_or_default()
}

/// Range of `D = lhs - rhs` that the operand types allow, when one side is a literal.
fn domain_of(
    literal: LiteralSide,
    bounds: Option<&crate::trace::TypeBounds>,
) -> Option<(i128, i128)> {
    let bounds = bounds?;
    match literal {
        LiteralSide::Rhs(k) => Some((bounds.lhs.0.checked_sub(k)?, bounds.lhs.1.checked_sub(k)?)),
        LiteralSide::Lhs(k) => Some((k.checked_sub(bounds.rhs.1)?, k.checked_sub(bounds.rhs.0)?)),
        LiteralSide::None => None,
    }
}

struct Session {
    progress: bool,
    serial_timeout_rechecks: bool,
    package_dir: PathBuf,
    binaries: Vec<TestBinary>,
}

/// One mutant and the tests it has to face, per test binary.
struct Job {
    site: u32,
    mutation: Mutation,
    /// `(index of the binary, tests to run or None for all of them)`.
    tests: Vec<(usize, Option<Vec<String>>)>,
}

#[derive(Debug, Clone, PartialEq)]
struct JobResult {
    first_pass_timeout: bool,
    unconfirmed_timeout: bool,
    /// The tests the mutant passed, when it passed all of them.
    survived: Option<Vec<String>>,
    timed_out: bool,
    /// The mutant could not be run as what it is: neither found out nor passed.
    unable: bool,
}

impl report::TimeoutRechecks {
    fn include(&mut self, result: &JobResult) {
        if result.first_pass_timeout {
            self.first_pass += 1;
            if result.unconfirmed_timeout || result.unable {
                self.unconfirmed += 1;
            } else if result.timed_out {
                self.confirmed += 1;
            } else {
                self.recovered += 1;
            }
        }
    }
}

impl JobResult {
    fn progress_outcome(&self) -> progress::Outcome {
        if self.unable {
            progress::Outcome::Unable
        } else if self.timed_out {
            progress::Outcome::TimedOut
        } else if self.survived.is_some() {
            progress::Outcome::Survived
        } else {
            progress::Outcome::Failed
        }
    }
}

/// The tests of one binary whose observations satisfy `pick`.
fn tests_where<T>(
    per_test: Option<&BTreeMap<String, T>>,
    pick: impl Fn(&T) -> bool,
) -> BTreeSet<String> {
    per_test
        .into_iter()
        .flatten()
        .filter(|(_, obs)| pick(obs))
        .map(|(test, _)| test.clone())
        .collect()
}

type InfectedTests<'a> = Box<dyn Fn(&TestBinary, Mutation) -> BTreeSet<String> + 'a>;
type ReachingTests<'a> = Box<dyn Fn(&TestBinary) -> BTreeSet<String> + 'a>;

/// A predicted survivor.
struct PlannedGap {
    finding: Finding,
    /// Mutations to confirm the prediction with.
    survivors: Vec<Mutation>,
    /// The gap says nothing more than "the expression in this byte range had one value in
    /// every test". If that expression is a site of its own that already says so, the
    /// gap is not reported a second time.
    constant_expr: Option<(usize, usize)>,
}

/// The byte range of an operand without the parentheses and spaces around it.
fn core_range(start: usize, text: &str) -> (usize, usize) {
    let (mut from, mut to) = (0, text.len());
    loop {
        let inner = &text[from..to];
        let trimmed = inner.trim();
        from += inner.len() - inner.trim_start().len();
        to = from + trimmed.len();
        let wrapped = trimmed.starts_with('(') && trimmed.ends_with(')') && {
            // The first parenthesis must be the one the last closes: `(a) && (b)` is not wrapped.
            let mut depth = 0usize;
            trimmed.char_indices().all(|(i, c)| {
                match c {
                    '(' => depth += 1,
                    ')' => depth -= 1,
                    _ => {}
                }
                depth > 0 || i == trimmed.len() - 1
            })
        };
        if !wrapped {
            return (start + from, start + to);
        }
        from += 1;
        to -= 1;
    }
}

/// What the planner decided for one site.
struct SitePlan<'a> {
    /// Predicted survivors: the finding to report and the mutations to confirm it with.
    gaps: Vec<PlannedGap>,
    /// The comparison cannot have another outcome within its operand types. A function
    /// or condition built on it is constant for a reason no test can change.
    cannot_differ: bool,
    /// The expression had a single value in every test.
    constant: bool,
    /// Mutations some test observes differently.
    run: Vec<Mutation>,
    /// Tests of a binary whose execution the mutation can change.
    infected: InfectedTests<'a>,
    /// Tests of a binary that execute the site at all.
    reaching: ReachingTests<'a>,
}

enum Planned<'a> {
    Unreached,
    /// Reached, and its mutant cannot be run as a probe here: said, not passed over.
    NotEvaluated,
    /// Reached, but nothing to mutate (operands that are not numbers).
    Nothing,
    Plan(SitePlan<'a>),
}

/// A predicted survivor with the jobs that confirm it and the expression it calls constant.
type ConfirmedGap = (Finding, Vec<usize>, Option<(usize, usize)>);

/// A site with its jobs: indices into the job list.
struct SiteWork<'a> {
    file: &'a str,
    site: &'a Site,
    gaps: Vec<ConfirmedGap>,
    constant: bool,
    cannot_differ: bool,
    runs: Vec<(Mutation, usize)>,
}

impl Session {
    fn note(&self, text: &str) {
        if self.progress {
            eprintln!("tmt: {text}");
        }
    }

    fn merged<T: Default + Copy>(
        &self,
        select: impl Fn(&Trace) -> Option<&BTreeMap<String, T>>,
        merge: impl Fn(&mut T, &T),
    ) -> T {
        let mut all = T::default();
        for obs in self
            .binaries
            .iter()
            .filter_map(|b| select(&b.trace))
            .flat_map(|t| t.values())
        {
            merge(&mut all, obs);
        }
        all
    }

    fn plan<'a>(&'a self, file: &str, site: &'a Site, verify: bool) -> Planned<'a> {
        let id = site.id;
        let reach_plan = |mutation: Mutation| SitePlan {
            gaps: vec![],
            constant: false,
            cannot_differ: false,
            run: vec![mutation],
            infected: Box::new(move |bin, _| bin.trace.reach.get(&id).cloned().unwrap_or_default()),
            reaching: Box::new(move |bin| bin.trace.reach.get(&id).cloned().unwrap_or_default()),
        };
        let reached = self
            .binaries
            .iter()
            .any(|b| b.trace.reach.contains_key(&id));
        let opaque = self
            .merged(|t| t.arith.get(&id), plan::ArithObs::merge)
            .opaque;
        Planned::Plan(match &site.kind {
            SiteKind::Stmt
            | SiteKind::Neg { .. }
            | SiteKind::Field { .. }
            | SiteKind::Body { .. }
                if reached =>
            {
                reach_plan(Mutation::Delete)
            }
            SiteKind::Stmt
            | SiteKind::Neg { .. }
            | SiteKind::Field { .. }
            | SiteKind::Body { .. } => return Planned::Unreached,
            SiteKind::Guard { can_be_false } => {
                let can_be_false = *can_be_false;
                let allowed = move |m: &Mutation| can_be_false || *m != Mutation::Const(false);
                let merged = self.merged(|t| t.returns.get(&id), plan::ReturnObs::merge);
                let guard_plan = plan::plan_return(&merged);
                if guard_plan.unreached {
                    return Planned::Unreached;
                }
                let gaps = guard_plan
                    .always
                    .filter(|always| allowed(&Mutation::Const(*always)))
                    .map(|always| PlannedGap {
                        finding: report::guard_gap_finding(file, site, always, verify),
                        survivors: vec![Mutation::Const(always)],
                        // The guard is the condition it consists of.
                        constant_expr: Some((site.start, site.end)),
                    })
                    .into_iter()
                    .collect();
                SitePlan {
                    constant: false,
                    cannot_differ: false,
                    gaps,
                    run: guard_plan.run.into_iter().filter(allowed).collect(),
                    infected: Box::new(move |bin, m| {
                        tests_where(bin.trace.returns.get(&id), |obs| match m {
                            Mutation::Const(value) => obs.infected_by_const(value),
                            _ => obs.reached(),
                        })
                    }),
                    reaching: Box::new(move |bin| {
                        tests_where(bin.trace.returns.get(&id), plan::ReturnObs::reached)
                    }),
                }
            }
            SiteKind::Arm { .. } if reached => reach_plan(Mutation::Delete),
            SiteKind::Arm { .. } => return Planned::Unreached,
            SiteKind::Not { .. } if reached => reach_plan(Mutation::DropNot),
            // `a..=b` with `b` the largest value of its type is no range the probe could
            // return in place of `a..b`: the mutant would run as the original and pass.
            SiteKind::Range { .. }
                if reached
                    && self
                        .binaries
                        .iter()
                        .any(|b| b.trace.ranges_at_top.contains(&id)) =>
            {
                return Planned::NotEvaluated;
            }
            SiteKind::Range { .. } if reached => reach_plan(Mutation::RangeEnd),
            // `!` on an integer or a user type is not negation of a condition, and a range
            // of something other than integers has no "one more".
            SiteKind::Not { .. } | SiteKind::Range { .. } if opaque => return Planned::Nothing,
            SiteKind::Not { .. } | SiteKind::Range { .. } => return Planned::Unreached,
            SiteKind::Cmp { op, literal, .. } => {
                let op = *op;
                let mut merged = Obs::default();
                for bin in &self.binaries {
                    merged.merge(&bin.trace.merged(id));
                }
                let bounds = self.binaries.iter().find_map(|b| b.trace.bounds.get(&id));
                let cmp_plan = plan::plan_cmp(op, &merged, domain_of(*literal, bounds));
                let mut gaps = vec![];
                let mut constant = false;
                match &cmp_plan.gap {
                    Some(plan::InputGap::Unreached) => return Planned::Unreached,
                    Some(gap) => {
                        constant = matches!(gap, plan::InputGap::OneSided { .. });
                        if let Some(finding) =
                            report::input_gap_finding(file, site, gap, &merged, verify)
                        {
                            gaps.push(PlannedGap {
                                finding,
                                survivors: gap.survivors(),
                                constant_expr: None,
                            });
                        }
                    }
                    None => {}
                }
                SitePlan {
                    gaps,
                    constant,
                    cannot_differ: cmp_plan.cannot_differ,
                    run: cmp_plan.run,
                    infected: Box::new(move |bin, m| {
                        tests_where(bin.trace.cmp.get(&id), |obs| plan::infects(op, m, obs))
                    }),
                    reaching: Box::new(move |bin| {
                        tests_where(bin.trace.cmp.get(&id), Obs::reached)
                    }),
                }
            }
            SiteKind::Logic {
                and,
                lhs,
                rhs,
                gap: op_gap,
            } => {
                let and = *and;
                let lhs_range = core_range(site.start, lhs);
                let rhs_range = core_range(op_gap.1, rhs);
                let merged = self.merged(|t| t.logic.get(&id), plan::LogicObs::merge);
                let logic_plan = plan::plan_logic(and, &merged);
                if logic_plan.gaps.contains(&plan::LogicGap::Unreached) {
                    return Planned::Unreached;
                }
                let gaps: Vec<_> = logic_plan
                    .gaps
                    .iter()
                    .filter_map(|gap| {
                        // "The operand never decides" is the same statement as "the operand
                        // had one value whenever it was evaluated".
                        let (mutation, operand) = match gap {
                            plan::LogicGap::RhsNeverDecides => (Mutation::LogicDropRhs, rhs_range),
                            _ => (Mutation::LogicDropLhs, lhs_range),
                        };
                        Some(PlannedGap {
                            finding: report::logic_gap_finding(file, site, *gap, verify)?,
                            survivors: vec![mutation],
                            constant_expr: Some(operand),
                        })
                    })
                    .collect();
                SitePlan {
                    constant: merged.constant_value(and).is_some(),
                    cannot_differ: false,
                    gaps,
                    run: logic_plan.run,
                    infected: Box::new(move |bin, m| {
                        tests_where(bin.trace.logic.get(&id), |obs| {
                            plan::logic_infects(and, m, obs)
                        })
                    }),
                    reaching: Box::new(move |bin| {
                        tests_where(bin.trace.logic.get(&id), plan::LogicObs::reached)
                    }),
                }
            }
            SiteKind::Arith { .. } | SiteKind::MinMax { .. } => {
                // `max` / `min` are recorded like an operator and its swap: they agree
                // exactly when the operands are equal.
                let merged = self.merged(|t| t.arith.get(&id), plan::ArithObs::merge);
                let mutations: Vec<(Mutation, plan::ArithObs)> = match &site.kind {
                    SiteKind::MinMax { .. } => vec![(Mutation::MinMax, merged)],
                    SiteKind::Arith { op, assign, .. } if op.second(*assign).is_some() => vec![
                        (Mutation::ArithSwap, merged),
                        (Mutation::ArithSwap2, merged.second()),
                    ],
                    _ => vec![(Mutation::ArithSwap, merged)],
                };
                let (mut gaps, mut run) = (vec![], vec![]);
                for (mutation, seen) in mutations {
                    match plan::plan_arith(&seen) {
                        plan::ArithPlan::Unreached => return Planned::Unreached,
                        // Not numbers (`String + &str`, `Duration + Duration`): nothing to mutate.
                        plan::ArithPlan::Opaque => return Planned::Nothing,
                        plan::ArithPlan::NeverDiffers => {
                            let finding = if mutation == Mutation::MinMax {
                                report::min_max_gap_finding(file, site, verify)
                            } else {
                                report::arith_gap_finding(file, site, mutation, verify)
                            };
                            gaps.extend(finding.map(|finding| PlannedGap {
                                finding,
                                survivors: vec![mutation],
                                constant_expr: None,
                            }));
                        }
                        plan::ArithPlan::Run => run.push(mutation),
                    }
                }
                SitePlan {
                    gaps,
                    constant: false,
                    cannot_differ: false,
                    run,
                    infected: Box::new(move |bin, m| {
                        tests_where(bin.trace.arith.get(&id), |obs| obs.infected_by(m))
                    }),
                    reaching: Box::new(move |bin| {
                        tests_where(bin.trace.arith.get(&id), |obs| obs.same || obs.differs)
                    }),
                }
            }
            SiteKind::Return {
                sole_expr, option, ..
            } => {
                let sole_expr = *sole_expr;
                let is_option = option.is_some();
                let merged = self.merged(|t| t.returns.get(&id), plan::ReturnObs::merge);
                let return_plan = if is_option {
                    plan::plan_option_return(&merged)
                } else {
                    plan::plan_return(&merged)
                };
                if return_plan.unreached {
                    return Planned::Unreached;
                }
                let gaps: Vec<_> = return_plan
                    .always
                    .and_then(|always| {
                        Some(PlannedGap {
                            finding: report::return_gap_finding(file, site, always, verify)?,
                            survivors: vec![Mutation::Const(always)],
                            // A body that is one expression returns that expression's value.
                            constant_expr: sole_expr,
                        })
                    })
                    .into_iter()
                    .collect();
                SitePlan {
                    constant: false,
                    cannot_differ: false,
                    gaps,
                    run: return_plan.run,
                    infected: Box::new(move |bin, m| {
                        tests_where(bin.trace.returns.get(&id), |obs| match m {
                            Mutation::Const(value) => obs.infected_by_const(value),
                            _ => obs.reached(),
                        })
                    }),
                    reaching: Box::new(move |bin| {
                        tests_where(bin.trace.returns.get(&id), plan::ReturnObs::reached)
                    }),
                }
            }
        })
    }

    fn job(
        &self,
        site: u32,
        mutation: Mutation,
        tests_of: &dyn Fn(&TestBinary) -> BTreeSet<String>,
    ) -> Job {
        let tests = self
            .binaries
            .iter()
            .enumerate()
            .filter_map(|(index, bin)| {
                let tests = tests_of(bin);
                if tests.is_empty() {
                    return None;
                }
                let filter = match bin.runner {
                    Runner::Exe(_) => test_filter(&tests, &bin.tests),
                    Runner::Doc { .. } | Runner::Isolated { .. } | Runner::Nextest { .. } => None,
                };
                Some((index, filter))
            })
            .collect();
        Job {
            site,
            mutation,
            tests,
        }
    }

    /// Runs one mutant against its tests, stopping at the first test that fails.
    /// Runs one mutant while `sharing` jobs run at the same time.
    fn run_job(&self, job: &Job, sharing: usize) -> Result<JobResult> {
        let spec = format!("{}:{}", job.site, job.mutation.encode());
        let mut passed = vec![];
        // One failure is enough: the quick binaries first.
        let mut tests: Vec<&(usize, Option<Vec<String>>)> = job.tests.iter().collect();
        tests.sort_by_key(|(index, _)| self.binaries[*index].baseline);
        for (index, filter) in tests {
            let bin = &self.binaries[*index];
            let outcome = run_binary(
                bin,
                filter.as_deref(),
                &[("TMT_MUTANT", &spec)],
                bin.filtered_limit(filter.as_deref(), false),
                RunOutput::FirstFailure,
                sharing,
            )?;
            if outcome != Outcome::Pass {
                return Ok(JobResult {
                    first_pass_timeout: false,
                    unconfirmed_timeout: false,
                    survived: None,
                    timed_out: outcome == Outcome::Timeout,
                    unable: outcome == Outcome::Unable,
                });
            }
            match filter {
                Some(names) => passed.extend(names.iter().cloned()),
                None if matches!(bin.runner, Runner::Isolated { .. }) => {
                    passed.push(bin.name.clone())
                }
                None => passed.push(format!("all tests of `{}`", bin.name)),
            }
        }
        Ok(JobResult {
            first_pass_timeout: false,
            unconfirmed_timeout: false,
            survived: Some(passed),
            timed_out: false,
            unable: false,
        })
    }

    /// Runs a surviving mutant once more with tracing on, and returns what it changed in
    /// each test compared with the original run.
    fn trace_job(
        &self,
        job: &Job,
        scratch: &Path,
        sharing: usize,
    ) -> Result<BTreeMap<String, BTreeSet<Change>>> {
        let spec = format!("{}:{}", job.site, job.mutation.encode());
        let mut all: BTreeMap<String, BTreeSet<Change>> = BTreeMap::new();
        for (index, filter) in &job.tests {
            let bin = &self.binaries[*index];
            let path = scratch.join(format!("effects-{}-{index}.tsv", job.site));
            let _ = fs::remove_file(&path);
            let trace_env = path.to_string_lossy().into_owned();
            let outcome = run_binary(
                bin,
                filter.as_deref(),
                &[("TMT_MUTANT", &spec), ("TMT_TRACE", &trace_env)],
                bin.filtered_limit(filter.as_deref(), true),
                RunOutput::Quiet,
                sharing,
            )?;
            let text = fs::read_to_string(&path).unwrap_or_default();
            let _ = fs::remove_file(&path);
            if outcome != Outcome::Pass {
                continue; // not the run we saw before: say nothing rather than guess
            }
            let mutated = Trace::parse(&text);
            let ran: Option<BTreeSet<String>> =
                filter.as_ref().map(|names| names.iter().cloned().collect());
            for (test, found) in propagation::changes(&bin.trace, &mutated, ran.as_ref(), job.site)
            {
                all.entry(test).or_default().extend(found);
            }
        }
        Ok(all)
    }

    /// Runs every job exactly once on `workers` threads (formal/tla/JobPool.tla) and
    /// returns the results in job order.
    /// Runs the mutants, `workers` at a time. A mutant that ran out of time while others
    /// were running may only have been slowed down by them: it gets a second run on its
    /// own, and that one counts.
    fn run_jobs(&self, jobs: &[Job], workers: usize, label: &str) -> Result<Vec<JobResult>> {
        let run = |index: usize, sharing| self.run_job(&jobs[index], sharing);
        evaluate_jobs(
            jobs.len(),
            workers,
            self.serial_timeout_rechecks,
            self.progress,
            label,
            &run,
        )
    }
}

fn evaluate_jobs(
    count: usize,
    workers: usize,
    serial_rechecks: bool,
    progress_enabled: bool,
    label: &str,
    run: &(impl Fn(usize, usize) -> Result<JobResult> + Sync),
) -> Result<Vec<JobResult>> {
    let sharing = workers.clamp(1, count.max(1));
    let mut results = progress::track(progress_enabled, label, count, |progress| {
        run_pool(count, workers, |index| {
            let mut result = run(index, sharing)?;
            if sharing > 1 && result.timed_out {
                result.first_pass_timeout = true;
                result.unconfirmed_timeout = true;
            }
            if !result.unconfirmed_timeout || !serial_rechecks {
                progress.complete(result.progress_outcome());
            }
            Ok(result)
        })
    })?;
    let pending: Vec<_> = results
        .iter()
        .enumerate()
        .filter_map(|(index, result)| result.unconfirmed_timeout.then_some(index))
        .collect();
    if !pending.is_empty() && progress_enabled {
        eprintln!(
            "tmt: {label}: {} first-pass timeouts, {} pending serial rechecks",
            pending.len(),
            if serial_rechecks { pending.len() } else { 0 }
        );
    }
    if serial_rechecks {
        progress::track_timeout_rechecks(progress_enabled, pending.len(), |progress| {
            for index in pending {
                let mut confirmed = run(index, 1)?;
                confirmed.first_pass_timeout = true;
                let outcome = if confirmed.timed_out {
                    progress::Outcome::TimedOut
                } else if confirmed.unable {
                    progress::Outcome::Unable
                } else {
                    progress::Outcome::Survived
                };
                results[index] = confirmed;
                progress.complete(outcome);
            }
            Ok::<_, anyhow::Error>(())
        })?;
    }
    Ok(results)
}

/// Calls `work(i)` once for every `i < count`, on up to `workers` threads, and returns the
/// results in index order. The first error wins.
fn run_pool<T: Send>(
    count: usize,
    workers: usize,
    work: impl Fn(usize) -> Result<T> + Sync,
) -> Result<Vec<T>> {
    let next = std::sync::atomic::AtomicUsize::new(0);
    let done: Vec<Vec<(usize, Result<T>)>> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers.clamp(1, count.max(1)))
            .map(|_| {
                scope.spawn(|| {
                    let mut mine = vec![];
                    loop {
                        // Claiming is one atomic step: no two workers get the same index.
                        let index = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        if index >= count {
                            return mine;
                        }
                        mine.push((index, work(index)));
                    }
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("worker panicked"))
            .collect()
    });
    let mut slots: Vec<Option<T>> = (0..count).map(|_| None).collect();
    for (index, result) in done.into_iter().flatten() {
        slots[index] = Some(result?);
    }
    Ok(slots
        .into_iter()
        .map(|slot| slot.expect("every job ran"))
        .collect())
}

impl std::str::FromStr for report::Shard {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let (index, count) = text.split_once('/').context("expected a shard index K/N")?;
        let index: usize = index.parse().context("invalid shard index")?;
        let count: usize = count.parse().context("invalid shard count")?;
        if index >= count {
            bail!("shard index must be less than the shard count");
        }
        Ok(Self {
            index,
            count,
            round_robin: false,
        })
    }
}

impl report::Shard {
    fn contains(self, candidate: usize, candidates: usize) -> bool {
        candidate < candidates
            && (if self.round_robin {
                candidate % self.count
            } else {
                candidate / candidates.div_ceil(self.count)
            }) == self.index
    }
}

fn cargo_metadata(dir: &Path, cargo: &CargoConfig) -> Result<Metadata> {
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .args(cargo.flags())
        .current_dir(dir)
        .output()
        .context("running cargo metadata")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    serde_json::from_slice(&output.stdout).context("parsing cargo metadata")
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

pub fn run(options: &Options) -> Result<Report> {
    run_package(options, None, None)
}

fn run_package(
    options: &Options,
    batch: Option<&WorkspaceBatch<'_>>,
    workspace_shard: Option<WorkspaceShard>,
) -> Result<Report> {
    if let Some(shard) = options.shard
        && (!options.source_only || shard.index >= shard.count)
    {
        bail!("shards require --source-only and a valid shard index");
    }
    let mut seen_errors = HashSet::new();
    let mut error_values = vec![];
    for expression in &options.error_values {
        let expression = expression.trim();
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
    let _lock = if catalogue.is_none() {
        Some(WorkLock::take(&work)?)
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
    let skip =
        |path: &Path| forbidden(path) || path.file_name().is_some_and(|n| n == "node_modules");
    let state = WorkCopy::open(&work, &copy);
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
        for relative in &options.copy_fixtures {
            use std::path::Component;
            let relative = relative
                .components()
                .filter(|c| *c != Component::CurDir)
                .collect::<PathBuf>();
            if relative.as_os_str().is_empty()
                || relative
                    .components()
                    .any(|c| !matches!(c, Component::Normal(_)))
            {
                bail!(
                    "--copy-fixture must name a file or directory inside the workspace: {}",
                    relative.display()
                );
            }
            copy_fixture_entry(
                &workspace.join(&relative),
                &copy.join(&relative),
                &workspace,
                &forbidden,
                &state,
                &mut BTreeSet::new(),
            )?;
        }
        prune(&copy, &state)?;
        if options.git_index {
            git_index::snapshot(&workspace, &copy)?;
        }
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

        if let Some(catalogue) = catalogue {
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
    let paths = source_graph(roots, &copy, &state, &workspace)?;
    let scope = requested_scope(options, &workspace, &package_dir, &copy)?;
    let mut skipped = vec![];
    let mut files = vec![];
    let mut next_id = 0u32;
    for (path, owners) in paths {
        let Some(original) = state.original(&path, &workspace)? else {
            continue; // left by an earlier run; gone or rewritten before the build
        };
        let display = relative_source(&path, &package_dir).display().to_string();
        match instrument::discover_with_errors(&original, next_id, &error_values) {
            Ok(mut found) => {
                next_id += found.sites.len() as u32;
                if owners.consumers.len() != 1 || !owners.consumers.contains(&root_file) {
                    found.rebuild_sites.append(&mut found.sites);
                }
                let places = found
                    .sites
                    .iter()
                    .chain(&found.rebuild_sites)
                    .map(|site| {
                        let at = report::location("", site);
                        (at.line, at.column, at.code)
                    })
                    .collect();
                files.push(SourceFile {
                    display,
                    path,
                    owners: owners.production,
                    original,
                    places,
                    sites: found.sites,
                    rebuild_sites: found.rebuild_sites,
                    rendered: None,
                });
            }
            Err(err) => {
                let relative = path.strip_prefix(&copy).unwrap_or(&path).to_string_lossy();
                if scope
                    .as_ref()
                    .is_none_or(|scope| scope.touches(&relative, 1, usize::MAX))
                {
                    let diagnostic = format!("{err:#}");
                    note(&format!("skipping {display}: {diagnostic}"));
                    skipped.push(report::SkippedSite::new(
                        report::Location {
                            file: display,
                            line: 1,
                            column: 1,
                            ..Default::default()
                        },
                        &original,
                        original.len(),
                        report::SkipReason::UnparseableSource,
                        Some(diagnostic),
                    ));
                }
            }
        }
    }

    // Only the requested lines, if any were requested.
    if let Some(scope) = &scope {
        for file in &mut files {
            let relative = file
                .path
                .strip_prefix(&copy)
                .unwrap_or(&file.path)
                .to_string_lossy()
                .into_owned();
            let original = &file.original;
            let in_scope = |site: &Site| {
                scope.touches(&relative, scope::line_of(original, site.start), scope::line_of(original, site.end))
                    // A function is in scope by its signature line too.
                    || scope.touches(&relative, site.line, site.line)
            };
            file.sites.retain(in_scope);
            file.rebuild_sites.retain(in_scope);
        }
        files.retain(|f| !f.sites.is_empty() || !f.rebuild_sites.is_empty() || f.path == root_file);
    }
    let mut witness_candidates = vec![];
    if (options.list || options.rebuild || options.source_only)
        && root_target.test_build_arg() == Some(("--lib", None))
        && let Some(file) = files.iter_mut().find(|file| file.path == root_file)
    {
        let relative = file
            .path
            .strip_prefix(&copy)
            .unwrap_or(&file.path)
            .to_string_lossy();
        witness_candidates = witness::discover(&file.original);
        let locations: Vec<_> = witness_candidates
            .iter()
            .filter(|candidate| !candidate.existing)
            .map(|candidate| {
                let at = witness_location(file, candidate);
                (at.line, at.column, at.code)
            })
            .collect();
        file.places.extend(locations);
        witness_candidates.retain(|candidate| {
            scope.as_ref().is_none_or(|scope| {
                let site_in_scope = if candidate.edits.len() == 1 {
                    scope.touches(&relative, *candidate.scope.start(), *candidate.scope.end())
                } else {
                    candidate.edits.iter().any(|edit| {
                        scope.touches(
                            &relative,
                            scope::line_of(&file.original, edit.span.start),
                            scope::line_of(&file.original, edit.span.end),
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
    }
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
        && !relative.as_os_str().is_empty()
    {
        report_path = format!("{}/", relative.to_string_lossy().replace('\\', "/"));
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
        },
        skipped,
        ..Report::default()
    };
    if options.list {
        state.settle(&workspace)?;
        let mut candidates: Vec<_> = files
            .iter()
            .flat_map(|file| {
                file.sites
                    .iter()
                    .map(|site| (site, false))
                    .chain(file.rebuild_sites.iter().map(|site| (site, true)))
                    .flat_map(move |(site, requires_rebuild)| {
                        source_mutants(file, site, true)
                            .into_iter()
                            .map(move |mutant| report::Candidate {
                                what: mutant.what,
                                into: mutant.into,
                                requires_rebuild,
                                edits: vec![candidate_edit(
                                    file,
                                    mutant.start,
                                    mutant.end,
                                    mutant.replacement,
                                )],
                            })
                    })
            })
            .collect();
        let mut index = 0;
        candidates.retain(|_| {
            let selected = in_shard(index);
            index += 1;
            selected
        });
        if let Some(root) = files.iter().find(|file| file.path == root_file) {
            candidates.extend(
                witness_candidates
                    .iter()
                    .filter(|candidate| !candidate.existing)
                    .map(|candidate| report::Candidate {
                        what: "witness".into(),
                        into: candidate.description.clone(),
                        requires_rebuild: true,
                        edits: candidate
                            .edits
                            .iter()
                            .map(|edit| {
                                candidate_edit(
                                    root,
                                    edit.span.start,
                                    edit.span.end,
                                    edit.replacement.clone(),
                                )
                            })
                            .collect(),
                    }),
            );
        }
        report.mutants = Some(candidates);
        return Ok(report);
    }
    if empty_shard || (scope.is_some() && total_sites == 0) {
        // Restore files changed by a previous run before reporting a selection with no work.
        state.settle(&workspace)?;
        if let Some(path) = &options.baseline {
            crate::baseline::load(path)?;
        }
        if let Some(path) = &options.write_baseline {
            crate::baseline::write(path, &[])?;
        }
        report.record_evaluation();
        return Ok(report);
    }

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
    let build = loop {
        for file in &mut files {
            let rendered =
                instrument::render(&file.original, &file.sites, &|id| !disabled.contains(&id));
            let mut text = rendered.text.clone();
            if file.path == root_file {
                text.push_str(&root_suffix);
            }
            state.write(&file.path, &text)?;
            file.rendered = Some(rendered);
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
        disabled.extend(blamed_now);
        if disabled.len() == before || rounds > MAX_REPAIR_ROUNDS {
            bail!(
                "the crate does not build after instrumentation.\n\
                 {build_note}Does `cargo test --no-run` pass on the original for the same targets?\n{}",
                diagnostics()
            );
        }
    };
    let built_sources = BuiltSources::from_artifacts(&build.artifacts, &copy);
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
            if path.is_absolute()
                || path
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                bail!(
                    "--test-wrapper must be a path inside the workspace: {}",
                    path.display()
                );
            }
            let copied = copy.join(path);
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
                let all = list_tests(&exe, &cwd, &work, false, true)?;
                let ignored = list_tests(&exe, &cwd, &work, true, true)?;
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
                    tests: BTreeSet::new(),
                    runner,
                    baseline: Duration::ZERO,
                    traced: Duration::ZERO,
                    timeout: options.timeout_seconds.map(Duration::from_secs),
                    wrapper_grace: options.wrapper_grace,
                    report_slow: options.progress,
                    filtered_timings: Default::default(),
                    trace: Trace::default(),
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
                // Recording slows code in loops down a lot, and the time limit of every mutant
                // is a multiple of this: where it matters, time the tests as the mutants run them.
                if !options.source_only && bin.traced > SLOW_TRACE {
                    let started = Instant::now();
                    if run_baseline(
                        &bin,
                        &[],
                        bin.traced,
                        &work.join(format!("baseline-{index}-untraced.log")),
                        true,
                    )? {
                        bin.baseline = started.elapsed();
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
                    bin.tests = list_tests(exe, &bin.cwd, &work, false, false)?;
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
    report.analysis.test_selection = selected_tests;
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
                    &file.original,
                    site.end,
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
                    &file.original,
                    site.end,
                    report::SkipReason::RequiresSourceRebuild,
                    None,
                )
            }));
        }
    }
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
            let plan = match session.plan(&file.display, site, options.verify) {
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
    let mut results = session.run_jobs(&jobs, options.jobs, "mutants")?;
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
        for (job, again) in
            chosen
                .iter()
                .zip(session.run_jobs(&against_all, options.jobs, "audit mutants")?)
        {
            report.timeout_rechecks.include(&again);
            if again.survived.is_none() {
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
    let unable = results.iter().filter(|r| r.unable).count();
    not_evaluated.extend(
        jobs.iter()
            .zip(&results)
            .filter(|(_, result)| result.unable)
            .map(|(job, _)| job.site),
    );
    if !options.rebuild {
        report.mutants_not_evaluated = not_evaluated.len();
    }
    report.mutants_run = results.len() - unable;
    report.mutants_killed = results.iter().filter(|r| r.survived.is_none()).count() - unable;
    report.mutants_timed_out = results.iter().filter(|r| r.timed_out).count();

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
            if confirm.iter().any(|j| results[*j].survived.is_none()) {
                report.unstable.push(at.clone());
            } else if !repeats {
                report.findings.push(finding.clone());
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
            if let Some(tests) = &results[*job].survived {
                survivors.push(report::describe(item.site, *mutation));
                passing_tests.extend(tests.iter().cloned());
            }
        }
        if !survivors.is_empty() {
            // The first survivor is traced below to see what it changes.
            let first = item
                .runs
                .iter()
                .find(|(_, job)| results[*job].survived.is_some());
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
    for file in &files {
        // Tried again as the source says them: what did not compile as a probe, and
        // what a probe could not be (a range that ends at the top of its type).
        let retried = file.sites.iter().filter(|site| {
            (disabled.contains(&site.id) && !matches!(site.kind, SiteKind::Body { .. }))
                || not_evaluated.contains(&site.id)
        });
        for site in file.rebuild_sites.iter().chain(retried) {
            built.extend(
                source_mutants(file, site, options.source_only)
                    .into_iter()
                    .map(|mutant| (file, site, mutant, None)),
            );
        }
    }
    if options.shard.is_some() {
        let mut index = 0;
        built.retain(|_| {
            let selected = in_shard(index);
            index += 1;
            selected
        });
    }
    if !options.rebuild && !options.source_only {
        report.mutants_not_built = built.len();
        built.clear();
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
                let job_of = |mutation: Mutation| {
                    jobs.iter()
                        .position(|job| job.site == site.id && job.mutation == mutation)
                };
                let mut compared = vec![];
                for mutant in instrument::source_mutants(site) {
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
    if !built.is_empty() {
        session.note(&format!(
            "building {} mutants one by one (code a probe cannot be put in, or to compare with the probe)",
            built.len()
        ));
        let instrumented = |file: &SourceFile, text: &str| {
            if file.path == root_file {
                format!("{text}{root_suffix}")
            } else {
                text.to_string()
            }
        };
        // All tests are run against each of them; the quick ones first, since one
        // failure is enough.
        let mut skipped_targets = BTreeSet::new();
        let mut survivors: Vec<(report::Location, Vec<String>)> = vec![];
        progress::track(
            options.progress,
            "source mutants",
            built.len(),
            |progress| -> Result<()> {
                for (file, site, mutant, twin) in &built {
                    if !built_sources.contains(file) {
                        if twin.is_none() {
                            report.mutants_not_evaluated += 1;
                            if skipped_targets.insert((&file.path, site.start, site.end)) {
                                report.skipped.push(report::SkippedSite::new(
                                    report::location(&file.display, site),
                                    &file.original,
                                    site.end,
                                    report::SkipReason::SourceNotBuilt,
                                    None,
                                ));
                            }
                        } else if let Some(audit) = report.audit.as_mut() {
                            audit.not_compared += 1;
                        }
                        progress.complete(progress::Outcome::Unable);
                        continue;
                    }
                    let mutated = format!(
                        "{}{}{}",
                        &file.original[..mutant.start],
                        mutant.replacement,
                        &file.original[mutant.end..]
                    );
                    // `None`: it does not build. Otherwise what stopped it, if anything did.
                    let attempt = || -> Result<Option<Outcome>> {
                        state.write(&file.path, &instrumented(file, &mutated))?;
                        if !cargo_build()?.status.success() {
                            return Ok(None);
                        }
                        run_source_tests(&by_speed).map(Some)
                    };
                    let attempted = attempt();
                    // The file as it was, before another one is changed and whatever went wrong.
                    let rendered = file.rendered.as_ref().expect("rendered before building");
                    state.write(&file.path, &instrumented(file, &rendered.text))?;
                    let outcome = attempted?;
                    progress.complete(outcome.map_or(progress::Outcome::Unable, Into::into));
                    if let Some(job) = twin {
                        // Only compared: the probe's run is the one that is counted and reported.
                        let as_probe = results[*job].survived.is_some();
                        let Some(audit) = report.audit.as_mut() else {
                            continue;
                        };
                        // What does not build, and what the probe could not be, says nothing.
                        let comparable = outcome.filter(|_| !results[*job].unable);
                        match comparable {
                            Some(_) => audit.compared += 1,
                            None => audit.not_compared += 1,
                        }
                        if comparable.is_some_and(|built| (built == Outcome::Pass) != as_probe) {
                            let verdict = |passes: bool| if passes { "passes" } else { "fails" };
                            audit.source_disagreements.push(report::SourceDisagreement {
                                at: report::location(&file.display, site),
                                mutant: report::describe(site, mutation_of(site, mutant)),
                                as_probe: verdict(as_probe).to_string(),
                                built: verdict(!as_probe).to_string(),
                            });
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
                        let text = match (&site.kind, mutant.replacement.is_empty()) {
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
        report.findings.extend(
            survivors
                .into_iter()
                .map(|(at, survivors)| Finding::Survivor {
                    at,
                    survivors,
                    witness: None,
                }),
        );
    }

    if !witness_candidates.is_empty() {
        let file = files
            .iter()
            .find(|file| file.path == root_file)
            .expect("witness candidates in the root file");
        let rendered = file.rendered.as_ref().expect("rendered before building");
        let restore = format!("{}{root_suffix}", rendered.text);
        // These builds replace every probe in the library root with the source.
        // Keep the runtime only if another file in this build still contains probes.
        let witness_suffix = if files.iter().any(|other| {
            other.path != root_file
                && other
                    .rendered
                    .as_ref()
                    .is_some_and(|r| r.text != other.original)
        }) {
            root_suffix.as_str()
        } else {
            ""
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
                Runner::Exe(exe) => Some(list_tests(exe, &bin.cwd, &bin.git_ceiling, true, false)),
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
                        Runner::Exe(_) => {
                            bin.tests.contains(required) && !ignored.contains(required)
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
            state.write(&root_file, &format!("{source}{witness_suffix}"))?;
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
            )
        };
        let mut survivors = vec![];
        progress::track(
            options.progress,
            "source witness mutants",
            witness_candidates.len(),
            |progress| -> Result<()> {
                for candidate in &witness_candidates {
                    let at = witness_location(file, candidate);
                    let existing = candidate
                        .existing
                        .then(|| {
                            report.findings.iter().position(|finding| {
                                let location = finding.at();
                                location.file == at.file
                                    && location.line == at.line
                                    && location.column == at.column
                                    && finding.survivors().contains(&candidate.description)
                            })
                        })
                        .flatten();
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
                        progress.complete(progress::Outcome::Unable);
                        continue;
                    }
                    // A compound survivor is interesting only if each component fails on
                    // its own. Test the source edits without any generated witness tests.
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
                    state.write(&root_file, &restore)?;
                    let outcome = match attempted? {
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

        let originals = shared_witnesses(
            &file.original,
            &survivors,
            wrapper.as_deref(),
            compile_observer,
            note,
        );
        if !survivors.is_empty() {
            state.write(&root_file, &restore)?;
        }
        let originals = originals?;

        progress::track(
            options.progress,
            "distinguishing tests",
            survivors.len(),
            |progress| -> Result<()> {
                for (survivor, original) in survivors.into_iter().zip(originals) {
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
                                    )
                                    .ok()
                                    .flatten()
                                    && mutant.equals_expected == Some(false)
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
                    state.write(&root_file, &restore)?;
                    progress.complete(Outcome::Pass.into());
                    if let Some(index) = survivor.existing {
                        if let Some(evidence) = evidence {
                            report.findings[index].set_witness(evidence);
                        }
                    } else {
                        report.findings.push(Finding::Survivor {
                            at: survivor.at,
                            survivors: vec![survivor.candidate.description.clone()],
                            witness: evidence,
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
            places: &file.places,
        })
        .collect();
    name_findings(&mut report.findings, &places);

    // 8. Findings the user has already dealt with.
    let ignored: BTreeMap<&str, BTreeMap<usize, String>> = files
        .iter()
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
    let location = |finding: &Finding| (finding.at().file.clone(), finding.at().line);
    let before = report.findings.len();
    report.findings.retain(|finding| {
        let (file, line) = location(finding);
        !ignored
            .get(file.as_str())
            .is_some_and(|lines| lines.contains_key(&line))
    });
    if let Some(path) = &options.write_baseline {
        crate::baseline::write(path, &report.findings)?;
    }
    report
        .findings
        .retain(|finding| !known.as_ref().is_some_and(|known| known.knows(finding)));
    report.suppressed = before - report.findings.len();
    Ok(report)
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
    /// Every site of the file as (line, column, code), whatever part of it was analysed.
    places: &'a [(usize, usize, String)],
}

/// Gives every finding the item it is in and its name (`report::finding_id`). Which of
/// several places with the same code in one item a finding is at is counted over the
/// sites of the file, not over the findings: a finding keeps its number when the one
/// before it is closed.
fn name_findings(findings: &mut [Finding], files: &[FilePlaces]) {
    for file in files {
        let items = instrument::named_items(file.source);
        let item_at = |line: usize, column: usize| -> String {
            instrument::offset_of(file.source, line, column)
                .map_or("", |offset| instrument::enclosing_item(&items, offset))
                .to_string()
        };
        // Every place of the file a finding can be at: where, in which item, on which code.
        let places: BTreeSet<(String, &str, usize, usize)> = file
            .places
            .iter()
            .map(|(line, column, code)| (item_at(*line, *column), code.as_str(), *line, *column))
            .collect();
        for finding in findings.iter_mut() {
            let kind = finding.kind();
            let at = finding.at_mut();
            if at.file != file.display {
                continue;
            }
            at.item = item_at(at.line, at.column);
            let nth = places
                .iter()
                .filter(|(item, code, line, column)| {
                    *item == at.item && *code == at.code && (*line, *column) < (at.line, at.column)
                })
                .count();
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
            "with the mutant, these tests still reach the same code with the same outcomes: they do not use its result, or it is the same for their inputs".to_string(),
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
mod tests {
    use super::*;
    use crate::trace::UNKNOWN_TEST;

    #[test]
    fn failed_build_retains_spanless_and_source_diagnostics() {
        let stdout = r#"
{"reason":"compiler-message","message":{"level":"error","message":"linking with cc failed","rendered":"ld: library not found for -liconv","spans":[]}}
{"reason":"compiler-message","message":{"level":"error","message":"source error","rendered":null,"spans":[{"file_name":"src/lib.rs","byte_start":1,"line_start":2,"is_primary":true}]}}
"#;
        let failed = parse_build_output(stdout, &["p"]);
        assert_eq!(failed.errors.len(), 1);
        let text = failed.diagnostics.join("\n");
        assert!(text.contains("ld: library not found for -liconv"), "{text}");
        assert!(text.contains("source error"), "{text}");
    }

    #[test]
    fn test_build_scope_uses_only_known_local_features() {
        let package = Package {
            name: "pkg".to_owned(),
            dependencies: vec![],
            features: BTreeMap::from([
                ("default".to_owned(), vec!["alias".to_owned()]),
                (
                    "alias".to_owned(),
                    vec!["extra".to_owned(), "dep/extra".to_owned()],
                ),
                ("extra".to_owned(), vec![]),
                ("separate".to_owned(), vec![]),
            ]),
            manifest_path: PathBuf::from("workspace/pkg/Cargo.toml"),
            targets: vec![],
        };
        let enabled = |workspace: &str, features: &str, no_default_features, all_features| {
            package.enabled_local_features(
                Path::new(workspace),
                &CargoConfig {
                    features: vec![features.to_owned()],
                    no_default_features,
                    all_features,
                    locked: false,
                },
            )
        };
        let root = "workspace/pkg";
        assert_eq!(
            enabled(root, "", false, false),
            BTreeSet::from(["default", "alias", "extra"])
        );
        assert_eq!(
            enabled(root, "", true, true),
            BTreeSet::from(["default", "alias", "extra", "separate"])
        );
        assert_eq!(
            enabled(root, "alias, separate", true, false),
            BTreeSet::from(["alias", "extra", "separate"])
        );
        assert_eq!(
            enabled("workspace", "extra other/separate", true, false),
            BTreeSet::new()
        );
        assert_eq!(
            enabled("workspace", "pkg/extra other/separate", true, false),
            BTreeSet::from(["extra"])
        );
    }

    #[test]
    fn test_build_scope_preserves_cargo_target_eligibility() {
        let target = |name, kind, test, required_features: &[&str]| {
            serde_json::from_value(serde_json::json!({
                "name": name, "kind": [kind], "test": test, "src_path": "ignored.rs",
                "required-features": required_features,
            }))
            .unwrap()
        };
        let package = |name: &str, targets| Package {
            name: name.to_owned(),
            dependencies: vec![],
            targets,
            manifest_path: PathBuf::new(),
            features: BTreeMap::from([("extra".to_owned(), vec![])]),
        };
        let first = package(
            "first",
            vec![
                target("first", "proc-macro", true, &[]),
                target("name", "bin", true, &[]),
                target("name", "example", true, &[]),
                target("name", "bench", true, &[]),
                target("name", "test", true, &[]),
                target("conditional", "test", true, &["extra"]),
                target("dependency_feature", "test", true, &["dep/extra"]),
                target("disabled", "test", false, &[]),
                target("glob[a]", "test", true, &[]),
                target("--leading-dash", "test", true, &[]),
                target("build-script-build", "custom-build", false, &[]),
            ],
        );
        let second = package(
            "second",
            vec![
                target("second", "staticlib", true, &[]),
                target("name", "test", false, &[]),
            ],
        );
        let flags = |packages: &[&Package], names: &[&str], all| {
            test_build_flags(
                packages,
                &names.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                Path::new("workspace"),
                &CargoConfig {
                    all_features: all,
                    ..CargoConfig::default()
                },
            )
        };
        assert_eq!(
            flags(&[&first], &["name", "first", "unknown"], false).unwrap(),
            [
                "--bench=name",
                "--bin=name",
                "--example=name",
                "--lib",
                "--test=name"
            ]
        );
        assert_eq!(flags(&[&first], &[], false), None);
        assert_eq!(flags(&[&first], &["unknown", "disabled"], false), None);
        assert_eq!(flags(&[&first], &["glob[a]"], false), None);
        assert_eq!(
            flags(&[&first], &["--leading-dash"], false).unwrap(),
            ["--test=--leading-dash"]
        );
        assert_eq!(flags(&[&first], &["name", "conditional"], false), None);
        assert_eq!(flags(&[&first], &["dependency_feature"], true), None);
        assert_eq!(
            flags(&[&first], &["conditional"], true).unwrap(),
            ["--test=conditional"]
        );
        assert_eq!(flags(&[&first, &second], &["name"], false), None);
        assert_eq!(flags(&[&first, &second], &["first"], false), None);
        assert_eq!(
            flags(&[&first, &second], &["first", "second"], false).unwrap(),
            ["--lib"]
        );
    }

    #[test]
    fn parses_cargo_json_messages() {
        let stdout = concat!(
            r#"{"reason":"compiler-artifact","package_id":"p 0.1.0","target":{"name":"p","kind":["lib"]},"profile":{"test":false},"executable":null}"#,
            "\n",
            r#"{"reason":"compiler-artifact","package_id":"p 0.1.0","target":{"name":"p","kind":["lib"]},"profile":{"test":true},"executable":"/t/p-abc"}"#,
            "\n",
            r#"{"reason":"compiler-artifact","package_id":"dep 1.0.0","target":{"name":"dep","kind":["lib"]},"profile":{"test":true},"executable":"/t/dep-abc"}"#,
            "\n",
            r#"{"reason":"compiler-message","message":{"level":"warning","rendered":"w","spans":[{"file_name":"src/lib.rs","byte_start":1,"line_start":1,"is_primary":true}]}}"#,
            "\n",
            r#"{"reason":"compiler-message","message":{"level":"error","rendered":"E0308","spans":[{"file_name":"src/lib.rs","byte_start":40,"line_start":3,"is_primary":true},{"file_name":"src/lib.rs","byte_start":2,"line_start":1,"is_primary":false}]}}"#,
            "\n",
            r#"{"reason":"compiler-message","message":{"level":"error","rendered":"E0308","spans":[{"file_name":"src/lib.rs","byte_start":40,"line_start":3,"is_primary":true},{"file_name":"src/lib.rs","byte_start":2,"line_start":1,"is_primary":false}]}}"#,
            "\n",
            r#"{"reason":"compiler-message","message":{"level":"error","rendered":"aborting","spans":[]}}"#,
            "\n",
            "not json\n",
            r#"{"reason":"build-finished","success":false}"#,
            "\n",
        );
        let out = parse_build_output(stdout, &["p", "other"]);
        assert_eq!(
            out.test_binaries,
            [(
                "p".to_string(),
                PathBuf::from("/t/p-abc"),
                "p".to_string(),
                vec!["lib".to_string()]
            )]
        );
        // With several packages each binary is attributed to its own.
        let both = parse_build_output(stdout, &["dep", "p"]);
        let owners: Vec<&str> = both.test_binaries.iter().map(|b| b.2.as_str()).collect();
        assert_eq!(owners, ["p", "dep"]);
        assert_eq!(
            out.errors,
            [CompileError {
                primary: vec![ErrorSpan {
                    file: "src/lib.rs".into(),
                    byte_start: 40,
                    line: 3
                }],
                secondary: vec![ErrorSpan {
                    file: "src/lib.rs".into(),
                    byte_start: 2,
                    line: 1
                }],
                rendered: "E0308".into(),
            }]
        );
    }

    #[test]
    fn package_ids_are_matched_by_name() {
        assert!(is_package(
            "path+file:///w/target/tmt/src#eligibility@0.0.0",
            "eligibility"
        ));
        assert!(is_package(
            "path+file:///w/eligibility#0.1.0",
            "eligibility"
        ));
        assert!(is_package(
            "eligibility 0.1.0 (path+file:///w)",
            "eligibility"
        ));
        assert!(!is_package(
            "path+file:///w/eligibility#other@0.1.0",
            "eligibility-core"
        ));
        assert!(!is_package(
            "registry+https://github.com/rust-lang/crates.io-index#serde@1.0.0",
            "eligibility"
        ));
    }

    #[test]
    fn test_filter_falls_back_to_the_whole_binary() {
        let set = |names: &[&str]| {
            names
                .iter()
                .map(|n| n.to_string())
                .collect::<BTreeSet<String>>()
        };
        let known = set(&["tests::a", "tests::b", "tests::c"]);
        assert_eq!(
            test_filter(&set(&["tests::a", "tests::b"]), &known),
            Some(vec!["tests::a".to_string(), "tests::b".to_string()])
        );
        // Observed outside any test thread.
        assert_eq!(test_filter(&set(&["tests::a", UNKNOWN_TEST]), &known), None);
        // Observed on a thread the test named itself: not a test of this binary.
        assert_eq!(test_filter(&set(&["tests::a", "worker"]), &known), None);
        assert_eq!(test_filter(&set(&["worker"]), &known), None);

        let many: BTreeSet<String> = (0..=MAX_TEST_FILTERS).map(|i| format!("t{i}")).collect();
        assert_eq!(test_filter(&many, &many), None);
        let limit: BTreeSet<String> = (0..MAX_TEST_FILTERS).map(|i| format!("t{i}")).collect();
        assert!(test_filter(&limit, &limit).is_some());
    }

    #[test]
    fn libtest_listing() {
        let listing =
            "tests::a: test\ntests::nested::b: test\nbench_it: benchmark\n\n2 tests, 1 benchmark\n";
        assert_eq!(
            parse_test_list(listing).into_iter().collect::<Vec<_>>(),
            ["tests::a", "tests::nested::b"]
        );
    }

    #[test]
    fn domains_from_type_bounds() {
        let u8s = crate::trace::TypeBounds {
            lhs: (0, 255),
            rhs: (0, 255),
        };
        assert_eq!(
            domain_of(LiteralSide::Rhs(18), Some(&u8s)),
            Some((-18, 237))
        );
        assert_eq!(
            domain_of(LiteralSide::Lhs(18), Some(&u8s)),
            Some((-237, 18))
        );
        assert_eq!(domain_of(LiteralSide::None, Some(&u8s)), None);
        assert_eq!(domain_of(LiteralSide::Rhs(18), None), None);
        let wide = crate::trace::TypeBounds {
            lhs: (i128::MIN, 0),
            rhs: (0, 0),
        };
        assert_eq!(
            domain_of(LiteralSide::Rhs(1), Some(&wide)),
            None,
            "overflow is not a domain"
        );
    }

    #[test]
    fn out_of_line_module_locations() {
        let file = Path::new("src/cli.rs");
        let statements = "mod child; #[path = \"spec/support.rs\"] mod helper;";
        let roots = crate::modules::external(statements, file, Path::new("src"), true);
        assert_eq!(
            roots[0].paths,
            [
                PathBuf::from("src/child.rs"),
                PathBuf::from("src/child/mod.rs")
            ]
        );
        assert_eq!(roots[1].paths, [PathBuf::from("src/spec/support.rs")]);
        let module = crate::modules::external(statements, file, Path::new("src/cli"), true);
        assert_eq!(module[0].paths[0], PathBuf::from("src/cli/child.rs"));
        assert_eq!(module[1].paths[0], PathBuf::from("src/spec/support.rs"));
    }

    #[test]
    fn path_dependencies_outside_the_workspace_become_absolute() {
        let manifest = r#"[package]
name = "app"

[dependencies]
serde = "1"
core = { path = "../core" }          # inside the workspace: kept
outside = { path = "../../outside", version = "1" }

[dev-dependencies.helper]
path = "../../../far/helper"

[target.'cfg(unix)'.dependencies]
unix-only = { path = "../../unix" }

[patch.crates-io]
serde = { path = "../../serde-fork" }
"#;
        let out = absolutize_path_dependencies(
            manifest,
            Path::new("/w/ws/app"),
            Path::new("/w/ws"),
            Path::new("/copy"),
        )
        .unwrap()
        .expect("something to rewrite");
        assert!(
            out.contains(r#"core = { path = "../core" }          # inside the workspace: kept"#),
            "{out}"
        );
        assert!(
            out.contains(r#"outside = { path = "/w/outside", version = "1" }"#),
            "{out}"
        );
        assert!(out.contains("path = \"/far/helper\""), "{out}");
        assert!(out.contains(r#"unix-only = { path = "/w/unix" }"#), "{out}");
        assert!(
            out.contains(r#"serde = { path = "/w/serde-fork" }"#),
            "{out}"
        );
        assert!(out.contains("serde = \"1\""), "{out}");

        let inside = "[dependencies]\ncore = { path = \"../core\" }\n[workspace.dependencies]\nx = { path = \"x\" }\n";
        assert_eq!(
            absolutize_path_dependencies(
                inside,
                Path::new("/w/ws/app"),
                Path::new("/w/ws"),
                Path::new("/copy")
            )
            .unwrap(),
            None
        );
        let ws = "[workspace.dependencies]\nshared = { path = \"../shared\" }\n";
        let out = absolutize_path_dependencies(
            ws,
            Path::new("/w/ws"),
            Path::new("/w/ws"),
            Path::new("/copy"),
        )
        .unwrap()
        .unwrap();
        assert!(out.contains(r#"shared = { path = "/w/shared" }"#), "{out}");
        assert!(
            absolutize_path_dependencies(
                "not [ toml",
                Path::new("/w"),
                Path::new("/w"),
                Path::new("/copy")
            )
            .is_err()
        );
        assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
        // An absolute path into the workspace has to follow the code into the copy.
        let absolute = "[dependencies]\nhelper = { path = \"/w/ws/helper\" }\n";
        let out = absolutize_path_dependencies(
            absolute,
            Path::new("/w/ws/app"),
            Path::new("/w/ws"),
            Path::new("/copy"),
        )
        .unwrap()
        .unwrap();
        assert!(
            out.contains(r#"helper = { path = "/copy/helper" }"#),
            "{out}"
        );
    }

    #[test]
    fn pool_runs_every_job_once_and_keeps_the_order() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for workers in [0, 1, 2, 3, 8, 100] {
            for count in [0, 1, 2, 7, 50] {
                let calls: Vec<AtomicUsize> = (0..count).map(|_| AtomicUsize::new(0)).collect();
                let results = run_pool(count, workers, |i| {
                    calls[i].fetch_add(1, Ordering::SeqCst);
                    std::thread::yield_now();
                    Ok(i * 10)
                })
                .unwrap();
                assert_eq!(
                    results,
                    (0..count).map(|i| i * 10).collect::<Vec<_>>(),
                    "workers={workers}"
                );
                assert!(
                    calls.iter().all(|c| c.load(Ordering::SeqCst) == 1),
                    "workers={workers} count={count}"
                );
            }
        }
        let failed = run_pool(5, 3, |i| {
            if i == 3 {
                Err(anyhow!("job {i} broke"))
            } else {
                Ok(i)
            }
        });
        assert_eq!(failed.unwrap_err().to_string(), "job 3 broke");
    }

    #[test]
    fn operand_ranges_ignore_wrapping_parentheses() {
        assert_eq!(core_range(10, "a > 0"), (10, 15));
        assert_eq!(core_range(10, "(a > 0)"), (11, 16));
        assert_eq!(core_range(10, " ( (a > 0) ) "), (14, 19));
        // Two groups, not one wrapper.
        assert_eq!(core_range(0, "(a) && (b)"), (0, 10));
        assert_eq!(core_range(0, "f(a)"), (0, 4));
        assert_eq!(core_range(3, "()"), (4, 4));
        // Text that is not balanced is left as it is.
        assert_eq!(core_range(0, "(a"), (0, 2));
        assert_eq!(core_range(0, "a)"), (0, 2));
        assert_eq!(core_range(0, ")"), (0, 1));
    }

    #[test]
    fn pool_never_uses_more_threads_than_asked() {
        for workers in [1, 2] {
            let seen = std::sync::Mutex::new(HashSet::new());
            run_pool(6, workers, |_| {
                seen.lock().unwrap().insert(std::thread::current().id());
                // Long enough for an idle extra thread to take the next job.
                std::thread::sleep(Duration::from_millis(30));
                Ok(())
            })
            .unwrap();
            assert_eq!(seen.lock().unwrap().len(), workers);
        }
    }

    #[test]
    fn workspace_dependents() {
        let package = |name: &str, deps: &[&str]| Package {
            name: name.to_string(),
            features: BTreeMap::new(),
            dependencies: deps
                .iter()
                .map(|d| Dependency {
                    name: d.to_string(),
                })
                .collect(),
            manifest_path: PathBuf::new(),
            targets: vec![],
        };
        let packages = [
            package("core", &["serde"]),
            package("mid", &["core"]),
            package("app", &["mid", "other"]),
            package("other", &[]),
            package("cycle", &["app", "core"]),
        ];
        assert_eq!(dependents_of("core", &packages), ["app", "cycle", "mid"]);
        assert_eq!(dependents_of("mid", &packages), ["app", "cycle"]);
        assert_eq!(dependents_of("other", &packages), ["app", "cycle"]);
        assert!(dependents_of("app", &packages) == ["cycle"]);
        assert!(dependents_of("nobody", &packages).is_empty());
        // A package is not its own dependent, whatever depends on what.
        let mutual = [package("x", &["y"]), package("y", &["x"])];
        assert_eq!(dependents_of("x", &mutual), ["y"]);
    }

    #[test]
    fn failure_lines_of_libtest() {
        assert!(is_failure_line("test tests::a ... FAILED"));
        assert!(is_failure_line(
            "test src/lib.rs - double (line 3) ... FAILED"
        ));
        assert!(!is_failure_line("test tests::a ... ok"));
        assert!(!is_failure_line("test result: FAILED. 1 passed; 1 failed"));
        assert!(!is_failure_line("my output says ... FAILED"));
        assert!(!is_failure_line(
            "test tests::a ... FAILED (but printed by the test)"
        ));
    }

    #[test]
    fn effects_in_words() {
        let source = "fn f(s: &mut S) -> bool { s.reset(); s.count > 1 }";
        let mut found = instrument::discover(source, 0).unwrap().sites;
        found.retain(|s| !matches!(s.kind, SiteKind::Body { .. }));
        for (id, site) in found.iter_mut().enumerate() {
            site.id = id as u32;
        }
        let sites: BTreeMap<u32, (&str, &Site)> =
            found.iter().map(|s| (s.id, ("src/lib.rs", s))).collect();
        let texts: Vec<&str> = found.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(
            texts,
            ["fn f(s: &mut S) -> bool", "s.reset();", "s.count > 1"]
        );

        let set = |list: &[Change]| list.iter().cloned().collect::<BTreeSet<Change>>();
        let mut changes = BTreeMap::new();
        changes.insert(
            "tests::a".to_string(),
            set(&[
                Change::Returns {
                    site: 0,
                    value: true,
                },
                Change::NowRuns { site: 1 },
            ]),
        );
        changes.insert(
            "tests::b".to_string(),
            set(&[Change::NowRuns { site: 1 }, Change::NewOutcome { site: 2 }]),
        );
        changes.insert("?".to_string(), set(&[Change::NoLongerRuns { site: 2 }]));
        assert_eq!(
            describe_effects(&changes, &sites),
            [
                "with the mutant, `tests::a` gets `true` from `fn f(s: &mut S) -> bool` (src/lib.rs:1)",
                "with the mutant, `tests::a` (and 1 more) now runs `s.reset();` (src/lib.rs:1)",
                "with the mutant, a test no longer runs `s.count > 1` (src/lib.rs:1)",
                "(and 1 more such changes)",
            ]
        );
        // What is not listed is counted.
        let mut many = BTreeMap::new();
        many.insert(
            "tests::a".to_string(),
            set(&[
                Change::NowRuns { site: 0 },
                Change::NowRuns { site: 1 },
                Change::NowRuns { site: 2 },
                Change::NoLongerRuns { site: 0 },
                Change::NoLongerRuns { site: 1 },
                Change::NoLongerRuns { site: 2 },
            ]),
        );
        assert_eq!(
            describe_effects(&many, &sites).last().map(String::as_str),
            Some("(and 3 more such changes)")
        );
        // Up to three changes are all listed, with nothing after them.
        let mut few = BTreeMap::new();
        few.insert(
            "tests::a".to_string(),
            set(&[
                Change::NowRuns { site: 1 },
                Change::NewOutcome { site: 2 },
                Change::Returns {
                    site: 0,
                    value: false,
                },
            ]),
        );
        assert_eq!(
            describe_effects(&few, &sites),
            [
                "with the mutant, `tests::a` gets `false` from `fn f(s: &mut S) -> bool` (src/lib.rs:1)",
                "with the mutant, `tests::a` now runs `s.reset();` (src/lib.rs:1)",
                "with the mutant, `tests::a` takes `s.count > 1` (src/lib.rs:1) a way it did not before",
            ]
        );
        assert_eq!(
            describe_effects(&BTreeMap::new(), &sites),
            [
                "with the mutant, these tests still reach the same code with the same outcomes: they do not use its result, or it is the same for their inputs"
            ]
        );
    }

    #[test]
    fn mutant_timeout_scales_with_the_baseline() {
        assert_eq!(mutant_timeout(Duration::ZERO), Duration::from_secs(5));
        assert_eq!(
            mutant_timeout(Duration::from_secs(2)),
            Duration::from_secs(15)
        );
    }

    #[cfg(unix)]
    #[test]
    fn stopping_nextest_reaps_a_runner_that_ignores_termination() {
        use std::io::Read as _;
        use std::os::unix::process::CommandExt as _;

        let mut child = Command::new("sh")
            .args(["-c", "trap '' TERM; printf X; while :; do sleep 1; done"])
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdout.take().unwrap().read_exact(&mut [0]).unwrap();

        stop_nextest(&mut child, Duration::from_millis(10));
        let status = child.try_wait().unwrap().expect("runner was reaped");
        assert!(!status.success());
    }

    #[test]
    fn nextest_counts_only_a_test_failure_as_a_kill() {
        assert_eq!(nextest_outcome(Some(0)).unwrap(), Outcome::Pass);
        assert_eq!(nextest_outcome(Some(100)).unwrap(), Outcome::Fail);
        for code in [Some(4), Some(101), Some(102), Some(105), None] {
            assert!(nextest_outcome(code).is_err(), "{code:?}");
        }
    }

    #[test]
    fn test_polling_backs_off_and_respects_the_remaining_time() {
        let mut delay = TEST_POLL_INITIAL;
        for micros in [250, 500, 1_000, 2_000, 4_000, 5_000, 5_000] {
            assert_eq!(
                next_test_poll(&mut delay, Duration::from_secs(1)),
                Duration::from_micros(micros)
            );
        }
        let remaining = Duration::from_micros(120);
        assert_eq!(next_test_poll(&mut delay, remaining), remaining);
        assert_eq!(next_test_poll(&mut delay, Duration::ZERO), Duration::ZERO);
        assert_eq!(delay, Duration::from_millis(5));
    }
    /// A scratch directory of its own for one test.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tmt-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("ws/src")).unwrap();
        fs::create_dir_all(dir.join("work/src")).unwrap();
        dir.canonicalize().unwrap()
    }

    #[cfg(unix)]
    #[test]
    fn a_descendant_cannot_keep_the_completed_runner_output_open() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = scratch("descendant-output");
        let work = dir.join("work");
        let exe = work.join("runner");
        fs::write(&exe, "#!/bin/sh\nsleep 8 &\nexit 0\n").unwrap();
        fs::set_permissions(&exe, fs::Permissions::from_mode(0o755)).unwrap();
        let bin = TestBinary {
            name: "runner".into(),
            cwd: work.join("src"),
            marks: marks_dir(&work).unwrap(),
            git_ceiling: work,
            tests: BTreeSet::new(),
            runner: Runner::Exe(exe),
            baseline: Duration::ZERO,
            traced: Duration::ZERO,
            timeout: None,
            wrapper_grace: Duration::ZERO,
            report_slow: false,
            filtered_timings: Default::default(),
            trace: Trace::default(),
        };
        let started = Instant::now();
        assert_eq!(
            run_binary(
                &bin,
                None,
                &[],
                Duration::from_secs(2),
                RunOutput::FirstFailure,
                1
            )
            .unwrap(),
            Outcome::Pass
        );
        assert!(
            started.elapsed() < Duration::from_secs(6),
            "a descendant held the output pipe open"
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_detached_descendant_does_not_hold_the_runner_open() {
        let dir = scratch("detached-output");
        let work = dir.join("work");
        let source = work.join("runner.rs");
        let exe = work.join("runner");
        fs::write(
            &source,
            r#"
use std::os::unix::process::CommandExt;
fn main() {
    let child = std::process::Command::new("sleep").arg("8").process_group(0).spawn().unwrap();
    std::fs::write("child.pid", child.id().to_string()).unwrap();
}
"#,
        )
        .unwrap();
        let output = Command::new("rustc")
            .arg(&source)
            .arg("-o")
            .arg(&exe)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let bin = TestBinary {
            name: "runner".into(),
            cwd: work.join("src"),
            marks: marks_dir(&work).unwrap(),
            git_ceiling: work,
            tests: BTreeSet::new(),
            runner: Runner::Exe(exe),
            baseline: Duration::ZERO,
            traced: Duration::ZERO,
            timeout: None,
            wrapper_grace: Duration::ZERO,
            report_slow: false,
            filtered_timings: Default::default(),
            trace: Trace::default(),
        };
        let started = Instant::now();
        let outcome = run_binary(
            &bin,
            None,
            &[],
            Duration::from_secs(2),
            RunOutput::FirstFailure,
            1,
        );
        let elapsed = started.elapsed();
        // This test alone owns the separate child group.
        let pid = fs::read_to_string(bin.cwd.join("child.pid"))
            .unwrap()
            .parse()
            .unwrap();
        kill_test_group(pid);
        fs::remove_dir_all(dir).unwrap();
        assert_eq!(outcome.unwrap(), Outcome::Pass);
        assert!(
            elapsed < Duration::from_secs(6),
            "a detached descendant held the output pipe open"
        );
    }

    fn set_modified(path: &Path, time: std::time::SystemTime) {
        fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(time)
            .unwrap();
    }

    fn modified(path: &Path) -> std::time::SystemTime {
        fs::metadata(path).unwrap().modified().unwrap()
    }

    /// What a run does to the working copy up to the build. `write` stands for the
    /// instrumentation of the files in `selected`.
    fn copy_run(dir: &Path, selected: &[&str], write: bool) -> WorkCopy {
        let (workspace, work) = (dir.join("ws"), dir.join("work"));
        let copy = work.join("src");
        let state = WorkCopy::open(&work, &copy);
        copy_tree(&workspace, &copy, &workspace, &|_| false, &state).unwrap();
        prune(&copy, &state).unwrap();
        for name in selected {
            let path = copy.join(name);
            let original = state.original(&path, &workspace).unwrap().unwrap();
            assert_eq!(original, fs::read_to_string(workspace.join(name)).unwrap());
            if write {
                state
                    .write(&path, &format!("instrumented {original}"))
                    .unwrap();
            } else {
                state.record(&path).unwrap(); // interrupted before the write
            }
        }
        if write {
            state.settle(&workspace).unwrap();
        }
        state
    }

    /// Rows generated by `pict formal/pict/workcopy.pict /o:3 /e:formal/pict/workcopy.seed.tsv`.
    #[test]
    fn work_copy_table() {
        let table = include_str!("../tests/data/workcopy.pict.tsv");
        let hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
        let mut rows = 0;
        for (index, line) in table.lines().skip(1).enumerate() {
            let [earlier, source, selected] = line.split('\t').collect::<Vec<_>>()[..] else {
                panic!("bad row: {line}");
            };
            let dir = scratch(&format!("copy-table-{index}"));
            let (file, copied) = (dir.join("ws/src/a.rs"), dir.join("work/src/src/a.rs"));
            let log = dir.join("work").join(WRITTEN_LOG);
            fs::write(&file, "v1").unwrap();
            set_modified(&file, hour_ago);
            match earlier {
                "none" => {}
                "plain" => drop(copy_run(&dir, &[], true)),
                "written" => drop(copy_run(&dir, &["src/a.rs"], true)),
                "logged_only" => drop(copy_run(&dir, &["src/a.rs"], false)),
                _ => panic!("{line}"),
            }
            if copied.exists() {
                set_modified(&copied, hour_ago);
            }
            let before = fs::read_to_string(&copied).ok();
            match source {
                "same" => {}
                "edited" => fs::write(&file, "version 2").unwrap(),
                "deleted" => fs::remove_file(&file).unwrap(),
                _ => panic!("{line}"),
            }
            let selected = selected == "yes";
            copy_run(&dir, if selected { &["src/a.rs"] } else { &[] }, true);

            let wanted = match (source, selected) {
                ("deleted", _) => None,
                ("same", true) => Some("instrumented v1".to_string()),
                ("same", false) => Some("v1".to_string()),
                (_, true) => Some("instrumented version 2".to_string()),
                (_, false) => Some("version 2".to_string()),
            };
            assert_eq!(fs::read_to_string(&copied).ok(), wanted, "{line}");
            assert_eq!(
                fs::read_to_string(&log).unwrap_or_default(),
                if selected { "src/a.rs\n" } else { "" },
                "{line}"
            );
            // What did not change was not written again, apart from the copy an
            // interrupted run left under a log entry.
            if before == wanted && wanted.is_some() && earlier != "logged_only" {
                assert_eq!(modified(&copied), hour_ago, "{line}");
            }
            // What did change looks new to cargo, although its source is an hour old:
            // a copy that kept the time of its source would not be rebuilt.
            if before != wanted && wanted.is_some() {
                assert!(
                    modified(&copied) > hour_ago + Duration::from_secs(1800),
                    "{line}"
                );
            }
            // A further run that instruments nothing leaves a plain copy.
            copy_run(&dir, &[], true);
            assert_eq!(
                fs::read_to_string(&copied).ok(),
                fs::read_to_string(&file).ok(),
                "{line}"
            );
            assert_eq!(fs::read_to_string(&log).unwrap(), "", "{line}");
            fs::remove_dir_all(&dir).unwrap();
            rows += 1;
        }
        assert_eq!(rows, 20);
    }

    #[test]
    fn work_copy_keeps_modification_times() {
        let dir = scratch("copy-times");
        let hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
        let names = ["src/lib.rs", "src/plain.rs", "src/edited.rs"];
        for name in names {
            fs::write(dir.join("ws").join(name), format!("// {name}")).unwrap();
            set_modified(&dir.join("ws").join(name), hour_ago);
        }
        let copy = dir.join("work/src");
        let runtime = copy.join("src/__tmt_rt.rs");
        let run = || {
            let state = copy_run(&dir, &[], false);
            for name in ["src/lib.rs", "src/edited.rs"] {
                let original = state.original(&copy.join(name), &dir.join("ws")).unwrap();
                state
                    .write(&copy.join(name), &format!("probe {}", original.unwrap()))
                    .unwrap();
            }
            // A file of the tool's own, with no source: not taken for one to instrument.
            assert_eq!(state.original(&runtime, &dir.join("ws")).ok(), {
                if runtime.exists() { Some(None) } else { None }
            });
            state.write(&runtime, "runtime").unwrap();
            state.settle(&dir.join("ws")).unwrap();
        };
        run();
        for name in names.iter().map(|n| copy.join(n)).chain([runtime.clone()]) {
            set_modified(&name, hour_ago);
        }
        fs::write(dir.join("ws/src/edited.rs"), "// edited since").unwrap();
        run();
        // Nothing but the edited file was touched: cargo has one file to look at.
        for name in ["src/lib.rs", "src/plain.rs", "src/__tmt_rt.rs"] {
            assert_eq!(modified(&copy.join(name)), hour_ago, "{name}");
        }
        assert_ne!(modified(&copy.join("src/edited.rs")), hour_ago);
        assert_eq!(
            fs::read_to_string(copy.join("src/edited.rs")).unwrap(),
            "probe // edited since"
        );
        assert_eq!(
            fs::read_to_string(copy.join("src/lib.rs")).unwrap(),
            "probe // src/lib.rs"
        );
        assert_eq!(
            fs::read_to_string(dir.join("work").join(WRITTEN_LOG)).unwrap(),
            "src/__tmt_rt.rs\nsrc/edited.rs\nsrc/lib.rs\n"
        );
        // A source that was only touched is still what the copy holds: nothing to do.
        let later = hour_ago + Duration::from_secs(60);
        set_modified(&dir.join("ws/src/plain.rs"), later);
        run();
        assert_eq!(modified(&copy.join("src/plain.rs")), hour_ago);
        // Other bytes are copied, also where the size and the time of the source say
        // nothing (a file put back as it was long ago).
        let plain = fs::read_to_string(dir.join("ws/src/plain.rs")).unwrap();
        fs::write(dir.join("ws/src/plain.rs"), plain.to_uppercase()).unwrap();
        set_modified(
            &dir.join("ws/src/plain.rs"),
            hour_ago - Duration::from_secs(60),
        );
        run();
        assert_eq!(
            fs::read_to_string(copy.join("src/plain.rs")).unwrap(),
            plain.to_uppercase()
        );
        assert!(modified(&copy.join("src/plain.rs")) > later);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn work_copy_log_is_not_trusted() {
        let dir = scratch("copy-log");
        let (workspace, work) = (dir.join("ws"), dir.join("work"));
        let copy = work.join("src");
        let outside = dir.join("outside.txt");
        fs::write(&outside, "keep").unwrap();
        fs::write(workspace.join("src/a.rs"), "a").unwrap();
        // Entries that leave the copy are not entries.
        fs::write(
            work.join(WRITTEN_LOG),
            format!(
                "../../outside.txt\n{}\n\nsrc/../../../outside.txt\nsrc/a.rs\n",
                outside.display()
            ),
        )
        .unwrap();
        let state = WorkCopy::open(&work, &copy);
        assert_eq!(
            state.earlier,
            [copy.join("src/a.rs")].into_iter().collect::<HashSet<_>>()
        );
        copy_tree(&workspace, &copy, &workspace, &|_| false, &state).unwrap();
        state.settle(&workspace).unwrap();
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
        assert_eq!(fs::read_to_string(copy.join("src/a.rs")).unwrap(), "a");

        // Nothing is written outside the copy, and a name that would break the log is refused.
        let state = WorkCopy::open(&work, &copy);
        assert!(state.write(&outside, "x").is_err());
        assert!(state.write(&copy.join("src/a\nb.rs"), "x").is_err());
        assert!(!copy.join("src/a\nb.rs").exists());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
        fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn work_copy_log_follows_every_write() {
        let dir = scratch("copy-log-order");
        let (workspace, work) = (dir.join("ws"), dir.join("work"));
        let copy = work.join("src");
        let log = work.join(WRITTEN_LOG);
        let names = ["h", "c", "f", "a", "e", "b", "g", "d"].map(|n| format!("src/{n}.rs"));
        for name in &names {
            fs::write(workspace.join(name), "x").unwrap();
        }
        let state = copy_run(&dir, &[], false);
        // The entry is there before the file changes, once.
        state.record(&copy.join(&names[0])).unwrap();
        state.record(&copy.join(&names[0])).unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "src/h.rs\n");
        assert_eq!(fs::read_to_string(copy.join(&names[0])).unwrap(), "x");
        for name in &names {
            state.write(&copy.join(name), "probe").unwrap();
        }
        state.settle(&workspace).unwrap();
        let mut sorted = names.to_vec();
        sorted.sort();
        assert_eq!(
            fs::read_to_string(&log).unwrap(),
            sorted.iter().map(|n| format!("{n}\n")).collect::<String>()
        );

        // After the log was trimmed, a later write is logged again.
        let state = copy_run(&dir, &[], false);
        state.settle(&workspace).unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "");
        state.write(&copy.join(&names[3]), "probe").unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), "src/a.rs\n");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sync_copies_what_is_not_up_to_date_and_nothing_else() {
        let dir = scratch("copy-sync");
        let (workspace, work) = (dir.join("ws"), dir.join("work"));
        let copy = work.join("src");
        let hour_ago = std::time::SystemTime::now() - Duration::from_secs(3600);
        for name in [
            "same",
            "longer",
            "newer",
            "was_dir",
            "was_link",
            "short_link",
            "twin_link",
            "shorter",
        ] {
            fs::write(workspace.join("src").join(name), "source").unwrap();
            set_modified(&workspace.join("src").join(name), hour_ago);
        }
        // A file that cannot be written to is copied like any other.
        let locked = workspace.join("src/locked");
        fs::write(&locked, "read only").unwrap();
        let mut permissions = fs::metadata(&locked).unwrap().permissions();
        permissions.set_readonly(true);
        fs::set_permissions(&locked, permissions).unwrap();
        fs::create_dir_all(copy.join("src/was_dir/inner")).unwrap();
        // The same size and newer, and not the same: no copy of the source.
        fs::write(copy.join("src/same"), "SOURCE").unwrap();
        // A copy is one whatever its time says, and keeps that time (nothing to rebuild).
        fs::write(workspace.join("src/copied"), "as it is").unwrap();
        fs::write(copy.join("src/copied"), "as it is").unwrap();
        let long_ago = hour_ago - Duration::from_secs(7200);
        set_modified(&copy.join("src/copied"), long_ago);
        // A directory of the copy that has become a link is not walked into.
        let elsewhere = dir.join("elsewhere");
        fs::create_dir_all(&elsewhere).unwrap();
        std::os::unix::fs::symlink(&elsewhere, copy.join("linked")).unwrap();
        fs::create_dir_all(workspace.join("linked/deep")).unwrap();
        fs::write(workspace.join("linked/deep/z"), "z").unwrap();
        fs::write(copy.join("src/longer"), "SOURCE+").unwrap();
        fs::write(copy.join("src/newer"), "SOURCE").unwrap();
        set_modified(&copy.join("src/newer"), hour_ago - Duration::from_secs(60));
        // A link in the copy is replaced, not written through.
        let outside = dir.join("outside");
        fs::write(&outside, "keep").unwrap();
        std::os::unix::fs::symlink(&outside, copy.join("src/was_link")).unwrap();
        // Not even one to a file that holds what the source holds.
        let twin = dir.join("twin");
        fs::write(&twin, "source").unwrap();
        std::os::unix::fs::symlink(&twin, copy.join("src/twin_link")).unwrap();
        // The beginning of the source is not the source.
        fs::write(copy.join("src/shorter"), "sour").unwrap();
        // Not even one whose own size and time would pass for the file's.
        std::os::unix::fs::symlink("abcdef", copy.join("src/short_link")).unwrap();
        // What the run does not want goes, with the directories it leaves empty.
        fs::create_dir_all(copy.join("old/deep")).unwrap();
        fs::write(copy.join("old/deep/x"), "x").unwrap();
        fs::create_dir_all(workspace.join("keep/deep")).unwrap();
        fs::write(workspace.join("keep/deep/y"), "y").unwrap();

        copy_run(&dir, &[], true);
        let read = |name: &str| fs::read_to_string(copy.join(name)).unwrap();
        assert_eq!(read("src/locked"), "read only");
        assert_eq!(read("src/copied"), "as it is");
        assert_eq!(
            fs::metadata(copy.join("src/copied"))
                .unwrap()
                .modified()
                .unwrap(),
            long_ago
        );
        assert_eq!(read("linked/deep/z"), "z");
        assert!(fs::symlink_metadata(copy.join("linked")).unwrap().is_dir());
        assert_eq!(
            fs::read_dir(&elsewhere).unwrap().count(),
            0,
            "nothing written through"
        );
        for name in [
            "src/same",
            "src/longer",
            "src/newer",
            "src/was_dir",
            "src/was_link",
            "src/short_link",
            "src/twin_link",
            "src/shorter",
        ] {
            assert_eq!(read(name), "source", "{name}");
            assert!(
                fs::symlink_metadata(copy.join(name)).unwrap().is_file(),
                "{name}"
            );
        }
        assert!(
            fs::symlink_metadata(copy.join("src/was_link"))
                .unwrap()
                .is_file()
        );
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
        assert!(!copy.join("old").exists());
        assert_eq!(read("keep/deep/y"), "y");
        fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn a_finding_keeps_its_number_when_the_one_before_it_is_closed() {
        let source = "impl S {\n    fn f(a: u8, b: u8) -> u8 {\n        let x = if a > b { 1 } else { 0 };\n        let y = if a > b { 2 } else { 0 };\n        x + y\n    }\n}\nfn g(a: u8, b: u8) -> bool {\n    a > b\n}\n";
        let found = instrument::discover(source, 0).unwrap();
        let all: Vec<(usize, usize, String)> = found
            .sites
            .iter()
            .map(|site| {
                let at = report::location("", site);
                (at.line, at.column, at.code)
            })
            .collect();
        let places = [FilePlaces {
            display: "src/lib.rs",
            source,
            places: &all,
        }];
        let gap = |line: usize, column: usize, file: &str| Finding::InputGap {
            at: report::Location {
                file: file.to_string(),
                line,
                column,
                code: "a > b".to_string(),
                ..report::Location::default()
            },
            survivors: vec![],
            survivor_count: 0,
            add_tests: vec![],
            observed: None,
            verified: true,
        };
        let name = |finding: &Finding| {
            let at = finding.at();
            (at.item.clone(), at.id.clone())
        };
        // Only the second of the two in `f` is a finding: it is still the second.
        let mut findings = [
            gap(4, 20, "src/lib.rs"),
            gap(9, 5, "src/lib.rs"),
            gap(9, 5, "src/other.rs"),
        ];
        name_findings(&mut findings, &places);
        assert_eq!(
            name(&findings[0]),
            (
                "S::f".to_string(),
                "input_gap|src/lib.rs|S::f|a > b|1".to_string()
            )
        );
        assert_eq!(
            name(&findings[1]),
            (
                "g".to_string(),
                "input_gap|src/lib.rs|g|a > b|0".to_string()
            )
        );
        // A finding of a file that is not among the given ones is left without a name.
        assert_eq!(name(&findings[2]), (String::new(), String::new()));
    }

    #[test]
    fn an_audit_takes_its_sample_evenly_and_the_same_every_time() {
        assert_eq!(spread(0, 3), Vec::<usize>::new());
        assert_eq!(spread(5, 0), Vec::<usize>::new());
        assert_eq!(spread(3, 3), [0, 1, 2]);
        assert_eq!(spread(3, 10), [0, 1, 2]);
        assert_eq!(spread(10, 1), [0]);
        assert_eq!(spread(10, 3), [0, 3, 6]);
        assert_eq!(spread(10, 5), [0, 2, 4, 6, 8]);
        assert_eq!(spread(4, 3), [0, 1, 2]);
        assert_eq!(spread(7, 6), [0, 1, 2, 3, 4, 5]);
    }

    #[test]
    fn a_mutant_of_the_source_is_the_mutation_its_probe_runs() {
        // The runtime and the driver have to mean the same status.
        // The runtime ends a run it gives up as a failed one: the mark says which it was.
        assert_ne!(crate::tmt_rt::CANNOT_EVALUATE, 0);
        let source = "fn f(a: i32, b: i32, ok: bool, k: Option<i32>, base: O) -> O { \
                      go(); \
                      let x = match k { Some(n) if n > 1 => !ok && a < b, _ => -a + b > 0 || ok }; \
                      for _ in a..b {} for _ in a..=b {} \
                      O { on: x, ..base } }";
        let found = instrument::discover(source, 0).unwrap().sites;
        let mut seen = BTreeSet::new();
        for site in &found {
            for mutant in instrument::source_mutants(site) {
                let kind = format!("{:?}", site.kind);
                let kind = kind.split([' ', '{', '(']).next().unwrap().to_string();
                seen.insert((
                    kind,
                    mutant.into.clone(),
                    format!("{:?}", mutation_of(site, &mutant)),
                ));
            }
        }
        let expected = [
            ("Arith", "*", "ArithSwap2"),
            ("Arith", "-", "ArithSwap"),
            ("Arm", "Some(n) if n > 1", "Delete"),
            ("Body", "Default::default()", "Delete"),
            ("Cmp", "<", "Op(Lt)"),
            ("Cmp", "<=", "Op(Le)"),
            ("Cmp", "==", "Op(Eq)"),
            ("Cmp", ">", "Op(Gt)"),
            ("Cmp", ">=", "Op(Ge)"),
            ("Field", "on", "Delete"),
            ("Guard", "true", "Const(true)"),
            ("Logic", "&&", "LogicSwap"),
            ("Logic", "||", "LogicSwap"),
            ("Neg", "", "Delete"),
            ("Not", "", "DropNot"),
            ("Range", "..", "RangeEnd"),
            ("Range", "..=", "RangeEnd"),
            ("Stmt", "", "Delete"),
        ];
        // A comparison is planned in other words than it is written in: the mutations
        // that are the same change, as an audit looks for them.
        let like = |source: &str, into: &str| -> Vec<Mutation> {
            let sites = instrument::discover(source, 0).unwrap().sites;
            let site = sites
                .iter()
                .find(|site| matches!(site.kind, SiteKind::Cmp { .. }))
                .unwrap();
            let mutant = instrument::source_mutants(site)
                .into_iter()
                .find(|mutant| mutant.into == into)
                .unwrap();
            mutations_like(site, &mutant, true)
        };
        use plan::CmpOp::*;
        for (op, into, expected) in [
            ("==", "!=", vec![Mutation::Op(Ne), Mutation::Negate]),
            ("!=", "==", vec![Mutation::Op(Eq), Mutation::Negate]),
            ("<=", ">", vec![Mutation::Op(Gt), Mutation::Negate]),
            (">=", "<", vec![Mutation::Op(Lt), Mutation::Negate]),
            ("<", "<=", vec![Mutation::Op(Le), Mutation::Shift(1)]),
            (">", ">=", vec![Mutation::Op(Ge), Mutation::Shift(-1)]),
            ("<", "==", vec![Mutation::Op(Eq)]),
            ("<", ">", vec![Mutation::Op(Gt)]),
            (">", "==", vec![Mutation::Op(Eq)]),
            (">", "<", vec![Mutation::Op(Lt)]),
        ] {
            let source = format!("fn f(a: i32, b: i32) -> bool {{ a {op} b }}");
            assert_eq!(like(&source, into), expected, "{op} as {into}");
        }
        // Where the tests gave operands without an order, the negation of `>=` is not
        // `<`: only the operator itself is the same change. `!=` is still the negation.
        let unordered = |op: &str, into: &str| -> Vec<Mutation> {
            let source = format!("fn f(a: f64, b: f64) -> bool {{ a {op} b }}");
            let sites = instrument::discover(&source, 0).unwrap().sites;
            let site = sites
                .iter()
                .find(|site| matches!(site.kind, SiteKind::Cmp { .. }))
                .unwrap();
            let mutant = instrument::source_mutants(site)
                .into_iter()
                .find(|mutant| mutant.into == into)
                .unwrap();
            mutations_like(site, &mutant, false)
        };
        assert_eq!(unordered(">=", "<"), [Mutation::Op(Lt)]);
        assert_eq!(unordered("<=", ">"), [Mutation::Op(Gt)]);
        assert_eq!(unordered("==", "!="), [Mutation::Op(Ne), Mutation::Negate]);
        // The moved bound is the operator it is taken for, at every distance.
        for d in -3..=3 {
            assert_eq!(
                plan::eval_mutated_d(Lt, Mutation::Shift(1), d),
                Le.eval_d(d)
            );
            assert_eq!(
                plan::eval_mutated_d(Gt, Mutation::Shift(-1), d),
                Ge.eval_d(d)
            );
        }
        let seen: Vec<(&str, &str, &str)> = seen
            .iter()
            .map(|(kind, into, mutation)| (kind.as_str(), into.as_str(), mutation.as_str()))
            .collect();
        assert_eq!(seen, expected);
    }

    #[test]
    fn the_mark_of_a_run_is_a_new_file_and_says_one_of_three_things() {
        let dir = scratch("unable-mark");
        fs::create_dir_all(&dir).unwrap();
        let runs = std::sync::atomic::AtomicUsize::new(0);
        let name = |n: usize| dir.join(format!("tmt-unable-{}-{n}", std::process::id()));
        // A file of the name that would be next belongs to someone: it is not emptied,
        // and the mark gets another name.
        fs::write(name(0), "not ours").unwrap();
        fs::write(name(1), "nor this").unwrap();
        let mark = reserve_file(&dir, "unable", &runs).unwrap();
        assert_eq!(mark, name(2));
        assert!(mark.is_absolute());
        assert_eq!(fs::read_to_string(&mark).unwrap(), "");
        assert_eq!(fs::read_to_string(name(0)).unwrap(), "not ours");
        assert_eq!(fs::read_to_string(name(1)).unwrap(), "nor this");
        // Still there after the run: the run went to its end, and the mark is taken away.
        assert!(!release_mark(&mark).unwrap());
        assert!(!mark.exists());
        // Gone: the run gave up.
        let mark = reserve_file(&dir, "unable", &runs).unwrap();
        assert_eq!(mark, name(3));
        fs::remove_file(&mark).unwrap();
        assert!(release_mark(&mark).unwrap());
        // There, and not to be taken away (a directory is not a file to remove): neither
        // of the two, and said as an error. Whoever runs the tests, with whatever rights.
        let in_the_way = dir.join("a-directory");
        fs::create_dir_all(&in_the_way).unwrap();
        let refused = format!("{:#}", release_mark(&in_the_way).unwrap_err());
        assert!(refused.contains("cannot tell whether"), "{refused}");
        assert!(refused.contains("a-directory"), "{refused}");
        assert!(in_the_way.is_dir());
        // A mark cannot be made in what is no directory, and that is not tried for ever.
        let unmade = format!("{:#}", reserve_file(&name(0), "unable", &runs).unwrap_err());
        assert!(unmade.contains("creating"), "{unmade}");
        assert_eq!(fs::read_to_string(name(0)).unwrap(), "not ours");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_directory_of_the_marks_is_made_or_used_and_nothing_is_removed_for_it() {
        let dir = scratch("marks-dir");
        let work = dir.join("work");
        // Not there: made. There as a directory, with whatever is in it: used.
        assert_eq!(marks_dir(&work).unwrap(), work.join("marks"));
        assert!(work.join("marks").is_dir());
        fs::write(work.join("marks/left-over"), "x").unwrap();
        assert_eq!(marks_dir(&work).unwrap(), work.join("marks"));
        assert_eq!(
            fs::read_to_string(work.join("marks/left-over")).unwrap(),
            "x"
        );
        // A file of that name is somebody's: it stays, and the run does not start.
        let taken = dir.join("taken");
        fs::create_dir_all(&taken).unwrap();
        fs::write(taken.join("marks"), "grades of the class").unwrap();
        let said = format!("{:#}", marks_dir(&taken).unwrap_err());
        assert!(
            said.contains("is in the way") && said.contains("marks"),
            "{said}"
        );
        assert_eq!(
            fs::read_to_string(taken.join("marks")).unwrap(),
            "grades of the class"
        );
        // So is a link, whatever it points to: neither followed nor removed.
        #[cfg(unix)]
        {
            let linked = dir.join("linked");
            let elsewhere = dir.join("elsewhere");
            fs::create_dir_all(&linked).unwrap();
            fs::create_dir_all(&elsewhere).unwrap();
            std::os::unix::fs::symlink(&elsewhere, linked.join("marks")).unwrap();
            assert!(marks_dir(&linked).is_err());
            assert!(
                fs::symlink_metadata(linked.join("marks"))
                    .unwrap()
                    .is_symlink()
            );
            assert_eq!(fs::read_dir(&elsewhere).unwrap().count(), 0);
        }
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn concurrent_runs_share_the_cores() {
        // Alone a binary has them all; four at a time have a quarter each.
        assert_eq!(test_threads(16, 1), 16);
        assert_eq!(test_threads(16, 4), 4);
        assert_eq!(test_threads(18, 4), 4);
        // Never fewer than two: a single thread is the main thread, without a name.
        assert_eq!(test_threads(16, 8), 2);
        assert_eq!(test_threads(16, 9), 2);
        assert_eq!(test_threads(4, 16), 2);
        assert_eq!(test_threads(1, 1), 2);
        assert_eq!(test_threads(8, 0), 8);
    }

    #[test]
    fn cargo_is_told_the_same_everywhere() {
        assert!(CargoConfig::default().flags().is_empty());
        let some = CargoConfig {
            features: vec!["a".to_string(), "dep/b".to_string()],
            no_default_features: true,
            ..CargoConfig::default()
        };
        assert_eq!(
            some.flags(),
            ["--features", "a,dep/b", "--no-default-features"]
        );
        let all = CargoConfig {
            all_features: true,
            locked: true,
            ..CargoConfig::default()
        };
        assert_eq!(all.flags(), ["--all-features", "--locked"]);
    }

    #[test]
    fn source_mutants_are_described_like_the_others() {
        let source = "fn f(a: i32, b: i32, k: Option<i32>) -> bool { \
                      match k { Some(n) if n > 1 => !(a < b) || a + b == 0, _ => -a * b > 1 } }";
        let found = instrument::discover(source, 0).unwrap().sites;
        let described: Vec<String> = found
            .iter()
            .flat_map(|site| {
                instrument::source_mutants(site)
                    .into_iter()
                    .map(|mutant| report::describe(site, mutation_of(site, &mutant)))
            })
            .collect();
        assert_eq!(
            described,
            [
                "the body of `f` replaced by `true`",
                "the body of `f` replaced by `false`",
                // The arm can be removed, which is its guard being `false`.
                "remove the arm `Some(n) if n > 1 => ..`",
                "the guard `n > 1` replaced by `true`",
                "`n == 1`",
                "`n < 1`",
                "`n >= 1`",
                "`!(a < b) && a + b == 0`",
                "`(a < b)`",
                "`a == b`",
                "`a > b`",
                "`a <= b`",
                "`a + b != 0`",
                "`a - b`",
                "`a * b`",
                "`-a * b == 1`",
                "`-a * b < 1`",
                "`-a * b >= 1`",
                "`-a / b`",
                "`-a + b`",
                "`a`",
            ]
        );
        // Without a catch-all arm the guard can be `false` in its own right.
        let source = "fn f(k: K) -> u8 { match k { K::A(n) if n > 1 => 1, K::A(_) => 2 } }";
        let found = instrument::discover(source, 0).unwrap().sites;
        let guard = found
            .iter()
            .find(|s| matches!(s.kind, SiteKind::Guard { .. }))
            .unwrap();
        let described: Vec<String> = instrument::source_mutants(guard)
            .iter()
            .map(|mutant| report::describe(guard, mutation_of(guard, mutant)))
            .collect();
        assert_eq!(
            described,
            [
                "the guard `n > 1` replaced by `true`",
                "the guard `n > 1` replaced by `false`"
            ]
        );
    }
}
