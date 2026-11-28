//! Project and workspace configuration file (`tmt.toml`) discovery, parsing,
//! validation, and layered merging with CLI overrides.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::driver::StageExternal;
use crate::output_fixture::Fixture;

/// Maximum allowed size of a configuration file (1 MiB).
pub const MAX_CONFIG_BYTES: u64 = 1024 * 1024;

/// Candidate config paths checked in each directory (in display order).
pub const CONFIG_CANDIDATES: &[&str] = &["tmt.toml", ".config/tmt.toml", ".cargo/tmt.toml"];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
    Jsonl,
    Sarif,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TimeoutRecheck {
    #[default]
    Serial,
    None,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TestTool {
    #[default]
    Cargo,
    Nextest,
}

/// Declarative settings that may appear at the top level of `tmt.toml`, under
/// `[package.<name>]`, or as a CLI override layer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    // ── Mutation Mode ─────────────────────────────────────────────────────────
    pub rebuild: Option<bool>,
    pub source_only: Option<bool>,
    pub error: Option<Vec<String>>,
    pub literal_operators: Option<bool>,
    pub verify: Option<bool>,
    pub effects: Option<bool>,
    pub audit: Option<usize>,

    // ── Test Execution & Selection ────────────────────────────────────────────
    pub also_test: Option<Vec<String>>,
    pub dependents: Option<bool>,
    pub test_workspace: Option<bool>,
    pub doc: Option<bool>,
    pub test_tool: Option<TestTool>,
    pub nextest_profile: Option<String>,
    pub isolate_tests: Option<bool>,
    pub lib: Option<bool>,
    pub bins: Option<bool>,
    pub test: Option<Vec<String>>,
    pub test_target: Option<Vec<String>>,
    pub test_filter: Option<Vec<String>>,
    pub include_test: Option<Vec<String>>,
    pub exclude_test: Option<Vec<String>>,
    pub include_ignored: Option<bool>,
    pub test_wrapper: Option<PathBuf>,
    pub wrapper_grace: Option<u64>,

    // ── Output & Baselines ────────────────────────────────────────────────────
    pub format: Option<OutputFormat>,
    pub show_unreached: Option<bool>,
    pub quiet: Option<bool>,
    pub baseline: Option<PathBuf>,

    // ── Performance & Cargo Build ─────────────────────────────────────────────
    pub jobs: Option<u32>,
    pub baseline_jobs: Option<u32>,
    pub baseline_timeout: Option<u64>,
    pub timeout: Option<u64>,
    pub timeout_recheck: Option<TimeoutRecheck>,
    pub checkpoint: Option<bool>,
    pub opt_level: Option<u8>,
    pub features: Option<Vec<String>>,
    pub all_features: Option<bool>,
    pub no_default_features: Option<bool>,
    pub locked: Option<bool>,

    // ── Work Directory & Fixtures ─────────────────────────────────────────────
    pub work_dir: Option<PathBuf>,
    pub all_files: Option<bool>,
    pub git_index: Option<bool>,
    pub copy_fixture: Option<Vec<PathBuf>>,
    pub copy_fixture_exclude: Option<Vec<PathBuf>>,
    pub copy_output_fixture: Option<Vec<String>>,
    pub stage_external: Option<Vec<String>>,
}

/// Parsed representation of a `tmt.toml` file.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfigFile {
    pub defaults: Settings,
    pub package: BTreeMap<String, Settings>,
}

/// A configuration file loaded from disk together with its path and base directory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadedConfig {
    pub path: PathBuf,
    pub base_dir: PathBuf,
    pub file: ConfigFile,
}

impl LoadedConfig {
    /// Resolves settings for a single package (or top-level defaults when `package` is `None`)
    /// layered with CLI overrides, returning the validated effective settings.
    pub fn resolve(&self, package: Option<&str>, cli: &Settings) -> Result<Settings> {
        let mut combined = self.file.defaults.clone();
        if let Some(pkg) = package
            && let Some(pkg_settings) = self.file.package.get(pkg)
        {
            combined = combined.overlay(pkg_settings);
        }
        let merged = combined.overlay(cli);
        merged.validate_effective(&format!(
            "effective configuration (from {})",
            self.path.display()
        ))?;
        Ok(merged)
    }

    /// Resolves top-level settings layered with CLI overrides for workspace batch setup,
    /// deferring cross-field prerequisite checks (such as `copy_fixture_exclude` or
    /// `wrapper_grace`) until each workspace member's `[package.<name>]` layer is applied.
    pub fn resolve_workspace_base(&self, cli: &Settings) -> Result<Settings> {
        let merged = self.file.defaults.overlay(cli);
        merged.validate_layer(&format!(
            "workspace configuration (from {})",
            self.path.display()
        ))?;
        Ok(merged)
    }
}

/// Resolves settings when no configuration file was loaded.
pub fn resolve_without_config(cli: &Settings) -> Result<Settings> {
    cli.validate_effective("CLI options")?;
    Ok(cli.clone())
}

/// Resolves CLI-only settings for workspace batch setup when no configuration file was loaded.
pub fn resolve_workspace_base_without_config(cli: &Settings) -> Result<Settings> {
    cli.validate_layer("CLI options")?;
    Ok(cli.clone())
}

/// Loads configuration according to CLI selection (`--config` / `--no-config` / auto-discovery).
pub fn load_for_dir(
    dir: &Path,
    explicit: Option<&Path>,
    no_config: bool,
) -> Result<Option<LoadedConfig>> {
    if no_config {
        return Ok(None);
    }
    if let Some(path) = explicit {
        return load_explicit(path).map(Some);
    }
    discover(dir)
}

