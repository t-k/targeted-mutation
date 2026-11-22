//! Code that also runs in the child of a `fork`, before its `exec`.

use std::io;
use std::os::unix::process::CommandExt;
use std::process::Command;

/// The highest level a command may be started with.
pub const HIGHEST: u8 = 3;

/// Starts `true` unless the child finds the level too high.
pub fn start(level: u8) -> io::Result<bool> {
    let mut command = Command::new("true");
    // SAFETY: the hook compares two integers and allocates nothing when it passes.
    unsafe {
        command.pre_exec(move || {
            if level > HIGHEST {
                return Err(io::Error::from(io::ErrorKind::PermissionDenied));
            }
            Ok(())
        });
    }
    Ok(command.status()?.success())
}
