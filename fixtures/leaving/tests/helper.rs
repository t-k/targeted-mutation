use std::io::Write;
use std::os::unix::process::CommandExt;
use std::process::Command;

/// The helper is in a process group of its own, as a server started by a test often is.
/// A failed assertion ends the test before the helper is stopped.
#[test]
fn checks_the_limit_with_a_helper_running() {
    let mut helper = Command::new("sleep").arg("120").process_group(0).spawn().unwrap();
    if let Some(list) = std::env::var_os("LEAVING_PIDS") {
        let mut list = std::fs::OpenOptions::new().create(true).append(true).open(list).unwrap();
        writeln!(list, "{}", helper.id()).unwrap();
    }
    assert!(leaving::within(3));
    assert!(!leaving::within(4));
    helper.kill().unwrap();
    helper.wait().unwrap();
}
