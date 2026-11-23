pub fn cap(n: u32) -> u32 {
    n.min(10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_values_pass() {
        assert_eq!(cap(3), 3);
    }
}
