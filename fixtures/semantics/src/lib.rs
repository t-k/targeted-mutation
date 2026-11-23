//! Code whose tests notice if a probe changes what the program does: how often user code
//! runs, in which order, and how long temporaries live. The tests must pass unchanged on
//! the instrumented crate.

use std::cell::{Cell, RefCell};
use std::cmp::Ordering;

/// A local macro with the name of a standard one: its argument is turned into text.
macro_rules! vec {
    ($e:expr) => {
        stringify!($e)
    };
}

pub fn spelled(a: u32) -> &'static str {
    let _ = a;
    vec![a < 3]
}

/// Counts how often it is compared.
pub struct Counted<'a> {
    pub value: u32,
    pub comparisons: &'a Cell<u32>,
}

impl PartialEq for Counted<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl PartialOrd for Counted<'_> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.comparisons.set(self.comparisons.get() + 1);
        self.value.partial_cmp(&other.value)
    }
}

pub fn is_smaller(a: &Counted, b: &Counted) -> bool {
    a < b
}

/// The shared borrow taken by the first element lives until the end of the statement, so
/// the second element cannot borrow mutably.
pub fn borrow_state(cell: &RefCell<Vec<u8>>, extra: usize) -> (usize, bool) {
    let pair = (cell.borrow().len() + extra, cell.try_borrow_mut().is_ok());
    pair
}

/// Each operand of `&&` drops its temporaries when it is done: the borrow taken on the
/// left is gone by the time the right operand runs.
pub fn released_between_operands(cell: &RefCell<Vec<u8>>) -> bool {
    let both = cell.borrow().is_empty() && cell.try_borrow_mut().is_ok();
    both
}

/// Parameters that happen to be named like the variables the probes use.
pub fn shadowing(a: bool, __tmt_c: u8, __tmt_a: u32, __tmt_b: u32, __tmt_v: bool) -> bool {
    a && __tmt_c == 2 && __tmt_a + __tmt_b > 3 && !__tmt_v
}

/// The right operand runs exactly when the left one is true.
pub fn audited(ready: bool, audits: &Cell<u32>) -> bool {
    ready && {
        audits.set(audits.get() + 1);
        true
    }
}

/// Records the order in which the two sides of a compound assignment are evaluated.
pub fn accumulate(log: &RefCell<Vec<&'static str>>) -> u32 {
    let mut slots = [10u32];
    let mut total = 0u32;
    slots[{
        log.borrow_mut().push("index");
        0
    }] += {
        log.borrow_mut().push("value");
        1
    };
    total += slots[0];
    total
}

/// The borrow that `first()` is called on lives until the end of the statement, like any
/// temporary of its receiver.
pub fn end_borrow_state(cell: &RefCell<Vec<u8>>) -> (Option<u8>, bool) {
    let pair = (cell.borrow().first().copied(), cell.try_borrow_mut().is_ok());
    pair
}

fn counted<'a>(calls: &Cell<u32>, values: &'a [u8]) -> &'a [u8] {
    calls.set(calls.get() + 1);
    values
}

/// The receiver of `last()` is evaluated once.
pub fn last_of_counted(calls: &Cell<u32>, values: &[u8]) -> Option<u8> {
    counted(calls, values).last().copied()
}

/// What `first()` and `last()` give lives as long as the sequence, not as the variable
/// that refers to it.
pub fn head<'a>(names: &'a [String]) -> Option<&'a String> {
    names.first()
}

pub fn tail<'a>(names: &'a Vec<String>) -> Option<&'a String> {
    let names = names;
    names.last()
}

/// The end of a sequence that is made on the spot.
pub fn made_on_the_spot(n: u8) -> Option<u8> {
    [n, n + 1].to_vec().last().copied()
}

/// A `&mut` stays one for the call: what it gives lives as long as the sequence.
pub fn head_of_exclusive(values: &mut Vec<u8>) -> Option<&u8> {
    let end = values.first();
    end
}

/// The line a call ends on is the line the next one starts on.
pub fn lines_between(values: &[u8]) -> u32 {
    let before = line!();
    let _ = values
        .first();
    line!() - before
}

