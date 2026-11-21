use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use targeted_mutation::{driver, report};

#[derive(Clone, Copy, clap::ValueEnum)]
enum Sharding {
    Slice,
    RoundRobin,
}

#[derive(Clone, Copy, Default, clap::ValueEnum)]
enum TestTool {
    #[default]
    Cargo,
    Nextest,
}

/// Targeted mutation testing: finds the wrong implementations your tests would let
/// through, and says which input or assertion would catch them.
#[derive(Parser)]
#[command(name = "cargo-tmt", bin_name = "cargo tmt", version)]
struct Cli {
    /// Directory of the cargo package to analyse.
    #[arg(default_value = ".")]
    dir: PathBuf,
    /// Directory of the Cargo package or workspace to analyse.
    #[arg(short = 'd', long = "dir", conflicts_with = "dir")]
    directory: Option<PathBuf>,
    /// Package to analyse, when the directory is a workspace (repeatable).
    #[arg(short, long, value_delimiter = ',', conflicts_with = "workspace")]
    package: Vec<String>,
    /// Analyse all workspace members.
    #[arg(long)]
    workspace: bool,
    /// List source candidates and their edits without building or running tests.
    #[arg(long, conflicts_with_all = ["sarif", "baseline", "write_baseline"])]
    list: bool,
    /// Run the whole workspace's tests against every mutation.
    #[arg(long, num_args = 0..=1, require_equals = true, default_missing_value = "true")]
    test_workspace: Option<bool>,
    /// Print the report as JSON.
    #[arg(long)]
    json: bool,
    /// Trust the trace: do not re-run tests against predicted survivors.
    #[arg(long)]
    no_verify: bool,
    /// Directory for the working copy and its build (default: <target dir>/tmt).
    #[arg(long, value_name = "DIR")]
    work_dir: Option<PathBuf>,
    /// Also run the doc tests. They are recompiled for every mutant they reach.
    #[arg(long)]
    doc: bool,
    /// Copy files that git ignores as well (needed when the build reads generated,
    /// untracked files). By default they are left out of the working copy.
    #[arg(long)]
    all_files: bool,
    /// Reproduce the source Git index in an independent repository in the work copy.
    #[arg(long)]
    git_index: bool,
    /// Copy a workspace-relative file or directory even when Git ignores it (repeatable).
    #[arg(long, value_name = "PATH")]
    copy_fixture: Vec<PathBuf>,
    /// Mutants to run at the same time. Only raise it if the tests can run concurrently
    /// with themselves (no fixed ports, no shared files).
    #[arg(short, long, default_value_t = 1, value_name = "N")]
    jobs: usize,
    /// Analyse only the code changed since this git revision, and untracked files.
    #[arg(long, value_name = "REV")]
    since: Option<String>,
    /// Analyse only these lines: FILE:FROM-TO, FILE:LINE or FILE (relative to the package).
    #[arg(long, value_name = "FILE:FROM-TO")]
    lines: Vec<String>,
    /// Do not report the findings listed in this file.
    #[arg(long, value_name = "FILE")]
    baseline: Option<PathBuf>,
    /// Write this run's findings to a file that later runs can use as --baseline.
    #[arg(long, value_name = "FILE")]
    write_baseline: Option<PathBuf>,
    /// Also run the tests of this workspace package (repeatable).
    #[arg(long, value_name = "PACKAGE")]
    also_test: Vec<String>,
    /// Also run the tests of every workspace package that depends on the analysed one.
    #[arg(long)]
    dependents: bool,
    /// Do not trace surviving mutants to say what they change.
    #[arg(long)]
    no_effects: bool,
    /// Also run mutants that need a build of their own and verify distinguishing inputs
    /// for supported survivors. Slower than probes alone.
    #[arg(long)]
    rebuild: bool,
    /// Also replace Result returns with Err(EXPR), using this Rust expression (repeatable).
    #[arg(long = "error", value_name = "EXPR", allow_hyphen_values = true)]
    error_values: Vec<String>,
    /// Build every supported mutant in the selected source instead of using probes.
    #[arg(long, conflicts_with_all = ["rebuild", "audit", "no_verify"])]
    source_only: bool,
    /// Replace just the operator token, using Rust precedence for the new spelling.
    #[arg(long, requires = "source_only")]
    literal_operators: bool,
    /// Select a zero-based source-only shard K/N.
    #[arg(long, requires = "source_only", value_name = "K/N")]
    shard: Option<report::Shard>,
    /// Method for distributing candidates across shards (default: slice).
    #[arg(long, requires = "shard", value_enum)]
    sharding: Option<Sharding>,
    /// Select the runner (nextest requires --source-only).
    #[arg(long, value_enum, default_value = "cargo")]
    test_tool: TestTool,
    /// Nextest profile to use with --test-tool nextest.
    #[arg(long, value_name = "PROFILE")]
    nextest_profile: Option<String>,
    /// Run one selected libtest test per process, including its child threads.
    #[arg(long)]
    isolate_tests: bool,
    /// Select these Cargo test artifact names for running and, where possible, for building (repeatable).
    #[arg(long)]
    test_target: Vec<String>,
    /// Include these exact libtest names (repeatable).
    #[arg(long)]
    include_test: Vec<String>,
    /// Exclude these exact libtest names (repeatable).
    #[arg(long)]
    exclude_test: Vec<String>,
    /// Also allow tests marked #[ignore].
    #[arg(long)]
    include_ignored: bool,
    /// Executable in the workspace copy to invoke with the test binary and its arguments.
    #[arg(long, value_name = "PATH")]
    test_wrapper: Option<PathBuf>,
    /// Build the tests at this optimisation level (0 to 3) instead of the profile's own.
    /// The probes cost next to nothing in optimised code, which pays off when tests
    /// spend their time in loops; the build takes longer, and the tests no longer run
    /// exactly as `cargo test` builds them.
    #[arg(long, value_name = "N", value_parser = clap::value_parser!(u8).range(0..=3))]
    opt_level: Option<u8>,
    /// Features of the package to build with (as for `cargo test`; repeatable).
    #[arg(long, short = 'F', value_name = "FEATURES", value_delimiter = ',')]
    features: Vec<String>,
    /// Build with all features.
    #[arg(long, conflicts_with_all = ["features", "no_default_features"])]
    all_features: bool,
    /// Build without the default features.
    #[arg(long)]
    no_default_features: bool,
    /// Do not let cargo change the lock file.
    #[arg(long)]
    locked: bool,
    /// Check the run itself: run up to N of the surviving mutants once more against
    /// every test (not only the ones chosen for them), and record the tests a second
    /// time to see whether they do the same.
    #[arg(long, value_name = "N", conflicts_with = "no_verify")]
    audit: Option<usize>,
    /// Print the report as SARIF 2.1.0.
    #[arg(long, conflicts_with = "json")]
    sarif: bool,
    /// Also list the code that no test executes.
    #[arg(long)]
    show_unreached: bool,
    /// No progress output on stderr.
    #[arg(short, long)]
    quiet: bool,
}

