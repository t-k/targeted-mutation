//! Runs the whole pipeline on a cargo package:
//! copy -> instrument -> build once -> trace run -> plan -> run the infected mutants.
//!
//! The user's tree is never modified: everything happens in a copy under
//! `<target dir>/tmt/`.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::instrument::{self, LiteralSide, RUNTIME_MOD, Rendered, Site, SiteKind};
use crate::plan::{self, Mutation, Obs};
use crate::report::{self, Finding, Report};
use crate::trace::{Trace, UNKNOWN_TEST};

pub struct Options {
    /// Directory of the package (or workspace) to analyse.
    pub dir: PathBuf,
    pub package: Option<String>,
    /// Re-run the tests against one survivor per input gap to confirm the prediction.
    pub verify: bool,
    /// Where to put the copy and its build. Default: `<target dir>/tmt`.
    pub work_dir: Option<PathBuf>,
    /// Progress lines on stderr.
    pub progress: bool,
    /// Copy every file of the workspace, including the ones git ignores.
    pub all_files: bool,
    /// Also run the doc tests (slow: rustdoc recompiles them for every mutant they reach).
    pub doc: bool,
}

/// How far from an error to look for the probe that caused it (a probe can break type
/// inference a few lines further down).
const NEARBY_LINES: usize = 6;
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
    manifest_path: PathBuf,
    targets: Vec<Target>,
}

#[derive(Deserialize)]
struct Target {
    kind: Vec<String>,
    src_path: PathBuf,
}

/// One line of `cargo --message-format=json`.
#[derive(Deserialize)]
struct CargoMessage {
    reason: String,
    package_id: Option<String>,
    target: Option<ArtifactTarget>,
    profile: Option<Profile>,
    executable: Option<PathBuf>,
    message: Option<Diagnostic>,
}

#[derive(Deserialize)]
struct ArtifactTarget {
    name: String,
}

#[derive(Deserialize)]
struct Profile {
    test: bool,
}

#[derive(Deserialize)]
struct Diagnostic {
    level: String,
    rendered: Option<String>,
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
    /// `(target name, executable)` of the package's test binaries.
    test_binaries: Vec<(String, PathBuf)>,
    errors: Vec<CompileError>,
}

/// Package ids embed the path, and the build runs in a copy, so compare by name.
/// Ids look like `path+file:///dir#name@1.0.0`, `path+file:///dir/name#1.0.0` or `name 1.0.0 (path+...)`.
fn is_package(id: &str, name: &str) -> bool {
    let (path, fragment) = id.split_once('#').unwrap_or((id, ""));
    fragment.split('@').next() == Some(name)
        || path.rsplit('/').next() == Some(name)
        || id.split(' ').next() == Some(name)
}

