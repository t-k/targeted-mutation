use std::collections::BTreeSet;

use anyhow::Result;

use crate::plan::CmpOp;

use super::*;

#[test]
fn what_an_expression_can_be_is_read_from_its_last_operation() {
    use crate::plan::Reach;
    for (expr, expected) in [
        ("blocks * 8", Some(Reach::Multiples(8))),
        ("8 * blocks", Some(Reach::Multiples(8))),
        ("(blocks * (2))", Some(Reach::Multiples(2))),
        ("blocks << 3", Some(Reach::Multiples(8))),
        ("blocks << 9", Some(Reach::Multiples(512))),
        ("flags & 0xF0", Some(Reach::Bits(0xF0))),
        ("0b1010u8 & flags", Some(Reach::Bits(0b1010))),
        ("n % 10", Some(Reach::Below(10))),
        ("(a + b) % 7_u32", Some(Reach::Below(7))),
        // A product that wraps is a multiple only of a power of two.
        ("cents * 10", None),
        ("blocks * 1", None),
        ("blocks * 0", None),
        ("a * b", None),
        ("3 << blocks", None),
        ("blocks << 0", None),
        ("blocks << 200", None),
        ("10 % n", None),
        ("n % 0", None),
        ("blocks * 8 + 1", None),
        ("(blocks * 8) as u8", None),
        ("-(blocks * 8)", None),
        ("blocks", None),
        ("flags | 0xF0", None),
        ("blocks / 8", None),
        ("x * 8.0", None),
        ("not rust (", None),
    ] {
        assert_eq!(reach(expr).map(|(reach, _)| reach), expected, "{expr}");
        // Only a shift comes with a number of bits to check against the type.
        assert_eq!(
            reach(expr).and_then(|(_, bits)| bits).is_some(),
            expr.contains("<<") && expected.is_some(),
            "{expr}"
        );
    }
}

#[test]
fn code_of_the_tests_is_told_from_the_rest() {
    let source = r#"
            pub fn open(n: u32) -> bool { n > 1 }
            #[test]
            fn at_the_top() { assert!(open(2)); }
            #[cfg(test)]
            mod tests { fn helper() -> u32 { 3 } }
            mod plain {
                pub fn inside(n: u32) -> bool { n > 4 }
                #[cfg(test)]
                fn only_tested() -> u32 { 5 }
            }
            struct S;
            impl S {
                fn method(&self) -> u32 { 6 }
                #[cfg(test)]
                fn test_method(&self) -> u32 { 7 }
            }
            #[cfg(not(test))]
            fn never_in_tests() -> u32 { 8 }
        "#;
    let file = parse_source(source).unwrap();
    let ranges = test_only_ranges(source, &file);
    let of_tests = |text: &str| {
        let at = source.find(text).unwrap_or_else(|| panic!("{text}"));
        ranges.iter().any(|(from, to)| *from <= at && at < *to)
    };
    for text in ["open(2)", "{ 3 }", "{ 5 }", "{ 7 }", "fn at_the_top"] {
        assert!(of_tests(text), "{text}");
    }
    for text in ["n > 1", "n > 4", "{ 6 }", "{ 8 }", "struct S", "mod plain"] {
        assert!(!of_tests(text), "{text}");
    }
    let whole = "#![cfg(test)]\nfn f() {}\n";
    let file = parse_source(whole).unwrap();
    assert_eq!(test_only_ranges(whole, &file), [(0, whole.len())]);
}