/// Exit codes: 0 no gaps, 1 analysis failed, 2 gaps found, 3 no mutations executed.
fn main() -> ExitCode {
    let cli = Cli::parse_from(arguments(std::env::args_os()));
    let nextest = matches!(cli.test_tool, TestTool::Nextest);
    if !nextest && cli.nextest_profile.is_some() {
        eprintln!("tmt: --nextest-profile requires --test-tool nextest");
        return ExitCode::FAILURE;
    }
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
        audit: cli.audit,
        cargo: driver::CargoConfig {
            features: cli.features,
            all_features: cli.all_features,
            no_default_features: cli.no_default_features,
            locked: cli.locked,
        },
        jobs: cli.jobs,
        all_files: cli.all_files,
        git_index: cli.git_index,
        copy_fixtures: cli.copy_fixture,
        doc: cli.doc,
    };
    if cli.workspace || cli.package.len() > 1 {
        let members = match driver::run_packages(&options, &cli.package) {
            Ok(members) => members,
            Err(err) => {
                eprintln!("tmt: error: {err:#}");
                return ExitCode::FAILURE;
            }
        };
        return show_workspace(members, cli.json, cli.sarif, cli.show_unreached);
    }
    match driver::run(&options) {
        Ok(result) => {
            show_report(&result, cli.json, cli.sarif, cli.show_unreached);
            ExitCode::from(report_exit(&result))
        }
        Err(err) => {
            eprintln!("tmt: error: {err:#}");
            ExitCode::FAILURE
        }
    }
}

fn report_exit(result: &report::Report) -> u8 {
    if result.has_gaps() {
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
            "package": result.package,
            "analysis": result.analysis,
            "mutants": mutants,
            "skipped": result.skipped,
        })
    } else {
        serde_json::to_value(result).expect("report serialises")
    }
}

fn show_report(result: &report::Report, json: bool, sarif: bool, show_unreached: bool) {
    if sarif {
        println!("{}", report::render_sarif(result));
    } else if json {
        if result.mutants.is_some() {
            println!("{:#}", report_json(result));
        } else {
            println!(
                "{}",
                serde_json::to_string_pretty(result).expect("report serialises")
            );
        }
    } else if let Some(mutants) = &result.mutants {
        println!("{}: {} source candidates", result.package, mutants.len());
        for candidate in mutants {
            let edit = &candidate.edits[0];
            println!(
                "{}:{}:{}: {} -> {}",
                edit.file, edit.line, edit.column, candidate.what, candidate.into
            );
        }
    } else {
        print!("{}", report::render_text(result, show_unreached));
    }
}

fn show_workspace(
    members: Vec<(String, anyhow::Result<report::Report>)>,
    json: bool,
    sarif: bool,
    show_unreached: bool,
) -> ExitCode {
    let mut reports = vec![];
    let mut errors = vec![];
    for (package, result) in members {
        match result {
            Ok(result) => reports.push(result),
            Err(err) => {
                let message = format!("{err:#}");
                eprintln!("tmt: {package}: {message}");
                errors.push(serde_json::json!({"package": package, "error": message}));
            }
        }
    }
    if json {
        let packages: Vec<_> = reports.iter().map(report_json).collect();
        println!(
            "{:#}",
            serde_json::json!({"packages": packages, "errors": errors})
        );
    } else if sarif {
        let mut combined: serde_json::Value =
            serde_json::from_str(&report::render_sarif(&report::Report::default()))
                .expect("SARIF serialises");
        let runs: Vec<_> = reports
            .iter()
            .map(|result| {
                let mut sarif: serde_json::Value =
                    serde_json::from_str(&report::render_sarif(result)).expect("SARIF serialises");
                sarif["runs"][0]["properties"]["package"] = result.package.clone().into();
                sarif["runs"][0].take()
            })
            .collect();
        combined["runs"] = serde_json::Value::Array(runs);
        println!("{combined:#}");
    } else {
        for result in &reports {
            show_report(result, false, false, show_unreached);
        }
    }
    ExitCode::from(if !errors.is_empty() {
        1
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
    }
}