/// Loads a configuration file explicitly specified via `--config`.
pub fn load_explicit(path: &Path) -> Result<LoadedConfig> {
    let base_dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    load_from_path(path, base_dir, None)
}

/// Discovers a configuration file starting at `dir` and walking upward until a workspace root
/// (`Cargo.toml` containing `[workspace]`), `.git`, or filesystem root is reached.
pub fn discover(dir: &Path) -> Result<Option<LoadedConfig>> {
    if !dir.exists() {
        return Ok(None);
    }
    let start = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let start_dir = if start.is_file() {
        match start.parent() {
            Some(parent) => parent.to_path_buf(),
            None => return Ok(None),
        }
    } else {
        start
    };

    let mut current: Option<&Path> = Some(start_dir.as_path());
    while let Some(dir) = current {
        let mut found = Vec::new();
        for rel in CONFIG_CANDIDATES {
            let candidate = dir.join(rel);
            if candidate
                .try_exists()
                .with_context(|| format!("checking {}", candidate.display()))?
            {
                found.push(candidate);
            }
        }
        if found.len() > 1 {
            let names = found
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            bail!(
                "multiple tmt config files found in {}: {names} (keep only one or pass --config)",
                dir.display()
            );
        }
        if let Some(path) = found.into_iter().next() {
            let canonical_dir = fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
            return load_from_path(&path, canonical_dir.clone(), Some(&canonical_dir)).map(Some);
        }
        if is_workspace_manifest(&dir.join("Cargo.toml")) || dir.join(".git").exists() {
            break;
        }
        current = dir.parent();
    }
    Ok(None)
}

/// Reads `[package].name` from `dir/Cargo.toml` if present (or from the sole member of a
/// single-member virtual workspace), without invoking `cargo`.
pub fn detect_package_name(dir: &Path) -> Option<String> {
    let manifest = dir.join("Cargo.toml");
    let text = fs::read_to_string(&manifest).ok()?;
    let doc: toml_edit::DocumentMut = text.parse().ok()?;
    if let Some(name) = doc
        .get("package")
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
    {
        return Some(name.to_owned());
    }
    // Match `driver::select_package` for a single-member virtual workspace.
    let members = doc
        .get("workspace")
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array())?;
    if members.len() == 1
        && let Some(member_rel) = members.get(0).and_then(|v| v.as_str())
        && !member_rel.contains(['*', '?', '['])
    {
        let member_manifest = dir.join(member_rel).join("Cargo.toml");
        let member_text = fs::read_to_string(member_manifest).ok()?;
        let member_doc: toml_edit::DocumentMut = member_text.parse().ok()?;
        return member_doc
            .get("package")?
            .get("name")?
            .as_str()
            .map(ToOwned::to_owned);
    }
    None
}

fn is_workspace_manifest(manifest: &Path) -> bool {
    let Ok(text) = fs::read_to_string(manifest) else {
        return false;
    };
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return false;
    };
    doc.get("workspace").is_some_and(toml_edit::Item::is_table)
}

fn open_regular_config(path: &Path, reject_symlink: bool) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        ))]
        const O_NONBLOCK: i32 = 0x0004;
        #[cfg(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly"
        ))]
        const O_NOFOLLOW: i32 = 0x0100;

        #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "x86")))]
        const O_NONBLOCK: i32 = 0o4000;
        #[cfg(all(target_os = "linux", any(target_arch = "x86_64", target_arch = "x86")))]
        const O_NOFOLLOW: i32 = 0o400000;

        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        const O_NONBLOCK: i32 = 0o4000;
        #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
        const O_NOFOLLOW: i32 = 0o100000;

        #[cfg(not(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly",
            all(
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64")
            )
        )))]
        const O_NONBLOCK: i32 = 0;
        #[cfg(not(any(
            target_os = "macos",
            target_os = "ios",
            target_os = "freebsd",
            target_os = "netbsd",
            target_os = "openbsd",
            target_os = "dragonfly",
            all(
                target_os = "linux",
                any(target_arch = "x86_64", target_arch = "x86", target_arch = "aarch64")
            )
        )))]
        const O_NOFOLLOW: i32 = 0;

        let flags = if reject_symlink {
            O_NONBLOCK | O_NOFOLLOW
        } else {
            O_NONBLOCK
        };
        if flags != 0 {
            options.custom_flags(flags);
        }
    }
    let file = options
        .open(path)
        .with_context(|| format!("reading config file {}", path.display()))?;
    let metadata = file
        .metadata()
        .with_context(|| format!("inspecting config file {}", path.display()))?;
    if !metadata.is_file() {
        bail!("config path is not a regular file: {}", path.display());
    }
    Ok(file)
}

fn load_from_path(
    path: &Path,
    base_dir: PathBuf,
    confined_to: Option<&Path>,
) -> Result<LoadedConfig> {
    let open_target = if let Some(boundary) = confined_to {
        let resolved = fs::canonicalize(path)
            .with_context(|| format!("resolving config file {}", path.display()))?;
        if !resolved.starts_with(boundary) {
            bail!(
                "auto-discovered config file {} resolves outside {} (pass --config explicitly to allow)",
                path.display(),
                boundary.display()
            );
        }
        resolved
    } else {
        path.to_path_buf()
    };
    let file_handle = open_regular_config(&open_target, confined_to.is_some())?;
    let mut text = String::new();
    file_handle
        .take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("reading config file {}", path.display()))?;
    let text_len = text.len() as u64;
    if text_len > MAX_CONFIG_BYTES {
        bail!(
            "config file {} exceeds maximum size of {} bytes",
            path.display(),
            MAX_CONFIG_BYTES
        );
    }
    let mut file = parse_config(&text, &path.display().to_string())?;
    file.resolve_paths(&base_dir);
    Ok(LoadedConfig {
        path: path.to_path_buf(),
        base_dir,
        file,
    })
}

