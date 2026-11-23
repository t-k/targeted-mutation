//! Properties of the instrumenter that hold for any source file, checked on the sources
//! of this repository and on generated programs.
//!
//! Only the text is looked at here: whether a rewritten file is still a Rust source file,
//! whether it has the lines of the original, whether a mutant is a change. That the
//! rewritten program compiles and does what the original does is what the end-to-end
//! tests and `fixtures/semantics` are for.

use std::path::{Path, PathBuf};

use proptest::prelude::*;
use targeted_mutation::instrument::{Discovery, discover, offset_of, render, source_mutants};

/// What must hold for a source file the instrumenter accepts. `Err` says what does not.
fn check(source: &str) -> Result<(), String> {
    let Ok(found) = discover(source, 0) else {
        return Ok(());
    };
    sites_are_stretches_of_the_source(source, &found)?;
    probes_keep_the_file(source, &found)?;
    mutants_change_the_file(source, &found, 1)
}

fn sites_are_stretches_of_the_source(source: &str, found: &Discovery) -> Result<(), String> {
    for (index, site) in found.sites.iter().enumerate() {
        if site.id != index as u32 {
            return Err(format!("site {index} has the number {}", site.id));
        }
    }
    for site in found.sites.iter().chain(&found.rebuild_sites) {
        let at = format!("{:?} at {}:{}", site.kind, site.line, site.column);
        if site.start > site.end || source.get(site.start..site.end).is_none() {
            return Err(format!("{at} is not a stretch of the source"));
        }
        // A site is reported where its item or expression starts, which for a body is the
        // function and not the block.
        match offset_of(source, site.line, site.column) {
            Some(offset) if offset <= site.start => {}
            reported => return Err(format!("{at} is reported at {reported:?}")),
        }
    }
    Ok(())
}

fn probes_keep_the_file(source: &str, found: &Discovery) -> Result<(), String> {
    let none = render(source, &found.sites, &|_| false);
    if none.text != source {
        return Err("no probe enabled, and the text is another".to_string());
    }
    // All of them, each alone, and every other one: a probe is also written next to and
    // inside others that are left out when they do not compile.
    let alone = (0..found.sites.len() as u32).map(|only| (format!("only {only}"), Some(only)));
    let choices = [("all".to_string(), None)].into_iter().chain(alone);
    for (which, only) in choices {
        let rendered = render(source, &found.sites, &|id| {
            only.is_none_or(|only| only == id)
        });
        probed_text_is_the_file(source, &rendered.text).map_err(|why| format!("{which}: {why}"))?;
    }
    for parity in 0..2 {
        let rendered = render(source, &found.sites, &|id| id % 2 == parity);
        probed_text_is_the_file(source, &rendered.text)
            .map_err(|why| format!("every other from {parity}: {why}"))?;
    }
    Ok(())
}

fn probed_text_is_the_file(source: &str, text: &str) -> Result<(), String> {
    if let Err(error) = syn::parse_file(text) {
        return Err(format!("the probed text does not parse: {error}\n{text}"));
    }
    let (before, after) = (source.matches('\n').count(), text.matches('\n').count());
    if before != after {
        return Err(format!(
            "the probed text has {after} line ends, the source {before}\n{text}"
        ));
    }
    Ok(())
}

/// `every`: 1 for all the mutants, more to take one in so many (a file of this repository
/// has thousands, and each is a file to parse).
fn mutants_change_the_file(source: &str, found: &Discovery, every: usize) -> Result<(), String> {
    let tokens = |text: &str| {
        text.parse::<proc_macro2::TokenStream>()
            .map(|t| t.to_string())
    };
    let original = tokens(source).map_err(|error| format!("the source has no tokens: {error}"))?;
    let sites = found.sites.iter().chain(&found.rebuild_sites);
    let mutants = sites.flat_map(|site| source_mutants(site).into_iter().map(move |m| (site, m)));
    for (site, mutant) in mutants.step_by(every) {
        {
            let name = format!(
                "`{}` into `{}` at {}:{}",
                mutant.what, mutant.into, site.line, site.column
            );
            if mutant.start > mutant.end || source.get(mutant.start..mutant.end).is_none() {
                return Err(format!(
                    "{name} replaces what is not a stretch of the source"
                ));
            }
            let mut text = source.to_string();
            text.replace_range(mutant.start..mutant.end, &mutant.replacement);
            if let Err(error) = syn::parse_file(&text) {
                return Err(format!("{name} does not parse: {error}\n{text}"));
            }
            if tokens(&text).ok().as_ref() == Some(&original) {
                return Err(format!("{name} is the program it was made from"));
            }
        }
    }
    Ok(())
}

