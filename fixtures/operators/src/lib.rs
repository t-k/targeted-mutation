//! Boolean and arithmetic operators, and code inside macro arguments.

use std::time::Duration;

/// Tested with an owner and with nobody: the admin never decides.
pub fn can_edit(is_owner: bool, is_admin: bool) -> bool {
    is_owner || is_admin
}

/// Fully tested.
pub fn in_range(x: i32, lo: i32, hi: i32) -> bool {
    lo <= x && x <= hi
}

/// Only tested with a fee of zero.
pub fn total_with_fee(amount: u64, fee: u64) -> u64 {
    amount + fee
}

/// Tested with 3 x 4.
pub fn area(width: u32, height: u32) -> u32 {
    width * height
}

/// Only tested with a factor of one.
pub fn scale(value: f64, factor: f64) -> f64 {
    value * factor
}

/// `String + &str` and `Duration + Duration` are not numbers: left alone.
pub fn label(name: &str, index: usize) -> String {
    format!("{}-{}", name.to_owned() + "!", index + 1)
}

pub fn total_time(a: Duration, b: Duration) -> Duration {
    a + b
}

/// Comparisons inside `vec!`.
pub fn flags(n: i32) -> Vec<bool> {
    vec![n > 0, n % 2 == 0]
}

/// A let chain is not a boolean operator.
pub fn first_positive(values: &[i32]) -> Option<i32> {
    for value in values {
        if let Some(v) = Some(*value)
            && v > 0
        {
            return Some(v);
        }
    }
    None
}

/// `?` in the right operand of `&&`.
pub fn discounted(price: Option<u32>) -> Result<u32, String> {
    let price = price.ok_or("no price")?;
    Ok(if price > 3 && eligible(price)? { price - 1 } else { price })
}

fn eligible(price: u32) -> Result<bool, String> {
    if price > 1000 { Err("too expensive".into()) } else { Ok(price % 2 == 0) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn editing() {
        assert!(can_edit(true, false));
        assert!(!can_edit(false, false));
    }

    #[test]
    fn ranges() {
        assert!(!in_range(0, 1, 3));
        assert!(in_range(1, 1, 3));
        assert!(in_range(3, 1, 3));
        assert!(!in_range(4, 1, 3));
    }

    #[test]
    fn money_and_geometry() {
        assert_eq!(total_with_fee(100, 0), 100);
        assert_eq!(area(3, 4), 12);
        assert_eq!(scale(2.5, 1.0), 2.5);
    }

    #[test]
    fn text_and_time() {
        assert_eq!(label("a", 1), "a!-2");
        assert_eq!(total_time(Duration::from_secs(1), Duration::from_secs(2)), Duration::from_secs(3));
    }

    #[test]
    fn flags_and_chains() {
        assert_eq!(flags(2), [true, true]);
        assert_eq!(flags(1), [true, false]);
        assert_eq!(flags(0), [false, true]);
        assert_eq!(flags(-1), [false, false]);
        assert_eq!(first_positive(&[-1, 0, 1, 5]), Some(1));
        assert_eq!(first_positive(&[-1]), None);
    }

    #[test]
    fn discounts() {
        assert_eq!(discounted(None), Err("no price".into()));
        assert_eq!(discounted(Some(3)), Ok(3));
        assert_eq!(discounted(Some(4)), Ok(3));
        assert_eq!(discounted(Some(5)), Ok(5));
        assert_eq!(discounted(Some(1001)), Err("too expensive".into()));
    }
}