/// Parses and validates a `tmt.toml` string.
pub fn parse_config(text: &str, source: &str) -> Result<ConfigFile> {
    let text_len = text.len() as u64;
    if text_len > MAX_CONFIG_BYTES {
        bail!("config {source} exceeds maximum size of {MAX_CONFIG_BYTES} bytes");
    }
    validate_text_characters(text, source)?;

    let mut doc: toml_edit::DocumentMut = text
        .parse()
        .with_context(|| format!("parsing TOML in {source}"))?;

    let mut package = BTreeMap::new();
    if let Some(pkg_item) = doc.remove("package") {
        let Some(pkg_table) = pkg_item.as_table_like() else {
            bail!("{source}: `package` must be a table of per-package settings");
        };
        for (name, value) in pkg_table.iter() {
            if name.trim().is_empty() {
                bail!("{source}: `[package]` entry name must not be empty");
            }
            validate_decoded_string(name, &format!("{source}: `[package]` key"))?;
            let Some(sub_table) = value.as_table_like() else {
                bail!("{source}: `[package.{name}]` must be a table");
            };
            let mut sub_doc = toml_edit::DocumentMut::new();
            for (k, v) in sub_table.iter() {
                sub_doc.insert(k, v.clone());
            }
            let settings: Settings = toml_edit::de::from_document(sub_doc)
                .with_context(|| format!("parsing [package.{name}] in {source}"))?;
            if settings.format.is_some() {
                bail!(
                    "{source} [package.{name}]: `format` is only supported at the top level of tmt.toml"
                );
            }
            if settings.show_unreached.is_some() {
                bail!(
                    "{source} [package.{name}]: `show_unreached` is only supported at the top level of tmt.toml"
                );
            }
            settings.validate_layer(&format!("{source} [package.{name}]"))?;
            package.insert(name.to_owned(), settings);
        }
    }

    let defaults: Settings = toml_edit::de::from_document(doc)
        .with_context(|| format!("parsing top-level settings in {source}"))?;
    defaults.validate_layer(source)?;

    Ok(ConfigFile { defaults, package })
}

fn is_disallowed_bidi(ch: char) -> bool {
    matches!(
        ch,
        '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
    )
}

fn validate_text_characters(text: &str, source: &str) -> Result<()> {
    for ch in text.chars() {
        if ch == '\0' {
            bail!("{source}: config file must not contain NUL bytes");
        }
        if (ch.is_control() && !matches!(ch, '\n' | '\r' | '\t')) || is_disallowed_bidi(ch) {
            bail!(
                "{source}: config file contains disallowed control character U+{:04X}",
                ch as u32
            );
        }
    }
    Ok(())
}

fn validate_decoded_string(value: &str, context: &str) -> Result<()> {
    for ch in value.chars() {
        if ch == '\0' {
            bail!("{context}: value must be without control characters (found NUL byte)");
        }
        if ch.is_control() || is_disallowed_bidi(ch) {
            bail!(
                "{context}: value must be without control characters (found U+{:04X})",
                ch as u32
            );
        }
    }
    Ok(())
}

fn validate_decoded_path(path: &Path, context: &str) -> Result<()> {
    if let Some(text) = path.to_str() {
        validate_decoded_string(text, context)?;
    }
    Ok(())
}

impl ConfigFile {
    fn resolve_paths(&mut self, base_dir: &Path) {
        self.defaults.resolve_paths(base_dir);
        for pkg in self.package.values_mut() {
            pkg.resolve_paths(base_dir);
        }
    }
}

impl Settings {
    fn resolve_paths(&mut self, base_dir: &Path) {
        if let Some(baseline) = &mut self.baseline
            && baseline.is_relative()
        {
            *baseline = base_dir.join(&*baseline);
        }
        if let Some(work_dir) = &mut self.work_dir
            && work_dir.is_relative()
        {
            *work_dir = base_dir.join(&*work_dir);
        }
    }

    fn validate_decoded_strings(&self, context: &str) -> Result<()> {
        for (field, list) in [
            ("error", self.error.as_deref()),
            ("also_test", self.also_test.as_deref()),
            ("test", self.test.as_deref()),
            ("test_target", self.test_target.as_deref()),
            ("test_filter", self.test_filter.as_deref()),
            ("include_test", self.include_test.as_deref()),
            ("exclude_test", self.exclude_test.as_deref()),
            ("features", self.features.as_deref()),
            ("copy_output_fixture", self.copy_output_fixture.as_deref()),
            ("stage_external", self.stage_external.as_deref()),
        ] {
            if let Some(items) = list {
                for item in items {
                    validate_decoded_string(item, &format!("{context}: `{field}`"))?;
                }
            }
        }
        if let Some(profile) = &self.nextest_profile {
            validate_decoded_string(profile, &format!("{context}: `nextest_profile`"))?;
        }
        for (field, path) in [
            ("test_wrapper", self.test_wrapper.as_deref()),
            ("baseline", self.baseline.as_deref()),
            ("work_dir", self.work_dir.as_deref()),
        ] {
            if let Some(p) = path {
                validate_decoded_path(p, &format!("{context}: `{field}`"))?;
            }
        }
        for (field, paths) in [
            ("copy_fixture", self.copy_fixture.as_deref()),
            ("copy_fixture_exclude", self.copy_fixture_exclude.as_deref()),
        ] {
            if let Some(items) = paths {
                for p in items {
                    validate_decoded_path(p, &format!("{context}: `{field}`"))?;
                }
            }
        }
        Ok(())
    }