fn rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            if name != "target" && !name.to_string_lossy().starts_with('.') {
                rust_files(&path, files);
            }
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

#[test]
fn the_sources_of_this_repository() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = vec![];
    for dir in ["src", "tests", "fixtures"] {
        rust_files(&root.join(dir), &mut files);
    }
    assert!(files.len() > 30, "{} files", files.len());
    let mut sites = 0;
    for file in files {
        let source = std::fs::read_to_string(&file).unwrap();
        sites += discover(&source, 0).map_or(0, |found| found.sites.len());
        if let Err(why) = check_whole(&source) {
            panic!("{}: {why}", file.display());
        }
    }
    assert!(sites > 5000, "{sites} sites");
}

/// `check` without the probes taken one at a time: a file of this repository has
/// thousands of them.
fn check_whole(source: &str) -> Result<(), String> {
    let Ok(found) = discover(source, 0) else {
        return Ok(());
    };
    sites_are_stretches_of_the_source(source, &found)?;
    if render(source, &found.sites, &|_| false).text != source {
        return Err("no probe enabled, and the text is another".to_string());
    }
    for modulus in [1, 2, 3, 7] {
        for rest in 0..modulus {
            let rendered = render(source, &found.sites, &|id| id % modulus == rest);
            probed_text_is_the_file(source, &rendered.text)
                .map_err(|why| format!("every {modulus}th from {rest}: {}", first_lines(&why)))?;
        }
    }
    let mutants = found.sites.len() + found.rebuild_sites.len();
    mutants_change_the_file(source, &found, mutants / 40 + 1).map_err(|why| first_lines(&why))
}

fn first_lines(text: &str) -> String {
    text.lines().take(3).collect::<Vec<_>>().join("\n")
}

/// White space as it is written between two tokens: nothing more than a space, a line
/// end, a comment.
fn gap() -> impl Strategy<Value = String> {
    prop_oneof![
        6 => Just(" "),
        2 => Just("\n        "),
        1 => Just(" /* why */ "),
        1 => Just(" // why\n        "),
    ]
    .prop_map(str::to_string)
}

/// An expression that is one operand whatever stands next to it.
fn atom() -> impl Strategy<Value = String> {
    prop_oneof![
        Just("a"),
        Just("b"),
        Just("n"),
        Just("flag"),
        Just("v"),
        Just("text"),
        Just("self.x"),
        Just("0"),
        Just("1"),
        Just("2u8"),
        Just("1.5"),
        Just("true"),
        Just("false"),
        Just("\"t\""),
        Just("'c'"),
        Just("b'x'"),
        Just("None"),
        Just("LIMIT"),
        Just("u8::MAX"),
        Just("v.len()"),
        Just("v.is_empty()"),
        Just("v.first()"),
        Just("v.last()"),
        Just("self.items.last()"),
        Just("v.iter().last()"),
        Just("text.chars().last()"),
        Just("line!()"),
        Just("vec![]"),
        Just("String::new()"),
        Just("Default::default()"),
        Just("a.min(b)"),
        Just("a.max(b)"),
        Just("Ok(a)"),
        Just("Err(text.to_string())"),
    ]
    .prop_map(str::to_string)
}

fn assignment() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("="),
        Just("+="),
        Just("-="),
        Just("*="),
        Just("/="),
        Just("%="),
        Just("<<="),
        Just(">>="),
        Just("&="),
        Just("|="),
        Just("^="),
    ]
}

fn binary() -> impl Strategy<Value = &'static str> {
    prop_oneof![
        Just("+"),
        Just("-"),
        Just("*"),
        Just("/"),
        Just("%"),
        Just("<<"),
        Just(">>"),
        Just("&"),
        Just("|"),
        Just("^"),
        Just("<"),
        Just("<="),
        Just(">"),
        Just(">="),
        Just("=="),
        Just("!="),
        Just("&&"),
        Just("||"),
        Just(".."),
        Just("..="),
    ]
}