#[test]
fn comments_documentation_and_spacing_are_not_program_text() {
    let code = "/// Says.\n#[inline]\nfn f(a: u8) -> u8 {\n    a + 1 // one more\n}\n";
    for same in [
        "#[inline] fn f(a:u8)->u8{a+1}",
        "/// Says something else.\n#[inline]\nfn f(a: u8) -> u8 { /* why */ a + 1 }",
        "#[doc = \"other\"] #[inline] fn f(a: u8) -> u8 { a + 1 }",
    ] {
        assert_eq!(same_tokens(code, same), Some(true), "{same}");
    }
    for other in [
        "#[inline] fn f(a: u8) -> u8 { a + 2 }",
        "fn f(a: u8) -> u8 { a + 1 }",
        "#[inline] fn f(a: u8) -> u8 { (a + 1) }",
        "#[inline] fn f(a: u8) -> u8 { a + 1 } fn g() {}",
        "#![inline] fn f(a: u8) -> u8 { a + 1 }",
        "#[inline] fn f(a: u8) -> u8 { [a + 1][0] }",
    ] {
        assert_eq!(same_tokens(code, other), Some(false), "{other}");
    }
    assert_eq!(
        same_tokens("#![doc = \"a\"] fn f() {}", "fn f() {}"),
        Some(true)
    );
    // What is given to a macro is the macro's to read, documentation and all.
    for (one, other) in [
        (r#"m!(#[doc = "old"]);"#, r#"m!(#[doc = "new"]);"#),
        (
            r#"const T: &str = stringify!(#[doc = "old"]);"#,
            r#"const T: &str = stringify!(#[doc = "new"]);"#,
        ),
        (
            "macro_rules! m { (#[doc = \"old\"]) => {1}; }",
            "macro_rules! m { (#[doc = \"new\"]) => {1}; }",
        ),
        (
            "std::vec![#[doc = \"old\"] 1]",
            "std::vec![#[doc = \"new\"] 1]",
        ),
    ] {
        assert_eq!(same_tokens(one, other), Some(false), "{one}");
        assert_eq!(same_tokens(one, one), Some(true), "{one}");
    }
    // A doc comment there is still prose: the documentation of what the macro makes.
    for (one, other) in [
        ("m! { /// old\n fn f() {} }", "m! { /// new\n fn f() {} }"),
        (
            "bitflags! { /** old */ struct F: u8 { /// a\n const A = 1; } }",
            "bitflags! { /** new */ struct F: u8 { /// b\n const A = 1; } }",
        ),
        (
            "macro_rules! m { () => { /// old\n fn f() {} }; }",
            "macro_rules! m { () => { /// new\n fn f() {} }; }",
        ),
    ] {
        assert_eq!(same_tokens(one, other), Some(true), "{one}");
    }
    // A `!` that is no call, and code after a macro, are plain again.
    assert_eq!(
        same_tokens(
            "fn f(a: bool) -> bool { !{ /** a */ a } }",
            "fn f(a: bool) -> bool { !{ /** b */ a } }"
        ),
        Some(true)
    );
    assert_eq!(
        same_tokens("m!(1); /// a\nfn f() {}", "m!(1); /// b\nfn f() {}"),
        Some(true)
    );
    // A comma after the last of a list says nothing, except in a tuple of one.
    for (one, other) in [
        ("enum E { A, B }", "enum E { A, B, }"),
        ("struct S(pub u8);", "struct S(pub u8,);"),
        (
            "fn f(a: u8) { g(a,); s.h(a,) }",
            "fn f(a: u8,) { g(a); s.h(a) }",
        ),
        ("const A: [u8; 2] = [1, 2];", "const A: [u8; 2] = [1, 2,];"),
        ("type Hook = fn(u8);", "type Hook = fn(u8,);"),
        // What is called need not have a name.
        (
            "fn f(s: &S, t: &[fn(u8)]) { (s.callback)(1); t[0](2) }",
            "fn f(s: &S, t: &[fn(u8)]) { (s.callback)(1,); t[0](2,) }",
        ),
        (
            "fn f(n: u8) -> u8 { match n { 1 => 2, _ => 0 } }",
            "fn f(n: u8) -> u8 { match n { 1 => 2, _ => 0, } }",
        ),
    ] {
        assert_eq!(same_tokens(one, other), Some(true), "{one}");
    }
    for (one, other) in [
        ("const T: (u8,) = (1,);", "const T: (u8) = (1);"),
        // A pattern, a value after a keyword or an operator, one with the commas
        // of its types in it: a tuple of one is not what it holds.
        (
            "fn f() { let (x,) = (1u8,); }",
            "fn f() { let (x) = (1u8,); }",
        ),
        (
            "fn f() -> (u8,) { return (1,) }",
            "fn f() -> (u8,) { return (1) }",
        ),
        (
            "fn f() { let t = (Ok::<u8, u8>(1),); }",
            "fn f() { let t = (Ok::<u8, u8>(1)); }",
        ),
        (
            "fn f(a: (u8,), b: u8) -> bool { a > (b,) }",
            "fn f(a: (u8,), b: u8) -> bool { a > (b) }",
        ),
        (
            "fn f(x: u8) { for (a,) in [(x,)] {} }",
            "fn f(x: u8) { for (a) in [(x,)] {} }",
        ),
        // After an attribute.
        (
            "fn f() -> impl Sized { #[allow(unused_parens)] (2u8,) }",
            "fn f() -> impl Sized { #[allow(unused_parens)] (2u8) }",
        ),
        // After the name of a lifetime or of a label.
        ("fn f<'a>(x: &'a (u8,)) {}", "fn f<'a>(x: &'a (u8)) {}"),
        (
            "fn f() { let v = 'outer: loop { break 'outer (1u8,); }; }",
            "fn f() { let v = 'outer: loop { break 'outer (1u8); }; }",
        ),
        ("m!(a, b);", "m!(a, b,);"),
        ("enum E { A, B }", "enum E { A, B, C }"),
        ("fn f() { g(1, 2) }", "fn f() { g(1,, 2) }"),
    ] {
        assert_eq!(same_tokens(one, other), Some(false), "{one}");
    }
    // The name that stands for what a build leaves out is a name like any other
    // where two texts of a program are compared.
    assert_eq!(
        same_tokens(
            &format!("const A: [u8; 2] = [1, {LEFT_OUT}];"),
            "const A: [u8; 1] = [1];"
        ),
        Some(false)
    );
    let same_tokens = same_built_tokens;
    // What stands for code that is left out is not the name of a call: a tuple
    // of one after it keeps its comma.
    assert_eq!(
        same_tokens(
            &format!("fn f() -> impl Sized {{ {LEFT_OUT} (1u8,) }}"),
            &format!("fn f() -> impl Sized {{ {LEFT_OUT} (1u8) }}")
        ),
        Some(false)
    );
    // What a build leaves out goes with a comma of its own, wherever it stands.
    let hole = LEFT_OUT;
    for with in [
        format!("enum E {{ A, {hole}, B }}"),
        format!("enum E {{ A, B, {hole}, }}"),
        format!("enum E {{ A, B, {hole} }}"),
        format!("enum E {{ {hole}, A, B }}"),
        format!("enum E {{ A, {hole} /* note */ , B, }}"),
        format!("{hole} enum E {{ A, B }}"),
    ] {
        assert_eq!(same_tokens(&with, "enum E { A, B }"), Some(true), "{with}");
    }
    // A left-out element in a tuple keeps its comma conservatively so a 1-tuple
    // (including one with commas in type arguments) is never equated with `(x)`.
    assert_eq!(
        same_tokens(
            &format!("const T: (u8,) = (1, {hole});"),
            "const T: (u8,) = (1,);"
        ),
        Some(true)
    );
    assert_eq!(
        same_tokens(
            &format!("const T: (u8,) = (1, {hole});"),
            "const T: (u8,) = (1);"
        ),
        Some(false)
    );
    assert_eq!(
        same_tokens(
            &format!("const T: (u8, u8) = (1, 2, {hole});"),
            "const T: (u8, u8) = (1, 2,);"
        ),
        Some(true)
    );
    // An arm in braces has no comma of its own: the one before it is another arm's.
    assert_eq!(
        same_tokens(
            &format!("fn f(n: u8) -> u8 {{ match n {{ 0 => 1, {hole} _ => 0, }} }}"),
            "fn f(n: u8) -> u8 { match n { 0 => 1, _ => 0 } }"
        ),
        Some(true)
    );
    assert_eq!(
        same_tokens(&format!("enum E {{ A, {hole}, B }}"), "enum E { A }"),
        Some(false)
    );
    // A statement that is an expression goes with its semicolon; a field left out
    // of a tuple struct leaves no comma that counts.
    assert_eq!(
        same_tokens(
            &format!("fn f() {{ a(); {hole}; b() }}"),
            "fn f() { a(); b() }"
        ),
        Some(true)
    );
    assert_eq!(
        same_tokens(&format!("struct S(pub u8, {hole},);"), "struct S(pub u8);"),
        Some(true)
    );
    assert_eq!(same_tokens(code, "fn f( {"), None);
    assert_eq!(same_tokens("fn f( {", code), None);
}

#[test]
fn a_cfg_is_decided_only_as_far_as_the_features_decide_it() {
    let on: BTreeSet<String> = ["basic".to_string()].into();
    for (predicate, expected) in [
        (r#"feature = "basic""#, Some(true)),
        (r#"feature = "extra""#, Some(false)),
        (r#"not(feature = "extra")"#, Some(true)),
        (r#"not(feature = "basic")"#, Some(false)),
        ("test", None),
        (r#"target_os = "linux""#, None),
        ("not(test)", None),
        ("all()", Some(true)),
        ("any()", Some(false)),
        (r#"all(feature = "basic", feature = "extra")"#, Some(false)),
        (r#"all(test, feature = "extra")"#, Some(false)),
        (r#"all(feature = "extra", test)"#, Some(false)),
        (r#"all(test, feature = "basic")"#, None),
        (
            r#"all(feature = "basic", not(feature = "extra"))"#,
            Some(true),
        ),
        (r#"any(feature = "basic", feature = "extra")"#, Some(true)),
        (r#"any(test, feature = "basic")"#, Some(true)),
        (r#"any(feature = "basic", test)"#, Some(true)),
        (r#"any(test, feature = "extra")"#, None),
        (r#"any(feature = "extra", feature = "more")"#, Some(false)),
        (r#"not(any(test, feature = "basic"))"#, Some(false)),
        (r#"not(test, feature = "basic")"#, None),
        ("feature = 3", None),
        (r#"unknown(feature = "extra")"#, None),
        ("all(=)", None),
    ] {
        let meta: syn::Meta = syn::parse_str(predicate).unwrap();
        assert_eq!(cfg_value(&meta, &on), expected, "{predicate}");
    }
}

#[test]
fn code_under_a_feature_that_is_off_is_not_in_the_build() {
    let source = r#"
            #[cfg(feature = "extra")]
            pub fn item(n: u32) -> bool { n > 1 }
            #[cfg(feature = "basic")]
            pub fn kept(n: u32) -> bool { n > 2 }
            #[cfg(test)]
            fn only_tests(n: u32) -> bool { n > 3 }
            #[cfg(feature = "extra")]
            mod module { pub fn inside(n: u32) -> bool { n > 4 } }
            struct S;
            impl S {
                #[cfg(feature = "extra")]
                fn method(&self, n: u32) -> bool { n > 5 }
                fn open(&self, n: u32) -> bool { n > 6 }
            }
            trait T {
                #[cfg(feature = "extra")]
                fn provided(&self, n: u32) -> bool { n > 7 }
            }
            #[cfg(feature = "extra")]
            impl T for S {}
            fn body(n: u32) -> u32 {
                #[cfg(feature = "extra")]
                let n = n + 8;
                #[cfg(feature = "extra")]
                { return n + 9; }
                #[cfg(feature = "extra")]
                if n > 10 { return 0; }
                #[cfg(feature = "extra")]
                assert!(n > 11);
                match n {
                    #[cfg(feature = "extra")]
                    12 => 0,
                    _ => n + 13,
                }
            }
            #[cfg(not(feature = "extra"))]
            mod kept_module {
                #[cfg(feature = "extra")]
                const OFF: u32 = 14 + 1;
                const ON: u32 = 15 + 1;
            }
        "#;
    let file = parse_source(source).unwrap();
    let on: BTreeSet<String> = ["basic".to_string()].into();
    let ranges = inactive_ranges(source, &file, &on);
    let left_out = |text: &str| {
        let at = source.find(text).unwrap_or_else(|| panic!("{text}"));
        ranges.iter().any(|(from, to)| *from <= at && at < *to)
    };
    for off in [
        "n > 1 ", "n > 4", "n > 5", "n > 7", "n + 8", "n + 9", "n > 10", "n > 11", "12 =>",
        "14 + 1",
    ] {
        assert!(left_out(off), "{off}");
    }
    // An element of an array, an argument, a value in brackets: any expression.
    let listed = r#"
            pub const FORMATS: &[&str] = &["json", #[cfg(feature = "extra")] "yaml", #[cfg(feature = "basic")] "toml"];
            fn f() -> u8 { g(1, #[cfg(feature = "extra")] 2) }
            fn v(n: u8) -> Vec<bool> { std::vec![false, #[cfg(feature = "extra")] (n > 0)] }
            unsafe extern "C" { #[cfg(feature = "extra")] fn legacy(); fn kept(); }
        "#;
    let listed_file = parse_source(listed).unwrap();
    let listed_ranges = inactive_ranges(listed, &listed_file, &on);
    let texts: Vec<&str> = listed_ranges
        .iter()
        .map(|(from, to)| &listed[*from..*to])
        .collect();
    assert_eq!(
        texts,
        [
            "#[cfg(feature = \"extra\")] \"yaml\"",
            "#[cfg(feature = \"extra\")] 2",
            "#[cfg(feature = \"extra\")] (n > 0)",
            "#[cfg(feature = \"extra\")] fn legacy();"
        ]
    );
    // A `vec!` of the file's own, wherever it is defined, may do anything with what
    // it is given: nothing in it is taken to be left out.
    let own = r#"
            mod m {
                macro_rules! vec { (#[$attr:meta] $value:expr) => { $value }; }
                pub fn value() -> u8 { vec![#[cfg(feature = "extra")] 1] }
            }
        "#;
    assert_eq!(inactive_ranges(own, &parse_source(own).unwrap(), &on), []);
    // A variant of an enum and a field of a struct, which a feature may add.
    let shapes = r#"
            pub enum Format { Json, #[cfg(feature = "extra")] Yaml, #[cfg(feature = "basic")] Toml }
            pub struct Options { pub depth: u8, #[cfg(feature = "extra")] pub strict: bool }
        "#;
    let shapes_file = parse_source(shapes).unwrap();
    let shape_ranges = inactive_ranges(shapes, &shapes_file, &on);
    let texts: Vec<&str> = shape_ranges
        .iter()
        .map(|(from, to)| &shapes[*from..*to])
        .collect();
    assert_eq!(
        texts,
        [
            "#[cfg(feature = \"extra\")] Yaml",
            "#[cfg(feature = \"extra\")] pub strict: bool"
        ]
    );
    for on in ["n > 2", "n > 3", "n > 6", "n + 13", "15 + 1", "struct S"] {
        assert!(!left_out(on), "{on}");
    }
    // With the feature only what asks for its absence is left out, and a file that is
    // off as a whole is all out.
    let both: BTreeSet<String> = ["basic".to_string(), "extra".to_string()].into();
    let with_extra = inactive_ranges(source, &file, &both);
    assert_eq!(with_extra.len(), 1, "{with_extra:?}");
    assert!(source[with_extra[0].0..with_extra[0].1].starts_with("#[cfg(not(feature"));
    let whole = "#![cfg(feature = \"extra\")]\nfn f() {}\n";
    let file = parse_source(whole).unwrap();
    assert_eq!(inactive_ranges(whole, &file, &on), [(0, whole.len())]);
}

fn all(_: u32) -> bool {
    true
}

/// The sites of `src` without the function-level `Return` sites, numbered from
/// `first_id`: most tests are about one expression inside a small function.
fn found(src: &str, first_id: u32) -> Result<Discovery> {
    let mut all = discover(src, 0)?;
    all.sites
        .retain(|s| !matches!(s.kind, SiteKind::Return { .. } | SiteKind::Body { .. }));
    for (index, site) in all.sites.iter_mut().enumerate() {
        site.id = first_id + index as u32;
    }
    Ok(all)
}

#[test]
fn finds_a_comparison_with_a_literal() {
    let src = "pub fn is_eligible(age: u32) -> bool {\n    age >= 18\n}\n";
    let found = found(src, 5).unwrap();
    assert_eq!(found.sites.len(), 1);
    let site = &found.sites[0];
    assert_eq!(
        (site.id, site.line, site.column, site.text.as_str()),
        (5, 2, 5, "age >= 18")
    );
    let SiteKind::Cmp {
        op,
        lhs,
        rhs,
        literal,
        ..
    } = &site.kind
    else {
        panic!()
    };
    assert_eq!(
        (*op, lhs.as_str(), rhs.as_str(), *literal),
        (CmpOp::Ge, "age", "18", LiteralSide::Rhs(18))
    );
}

#[test]
fn literal_sides() {
    let src = "fn f(x: i32, y: i32) { let _ = (-5 < x, x == (3), 1 < 2, x <= y, x > 1.5 as i32); }";
    let literals: Vec<LiteralSide> = found(src, 0)
        .unwrap()
        .sites
        .iter()
        .filter_map(|s| match &s.kind {
            SiteKind::Cmp { literal, .. } => Some(*literal),
            SiteKind::Neg { .. } => None,
            _ => panic!(),
        })
        .collect();
    assert_eq!(
        literals,
        [
            LiteralSide::Lhs(-5),
            LiteralSide::Rhs(3),
            LiteralSide::None,
            LiteralSide::None,
            LiteralSide::None
        ]
    );
}

#[test]
fn renders_a_comparison_probe() {
    let src = "fn f(a: u8) -> bool { a  >=  18 }";
    let sites = found(src, 3).unwrap().sites;
    let out = render(src, &sites, &all);
    assert_eq!(
        out.text,
        "fn f(a: u8) -> bool { (match (&(a), &(18)) { (__tmt_l, __tmt_r) => { #[allow(unused_imports)] \
             use crate::__tmt_rt::{ValAny as _, ValNum as _}; crate::__tmt_rt::ord(3u32, 3u8, __tmt_l, __tmt_r, \
             (&crate::__tmt_rt::W(__tmt_l)).tmt_val(), (&crate::__tmt_rt::W(__tmt_r)).tmt_val()) } }) }"
    );
    assert_eq!(render(src, &sites, &|_| false).text, src);
}

#[test]
fn renders_statement_guards_and_nested_sites() {
    let src = "fn f(v: &mut Vec<u8>, a: u8) -> Result<(), ()> {\n    check((a < 3) == true)?;\n    v.push(a);v.clear();\n    Ok(())\n}";
    let sites = found(src, 0).unwrap().sites;
    let kinds: Vec<&str> = sites.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "check((a < 3) == true)?;",
            "v.push(a);",
            "v.clear();",
            "(a < 3) == true",
            "a < 3"
        ]
    );
    let out = render(src, &sites, &all);
    assert_eq!(
        out.text.lines().count(),
        src.lines().count(),
        "line numbers are preserved"
    );
    assert!(
        out.text
            .contains("if crate::__tmt_rt::live(0u32) { check((match (&(((match (&(a), &(3))"),
        "{}",
        out.text
    );
    assert!(out.text.contains("if crate::__tmt_rt::live(1u32) { v.push(a); }if crate::__tmt_rt::live(2u32) { v.clear(); }"), "{}", out.text);
    syn::parse_file(&out.text).expect("instrumented source parses");

    // Only the inner comparison enabled.
    let inner = render(src, &sites, &|id| id == 4);
    assert!(
        inner.text.contains("check(((match (&(a), &(3))"),
        "{}",
        inner.text
    );
    assert!(inner.text.contains(") == true)?;"), "{}", inner.text);
    syn::parse_file(&inner.text).unwrap();
}

#[test]
fn skips_tests_const_contexts_and_macros() {
    let src = r#"
            const LIMIT: bool = 1 < 2;
            static S: bool = 3 > 2;
            const fn c(a: u8) -> bool { a < 1 }
            fn arr(a: u8) -> [u8; { 2 > 1 } as usize] { let _ = [0u8; (4 > 3) as usize]; assert!(a < 9); [a; 1] }
            enum E { A = (5 > 4) as isize }
            struct T;
            impl T { const K: bool = 6 < 7; const fn k(a: u8) -> bool { a > 6 } }
            #[cfg(test)]
            mod tests { fn t(a: u8) -> bool { a == 1 } }
            #[cfg(test)]
            mod more_tests;
            #[cfg(test)]
            #[doc = "not a path"]
            mod documented;
            #[cfg(test)]
            #[path = "support/helpers.rs"]
            mod helpers;
            #[cfg(all(test, feature = "x"))]
            fn helper(a: u8) -> bool { a != 1 }
            #[test]
            fn a_test() { let a = 1; if a == 2 { panic!() } }
            #[tokio::test]
            async fn async_test() { let a = 1; if a == 3 { panic!() } }
            #[cfg(not(test))]
            fn real(a: u8) -> bool { a == 100 }
            #[cfg(feature = "test-utils")]
            fn also_real(a: u8) -> bool { a == 101 }
            fn pat(a: u8) -> bool { matches!(a, 1..=2) || match a { 0..=9 => a == 102, _ => false } }
        "#;
    let found = found(src, 0).unwrap();
    let texts: Vec<&str> = found.sites.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(
        texts,
        [
            "a == 100",
            "a == 101",
            "matches!(a, 1..=2) || match a { 0..=9 => a == 102, _ => false }",
            "0..=9 => ..",
            "a == 102"
        ]
    );
    assert_eq!(
        found.test_modules,
        [
            TestModule {
                name: "more_tests".into(),
                path: None
            },
            TestModule {
                name: "documented".into(),
                path: None
            },
            TestModule {
                name: "helpers".into(),
                path: Some("support/helpers.rs".into())
            },
        ]
    );
}

#[test]
fn deletable_statements() {
    let src = r#"
            async fn f(s: &mut S) -> Result<(), E> {
                s.validate()?;
                s.total += 1;
                s.flag = true;
                helper(s);
                s.flush().await?;
                s.sync().await;
                let x = s.get();
                println!("{x}");
                if x > 0 { return Ok(()); }
                s.done()
            }
        "#;
    let texts: Vec<String> = found(src, 0)
        .unwrap()
        .sites
        .into_iter()
        .filter(|s| s.kind == SiteKind::Stmt)
        .map(|s| s.text)
        .collect();
    assert_eq!(
        texts,
        [
            "s.validate()?;",
            "s.total += 1;",
            "s.flag = true;",
            "helper(s);",
            "s.flush().await?;",
            "s.sync().await;"
        ]
    );
}

#[test]
fn maps_offsets_back_and_blames_the_innermost_site() {
    let src = "fn f(a: u8, b: u8) -> bool { g(a < b); a > 1 }";
    let sites = found(src, 0).unwrap().sites;
    let out = render(src, &sites, &all);
    // Every original character keeps its identity through the mapping.
    for (orig, ch) in src.char_indices().filter(|(_, c)| c.is_alphanumeric()) {
        let hits: Vec<usize> = out
            .text
            .char_indices()
            .filter(|(o, c)| *c == ch && out.to_original(*o) == orig)
            .map(|(o, _)| o)
            .collect();
        assert!(
            !hits.is_empty(),
            "original offset {orig} ({ch}) is unreachable"
        );
    }
    // An error inside the probe of `a < b` blames it, not the enclosing statement.
    let probe = out.text.find("ord(1u32").unwrap();
    let orig = out.to_original(probe);
    assert_eq!(blame(&sites, &all, orig), [1]);
    // With the inner site disabled, the statement is next.
    assert_eq!(blame(&sites, &|id| id != 1, orig), [0]);
    assert_eq!(
        blame(&sites, &all, src.find("fn").unwrap()),
        Vec::<u32>::new()
    );
    assert_eq!(out.to_original(0), 0);
    assert_eq!(out.to_original(out.text.len()), src.len());
}

#[test]
fn blame_is_inclusive_at_both_ends() {
    let src = "fn f(a: u8) -> bool { a > 1 }";
    let sites = found(src, 0).unwrap().sites;
    let (start, end) = (sites[0].start, sites[0].end);
    assert_eq!(blame(&sites, &all, start - 1), Vec::<u32>::new());
    assert_eq!(blame(&sites, &all, start), [0]);
    assert_eq!(blame(&sites, &all, end), [0]);
    assert_eq!(blame(&sites, &all, end + 1), Vec::<u32>::new());
    // The probe opens before the first original character and closes after the last.
    let out = render(src, &sites, &all);
    assert_eq!(out.to_original(out.text.find("(match").unwrap()), start);
    assert_eq!(
        out.to_original(out.text.find("tmt_val()) } })").unwrap()),
        end
    );
    assert_eq!(
        out.to_original(out.text.find("), &(").unwrap() + 2),
        sites[0].start + 1
    );
}

#[test]
fn finds_sites_in_impls_and_array_elements() {
    let src = r#"
            struct T(u8);
            impl T { fn small(&self) -> bool { self.0 < 3 } }
            #[cfg(test)]
            impl T { fn only_in_tests(&self) -> bool { self.0 < 4 } }
            trait Tr { fn provided(&self, a: u8) -> bool { a < 5 } }
            fn arr(a: u8) -> [bool; 2] { [a < 6; 2] }
            fn closure(a: u8) -> bool { (|| a < 7)() }
        "#;
    let texts: Vec<String> = found(src, 0)
        .unwrap()
        .sites
        .into_iter()
        .map(|s| s.text)
        .collect();
    assert_eq!(texts, ["self.0 < 3", "a < 5", "a < 6", "a < 7"]);
}

#[test]
fn logic_and_arithmetic_sites() {
    let src = "fn f(a: bool, b: bool, x: u32, y: u32) -> u32 { if a && b || !a { x + y * 2 } else { x - 1 % y } }";
    let found = found(src, 0).unwrap().sites;
    let kinds: Vec<String> = found
        .iter()
        .map(|s| match &s.kind {
            SiteKind::Logic { and, lhs, rhs, .. } => format!("logic and={and} [{lhs}] [{rhs}]"),
            SiteKind::Arith { op, lhs, rhs, .. } => {
                format!("arith {} [{lhs}] [{rhs}]", op.symbol())
            }
            SiteKind::Not { operand, .. } => format!("not [{operand}]"),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "logic and=false [a && b] [!a]",
            "logic and=true [a] [b]",
            "not [a]",
            "arith + [x] [y * 2]",
            "arith * [y] [2]",
            "arith - [x] [1 % y]",
            "arith % [1] [y]",
        ]
    );
    let out = render(src, &found, &all).text;
    syn::parse_file(&out).expect("instrumented source parses");
    assert!(
            out.contains("(match { let __tmt_c: u8 = crate::__tmt_rt::lhs(1u32, true, a); __tmt_c } { 2u8 => crate::__tmt_rt::rhs(1u32, true, b), __tmt_c => __tmt_c == 1u8 })"),
            "{out}"
        );
    assert!(
            out.contains("(match (y, 2) { (__tmt_a, __tmt_b) => { #[allow(unused_imports)] use crate::__tmt_rt::{AnyMul as _, ArithPrim as _}; (&crate::__tmt_rt::Ar::new(__tmt_a, __tmt_b)).tmt_mul(4u32) } })"),
            "{out}"
        );
    // Every probe the renderer can emit exists in the runtime under that name.
    for op in ArithOp::ALL {
        assert_ne!(op.swapped(), op);
        for assign in [false, true] {
            let method = op.method(assign);
            let fallback = format!("{} {method} ", op.fallback_trait(assign));
            assert!(
                crate::RUNTIME_SOURCE.contains(&format!("fn {method}(")),
                "{method}"
            );
            assert!(crate::RUNTIME_SOURCE.contains(&fallback), "{fallback}");
            assert!(
                crate::RUNTIME_SOURCE.contains(&format!("trait {} ", op.prim_trait(assign))),
                "{op:?}"
            );
        }
    }
}

#[test]
fn let_chains_and_literal_arithmetic_are_left_alone() {
    let src = r#"
            fn f(o: Option<u8>, a: bool, n: u32) -> u32 {
                if let Some(x) = o && a && x > 1 { return 1; }
                let k = 60 * 60;
                (n & 3) | (n << 1) ^ k
            }
        "#;
    let texts: Vec<String> = found(src, 0)
        .unwrap()
        .sites
        .into_iter()
        .map(|s| s.text)
        .collect();
    assert_eq!(
        texts,
        [
            "x > 1",
            "(n & 3) | (n << 1) ^ k",
            "n & 3",
            "(n << 1) ^ k",
            "n << 1"
        ]
    );
}

#[test]
fn attributes_const_methods_and_let_on_the_right() {
    let src = r#"
            struct T(u8);
            impl T {
                const fn in_const(&self) -> bool { self.0 < 1 }
                #[test]
                fn test_method(&self) -> bool { self.0 < 2 }
                fn real(&self) -> bool { self.0 < 3 }
            }
            trait Tr {
                #[cfg(test)]
                fn only_in_tests(&self, a: u8) -> bool { a < 4 }
            }
            fn f(s: &mut S, o: Option<u8>, a: bool) -> Result<(), E> {
                #[cfg(feature = "x")]
                s.guarded();
                #[allow(unused)]
                s.total += 1;
                #[allow(unused)]
                s.check()?;
                #[allow(unused)]
                s.sync().await;
                s.plain();
                if a && let Some(v) = o { s.use_it(v); }
                Ok(())
            }
        "#;
    let texts: Vec<String> = found(src, 0)
        .unwrap()
        .sites
        .into_iter()
        .map(|s| s.text)
        .collect();
    // syn attaches the attribute of `#[a] x += 1;` to the left operand, so that statement
    // is guarded together with its attribute; the guard is valid around it either way.
    assert_eq!(
        texts,
        [
            "self.0 < 3",
            "#[allow(unused)]\n                s.total += 1;",
            "s.plain();",
            "#[allow(unused)]\n                s.total += 1",
            "s.use_it(v);"
        ]
    );
    let found = found(src, 0).unwrap().sites;
    syn::parse_file(&render(src, &found, &all).text).expect("instrumented source parses");

    // Conditional or skip attributes on assignments attach to the left operand in syn 2
    // and must not produce an empty `if live(id) {}` statement wrapper.
    let guarded = r#"
        fn g(s: &mut S, x: &mut u32, mut a: u32, mut b: u32) {
            #[cfg(feature = "off")]
            s.total += 1;
            #[mutants::skip]
            *x = 2;
            #[cfg_attr(test, allow(unused))]
            *x += 3;
            #[cfg(feature = "off")]
            (a, b) = (1, 2);
            #[cfg(feature = "off")]
            [a, b] = [1, 2];
            s.plain();
        }
    "#;
    let guarded_stmts: Vec<String> = discover(guarded, 0)
        .unwrap()
        .sites
        .into_iter()
        .filter(|s| s.kind == SiteKind::Stmt)
        .map(|s| s.text)
        .collect();
    assert_eq!(guarded_stmts, ["s.plain();"]);
}

#[test]
fn compound_assignment_negation_and_bit_operators() {
    let src = "fn f(total: &mut u32, n: u32, ok: bool, m: Mask) -> u32 { *total += n; *total <<= 1; if !ok { return !n; } let _ = !m; n & 7 }";
    let sites = found(src, 0).unwrap().sites;
    let kinds: Vec<String> = sites
        .iter()
        .map(|s| match &s.kind {
            SiteKind::Arith {
                op,
                assign,
                lhs,
                rhs,
                ..
            } => format!(
                "{lhs} {}{} {rhs}",
                op.symbol(),
                if *assign { "=" } else { "" }
            ),
            SiteKind::Not { operand, .. } => format!("!{operand}"),
            SiteKind::Stmt => "stmt".to_string(),
            other => format!("{other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "stmt",
            "stmt",
            "*total += n",
            "*total <<= 1",
            "!ok",
            "!n",
            "!m",
            "n & 7"
        ]
    );
    let out = render(src, &sites, &all).text;
    syn::parse_file(&out).expect("instrumented source parses");
    for expected in [
        "(match (&mut (*total), n) { (__tmt_a, __tmt_b) => { #[allow(unused_imports)] use crate::__tmt_rt::{AnyAddAssign as _, ArithAssignPrim as _}; (&crate::__tmt_rt::Aa::new(__tmt_a, __tmt_b)).tmt_add_assign(2u32) } });",
        "use crate::__tmt_rt::{AnyShlAssign as _, BitAssignPrim as _}; (&crate::__tmt_rt::Aa::new(__tmt_a, __tmt_b)).tmt_shl_assign(3u32) } });",
        "if (match (ok) { __tmt_a => { #[allow(unused_imports)] use crate::__tmt_rt::{AnyNot as _, NotPrim as _}; (&crate::__tmt_rt::Un::new(__tmt_a)).tmt_not(4u32) } }) {",
        "(match (n, 7) { (__tmt_a, __tmt_b) => { #[allow(unused_imports)] use crate::__tmt_rt::{AnyBitAnd as _, BitPrim as _}; (&crate::__tmt_rt::Ar::new(__tmt_a, __tmt_b)).tmt_bitand(7u32) } })",
    ] {
        assert!(out.contains(expected), "missing {expected}\nin {out}");
    }
    // `!a && b`: the negation and the conjunction start at the same character.
    let nested = "fn f(a: bool, b: bool) -> bool { !a && b }";
    let sites = found(nested, 0).unwrap().sites;
    let out = render(nested, &sites, &all).text;
    syn::parse_file(&out).expect("instrumented source parses");
    assert!(
        out.contains(
            "(match { let __tmt_c: u8 = crate::__tmt_rt::lhs(0u32, true, (match (a) { __tmt_a => {"
        ),
        "{out}"
    );
    // Names the probes introduce are never in scope where user code runs.
    let capture =
        "fn f(a: bool, __tmt_c: u8, __tmt_a: u8) -> bool { a && __tmt_c == 2 && __tmt_a + 1 > 0 }";
    let sites = found(capture, 0).unwrap().sites;
    let out = render(capture, &sites, &all).text;
    syn::parse_file(&out).expect("instrumented source parses");
    // The arms that bind a probe variable contain only probe code.
    for arm in out.split("__tmt_c => ").skip(1) {
        assert!(arm.starts_with("__tmt_c == 1u8 })"), "{arm}");
    }
    for arm in out.split("(__tmt_a, __tmt_b) => ").skip(1) {
        assert!(
            arm.starts_with("{ #[allow(unused_imports)] use crate::__tmt_rt::"),
            "{arm}"
        );
    }
}

#[test]
fn compound_assignment_is_probed_only_on_plain_places() {
    let src = r#"
            fn f(s: &mut S, v: &mut [u32], p: &mut u32, i: usize) {
                let mut local = 0u32;
                local += 1;
                s.count += 1;
                s.inner.total -= 2;
                *p *= 3;
                (*p) += 4;
                v[i] += 5;
                v[next()] += 6;
                s.get_mut().x += 7;
                *s.slot() += 8;
            }
        "#;
    let probed: Vec<String> = found(src, 0)
        .unwrap()
        .sites
        .into_iter()
        .filter(|s| matches!(s.kind, SiteKind::Arith { assign: true, .. }))
        .map(|s| s.text)
        .collect();
    assert_eq!(
        probed,
        [
            "local += 1",
            "s.count += 1",
            "s.inner.total -= 2",
            "*p *= 3",
            "(*p) += 4"
        ]
    );
    // The others are still statements that can be deleted.
    let deletable = found(src, 0)
        .unwrap()
        .sites
        .iter()
        .filter(|s| s.kind == SiteKind::Stmt)
        .count();
    assert_eq!(deletable, 9);
}

#[test]
fn bool_functions_are_return_sites() {
    let src = r#"
            pub fn is_ok(a: u8) -> bool { let b = a; b > 1 }
            fn count(a: u8) -> u8 { a }
            fn unit(a: u8) { let _ = a; }
            const fn in_const(a: u8) -> bool { a > 2 }
            #[test]
            fn a_test() -> bool { true }
            struct T;
            impl T { fn method(&self) -> bool { true } }
            trait Tr { fn required(&self) -> bool; fn provided(&self) -> bool { false } }
            fn boxed() -> Box<bool> { Box::new(true) }
            fn qualified() -> core::primitive::bool { true }
        "#;
    let sites = discover(src, 0).unwrap().sites;
    let returns: Vec<(&str, &str, usize)> = sites
        .iter()
        .filter_map(|s| match &s.kind {
            SiteKind::Return {
                name, option: None, ..
            } => Some((name.as_str(), s.text.as_str(), s.line)),
            _ => None,
        })
        .collect();
    assert_eq!(
        returns,
        [
            ("is_ok", "fn is_ok(a: u8) -> bool", 2),
            ("method", "fn method(&self) -> bool", 9),
            ("provided", "fn provided(&self) -> bool", 10),
        ]
    );
    let out = render(src, &sites, &all).text;
    syn::parse_file(&out).expect("instrumented source parses");
    assert!(
        out.contains(
            "pub fn is_ok(a: u8) -> bool { let __tmt_v: bool = { \
                 if !crate::__tmt_rt::live(1u32) { return true; } \
                 if !crate::__tmt_rt::live(2u32) { return false; } let b = a; (match"
        ),
        "{out}"
    );
    assert!(
        out.contains("}) }; crate::__tmt_rt::ret(0u32, __tmt_v) }"),
        "{out}"
    );
    assert_eq!(
        out.lines().count(),
        src.lines().count(),
        "line numbers are preserved"
    );
    // A compile error in the wrapper is blamed on the function's site.
    let rendered = render(src, &sites, &all);
    let wrapper = rendered.text.find("ret(0u32").unwrap();
    assert_eq!(rendered.inserted_by(wrapper), Some(0));
    // And one in a replaced body on that replacement alone: by position they are
    // all the same place.
    let replaced = rendered.text.find("return false; } let b").unwrap();
    assert_eq!(rendered.inserted_by(replaced), Some(2));
    assert_eq!(blame(&sites, &all, rendered.to_original(replaced)), [0]);
}

#[test]
fn ids_are_consecutive_across_every_kind_of_site() {
    let src = r#"
            fn a(x: u8) -> bool { x > 1 }
            fn b(x: u8) -> bool { !(x > 2) && !!flag(x) }
            fn c(s: &mut S) { s.n += 1; s.go(); }
        "#;
    let sites = discover(src, 10).unwrap().sites;
    let ids: Vec<u32> = sites.iter().map(|s| s.id).collect();
    assert_eq!(ids, (10..10 + sites.len() as u32).collect::<Vec<_>>());
    let texts: Vec<&str> = sites
        .iter()
        .filter(|s| !matches!(s.kind, SiteKind::Body { .. }))
        .map(|s| s.text.as_str())
        .collect();
    assert_eq!(
        texts,
        [
            "fn a(x: u8) -> bool",
            "x > 1",
            "fn b(x: u8) -> bool",
            "!(x > 2) && !!flag(x)",
            "!(x > 2)",
            "x > 2",
            "!!flag(x)",
            "!flag(x)",
            "s.n += 1;",
            "s.go();",
            "s.n += 1",
        ]
    );
    syn::parse_file(&render(src, &sites, &all).text).expect("instrumented source parses");
}

#[test]
fn only_calls_and_assignments_are_deletable() {
    let src = r#"
            async fn f(s: &mut S, v: Vec<u8>) -> Result<(), E> {
                s.field;
                v[0];
                (s.a, s.b);
                s.value?;
                s.future.await;
                s.x = 1;
                s.x -= 1;
                s.x == 1;
                s.call().await?;
                Ok(())
            }
        "#;
    let texts: Vec<String> = found(src, 0)
        .unwrap()
        .sites
        .into_iter()
        .filter(|s| s.kind == SiteKind::Stmt)
        .map(|s| s.text)
        .collect();
    assert_eq!(texts, ["s.x = 1;", "s.x -= 1;", "s.call().await?;"]);
}

#[test]
fn ranges_match_arms_min_max_and_option_results() {
    let src = r#"
            fn find(v: &[u32], limit: u32) -> Option<u32> {
                for i in 0..v.len() {
                    let capped = v[i].min(limit).max(1);
                    match capped {
                        0 => return None,
                        n if n > 5 => return Some(n),
                        1..=3 => continue,
                        other => { let _ = other; }
                    }
                }
                let _ = &v[1..=2];
                for _ in 1..=limit as usize {}
                let _ = 'a'..'z';
                let _ = v.iter().max();
                None
            }
            fn exhaustive(k: Kind) -> u8 { match k { Kind::A => 1, Kind::B => 2 } }
            fn hidden() -> Option<impl Sized> { Some(1) }
            fn first(v: &[u32]) -> Option<u32> { v.first().copied() }
        "#;
    let sites = discover(src, 0).unwrap().sites;
    let kinds: Vec<String> = sites
        .iter()
        .filter_map(|s| match &s.kind {
            SiteKind::Return { name, option, .. } => Some(format!("return {name} {option:?}")),
            SiteKind::Range {
                inclusive,
                lhs,
                rhs,
                ..
            } => Some(format!("range {lhs} {rhs} inclusive={inclusive}")),
            SiteKind::Arm { pattern, guard, .. } => Some(format!(
                "arm {pattern} guarded={}",
                matches!(guard, ArmGuard::Existing(..))
            )),
            SiteKind::MinMax {
                max,
                receiver,
                argument,
                ..
            } => Some(format!("minmax {receiver} {argument} max={max}")),
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "range 0 v.len() inclusive=false",
            "minmax v[i].min(limit) 1 max=true",
            "minmax v[i] limit max=false",
            // The catch-all arm and the last arm stay; a match without one is left alone.
            "arm 0 guarded=false",
            "arm n if n > 5 guarded=true",
            "arm 1..=3 guarded=false",
            // `1..=2` and `'a'..'z'` have literals at both ends.
            "range 1 limit as usize inclusive=true",
            "return first Some(\"Option<u32>\")",
        ]
    );
    let out = render(src, &sites, &all).text;
    syn::parse_file(&out).expect("instrumented source parses");
    assert_eq!(out.lines().count(), src.lines().count());
    let option = sites
        .iter()
        .find(|site| matches!(&site.kind, SiteKind::Return { name, .. } if name == "first"))
        .unwrap();
    assert_eq!(out.matches("let __tmt_v: Option<u32> = {").count(), 1);
    assert!(
        out.contains(&format!(
            "crate::__tmt_rt::ret_opt({}u32, __tmt_v)",
            option.id
        )),
        "{out}"
    );
    assert!(out.contains("0 if crate::__tmt_rt::arm("), "{out}");
    assert!(
        out.contains("n if crate::__tmt_rt::arm(") && out.contains(") && ("),
        "{out}"
    );
    assert!(out.contains("1..=3 if crate::__tmt_rt::arm("), "{out}");
    assert!(
        out.contains(".tmt_range(") && out.contains(".tmt_range_inclusive("),
        "{out}"
    );
    // The call that was written, and the other one for the mutant, on the operands
    // as they were evaluated once; the receiver is borrowed, not moved.
    assert!(
            out.contains("(match (&(v[i]), limit) {")
                && out.contains(
                    "Mm(__tmt_a, &__tmt_b)).tmt_other(5u32) { (*__tmt_a).max(__tmt_b) } else { (*__tmt_a).min(__tmt_b) }"
                )
                && out.contains(
                    ".tmt_other(4u32) { (*__tmt_a).min(__tmt_b) } else { (*__tmt_a).max(__tmt_b) }"
                ),
            "{out}"
        );
    assert!(
        !out.contains(".min(limit)") && !out.contains(".max(1)"),
        "{out}"
    );
    assert!(
        out.contains("v.iter().max()"),
        "an iterator's max is another method: {out}"
    );
}

#[test]
fn macro_arguments() {
    let src = r#"
            fn f(a: u32, b: u32, out: &mut String) -> Vec<u32> {
                assert!(a < 100);
                debug_assert!(a < 101, "message {}", a < 102);
                assert_eq!(a < 1, b < 2, "context {}", a + 3);
                let _ = format!("{} {name}", a < 4, name = b < 5);
                write!(out, "{}", a < 6).unwrap();
                if matches!(a.cmp(&b), std::cmp::Ordering::Less if a < 7) || matches!(a < 8, true) { panic!("{}", a < 9); }
                custom!(a < 10);
                let _ = vec![a < 11; 2];
                vec![a + b, (a < 12) as u32]
            }
        "#;
    let texts: Vec<String> = found(src, 0)
        .unwrap()
        .sites
        .into_iter()
        .filter(|s| matches!(s.kind, SiteKind::Cmp { .. } | SiteKind::Arith { .. }))
        .map(|s| s.text)
        .collect();
    assert_eq!(
        texts,
        [
            "a < 1", "b < 2", "a + 3", "a < 4", "b < 5", "a < 6", "a < 8", "a < 9", "a + b",
            "a < 12"
        ]
    );
    let found = found(src, 0).unwrap().sites;
    syn::parse_file(&render(src, &found, &all).text).expect("instrumented source parses");
}

#[test]
fn non_ascii_source_keeps_byte_offsets() {
    let src = "fn f(a: u8) -> bool { let s = \"日本語\"; s.len() > 3 && a < 2 }";
    let sites = found(src, 0).unwrap().sites;
    let texts: Vec<&str> = sites.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(texts, ["s.len() > 3 && a < 2", "s.len() > 3", "a < 2"]);
    syn::parse_file(&render(src, &sites, &all).text).unwrap();
}

#[test]
fn unparseable_source_is_an_error() {
    assert!(discover("fn f( {", 0).is_err());
}
/// Every site as `kind text`, in discovery order, without the replaced bodies.
fn labelled(src: &str) -> Vec<String> {
    discover(src, 0)
        .unwrap()
        .sites
        .iter()
        .filter(|s| !matches!(s.kind, SiteKind::Body { .. }))
        .map(|s| {
            let kind = match &s.kind {
                SiteKind::Cmp { .. } => "cmp",
                SiteKind::Logic { .. } => "logic",
                SiteKind::Arith { .. } => "arith",
                SiteKind::Not { .. } => "not",
                SiteKind::Return { .. } => "return",
                SiteKind::MinMax { .. } => "minmax",
                SiteKind::SliceEnd { .. } => "end",
                SiteKind::StrAffix { .. } => "straffix",
                SiteKind::StrReplace { .. } => "strreplace",
                SiteKind::Clamp { .. } => "clamp",
                SiteKind::Rounding { .. } => "rounding",
                SiteKind::Quantifier { .. } => "quantifier",
                SiteKind::LiteralSet { .. } => "literalset",
                SiteKind::Range { .. } => "range",
                SiteKind::Arm { .. } => "arm",
                SiteKind::Stmt => "stmt",
                SiteKind::Neg { .. } => "neg",
                SiteKind::Guard { .. } => "guard",
                SiteKind::Field { .. } => "field",
                SiteKind::Body { .. } => "body",
            };
            format!("{kind} {}", s.text)
        })
        .collect()
}

#[test]
fn byte_order_mark_and_interpreter_line_are_refused() {
    let body = "fn f(a: u8) -> bool { a > 1 }";
    assert!(discover(&format!("\u{feff}{body}"), 0).is_err());
    assert!(discover(&format!("#!/usr/bin/env run\n{body}"), 0).is_err());
    // An inner attribute starts with the same two characters and is fine.
    assert_eq!(
        labelled(&format!("#![allow(dead_code)]\n{body}")),
        ["return fn f(a: u8) -> bool", "cmp a > 1"]
    );
}

#[test]
fn which_result_types_make_a_return_site() {
    let src = "fn a() -> <S as Tr>::Option<u8> { g() }\n\
                   fn b() -> Option { g() }\n\
                   fn c() -> Vec<u8> { g() }\n\
                   fn d() -> std::option::Option<u8> { g() }\n\
                   fn e() -> <S>::bool { g() }\n\
                   fn e2() -> <S as Tr>::bool { g() }\n\
                   fn one(a: u8) -> bool { a > 1 }\n\
                   fn two(a: u8) -> bool { let b = a; b > 1 }\n\
                   fn early(a: u8) -> bool { return a > 1; }\n";
    let returns: Vec<(String, Option<String>, Option<&str>)> = discover(src, 0)
        .unwrap()
        .sites
        .into_iter()
        .filter_map(|s| match s.kind {
            SiteKind::Return {
                name,
                option,
                sole_expr,
            } => Some((name, option, sole_expr.map(|(from, to)| &src[from..to]))),
            _ => None,
        })
        .collect();
    assert_eq!(
        returns,
        [
            (
                "d".to_string(),
                Some("std::option::Option<u8>".to_string()),
                Some("g()")
            ),
            // Only a body that is one expression has a "sole expression".
            ("one".to_string(), None, Some("a > 1")),
            ("two".to_string(), None, None),
        ]
    );
}

#[test]
fn places_chains_attributes_and_shadowed_macros() {
    // A parenthesised variable is still a plain place. An attribute on a statement
    // belongs to its leftmost operand, which is then not probed; the statement can
    // still be deleted.
    assert_eq!(
        labelled("fn f(mut x: i32) -> i32 { #[allow(unused)] x += 1; (x) += 2; x }"),
        [
            "stmt #[allow(unused)] x += 1;",
            "stmt (x) += 2;",
            "arith (x) += 2"
        ]
    );
    // `cond && let ..` is a let chain as much as `let .. && cond`.
    assert_eq!(
        labelled("fn f(a: u8, o: Option<u8>) { if a > 1 && let Some(b) = o { g(b); } }"),
        ["cmp a > 1", "stmt g(b);"]
    );
    // And so is everything joined to one.
    assert_eq!(
        labelled("fn f(a: u8, o: Option<u8>) { if a > 1 && let Some(b) = o && b > 2 { g(b); } }"),
        ["cmp a > 1", "cmp b > 2", "stmt g(b);"]
    );
    // A macro of this file under a standard name takes anything as its arguments.
    assert_eq!(
        labelled(
            "macro_rules! vec { ($($t:tt)*) => {} }\n\
                 fn f(a: u8) { let _ = vec![a > 1]; let _ = format!(\"{}\", a > 2); }"
        ),
        ["cmp a > 2"]
    );
    assert_eq!(
        labelled("trait T { const fn f(a: u8) -> bool { a > 1 } fn g(a: u8) -> bool { a > 2 } }"),
        ["return fn g(a: u8) -> bool", "cmp a > 2"]
    );
}

#[test]
fn first_and_last_of_what_may_be_a_slice() {
    assert_eq!(
        labelled(
            "fn f(v: &[u8], s: &S) { let _ = v.first(); let _ = s.items.last(); \
                 let _ = s.all().last(); let _ = (v).first().copied(); }"
        ),
        [
            "end v.first()",
            "end s.items.last()",
            "end s.all().last()",
            "end (v).first()"
        ]
    );
    // With an argument, a type or an attribute it is not the method of a slice, and
    // the last of an iterator is found by running through it.
    assert_eq!(
        labelled(
            "fn f(v: &[u8], t: &T) { let _ = t.first(1); let _ = t.last::<u8>(); \
                 let _ = #[allow(unused)] v.last(); let _ = v.iter().last(); \
                 let _ = (v.iter().map(g)).last(); let _ = t.name.split('/').last(); \
                 let _ = v.first_mut(); }"
        ),
        [] as [&str; 0]
    );
    let src = "fn f(v: &[u8]) -> Option<&u8> { v.last() }";
    let found = discover(src, 0).unwrap();
    let out = render(src, &found.sites, &|_| true).text;
    // A variable is named again for the call.
    assert!(out.contains("({ #[allow(unused_imports)] use"), "{out}");
    assert!(
        out.contains("match (&crate::__tmt_rt::Ends(&(v))).tmt_other_end("),
        "{out}"
    );
    assert_eq!(out.matches("(v)").count(), 4, "{out}");
    assert!(out.contains(", false) {"), "{out}");
    assert!(
            out.contains("End::Other => { let __tmt_o = (v).first(); if false { crate::__tmt_rt::same_type(&__tmt_o, &(v).last()); } __tmt_o }, crate::__tmt_rt::End::Same(__tmt_s) => { let __tmt_r = (v).last(); __tmt_s.tmt_end_seen("),
            "{out}"
        );
    // Anything else is evaluated once, and a line that ends in the call still ends.
    let src = "fn f(s: &S) -> Option<&u8> { s.all()\n  .first() }";
    let found = discover(src, 0).unwrap();
    let out = render(src, &found.sites, &|_| true).text;
    assert!(out.contains("(match (&(s.all())) { __tmt_a =>"), "{out}");
    assert!(
        out.contains("End::Other => { let __tmt_o = (*__tmt_a).last();"),
        "{out}"
    );
    assert!(out.contains("let __tmt_r = (*__tmt_a).first();"), "{out}");
    assert!(out.contains("__tmt_r } } } })\n }"), "{out}");
    assert_eq!(out.matches('\n').count(), 1, "{out}");
    let src = "fn f(v: &[u8]) -> Option<&u8> { v.last() }";
    let found = discover(src, 0).unwrap();
    // Written into the source, only the name of the method changes.
    let mutants: Vec<_> = found.sites.iter().flat_map(source_mutants).collect();
    let end = mutants.iter().find(|m| m.what == "last").unwrap();
    assert_eq!(
        (
            &src[end.start..end.end],
            end.replacement.as_str(),
            end.into.as_str()
        ),
        ("last", "first", "first")
    );
}

#[test]
fn min_max_and_ranges_that_are_left_alone() {
    assert_eq!(
        labelled(
            "fn f(a: u8, b: u8) -> u8 { let _ = a.max::<u8>(b); \
                 let _ = #[allow(unused)] a.min(b); let _ = a.max(b, 1); a.max(b) }"
        ),
        ["minmax a.max(b)"]
    );
    // One literal end is enough to fix the type; the ends are searched for sites.
    let src = "fn f(a: usize, b: usize, v: &[u8]) { for _ in a..10 {} for _ in 1..b {} \
                   for _ in (a + 1)..(b * 2) {} for _ in 3..4 {} for _ in (5)..(-6) {} }";
    assert_eq!(
        labelled(src),
        [
            "range a..10",
            "range 1..b",
            "range (a + 1)..(b * 2)",
            "arith a + 1",
            "arith b * 2",
            // Two literal ends are no range site; the minus in one is still a minus.
            "neg -6"
        ]
    );
    let out = render(src, &discover(src, 0).unwrap().sites, &all).text;
    // The `..` itself is gone: the ends are the two arguments of the probe.
    assert!(out.contains("for _ in (match (a, 10) {"), "{out}");
    assert!(!out.contains("a..10") && !out.contains("1..b"), "{out}");
    assert!(out.contains("3..4") && out.contains("(5)..((if "), "{out}");
}

#[test]
fn which_arms_can_be_removed() {
    let src = "fn f(k: u8) -> u8 {\n\
                   \x20   let a = match k { 1 => 1, x => x, 2 => 2, #[cfg(any())] 3 => 3, _rest => 0 };\n\
                   \x20   let b = match k { 1 => 1, NONE => 0 };\n\
                   \x20   let c = match k { 1 => 1, n @ 0..=9 => n };\n\
                   \x20   let d = match k {\n\
                   \x20       1 => 1,\n\
                   \x20       _ => 0,\n\
                   \x20   };\n\
                   \x20   a + b + c + d\n\
                   }\n";
    let arms: Vec<(u32, usize, usize, String)> = discover(src, 7)
        .unwrap()
        .sites
        .into_iter()
        .filter(|s| matches!(s.kind, SiteKind::Arm { .. }))
        .map(|s| (s.id, s.line, s.column, s.text))
        .collect();
    // Neither a catch-all arm in the middle nor an arm with an attribute; and no arm
    // at all when the last one is a constant or binds only part of the values.
    assert_eq!(
        arms,
        [
            // After the two replaced bodies of a function that returns `u8`.
            (9, 2, 23, "1 => ..".to_string()),
            (10, 2, 39, "2 => ..".to_string()),
            (11, 6, 9, "1 => ..".to_string()),
        ]
    );
}

#[test]
fn offsets_around_inserted_text() {
    let src = "fn f(a: u8) -> bool { a > 1 }";
    let sites = found(src, 0).unwrap().sites;
    let out = render(src, &sites, &all);
    let start = sites[0].start;
    // `a` is copied right after the opening text of the probe: the last inserted
    // character before it still maps to the point of insertion.
    let copied_a = out.text.find("a), &(").unwrap();
    assert_eq!(out.to_original(copied_a), start);
    assert_eq!(out.to_original(copied_a - 1), start);
    assert_eq!(out.to_original(copied_a + 1), start + 1);
    // Past the end is the end.
    assert_eq!(out.to_original(out.text.len() + 10), src.len());

    // The probe of `b + 1` opens right after the replaced `..`: an error there is
    // inside `b + 1`, not in front of the `..`.
    let src = "fn f(a: usize, b: usize) { for _ in a..b + 1 {} }";
    let sites = discover(src, 0).unwrap().sites;
    let out = render(src, &sites, &all);
    let inner_probe = out.text.find("(match (a, ").unwrap() + "(match (a, ".len();
    assert!(
        out.text[inner_probe..].starts_with("(match (b, 1)"),
        "{}",
        out.text
    );
    assert_eq!(out.to_original(inner_probe), src.find("b + 1").unwrap());
    let arith = sites
        .iter()
        .find(|s| matches!(s.kind, SiteKind::Arith { .. }))
        .unwrap();
    assert_eq!(
        blame(&sites, &all, out.to_original(inner_probe)),
        [arith.id]
    );
    // The text of a probe knows its site without going through the original.
    assert_eq!(out.inserted_by(inner_probe), Some(arith.id));
    assert_eq!(out.inserted_by(out.text.find("b, 1").unwrap()), None);
    // From its first to its last byte, and not one further.
    let opening = "(match (b, ";
    let (first, last) = (inner_probe, inner_probe + "(match (".len() - 1);
    assert!(out.text[inner_probe..].starts_with(opening));
    assert_eq!(out.inserted_by(first), Some(arith.id));
    assert_eq!(out.inserted_by(last), Some(arith.id));
    assert_eq!(out.inserted_by(last + 1), None);
    assert_eq!(out.inserted_by(0), None);
}
fn values(ty: &str) -> Vec<String> {
    let function: syn::ItemFn = syn::parse_str(&format!("fn f() -> {ty} {{ todo!() }}"))
        .unwrap_or_else(|e| panic!("{ty}: {e}"));
    body_values(&function.sig.output)
}

/// The table of cargo-mutants (https://mutants.rs/mutants.html), row by row.
#[test]
fn replacement_values_by_return_type() {
    let rows: &[(&str, &[&str])] = &[
        ("()", &["()"]),
        ("i8", &["0", "1", "-1"]),
        ("isize", &["0", "1", "-1"]),
        ("i128", &["0", "1", "-1"]),
        ("u8", &["0", "1"]),
        ("usize", &["0", "1"]),
        ("u128", &["0", "1"]),
        ("f32", &["0.0", "1.0", "-1.0"]),
        ("f64", &["0.0", "1.0", "-1.0"]),
        ("bool", &["true", "false"]),
        (
            "NonZeroI32",
            &["1.try_into().unwrap()", "(-1).try_into().unwrap()"],
        ),
        ("std::num::NonZeroU8", &["1.try_into().unwrap()"]),
        ("NonZero<u64>", &["1.try_into().unwrap()"]),
        (
            "NonZero<i64>",
            &["1.try_into().unwrap()", "(-1).try_into().unwrap()"],
        ),
        ("String", &["String::new()", "\"xyzzy\".into()"]),
        ("&str", &["\"\"", "\"xyzzy\""]),
        ("&'static str", &["\"\"", "\"xyzzy\""]),
        // A `str` that can be written to is not a literal.
        ("&mut str", &["Box::leak(Box::new(Default::default()))"]),
        ("&u8", &["Box::leak(Box::new(0))", "Box::leak(Box::new(1))"]),
        (
            "&mut bool",
            &["Box::leak(Box::new(true))", "Box::leak(Box::new(false))"],
        ),
        (
            "&[u8]",
            &[
                "Vec::leak(Vec::new())",
                "Vec::leak(vec![0])",
                "Vec::leak(vec![1])",
            ],
        ),
        (
            "&mut [bool]",
            &[
                "Vec::leak(Vec::new())",
                "Vec::leak(vec![true])",
                "Vec::leak(vec![false])",
            ],
        ),
        ("Result<bool, Error>", &["Ok(true)", "Ok(false)"]),
        ("io::Result<()>", &["Ok(())"]),
        // Without arguments it is an alias: `fmt::Result`, or the crate's own.
        ("fmt::Result", &["Ok(Default::default())"]),
        ("Result", &["Ok(Default::default())"]),
        ("Option<u8>", &["None", "Some(0)", "Some(1)"]),
        ("Vec<bool>", &["vec![]", "vec![true]", "vec![false]"]),
        ("Box<u8>", &["Box::new(0)", "Box::new(1)"]),
        (
            "std::rc::Rc<u8>",
            &["std::rc::Rc::new(0)", "std::rc::Rc::new(1)"],
        ),
        ("Arc<bool>", &["Arc::new(true)", "Arc::new(false)"]),
        ("RefCell<()>", &["RefCell::new(())"]),
        ("Mutex<()>", &["Mutex::new(())"]),
        ("Cell<()>", &["Cell::new(())"]),
        (
            "HashSet<u8>",
            &[
                "HashSet::new()",
                "HashSet::from_iter([0])",
                "HashSet::from_iter([1])",
            ],
        ),
        (
            "BTreeSet<()>",
            &["BTreeSet::new()", "BTreeSet::from_iter([()])"],
        ),
        (
            "BinaryHeap<()>",
            &["BinaryHeap::new()", "BinaryHeap::from_iter([()])"],
        ),
        (
            "LinkedList<()>",
            &["LinkedList::new()", "LinkedList::from_iter([()])"],
        ),
        (
            "VecDeque<()>",
            &["VecDeque::new()", "VecDeque::from_iter([()])"],
        ),
        (
            "HashMap<bool, u8>",
            &[
                "HashMap::new()",
                "HashMap::from_iter([(true, 0)])",
                "HashMap::from_iter([(true, 1)])",
                "HashMap::from_iter([(false, 0)])",
                "HashMap::from_iter([(false, 1)])",
            ],
        ),
        (
            "std::collections::BTreeMap<(), ()>",
            &[
                "std::collections::BTreeMap::new()",
                "std::collections::BTreeMap::from_iter([((), ())])",
            ],
        ),
        (
            "Cow<'a, str>",
            &[
                "Cow::Borrowed(\"\")",
                "Cow::Owned(\"\".to_owned())",
                "Cow::Borrowed(\"xyzzy\")",
                "Cow::Owned(\"xyzzy\".to_owned())",
            ],
        ),
        ("[u8; 4]", &["[0; 4]", "[1; 4]"]),
        ("[bool; N + 1]", &["[true; N + 1]", "[false; N + 1]"]),
        (
            "(u8, bool)",
            &["(0, true)", "(0, false)", "(1, true)", "(1, false)"],
        ),
        ("(bool,)", &["(true,)", "(false,)"]),
        ("HttpResponse", &["HttpResponse::Ok().finish()"]),
        (
            "actix_web::HttpResponse",
            &["actix_web::HttpResponse::Ok().finish()"],
        ),
        ("Config", &["Default::default()"]),
        ("Self", &["Default::default()"]),
        ("<T as Tr>::Out", &["Default::default()"]),
        // Not the `String` everyone knows.
        ("<T as Tr>::String", &["Default::default()"]),
        ("<T>::bool", &["Default::default()"]),
        ("fn(u8) -> u8", &["Default::default()"]),
        ("*const u8", &["Default::default()"]),
        // Nested: every value of the inner type, wrapped.
        (
            "Result<Option<String>, E>",
            &[
                "Ok(None)",
                "Ok(Some(String::new()))",
                "Ok(Some(\"xyzzy\".into()))",
            ],
        ),
        ("(Vec<()>)", &["vec![]", "vec![()]"]),
        ("Option<Config>", &["None", "Some(Default::default())"]),
        // Nothing can be returned in place of these.
        ("!", &[]),
        ("impl Iterator<Item = u8>", &[]),
        ("Box<impl Fn()>", &[]),
        ("Option<!>", &["None"]),
    ];
    for (ty, expected) in rows {
        assert_eq!(values(ty), *expected, "{ty}");
    }
    // No return type at all is `()`.
    let unit: syn::ItemFn = syn::parse_str("fn f() {}").unwrap();
    assert_eq!(body_values(&unit.sig.output), ["()"]);
}

#[test]
fn which_functions_get_their_body_replaced() {
    let src = r#"
            fn plain(a: u8) -> u8 { a + 1 }
            fn already(a: u8) -> bool { let _ = a; true }
            fn sole() -> bool { true }
            fn spaced() -> (u8, bool) { ( 0 , true ) }
            unsafe fn raw(a: u8) -> u8 { a }
            fn empty() {}
            fn one_call(s: &mut S) { s.go(); }
            fn two_calls(s: &mut S) { s.go(); s.stop(); }
            fn typed(s: &mut S) -> u8 { s.exit(); }
            fn tight() -> bool {true}
            #[rustfmt::skip]
            fn formatted(a: u8) -> u8 { a + 6 }
            #[mutants::timeout(5)]
            fn slow(a: u8) -> u8 { a + 7 }
            impl S { #[mutants::skip] fn marked_method(&self, a: u8) -> u8 { a + 8 } }
            trait U { #[mutants::skip] fn marked_default(&self, a: u8) -> u8 { a + 9 } }
            fn one_let(a: u8) { let _ = a; }
            fn never() -> ! { loop {} }
            fn hidden() -> impl Sized { 1 }
            #[mutants::skip]
            fn marked(a: u8) -> u8 { a + 2 }
            #[cfg_attr(test, mutants::skip)]
            fn marked_in_tests(a: u8) -> u8 { a + 3 }
            #[mutants::skip]
            impl S { fn method(&self, a: u8) -> u8 { a + 4 } }
            #[mutants::skip]
            mod generated { fn inner(a: u8) -> u8 { a + 5 } }
            trait T { fn required(&self) -> u8; fn provided(&self) -> u8 { 7 } }
            async fn later() -> u8 { 9 }
        "#;
    let sites = discover(src, 0).unwrap().sites;
    let bodies: Vec<String> = sites
        .iter()
        .filter_map(|s| match &s.kind {
            SiteKind::Body { name, value, .. } => Some(format!("{name} {value}")),
            _ => None,
        })
        .collect();
    assert_eq!(
        bodies,
        [
            "plain 0",
            "plain 1",
            "already true",
            "already false",
            // The body is this value already, however it is spaced.
            "sole false",
            "spaced (0, false)",
            "spaced (1, true)",
            "spaced (1, false)",
            // One statement that can be deleted is the same mutant as no body.
            "two_calls ()",
            // With a result to return, one statement is not the whole of it.
            "typed 0",
            "typed 1",
            "tight false",
            // Other attributes with `skip` or `mutants` in them do not mark anything.
            "formatted 0",
            "formatted 1",
            "slow 0",
            "slow 1",
            "one_let ()",
            "provided 0",
            "provided 1",
            "later 0",
            "later 1",
        ]
    );
    // Nothing at all is mutated in what is marked to be skipped.
    let texts: Vec<&str> = sites.iter().map(|s| s.text.as_str()).collect();
    for skipped in ["a + 2", "a + 3", "a + 4", "a + 5", "a + 8", "a + 9"] {
        assert!(!texts.contains(&skipped), "{skipped}");
    }
    for mutated in ["a + 1", "a + 6", "a + 7"] {
        assert!(texts.contains(&mutated), "{mutated}");
    }
    let unsafe_sites: Vec<&Site> = sites.iter().filter(|s| s.text.contains("raw")).collect();
    assert!(unsafe_sites.is_empty(), "{unsafe_sites:?}");

    let body = sites
        .iter()
        .find(|s| matches!(s.kind, SiteKind::Body { .. }))
        .unwrap();
    assert_eq!(
        (body.line, body.column, body.text.as_str()),
        (2, 13, "fn plain(a: u8) -> u8")
    );
    let out = render(src, &sites, &all).text;
    syn::parse_file(&out).expect("instrumented source parses");
    assert_eq!(out.lines().count(), src.lines().count());
    assert!(
        out.contains(
            "fn plain(a: u8) -> u8 { if !crate::__tmt_rt::live(0u32) { return 0; } \
                 if !crate::__tmt_rt::live(1u32) { return 1; } (match (a, 1)"
        ),
        "{out}"
    );
    // A body that is exactly its expression: the function-level probes go around
    // the probe of the expression.
    let tight = "fn f(a: u8, b: u8) -> bool {a > b}";
    let sites = discover(tight, 0).unwrap().sites;
    let out = render(tight, &sites, &all).text;
    syn::parse_file(&out).expect("instrumented source parses");
    assert!(
        out.starts_with(
            "fn f(a: u8, b: u8) -> bool { let __tmt_v: bool = { \
                 if !crate::__tmt_rt::live(1u32) { return true; } \
                 if !crate::__tmt_rt::live(2u32) { return false; }(match (&(a), &(b))"
        ),
        "{out}"
    );
    assert!(
        out.ends_with(" })}; crate::__tmt_rt::ret(0u32, __tmt_v) }"),
        "{out}"
    );
}

#[test]
fn a_guard_that_is_written_true_or_false_has_only_the_other_for_a_mutant() {
    // `true` in place of `true` is the program it was made from.
    let src = "fn f(k: u8) -> u8 { match k { 0 if true => 1, 1 if (false) => 2, n if n > 3 => 3, _ => 4 } }";
    let found = discover(src, 0).unwrap();
    let guards = |sites: &[Site]| -> Vec<String> {
        sites
            .iter()
            .filter(|site| matches!(site.kind, SiteKind::Guard { .. }))
            .map(|site| {
                let into: Vec<String> = source_mutants(site).into_iter().map(|m| m.into).collect();
                format!("{} -> {}", site.text, into.join(" "))
            })
            .collect()
    };
    assert_eq!(guards(&found.sites), ["n > 3 -> true"]);
    // Built, not probed: a probe would report the mutant that is the program. The
    // arm with `true` can be removed, which is its guard being `false`.
    assert_eq!(guards(&found.rebuild_sites), ["(false) -> true"]);
    // Where the arm cannot be removed, `false` for a guard written `true` is built.
    let stuck = discover(
        "fn f(k: Option<u8>) -> u8 { match k { Some(0) if true => 1, Some(_) => 2, None => 3 } }",
        0,
    );
    assert_eq!(guards(&stuck.unwrap().rebuild_sites), ["true -> false"]);
    // An operator that binds otherwise is written with its operands, each on the
    // lines it was on.
    let split = discover("fn f(a: bool, b: bool) -> bool {\n    a ||\n    b\n}", 0).unwrap();
    let or = split
        .sites
        .iter()
        .find(|site| matches!(site.kind, SiteKind::Logic { .. }))
        .unwrap();
    assert_eq!(source_mutants(or)[0].replacement, "(a) && \n(b)");
    let mut mutants = source_mutants(&Site {
        id: 0,
        kind: SiteKind::Stmt,
        line: 1,
        column: 1,
        start: 0,
        end: 7,
        text: String::new(),
    });
    keep_line_ends("a(\n1\n);\nb", &mut mutants);
    assert_eq!(mutants[0].replacement, "\n\n");
    // An arm whose guard is written `false` is not there: `false` for its guard is
    // the same program. One whose guard is written `true` can still go.
    let arms: Vec<&str> = found
        .sites
        .iter()
        .chain(&found.rebuild_sites)
        .filter(|site| matches!(site.kind, SiteKind::Arm { .. }))
        .map(|site| site.text.as_str())
        .collect();
    assert_eq!(arms, ["0 if true => ..", "n if n > 3 => .."]);
}

#[test]
fn minus_guards_and_fields() {
    let src = r#"
            fn f(x: i32, y: f64, k: Kind, base: Options) -> Options {
                let a = -x + -5 - -0 + (-0.0 * -y) as i32;
                let b = match k {
                    Kind::A(n) if n > 1 => 1,
                    Kind::A(n) if let Some(m) = half(n) => m,
                    Kind::B if x > 0 && let Some(m) = half(x) => m,
                    #[cfg(any())]
                    Kind::B if x > 2 => 2,
                    Kind::B => 3,
                };
                let c = match b { n if n > 3 => 1, _ => 0 };
                let first = Options { depth: a, name, ..base };
                let second = Options { depth: -b, ..Default::default() };
                let third = Wrap::<u8> { 0: c, ..Wrap::new() };
                let _ = Options { depth: 1, ..base.clone() };
                let _ = Options { depth: 2, ..make(3) };
                let _ = Options { depth: 6, ..(pick())() };
                let _ = -2.5 + y;
                let _ = Options { #[cfg(any())] depth: 4, strict: true, ..*other };
                let _ = Options { depth: 5, name: n, strict: false };
                first
            }
        "#;
    let sites = found(src, 0).unwrap().sites;
    let kinds: Vec<String> = sites
        .iter()
        .filter_map(|s| match &s.kind {
            SiteKind::Neg {
                operand, literal, ..
            } => Some(format!("neg {} | {operand} | literal={literal}", s.text)),
            SiteKind::Guard { can_be_false } => {
                Some(format!("guard {} | can_be_false={can_be_false}", s.text))
            }
            SiteKind::Field {
                name,
                shorthand,
                from_base: Some(from_base),
                ..
            } => Some(format!(
                "field {} | {name} | shorthand={shorthand} | {from_base}",
                s.text
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        kinds,
        [
            "neg -x | x | literal=false",
            "neg -5 | 5 | literal=true",
            // `-0` is 0 either way; `-0.0` is not `0.0`.
            "neg -0.0 | 0.0 | literal=true",
            "neg -y | y | literal=false",
            // No catch-all arm: the arm cannot be removed, so `false` is a mutant.
            "guard n > 1 | can_be_false=true",
            // Where the arm can be removed, that is the guard being `false`.
            "guard n > 3 | can_be_false=false",
            "field depth: a | depth | shorthand=false | (base).depth",
            "field name | name | shorthand=true | (base).name",
            "field depth: -b | depth | shorthand=false | (Options { ..Default::default() }).depth",
            "neg -b | b | literal=false",
            "field 0: c | 0 | shorthand=false | (Wrap::<u8> { ..Wrap::new() }).0",
            "neg -2.5 | 2.5 | literal=true",
            // Not with a base that has arguments, a receiver or a computed function,
            // not a field with an attribute, and not without a base.
            "field strict: true | strict | shorthand=false | (*other).strict",
        ]
    );
    // A field is reported where its name stands.
    let field = sites
        .iter()
        .find(|s| s.text == "depth: a")
        .expect("the field is a site");
    let line = src.lines().nth(field.line - 1).unwrap();
    assert_eq!(line.find("depth: a"), Some(field.column - 1), "{line}");
    assert_eq!(&src[field.start..field.end], "a");
    let out = render(src, &sites, &all).text;
    syn::parse_file(&out).expect("instrumented source parses");
    assert_eq!(out.lines().count(), src.lines().count());
    for expected in [
        // The operand is evaluated once, outside the names of the probe.
        "(match (x) { __tmt_a => if crate::__tmt_rt::live(",
        // A literal stays a literal, with the type its place gives it.
        "u32) { -__tmt_a } else { __tmt_a } })",
        "u32) { -5 } else { 5 })",
        // A guard and the comparison it consists of: the guard goes around.
        "Kind::A(n) if crate::__tmt_rt::ret(",
        "u32, (match (&(n), &(1))",
        "n if crate::__tmt_rt::arm(",
        ") && (crate::__tmt_rt::ret(",
        "depth: if crate::__tmt_rt::live(",
        "{ a } else { (base).depth }",
        // A field written by its name alone gets the name back.
        "name: if crate::__tmt_rt::live(",
        "{ name } else { (base).name }, ..base }",
        "} else { (Options { ..Default::default() }).depth }, ..Default::default() }",
        "0: if crate::__tmt_rt::live(",
        "Options { depth: 1, ..base.clone() }",
        "Options { depth: 2, ..make(3) }",
        "Options { depth: 6, ..(pick())() }",
        "Options { depth: 5, name: n, strict: false }",
    ] {
        assert!(out.contains(expected), "{expected}\n{out}");
    }
    assert!(
        !out.contains("{ -0 }") && out.contains("{ -0.0 } else { 0.0 }"),
        "{out}"
    );
}
/// The source with each mutant of each site applied, as `what -> into: text`.
fn mutated(src: &str, sites: &[Site]) -> Vec<String> {
    sites
        .iter()
        .flat_map(|site| {
            source_mutants(site).into_iter().map(|m| {
                let text = format!("{}{}{}", &src[..m.start], m.replacement, &src[m.end..]);
                syn::parse_file(&text).unwrap_or_else(|e| panic!("{text}: {e}"));
                format!("{} -> {}: {}", m.what, m.into, text.trim())
            })
        })
        .collect()
}

#[test]
fn what_cannot_be_probed_is_mutated_in_the_source() {
    // Everything in a `const fn`, a compound assignment to more than a variable,
    // and the body of a function that returns an iterator.
    let src = "const fn c(a: u8, b: u8) -> bool { a < b }\n\
                   fn f(m: &mut Map, k: u8) { *m.entry(k).or_insert(0) += 1; m.total -= 1; }\n\
                   fn evens(n: u8) -> impl Iterator<Item = u8> + '_ { (0..n).step_by(2) }\n\
                   impl S { const fn k(&self) -> u8 { -self.0 } }\n\
                   trait T { const fn d(a: u8) -> u8 { a } }\n";
    let found = discover(src, 0).unwrap();
    assert!(found.rebuild_sites.iter().all(|s| s.id == NO_ID));
    let places: Vec<(usize, usize, &str)> = found
        .rebuild_sites
        .iter()
        .filter(|s| !matches!(s.kind, SiteKind::Return { .. }))
        .map(|s| (s.line, s.column, s.text.as_str()))
        .collect();
    assert_eq!(
        places,
        [
            (1, 1, "const fn c(a: u8, b: u8) -> bool"),
            (1, 1, "const fn c(a: u8, b: u8) -> bool"),
            (1, 36, "a < b"),
            (2, 28, "*m.entry(k).or_insert(0) += 1"),
            (3, 1, "fn evens(n: u8) -> impl Iterator<Item = u8> + '_"),
            (3, 1, "fn evens(n: u8) -> impl Iterator<Item = u8> + '_"),
            (3, 1, "fn evens(n: u8) -> impl Iterator<Item = u8> + '_"),
            // A range that is not run through by a `for`.
            (3, 53, "0..n"),
            (4, 10, "const fn k(&self) -> u8"),
            (4, 10, "const fn k(&self) -> u8"),
            (4, 36, "-self.0"),
            (5, 11, "const fn d(a: u8) -> u8"),
            (5, 11, "const fn d(a: u8) -> u8"),
        ]
    );
    assert_eq!(
        found.sites.iter().map(|s| s.id).collect::<Vec<_>>(),
        (0..found.sites.len() as u32).collect::<Vec<_>>()
    );
    let numbered: Vec<&str> = found.sites.iter().map(|s| s.text.as_str()).collect();
    assert_eq!(
        numbered,
        [
            "fn f(m: &mut Map, k: u8)",
            "*m.entry(k).or_insert(0) += 1;",
            "m.total -= 1;",
            "m.total -= 1",
        ]
    );
    assert_eq!(
        mutated(src, &found.rebuild_sites)
            .iter()
            .map(|line| line.lines().next().unwrap().to_string())
            .take(5)
            .collect::<Vec<_>>(),
        [
            "body -> true: const fn c(a: u8, b: u8) -> bool { true }",
            "body -> false: const fn c(a: u8, b: u8) -> bool { false }",
            "< -> ==: const fn c(a: u8, b: u8) -> bool { a == b }",
            "< -> >: const fn c(a: u8, b: u8) -> bool { a > b }",
            "< -> <=: const fn c(a: u8, b: u8) -> bool { a <= b }",
        ]
    );
    let whats: Vec<String> = found
        .rebuild_sites
        .iter()
        .flat_map(|site| {
            source_mutants(site)
                .into_iter()
                .map(|m| format!("{}: {} -> {}", site.text, m.what, m.into))
        })
        .skip(5)
        .collect();
    assert_eq!(
        whats,
        [
            "*m.entry(k).or_insert(0) += 1: += -> -=",
            "*m.entry(k).or_insert(0) += 1: += -> *=",
            "fn evens(n: u8) -> impl Iterator<Item = u8> + '_: body -> ::core::iter::empty()",
            "fn evens(n: u8) -> impl Iterator<Item = u8> + '_: body -> ::core::iter::once(0)",
            "fn evens(n: u8) -> impl Iterator<Item = u8> + '_: body -> ::core::iter::once(1)",
            "0..n: .. -> ..=",
            "const fn k(&self) -> u8: body -> 0",
            "const fn k(&self) -> u8: body -> 1",
            "-self.0: - -> ",
            "const fn d(a: u8) -> u8: body -> 0",
            "const fn d(a: u8) -> u8: body -> 1",
        ]
    );
}

#[test]
fn fields_are_removed_from_arbitrary_struct_updates() {
    // Full-strength table from fields.pict.
    for base in ["base", "make(seed)", "builder.make(seed)"] {
        for field in ["value: 9", "value"] {
            for attribute in ["", "#[cfg(any())] "] {
                let original =
                    format!("fn f() -> S {{ S {{ {attribute}{field}, other: 2, ..{base} }} }}");
                let found = discover(&original, 0).unwrap();
                let fields: Vec<_> = found
                    .sites
                    .iter()
                    .chain(&found.rebuild_sites)
                    .flat_map(source_mutants)
                    .filter(|mutant| mutant.what == "field" && mutant.into == "value")
                    .collect();
                if attribute.is_empty() {
                    assert_eq!(fields.len(), 1, "{original}");
                    let m = &fields[0];
                    let changed = format!(
                        "{}{}{}",
                        &original[..m.start],
                        m.replacement,
                        &original[m.end..]
                    );
                    assert_eq!(
                        changed,
                        format!("fn f() -> S {{ S {{  other: 2, ..{base} }} }}")
                    );
                    syn::parse_file(&changed).unwrap();
                } else {
                    assert!(fields.is_empty(), "{original}");
                }
            }
        }
    }
    let plain = discover("fn plain() -> S { S { value: 9 } }", 0).unwrap();
    assert!(
        plain
            .sites
            .iter()
            .chain(&plain.rebuild_sites)
            .flat_map(source_mutants)
            .all(|m| m.what != "field")
    );
}

#[test]
fn unknown_wrappers_and_nested_iterators_have_source_body_candidates() {
    // Full-strength table from generics.pict.
    for outer in ["Wrapper", "Option", "Result"] {
        for inner in ["bool", "impl Iterator<Item = u8>"] {
            for error in [false, true] {
                let error_values: &[&str] = if error { &["7"] } else { &[] };
                let extra = if outer == "Result" { ", u8" } else { "" };
                let source = format!("fn f() -> {outer}<{inner}{extra}> {{ todo!() }}");
                let found = discover_with_errors(&source, 0, error_values).unwrap();
                let values: Vec<&str> = found
                    .sites
                    .iter()
                    .chain(&found.rebuild_sites)
                    .filter_map(|site| match &site.kind {
                        SiteKind::Body { value, .. } => Some(value.as_str()),
                        _ => None,
                    })
                    .collect();
                let example = match (outer, inner) {
                    ("Wrapper", "bool") => "Wrapper::from_iter([false])",
                    ("Option", "bool") => "Some(false)",
                    ("Result", "bool") => "Ok(false)",
                    ("Wrapper", _) => "Wrapper::new(::core::iter::once(1))",
                    ("Option", _) => "Some(::core::iter::once(1))",
                    ("Result", _) => "Ok(::core::iter::once(1))",
                    _ => unreachable!(),
                };
                assert!(values.contains(&example), "{source}: {values:?}");
                assert_eq!(
                    values.contains(&"Err(7)"),
                    outer == "Result" && error,
                    "{source}"
                );
                if inner != "bool" {
                    assert!(found.sites.is_empty(), "{source}");
                }
            }
        }
    }
}

#[test]
fn source_deletes_the_whole_statement_and_the_unguarded_arm() {
    let source = "fn clear(v: &mut Vec<u8>) { v.clear(); }\n\
            fn select(x: u8) -> u8 { match x { 0 => 4, #[allow(unused)] 1 => 5, _ => 6 } }";
    let found = discover(source, 0).unwrap();
    let removals: Vec<_> = found
        .sites
        .iter()
        .chain(&found.rebuild_sites)
        .flat_map(source_mutants)
        .filter(|mutant| mutant.replacement.is_empty())
        .map(|mutant| &source[mutant.start..mutant.end])
        .collect();
    assert!(removals.contains(&"v.clear();"), "{removals:?}");
    assert!(removals.contains(&"0 => 4,"), "{removals:?}");
    assert!(
        removals.contains(&"#[allow(unused)] 1 => 5,"),
        "{removals:?}"
    );
}

#[test]
fn source_mutants_of_every_kind() {
    let src = "fn f(a: i32, b: i32, k: K, base: O) -> O { \
                   let x = match k { K::A(n) if n >= 1 => !(a != b) || a <= -b, K::B => a % b > 0 && a == b, _ => false }; \
                   O { on: x, a, ..base } }";
    let sites = found(src, 0).unwrap().sites;
    // Every one of them is still a source file.
    assert_eq!(mutated(src, &sites).len(), 18);
    let changes: Vec<String> = sites
        .iter()
        .flat_map(source_mutants)
        .map(|m| {
            format!(
                "{} -> {} | `{}` becomes `{}`",
                m.what,
                m.into,
                &src[m.start..m.end],
                m.replacement
            )
        })
        .collect();
    assert_eq!(
        changes,
        [
            // A guarded arm uses a false guard, and an unguarded arm is removed.
            "arm -> K::A(n) if n >= 1 | `n >= 1` becomes `false`",
            "arm -> K::B | `K::B => a % b > 0 && a == b,` becomes ``",
            "guard -> true | `n >= 1` becomes `true`",
            ">= -> < | ` >= ` becomes ` < `",
            "|| -> && | `!(a != b) || a <= -b` becomes `(!(a != b)) && (a <= -b)`",
            "! ->  | `!` becomes ``",
            "!= -> == | ` != ` becomes ` == `",
            "<= -> > | ` <= ` becomes ` > `",
            "- ->  | `-` becomes ``",
            "&& -> || | `a % b > 0 && a == b` becomes `(a % b > 0 || a == b)`",
            "> -> == | ` > ` becomes ` == `",
            "> -> < | ` > ` becomes ` < `",
            "> -> >= | ` > ` becomes ` >= `",
            "% -> / | ` % ` becomes ` / `",
            "% -> + | `a % b` becomes `(a + b)`",
            "== -> != | ` == ` becomes ` != `",
            "field -> on | `on: x,` becomes ``",
            "field -> a | `a,` becomes ``",
        ]
    );
    // What exists only as a probe has no mutant of the source.
    let src = "fn f(a: u8, b: u8) -> bool { g(); for _ in a..b {} a.max(b) > 1 }";
    let sites = discover(src, 0).unwrap().sites;
    for site in &sites {
        let none = matches!(site.kind, SiteKind::Return { .. } | SiteKind::MinMax { .. });
        assert_eq!(source_mutants(site).is_empty(), none, "{:?}", site.kind);
    }
    // A range has its end taken in or left out, whichever it is not.
    let ends = |src: &str| -> Vec<String> {
        discover(src, 0)
            .unwrap()
            .sites
            .iter()
            .filter(|site| matches!(site.kind, SiteKind::Range { .. }))
            .flat_map(source_mutants)
            .map(|m| format!("{}{}{}", &src[..m.start], m.replacement, &src[m.end..]))
            .collect()
    };
    assert_eq!(
        ends("fn f(a: u8, b: u8) { for _ in a..b {} }"),
        ["fn f(a: u8, b: u8) { for _ in a..=b {} }"]
    );
    assert_eq!(
        ends("fn f(a: u8, b: u8) { for _ in a ..= b {} }"),
        ["fn f(a: u8, b: u8) { for _ in a..b {} }"]
    );
}

#[test]
fn iterators_to_return_instead() {
    let values = |ty: &str| {
        let function: syn::ItemFn =
            syn::parse_str(&format!("fn f() -> {ty} {{ todo!() }}")).unwrap();
        iterator_values(&function.sig.output)
    };
    assert_eq!(
        values("impl Iterator<Item = bool>"),
        [
            "::core::iter::empty()",
            "::core::iter::once(true)",
            "::core::iter::once(false)"
        ]
    );
    assert_eq!(
        values("impl std::iter::Iterator<Item = String> + Send + 'a").len(),
        3
    );
    // The item is the binding called `Item`, wherever it stands.
    assert_eq!(
        values("impl Iterator<Extra = u8, Item = ()>"),
        ["::core::iter::empty()", "::core::iter::once(())"]
    );
    // An item that is itself opaque has no value to make up.
    assert_eq!(
        values("impl Iterator<Item = impl Sized>"),
        ["::core::iter::empty()"]
    );
    for other in [
        "impl Sized",
        "impl Iterator",
        "impl IntoIterator<Item = u8>",
        "Box<dyn Iterator<Item = u8>>",
        "u8",
        "()",
    ] {
        assert!(values(other).is_empty(), "{other}");
    }
    let unit: syn::ItemFn = syn::parse_str("fn f() {}").unwrap();
    assert!(iterator_values(&unit.sig.output).is_empty());
}

/// The source with each mutant of its sites that are built, one after the other.
fn built(src: &str) -> Vec<String> {
    let found = discover(src, 0).unwrap();
    found
        .rebuild_sites
        .iter()
        .flat_map(source_mutants)
        .map(|m| format!("{}{}{}", &src[..m.start], m.replacement, &src[m.end..]))
        .collect()
}

#[test]
fn a_body_that_is_written_as_the_value_and_one_that_only_computes() {
    let bodies = |src: &str| -> Vec<(String, bool)> {
        discover(src, 0)
            .unwrap()
            .sites
            .iter()
            .filter_map(|site| match &site.kind {
                SiteKind::Body {
                    value, sole_expr, ..
                } => Some((value.clone(), sole_expr.is_some())),
                _ => None,
            })
            .collect()
    };
    let values = |src: &str| -> Vec<String> { bodies(src).into_iter().map(|b| b.0).collect() };
    // The value the body already is makes no mutant, however it is spaced.
    assert_eq!(values("fn t() -> bool { true }"), ["false"]);
    assert_eq!(values("fn n() -> u8 {  0  }"), ["1"]);
    assert_eq!(
        values("fn s() -> String { String :: new () }"),
        ["\"xyzzy\".into()"]
    );
    // White space inside a literal is not spacing: `" "` is not the empty string.
    assert_eq!(
        values("fn s() -> &'static str { \" \" }"),
        ["\"\"", "\"xyzzy\""]
    );
    // A body that only computes its value can be said at that expression; one
    // that calls, assigns or leaves early does more than give a value.
    for (body, only_computes) in [
        ("n > 0", true),
        ("!(self.limit < n) && n as u8 != 3", true),
        ("(n, 1).0 > *&n", true),
        ("self.push(n) > 0", false),
        ("next() > 0", false),
        ("matches!(n, 1)", false),
        ("(if n > 1 { n } else { 0 }) > 0", false),
        ("list[n] > 0", false),
        ("check(n)? > 0", false),
        ("(self.count += n) == ()", false),
        ("(self.count = n) == ()", false),
    ] {
        let src = format!("impl S {{ fn f(&mut self, n: usize) -> bool {{ {body} }} }}");
        let all = bodies(&src);
        assert_eq!(all.len(), 2, "{body}");
        assert!(all.iter().all(|b| b.1 == only_computes), "{body}: {all:?}");
    }
}

#[test]
fn items_are_named_by_what_is_around_them() {
    let src = "fn top(a: u8) -> u8 { fn inner(b: u8) -> u8 { b } inner(a) }\n\
                   mod m { pub fn in_mod() {} const K: u8 = 1; }\n\
                   impl Store { fn commit(&self) {} const LIMIT: u8 = 2; }\n\
                   impl<T> fmt::Display for Id<T> { fn fmt(&self) {} }\n\
                   trait Tr { fn provided(&self) {} fn required(&self); }\n\
                   static S: u8 = 3;\nlet";
    // Not a source file: nothing is named.
    assert!(named_items(src).is_empty());
    let src = src.strip_suffix("let").unwrap();
    let items = named_items(src);
    let names: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "top",
            "top::inner",
            "m::in_mod",
            "m::K",
            "Store::commit",
            "Store::LIMIT",
            "<fmt::Display for Id<T>>::fmt",
            "Tr::provided",
            "Tr::required",
            "S",
        ]
    );
    // The innermost item a place is in; none between the items.
    let at = |text: &str| src.find(text).unwrap();
    assert_eq!(enclosing_item(&items, at("{ b }") + 2), "top::inner");
    assert_eq!(enclosing_item(&items, at("inner(a)")), "top");
    assert_eq!(enclosing_item(&items, at("fn top")), "top");
    assert_eq!(enclosing_item(&items, at("mod m")), "");
    assert_eq!(enclosing_item(&items, at("= 2") + 2), "Store::LIMIT");
    assert_eq!(enclosing_item(&items, src.len()), "");
    // An item is its first byte to its last, and not a byte more on either side.
    let (first, after) = (at("pub fn in_mod"), at("pub fn in_mod() {}") + 18);
    assert_eq!(enclosing_item(&items, first - 1), "");
    assert_eq!(enclosing_item(&items, first), "m::in_mod");
    assert_eq!(enclosing_item(&items, after - 1), "m::in_mod");
    assert_eq!(enclosing_item(&items, after), "");
    // Lines and columns count from one, columns in characters.
    let text = "ab\nçd e\n";
    assert_eq!(offset_of(text, 1, 1), Some(0));
    assert_eq!(offset_of(text, 1, 2), Some(1));
    assert_eq!(offset_of(text, 2, 1), Some(3));
    assert_eq!(offset_of(text, 2, 2), Some(5), "after the two bytes of `ç`");
    assert_eq!(offset_of(text, 2, 99), Some(text.len()));
    assert_eq!(offset_of(text, 0, 1), None);
    assert_eq!(offset_of(text, 1, 0), None);
}

#[test]
fn only_the_range_of_a_for_is_probed() {
    let src = "fn f(a: u8, b: u8, v: &[u8]) -> usize {\n\
                   for _ in a..b {}\n\
                   for _ in (a..=b) {}\n\
                   for _ in (a..b).rev() {}\n\
                   let kept = a..b;\n\
                   let _ = &v[a as usize..b as usize];\n\
                   for _ in kept.clone() {}\n\
                   for _ in 0..3 {}\n\
                   let _ = &v[0..3];\n\
                   (a..=b).count() }";
    let all = discover(src, 0).unwrap();
    let ranges = |sites: &[Site]| -> Vec<usize> {
        sites
            .iter()
            .filter(|site| matches!(site.kind, SiteKind::Range { .. }))
            .map(|site| site.line)
            .collect()
    };
    // Run through directly by a `for`, with or without parentheses.
    assert_eq!(ranges(&all.sites), [2, 3]);
    // Kept, indexed with, or given to a method: built, as the source says them.
    // And two literals wherever they are: nothing gives a probe their type.
    assert_eq!(ranges(&all.rebuild_sites), [4, 5, 6, 8, 9, 10]);
    let literal: Vec<(&str, String)> = all
        .rebuild_sites
        .iter()
        .filter(|site| site.line == 8 || site.line == 9)
        .flat_map(|site| {
            source_mutants(site)
                .into_iter()
                .map(move |mutant| (site.text.as_str(), mutant.replacement))
        })
        .collect();
    assert_eq!(
        literal,
        [("0..3", "..=".to_string()), ("0..3", "..=".to_string())]
    );
}

#[test]
fn what_marks_code_as_not_to_be_mutated() {
    let src = "#[allow(mutants, skip)] fn loosely(a: u8) -> u8 { a + 1 }\n\
                   #[doc = \"mutants skip\"] fn spoken_of(a: u8) -> u8 { a + 2 }\n\
                   #[mutants :: skip] fn spaced(a: u8) -> u8 { a + 3 }\n\
                   #[cfg_attr(test, mutants::skip)] fn conditional(a: u8) -> u8 { a + 4 }\n\
                   #[mutants::skip] mod generated;\n\
                   #[mutants::skip] #[path = \"gen/other.rs\"] mod other;\n\
                   mod plain;\n\
                   mod inline { fn inner(a: u8) -> u8 { a + 5 } }\n\
                   #[mutants::skip] mod marked { fn inner(a: u8) -> u8 { a + 6 } }\n";
    let all = discover(src, 0).unwrap();
    let texts: Vec<&str> = found_arith(&all.sites);
    assert_eq!(texts, ["a + 1", "a + 2", "a + 5"]);
    // The file of a marked module is not looked into either.
    assert_eq!(
        all.test_modules,
        [
            TestModule {
                name: "generated".to_string(),
                path: None
            },
            TestModule {
                name: "other".to_string(),
                path: Some("gen/other.rs".to_string())
            },
        ]
    );
}

fn found_arith(sites: &[Site]) -> Vec<&str> {
    sites
        .iter()
        .filter(|site| matches!(site.kind, SiteKind::Arith { .. }))
        .map(|site| site.text.as_str())
        .collect()
}

#[test]
fn a_replaced_operator_keeps_its_operands() {
    // Every replacement is the same tree with another operator, also where the new
    // operator binds otherwise than the old one.
    assert_eq!(
        built("const X: i32 = 8 - 2 * 3;"),
        [
            "const X: i32 = 8 + 2 * 3;",
            "const X: i32 = (8) / (2 * 3);",
            "const X: i32 = 8 - 2 / 3;",
            "const X: i32 = 8 - (2 + 3);",
        ]
    );
    assert_eq!(
        built("const B: bool = P || Q && R;"),
        [
            "const B: bool = (P) && (Q && R);",
            "const B: bool = P || (Q || R);",
        ]
    );
    assert_eq!(
        built("const M: u8 = A & B | C;"),
        [
            "const M: u8 = (A & B) & (C);",
            "const M: u8 = (A & B) ^ (C);",
            "const M: u8 = (A | B) | C;",
            "const M: u8 = (A ^ B) | C;",
        ]
    );
    assert_eq!(
        built("const S: u8 = 1 << 2 + 1;"),
        [
            "const S: u8 = 1 >> 2 + 1;",
            "const S: u8 = 1 << 2 - 1;",
            "const S: u8 = 1 << (2) * (1);",
        ]
    );
    // Comparisons bind alike, and an assignment has its operands to itself.
    assert_eq!(
        built("const C: bool = A + 1 < B;")[..3],
        [
            "const C: bool = A + 1 == B;",
            "const C: bool = A + 1 > B;",
            "const C: bool = A + 1 <= B;",
        ]
    );
    let assigned = built("const fn f(mut x: u8) -> u8 { x += 2 * 3; x }");
    for expected in [
        "const fn f(mut x: u8) -> u8 { x -= 2 * 3; x }",
        "const fn f(mut x: u8) -> u8 { x *= 2 * 3; x }",
        "const fn f(mut x: u8) -> u8 { x += (2 + 3); x }",
    ] {
        assert!(assigned.iter().any(|text| text == expected), "{assigned:?}");
    }
}

#[test]
fn excluded_parent_items_do_not_contribute_const_sites() {
    let source = r#"
            #[cfg(test)] enum TestEnum { A = 1 + 2 }
            #[mutants::skip] struct Skip { a: [u8; 2 + 3] }
            #[cfg(test)] type Alias = [u8; 3 + 4];
            #[cfg(test)] union TestUnion { a: [u8; 4 + 5] }
            #[mutants::skip] trait Hidden { const DATA: [u8; 5 + 6]; }
            struct Data { #[cfg(test)] bytes: [u8; 6 + 7] }
            fn f() { #[cfg(test)] let _: [u8; 8 + 9] = [0; 17]; }
            const DATA: [u8; 7 + 8] = [0; 15];
        "#;
    let found = discover(source, 0).unwrap();
    let sites: Vec<_> = found
        .rebuild_sites
        .iter()
        .map(|site| site.text.as_str())
        .collect();
    assert_eq!(sites, ["7 + 8"]);
}

#[test]
fn compile_time_expressions_get_source_mutants_but_never_probes() {
    // Full strength-three table from const.pict.
    for (place, context) in [
        ("inline", "let _ = const { 6 * 7 };"),
        ("repeat", "let _ = [0u8; 6 * 7];"),
        ("type length", "let _: [u8; 6 * 7];"),
        ("discriminant", "enum Value { One = 6 * 7 }"),
        ("const argument", "let _: Items<{ 6 * 7 }>;"),
        ("const default", "struct Items<const N: usize = { 6 * 7 }>;"),
    ] {
        for qualifier in ["", "const "] {
            for marker in ["", "#[mutants::skip] "] {
                let source =
                    format!("{marker}{qualifier}fn f(x: u32) -> u32 {{ {context} x * 2 }}");
                let found = discover(&source, 0).unwrap();
                let variants: Vec<_> = found
                    .rebuild_sites
                    .iter()
                    .filter(|site| site.text == "6 * 7")
                    .flat_map(source_mutants)
                    .map(|mutant| mutant.into)
                    .collect();
                let expected: &[&str] = if marker.is_empty() { &["/", "+"] } else { &[] };
                assert_eq!(variants, expected, "{place}: {source}");
                assert!(
                    found.sites.iter().all(|site| site.text != "6 * 7"),
                    "{source}"
                );
                assert_eq!(
                    found.sites.iter().any(|site| site.text == "x * 2"),
                    marker.is_empty() && qualifier.is_empty(),
                    "{source}"
                );
            }
        }
    }
}

#[test]
fn a_test_branch_or_skip_documentation_does_not_exclude_production_code() {
    let source = r#"
            /// The mutants::skip attribute is documented here.
            #[cfg(any(test, feature = "production"))]
            fn public(x: u32) -> u32 { x + 1 }

            #[cfg(all(test, feature = "production"))]
            fn for_tests(x: u32) -> u32 { x + 2 }

            #[cfg_attr(any(), mutants::skip)]
            fn skipped(x: u32) -> u32 { x + 3 }
        "#;
    let found = discover(source, 0).unwrap();
    let arithmetic: Vec<_> = found
        .sites
        .iter()
        .filter(|site| matches!(site.kind, SiteKind::Arith { .. }))
        .map(|site| site.text.as_str())
        .collect();
    assert_eq!(arithmetic, ["x + 1"]);
}

#[test]
fn constants_and_literal_arithmetic_are_mutated_in_the_source() {
    let src = "const HALF: u64 = SPAN / 2;\n\
                   static LIMIT: i32 = -(1 << 4);\n\
                   impl S { const K: bool = A < B; }\n\
                   trait T { const D: u8 = 3 + 4; const R: u8; }\n\
                   #[cfg(test)]\n\
                   const ONLY_IN_TESTS: u8 = 5 + 6;\n\
                   #[mutants::skip]\n\
                   const MARKED: u8 = 7 + 8;\n\
                   #[cfg(test)]\n\
                   static TEST_LIMIT: u8 = 9 + 1;\n\
                   impl S { #[mutants::skip] const SKIPPED: u8 = 9 + 2; }\n\
                   trait U { #[cfg(test)] const ONLY: u8 = 9 + 3; }\n\
                   fn f(x: u32) -> u32 { let _ = [0u8; 2 + 3]; x + 60 * 60 }\n\
                   fn one(s: &mut S) { s.go(); }\n\
                   fn two(s: &mut S) { s.go(); s.stop(); }\n\
                   fn value(s: &mut S) -> u8 { s.get() }\n";
    let found = discover(src, 0).unwrap();
    let built: Vec<String> = found
        .rebuild_sites
        .iter()
        .flat_map(|site| {
            source_mutants(site)
                .into_iter()
                .map(|m| format!("{}: {} -> {}", site.text, m.what, m.into))
        })
        .collect();
    assert_eq!(
        built,
        [
            "SPAN / 2: / -> *",
            "SPAN / 2: / -> %",
            "-(1 << 4): - -> ",
            "1 << 4: << -> >>",
            "A < B: < -> ==",
            "A < B: < -> >",
            "A < B: < -> <=",
            "3 + 4: + -> -",
            "3 + 4: + -> *",
            "2 + 3: + -> -",
            "2 + 3: + -> *",
            // Two literals in ordinary code too: a probe would give them a type.
            "60 * 60: * -> /",
            "60 * 60: * -> +",
        ]
    );
    mutated(src, &found.rebuild_sites);
    // The surrounding arithmetic is still probed.
    let probed: Vec<&str> = found
        .sites
        .iter()
        .filter(|s| matches!(s.kind, SiteKind::Arith { .. }))
        .map(|s| s.text.as_str())
        .collect();
    assert_eq!(probed, ["x + 60 * 60"]);
    // An empty body is not made where deleting the one statement is the same thing.
    assert_eq!(found.single_statement_bodies, ["one"]);
    let bodies: Vec<String> = found
        .sites
        .iter()
        .filter_map(|s| match &s.kind {
            SiteKind::Body { name, value, .. } => Some(format!("{name} {value}")),
            _ => None,
        })
        .collect();
    assert_eq!(bodies, ["f 0", "f 1", "two ()", "value 0", "value 1"]);
}
#[test]
fn an_attribute_excludes_the_compile_time_expressions_of_what_it_is_on() {
    // `@` is where the attribute goes: without one the length is a mutant to build.
    for context in [
        "fn f() { @let _ = [0u8; 6 * 7]; }",
        "fn f() { let _ = @const { 6 * 7 }; }",
        "struct Items<@const N: usize = { 6 * 7 }>;",
        "@enum E { A = 6 * 7 }",
        "enum E { @A = 6 * 7 }",
        "enum E { @A([u8; 6 * 7]) }",
        "@struct S { a: [u8; 6 * 7] }",
        "struct S { @a: [u8; 6 * 7] }",
        "@union U { a: [u8; 6 * 7] }",
        "@trait T { const D: [u8; 6 * 7]; }",
        "trait T { @type A = [u8; 6 * 7]; }",
        "@type A = [u8; 6 * 7];",
        "impl T for S { @type A = [u8; 6 * 7]; }",
    ] {
        for attribute in ["", "#[mutants::skip] ", "#[cfg(test)] "] {
            let source = context.replace('@', attribute);
            let found = discover(&source, 0).unwrap();
            let expected: &[&str] = if attribute.is_empty() {
                &["6 * 7"]
            } else {
                &[]
            };
            assert_eq!(found_arith(&found.rebuild_sites), expected, "{source}");
            assert!(found_arith(&found.sites).is_empty(), "{source}");
        }
    }
    // On the file itself it excludes everything in it.
    for attribute in ["#![mutants::skip]", "#![cfg(test)]"] {
        let source = format!("{attribute}\nconst N: u8 = 6 * 7;\nfn f(a: u8) -> u8 {{ a + 1 }}");
        let found = discover(&source, 0).unwrap();
        assert!(found.sites.is_empty(), "{source}");
        assert!(found.rebuild_sites.is_empty(), "{source}");
    }
}

#[test]
fn compile_time_expressions_are_found_inside_types() {
    let built = |source: &str| -> Vec<String> {
        let found = discover(source, 0).unwrap();
        assert!(found_arith(&found.sites).is_empty(), "{source}");
        found_arith(&found.rebuild_sites)
            .into_iter()
            .map(str::to_string)
            .collect()
    };
    // The element of an array is a type with lengths of its own.
    assert_eq!(
        built("fn f() { let _: [[u8; 1 + 2]; 3 + 4]; }"),
        ["1 + 2", "3 + 4"]
    );
    // So is the type of a const parameter.
    assert_eq!(built("struct S<const N: [u8; 2 + 3]>;"), ["2 + 3"]);
    // A const argument is computed by the compiler whatever it consists of: it is
    // never probed.
    assert_eq!(built("fn f() { let _: Items<{ N + 1 }>; }"), ["N + 1"]);
}

#[test]
fn only_the_skip_mark_and_a_cfg_that_needs_test_exclude_code() {
    let src = "#[mutants] fn bare(a: u8) -> u8 { a + 1 }\n\
                   #[mutants::skip::deeper] fn longer(a: u8) -> u8 { a + 2 }\n\
                   #[cfg(all(test))] fn all_of_one(a: u8) -> u8 { a + 3 }\n\
                   #[cfg(not(not(test)))] fn twice_not(a: u8) -> u8 { a + 4 }\n\
                   #[cfg(not())] fn malformed(a: u8) -> u8 { a + 5 }\n\
                   #[labels(test)] fn labelled(a: u8) -> u8 { a + 6 }\n";
    let found = discover(src, 0).unwrap();
    assert_eq!(
        found_arith(&found.sites),
        ["a + 1", "a + 2", "a + 5", "a + 6"]
    );
}

#[test]
fn which_arms_are_probed_built_or_left_alone() {
    let src = "fn f(x: u8, k: Kind, c: bool, a: u8) -> u8 {\n\
                   \x20   let p = match x { 0 => 4, #[allow(unused)] 1 => 5, #[allow(unused)] 2 if c => 6, _ => 7 };\n\
                   \x20   let q = match k { Kind::A => 1, Kind::B => 2 };\n\
                   \x20   let r = match x { #[mutants::skip] 3 => a + 1, #[cfg(test)] 4 => a + 2, 5 => a + 3, _ => a + 4 };\n\
                   \x20   p + q + r\n\
                   }\n";
    let found = discover(src, 0).unwrap();
    let arms = |sites: &[Site]| -> Vec<String> {
        sites
            .iter()
            .filter(|site| matches!(site.kind, SiteKind::Arm { .. }))
            .map(|site| site.text.clone())
            .collect()
    };
    // Probed: an arm without attributes above a last arm that takes everything.
    assert_eq!(arms(&found.sites), ["0 => ..", "5 => .."]);
    // Built: an unguarded arm with an attribute, where a `_` arm keeps the match
    // exhaustive. Neither a guarded one, nor the `_` arm, nor an arm of a match
    // without `_`, nor one that is marked or only for tests.
    assert_eq!(arms(&found.rebuild_sites), ["1 => .."]);
    // What a marked or test-only arm does is not mutated either.
    let in_arms: Vec<&str> = found_arith(&found.sites)
        .into_iter()
        .filter(|text| text.starts_with("a + "))
        .collect();
    assert_eq!(in_arms, ["a + 3", "a + 4"]);
}

#[test]
fn which_fields_of_a_struct_update_are_probed_built_or_left_alone() {
    let src = "fn f(a: u8, base: S) {\n\
                   \x20   let _ = S { #[mutants::skip] v: a + 1, #[cfg(test)] w: a + 2, x: a + 3, ..base };\n\
                   \x20   g(#[mutants::skip] S { v: a + 4, ..base });\n\
                   \x20   g(#[cfg(test)] S { v: a + 5, ..base });\n\
                   \x20   g(#[allow(unused)] S { v: a + 6, ..base });\n\
                   \x20   g(<S as Tr>::Out { v: a + 7, ..base });\n\
                   }\n";
    let found = discover(src, 0).unwrap();
    let fields = |sites: &[Site]| -> Vec<String> {
        sites
            .iter()
            .filter_map(|site| match &site.kind {
                SiteKind::Field { from_base, .. } => Some(format!(
                    "{} | {}",
                    site.text,
                    from_base.as_deref().unwrap_or("-")
                )),
                _ => None,
            })
            .collect()
    };
    // Probed: the field is read from the base instead.
    assert_eq!(fields(&found.sites), ["x: a + 3 | (base).x"]);
    // Built: an expression with an attribute or a qualified path is not written a
    // second time. Marked and test-only fields and expressions have no mutants.
    assert_eq!(
        fields(&found.rebuild_sites),
        ["v: a + 6 | -", "v: a + 7 | -"]
    );
    assert_eq!(found_arith(&found.sites), ["a + 3", "a + 6", "a + 7"]);
}

#[test]
fn body_values_that_are_only_built() {
    let bodies = |sites: &[Site]| -> Vec<String> {
        sites
            .iter()
            .filter_map(|site| match &site.kind {
                SiteKind::Body { value, .. } => Some(value.clone()),
                _ => None,
            })
            .collect()
    };
    // An unknown wrapper is probed with its default alone; its constructors are
    // guesses that have to compile, except the one the body already is.
    let found = discover("fn f() -> Wrapper<bool> { Wrapper::new() }", 0).unwrap();
    assert_eq!(bodies(&found.sites), ["Default::default()"]);
    assert_eq!(values("Wrapper<bool>"), ["Default::default()"]);
    assert_eq!(
        bodies(&found.rebuild_sites),
        [
            "Wrapper::from_iter([true])",
            "Wrapper::new(true)",
            "Wrapper::from(true)",
            "Wrapper::from_iter([false])",
            "Wrapper::new(false)",
            "Wrapper::from(false)",
        ]
    );
    // An opaque type anywhere in the return type leaves nothing to probe.
    assert!(values("(bool, impl Iterator<Item = u8>)").is_empty());
    // Iterators of iterators are built, and not offered to who asks for the
    // iterators to return.
    let nested = "impl Iterator<Item = impl Iterator<Item = u8>>";
    let found = discover(&format!("fn f() -> {nested} {{ todo!() }}"), 0).unwrap();
    assert!(bodies(&found.sites).is_empty());
    assert_eq!(
        bodies(&found.rebuild_sites),
        [
            "::core::iter::empty()",
            "::core::iter::once(::core::iter::empty())",
            "::core::iter::once(::core::iter::once(0))",
            "::core::iter::once(::core::iter::once(1))",
        ]
    );
    let function: syn::ItemFn =
        syn::parse_str(&format!("fn f() -> {nested} {{ todo!() }}")).unwrap();
    assert_eq!(
        iterator_values(&function.sig.output),
        ["::core::iter::empty()"]
    );
}

#[test]
fn inner_attributes_stay_before_body_and_return_probes() {
    let source = "\
fn f(x: i32) -> bool {
    #![allow(clippy::all)]
    #![allow(dead_code)]
    x > 0
}
";
    let found = discover(source, 0).unwrap();
    let rendered = render(source, &found.sites, &|_| true);
    assert!(
        syn::parse_file(&rendered.text).is_ok(),
        "rendered source must parse:\n{}",
        rendered.text
    );
    assert_eq!(
        rendered.text.matches('\n').count(),
        source.matches('\n').count()
    );
    let attr_pos = rendered.text.find("#![allow(dead_code)]").unwrap();
    let probe_pos = rendered.text.find("__tmt_rt::").unwrap();
    assert!(attr_pos < probe_pos, "{}", rendered.text);
    for site in found
        .sites
        .iter()
        .filter(|s| matches!(s.kind, SiteKind::Body { .. }))
    {
        for mutant in source_mutants(site) {
            let mut text = source.to_string();
            text.replace_range(mutant.start..mutant.end, &mutant.replacement);
            assert!(
                syn::parse_file(&text).is_ok(),
                "body mutant failed:\n{text}"
            );
        }
    }
}

#[test]
fn same_built_tokens_keeps_single_element_tuple_comma_after_left_out_item() {
    let single_tuple = format!("fn f() -> (i32,) {{ (1, {LEFT_OUT}) }}");
    let plain_expr = "fn f() -> (i32,) { (1) }";
    let explicit_tuple = "fn f() -> (i32,) { (1,) }";
    assert_eq!(same_built_tokens(&single_tuple, plain_expr), Some(false));
    assert_eq!(same_built_tokens(&single_tuple, explicit_tuple), Some(true));
    // Commas inside type arguments must not trick tuple comma handling into dropping
    // the sole tuple comma.
    let generic_tuple = format!("fn f() {{ let _ = (g::<A, B>(), {LEFT_OUT}); }}");
    let generic_plain = "fn f() { let _ = (g::<A, B>()); }";
    let generic_explicit = "fn f() { let _ = (g::<A, B>(),); }";
    assert_eq!(
        same_built_tokens(&generic_tuple, generic_plain),
        Some(false)
    );
    assert_eq!(
        same_built_tokens(&generic_tuple, generic_explicit),
        Some(true)
    );
}

#[test]
fn deleting_unary_operator_does_not_merge_adjacent_tokens() {
    for (source, expected) in [
        (
            "fn f(x: bool) -> bool { return!x; }",
            "fn f(x: bool) -> bool { return x; }",
        ),
        ("fn g() -> i32 { return-1; }", "fn g() -> i32 { return 1; }"),
        (
            "fn h(a: i32, b: &i32) -> i32 { a/!*b }",
            "fn h(a: i32, b: &i32) -> i32 { a/ *b }",
        ),
        (
            "fn u(℘: bool) -> bool { return!℘; }",
            "fn u(℘: bool) -> bool { return ℘; }",
        ),
    ] {
        let found = discover(source, 0).unwrap();
        let unary = found
            .sites
            .iter()
            .find(|s| matches!(s.kind, SiteKind::Not { .. } | SiteKind::Neg { .. }))
            .expect("unary site");
        for mutant in source_mutants(unary) {
            let mut text = source.to_string();
            text.replace_range(mutant.start..mutant.end, &mutant.replacement);
            assert!(syn::parse_file(&text).is_ok(), "merged tokens:\n{text}");
            assert_eq!(same_tokens(&text, expected), Some(true), "{text}");
        }
    }
}

#[test]
fn discovers_renders_and_mutates_value_aware_method_sites() {
    let source = r#"
pub fn checks(s: &str, x: i32, y: f64, xs: &[i32]) -> bool {
    let a = s.starts_with("pre") && s.ends_with("suf");
    let b = s.replace("a", "b");
    let c = s.replacen("a", "b", 1);
    let d = s.replacen("a", "b", 2);
    let e = x.clamp(0, 100);
    let f = y.floor() + y.ceil() + y.round() + y.trunc();
    let g = xs.iter().all(|&n| n > 0) && xs.iter().any(|&n| n < 0);
    let h = ["GET", "POST", "PUT"].contains(&s);
    let _dup = ["a", "a"].contains(&s);
    a && !b.is_empty() && !c.is_empty() && !d.is_empty() && e >= 0 && f >= 0.0 && g && h
}
"#;
    let found = discover(source, 0).unwrap();
    let has_kind = |pred: fn(&SiteKind) -> bool| found.sites.iter().any(|s| pred(&s.kind));
    assert!(has_kind(|k| matches!(
        k,
        SiteKind::StrAffix { starts: true, .. }
    )));
    assert!(has_kind(|k| matches!(
        k,
        SiteKind::StrAffix { starts: false, .. }
    )));
    assert!(has_kind(|k| matches!(
        k,
        SiteKind::StrReplace {
            replace_all: true,
            ..
        }
    )));
    assert!(has_kind(|k| matches!(
        k,
        SiteKind::StrReplace {
            replace_all: false,
            ..
        }
    )));
    // `replacen(..., 2)` is not mutated to `replace`.
    assert_eq!(
        found
            .sites
            .iter()
            .filter(|s| matches!(s.kind, SiteKind::StrReplace { .. }))
            .count(),
        2
    );
    assert!(has_kind(|k| matches!(k, SiteKind::Clamp { .. })));
    assert_eq!(
        found
            .sites
            .iter()
            .filter(|s| matches!(s.kind, SiteKind::Rounding { .. }))
            .count(),
        4
    );
    assert_eq!(
        found
            .sites
            .iter()
            .filter(|s| matches!(s.kind, SiteKind::Quantifier { .. }))
            .count(),
        2
    );
    // LiteralSet goes to rebuild_sites and skips duplicate-element arrays.
    let literal_sets: Vec<_> = found
        .rebuild_sites
        .iter()
        .filter(|s| matches!(s.kind, SiteKind::LiteralSet { .. }))
        .collect();
    assert_eq!(literal_sets.len(), 1);
    assert_eq!(source_mutants(literal_sets[0]).len(), 3);

    // Rendered probes parse as valid Rust with identical line counts.
    let rendered = render(source, &found.sites, &|_| true);
    assert_eq!(
        rendered.text.matches('\n').count(),
        source.matches('\n').count()
    );
    assert!(syn::parse_file(&rendered.text).is_ok(), "{}", rendered.text);

    // Every source mutant parses as valid Rust and preserves line counts.
    for site in found.sites.iter().chain(&found.rebuild_sites) {
        let mut mutants = source_mutants(site);
        keep_line_ends(source, &mut mutants);
        for mutant in mutants {
            let mut text = source.to_string();
            text.replace_range(mutant.start..mutant.end, &mutant.replacement);
            assert_eq!(text.matches('\n').count(), source.matches('\n').count());
            assert!(syn::parse_file(&text).is_ok(), "invalid mutant:\n{text}");
        }
    }
}

#[test]
fn clamp_impure_bounds_literal_set_value_dedup_and_multiline_line_preservation() {
    let source = r#"
fn impure_clamp(x: i32) -> i32 {
    x.clamp(
        lo_fn(),
        hi_fn(),
    )
}
fn multiline_replace(s: &str) -> String {
    s.replace(
        "a",
        "b",
    )
}
fn dup_hex(x: u32) -> bool {
    [1, 0x1].contains(&x)
}
fn dup_raw_str(s: &str) -> bool {
    ["a", r"a"].contains(&s)
}
fn dup_signed_zero(x: f64) -> bool {
    [0.0, -0.0].contains(&x)
}
fn dup_f32_rounding(x: f32) -> bool {
    [16_777_216.0, 16_777_217.0].contains(&x)
}
fn panicking_clamp(x: i32, d: i32) -> i32 {
    x.clamp(1 / d, 10)
}
"#;
    let found = discover(source, 0).unwrap();
    // Impure or potentially panicking clamp bounds go to rebuild_sites instead of probes.
    assert!(
        !found
            .sites
            .iter()
            .any(|s| matches!(s.kind, SiteKind::Clamp { .. }))
    );
    assert_eq!(
        found
            .rebuild_sites
            .iter()
            .filter(|s| matches!(s.kind, SiteKind::Clamp { .. }))
            .count(),
        2
    );
    let clamp_site = found
        .rebuild_sites
        .iter()
        .find(|s| matches!(s.kind, SiteKind::Clamp { .. }))
        .expect("impure clamp in rebuild_sites");
    let mut clamp_mutants = source_mutants(clamp_site);
    keep_line_ends(source, &mut clamp_mutants);
    assert_eq!(clamp_mutants.len(), 2);
    let line_of = |text: &str, needle: &str| {
        text.lines()
            .position(|l| l.contains(needle))
            .expect("needle present")
    };
    let orig_lo_line = line_of(source, "lo_fn()");
    let orig_hi_line = line_of(source, "hi_fn()");
    let mut drop_lo = source.to_string();
    drop_lo.replace_range(
        clamp_mutants[0].start..clamp_mutants[0].end,
        &clamp_mutants[0].replacement,
    );
    assert!(syn::parse_file(&drop_lo).is_ok(), "{drop_lo}");
    assert_eq!(line_of(&drop_lo, "hi_fn()"), orig_hi_line);
    let mut drop_hi = source.to_string();
    drop_hi.replace_range(
        clamp_mutants[1].start..clamp_mutants[1].end,
        &clamp_mutants[1].replacement,
    );
    assert!(syn::parse_file(&drop_hi).is_ok(), "{drop_hi}");
    assert_eq!(line_of(&drop_hi, "lo_fn()"), orig_lo_line);

    // Multiline replace keeps both argument lines intact.
    let rep_site = found
        .sites
        .iter()
        .find(|s| matches!(s.kind, SiteKind::StrReplace { .. }))
        .expect("replace site");
    let mut rep_mutants = source_mutants(rep_site);
    keep_line_ends(source, &mut rep_mutants);
    let mut rep_text = source.to_string();
    rep_text.replace_range(
        rep_mutants[0].start..rep_mutants[0].end,
        &rep_mutants[0].replacement,
    );
    assert!(syn::parse_file(&rep_text).is_ok(), "{rep_text}");
    assert_eq!(line_of(&rep_text, "\"a\""), line_of(source, "\"a\""));
    assert_eq!(line_of(&rep_text, "\"b\""), line_of(source, "\"b\""));

    // Value-equivalent duplicates are rejected from LiteralSet.
    assert!(
        !found
            .rebuild_sites
            .iter()
            .any(|s| matches!(s.kind, SiteKind::LiteralSet { .. }))
    );
}

#[test]
fn disjoint_bits_matches_pict_table() {
    let table = include_str!("../../tests/data/bit-disjoint.pict.tsv");
    let all_primitive = |_: usize, _: usize| true;
    let no_primitive = |_: usize, _: usize| false;
    let mut rows = 0;
    for line in table.lines().skip(1).filter(|l| !l.trim().is_empty()) {
        let cols: Vec<&str> = line.split('\t').collect();
        assert_eq!(cols.len(), 5, "unexpected row: {line}");
        let (op, int_type, lhs_shape, rhs_shape) = (cols[0], cols[1], cols[2], cols[3]);
        let bounds = match int_type {
            "u8" => Some((u8::MIN as i128, u8::MAX as i128)),
            "u16" => Some((u16::MIN as i128, u16::MAX as i128)),
            "i8" => Some((i8::MIN as i128, i8::MAX as i128)),
            "unknown" => None,
            other => panic!("unexpected IntType: {other}"),
        };
        let lhs = match lhs_shape {
            "shl4" => "a << 4",
            "shl2" => "a << 2",
            "mask_hi" => "a & 0xf0",
            "cast_u8_shl8" => "((a as u8) as u16) << 8",
            "plain" => "a",
            other => panic!("unexpected LhsShape: {other}"),
        };
        let rhs = match rhs_shape {
            "shr4" => "b >> 4",
            "shr2" => "b >> 2",
            "mask_lo" => "b & 0x0f",
            "lit_low" => "0x03",
            "rem_4" => "b % 4",
            "plain" => "b",
            other => panic!("unexpected RhsShape: {other}"),
        };
        let actual = super::disjoint_bits(
            lhs,
            0,
            rhs,
            100,
            bounds,
            &all_primitive,
            &[],
            &[],
            &|_, _| None,
        );
        let expected = match (int_type, lhs_shape, rhs_shape) {
            // u8: shl4 => 0xF0, shl2 => 0xFC, mask_hi => 0xF0, cast_u8_shl8 => 0xFF (shift >= 8)
            // u8 rhs: shr4 => 0x0F, shr2 => 0x3F, mask_lo => 0x0F, lit_low => 0x03, rem_4 => 0x03
            ("u8", "shl4" | "mask_hi", "shr4" | "mask_lo" | "lit_low" | "rem_4") => true,
            ("u8", "shl2", "lit_low" | "rem_4") => true,
            // u16: shl4 => 0xFFF0, shl2 => 0xFFFC, mask_hi => 0x00F0, cast_u8_shl8 => 0xFF00
            // u16 rhs: shr4 => 0x0FFF, shr2 => 0x3FFF, mask_lo => 0x000F, lit_low => 0x0003, rem_4 => 0x0003
            ("u16", "shl4" | "mask_hi" | "cast_u8_shl8", "mask_lo" | "lit_low" | "rem_4") => true,
            ("u16", "shl2", "lit_low" | "rem_4") => true,
            // i8: arithmetic shr and rem are signed => conservative 0xFF; shl4 => 0xF0, mask_hi => 0xF0, shl2 => 0xFC
            ("i8", "shl4" | "mask_hi", "mask_lo" | "lit_low") => true,
            ("i8", "shl2", "lit_low") => true,
            // unknown bounds: conservative false (no runtime type width proof)
            _ => false,
        };
        assert_eq!(
            actual, expected,
            "op={op} type={int_type} lhs={lhs} rhs={rhs}"
        );
        rows += 1;
    }
    assert!(rows >= 30, "expected full PICT table, got {rows} rows");

    // Non-primitive (overloaded) inner operators must never be assumed to mask bits.
    assert!(!super::disjoint_bits(
        "flags & 0xf0",
        0,
        "0x0f",
        100,
        Some((u8::MIN as i128, u8::MAX as i128)),
        &no_primitive,
        &[],
        &[],
        &|_, _| None,
    ));

    // i128 bounds must not overflow during signed width detection.
    assert!(super::disjoint_bits(
        "a << 4",
        0,
        "0x0f",
        100,
        Some((i128::MIN, i128::MAX)),
        &all_primitive,
        &[],
        &[],
        &|_, _| None,
    ));
    assert!(!super::disjoint_bits(
        "a << 4",
        0,
        "b >> 4",
        100,
        Some((i128::MIN, i128::MAX)),
        &all_primitive,
        &[],
        &[],
        &|_, _| None,
    ));

    // Pure vs side-effecting logic right-hand operands.
    assert!(super::is_pure_logic_operand("value == \"$u\""));
    assert!(super::is_pure_logic_operand("value.starts_with(\"$E\")"));
    assert!(!super::is_pure_logic_operand("{ *count += 1; true }"));
    assert!(!super::is_pure_logic_operand("side_effect()"));
}

#[test]
fn capacity_hint_arguments_and_reserve_statements_are_skipped() {
    let source = r#"
use std::collections::{HashMap, HashSet, VecDeque};

fn make(groups: usize, n: usize, a: usize, b: usize) {
    let mut v: Vec<u8> = Vec::with_capacity(groups * 3);
    let mut s = String::with_capacity(n + 8);
    let _m: HashMap<usize, usize> = std::collections::HashMap::with_capacity(n * 2);
    let _d: VecDeque<usize> = VecDeque::with_capacity(n + 1);
    let _h: HashSet<usize> = HashSet::with_capacity_and_hasher(n * 4, make_hasher(a + b));
    Vec::reserve(&mut v, n * 3);
    Vec::reserve_exact(&mut v, n + 2);
    s.reserve_exact(n + 4);
    pick_vec(&mut v, a * 2).reserve_exact(n + 16);
    let _side = Vec::with_capacity({ audit(); n + 11 });
    let _custom = Quota::with_capacity(n + 7);
    let _local_vec = domain::Vec::with_capacity(n + 13);
    account.reserve(n + 9);
    let _ = v.try_reserve(n * 5);
    let _ = with_capacity(n * 6);
}
"#;
    let found = discover(source, 0).unwrap();
    let texts: Vec<&str> = found.sites.iter().map(|s| s.text.as_str()).collect();

    // Standard capacity-hint arithmetic and plain-place reserve statements must be excluded.
    for skipped in [
        "groups * 3",
        "n + 8",
        "n * 2",
        "n + 1",
        "n * 4",
        "n * 3",
        "n + 2",
        "n + 4",
        "n + 16",
        "Vec::reserve(&mut v, n * 3);",
        "Vec::reserve_exact(&mut v, n + 2);",
        "s.reserve_exact(n + 4);",
    ] {
        assert!(
            !texts.contains(&skipped),
            "expected `{skipped}` to be skipped, got {texts:?}"
        );
    }

    // Non-capacity arguments, side-effecting argument blocks, side-effecting receiver
    // statements, custom/local-module APIs, receivers, fallible try_reserve, and free
    // functions stay active.
    for kept in [
        "a + b",
        "a * 2",
        "pick_vec(&mut v, a * 2).reserve_exact(n + 16);",
        "audit();",
        "n + 11",
        "n + 7",
        "n + 13",
        "account.reserve(n + 9);",
        "n + 9",
        "n * 5",
        "n * 6",
    ] {
        assert!(
            texts.contains(&kept),
            "expected `{kept}` to remain a site, got {texts:?}"
        );
    }
}

#[test]
fn disjoint_bits_widening_cast_matches_pict_table() {
    use super::{BitValueType, LocalBitType};

    let table = include_str!("../../tests/data/bit-disjoint-cast.pict.tsv");
    let all_primitive = |_: usize, _: usize| true;
    let mut rows = 0;
    for line in table.lines().skip(1).filter(|l| !l.trim().is_empty()) {
        let cols: Vec<&str> = line.split('\t').collect();
        assert_eq!(cols.len(), 5, "unexpected row: {line}");
        let (dst_type, src_type, cast_form, shift, rhs_kind) =
            (cols[0], cols[1], cols[2], cols[3], cols[4]);

        let bounds = match dst_type {
            "u16" => Some((u16::MIN as i128, u16::MAX as i128)),
            "u32" => Some((u32::MIN as i128, u32::MAX as i128)),
            "i32" => Some((i32::MIN as i128, i32::MAX as i128)),
            "usize" => Some((0, u64::MAX as i128)),
            "unknown" => None,
            other => panic!("unexpected DstType: {other}"),
        };
        let dst_ident = match dst_type {
            "unknown" => "u32",
            other => other,
        };
        let mut local_types: Vec<(String, LocalBitType)> =
            vec![("b".into(), LocalBitType::Value(BitValueType::Unsigned(8)))];
        if cast_form != "shadowed" {
            let src_val = match src_type {
                "u8" => Some(BitValueType::Unsigned(8)),
                "u16" => Some(BitValueType::Unsigned(16)),
                "i8" => Some(BitValueType::Signed(8)),
                "bool" => Some(BitValueType::Bool),
                "unknown" => None,
                other => panic!("unexpected SrcType: {other}"),
            };
            if let Some(ty) = src_val {
                if cast_form == "slice_index" {
                    local_types.push(("buf".into(), LocalBitType::SliceOf(ty)));
                } else {
                    local_types.push(("a".into(), LocalBitType::Value(ty)));
                }
            }
        }

        let base_cast = match cast_form {
            "as_cast" | "shadowed" => format!("(a as {dst_ident})"),
            "from_call" => format!("{dst_ident}::from(a)"),
            "nested_cast" => format!("((a as {src_type}) as {dst_ident})"),
            "slice_index" => format!("(buf[0] as {dst_ident})"),
            other => panic!("unexpected CastForm: {other}"),
        };
        let shift_bits: u32 = match shift {
            "shl8" => 8,
            "shl16" => 16,
            "shl4" => 4,
            other => panic!("unexpected Shift: {other}"),
        };
        let lhs = format!("({base_cast} << {shift_bits})");
        let rhs = match rhs_kind {
            "u8_cast" => format!("(b as {dst_ident})"),
            "u8_shl8" => format!("((b as {dst_ident}) << 8)"),
            "mask_lo" => "c & 0x0f".to_string(),
            "plain" => "c".to_string(),
            other => panic!("unexpected RhsKind: {other}"),
        };

        let actual = super::disjoint_bits(
            &lhs,
            0,
            &rhs,
            200,
            bounds,
            &all_primitive,
            &local_types,
            &[],
            &|_, _| None,
        );

        let dst_width: Option<u32> = match dst_type {
            "u16" => Some(16),
            "u32" | "i32" => Some(32),
            "usize" => Some(64),
            "unknown" => None,
            _ => unreachable!(),
        };
        let expected = if let Some(dw) = dst_width {
            let full = if dw >= 128 {
                u128::MAX
            } else {
                (1u128 << dw) - 1
            };
            let base_ones = if cast_form == "shadowed" {
                full
            } else {
                match src_type {
                    "u8" => 0xffu128 & full,
                    "u16" => 0xffffu128 & full,
                    "bool" => 0x1u128 & full,
                    // Signed widening sign-extends negative values -> full mask.
                    "i8" | "unknown" => full,
                    _ => unreachable!(),
                }
            };
            let lhs_ones = if shift_bits < dw {
                (base_ones << shift_bits) & full
            } else {
                full
            };
            let rhs_ones = match rhs_kind {
                "u8_cast" => 0xffu128 & full,
                "u8_shl8" => {
                    if 8 < dw {
                        (0xffu128 << 8) & full
                    } else {
                        full
                    }
                }
                "mask_lo" => 0x0fu128 & full,
                "plain" => full,
                _ => unreachable!(),
            };
            (lhs_ones & rhs_ones) == 0
        } else {
            false
        };

        assert_eq!(
            actual, expected,
            "dst={dst_type} src={src_type} form={cast_form} shift={shift} rhs={rhs_kind} lhs={lhs} rhs_expr={rhs}"
        );
        rows += 1;
    }
    assert!(rows >= 30, "expected full PICT table, got {rows} rows");
}

#[test]
fn discover_collects_local_types_for_widening_bitpack_and_byte_char_literals() {
    let source = r#"
fn pack3(first: u8, second: u8, third: u8) -> u32 {
    ((first as u32) << 16) | ((second as u32) << 8) | third as u32
}

fn pack_from(first: u8, second: u8, third: u8) -> u32 {
    (u32::from(first) << 16) | (u32::from(second) << 8) | u32::from(third)
}

fn pack_slice(buf: &[u8]) -> u32 {
    ((buf[0] as u32) << 24) | ((buf[1] as u32) << 16) | ((buf[2] as u32) << 8) | buf[3] as u32
}

fn pack_signed(first: i8, second: i8) -> u32 {
    ((first as u32) << 8) | (second as u32)
}

fn pack_shadowed(first: u8, second: u8) -> u32 {
    let second = widen_unknown(second);
    ((first as u32) << 8) | (second as u32)
}

fn byte_and_char(tag: u8, ch: char) -> bool {
    tag == b'A' || b'\n' == tag || ch == 'Z' || '0' <= ch
}
"#;
    let found = discover(source, 0).unwrap();
    let all_primitive = |_: usize, _: usize| true;
    let u32_bounds = Some((u32::MIN as i128, u32::MAX as i128));

    let disjoint_for = |snippet: &str| {
        let site = found
            .sites
            .iter()
            .find(|s| s.text == snippet)
            .unwrap_or_else(|| panic!("site `{snippet}` not found"));
        let SiteKind::Arith {
            lhs,
            rhs,
            gap,
            local_types,
            shadowed_prims,
            ..
        } = &site.kind
        else {
            panic!("expected Arith for `{snippet}`");
        };
        super::disjoint_bits(
            lhs,
            site.start,
            rhs,
            gap.1,
            u32_bounds,
            &all_primitive,
            local_types,
            shadowed_prims,
            &|_, _| None,
        )
    };

    assert!(disjoint_for(
        "((first as u32) << 16) | ((second as u32) << 8)"
    ));
    assert!(disjoint_for(
        "((first as u32) << 16) | ((second as u32) << 8) | third as u32"
    ));
    assert!(disjoint_for(
        "(u32::from(first) << 16) | (u32::from(second) << 8) | u32::from(third)"
    ));
    assert!(disjoint_for(
        "((buf[0] as u32) << 24) | ((buf[1] as u32) << 16) | ((buf[2] as u32) << 8) | buf[3] as u32"
    ));
    assert!(!disjoint_for("((first as u32) << 8) | (second as u32)"));

    let shadow_sites: Vec<bool> = found
        .sites
        .iter()
        .filter(|s| s.text == "((first as u32) << 8) | (second as u32)")
        .map(|s| match &s.kind {
            SiteKind::Arith {
                lhs,
                rhs,
                gap,
                local_types,
                shadowed_prims,
                ..
            } => super::disjoint_bits(
                lhs,
                s.start,
                rhs,
                gap.1,
                u32_bounds,
                &all_primitive,
                local_types,
                shadowed_prims,
                &|_, _| None,
            ),
            _ => unreachable!(),
        })
        .collect();
    assert_eq!(shadow_sites, vec![false, false]);

    let cmp_literals: Vec<(&str, LiteralSide)> = found
        .sites
        .iter()
        .filter_map(|s| match &s.kind {
            SiteKind::Cmp { literal, .. } => Some((s.text.as_str(), *literal)),
            _ => None,
        })
        .collect();
    assert_eq!(
        cmp_literals,
        vec![
            ("tag == b'A'", LiteralSide::Rhs(65)),
            ("b'\\n' == tag", LiteralSide::Lhs(10)),
            ("ch == 'Z'", LiteralSide::Rhs(90)),
            ("'0' <= ch", LiteralSide::Lhs(48)),
        ]
    );
}

#[test]
fn disjoint_bits_stays_conservative_on_prim_alias_overloaded_binop_let_chain_and_mod_const() {
    let all_primitive = |_: usize, _: usize| true;
    let u32_bounds = Some((u32::MIN as i128, u32::MAX as i128));

    let check_not_disjoint = |source: &str, snippet: &str| {
        let found = discover(source, 0).unwrap();
        let site = found
            .sites
            .iter()
            .find(|s| s.text == snippet)
            .unwrap_or_else(|| panic!("site `{snippet}` not found in {source}"));
        let SiteKind::Arith {
            lhs,
            rhs,
            gap,
            local_types,
            shadowed_prims,
            ..
        } = &site.kind
        else {
            panic!("expected Arith");
        };
        assert!(
            !super::disjoint_bits(
                lhs,
                site.start,
                rhs,
                gap.1,
                u32_bounds,
                &all_primitive,
                local_types,
                shadowed_prims,
                &|_, _| None,
            ),
            "expected `{snippet}` to stay conservative"
        );
    };

    // 1. Type alias shadowing primitive `u8`.
    check_not_disjoint(
        r#"
#[allow(non_camel_case_types)]
type u8 = u16;
fn f(hi: u8, lo: u8) -> u32 {
    ((hi as u32) << 8) | (lo as u32)
}
"#,
        "((hi as u32) << 8) | (lo as u32)",
    );

    // 2. Overloaded binary op inside cast must not inherit operand width.
    check_not_disjoint(
        r#"
fn f(a: u8, wide: Wide) -> u32 {
    ((a + wide) as u32) | 0x100
}
"#,
        "((a + wide) as u32) | 0x100",
    );

    // 3. `if let` chain shadowing outer `lo: u8` before RHS of `&&`.
    check_not_disjoint(
        r#"
fn f(hi: u8, lo: u8, opt: Option<u16>) -> bool {
    if let Some(lo) = opt && (((hi as u32) << 8) | (lo as u32)) != 0 {
        true
    } else {
        false
    }
}
"#,
        "((hi as u32) << 8) | (lo as u32)",
    );

    // 4. Inline module constant shadowing parent module constant of narrower type.
    check_not_disjoint(
        r#"
const X: u8 = 1;
mod inner {
    const X: u16 = 0x100;
    fn f(hi: u8) -> u32 {
        ((hi as u32) << 8) | (X as u32)
    }
}
"#,
        "((hi as u32) << 8) | (X as u32)",
    );

    // 5. Conditional `#[cfg]` let binding must not override outer type.
    check_not_disjoint(
        r#"
fn f(lo: u16) -> u32 {
    #[cfg(feature = "narrow")]
    let lo = lo as u8;
    0x100u32 | (lo as u32)
}
"#,
        "0x100u32 | (lo as u32)",
    );

    // 6. Inline module `use super::WIDE as LO` must not inherit parent `LO: u8`.
    check_not_disjoint(
        r#"
const LO: u8 = 1;
const WIDE: u16 = 0x100;
mod inner {
    use super::WIDE as LO;
    fn f() -> u32 {
        0x100u32 | (LO as u32)
    }
}
"#,
        "0x100u32 | (LO as u32)",
    );

    // 7. Local `mod core` shadowing `core::primitive::u8`.
    check_not_disjoint(
        r#"
mod core {
    pub mod primitive {
        pub type u8 = u16;
    }
}
fn f(lo: core::primitive::u8) -> u32 {
    0x100u32 | (lo as u32)
}
"#,
        "0x100u32 | (lo as u32)",
    );

    // 8. Signed widening sign-extends into upper bits (`false`), while guaranteed-zero
    // low bits from an outer shift stay disjoint (`true`).
    assert!(!super::disjoint_bits(
        "(a as u32)",
        0,
        "((b as u32) << 8)",
        100,
        u32_bounds,
        &all_primitive,
        &[
            (
                "a".into(),
                super::LocalBitType::Value(super::BitValueType::Signed(8))
            ),
            (
                "b".into(),
                super::LocalBitType::Value(super::BitValueType::Unsigned(8))
            ),
        ],
        &[],
        &|_, _| None,
    ));
    assert!(super::disjoint_bits(
        "((a as u32) << 1)",
        0,
        "1",
        100,
        u32_bounds,
        &all_primitive,
        &[(
            "a".into(),
            super::LocalBitType::Value(super::BitValueType::Signed(8))
        )],
        &[],
        &|_, _| None,
    ));
}

#[test]
fn disjoint_bits_runtime_cast_bounds_matches_pict_table() {
    let table = include_str!("../../tests/data/runtime-cast-disjoint.pict.tsv");
    let all_primitive = |_: usize, _: usize| true;
    let mut rows = 0;
    for line in table.lines().skip(1).filter(|l| !l.trim().is_empty()) {
        let cols: Vec<&str> = line.split('\t').collect();
        assert_eq!(cols.len(), 5, "unexpected row: {line}");
        let (rt_type, dst_type, cast_kind, shift, rhs_kind) =
            (cols[0], cols[1], cols[2], cols[3], cols[4]);

        let outer_bounds = match dst_type {
            "u32" | "alias_word" => Some((u32::MIN as i128, u32::MAX as i128)),
            "i32" => Some((i32::MIN as i128, i32::MAX as i128)),
            other => panic!("unexpected DstType: {other}"),
        };
        let dst_ident = match dst_type {
            "alias_word" => "Word",
            other => other,
        };
        let lhs_rt: Option<(i128, i128)> = match rt_type {
            "u8" => Some((0, 255)),
            "u16" => Some((0, 65535)),
            "i8" => Some((-128, 127)),
            "bool" => Some((0, 1)),
            "char" => Some((0, 0x1f_ffff)),
            "polymorphic" | "none" => None,
            other => panic!("unexpected RuntimeType: {other}"),
        };
        let base_cast = match cast_kind {
            "as_cast" => format!("(first as {dst_ident})"),
            "from_call" => format!("{dst_ident}::from(first)"),
            other => panic!("unexpected CastKind: {other}"),
        };
        let shift_bits: u32 = match shift {
            "shl0" => 0,
            "shl8" => 8,
            "shl16" => 16,
            other => panic!("unexpected Shift: {other}"),
        };
        let lhs = if shift_bits == 0 {
            base_cast
        } else {
            format!("({base_cast} << {shift_bits})")
        };
        let rhs = match rhs_kind {
            "u8_cast" => format!("(second as {dst_ident})"),
            "u8_shl8" => format!("((second as {dst_ident}) << 8)"),
            "lit_1" => "1".to_string(),
            other => panic!("unexpected RhsKind: {other}"),
        };

        // No static local_types at all: only runtime cast_bounds proves the source widths.
        let cast_bounds = |from: usize, _to: usize| -> Option<(i128, i128)> {
            if from < 200 { lhs_rt } else { Some((0, 255)) }
        };
        let actual = super::disjoint_bits(
            &lhs,
            0,
            &rhs,
            200,
            outer_bounds,
            &all_primitive,
            &[],
            &[],
            &cast_bounds,
        );

        let full = 0xffff_ffffu128;
        // User-defined `Word::from` is never assumed to be a primitive widening conversion.
        let lhs_base = if cast_kind == "from_call" && dst_type == "alias_word" {
            full
        } else {
            match rt_type {
                "u8" => 0xff,
                "u16" => 0xffff,
                "bool" => 0x1,
                "char" => 0x1f_ffff,
                "i8" | "polymorphic" | "none" => full,
                _ => unreachable!(),
            }
        };
        let lhs_ones = (lhs_base << shift_bits) & full;
        let rhs_ones = match rhs_kind {
            "u8_cast" => 0xff,
            "u8_shl8" => 0xff00,
            "lit_1" => 1,
            _ => unreachable!(),
        };
        let expected = (lhs_ones & rhs_ones) == 0;
        assert_eq!(
            actual, expected,
            "rt={rt_type} dst={dst_type} kind={cast_kind} shift={shift} rhs={rhs_kind}"
        );
        rows += 1;
    }
    assert!(rows >= 25, "expected full PICT table, got {rows} rows");
}