    /// Validates a single configuration layer (value ranges, syntax, and intra-layer conflicts).
    pub fn validate_layer(&self, context: &str) -> Result<()> {
        self.validate_decoded_strings(context)?;
        if self.jobs == Some(0) {
            bail!("{context}: `jobs` must be at least 1");
        }
        if self.baseline_jobs == Some(0) {
            bail!("{context}: `baseline_jobs` must be at least 1");
        }
        if self.baseline_timeout == Some(0) {
            bail!("{context}: `baseline_timeout` must be at least 1");
        }
        if self.timeout == Some(0) {
            bail!("{context}: `timeout` must be at least 1");
        }
        if let Some(opt) = self.opt_level
            && opt > 3
        {
            bail!("{context}: `opt_level` must be between 0 and 3 (got {opt})");
        }
        if let Some(fixtures) = &self.copy_output_fixture {
            for spec in fixtures {
                Fixture::from_str(spec).with_context(|| {
                    format!("{context}: invalid `copy_output_fixture` entry `{spec}`")
                })?;
            }
        }
        if let Some(externals) = &self.stage_external {
            for spec in externals {
                StageExternal::from_str(spec).with_context(|| {
                    format!("{context}: invalid `stage_external` entry `{spec}`")
                })?;
            }
        }
        if self.rebuild == Some(true) && self.source_only == Some(true) {
            bail!("{context}: `rebuild` and `source_only` cannot both be true");
        }
        if self.source_only == Some(true) && self.audit.is_some() {
            bail!("{context}: `source_only` cannot be combined with `audit`");
        }
        if self.source_only == Some(true) && self.verify == Some(false) {
            bail!("{context}: `source_only` cannot be combined with `verify = false`");
        }
        if self.audit.is_some() && self.verify == Some(false) {
            bail!("{context}: `audit` cannot be combined with `verify = false`");
        }
        if self.all_features == Some(true)
            && (self.features.as_ref().is_some_and(|f| !f.is_empty())
                || self.no_default_features == Some(true))
        {
            bail!(
                "{context}: `all_features` cannot be combined with `features` or `no_default_features`"
            );
        }
        if self.test_tool == Some(TestTool::Cargo) && self.nextest_profile.is_some() {
            bail!("{context}: `nextest_profile` requires `test_tool = \"nextest\"`");
        }
        Ok(())
    }

    /// Overlays `upper` on top of `self`. Upper values replace lower values, and explicit
    /// mode switches in `upper` clear mutually exclusive lower-layer settings.
    pub fn overlay(&self, upper: &Settings) -> Settings {
        let mut out = Settings {
            rebuild: upper.rebuild.or(self.rebuild),
            source_only: upper.source_only.or(self.source_only),
            error: upper.error.clone().or_else(|| self.error.clone()),
            literal_operators: upper.literal_operators.or(self.literal_operators),
            verify: upper.verify.or(self.verify),
            effects: upper.effects.or(self.effects),
            audit: upper.audit.or(self.audit),
            also_test: upper.also_test.clone().or_else(|| self.also_test.clone()),
            dependents: upper.dependents.or(self.dependents),
            test_workspace: upper.test_workspace.or(self.test_workspace),
            doc: upper.doc.or(self.doc),
            test_tool: upper.test_tool.or(self.test_tool),
            nextest_profile: upper
                .nextest_profile
                .clone()
                .or_else(|| self.nextest_profile.clone()),
            isolate_tests: upper.isolate_tests.or(self.isolate_tests),
            lib: upper.lib.or(self.lib),
            bins: upper.bins.or(self.bins),
            test: upper.test.clone().or_else(|| self.test.clone()),
            test_target: upper
                .test_target
                .clone()
                .or_else(|| self.test_target.clone()),
            test_filter: upper
                .test_filter
                .clone()
                .or_else(|| self.test_filter.clone()),
            include_test: upper
                .include_test
                .clone()
                .or_else(|| self.include_test.clone()),
            exclude_test: upper
                .exclude_test
                .clone()
                .or_else(|| self.exclude_test.clone()),
            include_ignored: upper.include_ignored.or(self.include_ignored),
            test_wrapper: upper
                .test_wrapper
                .clone()
                .or_else(|| self.test_wrapper.clone()),
            wrapper_grace: upper.wrapper_grace.or(self.wrapper_grace),
            format: upper.format.or(self.format),
            show_unreached: upper.show_unreached.or(self.show_unreached),
            quiet: upper.quiet.or(self.quiet),
            baseline: upper.baseline.clone().or_else(|| self.baseline.clone()),
            jobs: upper.jobs.or(self.jobs),
            baseline_jobs: upper.baseline_jobs.or(self.baseline_jobs),
            baseline_timeout: upper.baseline_timeout.or(self.baseline_timeout),
            timeout: upper.timeout.or(self.timeout),
            timeout_recheck: upper.timeout_recheck.or(self.timeout_recheck),
            checkpoint: upper.checkpoint.or(self.checkpoint),
            opt_level: upper.opt_level.or(self.opt_level),
            features: upper.features.clone().or_else(|| self.features.clone()),
            all_features: upper.all_features.or(self.all_features),
            no_default_features: upper.no_default_features.or(self.no_default_features),
            locked: upper.locked.or(self.locked),
            work_dir: upper.work_dir.clone().or_else(|| self.work_dir.clone()),
            all_files: upper.all_files.or(self.all_files),
            git_index: upper.git_index.or(self.git_index),
            copy_fixture: upper
                .copy_fixture
                .clone()
                .or_else(|| self.copy_fixture.clone()),
            copy_fixture_exclude: upper
                .copy_fixture_exclude
                .clone()
                .or_else(|| self.copy_fixture_exclude.clone()),
            copy_output_fixture: upper
                .copy_output_fixture
                .clone()
                .or_else(|| self.copy_output_fixture.clone()),
            stage_external: upper
                .stage_external
                .clone()
                .or_else(|| self.stage_external.clone()),
        };

        // Resolve cross-layer mutually exclusive overrides so upper layer wins cleanly.
        if upper.source_only == Some(true) {
            if upper.rebuild.is_none() {
                out.rebuild = Some(false);
            }
            if upper.audit.is_none() {
                out.audit = None;
            }
            if upper.verify.is_none() {
                out.verify = Some(true);
            }
        }
        if upper.rebuild == Some(true) && upper.source_only.is_none() {
            out.source_only = Some(false);
        }
        if upper.audit.is_some() {
            if upper.verify.is_none() {
                out.verify = Some(true);
            }
            if upper.source_only.is_none() {
                out.source_only = Some(false);
            }
        }
        if out.source_only == Some(false) && upper.literal_operators.is_none() {
            out.literal_operators = Some(false);
        }
        if upper.verify == Some(false) && upper.audit.is_none() {
            out.audit = None;
        }
        if upper.all_features == Some(true) {
            if upper.features.is_none() {
                out.features = None;
            }
            if upper.no_default_features.is_none() {
                out.no_default_features = Some(false);
            }
        }
        if (upper.features.as_ref().is_some_and(|f| !f.is_empty())
            || upper.no_default_features == Some(true))
            && upper.all_features.is_none()
        {
            out.all_features = Some(false);
        }
        if upper.test_tool == Some(TestTool::Cargo) && upper.nextest_profile.is_none() {
            out.nextest_profile = None;
        }
        if upper.copy_fixture.as_ref().is_some_and(Vec::is_empty)
            && upper.copy_fixture_exclude.is_none()
        {
            out.copy_fixture_exclude = None;
        }

        out
    }

