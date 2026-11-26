use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Result, bail};
use clap::Parser;
use targeted_mutation::{config, driver, report};

const HEADING_TARGET: &str = "Target & Scope";
const HEADING_MUTATION: &str = "Mutation Mode";
const HEADING_TESTS: &str = "Test Execution & Selection";
const HEADING_OUTPUT: &str = "Output & Baselines";
const HEADING_BUILD: &str = "Performance & Cargo Build";
const HEADING_WORK: &str = "Work Directory & Fixtures";

const SHORT_EXAMPLES: &str = "\
Examples:
  cargo tmt                               Analyse the current package
  cargo tmt --since main                  Analyse changes since `main`
  cargo tmt --rebuild                     Also build source-only mutants and witnesses
  cargo tmt --format sarif > tmt.sarif    Write SARIF 2.1.0 output

Run `cargo tmt --help` for detailed option descriptions and exit codes.";

const LONG_EXAMPLES: &str = "\
Examples:
  cargo tmt                               Analyse the current package
  cargo tmt --since main                  Analyse changes since `main`
  cargo tmt --rebuild                     Also build source-only mutants and witnesses
  cargo tmt --workspace --format jsonl    Stream one JSON record per workspace member
  cargo tmt --format sarif > tmt.sarif    Write SARIF 2.1.0 for code scanning
  cargo tmt --source-only --shard 0/4     Run one shard in CI

Exit codes:
  0  No gaps found
  1  Analysis or CLI error (invalid arguments or baseline tests failed)
  2  Test gaps, unstable predictions, or audit discrepancies found
  3  Incomplete evaluation (unconfirmed timeouts, no mutants executed, or source-only timeout/unevaluated)

Config file:
  Auto-discovered from the target directory upward to the workspace or Git root:
    tmt.toml, .config/tmt.toml, .cargo/tmt.toml
  Precedence: built-in defaults < top-level config < [package.<name>] < CLI flags.
  Use --config FILE to load a specific file, or --no-config to disable discovery.

Report counts (JSON):
  mutants_run = mutants_killed + mutants_survived
  mutants_timed_out is a subset of mutants_killed (mutants_timed_out <= mutants_killed)
  mutants_survived = survivor_breakdown.{reported + suppressed + redundant_constant + preexisting_unchanged + unchanged_not_compared + unconfirmed_gap}
  evaluation is \"partial\" when mutants_not_built, mutants_not_evaluated, skipped_sites, unparseable_source, unchanged_not_compared, unconfirmed timeouts, or source-only timeouts remain";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
enum OutputFormat {
    /// Human-readable terminal report.
    #[default]
    Text,
    /// Pretty-printed JSON report.
    Json,
    /// One compact JSON record per completed package (streamed).
    Jsonl,
    /// SARIF 2.1.0 for code scanning tools.
    Sarif,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum Sharding {
    Slice,
    RoundRobin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum TimeoutRecheck {
    Serial,
    None,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
enum TestTool {
    #[default]
    Cargo,
    Nextest,
}

/// Targeted mutation testing: finds the wrong implementations your tests would let
/// through, and says which input or assertion would catch them.
#[derive(Parser)]
#[command(
    name = "cargo-tmt",
    bin_name = "cargo tmt",
    version,
    after_help = SHORT_EXAMPLES,
    after_long_help = LONG_EXAMPLES
)]
struct Cli {
    // ── Target & Scope ────────────────────────────────────────────────────────
    /// Directory of the Cargo package or workspace to analyse.
    #[arg(default_value = ".")]
    dir: PathBuf,

