//! Code behind features: what is analysed is what the chosen features compile.

#[cfg(feature = "basic")]
pub fn basic(n: u32) -> bool {
    n > 5
}

#[cfg(feature = "extra")]
pub fn extra(n: u32) -> bool {
    n > 10
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "basic")]
    #[test]
    fn basic_at_its_limit() {
        assert!(!super::basic(5));
        assert!(super::basic(6));
    }

    #[cfg(feature = "extra")]
    #[test]
    fn extra_far_from_its_limit() {
        assert!(super::extra(50));
    }
}
