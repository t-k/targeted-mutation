pub fn is_large(size: u64) -> bool {
    size > 1000
}

#[cfg(test)]
mod tests {
    #[test]
    fn sizes() {
        assert!(!super::is_large(10));
        assert!(super::is_large(5000));
    }
}
