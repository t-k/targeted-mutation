//! Runtime assets declared relative to the workspace and isolated Cargo output.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

const OWNERSHIP: &str = ".tmt-output-fixtures.json";
const OWNERSHIP_TEMP: &str = ".tmt-output-fixtures-new";
const STAGING: &str = ".tmt-output-fixture-stage";

/// A workspace input and its destination inside TMT's isolated Cargo output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Fixture {
    pub from: PathBuf,
    pub to: PathBuf,
}

impl std::str::FromStr for Fixture {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        let (from, to) = text
            .split_once('=')
            .context("expected a fixture mapping FROM=TO")?;
        Ok(Self {
            from: relative(Path::new(from))?,
            to: relative(Path::new(to))?,
        })
    }
}

fn relative(path: &Path) -> Result<PathBuf> {
    let mut clean = PathBuf::new();
    for part in path.components() {
        match part {
            Component::Normal(name) => clean.push(name),
            Component::CurDir => {}
            _ => bail!(
                "output fixture path must be relative and confined: {}",
                path.display()
            ),
        }
    }
    if clean.as_os_str().is_empty() {
        bail!("output fixture path must name a file or directory");
    }
    Ok(clean)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    File,
    Directory,
}

enum Planned {
    File(PathBuf),
    Directory { shared: bool },
}