/// Expressions, each of them one operand: what is not one by itself is in parentheses,
/// so that any two of them can be put either side of any operator.
fn expression() -> impl Strategy<Value = String> {
    atom().prop_recursive(5, 48, 4, |inner| {
        let e = || inner.clone();
        prop_oneof![
            4 => (e(), gap(), binary(), gap(), e())
                .prop_map(|(l, before, op, after, r)| format!("({l}{before}{op}{after}{r})")),
            1 => (e(), binary(), e(), binary(), e())
                .prop_map(|(a, p, b, q, c)| format!("({a} {p} {b} {q} {c})"))
                .prop_filter("an expression", |text| syn::parse_str::<syn::Expr>(text).is_ok()),
            1 => e().prop_map(|x| format!("(-{x})")),
            1 => e().prop_map(|x| format!("(!{x})")),
            1 => e().prop_map(|x| format!("(*{x})")),
            1 => e().prop_map(|x| format!("(&{x})")),
            1 => e().prop_map(|x| format!("({x} as u8)")),
            1 => e().prop_map(|x| format!("({x}?)")),
            1 => e().prop_map(|x| format!("({x})")),
            1 => e().prop_map(|x| format!("{x}.first()")),
            1 => (e(), gap()).prop_map(|(x, gap)| format!("{x}{gap}.last()")),
            1 => (e(), gap()).prop_map(|(x, gap)| format!("{x}.last({gap})")),
            1 => e().prop_map(|x| format!("{x}.unwrap()")),
            1 => (e(), e(), gap()).prop_map(|(x, y, gap)| format!("{x}.min({gap}{y})")),
            1 => (e(), e()).prop_map(|(x, y)| format!("{x}.max({y},)")),
            1 => (e(), e()).prop_map(|(x, y)| format!("{x}.iter().map(|it| {y}).sum::<i32>()")),
            1 => (e(), e()).prop_map(|(x, y)| format!("{x}[{y}]")),
            1 => (e(), e()).prop_map(|(x, y)| format!("call({x}, {y})")),
            1 => (e(), e()).prop_map(|(x, y)| format!("Self::of::<u8>({x}, {y},)")),
            1 => (e(), e()).prop_map(|(x, y)| format!("vec![{x}, {y}]")),
            1 => (e(), e()).prop_map(|(x, y)| format!("[{x}, {y}]")),
            1 => (e(), e()).prop_map(|(x, y)| format!("({x}, {y})")),
            1 => e().prop_map(|x| format!("Some({x})")),
            1 => (e(), e()).prop_map(|(x, y)| format!("Pair {{ x: {x}, #[cfg(unix)] y: {y} }}")),
            1 => (e(), e()).prop_map(|(x, y)| format!("format!(\"{{}}{{y}}\", {x}, y = {y})")),
            1 => (e(), e()).prop_map(|(x, y)| format!("matches!({x}, Some(0) if {y})")),
            2 => (e(), e(), e())
                .prop_map(|(c, x, y)| format!("(if {c} {{ {x} }} else {{ {y} }})")),
            1 => (e(), e(), e())
                .prop_map(|(c, x, y)| format!("(if let Some(it) = {c} {{ {x} }} else {{ {y} }})")),
            2 => (e(), e(), e(), e(), gap()).prop_map(|(on, c, x, y, gap)| format!(
                "(match {on} {{ 0 | 1 => {x},{gap}it if {c} => {y}, #[cfg(test)] 9 => 0, _ => {x} }})"
            )),
            1 => (e(), e()).prop_map(|(x, y)| format!("{{ let it = {x}; {y} }}")),
            1 => (e(), e()).prop_map(|(x, y)| format!("(|it: u8| {x})({y})")),
            1 => e().prop_map(|x| format!("(move || -> u8 {{ {x} }})()")),
            1 => e().prop_map(|x| format!("('l: loop {{ break 'l {x}; }})")),
            1 => e().prop_map(|x| format!("unsafe {{ {x} }}")),
            1 => e().prop_map(|x| format!("(return {x})")),
            1 => e().prop_map(|x| format!("{x}.await")),
        ]
    })
}

