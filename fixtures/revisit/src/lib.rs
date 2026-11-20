//! Mutants that have to be run as what they are, and code a probe must not change.

/// Twice through one comparison: what the second time sees depends on the first. In
/// place of `<`, `>=` and `>` answer alike for the 2 the test starts with, and only one
/// of them fails the test.
pub fn settle(start: i32) -> i32 {
    let mut x = start;
    for _ in 0..2 {
        if x < 1 {
            x -= 1
        } else {
            x = 3
        }
    }
    x
}

/// Up to the last value of the type: `254..255` is one number, `254..=255` are two.
pub fn top_count(from: u8, to: u8) -> usize {
    let mut n = 0;
    for _ in from..to {
        n += 1;
    }
    n
}

/// A type with a `max` of its own, which is not the one of `Ord`.
#[derive(PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Debug)]
pub struct Level(pub u8);

impl Level {
    /// The higher of the two, and never above nine.
    pub fn max(self, other: Level) -> Level {
        let higher = if self.0 >= other.0 { self } else { other };
        if higher.0 > 9 {
            Level(9)
        } else {
            higher
        }
    }
}

pub fn capped(a: Level, b: Level) -> Level {
    a.max(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settles() {
        assert_eq!(settle(2), 3);
    }

    #[test]
    fn counts_up_to_the_top() {
        assert_eq!(top_count(254, u8::MAX), 1);
    }

    #[test]
    fn levels_are_capped() {
        assert_eq!(capped(Level(20), Level(3)), Level(9));
        assert_eq!(capped(Level(2), Level(3)), Level(3));
        assert_eq!(capped(Level(3), Level(3)), Level(3));
        assert_eq!(capped(Level(4), Level(3)), Level(4));
        assert_eq!(capped(Level(3), Level(10)), Level(9));
        assert_eq!(capped(Level(9), Level(1)), Level(9));
    }
}
