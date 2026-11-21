//! Tests that leave two implementations distinguishable only on additional inputs.

#![allow(unused_variables)]

pub fn chars(text: &str) -> usize {
    text.chars().count()
}

pub fn expiry(created: u64, updated: u64, expires: u64) -> bool {
    created < expires
}

pub struct Entry {
    pub created: u64,
    pub updated: u64,
}

pub fn field_expiry(entry: &Entry, expires: u64) -> bool {
    entry.created < expires
}

pub fn order(entries: &mut [Entry]) {
    entries.sort_by_key(|entry| entry.created);
}

pub fn encode(value: u16) -> [u8; 2] {
    value.to_le_bytes()
}

pub fn decode(bytes: [u8; 2]) -> u16 {
    u16::from_le_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn existing_inputs() {
        assert_eq!(chars("ascii"), 5);
        assert!(expiry(10, 10, 20));
        assert!(field_expiry(
            &Entry {
                created: 10,
                updated: 10,
            },
            20,
        ));
        let mut rows = [
            Entry {
                created: 20,
                updated: 20,
            },
            Entry {
                created: 10,
                updated: 10,
            },
        ];
        order(&mut rows);
        assert_eq!(rows[0].created, 10);
    }

    #[test]
    fn round_trip() {
        assert_eq!(decode(encode(0x0102)), 0x0102);
    }
}
