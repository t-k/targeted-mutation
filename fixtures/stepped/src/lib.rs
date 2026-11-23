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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_to_the_limits() {
        assert!(!is_wide(2));
        assert!(is_wide(3));
        assert!(!is_marked(0b0100));
        assert!(is_marked(0b1000));
    }

    #[test]
    fn far_from_the_limit() {
        assert!(!is_long(1));
        assert!(is_long(5));
    }
}
