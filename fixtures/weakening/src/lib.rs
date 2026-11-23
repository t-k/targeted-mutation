//! Tests that are about to change: `tests/e2e.rs` weakens them and asks what they no
//! longer find out.

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum Status {
    Open,
    Paid,
    Cancelled,
}

impl Status {
    pub fn is_terminal(self) -> bool {
        self != Status::Open
    }
}

/// What an order comes to when it is settled.
pub fn settle(amount: u32, refund: bool) -> Status {
    if refund || amount == 0 {
        Status::Cancelled
    } else {
        Status::Paid
    }
}

pub fn is_eligible(age: u32) -> bool {
    age >= 18
}

/// Never tested at its limit: a gap that was there before.
pub fn is_senior(age: u32) -> bool {
    age >= 65
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settled_orders() {
        assert_eq!(settle(10, false), Status::Paid);
        assert_eq!(settle(10, true), Status::Cancelled);
        assert_eq!(settle(0, false), Status::Cancelled);
    }

    #[test]
    fn ages() {
        assert!(!is_eligible(17));
        assert!(is_eligible(18));
        assert!(is_eligible(20));
        assert!(!is_senior(30));
        assert!(is_senior(80));
    }
}
