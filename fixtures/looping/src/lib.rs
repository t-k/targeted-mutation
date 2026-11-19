//! Deleting the increment makes the loop endless: the mutant has to be stopped by the timeout.

pub fn count_up(n: u32) -> u32 {
    let mut i = 0;
    while i != n {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts() {
        assert_eq!(count_up(0), 0);
        assert_eq!(count_up(3), 3);
    }
}
