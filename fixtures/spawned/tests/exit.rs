//! A test that does not fail but ends the test program, with the status the runtime
//! happens to use for a run it gives up. That is a mutant found out all the same.

#[test]
fn doubles() {
    if spawned::double(3) != 6 {
        std::process::exit(86);
    }
}