    /// Directory of the Cargo package or workspace to analyse.
    #[arg(
        short = 'd',
        long = "dir",
        value_name = "DIR",
        conflicts_with = "dir",
        help_heading = HEADING_TARGET
    )]
    directory: Option<PathBuf>,

    /// Workspace member to analyse (repeatable or comma-separated).
    #[arg(
        short,
        long,
        value_name = "PACKAGE",
        value_delimiter = ',',
        conflicts_with = "workspace",
        help_heading = HEADING_TARGET
    )]
    package: Vec<String>,

    /// Analyse all workspace members.
    #[arg(long, help_heading = HEADING_TARGET)]
    workspace: bool,

    /// Analyse only code changed since this Git revision, plus untracked files.
    #[arg(long, value_name = "REV", help_heading = HEADING_TARGET)]
    since: Option<String>,

    /// Analyse only these lines: FILE:FROM-TO, FILE:LINE, or FILE (repeatable).
    ///
    /// Paths are relative to each analysed package.
    #[arg(long, value_name = "FILE:FROM-TO", help_heading = HEADING_TARGET)]
    lines: Vec<String>,

    /// Configuration file to load instead of auto-discovering `tmt.toml`.
    #[arg(
        long,
        value_name = "FILE",
        conflicts_with = "no_config",
        help_heading = HEADING_TARGET
    )]
    config: Option<PathBuf>,

    /// Disable automatic discovery of `tmt.toml`, `.config/tmt.toml`, and `.cargo/tmt.toml`.
    #[arg(long, help_heading = HEADING_TARGET)]
    no_config: bool,

    // ── Mutation Mode ─────────────────────────────────────────────────────────
    /// Also build source-only mutants and verify distinguishing witnesses (slower).
    ///
    /// Runs mutants that cannot be expressed as runtime probes (such as `const fn`,
    /// `impl Iterator` bodies, and shared-target modules) and synthesizes concrete
    /// distinguishing inputs or assertions for supported survivors.
    #[arg(long, help_heading = HEADING_MUTATION)]
    rebuild: bool,

    /// Build every supported mutant directly from source instead of using probes.
    ///
    /// Useful when tests spawn children that clear environment variables. Slower than
    /// probes and omits trace-based input or assertion suggestions.
    #[arg(
        long,
        conflicts_with_all = ["rebuild", "audit", "no_verify"],
        help_heading = HEADING_MUTATION
    )]
    source_only: bool,

    /// Preview source mutation candidates without building or running tests.
    #[arg(
        long,
        conflicts_with_all = ["sarif", "baseline", "write_baseline"],
        help_heading = HEADING_MUTATION
    )]
    list: bool,

    /// Also replace `Result` returns with `Err(EXPR)` (repeatable).
    ///
    /// EXPR is Rust code evaluated in the function's scope (e.g. `"mutated".into()`).
    #[arg(
        long = "error",
        value_name = "EXPR",
        allow_hyphen_values = true,
        help_heading = HEADING_MUTATION
    )]
    error_values: Vec<String>,

    /// Replace only the operator token without parenthesizing operands (`--source-only`).
    #[arg(long, help_heading = HEADING_MUTATION)]
    literal_operators: bool,

    /// Select zero-based source-only shard K/N (`--source-only`).
    #[arg(long, value_name = "K/N", help_heading = HEADING_MUTATION)]
    shard: Option<report::Shard>,

    /// Method for distributing candidates across shards (default: slice).
    #[arg(
        long,
        requires = "shard",
        value_enum,
        value_name = "METHOD",
        help_heading = HEADING_MUTATION
    )]
    sharding: Option<Sharding>,

    /// Trust the trace without re-running tests against predicted survivors.
    #[arg(long, help_heading = HEADING_MUTATION)]
    no_verify: bool,

    /// Skip effect tracing on surviving mutants.
    #[arg(long, help_heading = HEADING_MUTATION)]
    no_effects: bool,

    /// Audit up to N survivors and probe mutants against full test runs.
    ///
    /// Re-runs up to N surviving mutants against every test binary, compares up to N
    /// probe mutants against fresh source builds, and records tests a second time to
    /// detect nondeterminism. Discrepancies exit with code 2.
    #[arg(
        long,
        value_name = "N",
        conflicts_with = "no_verify",
        help_heading = HEADING_MUTATION
    )]
    audit: Option<usize>,

    // ── Test Execution & Selection ────────────────────────────────────────────
    /// Also run tests from this workspace package (repeatable).
    #[arg(long, value_name = "PACKAGE", help_heading = HEADING_TESTS)]
    also_test: Vec<String>,

    /// Also run tests of workspace packages depending on this package.
    #[arg(long, help_heading = HEADING_TESTS)]
    dependents: bool,

    /// Run tests from all workspace members against every mutation.
    #[arg(
        long,
        value_name = "BOOL",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "true",
        help_heading = HEADING_TESTS
    )]
    test_workspace: Option<bool>,

    /// Also run doc tests (recompiled per reached mutant; slower).
    #[arg(long, help_heading = HEADING_TESTS)]
    doc: bool,

    /// Test runner (`nextest` requires `--source-only`).
    #[arg(
        long,
        value_enum,
        value_name = "TOOL",
        help_heading = HEADING_TESTS
    )]
    test_tool: Option<TestTool>,

    /// Nextest profile for `--test-tool nextest`.
    #[arg(long, value_name = "PROFILE", help_heading = HEADING_TESTS)]
    nextest_profile: Option<String>,

    /// Run each selected libtest test in its own process.
    #[arg(long, help_heading = HEADING_TESTS)]
    isolate_tests: bool,

    /// Test only the package's library target.
    #[arg(long, help_heading = HEADING_TESTS)]
    lib: bool,

    /// Test all binary targets.
    #[arg(long, help_heading = HEADING_TESTS)]
    bins: bool,

    /// Test only the specified integration test target (repeatable).
    #[arg(long = "test", value_name = "NAME", help_heading = HEADING_TESTS)]
    integration_test: Vec<String>,

    /// Cargo test targets to run and build when possible (repeatable).
    #[arg(long, value_name = "TARGET", help_heading = HEADING_TESTS)]
    test_target: Vec<String>,

    /// Include libtest tests whose name contains PATTERN (repeatable).
    #[arg(long, value_name = "PATTERN", help_heading = HEADING_TESTS)]
    test_filter: Vec<String>,

    /// Include exact libtest test names (repeatable).
    #[arg(long, value_name = "NAME", help_heading = HEADING_TESTS)]
    include_test: Vec<String>,

    /// Exclude exact libtest test names (repeatable).
    #[arg(long, value_name = "NAME", help_heading = HEADING_TESTS)]
    exclude_test: Vec<String>,

    /// Also run tests marked `#[ignore]`.
    #[arg(long, help_heading = HEADING_TESTS)]
    include_ignored: bool,

    /// Workspace-relative wrapper invoked with each test binary.
    #[arg(long, value_name = "PATH", help_heading = HEADING_TESTS)]
    test_wrapper: Option<PathBuf>,

    /// Grace period for `--test-wrapper` on Unix timeout (default: 2; 0 kills immediately).
    #[arg(long, value_name = "SECONDS", help_heading = HEADING_TESTS)]
    wrapper_grace: Option<u64>,

    // ── Output & Baselines ────────────────────────────────────────────────────
    /// Output format (default: text).
    #[arg(
        long,
        value_enum,
        value_name = "FORMAT",
        help_heading = HEADING_OUTPUT
    )]
    format: Option<OutputFormat>,

    /// Also list mutation sites that no test executed.
    #[arg(long, help_heading = HEADING_OUTPUT)]
    show_unreached: bool,

    /// Suppress progress output on stderr.
    #[arg(short, long, help_heading = HEADING_OUTPUT)]
    quiet: bool,

    /// Suppress findings already recorded in this baseline file.
    #[arg(long, value_name = "FILE", help_heading = HEADING_OUTPUT)]
    baseline: Option<PathBuf>,

    /// Save this run's findings to a baseline file for future `--baseline` runs.
    #[arg(long, value_name = "FILE", help_heading = HEADING_OUTPUT)]
    write_baseline: Option<PathBuf>,

    /// Shorthand for `--format json`.
    #[arg(
        long,
        conflicts_with_all = ["format", "jsonl", "sarif"],
        help_heading = HEADING_OUTPUT
    )]
    json: bool,

    /// Shorthand for `--format jsonl`.
    #[arg(
        long,
        conflicts_with_all = ["format", "json", "sarif"],
        help_heading = HEADING_OUTPUT
    )]
    jsonl: bool,

    /// Shorthand for `--format sarif`.
    #[arg(
        long,
        conflicts_with_all = ["format", "json", "jsonl"],
        help_heading = HEADING_OUTPUT
    )]
    sarif: bool,

    // ── Performance & Cargo Build ─────────────────────────────────────────────
    /// Mutants to run in parallel (default: 1).
    ///
    /// Raise this only when tests can run concurrently with themselves (no fixed ports
    /// or shared files).
    #[arg(
        short,
        long,
        value_name = "N",
        value_parser = clap::value_parser!(u32).range(1..),
        help_heading = HEADING_BUILD
    )]
    jobs: Option<u32>,

    /// Concurrent isolated baseline tests (default: 1).
    #[arg(
        long,
        value_name = "N",
        value_parser = clap::value_parser!(u32).range(1..),
        help_heading = HEADING_BUILD
    )]
    baseline_jobs: Option<u32>,

    /// Timeout per test runner invocation under a mutant, in seconds.
    #[arg(
        long,
        value_name = "SECONDS",
        value_parser = clap::value_parser!(u64).range(1..),
        help_heading = HEADING_BUILD
    )]
    timeout: Option<u64>,

    /// Policy for rechecking parallel worker timeouts (default: serial).
    #[arg(
        long,
        value_enum,
        value_name = "POLICY",
        help_heading = HEADING_BUILD
    )]
    timeout_recheck: Option<TimeoutRecheck>,

    /// Save and resume completed mutant outcomes via `checkpoint.jsonl`.
    #[arg(long, conflicts_with = "list", help_heading = HEADING_BUILD)]
    checkpoint: bool,

    /// Override Cargo optimisation level for test builds (0 to 3).
    ///
    /// Applies to both probe and `--source-only` builds. Probes have near-zero overhead
    /// when optimised, which helps loop-heavy tests, at the cost of slower builds and
    /// tests differing from plain `cargo test`.
    #[arg(
        long,
        value_name = "N",
        value_parser = clap::value_parser!(u8).range(0..=3),
        help_heading = HEADING_BUILD
    )]
    opt_level: Option<u8>,

    /// Cargo features to enable, comma-separated (repeatable).
    #[arg(
        long,
        short = 'F',
        value_name = "FEATURES",
        value_delimiter = ',',
        help_heading = HEADING_BUILD
    )]
    features: Vec<String>,

    /// Build with all Cargo features enabled.
    #[arg(
        long,
        conflicts_with_all = ["features", "no_default_features"],
        help_heading = HEADING_BUILD
    )]
    all_features: bool,

    /// Build without default Cargo features.
    #[arg(long, help_heading = HEADING_BUILD)]
    no_default_features: bool,

    /// Pass `--locked` to Cargo so `Cargo.lock` stays unchanged.
    #[arg(long, help_heading = HEADING_BUILD)]
    locked: bool,

    // ── Work Directory & Fixtures ─────────────────────────────────────────────
    /// Directory for the isolated working copy and build (default: `<target-dir>/tmt`).
    #[arg(long, value_name = "DIR", help_heading = HEADING_WORK)]
    work_dir: Option<PathBuf>,

    /// Also copy gitignored files into the working copy.
    ///
    /// Needed when the build reads generated, untracked files. Git metadata,
    /// `node_modules`, and build caches remain excluded by default.
    #[arg(long, help_heading = HEADING_WORK)]
    all_files: bool,

    /// Create an independent Git index in the working copy for commands like `git ls-files`.
    #[arg(long, help_heading = HEADING_WORK)]
    git_index: bool,

    /// Copy a workspace-relative path even if gitignored (repeatable).
    #[arg(long, value_name = "PATH", help_heading = HEADING_WORK)]
    copy_fixture: Vec<PathBuf>,

    /// Exclude a workspace-relative path from `--copy-fixture` (repeatable).
    #[arg(long, value_name = "PATH", help_heading = HEADING_WORK)]
    copy_fixture_exclude: Vec<PathBuf>,

    /// Stage workspace path FROM at TO relative to isolated Cargo output (repeatable).
    #[arg(long, value_name = "FROM=TO", help_heading = HEADING_WORK)]
    copy_output_fixture: Vec<targeted_mutation::output_fixture::Fixture>,

    /// Stage an external parent-relative path into the isolated work copy (repeatable).
    ///
    /// Accepts either `PATH` (e.g. `../../proto`) or `SRC=REL` (e.g. `/abs/proto=../../proto`),
    /// where `REL` ascends above the workspace via `..`.
    #[arg(long, value_name = "SRC[=REL]", help_heading = HEADING_WORK)]
    stage_external: Vec<driver::StageExternal>,
}