    /// Validates the final merged settings after all layers have been combined.
    pub fn validate_effective(&self, context: &str) -> Result<()> {
        self.validate_layer(context)?;

        let source_only = self.source_only.unwrap_or(false);
        let literal_operators = self.literal_operators.unwrap_or(false);
        let test_tool = self.test_tool.unwrap_or_default();

        if literal_operators && !source_only {
            bail!("{context}: `literal_operators` requires `source_only = true`");
        }
        if self.nextest_profile.is_some() && test_tool != TestTool::Nextest {
            bail!("{context}: `nextest_profile` requires `test_tool = \"nextest\"`");
        }
        if self.wrapper_grace.is_some() && self.test_wrapper.is_none() {
            bail!("{context}: `wrapper_grace` requires `test_wrapper`");
        }
        if self
            .copy_fixture_exclude
            .as_ref()
            .is_some_and(|e| !e.is_empty())
            && self.copy_fixture.as_ref().is_none_or(Vec::is_empty)
        {
            bail!("{context}: `copy_fixture_exclude` requires `copy_fixture`");
        }
        Ok(())
    }

    /// Parses `copy_output_fixture` strings into validated `Fixture` values.
    pub fn parsed_output_fixtures(&self) -> Result<Vec<Fixture>> {
        self.copy_output_fixture
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|s| Fixture::from_str(s))
            .collect()
    }

    /// Parses `stage_external` strings into validated `StageExternal` values.
    pub fn parsed_stage_external(&self) -> Result<Vec<StageExternal>> {
        self.stage_external
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .map(|s| StageExternal::from_str(s))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "tmt-config-test-{}-{}-{label}",
            std::process::id(),
            id
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, content).unwrap();
    }

    #[test]
    fn config_file_pict_table() {
        let table = include_str!("../tests/data/config-file.pict.tsv");
        let mut lines = table.lines();
        let _header = lines.next().expect("header");

        for (idx, line) in lines.filter(|l| !l.trim().is_empty()).enumerate() {
            let cols: Vec<&str> = line.split('\t').collect();
            assert_eq!(cols.len(), 5, "row {idx}: {line}");
            let (source, pkg_override, cli_override, path_base, validity) =
                (cols[0], cols[1], cols[2], cols[3], cols[4]);

            let root = temp_dir(&format!("pict-{idx}"));
            let pkg_dir = if source == "auto_workspace_parent" {
                write(
                    &root.join("Cargo.toml"),
                    "[workspace]\nmembers = [\"crates/alpha\"]\n",
                );
                let member = root.join("crates/alpha");
                write(
                    &member.join("Cargo.toml"),
                    "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
                );
                member
            } else {
                write(
                    &root.join("Cargo.toml"),
                    "[package]\nname = \"alpha\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
                );
                root.clone()
            };

            let abs_baseline = root.join("abs-baseline.json");
            let mut body = String::new();
            match validity {
                "valid" => {
                    body.push_str("jobs = 4\n");
                    body.push_str("rebuild = true\n");
                    body.push_str("exclude_test = [\"top_skip\"]\n");
                    match path_base {
                        "relative" => body.push_str("baseline = \"base/rel.json\"\n"),
                        "absolute" => {
                            let escaped = abs_baseline.display().to_string().replace('\\', "\\\\");
                            body.push_str(&format!("baseline = \"{escaped}\"\n"));
                        }
                        "absent" => {}
                        other => panic!("unexpected PathBase: {other}"),
                    }
                    match pkg_override {
                        "matching" => {
                            body.push_str(
                                "\n[package.alpha]\njobs = 2\nexclude_test = [\"pkg_skip\"]\n",
                            );
                        }
                        "other_only" => {
                            body.push_str(
                                "\n[package.beta]\njobs = 8\nexclude_test = [\"beta_skip\"]\n",
                            );
                        }
                        "none" => {}
                        other => panic!("unexpected PackageOverride: {other}"),
                    }
                }
                "unknown_key" => body.push_str("no_such_option = true\n"),
                "invalid_value" => body.push_str("jobs = 0\n"),
                "duplicate_candidates" => {
                    body.push_str("jobs = 2\n");
                }
                "missing_explicit" => {}
                other => panic!("unexpected Validity: {other}"),
            }

            let explicit_path = root.join("custom/explicit.toml");
            let mut explicit_arg: Option<PathBuf> = None;
            let mut no_config = false;

            match source {
                "none" => {}
                "disabled" => {
                    write(&root.join("tmt.toml"), "jobs = 9\n");
                    no_config = true;
                }
                "auto_root" | "auto_workspace_parent" => {
                    write(&root.join("tmt.toml"), &body);
                    if validity == "duplicate_candidates" {
                        write(&root.join(".config/tmt.toml"), "jobs = 3\n");
                    }
                }
                "auto_dot_config" => {
                    write(&root.join(".config/tmt.toml"), &body);
                    if validity == "duplicate_candidates" {
                        write(&root.join(".cargo/tmt.toml"), "jobs = 3\n");
                    }
                }
                "auto_dot_cargo" => {
                    write(&root.join(".cargo/tmt.toml"), &body);
                    if validity == "duplicate_candidates" {
                        write(&root.join("tmt.toml"), "jobs = 3\n");
                    }
                }
                "explicit" => {
                    if validity != "missing_explicit" {
                        write(&explicit_path, &body);
                    }
                    explicit_arg = Some(explicit_path.clone());
                }
                other => panic!("unexpected ConfigSource: {other}"),
            }

            let loaded = load_for_dir(&pkg_dir, explicit_arg.as_deref(), no_config);
            if validity != "valid" {
                assert!(
                    loaded.is_err(),
                    "expected error for row {idx} ({line}), got {loaded:?}"
                );
                let _ = fs::remove_dir_all(&root);
                continue;
            }
            let loaded = loaded.unwrap_or_else(|e| panic!("row {idx} ({line}) failed: {e:#}"));
            if matches!(source, "none" | "disabled") {
                assert!(loaded.is_none(), "row {idx}: expected no config");
                let _ = fs::remove_dir_all(&root);
                continue;
            }
            let loaded = loaded.unwrap_or_else(|| panic!("row {idx}: expected config to load"));

            let mut cli = Settings::default();
            match cli_override {
                "none" => {}
                "scalar" => cli.jobs = Some(1),
                "array" => cli.exclude_test = Some(vec!["cli_skip".into()]),
                "conflicting_mode" => cli.source_only = Some(true),
                other => panic!("unexpected CliOverride: {other}"),
            }

            let effective = loaded
                .resolve(Some("alpha"), &cli)
                .unwrap_or_else(|e| panic!("row {idx} ({line}) resolve failed: {e:#}"));

            let expected_jobs = match (cli_override, pkg_override) {
                ("scalar", _) => Some(1),
                (_, "matching") => Some(2),
                _ => Some(4),
            };
            assert_eq!(effective.jobs, expected_jobs, "row {idx}: {line}");

            let expected_exclude = match (cli_override, pkg_override) {
                ("array", _) => Some(vec!["cli_skip".into()]),
                (_, "matching") => Some(vec!["pkg_skip".into()]),
                _ => Some(vec!["top_skip".into()]),
            };
            assert_eq!(
                effective.exclude_test, expected_exclude,
                "row {idx}: {line}"
            );

            if cli_override == "conflicting_mode" {
                assert_eq!(effective.source_only, Some(true), "row {idx}");
                assert_eq!(effective.rebuild, Some(false), "row {idx}");
            } else {
                assert_eq!(effective.rebuild, Some(true), "row {idx}");
            }

            let expected_base = if source == "explicit" {
                root.join("custom")
            } else {
                fs::canonicalize(&root).unwrap()
            };
            match path_base {
                "absent" => assert_eq!(effective.baseline, None, "row {idx}"),
                "relative" => assert_eq!(
                    effective.baseline,
                    Some(expected_base.join("base/rel.json")),
                    "row {idx}: {line}"
                ),
                "absolute" => assert_eq!(effective.baseline, Some(abs_baseline), "row {idx}"),
                _ => unreachable!(),
            }

            let _ = fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn discovery_stops_at_workspace_and_git_boundaries() {
        let outer = temp_dir("boundary");
        write(&outer.join("tmt.toml"), "jobs = 9\n");

        // Case 1: inner crate declares its own `[workspace]`, so outer tmt.toml must not leak.
        let isolated_ws = outer.join("isolated-ws");
        write(
            &isolated_ws.join("Cargo.toml"),
            "[workspace]\n[package]\nname = \"iso\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        assert!(discover(&isolated_ws).unwrap().is_none());

        // Case 2: inner repo has `.git`, so outer tmt.toml must not leak.
        let inner_git = outer.join("inner-git");
        fs::create_dir_all(inner_git.join(".git")).unwrap();
        write(
            &inner_git.join("Cargo.toml"),
            "[package]\nname = \"gitpkg\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        assert!(discover(&inner_git).unwrap().is_none());

        // Case 3: nested member inside a workspace without its own tmt.toml stops at workspace root
        // and picks up the workspace root's `.config/tmt.toml`, resolving package-level paths.
        let ws = outer.join("ws");
        write(&ws.join("Cargo.toml"), "[workspace]\nmembers = [\"a\"]\n");
        write(
            &ws.join(".config/tmt.toml"),
            "opt_level = 3\n\n[package.a]\nwork_dir = \"custom-work\"\nbaseline = \"a-base.json\"\n",
        );
        let member = ws.join("a");
        write(
            &member.join("Cargo.toml"),
            "[package]\nname = \"a\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        let loaded = discover(&member).unwrap().expect("workspace config");
        let ws_canon = fs::canonicalize(&ws).unwrap();
        assert_eq!(loaded.file.defaults.opt_level, Some(3));
        assert_eq!(loaded.base_dir, ws_canon);
        let eff = loaded.resolve(Some("a"), &Settings::default()).unwrap();
        assert_eq!(eff.work_dir, Some(ws_canon.join("custom-work")));
        assert_eq!(eff.baseline, Some(ws_canon.join("a-base.json")));

        let _ = fs::remove_dir_all(&outer);
    }

    #[test]
    fn size_limit_non_file_and_symlink_boundary_are_enforced() {
        let dir = temp_dir("io-limits");

        // Exact 1 MiB file succeeds; 1 MiB + 1 byte fails.
        let exact = dir.join("exact.toml");
        let pad_exact = "#".repeat(MAX_CONFIG_BYTES as usize);
        write(&exact, &pad_exact);
        assert!(load_explicit(&exact).is_ok());

        let over = dir.join("over.toml");
        let pad_over = "#".repeat(MAX_CONFIG_BYTES as usize + 1);
        write(&over, &pad_over);
        assert!(load_explicit(&over).is_err());

        // Passing a directory as `--config` fails.
        let subdir = dir.join("subdir");
        fs::create_dir_all(&subdir).unwrap();
        assert!(load_explicit(&subdir).is_err());

        // Auto-discovery rejects a symlink resolving outside the discovered directory
        // (both direct file symlink and `.config` directory symlink), and FIFOs never block.
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let outside = temp_dir("outside-symlink");
            let secret_cfg = outside.join("outside.toml");
            write(&secret_cfg, "jobs = 2\n");
            let proj = dir.join("proj");
            fs::create_dir_all(&proj).unwrap();
            symlink(&secret_cfg, proj.join("tmt.toml")).unwrap();
            assert!(discover(&proj).is_err());
            // Explicit `--config` is allowed when the user names it directly.
            assert!(load_explicit(&proj.join("tmt.toml")).is_ok());

            // `.config` itself symlinked outside the project must also be rejected.
            let proj2 = dir.join("proj2");
            fs::create_dir_all(&proj2).unwrap();
            let outside_cfg_dir = outside.join("cfgdir");
            write(&outside_cfg_dir.join("tmt.toml"), "jobs = 2\n");
            symlink(&outside_cfg_dir, proj2.join(".config")).unwrap();
            assert!(discover(&proj2).is_err());

            // FIFO config fails immediately without blocking (both auto-discovered and explicit).
            let fifo_proj = dir.join("fifo-proj");
            fs::create_dir_all(&fifo_proj).unwrap();
            let fifo_path = fifo_proj.join("tmt.toml");
            let status = std::process::Command::new("mkfifo")
                .arg(&fifo_path)
                .status()
                .expect("mkfifo");
            assert!(status.success());
            assert!(discover(&fifo_proj).is_err());
            assert!(load_explicit(&fifo_path).is_err());

            let _ = fs::remove_dir_all(&outside);
        }

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_rejects_dangerous_chars_escaped_controls_and_invalid_values() {
        assert!(parse_config("jobs = 1\0\n", "test").is_err());
        assert!(parse_config("jobs = 1\x07\n", "test").is_err());
        assert!(parse_config("# \u{202E}bidi\njobs = 1\n", "test").is_err());
        assert!(parse_config("# \u{061C}alm\njobs = 1\n", "test").is_err());

        // Exact limit succeeds; 1 byte over fails.
        assert!(parse_config(&"#".repeat(MAX_CONFIG_BYTES as usize), "test").is_ok());
        assert!(parse_config(&"#".repeat(MAX_CONFIG_BYTES as usize + 1), "test").is_err());

        // Valid boundary values succeed.
        let ok = parse_config(
            "opt_level = 3\njobs = 1\nbaseline_jobs = 1\nbaseline_timeout = 1\ntimeout = 1\n",
            "test",
        )
        .unwrap();
        assert_eq!(ok.defaults.opt_level, Some(3));
        assert_eq!(ok.defaults.baseline_timeout, Some(1));

        for bad in [
            "jobs = 0\n",
            "baseline_jobs = 0\n",
            "baseline_timeout = 0\n",
            "timeout = 0\n",
            "opt_level = 4\n",
            "unknown_field = 1\n",
            "rebuild = true\nsource_only = true\n",
            "source_only = true\naudit = 2\n",
            "source_only = true\nverify = false\n",
            "audit = 2\nverify = false\n",
            "all_features = true\nfeatures = [\"a\"]\n",
            "all_features = true\nno_default_features = true\n",
            "test_tool = \"cargo\"\nnextest_profile = \"ci\"\n",
            "copy_output_fixture = [\"missing_equals\"]\n",
            "stage_external = [\"not_parent_relative\"]\n",
            "package = \"not_a_table\"\n",
            "[package]\nfoo = 123\n",
            "[package.foo]\nunknown = true\n",
            "[package.foo]\njobs = 0\n",
            "[package.foo]\nformat = \"json\"\n",
            "[package.foo]\nshow_unreached = true\n",
            // TOML-escaped control / NUL / BiDi characters in values and package keys.
            "exclude_test = [\"bad\\u0000name\"]\n",
            "nextest_profile = \"ci\\u001b[31m\"\n",
            "baseline = \"foo\\u202Ebar.json\"\n",
            "baseline = \"foo\\u061cbar.json\"\n",
            "[package.\"bad\\u0000pkg\"]\njobs = 1\n",
        ] {
            assert!(
                parse_config(bad, "test").is_err(),
                "expected parse_config error for:\n{bad}"
            );
        }
    }

    #[test]
    fn overlay_and_effective_validation_cover_all_mode_switches_and_requirements() {
        let base = parse_config(
            r#"
            all_features = true
            test_tool = "nextest"
            nextest_profile = "ci"
            source_only = true
            literal_operators = true
            exclude_test = ["slow_a", "slow_b"]
            copy_fixture = ["data"]
            copy_fixture_exclude = ["data/tmp"]
            test_wrapper = "scripts/wrap.sh"
            wrapper_grace = 5
            "#,
            "tmt.toml",
        )
        .unwrap()
        .defaults;
        // Base itself is valid as effective settings.
        base.validate_effective("base").unwrap();

        // Explicit `source_only = false` clears inherited `literal_operators`.
        let disable_source = Settings {
            source_only: Some(false),
            ..Settings::default()
        };
        let disabled = base.overlay(&disable_source);
        assert_eq!(disabled.source_only, Some(false));
        assert_eq!(disabled.literal_operators, Some(false));
        disabled.validate_effective("disabled").unwrap();

        // Switching to `audit` clears `source_only` and inherited `literal_operators`.
        let audit_cli = Settings {
            audit: Some(3),
            ..Settings::default()
        };
        let audited = base.overlay(&audit_cli);
        assert_eq!(audited.source_only, Some(false));
        assert_eq!(audited.literal_operators, Some(false));
        audited.validate_effective("audited").unwrap();

        // CLI switches features, test_tool, rebuild, and clears arrays.
        let cli = Settings {
            features: Some(vec!["minimal".into()]),
            test_tool: Some(TestTool::Cargo),
            rebuild: Some(true),
            exclude_test: Some(vec![]),
            copy_fixture: Some(vec![]),
            ..Settings::default()
        };
        let merged = base.overlay(&cli);
        merged.validate_effective("merged").unwrap();
        assert_eq!(merged.all_features, Some(false));
        assert_eq!(merged.features, Some(vec!["minimal".into()]));
        assert_eq!(merged.test_tool, Some(TestTool::Cargo));
        assert_eq!(merged.nextest_profile, None);
        assert_eq!(merged.rebuild, Some(true));
        assert_eq!(merged.source_only, Some(false));
        assert_eq!(merged.literal_operators, Some(false));
        assert_eq!(merged.exclude_test, Some(vec![]));
        assert_eq!(merged.copy_fixture, Some(vec![]));
        assert_eq!(merged.copy_fixture_exclude, None);

        // Effective validation catches missing prerequisites.
        assert!(
            Settings {
                literal_operators: Some(true),
                ..Settings::default()
            }
            .validate_effective("lit")
            .is_err()
        );
        assert!(
            Settings {
                nextest_profile: Some("ci".into()),
                ..Settings::default()
            }
            .validate_effective("nextest")
            .is_err()
        );
        assert!(
            Settings {
                wrapper_grace: Some(5),
                ..Settings::default()
            }
            .validate_effective("grace")
            .is_err()
        );
        assert!(
            Settings {
                copy_fixture_exclude: Some(vec![PathBuf::from("a")]),
                ..Settings::default()
            }
            .validate_effective("exclude")
            .is_err()
        );

        // LoadedConfig::resolve rejects merged settings that fail effective validation.
        let loaded = LoadedConfig {
            path: PathBuf::from("tmt.toml"),
            base_dir: PathBuf::from("."),
            file: parse_config("jobs = 2\n", "tmt.toml").unwrap(),
        };
        assert!(
            loaded
                .resolve(
                    None,
                    &Settings {
                        wrapper_grace: Some(3),
                        ..Settings::default()
                    }
                )
                .is_err()
        );
    }

    #[test]
    fn detect_package_name_reads_manifest_and_single_member_virtual_workspace() {
        let dir = temp_dir("pkg-name");
        write(
            &dir.join("Cargo.toml"),
            "[package]\nname = \"my-crate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        assert_eq!(detect_package_name(&dir).as_deref(), Some("my-crate"));

        let virt_empty = temp_dir("virt-ws-empty");
        write(
            &virt_empty.join("Cargo.toml"),
            "[workspace]\nmembers = []\n",
        );
        assert_eq!(detect_package_name(&virt_empty), None);

        let virt_single = temp_dir("virt-ws-single");
        write(
            &virt_single.join("Cargo.toml"),
            "[workspace]\nmembers = [\"crates/only\"]\n",
        );
        write(
            &virt_single.join("crates/only/Cargo.toml"),
            "[package]\nname = \"only-crate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        );
        assert_eq!(
            detect_package_name(&virt_single).as_deref(),
            Some("only-crate")
        );

        let _ = fs::remove_dir_all(&dir);
        let _ = fs::remove_dir_all(&virt_empty);
        let _ = fs::remove_dir_all(&virt_single);
    }
}
