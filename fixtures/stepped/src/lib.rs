//! Comparisons of values that cannot be every number: a count of blocks times eight,
//! the bits of a mask. A boundary value that no input gives is not a test to ask for.

/// Tested with 2 and 3 blocks: 16 and 24, and nothing a block count gives lies between.
pub fn is_wide(blocks: u32) -> bool {
    blocks * 8 > 20
}

/// Tested with 1 and 5 blocks: 8 and 40, with 16 and 24 between them.
pub fn is_long(blocks: u32) -> bool {
    blocks * 8 > 20
}

/// Tested with the bits 0100 and 1000: no other bits of the mask lie between.
pub fn is_marked(flags: u32) -> bool {
    flags & 0b1100 >= 0b0101
}

/// The last digit is never ten: there is no other outcome to test.
pub fn has_a_last_digit(n: u32) -> bool {
    n % 10 < 10
}

/// A rate in eighths, with a multiplication of its own: times eight it is any number.
pub struct Eighths(pub u32);

impl std::ops::Mul<u32> for Eighths {
    type Output = u32;

    fn mul(self, by: u32) -> u32 {
        self.0 + by - 8
    }
}

/// Tested with 16 and 24: here every number between them can be, and 20 and 21 are
/// the ones to test.
pub fn is_rated(rate: Eighths) -> bool {
    rate * 8 > 20
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_to_the_limits() {
        assert!(!is_wide(2));
        assert!(is_wide(3));
        assert!(!is_marked(0b0100));
        assert!(is_marked(0b1000));
        assert!(has_a_last_digit(7));
        assert!(!is_rated(Eighths(16)));
        assert!(is_rated(Eighths(24)));
    }

    #[test]
    fn far_from_the_limit() {
        assert!(!is_long(1));
        assert!(is_long(5));
    }
}
