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

/// A range that reaches the last value of its type only once its end is taken in: the
/// first count is then 1, and the second range would be `255..=255`.
pub fn last_count() -> usize {
    let mut end = 0u8;
    let mut n = 0;
    for _ in 0..2 {
        n = 0;
        for _ in end..end {
            n += 1;
        }
        if n > 0 {
            end = u8::MAX;
        }
    }
    n
}

/// A range that is looked at, not run through: `0..end` and `0..=end` are not the same
/// thing here, whatever numbers they hold.
pub fn takes_its_end_in(end: u8) -> bool {
    use std::ops::{Bound, RangeBounds};
    let range = 0u8..end;
    matches!(range.end_bound(), Bound::Included(_))
}

/// A number that is not one has no order: `x >= 0.0` is false for it, and so is
/// `x < 0.0`, while "not `x >= 0.0`" is true.
pub fn not_negative(x: f64) -> bool {
    x >= 0.0
}

/// A type that cannot be copied, says when it is dropped, and has a `max` that only
/// looks at it.
pub struct Gauge<'a> {
    pub level: u32,
    pub dropped: &'a std::cell::Cell<bool>,
}

impl Drop for Gauge<'_> {
    fn drop(&mut self) {
        self.dropped.set(true);
    }
}

impl Gauge<'_> {
    pub fn max(&self, floor: u32) -> u32 {
        if self.level > floor {
            self.level
        } else {
            floor
        }
    }

    pub fn min(&self, ceiling: u32) -> u32 {
        if self.level < ceiling {
            self.level
        } else {
            ceiling
        }
    }
}

/// The reading, and whether the gauge was gone when it was taken (it is not: the gauge
/// lives to the end of the function).
pub fn reading(dropped: &std::cell::Cell<bool>) -> (u32, bool) {
    let gauge = Gauge { level: 3, dropped };
    let high = gauge.max(1);
    (high, dropped.get())
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
    fn nothing_is_counted_in_the_end() {
        assert_eq!(last_count(), 0);
    }

    #[test]
    fn the_gauge_outlives_its_reading() {
        let dropped = std::cell::Cell::new(false);
        assert_eq!(reading(&dropped), (3, false));
        assert!(dropped.get());
        assert_eq!(Gauge { level: 1, dropped: &dropped }.max(4), 4);
        assert_eq!(Gauge { level: 4, dropped: &dropped }.max(4), 4);
        assert_eq!(Gauge { level: 5, dropped: &dropped }.max(4), 5);
        assert_eq!(Gauge { level: 1, dropped: &dropped }.min(4), 1);
        assert_eq!(Gauge { level: 4, dropped: &dropped }.min(4), 4);
        assert_eq!(Gauge { level: 5, dropped: &dropped }.min(4), 4);
    }

    #[test]
    fn a_range_written_with_two_dots_leaves_its_end_out() {
        assert!(!takes_its_end_in(3));
    }

    #[test]
    fn what_is_not_a_number_is_not_not_negative() {
        assert!(!not_negative(f64::NAN));
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
