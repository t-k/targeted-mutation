//! The age check from the README: the tests pass 17 and 20, so the boundary can move.

pub fn is_eligible(age: u32) -> bool {
    age >= 18
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minor_is_rejected() {
        assert!(!is_eligible(17));
    }

    #[test]
    fn adult_is_accepted() {
        assert!(is_eligible(20));
    }
}
