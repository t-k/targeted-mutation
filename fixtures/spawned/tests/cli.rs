//! Only asks whether the program ends well, not what it prints.

#[test]
fn the_program_ends_well() {
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_spawned"))
        .status()
        .unwrap();
    assert!(status.success());
}
