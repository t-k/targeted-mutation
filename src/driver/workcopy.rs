//! Isolated working copy synchronization, fixture confinement, module graph, and mark files.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

use crate::git_index;

use super::*;

pub(super) fn isolate_copy_command(command: &mut Command, work: &Path) {
    git_index::remove_git_overrides(command);
    command.env("GIT_CEILING_DIRECTORIES", work);
}

/// Name of the log of written files, beside the working copy.
pub(super) const WRITTEN_LOG: &str = ".tmt-written";

/// The working copy and what the runs did to it (formal/tla/WorkCopy.tla).
///
/// Most of the copy is plain copies of the user's files. The files a run instruments
/// are rewritten, and the log names them. The next run leaves those alone when it brings
/// the copy up to date, renders them again from the user's files, and writes only what
/// came out different: a file that keeps its modification time is not rebuilt by cargo.
/// What the log names and the run did not write again goes back to a plain copy.
pub(super) struct WorkCopy {
    pub(super) root: PathBuf,
    /// Directory inside `root` that corresponds to the user's `workspace`.
    /// Equal to `root` unless external parent-relative paths are staged above the workspace copy.
    pub(super) copy_root: PathBuf,
    pub(super) log: PathBuf,
    /// Paths of the copy that the current run wants to exist.
    pub(super) kept: RefCell<HashSet<PathBuf>>,
    /// Named by the log when the run started.
    pub(super) earlier: HashSet<PathBuf>,
    /// Those of `earlier` that the user's tree still has.
    pub(super) visited: RefCell<HashSet<PathBuf>>,
    /// Written by this run.
    pub(super) written: RefCell<HashSet<PathBuf>>,
    /// What the log names now.
    pub(super) logged: RefCell<HashSet<PathBuf>>,
}

impl WorkCopy {
    #[cfg(test)]
    pub(super) fn open(work: &Path, root: &Path) -> WorkCopy {
        Self::open_with_copy_root(work, root, root)
    }

