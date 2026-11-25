//! Persistent cache of probe sites disabled by compile-error repair.
//!
//! Stored inside `--work-dir` as `.tmt-repair-cache.json` and keyed by a SHA-256
//! fingerprint of the instrumented source files, Cargo manifests/lockfile, build
//! configuration, and toolchain version. Stale or corrupt caches are ignored safely.

use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{CargoConfig, SourceFile};

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

#[allow(clippy::too_many_arguments)]
pub(super) fn compute_fingerprint(
    copy: &Path,
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

    for manifest_name in ["Cargo.toml", "Cargo.lock"] {
        let path = copy.join(manifest_name);
        if let Ok(bytes) = fs::read(&path) {
            frame(&mut hash, manifest_name.as_bytes());
            frame(&mut hash, &bytes);
        }
    }

    frame(&mut hash, b"sources");
    for file in files {
        frame(&mut hash, file.display.as_bytes());
        frame(&mut hash, file.original.as_bytes());
    }

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
pub(super) fn save(work: &Path, fingerprint: &str, disabled: &[CachedSite]) -> Result<()> {
    let path = work.join(CACHE_FILE);
    let temp = work.join(format!("{CACHE_FILE}.new"));
    for file in [&path, &temp] {
        if fs::symlink_metadata(file).is_ok_and(|meta| meta.is_symlink()) {
            let _ = fs::remove_file(file);
        }
    }
    let payload = CachePayload {
        version: CACHE_VERSION,
        fingerprint: fingerprint.to_string(),
        disabled: disabled.to_vec(),
    };
    let bytes = serde_json::to_vec(&payload)?;
    fs::write(&temp, bytes)?;
    fs::rename(&temp, &path)?;
    Ok(())
}
