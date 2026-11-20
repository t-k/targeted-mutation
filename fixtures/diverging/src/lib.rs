//! A right operand that gives no value: it leaves the function. The expression is
//! executed all the same, and `||` in place of `&&` would not get as far as the error.

pub fn checked(flag: bool, n: i32) -> Result<bool, String> {
    Ok(flag && positive(n)?)
}

fn positive(n: i32) -> Result<bool, String> {
    if n < -100 {
        Err("too small".to_string())
    } else {
        Ok(n > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn too_small_is_an_error() {
        assert!(checked(true, -500).is_err());
    }
}