    pub(super) fn open_with_copy_root(work: &Path, root: &Path, copy_root: &Path) -> WorkCopy {
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
            copy_root: copy_root.to_path_buf(),
            log,
            kept: RefCell::default(),
            logged: RefCell::new(earlier.clone()),
            earlier,
            visited: RefCell::default(),
            written: RefCell::default(),
        }
    }

    pub(super) fn is_kept(&self, path: &Path) -> bool {
        self.kept.borrow().contains(path) || self.earlier.contains(path)
    }

    /// The text of the user's file that `path` of the copy stands for. `None` for what
    /// only an earlier run left there.
    pub(super) fn original(&self, path: &Path, workspace: &Path) -> Result<Option<String>> {
        let source = if !self.earlier.contains(path) {
            path.to_path_buf() // a plain copy
        } else if !self.visited.borrow().contains(path) {
            return Ok(None);
        } else {
            workspace.join(path.strip_prefix(&self.copy_root).unwrap_or(path))
        };
        fs::read_to_string(&source)
            .map(Some)
            .with_context(|| format!("reading {}", source.display()))
    }

    /// Writes a file of the working copy. Refuses to write anywhere else, and never
    /// through a symbolic link. The log names the file before it changes.
    pub(super) fn write(&self, path: &Path, content: &str) -> Result<()> {
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

    pub(super) fn record(&self, path: &Path) -> Result<()> {
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
    pub(super) fn settle(&self, workspace: &Path) -> Result<()> {
        let written = self.written.borrow();
        for path in self.earlier.difference(&written) {
            if fs::symlink_metadata(path).is_ok() {
                fs::remove_file(path).with_context(|| format!("removing {}", path.display()))?;
            }
            if self.visited.borrow().contains(path) {
                let source = workspace.join(path.strip_prefix(&self.copy_root).unwrap_or(path));
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
pub(super) fn sync_file(src: &Path, dst: &Path, state: &WorkCopy) -> Result<()> {
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
pub(super) fn same_bytes(a: &Path, b: &Path) -> bool {
    let (Ok(a), Ok(b)) = (fs::File::open(a), fs::File::open(b)) else {
        return false;
    };
    compare_streams(a, b).unwrap_or(false)
}

/// Makes `dir` a directory of the working copy. Whatever else is in the way below the
/// copy's root goes first, a link above all: a link is followed, and what is written
/// "into the copy" would land where it points.
pub(super) fn real_dir(dir: &Path, state: &WorkCopy) -> Result<()> {
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
pub(super) fn copy_as_new(src: &Path, dst: &Path) -> Result<()> {
    fs::copy(src, dst).with_context(|| format!("copying {}", src.display()))?;
    // Opened for reading: the copy of a read-only file is read-only too.
    fs::File::open(dst)
        .and_then(|file| file.set_modified(std::time::SystemTime::now()))
        .with_context(|| format!("touching {}", dst.display()))
}

/// Removes from the working copy whatever the current run did not put there.
pub(super) fn prune(dir: &Path, state: &WorkCopy) -> Result<bool> {
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
pub(super) fn copy_entry(
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
        // Only check skipped ancestors strictly below `workspace` so a workspace situated
        // under a directory named e.g. `node_modules` does not drop its own symlinks.
        let skipped = if target.starts_with(workspace) {
            target
                .ancestors()
                .take_while(|ancestor| *ancestor != workspace && ancestor.starts_with(workspace))
                .any(&skip)
        } else {
            skip(&target)
        };
        if skipped {
            return Ok(());
        }
        if target.is_dir() {
            let contains_link = src
                .parent()
                .is_some_and(|dir| dir.canonicalize().is_ok_and(|dir| dir.starts_with(&target)));
            if !target.starts_with(workspace) || contains_link {
                return Ok(());
            }
            return copy_tree(&target, dst, workspace, skip, state);
        }
        if target.is_file() {
            sync_file(&target, dst, state)?;
        }
    } else if kind.is_dir() {
        // Build output and other caches (cargo marks its target directories this way).
        if !src.join("CACHEDIR.TAG").exists() {
            copy_tree(src, dst, workspace, skip, state)?;
        }
    } else if kind.is_file() {
        sync_file(src, dst, state)?;
    }
    Ok(())
}

pub(super) fn copy_tree(
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

/// An external file or directory staged relative to the isolated workspace copy,
/// for monorepo packages whose `build.rs` or `include_str!` references parent paths.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct StageExternal {
    /// Source path (workspace-relative or absolute).
    pub from: PathBuf,
    /// Relative destination from the isolated workspace copy (must ascend via `..`).
    pub rel: PathBuf,
}

impl std::str::FromStr for StageExternal {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        if text.bytes().any(|b| b < 0x20 || b == 0x7f) {
            bail!("--stage-external must not contain control characters");
        }
        let (from, rel) = match text.split_once('=') {
            Some((from, rel)) => (from, rel),
            None => (text, text),
        };
        if from.is_empty() {
            bail!("--stage-external source path must not be empty");
        }
        let rel = validate_external_rel(Path::new(rel))?;
        Ok(Self {
            from: PathBuf::from(from),
            rel,
        })
    }
}

/// Validates and normalizes a parent-relative staging target (e.g. `../../proto`).
/// Returns `(normalized_rel, parent_depth)`.
pub(super) fn parse_external_rel(path: &Path) -> Result<(PathBuf, usize)> {
    use std::path::Component;
    let mut parents = 0usize;
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                if tail.pop().is_none() {
                    parents += 1;
                }
            }
            Component::Normal(name) => tail.push(name.to_os_string()),
            Component::RootDir | Component::Prefix(_) => {
                bail!(
                    "--stage-external destination must be a relative path: {}",
                    path.display()
                );
            }
        }
    }
    if parents == 0 {
        bail!(
            "--stage-external destination must escape the workspace via `..` (use --copy-fixture for paths inside the workspace): {}",
            path.display()
        );
    }
    if tail.is_empty() {
        bail!(
            "--stage-external destination must name a file or directory under a parent, not the parent itself: {}",
            path.display()
        );
    }
    let mut normalized = PathBuf::new();
    for _ in 0..parents {
        normalized.push("..");
    }
    for name in tail {
        normalized.push(name);
    }
    Ok((normalized, parents))
}

fn validate_external_rel(path: &Path) -> Result<PathBuf> {
    parse_external_rel(path).map(|(normalized, _)| normalized)
}

/// Computes the subdirectory inside `<work>/src` where the workspace copy should live
/// so that every `--stage-external` parent-relative target stays confined under `<work>/src`.
pub(super) fn staged_copy_subdir(workspace: &Path, staged: &[StageExternal]) -> Result<PathBuf> {
    if staged.is_empty() {
        return Ok(PathBuf::new());
    }
    let mut max_depth = 0usize;
    for entry in staged {
        let (_, depth) = parse_external_rel(&entry.rel)?;
        max_depth = max_depth.max(depth);
    }
    let repo_prefix = super::repository_prefix(workspace);
    let mut parts: Vec<String> = Path::new(&repo_prefix)
        .components()
        .filter_map(|part| match part {
            std::path::Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let mut pad = 0usize;
    while parts.len() < max_depth {
        parts.insert(0, format!("_parent_{pad}"));
        pad += 1;
    }
    Ok(parts.into_iter().collect())
}

/// Copies an external file or directory into the confined staging area under `<work>/src`.
pub(super) fn copy_external_entry(
    from: &Path,
    to: &Path,
    work: &Path,
    forbidden: &dyn Fn(&Path) -> bool,
    state: &WorkCopy,
    visiting: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let resolved = from
        .canonicalize()
        .with_context(|| format!("resolving external path {}", from.display()))?;
    if resolved.starts_with(work)
        || resolved
            .components()
            .any(|part| part.as_os_str() == ".git")
        || forbidden(&resolved)
    {
        bail!(
            "--stage-external source resolves to a forbidden path: {}",
            resolved.display()
        );
    }
    let kind = fs::metadata(&resolved)?.file_type();
    if let Some(parent) = to.parent() {
        real_dir(parent, state)?;
    }
    if kind.is_file() {
        return sync_file(&resolved, to, state);
    }
    if !kind.is_dir() || !visiting.insert(resolved.clone()) {
        bail!(
            "--stage-external source {} is a cycle or is not a regular file or directory",
            from.display()
        );
    }
    real_dir(to, state)?;
    state.kept.borrow_mut().insert(to.to_path_buf());
    for entry in fs::read_dir(&resolved).with_context(|| format!("reading {}", resolved.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == ".git" || n == "node_modules")
            || (path.is_dir() && path.join("CACHEDIR.TAG").exists())
            || forbidden(&path)
        {
            continue;
        }
        copy_external_entry(
            &path,
            &to.join(entry.file_name()),
            work,
            forbidden,
            state,
            visiting,
        )?;
    }
    visiting.remove(&resolved);
    Ok(())
}

pub(super) fn fixture_relative(path: &Path) -> Result<PathBuf> {
    use std::path::Component;
    let relative = path
        .components()
        .filter(|part| *part != Component::CurDir)
        .collect::<PathBuf>();
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        bail!(
            "fixture path must be inside the workspace: {}",
            path.display()
        );
    }
    Ok(relative)
}

pub(super) fn wrapper_relative(path: &Path) -> Result<PathBuf> {
    use std::path::Component;
    let raw = path.to_string_lossy();
    if raw.bytes().any(|b| b < 0x20 || b == 0x7f) {
        bail!(
            "--test-wrapper must not contain control characters: {}",
            path.display()
        );
    }
    let relative = path
        .components()
        .filter(|part| *part != Component::CurDir)
        .collect::<PathBuf>();
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        bail!(
            "--test-wrapper must be a path inside the workspace: {}",
            path.display()
        );
    }
    Ok(relative)
}

pub(super) fn fixture_exclusions(
    workspace: &Path,
    roots: &[PathBuf],
    excluded: &[PathBuf],
) -> Result<BTreeSet<PathBuf>> {
    excluded
        .iter()
        .map(|path| {
            let relative = fixture_relative(path)?;
            if !roots
                .iter()
                .any(|root| relative != *root && relative.starts_with(root))
            {
                bail!(
                    "--copy-fixture-exclude must name an entry under a --copy-fixture: {}",
                    path.display()
                );
            }
            let path = workspace.join(relative);
            fs::symlink_metadata(&path)
                .with_context(|| format!("excluding fixture {}", path.display()))?;
            confined_fixture_path(workspace, &path)?;
            Ok(path)
        })
        .collect()
}

pub(super) fn confined_fixture_path(workspace: &Path, path: &Path) -> Result<()> {
    use std::collections::VecDeque;
    use std::path::Component;

    let parts = |relative: &Path| {
        relative
            .components()
            .map(|part| part.as_os_str().to_owned())
            .collect::<Vec<_>>()
    };
    let mut pending = VecDeque::from(parts(path.strip_prefix(workspace)?));
    let mut resolved = workspace.to_path_buf();
    let mut hops = 0;
    while let Some(part) = pending.pop_front() {
        match Path::new(&part).components().next() {
            Some(Component::ParentDir) if resolved != workspace => {
                resolved.pop();
            }
            Some(Component::CurDir) => {}
            Some(Component::Normal(_)) => {
                resolved.push(part);
                match fs::symlink_metadata(&resolved) {
                    Ok(metadata) if metadata.is_symlink() => {
                        hops += 1;
                        if hops > 40 {
                            bail!("fixture {} contains a symbolic link cycle", path.display());
                        }
                        let target = fs::read_link(&resolved)?;
                        resolved.pop();
                        let target = if target.is_absolute() {
                            resolved = workspace.to_path_buf();
                            target.strip_prefix(workspace).with_context(|| {
                                format!("fixture {} points outside the workspace", path.display())
                            })?
                        } else {
                            &target
                        };
                        for component in parts(target).into_iter().rev() {
                            pending.push_front(component);
                        }
                    }
                    Ok(_) => {}
                    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                    Err(err) => {
                        return Err(err)
                            .with_context(|| format!("checking fixture {}", path.display()));
                    }
                }
            }
            _ => bail!("fixture {} points outside the workspace", path.display()),
        }
    }
    Ok(())
}

/// An explicitly selected fixture may include ignored directories. Resolve links to
/// their contents inside the workspace, without following them into a parent cycle.
pub(super) fn copy_fixture_entry(
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
        let destination = to.join(entry.file_name());
        let projected = workspace.join(
            destination
                .strip_prefix(&state.root)
                .expect("fixture under copy"),
        );
        if !forbidden(&path) && !forbidden(&projected) {
            copy_fixture_entry(&path, &destination, workspace, forbidden, state, visiting)?;
        }
    }
    visiting.remove(&resolved);
    Ok(())
}

/// The files under `dir` that git tracks or would track (not ignored), relative to `dir`.
/// `None` when `dir` is not in a git work tree, or is itself ignored: then git has nothing
/// to say about which files matter.
pub(super) fn git_visible_files(dir: &Path) -> Result<Option<Vec<PathBuf>>> {
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
pub(super) fn copy_git_visible(
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
pub(super) struct ModuleOwners {
    pub(super) production: BTreeSet<PathBuf>,
    pub(super) consumers: BTreeSet<PathBuf>,
}

pub(super) fn source_graph(
    roots: impl IntoIterator<Item = (PathBuf, bool)>,
    copy: &Path,
    mut original: impl FnMut(&Path) -> Result<Option<String>>,
) -> Result<BTreeMap<PathBuf, ModuleOwners>> {
    use std::collections::{VecDeque, btree_map::Entry};
    let mut files: BTreeMap<PathBuf, ModuleOwners> = BTreeMap::new();
    let mut edges = BTreeMap::new();
    for (root, production) in roots {
        let directory = root.parent().expect("Cargo target directory").to_path_buf();
        let mut pending = VecDeque::from([(root.clone(), directory, production)]);
        let mut seen = HashSet::new();
        while let Some((path, directory, active)) = pending.pop_front() {
            if !seen.insert((path.clone(), directory.clone(), active)) {
                continue;
            }
            // A file can resolve relative modules differently as a target root or a module.
            // Keep eligibility from the file itself so this can also be reused by test roots.
            let children = match edges.entry((path.clone(), directory.clone())) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let children = original(&path)?.map(|source| {
                        crate::modules::external(&source, &path, &directory)
                            .into_iter()
                            .filter_map(|module| {
                                let file = module
                                    .paths
                                    .iter()
                                    .map(|path| normalize(path))
                                    .find(|path| path.starts_with(copy) && path.is_file())?;
                                Some((file, normalize(&module.directory), module.production))
                            })
                            .collect::<Vec<_>>()
                    });
                    entry.insert(children)
                }
            };
            let Some(children) = children else {
                continue;
            };
            let record = files.entry(path).or_default();
            record.consumers.insert(root.clone());
            if active {
                record.production.insert(root.clone());
            }
            pending.extend(children.iter().map(|(child, directory, production)| {
                (child.clone(), directory.clone(), active && *production)
            }));
        }
    }
    files.retain(|_, owners| !owners.production.is_empty());
    Ok(files)
}

/// Resolves `.` and `..` without touching the file system.
pub(super) fn normalize(path: &Path) -> PathBuf {
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

pub(super) fn relative_source(path: &Path, package: &Path) -> PathBuf {
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
pub(super) fn absolutize_path_dependencies(
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

/// The directory of the marks in a working directory: made if it is not there, used if
/// it is one. Anything else of that name (a file, a link) is somebody's, in a directory
/// that was given to work in before it was this tool's: it is left alone and said.
pub(super) fn marks_dir(work: &Path) -> Result<PathBuf> {
    work_directory(work, "marks")
}

pub(super) fn work_directory(work: &Path, name: &str) -> Result<PathBuf> {
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
pub(super) fn reserve_file(
    dir: &Path,
    label: &str,
    runs: &std::sync::atomic::AtomicUsize,
) -> Result<PathBuf> {
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

/// Takes the mark away after the run and says whether the run had given up (the mark
/// was gone). Anything else that keeps it from being taken away leaves the question
/// open, and is an error rather than an answer.
pub(super) fn release_mark(mark: &Path) -> Result<bool> {
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
