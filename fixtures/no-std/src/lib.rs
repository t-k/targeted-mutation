//! A `no_std` library. Its tests link std, so the probes work.
#![no_std]

pub fn saturating_level(raw: u16) -> u8 {
    if raw > 255 {
        return 255;
    }
    raw as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels() {
        assert_eq!(saturating_level(7), 7);
        assert_eq!(saturating_level(1000), 255);
    }
}
