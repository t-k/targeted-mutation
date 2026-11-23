//! A fee in basis points. The tests only pass round amounts, so code that divides
//! before it multiplies (and so rounds too early) passes them too.

pub fn fee(cents: u32, bps: u32) -> u64 {
    u64::from(cents) * u64::from(bps) / 10_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_amounts() {
        assert_eq!(fee(10_000, 125), 125);
        assert_eq!(fee(20_000, 250), 500);
    }
}
