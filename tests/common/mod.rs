use std::ffi::OsStr;
use std::process::Command;

/// The outer Cargo build may have custom output paths. Fixture subprocesses
/// use their own Cargo defaults unless a test explicitly sets an override.
pub fn fixture_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    for name in [
        "CARGO_TARGET_DIR",
        "CARGO_BUILD_TARGET_DIR",
        "CARGO_BUILD_BUILD_DIR",
        "TMT_MUTANT",
        "TMT_TRACE",
    ] {
        command.env_remove(name);
    }
    command
}
