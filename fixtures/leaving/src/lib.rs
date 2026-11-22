//! A crate whose test starts a helper and stops it only when its assertion holds.

/// Whether `n` is within the limit.
pub fn within(n: u8) -> bool {
    n <= 3
}
