//! One function for each kind of mutant: a function's body replaced by a value, the
//! second replacement of an operator, a dropped minus, a constant guard, a deleted field.

use std::collections::HashMap;

pub fn opposite(x: i32) -> i32 {
    -x
}

pub fn freezing() -> i32 {
    -5
}

pub fn tripled(x: i32) -> i32 {
    x * 3
}

pub fn sign(n: i32) -> &'static str {
    match n {
        n if n < 0 => "negative",
        0 => "zero",
        _ => "positive",
    }
}

pub enum Shape {
    Square(u32),
    Circle(u32),
}

pub fn is_big(shape: &Shape) -> bool {
    match shape {
        Shape::Square(side) if *side > 10 => true,
        Shape::Square(_) => false,
        Shape::Circle(radius) => *radius > 5,
    }
}

#[derive(Clone, Debug, PartialEq, Default)]
pub struct Options {
    pub depth: u32,
    pub name: String,
    pub strict: bool,
}

pub fn strict(depth: u32) -> Options {
    Options {
        depth,
        strict: true,
        ..Default::default()
    }
}

pub fn renamed(base: Options, name: &str) -> Options {
    Options {
        name: name.to_string(),
        ..base
    }
}

pub fn count(words: &[&str]) -> usize {
    words.len()
}

pub fn label(n: u32) -> String {
    format!("#{n}")
}

pub fn lengths(words: &[&str]) -> HashMap<String, usize> {
    words.iter().map(|w| (w.to_string(), w.len())).collect()
}

pub fn longest<'a>(words: &[&'a str]) -> Option<&'a str> {
    words.iter().copied().max_by_key(|w| w.len())
}

pub fn parse(text: &str) -> Result<u32, String> {
    text.trim().parse().map_err(|_| format!("not a number: {text}"))
}

pub fn pair(n: u32) -> (u32, bool) {
    (n + 1, n > 0)
}

pub fn record(log: &mut Vec<String>, line: &str) {
    if !line.is_empty() {
        log.push(line.to_string());
    }
}

#[cfg_attr(any(), mutants::skip)]
pub fn untouched(x: i32) -> i32 {
    x + 1
}

pub fn evens(limit: u32) -> impl Iterator<Item = u32> {
    (0..limit).filter(|n| n % 2 == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers() {
        assert_eq!(opposite(0), 0);
        assert!(freezing() != 0);
        assert_eq!(tripled(0), 0);
        assert_eq!(untouched(1), 2);
        assert_eq!(evens(5).count(), 3);
    }

    #[test]
    fn signs_and_shapes() {
        assert_eq!(sign(-2), "negative");
        assert_eq!(sign(0), "zero");
        assert_eq!(sign(3), "positive");
        assert!(is_big(&Shape::Square(11)));
        assert!(is_big(&Shape::Circle(6)));
        assert!(!is_big(&Shape::Circle(5)));
    }

    #[test]
    fn options() {
        assert_eq!(strict(0).depth, 0);
        assert!(strict(3).strict);
        let base = Options {
            depth: 2,
            name: "x".to_string(),
            strict: false,
        };
        assert_eq!(renamed(base, "y").name, "y");
    }

    #[test]
    fn values() {
        assert!(count(&["a", "b"]) > 0);
        assert!(!label(7).is_empty());
        assert_eq!(lengths(&["ab"])["ab"], 2);
        assert!(longest(&["a", "bcd"]).is_some());
        assert!(parse("12").is_ok());
        assert_eq!(pair(0), (1, false));
        let mut log = vec![];
        record(&mut log, "");
        assert!(log.is_empty());
    }
}