pub mod shadowed {
    /// A `first` and a `last` that a trait of the crate gives to `Vec`: they are found before
    /// the methods of a slice, and are not its ends.
    pub trait Ends {
        fn first(&self) -> Option<&u8>;
        fn last(&self) -> Option<&u8>;
    }

    impl Ends for Vec<u8> {
        fn first(&self) -> Option<&u8> {
            None
        }
        fn last(&self) -> Option<&u8> {
            self.as_slice().get(1)
        }
    }

    pub fn first_by_the_trait(values: Vec<u8>) -> Option<u8> {
        values.first().copied()
    }

    pub fn last_by_the_trait(values: Vec<u8>) -> Option<u8> {
        values.last().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_macros_keep_their_argument() {
        assert_eq!(spelled(1), "a < 3");
    }

    #[test]
    fn a_comparison_runs_user_code_once() {
        let comparisons = Cell::new(0);
        let (one, two) = (
            Counted { value: 1, comparisons: &comparisons },
            Counted { value: 2, comparisons: &comparisons },
        );
        assert!(is_smaller(&one, &two));
        assert_eq!(comparisons.get(), 1);
        assert!(!is_smaller(&two, &one));
        assert!(!is_smaller(&one, &one));
        assert_eq!(comparisons.get(), 3);
    }

    #[test]
    fn temporaries_live_as_long_as_before() {
        let cell = RefCell::new([1u8, 2].to_vec());
        assert_eq!(borrow_state(&cell, 1), (3, false));
        assert_eq!(borrow_state(&cell, 0), (2, false));
        let empty = RefCell::new(Vec::new());
        assert!(released_between_operands(&empty));
        assert!(!released_between_operands(&cell));
    }

    #[test]
    fn probe_variables_do_not_capture() {
        assert!(shadowing(true, 2, 3, 1, false));
        assert!(!shadowing(true, 0, 3, 1, false));
        assert!(!shadowing(true, 1, 3, 1, false));
        assert!(!shadowing(true, 2, 1, 1, false));
        assert!(!shadowing(true, 2, 3, 1, true));
        assert!(!shadowing(false, 2, 3, 1, false));
    }

    #[test]
    fn the_right_operand_runs_when_it_should() {
        let audits = Cell::new(0);
        assert!(audited(true, &audits));
        assert_eq!(audits.get(), 1);
        assert!(!audited(false, &audits));
        assert_eq!(audits.get(), 1);
    }

    #[test]
    fn compound_assignment_evaluates_the_value_first() {
        let log = RefCell::new(Vec::new());
        assert_eq!(accumulate(&log), 11);
        assert_eq!(*log.borrow(), ["value", "index"]);
    }

    #[test]
    fn the_ends_of_a_sequence_are_asked_for_as_written() {
        let cell = RefCell::new([1u8, 2].to_vec());
        assert_eq!(end_borrow_state(&cell), (Some(1), false));
        let calls = Cell::new(0);
        assert_eq!(last_of_counted(&calls, &[4, 5]), Some(5));
        assert_eq!(last_of_counted(&calls, &[]), None);
        assert_eq!(calls.get(), 2);
        let names = ["a".to_string(), "b".to_string()].to_vec();
        assert_eq!(head(&names).map(String::as_str), Some("a"));
        assert_eq!(tail(&names).map(String::as_str), Some("b"));
        assert_eq!(made_on_the_spot(3), Some(4));
        assert_eq!(head_of_exclusive(&mut [8, 9].to_vec()), Some(&8));
        assert_eq!(lines_between(&[1, 2]), 3);
        // One element each: of a slice, that would be the same end.
        assert_eq!(shadowed::first_by_the_trait([7].to_vec()), None);
        assert_eq!(shadowed::last_by_the_trait([7].to_vec()), None);
        assert_eq!(shadowed::last_by_the_trait([7, 8].to_vec()), Some(8));
        assert_eq!(shadowed::last_by_the_trait(Vec::new()), None);
    }
}
