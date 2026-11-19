//! Ranges, `min` / `max`, `match` arms and `Option` results, with thin tests.

/// Tested only with a slice whose next element is zero, so one element too many adds nothing.
pub fn sum_first(values: &[u32], n: usize) -> u32 {
    let mut total = 0;
    for i in 0..n {
        total += values[i];
    }
    total
}

/// Tested only with a level equal to the cap.
pub fn clamp_level(level: u32, cap: u32) -> u32 {
    level.min(cap)
}

/// Tested with a level above and below the cap.
pub fn at_least(level: u32, floor: u32) -> u32 {
    level.max(floor)
}

/// Tested only with slices that contain no even number.
pub fn first_even(values: &[u32]) -> Option<u32> {
    values.iter().copied().find(|v| v % 2 == 0)
}

/// Tested with a hit and a miss.
pub fn position_of(values: &[u32], wanted: u32) -> Option<usize> {
    values.iter().position(|v| *v == wanted)
}

/// The test only checks that a small number is not "none".
pub fn bucket(n: u32) -> &'static str {
    match n {
        0 => "none",
        few if few < 10 => "few",
        _ => "many",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums() {
        assert_eq!(sum_first(&[1, 2, 0], 2), 3);
    }

    #[test]
    fn levels() {
        assert_eq!(clamp_level(5, 5), 5);
        assert_eq!(at_least(1, 3), 3);
        assert_eq!(at_least(7, 3), 7);
    }

    #[test]
    fn searching() {
        assert_eq!(first_even(&[1, 3]), None);
        assert_eq!(position_of(&[4, 5], 5), Some(1));
        assert_eq!(position_of(&[4, 5], 6), None);
    }

    #[test]
    fn buckets() {
        assert_eq!(bucket(0), "none");
        assert_ne!(bucket(3), "none");
        assert_eq!(bucket(50), "many");
    }
}