impl From<OutputFormat> for config::OutputFormat {
    fn from(value: OutputFormat) -> Self {
        match value {
            OutputFormat::Text => Self::Text,
            OutputFormat::Json => Self::Json,
            OutputFormat::Jsonl => Self::Jsonl,
            OutputFormat::Sarif => Self::Sarif,
        }
    }
}

impl From<config::OutputFormat> for OutputFormat {
    fn from(value: config::OutputFormat) -> Self {
        match value {
            config::OutputFormat::Text => Self::Text,
            config::OutputFormat::Json => Self::Json,
            config::OutputFormat::Jsonl => Self::Jsonl,
            config::OutputFormat::Sarif => Self::Sarif,
        }
    }
}

impl From<TimeoutRecheck> for config::TimeoutRecheck {
    fn from(value: TimeoutRecheck) -> Self {
        match value {
            TimeoutRecheck::Serial => Self::Serial,
            TimeoutRecheck::None => Self::None,
        }
    }
}

impl From<TestTool> for config::TestTool {
    fn from(value: TestTool) -> Self {
        match value {
            TestTool::Cargo => Self::Cargo,
            TestTool::Nextest => Self::Nextest,
        }
    }
}

impl Cli {
    fn explicit_output_format(&self) -> Option<OutputFormat> {
        if let Some(format) = self.format {
            Some(format)
        } else if self.json {
            Some(OutputFormat::Json)
        } else if self.jsonl {
            Some(OutputFormat::Jsonl)
        } else if self.sarif {
            Some(OutputFormat::Sarif)
        } else {
            None
        }
    }

    fn output_format(&self) -> OutputFormat {
        self.explicit_output_format().unwrap_or_default()
    }

    fn target_dir(&self) -> PathBuf {
        self.directory.clone().unwrap_or_else(|| self.dir.clone())
    }