fn statement() -> impl Strategy<Value = String> {
    let e = expression;
    prop_oneof![
        3 => e().prop_map(|x| format!("let it = {x};")),
        1 => e().prop_map(|x| format!("let mut it: u8 = {x};")),
        1 => (e(), e()).prop_map(|(x, y)| format!("let Some(it) = {x} else {{ return {y}; }};")),
        1 => e().prop_map(|x| format!("#[cfg(feature = \"extra\")] let it = {x};")),
        1 => (assignment(), e()).prop_map(|(op, x)| format!("n {op} {x};")),
        1 => e().prop_map(|x| format!("self.x = {x};")),
        1 => e().prop_map(|x| format!("{x};")),
        1 => (e(), e()).prop_map(|(c, x)| format!("if {c} {{ return {x}; }}")),
        1 => (e(), e()).prop_map(|(c, x)| format!("while {c} {{ {x}; continue; }}")),
        1 => (e(), e()).prop_map(|(x, y)| format!("for it in {x} {{ if {y} {{ break; }} }}")),
        1 => e().prop_map(|x| format!("assert!({x});")),
        1 => (e(), e()).prop_map(|(x, y)| format!("assert_eq!({x}, {y}, \"why\");")),
        1 => (e(), e()).prop_map(|(x, y)| format!("debug_assert_ne!({x},\n {y});")),
        1 => e().prop_map(|x| format!("println!(\"{{}}\", {x});")),
    ]
}

fn function() -> impl Strategy<Value = String> {
    let head = prop_oneof![
        4 => Just("fn"), 2 => Just("pub fn"), 1 => Just("const fn"), 1 => Just("async fn"),
        1 => Just("pub(crate) unsafe fn"), 1 => Just("#[inline]\nfn"),
        1 => Just("#[cfg(test)]\nfn"), 1 => Just("#[test]\nfn"),
        1 => Just("/// Says.\n#[must_use]\npub fn"), 1 => Just("#[cfg(not(test))] fn"),
    ];
    let parameters = prop_oneof![
        Just("a: i32, b: i32"),
        Just("&self, a: u8, v: &[u8]"),
        Just("&mut self, n: usize"),
        Just("v: Vec<u8>, text: &str, flag: bool"),
        Just(""),
        Just("mut n: u64, (a, b): (u8, u8),"),
    ];
    let returns = prop_oneof![
        Just(" -> i32"),
        Just(" -> bool"),
        Just(" -> Option<u8>"),
        Just(""),
        Just(" -> ()"),
        Just(" -> Result<u32, String>"),
        Just(" -> String"),
        Just(" -> Vec<u8>"),
        Just(" -> impl Iterator<Item = u8>"),
        Just(" -> &'static str"),
        Just(" -> (u8, bool)"),
        Just(" -> Option<&u8>"),
        Just(" -> usize"),
        Just(" -> f64"),
        Just(" -> Self"),
    ];
    let body = (
        prop::collection::vec(statement(), 0..4),
        prop::option::of(expression()),
    );
    (head, "[a-z]{1,3}", parameters, returns, body).prop_map(
        |(head, name, parameters, returns, (statements, last))| {
            let mut text = format!("{head} f_{name}({parameters}){returns} {{\n");
            for statement in statements {
                text.push_str(&format!("    {statement}\n"));
            }
            if let Some(last) = last {
                text.push_str(&format!("    {last}\n"));
            }
            text + "}\n"
        },
    )
}

/// A source file: functions, on their own and as methods, with or without the tests'
/// module.
fn program() -> impl Strategy<Value = String> {
    let item = (function(), 0..5u8).prop_map(|(function, place)| match place {
        0 | 1 => function,
        2 => format!("impl Pair {{\n{function}}}\n"),
        3 => format!("trait Says {{\n{function}}}\n"),
        _ => format!("#[cfg(test)]\nmod tests {{\n    use super::*;\n{function}}}\n"),
    });
    prop::collection::vec(item, 1..4).prop_map(|items| {
        let mut text =
            "const LIMIT: u8 = 3;\nstruct Pair { x: u8, y: u8, items: Vec<u8> }\n".to_string();
        for item in items {
            text.push_str(&item);
        }
        text
    })
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2000, max_shrink_iters: 300, ..ProptestConfig::default() })]

    #[test]
    fn generated_programs(source in program()) {
        // Nearly all of them parse (what stands either side of an operator is in
        // parentheses), and one that does not has no sites: `check` passes it.
        if let Err(why) = check(&source) {
            return Err(TestCaseError::fail(format!("{why}\nin\n{source}")));
        }
    }
}
