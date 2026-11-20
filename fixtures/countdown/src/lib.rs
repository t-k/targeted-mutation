//! Loops that come closer to a bound in every round: the nearest value is the last one
//! seen, on the thread of the test and on a thread the test starts.

pub fn below(i: u32, limit: u32) -> bool {
    i < limit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_towards_the_limit() {
        for i in 0..40 {
            assert!(below(i, 100));
        }
    }

    #[test]
    fn a_helper_counts_further() {
        std::thread::Builder::new()
            .name("helper".to_string())
            .spawn(|| {
                for i in 0..90 {
                    assert!(below(i, 100));
                }
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn comes_down_from_above() {
        for i in (150..300).rev() {
            assert!(!below(i, 100));
        }
    }
}