    fn to_settings(&self) -> config::Settings {
        let non_empty_vec = |v: &[String]| (!v.is_empty()).then(|| v.to_vec());
        let non_empty_paths = |v: &[PathBuf]| (!v.is_empty()).then(|| v.to_vec());
        config::Settings {
            rebuild: self.rebuild.then_some(true),
            source_only: self.source_only.then_some(true),
            error: non_empty_vec(&self.error_values),
            literal_operators: self.literal_operators.then_some(true),
            verify: self.no_verify.then_some(false),
            effects: self.no_effects.then_some(false),
            audit: self.audit,
            also_test: non_empty_vec(&self.also_test),
            dependents: self.dependents.then_some(true),
            test_workspace: self.test_workspace,
            doc: self.doc.then_some(true),
            test_tool: self.test_tool.map(Into::into),
            nextest_profile: self.nextest_profile.clone(),
            isolate_tests: self.isolate_tests.then_some(true),
            lib: self.lib.then_some(true),
            bins: self.bins.then_some(true),
            test: non_empty_vec(&self.integration_test),
            test_target: non_empty_vec(&self.test_target),
            test_filter: non_empty_vec(&self.test_filter),
            include_test: non_empty_vec(&self.include_test),
            exclude_test: non_empty_vec(&self.exclude_test),
            include_ignored: self.include_ignored.then_some(true),
            test_wrapper: self.test_wrapper.clone(),
            wrapper_grace: self.wrapper_grace,
            format: self.explicit_output_format().map(Into::into),
            show_unreached: self.show_unreached.then_some(true),
            quiet: self.quiet.then_some(true),
            baseline: self.baseline.clone(),
            jobs: self.jobs,
            baseline_jobs: self.baseline_jobs,
            timeout: self.timeout,
            timeout_recheck: self.timeout_recheck.map(Into::into),
            checkpoint: self.checkpoint.then_some(true),
            opt_level: self.opt_level,
            features: non_empty_vec(&self.features),
            all_features: self.all_features.then_some(true),
            no_default_features: self.no_default_features.then_some(true),
            locked: self.locked.then_some(true),
            work_dir: self.work_dir.clone(),
            all_files: self.all_files.then_some(true),
            git_index: self.git_index.then_some(true),
            copy_fixture: non_empty_paths(&self.copy_fixture),
            copy_fixture_exclude: non_empty_paths(&self.copy_fixture_exclude),
            copy_output_fixture: (!self.copy_output_fixture.is_empty()).then(|| {
                self.copy_output_fixture
                    .iter()
                    .map(|f| format!("{}={}", f.from.display(), f.to.display()))
                    .collect()
            }),
            stage_external: (!self.stage_external.is_empty()).then(|| {
                self.stage_external
                    .iter()
                    .map(|s| format!("{}={}", s.from.display(), s.rel.display()))
                    .collect()
            }),
        }
    }

    fn validate(&self) -> Result<(), clap::Error> {
        use clap::{CommandFactory, error::ErrorKind};

        if matches!(self.test_tool, Some(TestTool::Cargo)) && self.nextest_profile.is_some() {
            return Err(Cli::command().error(
                ErrorKind::MissingRequiredArgument,
                "--nextest-profile requires --test-tool nextest",
            ));
        }
        if self.list && self.output_format() == OutputFormat::Sarif {
            return Err(Cli::command().error(
                ErrorKind::ArgumentConflict,
                "the argument '--list' cannot be used with '--format sarif'",
            ));
        }
        Ok(())
    }
}

fn build_driver_options(
    cli: &Cli,
    effective: &config::Settings,
    require_source_only_for_shard: bool,
) -> Result<driver::Options> {
    let source_only = effective.source_only.unwrap_or(false);
    if require_source_only_for_shard && cli.shard.is_some() && !source_only {
        bail!("--shard requires --source-only (or `source_only = true` in config)");
    }
    let output_format: OutputFormat = effective.format.unwrap_or_default().into();
    if cli.list && output_format == OutputFormat::Sarif {
        bail!("the argument '--list' cannot be used with '--format sarif'");
    }
    let nextest = matches!(effective.test_tool, Some(config::TestTool::Nextest));
    let output_fixtures = if !cli.copy_output_fixture.is_empty() {
        cli.copy_output_fixture.clone()
    } else {
        effective.parsed_output_fixtures()?
    };
    let stage_external = if !cli.stage_external.is_empty() {
        cli.stage_external.clone()
    } else {
        effective.parsed_stage_external()?
    };
    Ok(driver::Options {
        dir: cli.target_dir(),
        package: cli.package.first().cloned(),
        list: cli.list,
        test_workspace: effective.test_workspace.unwrap_or(false),
        verify: effective.verify.unwrap_or(true),
        work_dir: effective.work_dir.clone(),
        progress: !effective.quiet.unwrap_or(false),
        since: cli.since.clone(),
        lines: cli.lines.clone(),
        baseline: if cli.list {
            None
        } else {
            effective.baseline.clone()
        },
        write_baseline: cli.write_baseline.clone(),
        also_test: effective.also_test.clone().unwrap_or_default(),
        dependents: effective.dependents.unwrap_or(false),
        effects: effective.effects.unwrap_or(true),
        rebuild: effective.rebuild.unwrap_or(false),
        error_values: effective.error.clone().unwrap_or_default(),
        source_only,
        literal_operators: effective.literal_operators.unwrap_or(false),
        shard: cli.shard.map(|mut shard| {
            shard.round_robin = matches!(cli.sharding, Some(Sharding::RoundRobin));
            shard
        }),
        nextest: nextest.then_some(report::Nextest {
            profile: effective.nextest_profile.clone(),
        }),
        test_selection: driver::TestSelection {
            isolate: effective.isolate_tests.unwrap_or(false),
            lib: effective.lib.unwrap_or(false),
            bins: effective.bins.unwrap_or(false),
            integration_tests: effective.test.clone().unwrap_or_default(),
            targets: effective.test_target.clone().unwrap_or_default(),
            include: effective.include_test.clone().unwrap_or_default(),
            filter: effective.test_filter.clone().unwrap_or_default(),
            exclude: effective.exclude_test.clone().unwrap_or_default(),
            include_ignored: effective.include_ignored.unwrap_or(false),
            wrapper: effective.test_wrapper.clone(),
        },
        opt_level: effective.opt_level,
        timeout_seconds: effective.timeout,
        baseline_jobs: effective.baseline_jobs.unwrap_or(1) as usize,
        serial_timeout_rechecks: !matches!(
            effective.timeout_recheck,
            Some(config::TimeoutRecheck::None)
        ),
        wrapper_grace: std::time::Duration::from_secs(effective.wrapper_grace.unwrap_or(2)),
        audit: effective.audit,
        cargo: driver::CargoConfig {
            features: effective.features.clone().unwrap_or_default(),
            all_features: effective.all_features.unwrap_or(false),
            no_default_features: effective.no_default_features.unwrap_or(false),
            locked: effective.locked.unwrap_or(false),
        },
        jobs: effective.jobs.unwrap_or(1) as usize,
        all_files: effective.all_files.unwrap_or(false),
        git_index: effective.git_index.unwrap_or(false),
        copy_fixtures: effective.copy_fixture.clone().unwrap_or_default(),
        copy_fixture_excludes: effective.copy_fixture_exclude.clone().unwrap_or_default(),
        output_fixtures,
        stage_external,
        checkpoint: if cli.list {
            false
        } else {
            effective.checkpoint.unwrap_or(false)
        },
        doc: effective.doc.unwrap_or(false),
    })
}

