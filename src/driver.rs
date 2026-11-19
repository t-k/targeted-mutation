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
use crate::propagation::{self, Change};
use crate::report::{self, Finding, Report};
use crate::scope::{self, Scope};
use crate::trace::Trace;

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
    /// How many mutants to run at the same time.
    pub jobs: usize,
    /// Copy every file of the workspace, including the ones git ignores.
    pub all_files: bool,
    /// Also run the doc tests (slow: rustdoc recompiles them for every mutant they reach).
    pub doc: bool,
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
    manifest_path: PathBuf,
    targets: Vec<Target>,
}

#[derive(Deserialize)]
struct Dependency {
    name: String,
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
    /// `(target name, executable, package)` of the test binaries.
    test_binaries: Vec<(String, PathBuf, String)>,
    errors: Vec<CompileError>,
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
                if let (Some(exe), Some(target), Some(profile), Some(id)) = (
                    msg.executable,
                    msg.target,
                    msg.profile,
                    msg.package_id.as_deref(),
                ) && profile.test
                    && let Some(package) = packages.iter().find(|name| is_package(id, name))
                {
                    out.test_binaries
                        .push((target.name, exe, package.to_string()));
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
    /// Directory the tests run in: their package's, as under `cargo test`.
    cwd: PathBuf,
    /// The tests the binary contains, by the name libtest selects them with.
    tests: BTreeSet<String>,
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

/// Paths of the working copy that the current run wants to exist.
type Kept = std::cell::RefCell<HashSet<PathBuf>>;

/// Makes `dst` a copy of the file `src`, leaving it alone if it already is one. A file
/// that keeps its modification time is not rebuilt by cargo, so a second run only
/// recompiles what changed.
fn sync_file(src: &Path, dst: &Path, kept: &Kept) -> Result<()> {
    kept.borrow_mut().insert(dst.to_path_buf());
    let up_to_date = match (fs::metadata(src), fs::symlink_metadata(dst)) {
        (Ok(from), Ok(to)) => {
            to.is_file()
                && from.len() == to.len()
                && matches!((from.modified(), to.modified()), (Ok(a), Ok(b)) if b >= a)
        }
        _ => false,
    };
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
    fs::copy(src, dst).with_context(|| format!("copying {}", src.display()))?;
    Ok(())
}

/// Removes from the working copy whatever the current run did not put there.
fn prune(dir: &Path, kept: &Kept) -> Result<bool> {
    let mut empty = true;
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let is_dir = fs::symlink_metadata(&path)?.is_dir();
        if is_dir {
            if prune(&path, kept)? {
                fs::remove_dir(&path)?;
            } else {
                empty = false;
            }
        } else if kept.borrow().contains(&path) {
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
    kept: &Kept,
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
            return copy_tree(&target, dst, workspace, skip, kept);
        }
        sync_file(&target, dst, kept)?;
    } else if kind.is_dir() {
        // Build output and other caches (cargo marks its target directories this way).
        if !src.join("CACHEDIR.TAG").exists() {
            copy_tree(src, dst, workspace, skip, kept)?;
        }
    } else {
        sync_file(src, dst, kept)?;
    }
    Ok(())
}

fn copy_tree(
    from: &Path,
    to: &Path,
    workspace: &Path,
    skip: &dyn Fn(&Path) -> bool,
    kept: &Kept,
) -> Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from).with_context(|| format!("reading {}", from.display()))? {
        let src = entry?.path();
        if skip(&src) {
            continue;
        }
        let Some(name) = src.file_name() else {
            continue;
        };
        copy_entry(&src, &to.join(name), workspace, skip, kept)?;
    }
    Ok(())
}

/// Writes a file of the working copy. Refuses to write anywhere else, and never through
/// a symbolic link.
fn write_in_copy(copy: &Path, path: &Path, content: &str) -> Result<()> {
    if fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()) {
        fs::remove_file(path)?;
    }
    let parent = path.parent().and_then(|p| p.canonicalize().ok());
    let inside = match (parent, copy.canonicalize()) {
        (Some(parent), Ok(copy)) => parent.starts_with(copy),
        _ => false,
    };
    if !inside {
        bail!(
            "refusing to write outside the working copy: {}",
            path.display()
        );
    }
    if fs::read_to_string(path).is_ok_and(|existing| existing == content) {
        return Ok(()); // keep the modification time: nothing to rebuild
    }
    fs::write(path, content).with_context(|| format!("writing {}", path.display()))
}