impl Planned {
    fn kind(&self) -> Kind {
        match self {
            Self::File(_) => Kind::File,
            Self::Directory { .. } => Kind::Directory,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
struct Stamp {
    length: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    device_and_inode: (u64, u64),
}

impl Stamp {
    fn from_metadata(metadata: &fs::Metadata) -> Result<Self> {
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt as _;
        Ok(Self {
            length: metadata.len(),
            modified: metadata.modified()?,
            #[cfg(unix)]
            device_and_inode: (metadata.dev(), metadata.ino()),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Owned {
    File(Vec<Stamp>),
    Directory,
}

impl Owned {
    fn check(&self, metadata: &fs::Metadata) -> Result<Option<Self>> {
        Ok(match self {
            Self::File(stamps) if metadata.is_file() => {
                let current = Stamp::from_metadata(metadata)?;
                stamps.contains(&current).then(|| Self::File(vec![current]))
            }
            Self::Directory if metadata.is_dir() => Some(Self::Directory),
            _ => None,
        })
    }

    fn kind(&self) -> Kind {
        match self {
            Self::File(_) => Kind::File,
            Self::Directory => Kind::Directory,
        }
    }
}

type Ownership = BTreeMap<PathBuf, Owned>;

/// The caller holds the work-directory lock and invokes staging before running tests.
pub(crate) fn stage(
    workspace: &Path,
    work: &Path,
    target_dir: &Path,
    fixtures: &[Fixture],
) -> Result<()> {
    let work = work.canonicalize()?;
    let target = target_dir.canonicalize()?;
    if target == work || !target.starts_with(&work) || !fs::symlink_metadata(target_dir)?.is_dir() {
        bail!("the output fixture target must be a directory inside the TMT work directory");
    }

    let recorded = read_ownership(&work.join(OWNERSHIP))?;
    if fixtures.is_empty() && recorded.is_empty() {
        return Ok(());
    }
    let workspace = workspace.canonicalize()?;
    let mut plan = BTreeMap::new();
    let mut destinations = BTreeSet::new();
    for fixture in fixtures {
        let from = relative(&fixture.from)?;
        let to = relative(&fixture.to)?;
        if destinations
            .iter()
            .any(|other: &PathBuf| to.starts_with(other) || other.starts_with(&to))
        {
            bail!("overlapping output fixture destination: {}", to.display());
        }
        for parent in to
            .ancestors()
            .skip(1)
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            plan.entry(parent.to_path_buf())
                .or_insert(Planned::Directory { shared: true });
        }
        plan_source(
            &workspace.join(from),
            &to,
            &workspace,
            &work,
            &mut plan,
            &mut BTreeSet::new(),
        )?;
        destinations.insert(to);
    }

    // A replaced file is no longer ours. Cargo can choose a different output
    // path in a later build, and tests may create files of their own.
    let mut previous = Ownership::new();
    for (path, receipt) in &recorded {
        if let Some(metadata) = output_metadata(&target, path)?
            && let Some(current) = receipt.check(&metadata)?
        {
            previous.insert(path.clone(), current);
        }
    }

    // Validate the whole plan before modifying Cargo output.
    let mut shared = BTreeSet::new();
    for (path, entry) in &plan {
        if let Some(metadata) = output_metadata(&target, path)? {
            let shared_directory = matches!(entry, Planned::Directory { shared: true });
            let kind_matches = match entry.kind() {
                Kind::File => metadata.is_file(),
                Kind::Directory => metadata.is_dir(),
            };
            if !kind_matches || !shared_directory && !previous.contains_key(path) {
                bail!(
                    "output fixture destination already exists: {}",
                    path.display()
                );
            }
            if shared_directory && !previous.contains_key(path) {
                shared.insert(path.clone());
            }
        }
    }

    // Prepare complete files privately. The ownership journal can describe both
    // sides of each rename, even if the next run starts after an interruption.
    let staging = Staging::new(work.join(STAGING))?;
    let mut wanted = Ownership::new();
    let mut prepared = BTreeMap::new();
    for (index, (path, entry)) in plan.iter().enumerate() {
        let receipt = match entry {
            Planned::Directory { .. } if shared.contains(path) => continue,
            Planned::Directory { .. } => Owned::Directory,
            Planned::File(from) => {
                let temporary = staging.0.join(index.to_string());
                copy_file(from, &temporary)?;
                let stamp = Stamp::from_metadata(&fs::metadata(&temporary)?)?;
                prepared.insert(path.clone(), temporary);
                Owned::File(vec![stamp])
            }
        };
        wanted.insert(path.clone(), receipt);
    }

    let mut pending = previous.clone();
    for (path, receipt) in &wanted {
        match (pending.get_mut(path), receipt) {
            (Some(Owned::File(old)), Owned::File(fresh)) => old.extend(fresh.iter().cloned()),
            _ => {
                pending.insert(path.clone(), receipt.clone());
            }
        }
    }
    if pending != recorded {
        write_ownership(&work, &pending)?;
    }

    for (path, receipt) in previous.iter().rev() {
        if !wanted.contains_key(path) {
            remove_owned(&target.join(path), receipt.kind())?;
        }
    }
    for (path, entry) in plan {
        let output = target.join(&path);
        match entry {
            Planned::Directory { .. } if !output.exists() => {
                fs::create_dir(&output).with_context(|| {
                    format!("creating output fixture directory {}", output.display())
                })?
            }
            Planned::Directory { .. } => {}
            Planned::File(_) => {
                fs::rename(&prepared[&path], &output)
                    .with_context(|| format!("installing output fixture {}", output.display()))?;
            }
        }
    }
    if pending != wanted {
        write_ownership(&work, &wanted)?;
    }

    Ok(())
}

fn plan_source(
    from: &Path,
    to: &Path,
    workspace: &Path,
    work: &Path,
    plan: &mut BTreeMap<PathBuf, Planned>,
    visiting: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let resolved = from
        .canonicalize()
        .with_context(|| format!("resolving output fixture source {}", from.display()))?;
    let allowed = |path: &Path| {
        path.strip_prefix(workspace).is_ok_and(|relative| {
            !relative.as_os_str().is_empty()
                && !path.starts_with(work)
                && !relative.starts_with("target")
                && !relative.components().any(|part| part.as_os_str() == ".git")
        })
    };
    if !allowed(from) || !allowed(&resolved) {
        bail!(
            "output fixture source {} resolves outside the allowed workspace inputs: {}",
            from.display(),
            resolved.display()
        );
    }
    let metadata = fs::metadata(&resolved)?;
    if metadata.is_file() {
        plan.insert(to.to_path_buf(), Planned::File(resolved));
    } else if metadata.is_dir() && visiting.insert(resolved.clone()) {
        plan.insert(to.to_path_buf(), Planned::Directory { shared: false });
        for child in fs::read_dir(&resolved)? {
            let child = child?;
            plan_source(
                &child.path(),
                &to.join(child.file_name()),
                workspace,
                work,
                plan,
                visiting,
            )?;
        }
        visiting.remove(&resolved);
    } else {
        bail!(
            "output fixture source {} is a cycle or has an unsupported file type",
            from.display()
        );
    }
    Ok(())
}

fn output_metadata(target: &Path, relative: &Path) -> Result<Option<fs::Metadata>> {
    let mut parts = relative.components().peekable();
    let mut current = target.to_path_buf();
    while let Some(part) = parts.next() {
        current.push(part);
        let Some(metadata) = inspect(&current)? else {
            return Ok(None);
        };
        if parts.peek().is_none() && (metadata.is_dir() || metadata.is_file()) {
            return Ok(Some(metadata));
        }
        if !metadata.is_dir() {
            bail!(
                "output fixture destination crosses an unsafe path: {}",
                current.display()
            );
        }
    }
    unreachable!("relative output fixture paths are nonempty")
}

fn inspect(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("inspecting {}", path.display())),
    }
}

fn remove_owned(path: &Path, kind: Kind) -> Result<()> {
    let result = match kind {
        Kind::File => fs::remove_file(path),
        Kind::Directory => fs::remove_dir(path),
    };
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        // Tests or Cargo may have created their own files under a former fixture.
        Err(error) if kind == Kind::Directory && error.kind() == ErrorKind::DirectoryNotEmpty => {
            Ok(())
        }
        Err(error) => {
            Err(error).with_context(|| format!("removing old output fixture {}", path.display()))
        }
    }
}

struct Staging(PathBuf);

impl Staging {
    fn new(path: PathBuf) -> Result<Self> {
        if let Some(existing) = inspect(&path)? {
            if !existing.is_dir() {
                bail!(
                    "output fixture staging path is not a directory: {}",
                    path.display()
                );
            }
            fs::remove_dir_all(&path)?;
        }
        fs::create_dir(&path)?;
        Ok(Self(path))
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn copy_file(from: &Path, to: &Path) -> Result<()> {
    let copy = || -> io::Result<()> {
        let mut input = File::open(from)?;
        let mut output = OpenOptions::new().write(true).create_new(true).open(to)?;
        io::copy(&mut input, &mut output)?;
        output.set_permissions(input.metadata()?.permissions())?;
        Ok(())
    };
    copy().with_context(|| {
        format!(
            "copying output fixture {} to {}",
            from.display(),
            to.display()
        )
    })
}

fn read_ownership(path: &Path) -> Result<Ownership> {
    let Some(metadata) = inspect(path)? else {
        return Ok(Ownership::new());
    };
    if !metadata.is_file() {
        bail!(
            "output fixture ownership is not a regular file: {}",
            path.display()
        );
    }
    let owned: Ownership = serde_json::from_reader(io::BufReader::new(File::open(path)?))
        .with_context(|| format!("reading output fixture ownership {}", path.display()))?;
    for entry in owned.keys() {
        if relative(entry)? != *entry {
            bail!("invalid output fixture ownership path: {}", entry.display());
        }
    }
    Ok(owned)
}

fn write_ownership(work: &Path, owned: &Ownership) -> Result<()> {
    let temporary = work.join(OWNERSHIP_TEMP);
    if let Some(existing) = inspect(&temporary)? {
        if !existing.is_file() {
            bail!(
                "output fixture ownership staging path is not a regular file: {}",
                temporary.display()
            );
        }
        fs::remove_file(&temporary)?;
    }
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)?;
    serde_json::to_writer(&file, owned)?;
    file.sync_all()?;
    drop(file);
    fs::rename(temporary, work.join(OWNERSHIP)).context("committing output fixture ownership")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Tree {
        workspace: PathBuf,
        work: PathBuf,
        target: PathBuf,
    }

    impl Tree {
        fn new() -> Self {
            static NEXT: AtomicUsize = AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "tmt-output-fixture-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir(&root).unwrap();
            let work = root.join("work");
            let target = work.join("target");
            fs::create_dir(&work).unwrap();
            fs::create_dir(&target).unwrap();
            Self {
                workspace: root.canonicalize().unwrap(),
                work,
                target,
            }
        }

        fn stage(&self, fixtures: &[Fixture]) -> Result<()> {
            stage(&self.workspace, &self.work, &self.target, fixtures)
        }
    }

    impl Drop for Tree {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.workspace).unwrap();
        }
    }

    fn mapping(from: &str, to: &str) -> Fixture {
        Fixture {
            from: from.into(),
            to: to.into(),
        }
    }

    #[test]
    fn paths_must_remain_relative_to_their_roots() {
        let fixture: Fixture = "./runtime/app.mjs=debug/runtime/app.mjs".parse().unwrap();
        assert_eq!(fixture, mapping("runtime/app.mjs", "debug/runtime/app.mjs"));
        for text in [
            "missing-separator",
            "=debug/asset",
            "runtime=",
            "/tmp/asset=debug",
            "runtime=../output",
            "runtime/../escape=debug",
            "runtime=.",
            "runtime=/tmp/asset",
        ] {
            assert!(text.parse::<Fixture>().is_err(), "{text}");
        }
        assert_eq!(
            serde_json::to_value(fixture).unwrap(),
            serde_json::json!({"from": "runtime/app.mjs", "to": "debug/runtime/app.mjs"})
        );
    }

    #[test]
    fn updates_owned_assets_and_preserves_unowned_outputs() {
        let tree = Tree::new();
        let input = tree.workspace.join("runtime");
        fs::create_dir(&input).unwrap();
        fs::write(input.join("old.mjs"), "first").unwrap();
        fs::write(input.join("always.mjs"), "first").unwrap();
        fs::create_dir(tree.target.join("debug")).unwrap();
        fs::write(tree.target.join("debug/cargo-output"), "cargo").unwrap();
        let asset = mapping("runtime", "debug/runtime");
        tree.stage(std::slice::from_ref(&asset)).unwrap();
        assert_eq!(
            fs::read_to_string(tree.target.join("debug/runtime/old.mjs")).unwrap(),
            "first"
        );

        fs::remove_file(input.join("old.mjs")).unwrap();
        fs::write(input.join("new.mjs"), "second").unwrap();
        fs::write(input.join("always.mjs"), "updated").unwrap();
        fs::write(tree.target.join("debug/runtime/test-output"), "keep").unwrap();
        tree.stage(&[asset]).unwrap();

        assert!(!tree.target.join("debug/runtime/old.mjs").exists());
        assert_eq!(
            fs::read_to_string(tree.target.join("debug/runtime/new.mjs")).unwrap(),
            "second"
        );
        assert_eq!(
            fs::read_to_string(tree.target.join("debug/runtime/always.mjs")).unwrap(),
            "updated"
        );
        tree.stage(&[]).unwrap();
        assert!(!tree.target.join("debug/runtime/new.mjs").exists());
        assert!(!tree.target.join("debug/runtime/always.mjs").exists());
        assert_eq!(
            fs::read_to_string(tree.target.join("debug/runtime/test-output")).unwrap(),
            "keep"
        );
        assert_eq!(
            fs::read_to_string(tree.target.join("debug/cargo-output")).unwrap(),
            "cargo"
        );
        assert!(tree.stage(&[mapping("runtime", "debug/runtime")]).is_err());
    }

    #[test]
    fn does_not_replace_a_new_file_at_a_previously_owned_path() {
        let tree = Tree::new();
        fs::write(tree.workspace.join("asset"), "fixture data").unwrap();
        let asset = mapping("asset", "debug/asset");
        tree.stage(std::slice::from_ref(&asset)).unwrap();
        let output = tree.target.join("debug/asset");
        let new_artifact = tree.target.join("replacement");
        fs::write(&new_artifact, "an artifact Cargo just created").unwrap();
        fs::rename(&new_artifact, &output).unwrap();

        assert!(tree.stage(std::slice::from_ref(&asset)).is_err());
        assert_eq!(
            fs::read_to_string(&output).unwrap(),
            "an artifact Cargo just created"
        );
        tree.stage(&[]).unwrap();
        assert_eq!(
            fs::read_to_string(output).unwrap(),
            "an artifact Cargo just created"
        );
    }

    #[test]
    fn validates_all_destinations_before_copying() {
        let tree = Tree::new();
        fs::write(tree.workspace.join("asset"), "content").unwrap();
        fs::write(tree.target.join("cargo-output"), "build artifact").unwrap();
        assert!(
            tree.stage(&[mapping("asset", "new"), mapping("asset", "cargo-output")])
                .is_err()
        );
        assert!(!tree.target.join("new").exists());
        assert_eq!(
            fs::read_to_string(tree.target.join("cargo-output")).unwrap(),
            "build artifact"
        );
        assert!(
            tree.stage(&[mapping("asset", "nested"), mapping("asset", "nested/entry")])
                .is_err()
        );
        assert!(!tree.target.join("nested").exists());
    }

    #[cfg(unix)]
    #[test]
    fn output_fixture_replays_pict_table() {
        use std::os::unix::fs::symlink;
        let Some(table) = std::env::var_os("TMT_FIXTURE_PICT_TABLE") else {
            return;
        };
        let data = fs::read_to_string(table).unwrap();
        for row in data.lines().skip(1) {
            let values: Vec<_> = row.trim().split('\t').collect();
            let [source, destination, parent] = values[..] else {
                panic!("unexpected model row: {row}");
            };
            let tree = Tree::new();
            let input = tree.workspace.join("asset");
            let ordinary = tree.workspace.join("ordinary");
            fs::write(&ordinary, "value").unwrap();
            match source {
                "File" => fs::write(&input, "value").unwrap(),
                "Directory" => {
                    fs::create_dir(&input).unwrap();
                    fs::write(input.join("entry"), "value").unwrap();
                }
                "LocalLink" => symlink(&ordinary, &input).unwrap(),
                "OutsideLink" => symlink("/dev/null", &input).unwrap(),
                "DanglingLink" => symlink("missing", &input).unwrap(),
                _ => panic!("{row}"),
            }
            let output = tree.target.join("debug/asset");
            if parent == "Present" {
                fs::create_dir(output.parent().unwrap()).unwrap();
            }
            match destination {
                "Vacant" => {}
                "Occupied" => fs::write(&output, "unowned").unwrap(),
                "Link" => symlink(&ordinary, &output).unwrap(),
                _ => panic!("{row}"),
            }
            let accepted =
                matches!(source, "File" | "Directory" | "LocalLink") && destination == "Vacant";
            let result = tree.stage(&[mapping("asset", "debug/asset")]);
            assert_eq!(result.is_ok(), accepted, "{row}: {result:?}");
            if accepted {
                let file = if source == "Directory" {
                    output.join("entry")
                } else {
                    output
                };
                assert_eq!(fs::read_to_string(file).unwrap(), "value", "{row}");
            } else if destination == "Occupied" {
                assert_eq!(fs::read_to_string(output).unwrap(), "unowned", "{row}");
            } else if destination == "Link" {
                assert_eq!(fs::read_link(output).unwrap(), ordinary, "{row}");
            } else {
                assert!(!output.exists(), "{row}");
            }
            assert_eq!(fs::read_to_string(ordinary).unwrap(), "value", "{row}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn refuses_ancestor_links_and_source_cycles() {
        use std::os::unix::fs::symlink;
        let tree = Tree::new();
        fs::write(tree.workspace.join("asset"), "content").unwrap();
        let outside = tree.workspace.join("outside-target");
        fs::create_dir(&outside).unwrap();
        symlink(&outside, tree.target.join("linked")).unwrap();
        assert!(tree.stage(&[mapping("asset", "linked/written")]).is_err());
        assert!(!outside.join("written").exists());

        let cyclic = tree.workspace.join("cycle");
        fs::create_dir(&cyclic).unwrap();
        symlink(&cyclic, cyclic.join("again")).unwrap();
        assert!(tree.stage(&[mapping("cycle", "debug/cycle")]).is_err());
        assert!(!tree.target.join("debug/cycle").exists());
    }
}