fn resolve_settings(
    loaded: Option<&config::LoadedConfig>,
    package: Option<&str>,
    cli_settings: &config::Settings,
) -> Result<config::Settings> {
    match loaded {
        Some(cfg) => cfg.resolve(package, cli_settings),
        None => config::resolve_without_config(cli_settings),
    }
}

fn resolve_workspace_base_settings(
    loaded: Option<&config::LoadedConfig>,
    cli_settings: &config::Settings,
) -> Result<config::Settings> {
    match loaded {
        Some(cfg) => cfg.resolve_workspace_base(cli_settings),
        None => config::resolve_workspace_base_without_config(cli_settings),
    }
}

/// Parses and validates CLI arguments.
///
/// Help and version requests print to stdout and return `Err(ExitCode::SUCCESS)`.
/// All argument parse and validation errors print to stderr and return
/// `Err(ExitCode::FAILURE)` (exit code 1) so they are never confused with exit code 2
/// (test gaps found).
fn parse_cli(given: impl IntoIterator<Item = std::ffi::OsString>) -> Result<Cli, ExitCode> {
    let cli = match Cli::try_parse_from(arguments(given)) {
        Ok(cli) => cli,
        Err(err) => {
            if err.use_stderr() {
                eprint!("{}", report::sanitize_terminal(&err.render().to_string()));
                return Err(ExitCode::FAILURE);
            }
            let _ = err.print();
            return Err(ExitCode::SUCCESS);
        }
    };
    if let Err(err) = cli.validate() {
        eprint!("{}", report::sanitize_terminal(&err.render().to_string()));
        return Err(ExitCode::FAILURE);
    }
    Ok(cli)
}

/// Exit codes: 0 no gaps, 1 analysis/CLI error, 2 gaps found, 3 incomplete evaluation.
fn main() -> ExitCode {
    let cli = match parse_cli(std::env::args_os()) {
        Ok(cli) => cli,
        Err(code) => return code,
    };
    let target_dir = cli.target_dir();
    let loaded = match config::load_for_dir(&target_dir, cli.config.as_deref(), cli.no_config) {
        Ok(loaded) => loaded,
        Err(err) => {
            eprintln!(
                "tmt: error: {}",
                report::sanitize_terminal(&format!("{err:#}"))
            );
            return ExitCode::FAILURE;
        }
    };
    let cli_settings = cli.to_settings();
    let is_workspace = cli.workspace || cli.package.len() > 1;
    let mut single_package = if is_workspace {
        None
    } else {
        cli.package
            .first()
            .cloned()
            .or_else(|| config::detect_package_name(&target_dir))
    };
    // For a single-member virtual workspace using glob `members` (e.g. `crates/*`),
    // ask Cargo which package is selected when the config has per-package overrides.
    if !is_workspace
        && single_package.is_none()
        && loaded
            .as_ref()
            .is_some_and(|cfg| !cfg.file.package.is_empty())
        && let Ok(base_eff) = resolve_workspace_base_settings(loaded.as_ref(), &cli_settings)
    {
        let base_cargo = driver::CargoConfig {
            features: base_eff.features.unwrap_or_default(),
            all_features: base_eff.all_features.unwrap_or(false),
            no_default_features: base_eff.no_default_features.unwrap_or(false),
            locked: base_eff.locked.unwrap_or(false),
        };
        single_package = driver::select_single_package_name(&target_dir, &base_cargo);
    }
    let effective = match if is_workspace {
        resolve_workspace_base_settings(loaded.as_ref(), &cli_settings)
    } else {
        resolve_settings(loaded.as_ref(), single_package.as_deref(), &cli_settings)
    } {
        Ok(effective) => effective,
        Err(err) => {
            eprintln!(
                "tmt: error: {}",
                report::sanitize_terminal(&format!("{err:#}"))
            );
            return ExitCode::FAILURE;
        }
    };
    let output_format: OutputFormat = effective.format.unwrap_or_default().into();
    let show_unreached = effective.show_unreached.unwrap_or(false);
    let options = match build_driver_options(&cli, &effective, !is_workspace) {
        Ok(options) => options,
        Err(err) => {
            eprintln!(
                "tmt: error: {}",
                report::sanitize_terminal(&format!("{err:#}"))
            );
            return ExitCode::FAILURE;
        }
    };
    let per_package = |name: &str, _base: &driver::Options| -> Result<driver::Options> {
        let member_effective = resolve_settings(loaded.as_ref(), Some(name), &cli_settings)?;
        build_driver_options(&cli, &member_effective, true)
    };
    if output_format == OutputFormat::Jsonl {
        return stream_reports(&options, &cli.package, is_workspace, per_package);
    }
    if is_workspace {
        let members = match driver::run_packages_with(&options, &cli.package, per_package) {
            Ok(members) => members,
            Err(err) => {
                eprintln!(
                    "tmt: error: {}",
                    report::sanitize_terminal(&format!("{err:#}"))
                );
                return ExitCode::FAILURE;
            }
        };
        return show_workspace(members, output_format, show_unreached);
    }
    match driver::run(&options) {
        Ok(result) => {
            show_report(&result, output_format, show_unreached);
            ExitCode::from(report_exit(&result))
        }
        Err(err) => {
            eprintln!(
                "tmt: error: {}",
                report::sanitize_terminal(&format!("{err:#}"))
            );
            ExitCode::FAILURE
        }
    }
}

fn incomplete_source_run(result: &report::Report) -> bool {
    result.timeout_rechecks.unconfirmed > 0
        || (result.analysis.source_only
            && (result.mutants_timed_out > 0 || result.mutants_not_evaluated > 0))
}

fn report_exit(result: &report::Report) -> u8 {
    if incomplete_source_run(result) {
        3
    } else if result.has_gaps() {
        2
    } else if result.evaluation == Some(report::Evaluation::NoMutantsExecuted) {
        3
    } else {
        0
    }
}

fn report_json(result: &report::Report) -> serde_json::Value {
    if let Some(mutants) = &result.mutants {
        serde_json::json!({
            "schema_version": report::SCHEMA_VERSION,
            "package": result.package,
            "analysis": result.analysis,
            "mutants": mutants,
            "skipped": result.skipped,
        })
    } else {
        serde_json::to_value(result).expect("report serialises")
    }
}

