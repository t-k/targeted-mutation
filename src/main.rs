use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use targeted_mutation::{driver, report};

/// Targeted mutation testing: finds the wrong implementations your tests would let
/// through, and says which input or assertion would catch them.
#[derive(Parser)]
#[command(name = "tmt", version)]
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
    /// Also list the code that no test executes.
    #[arg(long)]
    show_unreached: bool,
    /// No progress output on stderr.
    #[arg(short, long)]
    quiet: bool,
}

/// Exit codes: 0 no gaps, 2 gaps found, 1 the analysis could not run.
fn main() -> ExitCode {
    let cli = Cli::parse();
    let options = driver::Options {
        dir: cli.dir,
        package: cli.package,
        verify: !cli.no_verify,
        work_dir: cli.work_dir,
        progress: !cli.quiet,
        doc: cli.doc,
    };
    match driver::run(&options) {
        Ok(result) => {
            if cli.json {
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
