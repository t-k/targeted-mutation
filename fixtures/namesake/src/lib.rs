//! A test that has a thread do its work, and gives that thread the name of another test.
//! What the thread sees is then taken for what the other test saw, and a mutant is run
//! against the wrong test. Only running it against all tests shows that.

pub fn above_five(x: u32) -> bool {
    x > 5
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn other() {}

    #[test]
    fn outer() {
        let seen = std::thread::Builder::new()
            .name("tests::other".to_string())
            .spawn(|| above_five(6))
            .unwrap()
            .join()
            .unwrap();
        assert!(seen);
    }

    #[test]
    fn third() {
        assert!(!above_five(1));
    }
}
