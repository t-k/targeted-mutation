use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use targeted_mutation::{driver, report};

/// Targeted mutation testing: finds the wrong implementations your tests would let
/// through, and says which input or assertion would catch them.
#[derive(Parser)]
#[command(name = "cargo-tmt", bin_name = "cargo tmt", version)]
struct Cli {
    /// Directory of the cargo package to analyse.
    #[arg(default_value = ".")]
    dir: PathBuf,
    /// Package to analyse, when the directory is a workspace.
    #[arg(short, long)]
    package: Option<String>,
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
    /// Also run the mutants that need a build of their own (code in a `const fn` and
    /// other places a probe cannot be put in). Slow: one build and one run of all tests
    /// for each of them.
    #[arg(long)]
    rebuild: bool,
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

/// Exit codes: 0 no gaps, 2 gaps found, 1 the analysis could not run.
fn main() -> ExitCode {
    let cli = Cli::parse_from(arguments(std::env::args_os()));
    let options = driver::Options {
        dir: cli.dir,
        package: cli.package,
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
        doc: cli.doc,
    };
    match driver::run(&options) {
        Ok(result) => {
            if cli.sarif {
                println!("{}", report::render_sarif(&result));
            } else if cli.json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&result).expect("report serialises")
                );
            } else {
                print!("{}", report::render_text(&result, cli.show_unreached));
            }
            if result.has_gaps() {
                ExitCode::from(2)
            } else {
                ExitCode::SUCCESS
            }
        }
        Err(err) => {
            eprintln!("tmt: error: {err:#}");
            ExitCode::FAILURE
        }
    }
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
