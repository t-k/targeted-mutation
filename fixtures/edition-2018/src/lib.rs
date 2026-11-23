//! A crate of the 2018 edition: what is put into it must build there too.

pub mod inner;

pub fn is_big(n: u32) -> bool {
    n >= 18
}

pub fn shifted(a: u8, b: u8) -> u8 {
    a << b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn far_from_the_limits() {
        assert!(is_big(20));
        assert!(!is_big(3));
        assert_eq!(shifted(1, 2), 4);
        assert_eq!(inner::sum(&[1, 2]), 3);
    }
}
