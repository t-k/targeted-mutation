//! A library whose only tests are doc tests.

/// ```
/// assert_eq!(doc_tested::double(2), 4);
/// ```
pub fn double(x: u32) -> u32 {
    x * 2
}

/// ```
/// assert!(doc_tested::is_even(4));
/// ```
pub fn is_even(x: u32) -> bool {
    x % 2 == 0
}
