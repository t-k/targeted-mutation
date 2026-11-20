//! Every mutant cargo-mutants generates for the fixture crates has a counterpart here.
//!
//! The lists in `tests/data/cargo-mutants` are the output of `cargo mutants --list`
//! (cargo-mutants 27.1.0) for each fixture.

use std::collections::BTreeMap;

use targeted_mutation::instrument::{Site, SiteKind, discover, source_mutants};

type Key = (String, usize, String, String);

fn squeezed(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

fn key(genre: &str, line: usize, what: &str, into: &str) -> Key {
    (genre.to_string(), line, squeezed(what), squeezed(into))
}

/// A value without the module path of the types in it: cargo-mutants writes
/// `Cow::Borrowed("")` where the return type is `std::borrow::Cow<'_, str>`, this tool
/// keeps the path the type was written with.
fn unqualified(value: &str) -> String {
    let mut text = value.to_string();
    loop {
        // A `module::` (or a leading `::`) right in front of a type name.
        let cut = text.match_indices("::").find_map(|(at, _)| {
            if !text[at + 2..].starts_with(char::is_uppercase) {
                return None;
            }
            let start = text[..at]
                .rfind(|c: char| !(c.is_alphanumeric() || c == '_'))
                .map_or(0, |before| before + 1);
            let module = &text[start..at];
            (module.is_empty() || module.starts_with(char::is_lowercase)).then_some((start, at + 2))
        });
        match cut {
            Some((from, to)) => text.replace_range(from..to, ""),
            None => return text,
        }
    }
}

/// Every mutant this tool has for a source file.
fn keys_of_file(source: &str) -> Vec<Key> {
    let found = discover(source, 0).unwrap();
    let mut keys: Vec<Key> = found
        .sites
        .iter()
        .chain(&found.rebuild_sites)
        .flat_map(|site| keys_of(source, site))
        .collect();
    // An empty body that is the same mutant as the deleted statement, which is a site.
    keys.extend(
        found
            .single_statement_bodies
            .iter()
            .map(|name| key("body", 0, name, "()")),
    );
    keys
}

fn line_of(source: &str, offset: usize) -> usize {
    source[..offset].matches('\n').count() + 1
}

/// The mutants of one site, named the way cargo-mutants names them.
fn keys_of(source: &str, site: &Site) -> Vec<Key> {
    // cargo-mutants gives the position of the operator, not of the expression.
    let operator_line = |gap: &(usize, usize)| {
        let between = &source[gap.0..gap.1];
        line_of(source, gap.0 + between.len() - between.trim_start().len())
    };
    let mut keys: Vec<Key> = source_mutants(site)
        .iter()
        .map(|mutant| match &site.kind {
            SiteKind::Cmp { gap, .. }
            | SiteKind::Logic { gap, .. }
            | SiteKind::Arith { gap, .. } => {
                key("op", operator_line(gap), &mutant.what, &mutant.into)
            }
            SiteKind::Not { .. } | SiteKind::Neg { .. } => {
                key("delete", site.line, &mutant.what, "")
            }
            SiteKind::Body { name, .. } => {
                let value = mutant.into.replace("::core::", "::std::");
                key("body", 0, name, &unqualified(&value))
            }
            SiteKind::Arm { pattern, .. } => key("arm", site.line, pattern, ""),
            SiteKind::Guard { .. } => key("guard", site.line, &site.text, &mutant.into),
            SiteKind::Field { name, .. } => key("field", site.line, name, ""),
            // Not mutants of cargo-mutants: results replaced after the body ran, the
            // other of `min` / `max`, the end of a range, a deleted statement.
            // Not one of cargo-mutants either, but it can be written in the source.
            SiteKind::Range { .. } => key("range", site.line, &mutant.what, &mutant.into),
            SiteKind::Return { .. } | SiteKind::MinMax { .. } | SiteKind::Stmt => {
                unreachable!("no source mutants")
            }
        })
        .collect();
    // Where the arm can be removed, that is its guard being `false`.
    if matches!(
        site.kind,
        SiteKind::Guard {
            can_be_false: false
        }
    ) {
        keys.push(key("guard", site.line, &site.text, "false"));
    }
    keys
}

/// `src/lib.rs:7:5: replace opposite -> i32 with 0` and the like.
fn key_of_listed(line: &str) -> Key {
    let mut parts = line.splitn(4, ':');
    let (_file, row, _column, text) = (
        parts.next().unwrap(),
        parts.next().unwrap().parse::<usize>().unwrap(),
        parts.next().unwrap(),
        parts.next().unwrap().trim(),
    );
    let without_function = |text: &str| -> String {
        text.rsplit_once(" in ")
            .map_or(text, |(before, _)| before)
            .to_string()
    };
    if let Some(rest) = text.strip_prefix("replace match guard ") {
        let rest = without_function(rest);
        let (guard, value) = rest.rsplit_once(" with ").unwrap();
        return key("guard", row, guard, value);
    }
    if let Some(rest) = text.strip_prefix("delete match arm ") {
        return key("arm", row, &without_function(rest), "");
    }
    if let Some(rest) = text.strip_prefix("delete field ") {
        return key("field", row, rest.split(' ').next().unwrap(), "");
    }
    if let Some(rest) = text.strip_prefix("delete ") {
        return key("delete", row, rest.split(' ').next().unwrap(), "");
    }
    let rest = text.strip_prefix("replace ").unwrap();
    let (what, value) = rest.split_once(" with ").unwrap();
    let operators = [
        "==", "!=", "<=", ">=", "<", ">", "&&", "||", "+=", "-=", "*=", "/=", "%=", "<<=", ">>=",
        "&=", "|=", "^=", "+", "-", "*", "/", "%", "<<", ">>", "&", "|", "^",
    ];
    if operators.contains(&what) {
        return key("op", row, what, &without_function(value));
    }
    // A function: `name -> type`, `Type::name`, `<impl Trait for Type>::name`.
    let name = what.split(" -> ").next().unwrap();
    let name = name.rsplit("::").next().unwrap();
    key("body", 0, name, &unqualified(value))
}

fn counted(keys: impl IntoIterator<Item = Key>) -> BTreeMap<Key, usize> {
    let mut all = BTreeMap::new();
    for key in keys {
        *all.entry(key).or_insert(0) += 1;
    }
    all
}

#[test]
fn every_mutant_of_cargo_mutants_has_a_counterpart() {
    // What is left out on purpose, with the reason. Everything else has to be there.
    let left_out: &[(&str, Key, &str)] = &[
        (
            "operators",
            key("op", 48, "&&", "||"),
            "a let chain: `if let .. || ..` does not compile",
        ),
        // The fixture marks `untouched` with `cfg_attr(any(), mutants::skip)` so that it
        // needs no dependency; cargo-mutants reads the plain attribute and `cfg_attr(test, ..)`.
        (
            "genres",
            key("body", 0, "untouched", "0"),
            "marked to be skipped",
        ),
        (
            "genres",
            key("body", 0, "untouched", "1"),
            "marked to be skipped",
        ),
        (
            "genres",
            key("body", 0, "untouched", "-1"),
            "marked to be skipped",
        ),
        ("genres", key("op", 100, "+", "-"), "marked to be skipped"),
        ("genres", key("op", 100, "+", "*"), "marked to be skipped"),
    ];
    let fixtures = [
        (
            "genres",
            include_str!("../fixtures/genres/src/lib.rs"),
            include_str!("data/cargo-mutants/genres.txt"),
        ),
        (
            "kitchen-sink",
            include_str!("../fixtures/kitchen-sink/src/lib.rs"),
            include_str!("data/cargo-mutants/kitchen-sink.txt"),
        ),
        (
            "shapes",
            include_str!("../fixtures/shapes/src/lib.rs"),
            include_str!("data/cargo-mutants/shapes.txt"),
        ),
        (
            "operators",
            include_str!("../fixtures/operators/src/lib.rs"),
            include_str!("data/cargo-mutants/operators.txt"),
        ),
        (
            "semantics",
            include_str!("../fixtures/semantics/src/lib.rs"),
            include_str!("data/cargo-mutants/semantics.txt"),
        ),
        (
            "eligibility",
            include_str!("../fixtures/eligibility/src/lib.rs"),
            include_str!("data/cargo-mutants/eligibility.txt"),
        ),
        (
            "batch-store",
            include_str!("../fixtures/batch-store/src/lib.rs"),
            include_str!("data/cargo-mutants/batch-store.txt"),
        ),
        (
            "looping",
            include_str!("../fixtures/looping/src/lib.rs"),
            include_str!("data/cargo-mutants/looping.txt"),
        ),
    ];
    let mut missing = vec![];
    let (mut theirs_total, mut ours_total) = (0, 0);
    for (name, source, listed) in fixtures {
        let ours = counted(keys_of_file(source));
        let theirs = counted(listed.lines().map(key_of_listed));
        theirs_total += theirs.values().sum::<usize>();
        ours_total += ours.values().sum::<usize>();
        for (key, count) in &theirs {
            let have = ours.get(key).copied().unwrap_or(0);
            let excused = left_out
                .iter()
                .any(|(fixture, left, _)| *fixture == name && left == key);
            if have < *count && !excused {
                missing.push(format!("{name}: {key:?} ({have} of {count})"));
            }
        }
    }
    assert!(missing.is_empty(), "{}", missing.join("\n"));
    // And none of the exceptions is one that is no longer needed.
    for (fixture, left, why) in left_out {
        let (_, source, listed) = fixtures.iter().find(|f| f.0 == *fixture).unwrap();
        let ours = keys_of_file(source);
        assert!(
            listed.lines().map(key_of_listed).any(|k| k == *left),
            "{left:?}"
        );
        assert!(!ours.contains(left), "{left:?} is there: {why}");
    }
    assert!(ours_total > theirs_total, "{ours_total} {theirs_total}");
}

/// The same check for any other crate, on request:
///
/// ```text
/// (cd CRATE && cargo mutants --list > /tmp/list.txt)
/// TMT_PARITY_SRC=CRATE TMT_PARITY_LIST=/tmp/list.txt cargo test --test parity another_crate -- --nocapture
/// ```
///
/// Prints the mutants of the list that have no counterpart, and fails if there are any.
#[test]
fn another_crate_on_request() {
    let (Ok(root), Ok(list)) = (
        std::env::var("TMT_PARITY_SRC"),
        std::env::var("TMT_PARITY_LIST"),
    ) else {
        return;
    };
    let listed = std::fs::read_to_string(&list).unwrap();
    let mut by_file: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for line in listed.lines().filter(|line| line.contains(": ")) {
        by_file
            .entry(line.split(':').next().unwrap())
            .or_default()
            .push(line);
    }
    let (mut theirs_total, mut ours_total, mut missing) = (0, 0, vec![]);
    for (file, lines) in by_file {
        let path = std::path::Path::new(&root).join(file);
        let source =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let ours = counted(keys_of_file(&source));
        let theirs = counted(lines.iter().map(|line| key_of_listed(line)));
        theirs_total += theirs.values().sum::<usize>();
        ours_total += ours.values().sum::<usize>();
        for (key, count) in &theirs {
            let have = ours.get(key).copied().unwrap_or(0);
            if have < *count {
                missing.push(format!("{file}: {key:?} ({have} of {count})"));
            }
        }
    }
    println!("listed: {theirs_total}, counterparts and more here: {ours_total}");
    assert!(missing.is_empty(), "{}", missing.join("\n"));
}
