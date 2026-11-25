//! Persistent cache of probe sites disabled by compile-error repair.
//!
//! Stored inside `--work-dir` as `.tmt-repair-cache.json` and keyed by a SHA-256
//! fingerprint of the full staged source tree (using uninstrumented originals for
//! instrumented files), Cargo configuration, and toolchain version. Stale or corrupt
//! caches are ignored safely.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::instrument::RUNTIME_MOD;

use super::{CargoConfig, SourceFile, WorkCopy};

const CACHE_FILE: &str = ".tmt-repair-cache.json";
const CACHE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(super) struct CachedSite {
    pub(super) file: String,
    pub(super) start: usize,
    pub(super) end: usize,
    pub(super) text: String,
    pub(super) reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct CachePayload {
    version: u32,
    fingerprint: String,
    disabled: Vec<CachedSite>,
}

fn frame(hash: &mut Sha256, bytes: &[u8]) {
    hash.update((bytes.len() as u64).to_le_bytes());
    hash.update(bytes);
}

fn hash_tree(
    hash: &mut Sha256,
    dir: &Path,
    stage_root: &Path,
    workspace: &Path,
    state: &WorkCopy,
    originals: &BTreeMap<&Path, &str>,
    runtime_name: &str,
) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .collect();
    paths.sort();
    for path in paths {
        let Ok(meta) = fs::symlink_metadata(&path) else {
            continue;
        };
        if path.file_name().and_then(|n| n.to_str()) == Some(runtime_name) {
            continue;
        }
        let rel = path.strip_prefix(stage_root).unwrap_or(&path);
        if meta.is_dir() {
            frame(hash, b"dir");
            frame(hash, rel.as_os_str().as_encoded_bytes());
            hash_tree(
                hash,
                &path,
                stage_root,
                workspace,
                state,
                originals,
                runtime_name,
            );
        } else if meta.is_file() {
            frame(hash, b"file");
            frame(hash, rel.as_os_str().as_encoded_bytes());
            if let Some(original) = originals.get(path.as_path()) {
                frame(hash, original.as_bytes());
            } else if path.starts_with(&state.copy_root) && state.earlier.contains(&path) {
                if let Ok(Some(orig)) = state.original(&path, workspace) {
                    frame(hash, orig.as_bytes());
                }
            } else if let Ok(bytes) = fs::read(&path) {
                frame(hash, &bytes);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn compute_fingerprint(
    stage_root: &Path,
    copy: &Path,
    workspace: &Path,
    state: &WorkCopy,
    package_name: &str,
    test_packages: &[&str],
    cargo: &CargoConfig,
    opt_level: Option<u8>,
    target_flags: Option<&[String]>,
    error_values: &[&str],
    doc: bool,
    files: &[SourceFile],
) -> String {
    let mut hash = Sha256::new();
    frame(&mut hash, b"tmt-repair-cache-v1");
    frame(&mut hash, env!("CARGO_PKG_VERSION").as_bytes());

    if let Ok(output) = Command::new("rustc")
        .args(["--version", "--verbose"])
        .current_dir(copy)
        .output()
        && output.status.success()
    {
        frame(&mut hash, &output.stdout);
    }

    frame(&mut hash, package_name.as_bytes());
    for pkg in test_packages {
        frame(&mut hash, pkg.as_bytes());
    }
    if let Ok(json) = serde_json::to_vec(cargo) {
        frame(&mut hash, &json);
    }
    frame(
        &mut hash,
        opt_level
            .map(|level| level.to_string())
            .unwrap_or_default()
            .as_bytes(),
    );
    if let Some(flags) = target_flags {
        frame(&mut hash, b"target_flags");
        for flag in flags {
            frame(&mut hash, flag.as_bytes());
        }
    } else {
        frame(&mut hash, b"package_wide");
    }
    frame(&mut hash, b"error_values");
    for expr in error_values {
        frame(&mut hash, expr.as_bytes());
    }
    frame(&mut hash, if doc { b"doc" } else { b"no_doc" });

    let mut rustflags: Vec<_> = std::env::vars_os()
        .filter(|(k, _)| k.to_string_lossy().ends_with("RUSTFLAGS"))
        .collect();
    rustflags.sort();
    frame(&mut hash, b"rustflags");
    for (k, v) in rustflags {
        frame(&mut hash, k.as_encoded_bytes());
        frame(&mut hash, v.as_encoded_bytes());
    }

    let originals: BTreeMap<&Path, &str> = files
        .iter()
        .map(|file| (file.path.as_path(), file.original.as_str()))
        .collect();
    let runtime_name = format!("{RUNTIME_MOD}.rs");
    frame(&mut hash, b"stage_tree");
    hash_tree(
        &mut hash,
        stage_root,
        stage_root,
        workspace,
        state,
        &originals,
        &runtime_name,
    );

    format!("{:x}", hash.finalize())
}

/// Loads cached repair exclusions when the fingerprint matches.
/// Returns an empty list on miss, mismatch, or unreadable/corrupt cache files.
pub(super) fn load(work: &Path, fingerprint: &str) -> Vec<CachedSite> {
    let path = work.join(CACHE_FILE);
    if fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_symlink()) {
        return vec![];
    }
    let Ok(text) = fs::read_to_string(&path) else {
        return vec![];
    };
    let Ok(payload) = serde_json::from_str::<CachePayload>(&text) else {
        return vec![];
    };
    if payload.version != CACHE_VERSION || payload.fingerprint != fingerprint {
        return vec![];
    }
    payload.disabled
}

/// Atomically writes the repair cache after a successful instrumented build.
/// Uses exclusive creation (`create_new`) so an existing hard link or symlink at the
/// temporary path can never be overwritten.
pub(super) fn save(work: &Path, fingerprint: &str, disabled: &[CachedSite]) -> Result<()> {
    let path = work.join(CACHE_FILE);
    if fs::symlink_metadata(&path).is_ok_and(|meta| meta.is_symlink()) {
        let _ = fs::remove_file(&path);
    }
    let payload = CachePayload {
        version: CACHE_VERSION,
        fingerprint: fingerprint.to_string(),
        disabled: disabled.to_vec(),
    };
    let bytes = serde_json::to_vec(&payload)?;
    let pid = std::process::id();
    let mut suffix = 0u64;
    let (temp, mut file) = loop {
        let candidate = work.join(format!("{CACHE_FILE}.{pid}.{suffix}.tmp"));
        match fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&candidate)
        {
            Ok(file) => break (candidate, file),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                suffix += 1;
            }
            Err(err) => return Err(err.into()),
        }
    };
    if let Err(err) = file.write_all(&bytes).and_then(|()| file.flush()) {
        let _ = fs::remove_file(&temp);
        return Err(err.into());
    }
    drop(file);
    if let Err(err) = fs::rename(&temp, &path) {
        let _ = fs::remove_file(&temp);
        return Err(err.into());
    }
    Ok(())
}
