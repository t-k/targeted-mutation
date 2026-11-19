//! Constructs that are awkward to instrument, with deliberately thin tests.
#![allow(unused_comparisons, clippy::all)]

use std::collections::HashMap;

pub const fn const_limit(a: u32) -> bool {
    a < 10
}

pub fn clamp_percent(value: i64) -> i64 {
    if value < 0 {
        return 0;
    }
    if value > 100 {
        return 100;
    }
    value
}

pub fn max_of<T: PartialOrd + Copy>(a: T, b: T) -> T {
    if a >= b { a } else { b }
}

pub fn is_hot(celsius: f64) -> bool {
    celsius > 30.5
}

pub fn same_text(a: &str, b: &String) -> bool {
    a == *b
}

pub fn non_empty(items: &[u8]) -> bool {
    items.len() > 0
}

/// Always true: must not be reported as a gap.
pub fn unsigned_floor(x: u32) -> bool {
    x >= 0
}

pub fn count_small(items: &[u32], limit: u32) -> usize {
    items.iter().filter(|x| **x < limit).count()
}

pub fn ref_less(a: &u32, b: &u32) -> bool {
    a < b
}

pub fn sum_until(items: &[u32], stop: u32) -> u32 {
    let mut total = 0;
    let mut i = 0;
    while i < items.len() {
        if items[i] == stop {
            break;
        }
        total += items[i];
        i += 1;
    }
    total
}

pub fn late_init(flag: bool) -> u32 {
    let value;
    value = if flag { 1 } else { 2 };
    value
}

pub fn parse_positive(text: &str) -> Result<u32, String> {
    let n: u32 = text.parse().map_err(|_| "not a number".to_string())?;
    ensure_positive(n)?;
    Ok(n)
}

fn ensure_positive(n: u32) -> Result<(), String> {
    if n == 0 { Err("zero".into()) } else { Ok(()) }
}

#[derive(Default)]
pub struct Counter {
    hits: HashMap<String, u32>,
    total: u32,
}

impl Counter {
    pub fn hit(&mut self, key: &str) {
        *self.hits.entry(key.to_string()).or_insert(0) += 1;
        self.total += 1;
    }

    pub fn get(&self, key: &str) -> u32 {
        self.hits.get(key).copied().unwrap_or(0)
    }

    pub fn total(&self) -> u32 {
        self.total
    }
}

pub fn in_thread(x: i32) -> bool {
    std::thread::spawn(move || x > 5).join().unwrap()
}

pub fn give_up() -> ! {
    fail_hard();
}

fn fail_hard() -> ! {
    panic!("gave up")
}

pub fn never_called(x: u8) -> bool {
    x == 7
}

#[cfg(test)]
mod tests;
