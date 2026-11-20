//! A disposable Git index for tests that enumerate files tracked in the source workspace.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};

use anyhow::{Context, Result, bail};

/// Clear environment variables that can redirect Git away from the working directory.
pub(crate) fn remove_git_overrides(command: &mut Command) {
    // Repository-local variables from `git rev-parse --local-env-vars`, plus the
    // namespace used by some server-side hooks.
    for key in [
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CONFIG",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_OBJECT_DIRECTORY",
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_IMPLICIT_WORK_TREE",
        "GIT_GRAFT_FILE",
        "GIT_INDEX_FILE",
        "GIT_NO_REPLACE_OBJECTS",
        "GIT_REPLACE_REF_BASE",
        "GIT_PREFIX",
        "GIT_SHALLOW_FILE",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
    ] {
        command.env_remove(key);
    }
}

fn git(dir: &Path) -> Command {
    let mut command = Command::new("git");
    command.current_dir(dir);
    remove_git_overrides(&mut command);
    command
}

fn output(result: Output, operation: &str) -> Result<Vec<u8>> {
    if !result.status.success() {
        bail!(
            "git {operation} failed: {}",
            String::from_utf8_lossy(&result.stderr).trim()
        );
    }
    Ok(result.stdout)
}

/// Copy the workspace-relative stage entries without linking to the source repository.
pub(crate) fn snapshot(workspace: &Path, copy: &Path) -> Result<()> {
    if !fs::symlink_metadata(copy)?.is_dir() {
        bail!(
            "cannot make a Git index in {}: not a directory",
            copy.display()
        );
    }
    let metadata = copy.join(".git");
    match fs::symlink_metadata(&metadata) {
        Err(err) if err.kind() == ErrorKind::NotFound => {}
        Err(err) => return Err(err).with_context(|| format!("inspecting {}", metadata.display())),
        Ok(_) => bail!("refusing to replace Git metadata in {}", metadata.display()),
    }

    let format = output(
        git(workspace)
            .args(["rev-parse", "--show-object-format"])
            .output()
            .context("reading the source Git object format")?,
        "rev-parse --show-object-format",
    )?;
    let format = std::str::from_utf8(&format)?.trim();
    let entries = output(
        git(workspace)
            .args(["ls-files", "--cached", "--stage", "-z"])
            .output()
            .context("reading the source Git index")?,
        "ls-files --cached --stage",
    )?;
    output(
        git(copy)
            .args(["init", "--quiet", "--template="])
            .arg(format!("--object-format={format}"))
            .output()
            .context("creating an isolated Git repository")?,
        "init",
    )?;

    let mut child = git(copy)
        .args(["update-index", "-z", "--index-info"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("starting Git to write the isolated index")?;
    let written = child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(&entries);
    let updated = child
        .wait_with_output()
        .context("waiting for Git to write the isolated index")?;
    output(updated, "update-index --index-info")
        .and_then(|_| written.context("writing the isolated Git index"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::process::Command;

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> Self {
            let path = std::env::temp_dir().join(format!("tmt-{name}-{}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn git(dir: &Path, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git")
            .current_dir(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    #[test]
    fn every_copy_command_excludes_ambient_repository_overrides() {
        let command = super::git(Path::new("."));
        let overrides = command
            .get_envs()
            .collect::<std::collections::BTreeMap<_, _>>();
        for name in [
            "GIT_DIR",
            "GIT_INDEX_FILE",
            "GIT_WORK_TREE",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        ] {
            assert_eq!(
                overrides.get(std::ffi::OsStr::new(name)),
                Some(&None),
                "{name}"
            );
        }
    }

    #[test]
    fn an_index_snapshot_keeps_staged_paths_relative_to_the_workspace() {
        let source = TestDirectory::new("index-source");
        let copy = TestDirectory::new("index-copy");
        let workspace = source.0.join("workspace");
        fs::create_dir(&workspace).unwrap();
        git(&source.0, &["init", "--quiet", "--object-format=sha256"]);
        git(
            &source.0,
            &["remote", "add", "origin", "source-only-remote"],
        );
        fs::write(source.0.join("outside"), "outside workspace").unwrap();
        fs::write(workspace.join("manifest"), "staged").unwrap();
        fs::write(workspace.join("deleted path"), "staged").unwrap();
        fs::write(workspace.join("unicode-猫"), "staged").unwrap();
        git(&source.0, &["add", "."]);
        fs::write(workspace.join("manifest"), "changed").unwrap();
        fs::remove_file(workspace.join("deleted path")).unwrap();
        fs::write(workspace.join("untracked"), "untracked").unwrap();
        fs::write(copy.0.join("manifest"), "changed").unwrap();

        snapshot(&workspace, &copy.0).unwrap();

        assert_eq!(
            git(&copy.0, &["ls-files", "--stage", "-z"]),
            git(&workspace, &["ls-files", "--stage", "-z"])
        );
        assert_eq!(
            git(&copy.0, &["rev-parse", "--show-object-format"]),
            b"sha256\n"
        );
        assert!(git(&copy.0, &["remote"]).is_empty());
        assert!(!copy.0.join(".git/objects/info/alternates").exists());
        assert!(!copy.0.join(".git/hooks").exists());
    }

    #[test]
    fn an_index_snapshot_refuses_existing_git_metadata() {
        let source = TestDirectory::new("index-existing-source");
        let copy = TestDirectory::new("index-existing-copy");
        git(&source.0, &["init", "--quiet"]);
        let metadata = copy.0.join(".git");
        fs::write(&metadata, "leave this alone").unwrap();

        assert!(snapshot(&source.0, &copy.0).is_err());
        assert_eq!(fs::read_to_string(metadata).unwrap(), "leave this alone");
    }
}