fn rust_files(dir: &Path, skip: &dyn Fn(&Path) -> bool, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)?
        .map(|e| e.map(|e| e.path()))
        .collect::<std::io::Result<_>>()?;
    entries.sort();
    for path in entries {
        let Ok(kind) = fs::symlink_metadata(&path).map(|m| m.file_type()) else {
            continue;
        };
        if skip(&path) || kind.is_symlink() {
            continue;
        }
        if kind.is_dir() {
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
    kept: &Kept,
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
            fs::create_dir_all(parent)?;
        }
        if kind.is_dir() {
            match git_visible_files(&src)? {
                Some(inner) => {
                    copied += copy_git_visible(&src, &dst, inner, workspace, skip, kept)?
                }
                None => copy_tree(&src, &dst, workspace, skip, kept)?,
            }
        } else {
            copy_entry(&src, &dst, workspace, skip, kept)?;
            copied += 1;
        }
    }
    Ok(copied)
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

fn mutant_timeout(baseline: Duration) -> Duration {
    Duration::from_secs(5) + baseline * 5
}

fn run_binary(
    bin: &TestBinary,
    tests: Option<&[String]>,
    env: &[(&str, &str)],
    timeout: Duration,
    stop_at_first_failure: bool,
) -> Result<Outcome> {
    use std::io::BufRead;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

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
            cmd.current_dir(&bin.cwd)
                .env("CARGO_MANIFEST_DIR", &bin.cwd);
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
        .envs(env.iter().copied())
        .stdin(Stdio::null())
        .stdout(if stop_at_first_failure {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stderr(Stdio::null());
    // Its own process group, so that a timeout can stop whatever the tests started too.
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("starting the tests of `{}`", bin.name))?;
    // One failed test is enough to know the mutant is killed: libtest reports each test as
    // it finishes, so the rest of the run does not have to be waited for.
    let failed = Arc::new(AtomicBool::new(false));
    let watcher = child.stdout.take().map(|stdout| {
        let failed = Arc::clone(&failed);
        std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout)
                .lines()
                .map_while(|l| l.ok())
            {
                if is_failure_line(&line) {
                    failed.store(true, Ordering::SeqCst);
                }
            }
        })
    });
    let kill = |child: &mut std::process::Child| {
        #[cfg(unix)]
        let _ = Command::new("kill")
            .args(["-KILL", &format!("-{}", child.id())])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = child.kill();
        let _ = child.wait();
    };
    let started = Instant::now();
    let outcome = loop {
        if let Some(status) = child.try_wait()? {
            break if status.success() {
                Outcome::Pass
            } else {
                Outcome::Fail
            };
        }
        if failed.load(Ordering::SeqCst) {
            kill(&mut child);
            break Outcome::Fail;
        }
        if started.elapsed() > timeout {
            kill(&mut child);
            break Outcome::Timeout;
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    if let Some(watcher) = watcher {
        let _ = watcher.join();
    }
    Ok(outcome)
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
        .map(|spec| prefix.join(spec).to_string_lossy().into_owned())
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
    /// The tests the mutant passed, when it passed all of them.
    survived: Option<Vec<String>>,
    timed_out: bool,
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
            SiteKind::Stmt if reached => reach_plan(Mutation::Delete),
            SiteKind::Stmt => return Planned::Unreached,
            SiteKind::Not { .. } if reached => reach_plan(Mutation::DropNot),
            // `!` on an integer or a user type is not negation of a condition.
            SiteKind::Not { .. } if opaque => return Planned::Nothing,
            SiteKind::Not { .. } => return Planned::Unreached,
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
            SiteKind::Arith { .. } => {
                let merged = self.merged(|t| t.arith.get(&id), plan::ArithObs::merge);
                let (gaps, run) = match plan::plan_arith(&merged) {
                    plan::ArithPlan::Unreached => return Planned::Unreached,
                    // Not numbers (`String + &str`, `Duration + Duration`): nothing to mutate.
                    plan::ArithPlan::Opaque => return Planned::Nothing,
                    plan::ArithPlan::NeverDiffers => (
                        report::arith_gap_finding(file, site, verify)
                            .map(|finding| PlannedGap {
                                finding,
                                survivors: vec![Mutation::ArithSwap],
                                constant_expr: None,
                            })
                            .into_iter()
                            .collect(),
                        vec![],
                    ),
                    plan::ArithPlan::Run => (vec![], vec![Mutation::ArithSwap]),
                };
                SitePlan {
                    gaps,
                    constant: false,
                    cannot_differ: false,
                    run,
                    infected: Box::new(move |bin, _| {
                        tests_where(bin.trace.arith.get(&id), |obs| obs.differs)
                    }),
                    reaching: Box::new(move |bin| {
                        tests_where(bin.trace.arith.get(&id), |obs| obs.same || obs.differs)
                    }),
                }
            }
            SiteKind::Return { sole_expr, .. } => {
                let sole_expr = *sole_expr;
                let merged = self.merged(|t| t.returns.get(&id), plan::ReturnObs::merge);
                let return_plan = plan::plan_return(&merged);
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
                    Runner::Doc { .. } => None,
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
    fn run_job(&self, job: &Job) -> Result<JobResult> {
        let spec = format!("{}:{}", job.site, job.mutation.encode());
        let mut passed = vec![];
        for (index, filter) in &job.tests {
            let bin = &self.binaries[*index];
            let outcome = run_binary(
                bin,
                filter.as_deref(),
                &[("TMT_MUTANT", &spec)],
                mutant_timeout(bin.baseline),
                true,
            )?;
            if outcome != Outcome::Pass {
                return Ok(JobResult {
                    survived: None,
                    timed_out: outcome == Outcome::Timeout,
                });
            }
            match filter {
                Some(names) => passed.extend(names.iter().cloned()),
                None => passed.push(format!("all tests of `{}`", bin.name)),
            }
        }
        Ok(JobResult {
            survived: Some(passed),
            timed_out: false,
        })
    }

    /// Runs a surviving mutant once more with tracing on, and returns what it changed in
    /// each test compared with the original run.
    fn trace_job(&self, job: &Job, scratch: &Path) -> Result<BTreeMap<String, BTreeSet<Change>>> {
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
                mutant_timeout(bin.baseline),
                false,
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
    fn run_jobs(&self, jobs: &[Job], workers: usize) -> Result<Vec<JobResult>> {
        run_pool(jobs.len(), workers, |index| self.run_job(&jobs[index]))
    }
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
    let copy = work.join("src");
    let marker = work.join(WORK_MARKER);
    if copy.exists()
        && !marker.exists() {
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
    let _lock = WorkLock::take(&work)?;
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
    let kept = Kept::default();
    fs::create_dir_all(&copy)?;
    let visible = if options.all_files {
        None
    } else {
        git_visible_files(&workspace)?
    };
    match visible {
        // Only what git would not ignore: build output, caches, local notes and secrets
        // that are kept out of the repository stay out of the copy too.
        Some(files) => {
            let copied = copy_git_visible(&workspace, &copy, files, &workspace, &skip, &kept)?;
            note(&format!(
                "copied {copied} files (ignored files are left out)"
            ));
        }
        None => copy_tree(&workspace, &copy, &workspace, &skip, &kept)?,
    }
    prune(&copy, &kept)?;
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
            write_in_copy(&copy, &copy.join(relative), &rewritten)?;
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

    // Only the requested lines, if any were requested.
    let scope = requested_scope(options, &workspace, &package_dir, &copy)?;
    if let Some(scope) = &scope {
        for file in &mut files {
            let relative = file
                .path
                .strip_prefix(&copy)
                .unwrap_or(&file.path)
                .to_string_lossy()
                .into_owned();
            let original = &file.original;
            file.sites.retain(|site| {
                scope.touches(&relative, scope::line_of(original, site.start), scope::line_of(original, site.end))
                    // A function is in scope by its signature line too.
                    || scope.touches(&relative, site.line, site.line)
            });
        }
        files.retain(|f| !f.sites.is_empty() || f.path == root_file);
    }
    let total_sites: usize = files.iter().map(|f| f.sites.len()).sum();
    note(&format!("{} sites in {} files", total_sites, files.len()));

    let root_source = fs::read_to_string(&root_file)?;
    write_in_copy(
        &copy,
        &source_dir.join(format!("{RUNTIME_MOD}.rs")),
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
            write_in_copy(&copy, &file.path, &text)?;
            file.rendered = Some(rendered);
        }
        if !files.iter().any(|f| f.path == root_file) {
            write_in_copy(&copy, &root_file, &format!("{root_source}{root_suffix}"))?;
        }
        note(if rounds == 0 {
            "building the instrumented crate"
        } else {
            "rebuilding without the sites that failed to compile"
        });
        let output = Command::new("cargo")
            .args(["test", "--no-run", "--message-format=json"])
            .args(test_packages.iter().flat_map(|p| ["-p", p.name.as_str()]))
            .current_dir(&copy)
            .env("CARGO_TARGET_DIR", &build_target)
            .stderr(Stdio::piped())
            .output()
            .context("running cargo test --no-run")?;
        let names: Vec<&str> = test_packages.iter().map(|p| p.name.as_str()).collect();
        let build = parse_build_output(&String::from_utf8_lossy(&output.stdout), &names);
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
    };
    let mut runners: Vec<(String, Runner, PathBuf)> = vec![];
    for (name, exe, owner) in build.test_binaries {
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
        // Tests of other packages are named after their package.
        let name = if owner == package.name {
            name
        } else {
            format!("{owner}::{name}")
        };
        runners.push((name, Runner::Exe(exe), cwd));
    }
    if options.doc && is_kind(root_target, &["lib", "rlib", "proc-macro"]) {
        let doc = Runner::Doc {
            copy: copy.clone(),
            target_dir: build_target.clone(),
            package: package.name.clone(),
        };
        runners.push(("doc tests".to_string(), doc, session.package_dir.clone()));
    }
    for (index, (name, runner, cwd)) in runners.into_iter().enumerate() {
        let trace_path = work.join(format!("trace-{index}.tsv"));
        let _ = fs::remove_file(&trace_path);
        let mut bin = TestBinary {
            name,
            cwd,
            tests: BTreeSet::new(),
            runner,
            baseline: Duration::ZERO,
            trace: Trace::default(),
        };
        session.note(&format!("tracing `{}`", bin.name));
        let started = Instant::now();
        let trace_env = trace_path.to_string_lossy().into_owned();
        let outcome = run_binary(
            &bin,
            None,
            &[("TMT_TRACE", &trace_env)],
            Duration::from_secs(3600),
            false,
        )?;
        if outcome != Outcome::Pass {
            bail!(
                "the tests in `{}` do not pass on the unmutated code; fix them first",
                bin.name
            );
        }
        bin.baseline = started.elapsed();
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
            let listing = Command::new(exe)
                .args(["--list", "--format", "terse"])
                .current_dir(&bin.cwd)
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .output()
                .with_context(|| format!("listing the tests of `{}`", bin.name))?;
            bin.tests = parse_test_list(&String::from_utf8_lossy(&listing.stdout));
        }
        session.binaries.push(bin);
    }

    // 6. Plan every site, run the mutants, and put the findings together.
    let mut report = Report {
        package: package.name.clone(),
        sites: total_sites,
        skipped_sites: disabled.len(),
        ..Report::default()
    };
    let scratch = work.clone();
    let mut jobs: Vec<Job> = vec![];
    let mut work: Vec<SiteWork> = vec![];
    // `(index of the finding, job of its first surviving mutant)`.
    let mut effect_jobs: Vec<(usize, usize)> = vec![];
    for file in &files {
        for site in file.sites.iter().filter(|s| !disabled.contains(&s.id)) {
            let plan = match session.plan(&file.display, site, options.verify) {
                Planned::Unreached => {
                    report.unreached.push(report::location(&file.display, site));
                    continue;
                }
                Planned::Nothing => continue,
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
    let results = session.run_jobs(&jobs, options.jobs)?;
    report.mutants_run = results.len();
    report.mutants_killed = results.iter().filter(|r| r.survived.is_none()).count();
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
            session.trace_job(&jobs[effect_jobs[index].1], &scratch)
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
            if let Finding::AssertionGap { effects, .. } = &mut report.findings[*finding] {
                *effects = describe_effects(&changes, &sites);
            }
        }
    }

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
        Some(path) => crate::baseline::load(path)?,
        None => BTreeSet::new(),
    };
    let location = |finding: &Finding| match finding {
        Finding::InputGap { at, .. } | Finding::AssertionGap { at, .. } => {
            (at.file.clone(), at.line)
        }
    };
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
        .retain(|finding| !known.contains(&crate::baseline::fingerprint(finding)));
    report.suppressed = before - report.findings.len();
    Ok(report)
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
            "with the mutant, nothing else changes in these tests: they do not use the result of this code".to_string(),
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
            [("p".to_string(), PathBuf::from("/t/p-abc"), "p".to_string())]
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
    }

    #[test]
    fn workspace_dependents() {
        let package = |name: &str, deps: &[&str]| Package {
            name: name.to_string(),
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
        let found = instrument::discover(source, 0).unwrap().sites;
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
        assert_eq!(
            describe_effects(&BTreeMap::new(), &sites),
            [
                "with the mutant, nothing else changes in these tests: they do not use the result of this code"
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
}
