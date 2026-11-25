//! Cargo metadata, JSON build output parsing, dependency tracking, and built-source reachability.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::instrument::{self};
use crate::reverts;

use super::*;

#[derive(Deserialize)]
pub(super) struct Metadata {
    pub(super) packages: Vec<Package>,
    pub(super) workspace_root: PathBuf,
    pub(super) target_directory: PathBuf,
}

#[derive(Deserialize)]
pub(super) struct Package {
    pub(super) name: String,
    #[serde(default)]
    pub(super) dependencies: Vec<Dependency>,
    pub(super) features: BTreeMap<String, Vec<String>>,
    pub(super) manifest_path: PathBuf,
    pub(super) targets: Vec<Target>,
}

impl Package {
    /// Only count features we can establish without resolving the dependency graph.
    pub(super) fn enabled_local_features(
        &self,
        workspace: &Path,
        cargo: &CargoConfig,
    ) -> BTreeSet<&str> {
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
pub(super) struct Dependency {
    pub(super) name: String,
}

#[derive(Deserialize)]
pub(super) struct Target {
    pub(super) name: String,
    pub(super) kind: Vec<String>,
    pub(super) src_path: PathBuf,
    pub(super) test: bool,
    #[serde(default)]
    pub(super) doctest: bool,
    #[serde(default, rename = "required-features")]
    pub(super) required_features: Vec<String>,
}

impl Target {
    pub(super) fn is_production(&self) -> bool {
        matches!(self.test_build_arg(), Some(("--lib" | "--bin", _)))
    }

    pub(super) fn is_lib(&self) -> bool {
        matches!(self.test_build_arg(), Some(("--lib", None)))
    }

    pub(super) fn is_bin(&self) -> bool {
        matches!(self.test_build_arg(), Some(("--bin", Some(_))))
    }

    pub(super) fn is_integration_test(&self) -> bool {
        matches!(self.test_build_arg(), Some(("--test", Some(_))))
    }

    /// The Cargo selector and its optional name. `--lib` selects every package's library.
    pub(super) fn test_build_arg(&self) -> Option<(&'static str, Option<&str>)> {
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

pub(super) fn target_selected(target: &Target, selection: &TestSelection) -> bool {
    if !selection.has_target_filters() {
        return true;
    }
    (selection.lib && target.is_lib())
        || (selection.bins && target.is_bin())
        || (target.is_integration_test() && selection.integration_tests.contains(&target.name))
        || selection.targets.contains(&target.name)
}

pub(super) fn artifact_target_selected(
    name: &str,
    kind: &[String],
    selection: &TestSelection,
) -> bool {
    if !selection.has_target_filters() {
        return true;
    }
    let first = kind.first().map(String::as_str);
    let is_lib = matches!(
        first,
        Some("lib" | "rlib" | "cdylib" | "dylib" | "staticlib" | "proc-macro")
    );
    let is_bin = first == Some("bin");
    let is_test = first == Some("test");
    (selection.lib && is_lib)
        || (selection.bins && is_bin)
        || (is_test && selection.integration_tests.iter().any(|t| t == name))
        || selection.targets.iter().any(|t| t == name)
}

/// Validates requested target selectors against Cargo metadata before building, so
/// misspelled or absent targets fail immediately rather than after a full compile.
pub(super) fn validate_target_selection(
    packages: &[&Package],
    selection: &TestSelection,
) -> Result<()> {
    let all_targets: Vec<&Target> = packages.iter().flat_map(|p| &p.targets).collect();
    if selection.lib && !all_targets.iter().any(|t| t.is_lib()) {
        bail!(
            "no tests matched: `--lib` was requested, but no selected package has a library target"
        );
    }
    if selection.bins && !all_targets.iter().any(|t| t.is_bin()) {
        bail!(
            "no tests matched: `--bins` was requested, but no selected package has a binary target"
        );
    }
    for name in &selection.integration_tests {
        if !all_targets
            .iter()
            .any(|t| t.is_integration_test() && t.name == *name)
        {
            bail!(
                "no tests matched: `--test {name}` matched no integration test target in the selected packages"
            );
        }
    }
    if !selection.targets.is_empty()
        && !all_targets
            .iter()
            .any(|t| t.test && selection.targets.contains(&t.name))
    {
        let hint = selection
            .targets
            .iter()
            .find_map(|name| {
                let lib_name = name.replace('-', "_");
                all_targets
                    .iter()
                    .find(|t| t.test && t.is_lib() && t.name == lib_name)
                    .map(|_| {
                        format!(
                            " (hint: pass `--lib` or `--test-target {lib_name}` for the library target)"
                        )
                    })
            })
            .unwrap_or_default();
        bail!(
            "no tests matched: `--test-target` matched no test target in the selected packages{hint}"
        );
    }
    Ok(())
}

/// Builds actionable hints when test selection matched zero tests.
pub(super) fn no_tests_matched_hint(
    packages: &[&Package],
    selection: &TestSelection,
    empty_bin_targets: &[(String, String)],
    discovered_tests: &[String],
) -> String {
    let mut hints = Vec::new();
    for (owner, bin_name) in empty_bin_targets {
        if !selection.targets.iter().any(|t| t == bin_name) {
            continue;
        }
        let lib_candidate = bin_name.replace('-', "_");
        if let Some(pkg) = packages.iter().find(|p| p.name == *owner)
            && pkg
                .targets
                .iter()
                .any(|t| t.test && t.is_lib() && t.name == lib_candidate)
        {
            hints.push(format!(
                "\nhint: target `{bin_name}` matched a binary target with no tests; pass `--lib` or `--test-target {lib_candidate}` to run library tests"
            ));
            break;
        }
    }
    if !selection.include.is_empty()
        && selection.filter.is_empty()
        && let Some(pattern) = selection.include.iter().find(|wanted| {
            !discovered_tests.iter().any(|known| known == *wanted)
                && discovered_tests
                    .iter()
                    .any(|known| known.contains(wanted.as_str()))
        })
    {
        hints.push(format!(
            "\nhint: `--include-test` requires exact test names; pass `--test-filter {pattern}` for substring/module matching"
        ));
    }
    hints.concat()
}

/// Select only test targets that Cargo's flags can express without broadening the selection.
pub(super) fn test_build_flags(
    packages: &[&Package],
    selection: &TestSelection,
    workspace: &Path,
    cargo: &CargoConfig,
) -> Option<Vec<String>> {
    if !selection.has_target_filters() {
        return None;
    }
    let selected = |target: &Target| {
        let explicit_cargo_selector = (selection.lib && target.is_lib())
            || (selection.bins && target.is_bin())
            || (target.is_integration_test() && selection.integration_tests.contains(&target.name));
        (target.test || explicit_cargo_selector) && target_selected(target, selection)
    };
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
pub(super) struct CargoMessage {
    pub(super) reason: String,
    pub(super) package_id: Option<String>,
    pub(super) target: Option<ArtifactTarget>,
    pub(super) profile: Option<Profile>,
    pub(super) executable: Option<PathBuf>,
    #[serde(default)]
    pub(super) filenames: Vec<PathBuf>,
    #[serde(default)]
    pub(super) fresh: bool,
    /// The features the target was built with; absent from what is not an artifact.
    pub(super) features: Option<Vec<String>>,
    pub(super) message: Option<Diagnostic>,
}

#[derive(Deserialize)]
pub(super) struct ArtifactTarget {
    pub(super) name: String,
    pub(super) kind: Vec<String>,
    pub(super) src_path: Option<PathBuf>,
}

#[derive(Deserialize)]
pub(super) struct Profile {
    pub(super) test: bool,
}

#[derive(Deserialize)]
pub(super) struct DiagnosticCode {
    pub(super) code: String,
}

#[derive(Deserialize)]
pub(super) struct Diagnostic {
    pub(super) level: String,
    pub(super) code: Option<DiagnosticCode>,
    pub(super) rendered: Option<String>,
    pub(super) message: Option<String>,
    #[serde(default)]
    pub(super) spans: Vec<DiagnosticSpan>,
}

#[derive(Deserialize)]
pub(super) struct DiagnosticSpan {
    pub(super) file_name: String,
    pub(super) byte_start: usize,
    pub(super) line_start: usize,
    pub(super) is_primary: bool,
}

#[derive(Debug, PartialEq, Clone)]
pub(super) struct ErrorSpan {
    pub(super) file: String,
    pub(super) byte_start: usize,
    pub(super) line: usize,
}

/// One compiler error. The primary spans say where it was detected, the secondary ones
/// often point at the cause (for "possibly-uninitialized", at the guard we inserted).
#[derive(Debug, PartialEq)]
pub(super) struct CompileError {
    pub(super) code: Option<String>,
    pub(super) message: String,
    pub(super) primary: Vec<ErrorSpan>,
    pub(super) secondary: Vec<ErrorSpan>,
    pub(super) rendered: String,
}

#[derive(Debug, Default, PartialEq)]
pub(super) struct BuildOutput {
    /// `(target name, executable, package, target kind)` of the test binaries.
    pub(super) test_binaries: Vec<(String, PathBuf, String, Vec<String>)>,
    pub(super) errors: Vec<CompileError>,
    pub(super) diagnostics: Vec<String>,
    pub(super) build_scripts: Vec<serde_json::Value>,
    pub(super) artifacts: Vec<(PathBuf, Vec<PathBuf>)>,
    /// The features each root file was built with, as cargo reports them.
    pub(super) features: Vec<(PathBuf, Vec<String>)>,
}

/// What a build of changed source came to.
#[derive(Debug, PartialEq)]
pub(super) enum SourceBuild {
    Built,
    /// The compiler refused the code: the change is not a program.
    Refused,
    /// The build ended badly and the compiler said nothing against the code (it was
    /// killed, the disk is full, cargo could not get its lock): this says nothing about
    /// the change. With the end of what was said, for the user.
    Failed(String),
}

pub(super) fn source_build(output: &std::process::Output) -> SourceBuild {
    if output.status.success() {
        return SourceBuild::Built;
    }
    let refused = !parse_build_output(&String::from_utf8_lossy(&output.stdout), &[])
        .diagnostics
        .is_empty();
    if refused {
        return SourceBuild::Refused;
    }
    let said = String::from_utf8_lossy(&output.stderr);
    let last: Vec<&str> = said.lines().rev().take(3).collect();
    let last: Vec<&str> = last.into_iter().rev().collect();
    SourceBuild::Failed(format!("{} ({})", last.join(" / "), output.status))
}

/// The workspace packages that depend on `package`, directly or through others.
pub(super) fn dependents_of(package: &str, packages: &[Package]) -> Vec<String> {
    let mut reverse = BTreeMap::<&str, Vec<&str>>::new();
    for other in packages {
        for dependency in &other.dependencies {
            reverse
                .entry(&dependency.name)
                .or_default()
                .push(&other.name);
        }
    }
    let mut found = BTreeSet::new();
    let mut frontier = vec![package];
    while let Some(current) = frontier.pop() {
        if let Some(next) = reverse.get(current) {
            for &other in next {
                if other != package && found.insert(other) {
                    frontier.push(other);
                }
            }
        }
    }
    found.into_iter().map(str::to_owned).collect()
}

/// Package ids embed the path, and the build runs in a copy, so compare by name.
/// Ids look like `path+file:///dir#name@1.0.0`, `path+file:///dir/name#1.0.0` or `name 1.0.0 (path+...)`.
pub(super) fn is_package(id: &str, name: &str) -> bool {
    let (path, fragment) = id.split_once('#').unwrap_or((id, ""));
    fragment.split('@').next() == Some(name)
        || path.rsplit('/').next() == Some(name)
        || id.split(' ').next() == Some(name)
}

pub(super) fn parse_build_output(stdout: &str, packages: &[&str]) -> BuildOutput {
    let mut out = BuildOutput::default();
    for line in stdout.lines() {
        let Ok(msg) = serde_json::from_str::<CargoMessage>(line) else {
            continue;
        };
        match msg.reason.as_str() {
            "compiler-artifact" => {
                let Some(target) = msg.target else { continue };
                if let Some(path) = target.src_path {
                    let root = normalize(&path);
                    if let Some(features) = msg.features {
                        out.features.push((root.clone(), features));
                    }
                    out.artifacts.push((root, msg.filenames));
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
            "build-script-executed" => {
                if let Ok(script) = serde_json::from_str(line) {
                    out.build_scripts.push(script);
                }
            }
            "compiler-message" => {
                let Some(diag) = msg.message else { continue };
                if diag.level != "error" {
                    continue;
                }
                let raw_message = diag.message.unwrap_or_default();
                let rendered = diag.rendered.unwrap_or_else(|| raw_message.clone());
                if !rendered.is_empty() && !out.diagnostics.contains(&rendered) {
                    out.diagnostics.push(rendered.clone());
                }
                let span = |s: &DiagnosticSpan| ErrorSpan {
                    file: s.file_name.clone(),
                    byte_start: s.byte_start,
                    line: s.line_start,
                };
                let error = CompileError {
                    code: diag.code.map(|c| c.code),
                    message: raw_message,
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
    out.test_binaries.sort_by(|left, right| {
        (&left.2, &left.3, &left.0, &left.1).cmp(&(&right.2, &right.3, &right.0, &right.1))
    });
    out
}

/// Cargo puts each rustc dep-info file beside an artifact with the crate's stem.
pub(super) fn dependency_file(artifact: &Path) -> Option<String> {
    let stem = artifact.file_stem()?.to_str()?;
    let unprefixed = match artifact.extension().and_then(|ext| ext.to_str()) {
        Some("a" | "so" | "dylib" | "rlib" | "rmeta") => stem.strip_prefix("lib"),
        _ => None,
    };
    unprefixed
        .and_then(|stem| fs::read_to_string(artifact.with_file_name(format!("{stem}.d"))).ok())
        .or_else(|| fs::read_to_string(artifact.with_extension("d")).ok())
}

pub(super) fn dep_inputs(text: &str, copy: &Path) -> BTreeSet<PathBuf> {
    // Rustc writes one empty rule per input. This avoids counting output
    // artifacts and excludes Cargo's transitive, promoted dependency file.
    text.lines()
        .filter_map(|line| line.strip_suffix(':'))
        .map(|name| name.replace("\\ ", " ").replace("\\#", "#"))
        .map(|name| normalize(&copy.join(name)))
        .collect()
}

pub(super) fn directory_entries(path: &Path) -> impl Iterator<Item = PathBuf> + use<> {
    fs::read_dir(path)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
}

pub(super) fn same_binary(a: &Path, b: &Path) -> Option<bool> {
    let (a, b) = (fs::File::open(a).ok()?, fs::File::open(b).ok()?);
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
    compare_streams(a, b).ok()
}

pub(super) fn compare_streams(
    a: impl std::io::Read,
    b: impl std::io::Read,
) -> std::io::Result<bool> {
    use std::io::{BufRead, BufReader, ErrorKind};
    const BUFFER_BYTES: usize = 64 * 1024;
    let mut a = BufReader::with_capacity(BUFFER_BYTES, a);
    let mut b = BufReader::with_capacity(BUFFER_BYTES, b);
    loop {
        let left = match a.fill_buf() {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let right = match b.fill_buf() {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        let count = left.len().min(right.len());
        if count == 0 {
            return Ok(left.is_empty() && right.is_empty());
        }
        if left[..count] != right[..count] {
            return Ok(false);
        }
        a.consume(count);
        b.consume(count);
    }
}

pub(super) fn artifact_inputs(artifact: &Path, root: &Path, copy: &Path) -> BTreeSet<PathBuf> {
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
pub(super) struct BuiltSources {
    pub(super) files: BTreeMap<PathBuf, BTreeSet<PathBuf>>,
    /// The features a root file was built with, build by build (a host and a target
    /// build may differ). A root without an entry was built by a cargo that did not
    /// say, or with flags that may turn features on behind cargo's back.
    pub(super) features: BTreeMap<PathBuf, Vec<BTreeSet<String>>>,
}

/// The ranges that every build leaves out: those of one build that lie within a range
/// of each of the others. With no build, nothing is known to be left out.
pub(super) fn left_out_of_all(per_build: &[Vec<(usize, usize)>]) -> Vec<(usize, usize)> {
    let mut all: Vec<(usize, usize)> = per_build
        .iter()
        .flatten()
        .filter(|(from, to)| {
            per_build
                .iter()
                .all(|ranges| ranges.iter().any(|(start, end)| start <= from && to <= end))
        })
        .copied()
        .collect();
    all.sort_unstable();
    all.dedup();
    all
}

/// Whether a cargo configuration file that holds for a build in `dir` speaks of both a
/// `--cfg` flag and a feature (`rustflags = ["--cfg", 'feature="x"']`): read as text, so
/// rather once too often than once too rarely.
pub(super) fn configuration_names_a_feature(dir: &Path, cargo_home: Option<PathBuf>) -> bool {
    let home = cargo_home
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cargo")));
    dir.ancestors()
        .map(|dir| dir.join(".cargo"))
        .chain(home)
        .flat_map(|dir| [dir.join("config.toml"), dir.join("config")])
        .filter_map(|file| fs::read_to_string(file).ok())
        .any(|text| text.contains("--cfg") && text.contains("feature"))
}

/// Whether the compiler may be told of features that cargo does not report
/// (`RUSTFLAGS='--cfg feature="x"'`).
pub(super) fn features_from_flags(
    flags: impl IntoIterator<Item = Option<std::ffi::OsString>>,
) -> bool {
    flags.into_iter().flatten().any(|value| {
        // `--cfg feature="x"`, not `-C target-feature=+x`.
        let value = value.to_string_lossy();
        value.contains("cfg") && value.replace("target-feature", "").contains("feature")
    })
}

impl BuiltSources {
    /// `features_known` is false where something other than cargo's features may turn
    /// one on: nothing is then taken to be left out.
    pub(super) fn from_artifacts(
        artifacts: &[(PathBuf, Vec<PathBuf>)],
        features: &[(PathBuf, Vec<String>)],
        features_known: bool,
        copy: &Path,
    ) -> Self {
        let mut built = Self::default();
        for (root, features) in features.iter().filter(|_| features_known) {
            let set: BTreeSet<String> = features.iter().cloned().collect();
            let sets = built.features.entry(root.clone()).or_default();
            if !sets.contains(&set) {
                sets.push(set);
            }
        }
        for (root, artifacts) in artifacts {
            if !root.starts_with(copy) {
                continue;
            }
            let files = built.files.entry(root.clone()).or_default();
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

    pub(super) fn contains(&self, source: &SourceFile) -> bool {
        source.owners.iter().any(|root| {
            self.files
                .get(root)
                .is_some_and(|files| files.contains(&source.path))
        })
    }

    /// The parts of a built file that no build of it compiled: code under a `cfg` that
    /// the features of every build that owns the file turn off. Nothing when one of
    /// those builds did not say its features.
    pub(super) fn left_out(&self, source: &SourceFile) -> Vec<(usize, usize)> {
        self.left_out_of(source, &source.original)
            .unwrap_or_default()
    }

    /// The feature sets of the builds that compiled the file. `None` when one of them
    /// did not say its features.
    pub(super) fn builds_of(&self, source: &SourceFile) -> Option<Vec<&BTreeSet<String>>> {
        let mut builds: Vec<&BTreeSet<String>> = vec![];
        for root in &source.owners {
            if !self
                .files
                .get(root)
                .is_some_and(|files| files.contains(&source.path))
            {
                continue;
            }
            builds.extend(self.features.get(root)?);
        }
        Some(builds)
    }

    /// The same for another text of the file. `None` when that is not Rust, or a build
    /// did not say its features: nothing is known then.
    pub(super) fn left_out_of(
        &self,
        source: &SourceFile,
        text: &str,
    ) -> Option<Vec<(usize, usize)>> {
        let builds = self.builds_of(source)?;
        let parsed = instrument::parse_source(text).ok()?;
        let per_build: Vec<Vec<(usize, usize)>> = builds
            .iter()
            .map(|features| instrument::inactive_ranges(text, &parsed, features))
            .collect();
        Some(left_out_of_all(&per_build))
    }

    /// Whether two texts of the file are the same program in every build of it: each
    /// build with what it leaves out of either, and the `cfg` attributes it settles as
    /// on, which are no part of what it builds. False when that is not known.
    pub(super) fn same_program(&self, source: &SourceFile, a: &str, b: &str) -> bool {
        let (Some(builds), Ok(parsed_a), Ok(parsed_b)) = (
            self.builds_of(source),
            instrument::parse_source(a),
            instrument::parse_source(b),
        ) else {
            return false;
        };
        let apart = |text: &str, parsed: &syn::File, features: &BTreeSet<String>| {
            let mut ranges = instrument::inactive_ranges(text, parsed, features);
            ranges.extend(instrument::settled_cfg_attributes(text, parsed, features));
            ranges
        };
        !builds.is_empty()
            && builds.iter().all(|features| {
                reverts::same_build(
                    a,
                    &apart(a, &parsed_a, features),
                    b,
                    &apart(b, &parsed_b, features),
                )
            })
    }
}

#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub(super) struct BuildProgressState {
    pub(super) compiled: usize,
    pub(super) cached: usize,
    pub(super) latest: String,
}

impl BuildProgressState {
    pub(super) fn observe_line(&mut self, line: &str) {
        let Ok(msg) = serde_json::from_str::<CargoMessage>(line) else {
            return;
        };
        if msg.reason == "compiler-artifact"
            && let Some(target) = msg.target
        {
            if msg.fresh {
                self.cached += 1;
            } else {
                self.compiled += 1;
            }
            self.latest = target.name;
        }
    }

    pub(super) fn format_line(&self) -> String {
        let latest = crate::report::sanitize_inline(&self.latest);
        if self.compiled > 0 && self.cached > 0 {
            format!(
                "building: {} compiled ({} cached, latest: {latest})",
                self.compiled, self.cached
            )
        } else if self.compiled > 0 {
            format!("building: {} compiled (latest: {latest})", self.compiled)
        } else if self.cached > 0 {
            format!("building: {} cached (latest: {latest})", self.cached)
        } else {
            "building: compiling...".to_string()
        }
    }
}

const BUILD_PROGRESS_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

pub(super) fn run_cargo_with_progress(
    command: &mut Command,
    progress: bool,
) -> Result<std::process::Output> {
    run_cargo_with_progress_interval(command, progress, BUILD_PROGRESS_INTERVAL, |line| {
        eprintln!("tmt: {line}");
    })
}

pub(super) fn run_cargo_with_progress_interval(
    command: &mut Command,
    progress: bool,
    interval: std::time::Duration,
    mut emit: impl FnMut(&str) + Send,
) -> Result<std::process::Output> {
    use std::io::{BufRead, BufReader, Read};
    use std::sync::{Mutex, mpsc};

    if !progress {
        return command.output().context("running cargo test --no-run");
    }

    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("running cargo test --no-run")?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let mut stderr = child.stderr.take().expect("stderr is piped");
    let state = Mutex::new(BuildProgressState::default());

    let (stdout_bytes, stderr_bytes) = std::thread::scope(|scope| -> Result<(Vec<u8>, Vec<u8>)> {
        let stderr_thread = scope.spawn(move || -> std::io::Result<Vec<u8>> {
            let mut buf = Vec::new();
            stderr.read_to_end(&mut buf)?;
            Ok(buf)
        });
        let (done_tx, done_rx) = mpsc::channel::<()>();
        let state_ref = &state;
        let reporter = scope.spawn(move || {
            while done_rx.recv_timeout(interval) == Err(mpsc::RecvTimeoutError::Timeout) {
                let line = state_ref
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .format_line();
                emit(&line);
            }
        });

        let mut stdout_buf = Vec::new();
        let mut reader = BufReader::new(stdout);
        let mut raw_line = Vec::new();
        loop {
            raw_line.clear();
            let read = reader.read_until(b'\n', &mut raw_line)?;
            if read == 0 {
                break;
            }
            stdout_buf.extend_from_slice(&raw_line);
            let lossy = String::from_utf8_lossy(&raw_line);
            state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .observe_line(&lossy);
        }
        drop(done_tx);
        let _ = reporter.join();
        let stderr_buf = stderr_thread
            .join()
            .expect("stderr reader thread panicked")?;
        Ok((stdout_buf, stderr_buf))
    })?;

    let status = child.wait().context("waiting for cargo test --no-run")?;
    Ok(std::process::Output {
        status,
        stdout: stdout_bytes,
        stderr: stderr_bytes,
    })
}

pub(super) fn cargo_metadata(dir: &Path, cargo: &CargoConfig) -> Result<Metadata> {
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
