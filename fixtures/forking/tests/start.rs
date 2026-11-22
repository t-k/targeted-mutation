use forking::{start, HIGHEST};

#[test]
fn starts_up_to_the_highest_level() {
    assert!(start(HIGHEST).unwrap());
}

#[test]
fn refuses_a_level_above_the_highest() {
    assert!(start(HIGHEST + 1).is_err());
}
