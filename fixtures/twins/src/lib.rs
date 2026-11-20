//! The same comparison in several places: each finding has to be known by where it is.

pub fn first(a: u32, b: u32) -> bool {
    a > b
}

pub fn second(a: u32, b: u32) -> bool {
    a > b
}

pub struct Pair;

impl Pair {
    pub fn both(a: u32, b: u32) -> u32 {
        let high = if a > b { 2 } else { 0 };
        let low = if a > b { 1 } else { 0 };
        high + low
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn far_apart() {
        assert!(first(9, 1));
        assert!(!first(1, 9));
        assert!(second(9, 1));
        assert!(!second(1, 9));
        assert_eq!(Pair::both(9, 1), 3);
        assert_eq!(Pair::both(1, 9), 0);
    }
}