fn parse_build_output(stdout: &str, package: &str) -> BuildOutput {
    let mut out = BuildOutput::default();
    for line in stdout.lines() {
        let Ok(msg) = serde_json::from_str::<CargoMessage>(line) else {
            continue;
        };
        match msg.reason.as_str() {
            "compiler-artifact" => {
                if let (Some(exe), Some(target), Some(profile)) =
                    (msg.executable, msg.target, msg.profile)
                    && profile.test
                    && msg
                        .package_id
                        .as_deref()
                        .is_some_and(|id| is_package(id, package))
                {
                    out.test_binaries.push((target.name, exe));
                }
            }
            "compiler-message" => {
                let Some(diag) = msg.message else { continue };
                if diag.level != "error" {
                    continue;
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
                    rendered: diag.rendered.unwrap_or_default(),
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

struct SourceFile {
    /// Path relative to the package directory, for reports.
    display: String,
    /// Path inside the working copy.
    path: PathBuf,
    original: String,
    sites: Vec<Site>,
    rendered: Option<Rendered>,
}

/// How a group of tests is started.
enum Runner {
    /// A libtest executable.
    Exe(PathBuf),
    /// The package's doc tests, through `cargo test --doc` in the working copy. rustdoc
    /// compiles them on every run, and they cannot be selected by name.
    Doc {
        copy: PathBuf,
        target_dir: PathBuf,
        package: String,
    },
}

struct TestBinary {
    name: String,
    runner: Runner,
    baseline: Duration,
    trace: Trace,
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum Outcome {
    Pass,
    Fail,
    Timeout,
}

fn copy_tree(from: &Path, to: &Path, skip: &dyn Fn(&Path) -> bool) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let entry = entry?;
        let src = entry.path();
        if skip(&src) {
            continue;
        }
        let dst = to.join(entry.file_name());
        let kind = entry.file_type()?;
        // Build output and other caches (cargo marks its target directories this way).
        if kind.is_dir() && src.join("CACHEDIR.TAG").exists() {
            continue;
        }
        if kind.is_dir() {
            copy_tree(&src, &dst, skip)?;
        } else if kind.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(fs::read_link(&src)?, &dst)?;
            #[cfg(not(unix))]
            fs::copy(&src, &dst).map(|_| ())?;
        } else {
            fs::copy(&src, &dst).with_context(|| format!("copying {}", src.display()))?;
        }
    }
    Ok(())
}

fn rust_files(dir: &Path, skip: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    entries.sort();
    for path in entries {
        if skip(&path) {
            continue;
        }
        if path.is_dir() {
            rust_files(&path, skip, out)?;
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
    Ok(())
}

/// The files under `dir` that git tracks or would track (not ignored), relative to `dir`.
/// `None` when `dir` is not in a git work tree, or is itself ignored: then git has nothing
/// to say about which files matter.
fn git_visible_files(dir: &Path) -> Option<Vec<PathBuf>> {
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .stderr(Stdio::null())
            .output()
            .ok()
    };
    if !git(&["rev-parse", "--is-inside-work-tree"])?
        .status
        .success()
    {
        return None;
    }
    // Exit status 0 means "ignored".
    if git(&["check-ignore", "-q", "."])?.status.success() {
        return None;
    }
    let listing = git(&[
        "ls-files",
        "--cached",
        "--others",
        "--exclude-standard",
        "-z",
    ])?;
    if !listing.status.success() {
        return None;
    }
    let files: Vec<PathBuf> = listing
        .stdout
        .split(|b| *b == 0)
        .filter(|name| !name.is_empty())
        .filter_map(|name| std::str::from_utf8(name).ok())
        .map(PathBuf::from)
        .collect();
    (!files.is_empty()).then_some(files)
}

/// Where the out-of-line module `module` declared in `file` lives.
fn module_paths(file: &Path, module: &instrument::TestModule) -> Vec<PathBuf> {
    let dir = file.parent().unwrap_or(Path::new("."));
    if let Some(path) = &module.path {
        return vec![dir.join(path)];
    }
    let name = &module.name;
    let stem = file.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    let base = if matches!(stem, "lib" | "main" | "mod") {
        dir.to_path_buf()
    } else {
        dir.join(stem)
    };
    vec![base.join(format!("{name}.rs")), base.join(name)]
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

/// Rewrites `path = "../outside"` dependencies of a manifest to absolute paths when they
/// point outside `workspace`. Returns `None` when nothing has to change.
fn absolutize_path_dependencies(
    manifest: &str,
    manifest_dir: &Path,
    workspace: &Path,
) -> Result<Option<String>> {
    use toml_edit::{DocumentMut, Item, TableLike, value};

    fn fix(deps: &mut dyn TableLike, manifest_dir: &Path, workspace: &Path, changed: &mut bool) {
        for (_, dep) in deps.iter_mut() {
            let Some(dep) = dep.as_table_like_mut() else {
                continue;
            };
            let Some(path) = dep.get("path").and_then(Item::as_str) else {
                continue;
            };
            let target = normalize(&manifest_dir.join(path));
            if !target.starts_with(workspace) {
                dep.insert("path", value(target.to_string_lossy().into_owned()));
                *changed = true;
            }
        }
    }
    const SECTIONS: [&str; 3] = ["dependencies", "dev-dependencies", "build-dependencies"];
    fn fix_sections(
        table: &mut dyn TableLike,
        manifest_dir: &Path,
        workspace: &Path,
        changed: &mut bool,
    ) {
        for section in SECTIONS {
            if let Some(deps) = table.get_mut(section).and_then(Item::as_table_like_mut) {
                fix(deps, manifest_dir, workspace, changed);
            }
        }
    }

    let mut doc: DocumentMut = manifest.parse().context("parsing Cargo.toml")?;
    let mut changed = false;
    fix_sections(doc.as_table_mut(), manifest_dir, workspace, &mut changed);
    if let Some(ws) = doc.get_mut("workspace").and_then(Item::as_table_like_mut) {
        fix_sections(ws, manifest_dir, workspace, &mut changed);
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
                fix_sections(inner, manifest_dir, workspace, &mut changed);
            } else {
                fix(inner, manifest_dir, workspace, &mut changed);
            }
        }
    }
    Ok(changed.then(|| doc.to_string()))
}

/// Tests to pass to the binary, or `None` for "run everything".
fn test_filter(tests: &BTreeSet<String>) -> Option<Vec<String>> {
    if tests.contains(UNKNOWN_TEST) || tests.len() > MAX_TEST_FILTERS {
        None
    } else {
        Some(tests.iter().cloned().collect())
    }
}

fn mutant_timeout(baseline: Duration) -> Duration {
    Duration::from_secs(5) + baseline * 5
}

fn run_binary(
    bin: &TestBinary,
    cwd: &Path,
    tests: Option<&[String]>,
    env: (&str, &str),
    timeout: Duration,
) -> Result<Outcome> {
    let threads = std::thread::available_parallelism()
        .map_or(2, |n| n.get())
        .max(2);
    let mut cmd = match &bin.runner {
        Runner::Exe(exe) => {
            let mut cmd = Command::new(exe);
            // With one test thread libtest runs tests on the main thread, and the runtime
            // could no longer tell which test it is in.
            cmd.arg(format!("--test-threads={threads}"));
            if let Some(tests) = tests {
                cmd.arg("--exact").args(tests);
            }
            cmd.current_dir(cwd).env("CARGO_MANIFEST_DIR", cwd);
            cmd
        }
        Runner::Doc {
            copy,
            target_dir,
            package,
        } => {
            let mut cmd = Command::new("cargo");
            cmd.args(["test", "--doc", "-p", package])
                .current_dir(copy)
                .env("CARGO_TARGET_DIR", target_dir);
            cmd
        }
    };
    cmd.env_remove("TMT_TRACE")
        .env_remove("TMT_MUTANT")
        .env(env.0, env.1)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting the tests of `{}`", bin.name))?;
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(if status.success() {
                Outcome::Pass
            } else {
                Outcome::Fail
            });
        }
        if started.elapsed() > timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(Outcome::Timeout);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
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
                let offset = file
                    .rendered
                    .as_ref()
                    .expect("rendered before building")
                    .to_original(span.byte_start);
                ids.extend(instrument::blame(&file.sites, &enabled, offset));
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
    package_dir: PathBuf,
    binaries: Vec<TestBinary>,
    mutants_run: usize,
    mutants_killed: usize,
    mutants_timed_out: usize,
}

impl Session {
    fn note(&self, text: &str) {
        if self.progress {
            eprintln!("tmt: {text}");
        }
    }

    /// Runs one mutant against the given tests of each binary. Returns the tests it was
    /// run against if every one of them passed (the mutant survived).
    fn survives(
        &mut self,
        site: u32,
        mutation: Mutation,
        tests_of: &dyn Fn(&TestBinary) -> BTreeSet<String>,
    ) -> Result<Option<Vec<String>>> {
        let spec = format!("{site}:{}", mutation.encode());
        let mut ran_against = vec![];
        self.mutants_run += 1;
        for bin in &self.binaries {
            let tests = tests_of(bin);
            if tests.is_empty() {
                continue;
            }
            let filter = match bin.runner {
                Runner::Exe(_) => test_filter(&tests),
                Runner::Doc { .. } => None,
            };
            let outcome = run_binary(
                bin,
                &self.package_dir,
                filter.as_deref(),
                ("TMT_MUTANT", &spec),
                mutant_timeout(bin.baseline),
            )?;
            if outcome != Outcome::Pass {
                self.mutants_killed += 1;
                self.mutants_timed_out += (outcome == Outcome::Timeout) as usize;
                return Ok(None);
            }
            match filter {
                Some(names) => ran_against.extend(names),
                None => ran_against.push(format!("all tests of `{}`", bin.name)),
            }
        }
        Ok(Some(ran_against))
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

/// What the planner decided for one site, in the form the session executes.
struct SitePlan<'a> {
    /// Predicted survivors: the finding to report and the mutations to confirm it with.
    gaps: Vec<(Finding, Vec<Mutation>)>,
    /// Mutations some test observes differently.
    run: Vec<Mutation>,
    /// Tests of a binary whose execution the mutation can change.
    infected: InfectedTests<'a>,
    /// Tests of a binary that execute the site at all.
    reaching: ReachingTests<'a>,
}

impl Session {
    fn analyse(
        &mut self,
        report: &mut Report,
        file: &str,
        site: &Site,
        verify: bool,
    ) -> Result<()> {
        let at = report::location(file, site);
        let id = site.id;
        let plan = match &site.kind {
            SiteKind::Stmt => {
                if !self
                    .binaries
                    .iter()
                    .any(|b| b.trace.reach.contains_key(&id))
                {
                    report.unreached.push(at);
                    return Ok(());
                }
                SitePlan {
                    gaps: vec![],
                    run: vec![Mutation::Delete],
                    infected: Box::new(move |bin, _| {
                        bin.trace.reach.get(&id).cloned().unwrap_or_default()
                    }),
                    reaching: Box::new(move |bin| {
                        bin.trace.reach.get(&id).cloned().unwrap_or_default()
                    }),
                }
            }
            SiteKind::Cmp { op, literal, .. } => {
                let op = *op;
                let mut merged = Obs::default();
                for bin in &self.binaries {
                    merged.merge(&bin.trace.merged(id));
                }
                let bounds = self.binaries.iter().find_map(|b| b.trace.bounds.get(&id));
                let cmp_plan = plan::plan_cmp(op, &merged, domain_of(*literal, bounds));
                let mut gaps = vec![];
                match &cmp_plan.gap {
                    Some(plan::InputGap::Unreached) => {
                        report.unreached.push(at);
                        return Ok(());
                    }
                    Some(gap) => {
                        if let Some(finding) =
                            report::input_gap_finding(file, site, gap, &merged, verify)
                        {
                            gaps.push((finding, gap.survivors()));
                        }
                    }
                    None => {}
                }
                SitePlan {
                    gaps,
                    run: cmp_plan.run,
                    infected: Box::new(move |bin, m| {
                        tests_where(bin.trace.cmp.get(&id), |obs| plan::infects(op, m, obs))
                    }),
                    reaching: Box::new(move |bin| {
                        tests_where(bin.trace.cmp.get(&id), Obs::reached)
                    }),
                }
            }
            SiteKind::Logic { and, .. } => {
                let and = *and;
                let mut merged = plan::LogicObs::default();
                for obs in self
                    .binaries
                    .iter()
                    .filter_map(|b| b.trace.logic.get(&id))
                    .flat_map(|t| t.values())
                {
                    merged.merge(obs);
                }
                let logic_plan = plan::plan_logic(and, &merged);
                if logic_plan.gaps.contains(&plan::LogicGap::Unreached) {
                    report.unreached.push(at);
                    return Ok(());
                }
                let gaps = logic_plan
                    .gaps
                    .iter()
                    .filter_map(|gap| {
                        let mutation = match gap {
                            plan::LogicGap::RhsNeverDecides => Mutation::LogicDropRhs,
                            _ => Mutation::LogicDropLhs,
                        };
                        Some((
                            report::logic_gap_finding(file, site, *gap, verify)?,
                            vec![mutation],
                        ))
                    })
                    .collect();
                SitePlan {
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
            SiteKind::Arith { .. } => {
                let mut merged = plan::ArithObs::default();
                for obs in self
                    .binaries
                    .iter()
                    .filter_map(|b| b.trace.arith.get(&id))
                    .flat_map(|t| t.values())
                {
                    merged.merge(obs);
                }
                let (gaps, run) = match plan::plan_arith(&merged) {
                    plan::ArithPlan::Unreached => {
                        report.unreached.push(at);
                        return Ok(());
                    }
                    // Not numbers (`String + &str`, `Duration + Duration`): nothing to mutate.
                    plan::ArithPlan::Opaque => return Ok(()),
                    plan::ArithPlan::NeverDiffers => (
                        report::arith_gap_finding(file, site, verify)
                            .map(|finding| (finding, vec![Mutation::ArithSwap]))
                            .into_iter()
                            .collect(),
                        vec![],
                    ),
                    plan::ArithPlan::Run => (vec![], vec![Mutation::ArithSwap]),
                };
                SitePlan {
                    gaps,
                    run,
                    infected: Box::new(move |bin, _| {
                        tests_where(bin.trace.arith.get(&id), |obs| obs.differs)
                    }),
                    reaching: Box::new(move |bin| {
                        tests_where(bin.trace.arith.get(&id), |obs| obs.same || obs.differs)
                    }),
                }
            }
        };

        for (finding, survivors) in plan.gaps {
            // The trace says no test can notice. Confirm by running every test that
            // reaches the site against the listed survivors.
            let mut confirmed = true;
            if verify {
                for m in survivors {
                    confirmed &= self.survives(id, m, &*plan.reaching)?.is_some();
                }
            }
            if confirmed {
                report.findings.push(finding);
            } else {
                report.unstable.push(at.clone());
            }
        }
        let mut survivors = vec![];
        let mut passing_tests = BTreeSet::new();
        for m in plan.run {
            let infected = &plan.infected;
            if let Some(tests) = self.survives(id, m, &|bin| infected(bin, m))? {
                survivors.push(report::describe(site, m));
                passing_tests.extend(tests);
            }
        }
        if !survivors.is_empty() {
            report.findings.push(Finding::AssertionGap {
                at,
                survivors,
                tests: passing_tests.into_iter().collect(),
            });
        }
        Ok(())
    }
}

pub fn run(options: &Options) -> Result<Report> {
    let note = |text: &str| {
        if options.progress {
            eprintln!("tmt: {text}");
        }
    };
    let dir = options
        .dir
        .canonicalize()
        .with_context(|| format!("no such directory: {}", options.dir.display()))?;

    // 1. Locate the package.
    let output = Command::new("cargo")
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .current_dir(&dir)
        .output()
        .context("running cargo metadata")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let metadata: Metadata =
        serde_json::from_slice(&output.stdout).context("parsing cargo metadata")?;
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
    let is_kind = |t: &Target, kinds: &[&str]| t.kind.iter().any(|k| kinds.contains(&k.as_str()));
    let root_target = package
        .targets
        .iter()
        .find(|t| is_kind(t, &["lib", "rlib", "cdylib", "staticlib", "dylib"]))
        .or_else(|| package.targets.iter().find(|t| is_kind(t, &["bin"])))
        .ok_or_else(|| anyhow!("package `{}` has no lib or bin target", package.name))?;

    // 2. Copy the workspace.
    let workspace = metadata.workspace_root.canonicalize()?;
    let work = match &options.work_dir {
        Some(dir) => {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            dir.canonicalize()?
        }
        None => metadata.target_directory.join("tmt"),
    };
    let copy = work.join("src");
    if copy.exists() {
        fs::remove_dir_all(&copy).with_context(|| format!("clearing {}", copy.display()))?;
    }
    let target_dir = metadata.target_directory.clone();
    let skip = |path: &Path| {
        path == target_dir
            || path == work
            || path
                .file_name()
                .is_some_and(|n| n == ".git" || n == "node_modules")
            || (path.parent() == Some(workspace.as_path())
                && path.file_name().is_some_and(|n| n == "target"))
    };
    match (options.all_files, git_visible_files(&workspace)) {
        // Only what git would not ignore: build output, caches, local notes and secrets
        // that are kept out of the repository stay out of the copy too.
        (false, Some(files)) => {
            note(&format!(
                "copying {} files (ignored files are left out)",
                files.len()
            ));
            for relative in files {
                let (src, dst) = (workspace.join(&relative), copy.join(&relative));
                if skip(&src) || src.ancestors().any(|a| a != workspace && skip(a)) {
                    continue;
                }
                let Ok(kind) = fs::symlink_metadata(&src).map(|m| m.file_type()) else {
                    continue; // deleted in the working tree
                };
                if let Some(parent) = dst.parent() {
                    fs::create_dir_all(parent)?;
                }
                if kind.is_dir() {
                    copy_tree(&src, &dst, &skip)?; // a submodule
                } else if kind.is_symlink() {
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(fs::read_link(&src)?, &dst)?;
                    #[cfg(not(unix))]
                    fs::copy(&src, &dst).map(|_| ())?;
                } else {
                    fs::copy(&src, &dst).with_context(|| format!("copying {}", src.display()))?;
                }
            }
        }
        _ => copy_tree(&workspace, &copy, &skip)?,
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
        if let Some(rewritten) = absolutize_path_dependencies(&text, manifest_dir, &workspace)? {
            fs::write(copy.join(relative), rewritten)?;
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

    // 3. Find the sites.
    let mut other_roots = HashSet::new();
    let mut other_dirs = HashSet::new();
    for target in &package.targets {
        if target.src_path == root_target.src_path {
            continue;
        }
        let path = in_copy(&target.src_path)?;
        if let Some(parent) = path.parent().filter(|p| *p != source_dir) {
            other_dirs.insert(parent.to_path_buf());
        }
        other_roots.insert(path);
    }
    let mut paths = vec![];
    rust_files(
        &source_dir,
        &|p| {
            other_roots.contains(p)
                || other_dirs.contains(p)
                || p.file_name().is_some_and(|n| n == "target")
        },
        &mut paths,
    )?;
    let mut files = vec![];
    let mut test_only = HashSet::new();
    let mut next_id = 0u32;
    for path in paths {
        let original =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let display = path
            .strip_prefix(&package_dir)
            .unwrap_or(&path)
            .display()
            .to_string();
        match instrument::discover(&original, next_id) {
            Ok(found) => {
                next_id += found.sites.len() as u32;
                for module in &found.test_modules {
                    test_only.extend(module_paths(&path, module));
                }
                files.push(SourceFile {
                    display,
                    path,
                    original,
                    sites: found.sites,
                    rendered: None,
                });
            }
            Err(err) => note(&format!("skipping {display}: {err:#}")),
        }
    }
    files.retain(|f| !f.path.ancestors().any(|a| test_only.contains(a)));
    let total_sites: usize = files.iter().map(|f| f.sites.len()).sum();
    note(&format!("{} sites in {} files", total_sites, files.len()));

    let root_source = fs::read_to_string(&root_file)?;
    fs::write(
        source_dir.join(format!("{RUNTIME_MOD}.rs")),
        crate::RUNTIME_SOURCE,
    )?;
    let std_alias = format!("extern crate std as {STD_ALIAS};");
    let root_suffix = if root_source.contains(&std_alias) {
        format!("\n#[doc(hidden)]\nmod {RUNTIME_MOD};\n")
    } else {
        format!("\n#[doc(hidden)]\n{std_alias}\n#[doc(hidden)]\nmod {RUNTIME_MOD};\n")
    };

    // 4. Build, disabling the sites whose probe does not compile (formal/tla/RepairLoop.tla).
    let mut disabled: HashSet<u32> = HashSet::new();
    let build_target = work.join("target");
    let mut rounds = 0;
    let build = loop {
        for file in &mut files {
            let rendered =
                instrument::render(&file.original, &file.sites, &|id| !disabled.contains(&id));
            let mut text = rendered.text.clone();
            if file.path == root_file {
                text.push_str(&root_suffix);
            }
            fs::write(&file.path, text)?;
            file.rendered = Some(rendered);
        }
        if !files.iter().any(|f| f.path == root_file) {
            fs::write(&root_file, format!("{root_source}{root_suffix}"))?;
        }
        note(if rounds == 0 {
            "building the instrumented crate"
        } else {
            "rebuilding without the sites that failed to compile"
        });
        let output = Command::new("cargo")
            .args([
                "test",
                "--no-run",
                "--message-format=json",
                "-p",
                &package.name,
            ])
            .current_dir(&copy)
            .env("CARGO_TARGET_DIR", &build_target)
            .stderr(Stdio::piped())
            .output()
            .context("running cargo test --no-run")?;
        let build = parse_build_output(&String::from_utf8_lossy(&output.stdout), &package.name);
        if output.status.success() {
            break build;
        }
        if build.errors.is_empty() {
            bail!(
                "the build failed:\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        rounds += 1;
        let before = disabled.len();
        let mut unexplained = vec![];
        let mut blamed_now: HashSet<u32> = HashSet::new();
        for error in &build.errors {
            let blamed = blame_error(error, &files, &copy, &disabled);
            if std::env::var_os("TMT_DEBUG").is_some() {
                eprintln!("tmt: debug: blaming {blamed:?} for\n{}", error.rendered);
            }
            if blamed.is_empty() {
                unexplained.push(error);
            }
            blamed_now.extend(blamed);
        }
        disabled.extend(blamed_now);
        if disabled.len() == before || rounds > MAX_REPAIR_ROUNDS {
            let shown = unexplained
                .first()
                .copied()
                .or(build.errors.first())
                .map_or("", |e| e.rendered.as_str());
            bail!(
                "the crate does not build after instrumentation, and the error is not at a probe.\n\
                 Does `cargo test --no-run` pass on the original?\n{shown}"
            );
        }
    };
    if build.test_binaries.is_empty() {
        bail!("`{}` has no test binaries", package.name);
    }

    // 5. Trace run: what does each test observe?
    let mut session = Session {
        progress: options.progress,
        package_dir,
        binaries: vec![],
        mutants_run: 0,
        mutants_killed: 0,
        mutants_timed_out: 0,
    };
    let mut runners: Vec<(String, Runner)> = build
        .test_binaries
        .into_iter()
        .map(|(name, exe)| (name, Runner::Exe(exe)))
        .collect();
    if options.doc && is_kind(root_target, &["lib", "rlib", "proc-macro"]) {
        let doc = Runner::Doc {
            copy: copy.clone(),
            target_dir: build_target.clone(),
            package: package.name.clone(),
        };
        runners.push(("doc tests".to_string(), doc));
    }
    for (index, (name, runner)) in runners.into_iter().enumerate() {
        let trace_path = work.join(format!("trace-{index}.tsv"));
        let _ = fs::remove_file(&trace_path);
        let mut bin = TestBinary {
            name,
            runner,
            baseline: Duration::ZERO,
            trace: Trace::default(),
        };
        session.note(&format!("tracing `{}`", bin.name));
        let started = Instant::now();
        let trace_env = trace_path.to_string_lossy().into_owned();
        let outcome = run_binary(
            &bin,
            &session.package_dir,
            None,
            ("TMT_TRACE", &trace_env),
            Duration::from_secs(3600),
        )?;
        if outcome != Outcome::Pass {
            bail!(
                "the tests in `{}` do not pass on the unmutated code; fix them first",
                bin.name
            );
        }
        bin.baseline = started.elapsed();
        bin.trace = Trace::parse(&fs::read_to_string(&trace_path).unwrap_or_default());
        session.binaries.push(bin);
    }

    // 6. Plan each site and run what has to be run.
    let mut report = Report {
        package: package.name.clone(),
        sites: total_sites,
        skipped_sites: disabled.len(),
        ..Report::default()
    };
    for file in &files {
        for site in file.sites.iter().filter(|s| !disabled.contains(&s.id)) {
            session.analyse(&mut report, &file.display, site, options.verify)?;
        }
    }
    report.mutants_run = session.mutants_run;
    report.mutants_killed = session.mutants_killed;
    report.mutants_timed_out = session.mutants_timed_out;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let out = parse_build_output(stdout, "p");
        assert_eq!(
            out.test_binaries,
            [("p".to_string(), PathBuf::from("/t/p-abc"))]
        );
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
        let some: BTreeSet<String> = ["tests::a", "tests::b"]
            .into_iter()
            .map(String::from)
            .collect();
        assert_eq!(
            test_filter(&some),
            Some(vec!["tests::a".to_string(), "tests::b".to_string()])
        );
        let mut unknown = some.clone();
        unknown.insert(UNKNOWN_TEST.to_string());
        assert_eq!(test_filter(&unknown), None);
        let many: BTreeSet<String> = (0..=MAX_TEST_FILTERS).map(|i| format!("t{i}")).collect();
        assert_eq!(test_filter(&many), None);
        let limit: BTreeSet<String> = (0..MAX_TEST_FILTERS).map(|i| format!("t{i}")).collect();
        assert!(test_filter(&limit).is_some());
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
        let tests = instrument::TestModule {
            name: "tests".into(),
            path: None,
        };
        assert_eq!(
            module_paths(Path::new("src/lib.rs"), &tests),
            [PathBuf::from("src/tests.rs"), PathBuf::from("src/tests")]
        );
        assert_eq!(
            module_paths(Path::new("src/store.rs"), &tests),
            [
                PathBuf::from("src/store/tests.rs"),
                PathBuf::from("src/store/tests")
            ]
        );
        assert_eq!(
            module_paths(Path::new("src/store/mod.rs"), &tests),
            [
                PathBuf::from("src/store/tests.rs"),
                PathBuf::from("src/store/tests")
            ]
        );
        let with_path = instrument::TestModule {
            name: "rt".into(),
            path: Some("runtime/rt.rs".into()),
        };
        assert_eq!(
            module_paths(Path::new("src/lib.rs"), &with_path),
            [PathBuf::from("src/runtime/rt.rs")]
        );
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
        let out =
            absolutize_path_dependencies(manifest, Path::new("/w/ws/app"), Path::new("/w/ws"))
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
            absolutize_path_dependencies(inside, Path::new("/w/ws/app"), Path::new("/w/ws"))
                .unwrap(),
            None
        );
        let ws = "[workspace.dependencies]\nshared = { path = \"../shared\" }\n";
        let out = absolutize_path_dependencies(ws, Path::new("/w/ws"), Path::new("/w/ws"))
            .unwrap()
            .unwrap();
        assert!(out.contains(r#"shared = { path = "/w/shared" }"#), "{out}");
        assert!(
            absolutize_path_dependencies("not [ toml", Path::new("/w"), Path::new("/w")).is_err()
        );
        assert_eq!(normalize(Path::new("/a/./b/../c")), PathBuf::from("/a/c"));
    }

    #[test]
    fn mutant_timeout_scales_with_the_baseline() {
        assert_eq!(mutant_timeout(Duration::ZERO), Duration::from_secs(5));
        assert_eq!(
            mutant_timeout(Duration::from_secs(2)),
            Duration::from_secs(15)
        );
    }
}
