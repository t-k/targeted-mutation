//! Code that is about to change: the tests of `tests/e2e.rs` change it and ask whether
//! the tests here tell the new code from the old.

pub mod limits;
pub mod steps;

/// Whether a session is still good.
pub fn valid(created: u64, used: u64, now: u64) -> bool {
    let _ = used;
    now - created < 30
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_and_stale() {
        assert!(valid(0, 0, 10));
        assert!(valid(90, 90, 100));
        assert!(!valid(0, 0, 40));
    }
}
