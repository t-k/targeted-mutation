pub fn label(n: u32) -> u32 {
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels_are_kept() {
        assert_eq!(label(3), 3);
    }
}