fn show_report(result: &report::Report, format: OutputFormat, show_unreached: bool) {
    match format {
        OutputFormat::Sarif => println!("{}", report::render_sarif(result)),
        OutputFormat::Json => {
            if result.mutants.is_some() {
                println!("{:#}", report_json(result));
            } else {
                println!(
                    "{}",
                    serde_json::to_string_pretty(result).expect("report serialises")
                );
            }
        }
        OutputFormat::Text => {
            if let Some(mutants) = &result.mutants {
                println!(
                    "{}: {} source candidates",
                    report::sanitize_inline(&result.package),
                    mutants.len()
                );
                for candidate in mutants {
                    let edit = &candidate.edits[0];
                    println!(
                        "{}:{}:{}: {} -> {}",
                        report::sanitize_inline(&edit.file),
                        edit.line,
                        edit.column,
                        report::sanitize_inline(&candidate.what),
                        report::sanitize_inline(&candidate.into)
                    );
                }
            } else {
                print!("{}", report::render_text(result, show_unreached));
            }
        }
        OutputFormat::Jsonl => unreachable!("jsonl output is streamed by stream_reports"),
    }
}

fn stream_reports(
    options: &driver::Options,
    packages: &[String],
    workspace: bool,
    options_for_package: impl FnMut(&str, &driver::Options) -> Result<driver::Options>,
) -> ExitCode {
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    let mut status = (0, 0);
    let mut emit = |name: String, result: anyhow::Result<report::Report>| -> anyhow::Result<()> {
        let (record, rank) = match result {
            Ok(result) => {
                let code = report_exit(&result);
                let priority = if incomplete_source_run(&result) {
                    3
                } else {
                    match code {
                        2 => 2,
                        3 => 1,
                        _ => 0,
                    }
                };
                let mut record = serde_json::json!({
                    "schema_version": report::SCHEMA_VERSION,
                    "package": result.package,
                });
                record["report"] = report_json(&result);
                (record, (priority, code))
            }
            Err(err) => (
                serde_json::json!({
                    "schema_version": report::SCHEMA_VERSION,
                    "package": name,
                    "error": format!("{err:#}"),
                }),
                (4, 1),
            ),
        };
        status = status.max(rank);
        serde_json::to_writer(&mut stdout, &record)?;
        writeln!(stdout)?;
        stdout.flush()?;
        Ok(())
    };
    let result = if workspace {
        driver::run_packages_each_with(options, packages, options_for_package, &mut emit)
    } else {
        emit(
            options.package.clone().unwrap_or_default(),
            driver::run(options),
        )
    };
    if let Err(err) = result {
        eprintln!(
            "tmt: error: {}",
            report::sanitize_terminal(&format!("{err:#}"))
        );
        return ExitCode::FAILURE;
    }
    ExitCode::from(status.1)
}

fn show_workspace(
    members: Vec<(String, anyhow::Result<report::Report>)>,
    format: OutputFormat,
    show_unreached: bool,
) -> ExitCode {
    let mut reports = vec![];
    let mut errors = vec![];
    for (package, result) in members {
        match result {
            Ok(result) => reports.push(result),
            Err(err) => {
                let message = format!("{err:#}");
                eprintln!(
                    "tmt: {}: {}",
                    report::sanitize_inline(&package),
                    report::sanitize_terminal(&message)
                );
                errors.push(serde_json::json!({"package": package, "error": message}));
            }
        }
    }
    let has_errors = !errors.is_empty();
    match format {
        OutputFormat::Json => {
            let mut combined = serde_json::json!({
                "schema_version": report::SCHEMA_VERSION,
            });
            combined["packages"] =
                serde_json::Value::Array(reports.iter().map(report_json).collect());
            combined["errors"] = serde_json::Value::Array(errors);
            println!("{combined:#}");
        }
        OutputFormat::Sarif => {
            let failed: Vec<(String, String)> = errors
                .iter()
                .map(|error| {
                    let text = |key: &str| error[key].as_str().unwrap_or_default().to_string();
                    (text("package"), text("error"))
                })
                .collect();
            println!("{:#}", report::workspace_sarif(&reports, &failed));
        }
        OutputFormat::Text => {
            for result in &reports {
                show_report(result, OutputFormat::Text, show_unreached);
            }
        }
        OutputFormat::Jsonl => unreachable!("jsonl output is streamed by stream_reports"),
    }
    ExitCode::from(if has_errors {
        1
    } else if reports.iter().any(incomplete_source_run) {
        3
    } else if reports.iter().any(|report| report_exit(report) == 2) {
        2
    } else if reports.iter().any(|report| report_exit(report) == 3) {
        3
    } else {
        0
    })
}

