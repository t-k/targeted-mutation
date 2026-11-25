use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use targeted_mutation::{driver, report};

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
    #[arg(long, requires = "source_only", help_heading = HEADING_MUTATION)]
    literal_operators: bool,

    /// Select zero-based source-only shard K/N (`--source-only`).
    #[arg(
        long,
        requires = "source_only",
        value_name = "K/N",
        help_heading = HEADING_MUTATION
    )]
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
        default_value = "cargo",
        help_heading = HEADING_TESTS
    )]
    test_tool: TestTool,

    /// Nextest profile for `--test-tool nextest`.
    #[arg(long, value_name = "PROFILE", help_heading = HEADING_TESTS)]
    nextest_profile: Option<String>,

    /// Run each selected libtest test in its own process.
    #[arg(long, help_heading = HEADING_TESTS)]
    isolate_tests: bool,

    /// Cargo test targets to run and build when possible (repeatable).
    #[arg(long, value_name = "TARGET", help_heading = HEADING_TESTS)]
    test_target: Vec<String>,

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

    /// Grace period for `--test-wrapper` on Unix timeout (0 kills immediately).
    #[arg(
        long,
        value_name = "SECONDS",
        default_value_t = 2,
        requires = "test_wrapper",
        help_heading = HEADING_TESTS
    )]
    wrapper_grace: u64,

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
    /// Mutants to run in parallel.
    ///
    /// Raise this only when tests can run concurrently with themselves (no fixed ports
    /// or shared files).
    #[arg(
        short,
        long,
        default_value_t = 1,
        value_name = "N",
        value_parser = clap::value_parser!(u32).range(1..),
        help_heading = HEADING_BUILD
    )]
    jobs: u32,

    /// Concurrent isolated baseline tests.
    #[arg(
        long,
        value_name = "N",
        default_value_t = 1,
        value_parser = clap::value_parser!(u32).range(1..),
        help_heading = HEADING_BUILD
    )]
    baseline_jobs: u32,

    /// Timeout per test runner invocation under a mutant, in seconds.
    #[arg(
        long,
        value_name = "SECONDS",
        value_parser = clap::value_parser!(u64).range(1..),
        help_heading = HEADING_BUILD
    )]
    timeout: Option<u64>,

    /// Policy for rechecking parallel worker timeouts.
    #[arg(
        long,
        value_enum,
        value_name = "POLICY",
        default_value = "serial",
        help_heading = HEADING_BUILD
    )]
    timeout_recheck: TimeoutRecheck,

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
    #[arg(
        long,
        value_name = "PATH",
        requires = "copy_fixture",
        help_heading = HEADING_WORK
    )]
    copy_fixture_exclude: Vec<PathBuf>,

    /// Stage workspace path FROM at TO relative to isolated Cargo output (repeatable).
    #[arg(long, value_name = "FROM=TO", help_heading = HEADING_WORK)]
    copy_output_fixture: Vec<targeted_mutation::output_fixture::Fixture>,
}

impl Cli {
    fn output_format(&self) -> OutputFormat {
        if let Some(format) = self.format {
            format
        } else if self.json {
            OutputFormat::Json
        } else if self.jsonl {
            OutputFormat::Jsonl
        } else if self.sarif {
            OutputFormat::Sarif
        } else {
            OutputFormat::Text
        }
    }

    fn validate(&self) -> Result<(), clap::Error> {
        use clap::{CommandFactory, error::ErrorKind};

        if !matches!(self.test_tool, TestTool::Nextest) && self.nextest_profile.is_some() {
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
    let output_format = cli.output_format();
    let nextest = matches!(cli.test_tool, TestTool::Nextest);
    let options = driver::Options {
        dir: cli.directory.unwrap_or(cli.dir),
        package: cli.package.first().cloned(),
        list: cli.list,
        test_workspace: cli.test_workspace.unwrap_or(false),
        verify: !cli.no_verify,
        work_dir: cli.work_dir,
        progress: !cli.quiet,
        since: cli.since,
        lines: cli.lines,
        baseline: cli.baseline,
        write_baseline: cli.write_baseline,
        also_test: cli.also_test,
        dependents: cli.dependents,
        effects: !cli.no_effects,
        rebuild: cli.rebuild,
        error_values: cli.error_values,
        source_only: cli.source_only,
        literal_operators: cli.literal_operators,
        shard: cli.shard.map(|mut shard| {
            shard.round_robin = matches!(cli.sharding, Some(Sharding::RoundRobin));
            shard
        }),
        nextest: nextest.then_some(report::Nextest {
            profile: cli.nextest_profile,
        }),
        test_selection: driver::TestSelection {
            isolate: cli.isolate_tests,
            targets: cli.test_target,
            include: cli.include_test,
            exclude: cli.exclude_test,
            include_ignored: cli.include_ignored,
            wrapper: cli.test_wrapper,
        },
        opt_level: cli.opt_level,
        timeout_seconds: cli.timeout,
        baseline_jobs: cli.baseline_jobs as usize,
        serial_timeout_rechecks: matches!(cli.timeout_recheck, TimeoutRecheck::Serial),
        wrapper_grace: std::time::Duration::from_secs(cli.wrapper_grace),
        audit: cli.audit,
        cargo: driver::CargoConfig {
            features: cli.features,
            all_features: cli.all_features,
            no_default_features: cli.no_default_features,
            locked: cli.locked,
        },
        jobs: cli.jobs as usize,
        all_files: cli.all_files,
        git_index: cli.git_index,
        copy_fixtures: cli.copy_fixture,
        copy_fixture_excludes: cli.copy_fixture_exclude,
        output_fixtures: cli.copy_output_fixture,
        checkpoint: cli.checkpoint,
        doc: cli.doc,
    };
    if output_format == OutputFormat::Jsonl {
        return stream_reports(
            &options,
            &cli.package,
            cli.workspace || cli.package.len() > 1,
        );
    }
    if cli.workspace || cli.package.len() > 1 {
        let members = match driver::run_packages(&options, &cli.package) {
            Ok(members) => members,
            Err(err) => {
                eprintln!(
                    "tmt: error: {}",
                    report::sanitize_terminal(&format!("{err:#}"))
                );
                return ExitCode::FAILURE;
            }
        };
        return show_workspace(members, output_format, cli.show_unreached);
    }
    match driver::run(&options) {
        Ok(result) => {
            show_report(&result, output_format, cli.show_unreached);
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

fn stream_reports(options: &driver::Options, packages: &[String], workspace: bool) -> ExitCode {
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
        driver::run_packages_each(options, packages, &mut emit)
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
        assert_eq!((cli.dir, cli.jobs), (PathBuf::from("crate"), 3));
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
            &["--nextest-profile", "ci"][..],
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
        assert!(long_help.contains("Examples:"));
        assert!(long_help.contains("Exit codes:"));
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
