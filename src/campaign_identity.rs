//! Fingerprint the effective inputs before cached outcomes are considered.

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, Result, ensure};
use sha2::{Digest, Sha256};

fn frame(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn hash_path(hash: &mut Sha256, path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    frame(hash, path.as_os_str().as_encoded_bytes());
    if metadata.is_symlink() {
        frame(hash, b"link");
        frame(hash, fs::read_link(path)?.as_os_str().as_encoded_bytes());
    } else if metadata.is_file() {
        frame(hash, b"file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            hash.update(metadata.permissions().mode().to_le_bytes());
        }
        let mut file = fs::File::open(path)?;
        hash.update(metadata.len().to_le_bytes());
        let mut left = metadata.len();
        let mut buffer = [0_u8; 64 * 1024];
        while left > 0 {
            let size = left.min(buffer.len() as u64) as usize;
            file.read_exact(&mut buffer[..size])?;
            hash.update(&buffer[..size]);
            left -= size as u64;
        }
        ensure!(
            file.read(&mut buffer[..1])? == 0,
            "{} changed during fingerprinting",
            path.display()
        );
    } else if metadata.is_dir() {
        frame(hash, b"directory");
        let mut entries = fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            hash_path(hash, &entry.path())?;
        }
        frame(hash, b"end");
    } else {
        anyhow::bail!("cannot fingerprint {}", path.display());
    }
    Ok(())
}

pub(crate) fn compute(
    copy: &Path,
    settings: &serde_json::Value,
    inputs: impl IntoIterator<Item = PathBuf>,
    nextest: bool,
) -> Result<String> {
    let mut hash = Sha256::new();
    frame(&mut hash, b"tmt-campaign-v1");
    frame(&mut hash, &serde_json::to_vec(settings)?);
    let mut env: Vec<_> = std::env::vars_os().collect();
    env.sort();
    for (name, value) in env {
        frame(&mut hash, name.as_encoded_bytes());
        frame(&mut hash, value.as_encoded_bytes());
    }
    frame(&mut hash, b"tools");
    for (tool, flags) in [
        ("cargo", &["--version", "--verbose"][..]),
        ("rustc", &["--version", "--verbose"][..]),
        ("cargo", &["nextest", "--version"][..]),
    ]
    .into_iter()
    .take(if nextest { 3 } else { 2 })
    {
        let output = Command::new(tool)
            .args(flags)
            .current_dir(copy)
            .output()
            .with_context(|| format!("identifying {tool} for the checkpoint"))?;
        ensure!(
            output.status.success(),
            "cannot identify {tool} for the checkpoint"
        );
        frame(&mut hash, &output.stdout);
    }
    hash_path(&mut hash, &std::env::current_exe()?)?;
    hash_path(&mut hash, copy).context("fingerprinting the working copy")?;
    for path in inputs {
        // Cargo's JSON can also list informational outputs absent on this host.
        if path.exists() {
            hash_path(&mut hash, &path)
                .with_context(|| format!("fingerprinting {}", path.display()))?;
            if fs::symlink_metadata(&path)?.is_symlink() {
                hash_path(&mut hash, &path.canonicalize()?)?;
            }
        }
    }
    Ok(format!("{:x}", hash.finalize()))
}