/// The arguments as they were meant. Cargo starts `cargo tmt --json` as
/// `cargo-tmt tmt --json`: the name of the subcommand comes first and is not one of
/// the arguments. (A directory called `tmt` is given as `./tmt`.)
fn arguments(given: impl IntoIterator<Item = std::ffi::OsString>) -> Vec<std::ffi::OsString> {
    let mut all: Vec<std::ffi::OsString> = given.into_iter().collect();
    if all.get(1).is_some_and(|first| first == "tmt") {
        all.remove(1);
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    fn given(args: &[&str]) -> Vec<String> {
        arguments(args.iter().map(std::ffi::OsString::from))
            .into_iter()
            .map(|arg| arg.into_string().unwrap())
            .collect()
    }

    #[test]
    fn the_name_of_the_subcommand_is_not_an_argument() {
        // As cargo starts it, and as it is started by its own name.
        assert_eq!(
            given(&["cargo-tmt", "tmt", ".", "--json"]),
            ["cargo-tmt", ".", "--json"]
        );
        assert_eq!(
            given(&["cargo-tmt", ".", "--json"]),
            ["cargo-tmt", ".", "--json"]
        );
        assert_eq!(given(&["cargo-tmt", "tmt"]), ["cargo-tmt"]);
        assert_eq!(given(&["cargo-tmt"]), ["cargo-tmt"]);
        // Only the first, and only in the first place.
        assert_eq!(given(&["cargo-tmt", "tmt", "tmt"]), ["cargo-tmt", "tmt"]);
        assert_eq!(given(&["cargo-tmt", "./tmt"]), ["cargo-tmt", "./tmt"]);
        assert_eq!(
            given(&["cargo-tmt", "-p", "tmt"]),
            ["cargo-tmt", "-p", "tmt"]
        );
        // And what is left is what the options are read from.
        let cli = Cli::parse_from(given(&["cargo-tmt", "tmt", "crate", "--jobs", "3"]));
        assert_eq!((cli.dir, cli.jobs), (PathBuf::from("crate"), Some(3)));
        let cli = Cli::parse_from(given(&["cargo-tmt", "tmt"]));
        assert_eq!(cli.dir, PathBuf::from("."));
        assert!(Cli::try_parse_from(given(&["cargo-tmt", "tmt", "--jobs", "0"])).is_err());
    }

    #[test]
    fn output_format_resolves_unified_option_and_legacy_shorthands() {
        let default_cli = Cli::parse_from(given(&["cargo-tmt", "tmt"]));
        assert_eq!(default_cli.output_format(), OutputFormat::Text);
        assert!(default_cli.validate().is_ok());

        for (args, expected) in [
            (&["--format", "text"][..], OutputFormat::Text),
            (&["--format", "json"][..], OutputFormat::Json),
            (&["--format", "jsonl"][..], OutputFormat::Jsonl),
            (&["--format", "sarif"][..], OutputFormat::Sarif),
            (&["--json"][..], OutputFormat::Json),
            (&["--jsonl"][..], OutputFormat::Jsonl),
            (&["--sarif"][..], OutputFormat::Sarif),
            (&["--list", "--format", "json"][..], OutputFormat::Json),
            (&["--list", "--format", "jsonl"][..], OutputFormat::Jsonl),
        ] {
            let mut argv = vec!["cargo-tmt", "tmt"];
            argv.extend_from_slice(args);
            let cli = Cli::parse_from(given(&argv));
            assert_eq!(cli.output_format(), expected, "for {args:?}");
            assert!(cli.validate().is_ok(), "for {args:?}");
        }
    }

    #[test]
    fn parse_cli_exits_one_on_errors_and_zero_on_help_or_version() {
        for ok_exit in [
            &["-h"][..],
            &["--help"][..],
            &["-V"][..],
            &["--version"][..],
        ] {
            let mut argv = vec!["cargo-tmt", "tmt"];
            argv.extend_from_slice(ok_exit);
            assert_eq!(
                parse_cli(argv.iter().map(std::ffi::OsString::from)).err(),
                Some(ExitCode::SUCCESS),
                "expected exit 0 for {ok_exit:?}"
            );
        }

        // Every parse error, value-range error, and cross-option validation error exits 1
        // so CI never mistakes a CLI error for exit code 2 (test gaps found).
        for bad in [
            &["--format", "xml"][..],
            &["--bogus"][..],
            &["--format", "json", "--sarif"][..],
            &["--format", "text", "--json"][..],
            &["--format", "sarif", "--jsonl"][..],
            &["--json", "--jsonl"][..],
            &["--json", "--sarif"][..],
            &["--list", "--sarif"][..],
            &["--list", "--format", "sarif"][..],
            &["--config", "tmt.toml", "--no-config"][..],
            &["--jobs", "0"][..],
            &["--baseline-jobs", "0"][..],
            &["--timeout", "0"][..],
            &["--opt-level", "4"][..],
        ] {
            let mut argv = vec!["cargo-tmt", "tmt"];
            argv.extend_from_slice(bad);
            assert_eq!(
                parse_cli(argv.iter().map(std::ffi::OsString::from)).err(),
                Some(ExitCode::FAILURE),
                "expected exit 1 for {bad:?}"
            );
        }

        // Cross-option requirements validated once effective settings are resolved
        // (so a config file can provide the companion setting, e.g. `test_tool = "nextest"`).
        for bad_effective in [
            &["--nextest-profile", "ci"][..],
            &["--wrapper-grace", "5"][..],
            &["--literal-operators"][..],
            &["--shard", "0/2"][..],
            &["--copy-fixture-exclude", "a"][..],
        ] {
            let mut argv = vec!["cargo-tmt", "tmt"];
            argv.extend_from_slice(bad_effective);
            let cli = parse_cli(argv.iter().map(std::ffi::OsString::from))
                .expect("syntactically valid CLI");
            let resolved = resolve_settings(None, None, &cli.to_settings())
                .and_then(|eff| build_driver_options(&cli, &eff, true));
            assert!(
                resolved.is_err(),
                "expected effective validation error for {bad_effective:?}"
            );
        }

        let valid_nextest = parse_cli(
            [
                "cargo-tmt",
                "tmt",
                "--source-only",
                "--test-tool",
                "nextest",
                "--nextest-profile",
                "ci",
            ]
            .iter()
            .map(std::ffi::OsString::from),
        );
        assert!(valid_nextest.is_ok());
    }

    #[test]
    fn all_legacy_long_flags_remain_available() {
        let cmd = Cli::command();
        let longs: std::collections::BTreeSet<&str> = cmd
            .get_arguments()
            .filter_map(|arg| arg.get_long())
            .collect();
        for flag in [
            "dir",
            "package",
            "workspace",
            "list",
            "test-workspace",
            "json",
            "jsonl",
            "no-verify",
            "work-dir",
            "doc",
            "all-files",
            "git-index",
            "copy-fixture",
            "copy-fixture-exclude",
            "copy-output-fixture",
            "checkpoint",
            "jobs",
            "since",
            "lines",
            "baseline",
            "write-baseline",
            "also-test",
            "dependents",
            "no-effects",
            "rebuild",
            "error",
            "source-only",
            "literal-operators",
            "shard",
            "sharding",
            "test-tool",
            "nextest-profile",
            "isolate-tests",
            "test-target",
            "include-test",
            "exclude-test",
            "include-ignored",
            "test-wrapper",
            "opt-level",
            "timeout",
            "baseline-jobs",
            "timeout-recheck",
            "wrapper-grace",
            "features",
            "all-features",
            "no-default-features",
            "locked",
            "audit",
            "sarif",
            "show-unreached",
            "quiet",
            "format",
            "config",
            "no-config",
        ] {
            assert!(longs.contains(flag), "missing flag --{flag}");
        }
    }

    #[test]
    fn cli_definition_and_grouped_help_are_valid() {
        let mut cmd = Cli::command();
        cmd.clone().debug_assert();

        let long_help = cmd.render_long_help().to_string();
        for heading in [
            HEADING_TARGET,
            HEADING_MUTATION,
            HEADING_TESTS,
            HEADING_OUTPUT,
            HEADING_BUILD,
            HEADING_WORK,
        ] {
            assert!(
                long_help.contains(heading),
                "missing heading `{heading}` in --help"
            );
        }
        assert!(long_help.contains("--format <FORMAT>"));
        assert!(long_help.contains("--config <FILE>"));
        assert!(long_help.contains("--no-config"));
        assert!(long_help.contains("Config file:"));
        assert!(long_help.contains("Examples:"));
        assert!(long_help.contains("Exit codes:"));
    }

    #[test]
    fn config_supplies_companion_flags_for_cli_and_list_ignores_baseline_and_checkpoint() {
        let cfg = config::LoadedConfig {
            path: PathBuf::from("tmt.toml"),
            base_dir: PathBuf::from("."),
            file: config::parse_config(
                r#"
                source_only = true
                test_tool = "nextest"
                baseline = ".tmt-baseline.json"
                checkpoint = true
                "#,
                "tmt.toml",
            )
            .unwrap(),
        };

        // Companion flags work when the prerequisite is set in config.
        let cli = parse_cli(
            [
                "cargo-tmt",
                "tmt",
                "--shard",
                "0/2",
                "--literal-operators",
                "--nextest-profile",
                "ci",
            ]
            .iter()
            .map(std::ffi::OsString::from),
        )
        .unwrap();
        let eff = resolve_settings(Some(&cfg), None, &cli.to_settings()).unwrap();
        let opts = build_driver_options(&cli, &eff, true).unwrap();
        assert!(opts.source_only);
        assert!(opts.literal_operators);
        assert_eq!(opts.shard.map(|s| (s.index, s.count)), Some((0, 2)));
        assert_eq!(
            opts.nextest.as_ref().and_then(|n| n.profile.as_deref()),
            Some("ci")
        );

        // `--list` gracefully ignores config-supplied `baseline` and `checkpoint`.
        let list_cli = parse_cli(
            ["cargo-tmt", "tmt", "--list"]
                .iter()
                .map(std::ffi::OsString::from),
        )
        .unwrap();
        let list_eff = resolve_settings(Some(&cfg), None, &list_cli.to_settings()).unwrap();
        let list_opts = build_driver_options(&list_cli, &list_eff, true).unwrap();
        assert!(list_opts.list);
        assert_eq!(list_opts.baseline, None);
        assert!(!list_opts.checkpoint);
    }

    #[test]
    fn report_exit_returns_two_on_audit_discrepancies() {
        let mut rep = report::Report {
            package: "demo".to_string(),
            ..Default::default()
        };
        assert_eq!(report_exit(&rep), 0);
        rep.audit = Some(report::Audit {
            sampled: 1,
            survivors: 1,
            not_audited: 0,
            disagreements: vec![],
            rebuilt: 0,
            compared: 0,
            not_compared: 0,
            source_disagreements: vec![],
            trace_differences: vec![report::TraceDifference {
                binary: "demo".to_string(),
                records: 0,
                rerun_failed: true,
            }],
        });
        assert_eq!(report_exit(&rep), 2);
        rep.timeout_rechecks.unconfirmed = 1;
        assert_eq!(report_exit(&rep), 3);
    }

    #[test]
    fn report_exit_pict_table() {
        let table = include_str!("../tests/data/report_evaluation.pict.tsv");
        let mut lines = table.lines();
        let _header = lines.next().expect("header");
        let dummy = || report::Location {
            file: "src/lib.rs".into(),
            line: 1,
            column: 1,
            code: "x".into(),
            ..report::Location::default()
        };
        for line in lines.filter(|line| !line.trim().is_empty()) {
            let cols: Vec<&str> = line.split('\t').collect();
            assert_eq!(cols.len(), 11, "row: {line}");
            let skipped = if cols[4] == "true" {
                vec![report::SkippedSite {
                    at: dummy(),
                    end_line: 1,
                    end_column: 2,
                    reason: report::SkipReason::UnparseableSource,
                    diagnostic: Some("parse error".into()),
                }]
            } else if cols[2] == "nonempty" {
                vec![report::SkippedSite {
                    at: dummy(),
                    end_line: 1,
                    end_column: 2,
                    reason: report::SkipReason::RequiresSourceRebuild,
                    diagnostic: None,
                }]
            } else {
                vec![]
            };
            let source_only_timeout = cols[8] == "true";
            let mut rep = report::Report {
                sites: usize::from(cols[0] == "nonzero"),
                mutants_run: usize::from(cols[1] == "nonzero"),
                mutants_killed: usize::from(cols[1] == "nonzero"),
                mutants_timed_out: usize::from(source_only_timeout),
                skipped,
                skipped_sites: usize::from(cols[3] == "nonzero"),
                mutants_not_built: usize::from(cols[5] == "nonzero"),
                mutants_not_evaluated: usize::from(cols[6] == "nonzero"),
                timeout_rechecks: report::TimeoutRechecks {
                    first_pass: usize::from(cols[7] == "nonzero"),
                    unconfirmed: usize::from(cols[7] == "nonzero"),
                    ..report::TimeoutRechecks::default()
                },
                unstable: if cols[9] == "true" {
                    vec![dummy()]
                } else {
                    vec![]
                },
                analysis: report::Analysis {
                    source_only: cols[10] == "true",
                    ..report::Analysis::default()
                },
                ..report::Report::default()
            };
            rep.evaluation = match cols[1] {
                "zero" => {
                    if cols[0] == "zero"
                        && cols[2] == "empty"
                        && cols[5] == "zero"
                        && cols[6] == "zero"
                    {
                        Some(report::Evaluation::NoSites)
                    } else {
                        Some(report::Evaluation::NoMutantsExecuted)
                    }
                }
                "nonzero" => {
                    if cols[3] == "nonzero"
                        || cols[4] == "true"
                        || cols[5] == "nonzero"
                        || cols[6] == "nonzero"
                        || cols[7] == "nonzero"
                        || source_only_timeout
                    {
                        Some(report::Evaluation::Partial)
                    } else {
                        None
                    }
                }
                other => panic!("unexpected mutants_run value: {other}"),
            };
            let expected = if cols[7] == "nonzero"
                || (cols[10] == "true" && (source_only_timeout || cols[6] == "nonzero"))
            {
                3
            } else if cols[9] == "true" {
                2
            } else if rep.evaluation == Some(report::Evaluation::NoMutantsExecuted) {
                3
            } else {
                0
            };
            assert_eq!(report_exit(&rep), expected, "row: {line}");
        }
    }
}
