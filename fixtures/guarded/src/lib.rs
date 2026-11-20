//! A guard that does something. A probe that takes the guard for `true` still evaluates
//! it; the guard replaced by `true` in the source does not. The two are not the same
//! mutant, and only building the second shows it.

use std::cell::Cell;

pub fn classify(n: u32, asked: &Cell<u32>) -> &'static str {
    match n {
        0 => "none",
        _ if note(asked) => "some",
        _ => "never",
    }
}

fn note(asked: &Cell<u32>) -> bool {
    asked.set(asked.get() + 1);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guard_is_asked_once() {
        let asked = Cell::new(0);
        assert_eq!(classify(3, &asked), "some");
        assert_eq!(asked.get(), 1);
        assert_eq!(classify(0, &asked), "none");
    }
}
