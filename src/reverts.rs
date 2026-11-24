//! A change undone as a mutant (`--since` with mutants built one by one): the code as it
//! was before, put back where the new code is. Tests that still pass do not tell the
//! change from what was there.

use std::collections::{HashMap, HashSet};

use proc_macro2::TokenTree;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use crate::instrument;
use crate::scope::{self, Hunk};

/// Cap AST sub-hunks per diff hunk to avoid quadratic rebuild blowup on giant new files.
const MAX_SUBHUNKS_PER_HUNK: usize = 16;

/// One hunk (or AST sub-hunk) of a file that is worth undoing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Change {
    pub hunk: Hunk,
    /// The first line of the change in the new text (1-based).
    pub line: usize,
    /// The new code of the change, or the line the old code was taken from before.
    pub code: String,
    /// The file with this change alone undone; `None` when that is not Rust any more.
    pub undone: Option<String>,
    /// Where the change is in the file as it is now, and what is put there to undo
    /// it: the old lines, with the lines of the tests as they are now.
    pub range: (usize, usize),
    pub put_back: String,
    /// Set by who asks: the change is of code the build leaves out now and undone. It
    /// is not tried alone, and still goes back with the others.
    pub outside: bool,
    /// When `Some`, this change is an AST sub-hunk decomposed from a larger hunk, holding
    /// the human-readable description for findings.
    pub subhunk: Option<String>,
}

impl Change {
    pub fn is_subhunk(&self) -> bool {
        self.subhunk.is_some()
    }

    /// What the mutant is, in the words of a finding.
    pub fn describe(&self) -> String {
        if let Some(description) = &self.subhunk {
            return description.clone();
        }
        let old = self.hunk.old.trim();
        let new_lines = self.hunk.count;
        match (old.is_empty(), new_lines) {
            (true, 1) => "without this line (it is new)".to_string(),
            (true, _) => format!("without these {new_lines} lines (they are new)"),
            (false, 0) => format!(
                "with the code that was removed here: `{}`",
                crate::report::one_line(old)
            ),
            (false, _) => format!(
                "with the code as it was: `{}`",
                crate::report::one_line(old)
            ),
        }
    }
}

impl Change {
    /// Whether undoing the change gives the same program for the build: then there is
    /// nothing to try. `off_now` are the ranges the build leaves out of the file as it
    /// is, `off_then` those of the file with this change undone (`None` when that is
    /// not known: the change is then tried). A change of what decides it (a `cfg` put
    /// on, taken off or reworded) leaves other code out before than after.
    #[cfg(test)]
    pub fn outside_the_build(
        &self,
        current: &str,
        off_now: &[(usize, usize)],
        off_then: Option<&[(usize, usize)]>,
    ) -> bool {
        match (&self.undone, off_then) {
            (Some(undone), Some(off_then)) => same_build(current, off_now, undone, off_then),
            _ => false,
        }
    }
}

/// Whether two texts of a file are the same program for a build that leaves these
/// ranges of each out: the same tokens in what is left. Where what is left asks where
/// it is in the file, any other text is another program.
pub(crate) fn same_build(
    now: &str,
    off_now: &[(usize, usize)],
    then: &str,
    off_then: &[(usize, usize)],
) -> bool {
    let built = |text: &str, off: &[(usize, usize)]| {
        // The outermost first: an attribute that is on may begin an item that is off.
        let mut ranges = off.to_vec();
        ranges.sort_unstable_by_key(|(from, to)| (*from, std::cmp::Reverse(*to)));
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for (from, to) in ranges {
            if from >= at && to <= text.len() {
                out.push_str(&text[at..from]);
                // In its place a name no program has: what is left out of a list
                // goes with its comma when the tokens are compared.
                out.push(' ');
                out.push_str(instrument::LEFT_OUT);
                at = to;
                out.push(' ');
            }
        }
        out.push_str(&text[at..]);
        out
    };
    let (built_now, built_then) = (built(now, off_now), built(then, off_then));
    if instrument::same_built_tokens(&built_now, &built_then) != Some(true) {
        return false;
    }
    // Code that asks where it is can tell a line more above it, or a column more
    // before it on its line. `line!` and `column!` say where they are themselves: those
    // have to be where they were. The location of a caller can be that of any call:
    // then every token that is kept has to be.
    let asked = asks_where_it_is(&built_now).max(asks_where_it_is(&built_then));
    if asked == Asks::Nothing {
        return true;
    }
    matches!(
        (places(now, off_now, asked), places(then, off_then, asked)),
        (Some(a), Some(b)) if a == b
    )
}

/// What code asks of its own place in the file.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Asks {
    Nothing,
    /// `line!` or `column!`: where the macro is written.
    ItsOwnPlace,
    /// `#[track_caller]` or `Location::caller`: where something is called from.
    APlaceOfACall,
}

type Trees = std::iter::Peekable<proc_macro2::token_stream::IntoIter>;

fn next_is(trees: &mut Trees, mark: char) -> bool {
    matches!(trees.peek(), Some(proc_macro2::TokenTree::Punct(p)) if p.as_char() == mark)
}

/// The line and column of the tokens of a text that are not in the ranges left out and
/// whose place may be asked for: every one (documentation aside), or the `line!` and
/// `column!` alone. `None` when the text does not lex.
fn places(text: &str, off: &[(usize, usize)], asked: Asks) -> Option<Vec<(usize, usize)>> {
    /// Whether the token is kept; then its place is noted.
    fn note(
        span: proc_macro2::Span,
        off: &[(usize, usize)],
        out: &mut Vec<(usize, usize)>,
    ) -> bool {
        let range = span.byte_range();
        let kept = !off
            .iter()
            .any(|(from, to)| *from <= range.start && range.end <= *to);
        if kept {
            out.push((span.start().line, span.start().column));
        }
        kept
    }
    fn walk(
        stream: proc_macro2::TokenStream,
        off: &[(usize, usize)],
        all: bool,
        out: &mut Vec<(usize, usize)>,
    ) {
        let mut trees: Trees = stream.into_iter().peekable();
        // Whether the last token was left out: its comma or semicolon goes with it,
        // as where the tokens are compared.
        let mut after_left_out = false;
        while let Some(tree) = trees.next() {
            let was_left_out = after_left_out;
            after_left_out = false;
            match tree {
                proc_macro2::TokenTree::Punct(hash) if hash.as_char() == '#' => {
                    // Documentation, as a comment or written out, outer or inner, is
                    // no place of the program (as it is no token of it).
                    let mut ahead = trees.clone();
                    if next_is(&mut ahead, '!') {
                        ahead.next();
                    }
                    if matches!(ahead.peek(), Some(proc_macro2::TokenTree::Group(group))
                        if group.delimiter() == proc_macro2::Delimiter::Bracket
                            && matches!(group.stream().into_iter().next(),
                                Some(proc_macro2::TokenTree::Ident(name)) if name == "doc"))
                    {
                        ahead.next();
                        trees = ahead;
                        continue;
                    }
                    if all {
                        after_left_out = !note(hash.span(), off, out);
                    } else {
                        after_left_out = was_left_out;
                    }
                }
                proc_macro2::TokenTree::Group(group) => {
                    if all {
                        after_left_out = !note(group.span(), off, &mut vec![]);
                        if !after_left_out {
                            note(group.span_open(), off, out);
                        }
                    }
                    walk(group.stream(), off, all, out);
                    if all && !after_left_out {
                        note(group.span_close(), off, out);
                    }
                }
                proc_macro2::TokenTree::Ident(name) => {
                    let asks_here =
                        (name == "line" || name == "column") && next_is(&mut trees, '!');
                    if all {
                        after_left_out = !note(name.span(), off, out);
                    } else if asks_here {
                        // Of what it asks for alone: a line more above a `column!`
                        // moves nothing it can tell.
                        let mut place = vec![];
                        note(name.span(), off, &mut place);
                        out.extend(place.into_iter().map(|(line, column)| {
                            if name == "line" {
                                (line, 0)
                            } else {
                                (0, column)
                            }
                        }));
                    }
                }
                proc_macro2::TokenTree::Punct(mark)
                    if was_left_out && matches!(mark.as_char(), ',' | ';') => {}
                other => {
                    if all {
                        after_left_out = !note(other.span(), off, out);
                    }
                }
            }
        }
    }
    let mut out = vec![];
    walk(
        text.parse::<proc_macro2::TokenStream>().ok()?,
        off,
        asked == Asks::APlaceOfACall,
        &mut out,
    );
    Some(out)
}

/// Whether code uses its own place in the file: `line!`, `column!`, the location of a
/// caller (`#[track_caller]`, `Location::caller`). A macro of another file that does is
/// not seen.
fn asks_where_it_is(text: &str) -> Asks {
    fn scan(stream: proc_macro2::TokenStream, found: &mut Asks) {
        let mut trees: Trees = stream.into_iter().peekable();
        while let Some(tree) = trees.next() {
            match tree {
                proc_macro2::TokenTree::Ident(name) => {
                    if name == "track_caller" {
                        *found = Asks::APlaceOfACall;
                    } else if name == "Location" && next_is(&mut trees, ':') {
                        // `Location::caller`: a type of that name alone asks nothing.
                        let mut ahead = trees.clone();
                        ahead.next();
                        ahead.next();
                        if matches!(ahead.next(), Some(proc_macro2::TokenTree::Ident(name)) if name == "caller")
                        {
                            *found = Asks::APlaceOfACall;
                        }
                    } else if (name == "line" || name == "column") && next_is(&mut trees, '!') {
                        *found = (*found).max(Asks::ItsOwnPlace);
                    }
                }
                proc_macro2::TokenTree::Group(group) => scan(group.stream(), found),
                _ => {}
            }
        }
    }
    // What does not lex is not known not to.
    let Ok(stream) = text.parse::<proc_macro2::TokenStream>() else {
        return Asks::APlaceOfACall;
    };
    let mut found = Asks::Nothing;
    scan(stream, &mut found);
    found
}

/// The ranges, each to the end of its last line when only a comment follows there: the
/// comment after a test is of the test's line.
fn with_trailing_comments(text: &str, ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    ranges
        .into_iter()
        .map(|(start, end)| {
            let rest = text[end..].lines().next().unwrap_or("");
            let after = rest.trim_start();
            if after.starts_with("//") {
                (start, end + rest.len())
            } else {
                (start, end)
            }
        })
        .collect()
}

/// What a hunk of a file changes.
enum Kind {
    /// The program: worth trying undone.
    Program(Change),
    /// Code of the tests alone.
    Tests,
    /// The program and its tests in lines that cannot be told apart: undone as a whole
    /// it would put the old tests back too, which says nothing of the tests as they are.
    Mixed,
    /// No token (comments, documentation, spacing), or nothing that fits the text.
    Nothing,
}

fn classify(current: &str, hunks: &[Hunk], include_subhunks: bool) -> Vec<(Hunk, Kind)> {
    let Ok(parsed) = instrument::parse_source(current) else {
        return vec![];
    };
    let tests_now = with_trailing_comments(current, instrument::test_only_ranges(current, &parsed));
    let lines = current.matches('\n').count() + 1;
    let mut ordered: Vec<&Hunk> = hunks.iter().collect();
    ordered.sort_by_key(|hunk| (hunk.start, hunk.count));
    let mut found = vec![];
    for hunk in ordered {
        let mut kind = |kind| found.push((hunk.clone(), kind));
        let Some(undone) = scope::undo(current, &[hunk]) else {
            kind(Kind::Nothing);
            continue;
        };
        if instrument::same_tokens(current, &undone) == Some(true) {
            kind(Kind::Nothing);
            continue;
        }
        // Where the new lines are, and where the old ones are in the undone text.
        let (from, to) = scope::range_of(current, hunk);
        let inside = |ranges: &[(usize, usize)], from: usize, to: usize| {
            ranges
                .iter()
                .any(|(start, end)| *start <= from && to <= *end)
        };
        let touches = |ranges: &[(usize, usize)], from: usize, to: usize| {
            ranges.iter().any(|(start, end)| *start < to && from < *end)
        };
        // The code of each side without the blank lines around it, which belong to
        // nothing.
        let trimmed = |text: &str, at: usize| {
            let spacing = text.len() - text.trim_start().len();
            (at + spacing, at + text.trim_end().len())
        };
        let undone_parsed = instrument::parse_source(&undone).ok();
        let (new_from, new_to) = trimmed(&current[from..to], from);
        let (old_from, old_to) = trimmed(&hunk.old, from);
        let tests_then = undone_parsed.as_ref().map(|parsed| {
            with_trailing_comments(&undone, instrument::test_only_ranges(&undone, parsed))
        });
        let new_empty = new_from >= new_to;
        let old_empty = old_from >= old_to;
        if new_empty && old_empty {
            kind(Kind::Nothing);
            continue;
        }
        // Each side: all of it code of the tests, some of it, or none.
        let new_all = new_empty || inside(&tests_now, new_from, new_to);
        let new_some = !new_empty && touches(&tests_now, new_from, new_to);
        let (old_all, old_some) = match &tests_then {
            _ if old_empty => (true, false),
            Some(tests) => (
                inside(tests, old_from, old_to),
                touches(tests, old_from, old_to),
            ),
            // Old lines that are not Rust alone: where they go says what they are.
            None => {
                let around =
                    inside(&tests_now, from, to) || (new_empty && inside(&tests_now, from, from));
                (around, around)
            }
        };
        if new_all && old_all {
            kind(Kind::Tests);
            continue;
        }
        let mut undone = undone;
        let mut undone_parsed = undone_parsed;
        if new_some || old_some {
            // Line for line the tests can be kept as they are and the program put
            // back; otherwise the two cannot be told apart.
            let Some(split) = keep_tests(current, hunk, from, to, &tests_now) else {
                kind(Kind::Mixed);
                continue;
            };
            if instrument::same_tokens(current, &split) == Some(true) {
                kind(Kind::Nothing);
                continue;
            }
            undone_parsed = instrument::parse_source(&split).ok();
            undone = split;
        }
        let line = if hunk.count == 0 {
            (hunk.start + 1).min(lines)
        } else {
            hunk.start
        };
        let code_line = current[scope::range_of(
            current,
            &Hunk {
                start: line,
                count: 1,
                ..hunk.clone()
            },
        )
        .0..]
            .lines()
            .find(|text| !text.trim().is_empty())
            .unwrap_or("");
        // What stands between the text before the change and the text after it.
        let put_back = undone[from..undone.len() - (current.len() - to)].to_string();
        let undone_for_subhunks = undone_parsed.as_ref().map(|tree| (undone.as_str(), tree));
        let whole = Change {
            hunk: hunk.clone(),
            line,
            code: crate::report::one_line(code_line.trim()),
            undone: undone_parsed.as_ref().map(|_| undone.clone()),
            range: (from, to),
            put_back,
            outside: false,
            subhunk: None,
        };
        let subhunks =
            if include_subhunks && let Some((undone_text, undone_tree)) = undone_for_subhunks {
                subhunk_changes(
                    current,
                    &parsed,
                    &tests_now,
                    undone_text,
                    undone_tree,
                    &whole,
                )
            } else {
                vec![]
            };
        kind(Kind::Program(whole));
        for sub in subhunks {
            kind(Kind::Program(sub));
        }
    }
    found
}

/// `current` with the lines of a hunk put back that are not code of the tests, when the
/// hunk has as many old lines as new ones: each old line then stands for the new line
/// at its place. `None` when it has not, or no line of it is the program's.
fn keep_tests(
    current: &str,
    hunk: &Hunk,
    from: usize,
    to: usize,
    tests_now: &[(usize, usize)],
) -> Option<String> {
    let new_lines: Vec<&str> = current[from..to].split_inclusive('\n').collect();
    let old_lines: Vec<&str> = hunk.old.split_inclusive('\n').collect();
    if new_lines.len() != old_lines.len() || new_lines.is_empty() {
        return None;
    }
    let mut out = String::from(&current[..from]);
    let mut at = from;
    let mut put_back = 0;
    for (new, old) in new_lines.iter().zip(&old_lines) {
        let code = new.trim();
        let start = at + (new.len() - new.trim_start().len());
        let of_tests = code.is_empty()
            || tests_now
                .iter()
                .any(|(first, last)| *first <= start && start + code.len() <= *last);
        if of_tests {
            out.push_str(new);
        } else {
            out.push_str(old);
            put_back += 1;
        }
        at += new.len();
    }
    out.push_str(&current[to..]);
    (put_back > 0).then_some(out)
}

/// The changes of one file to try undone, in the order of the file. Left out: a hunk that
/// changes no token (comments, documentation, spacing), and one that only touches code
/// of the tests. `current` is the file as it is now, and the hunks are of it.
pub(crate) fn changes(current: &str, hunks: &[Hunk]) -> Vec<Change> {
    classify(current, hunks, true)
        .into_iter()
        .filter_map(|(_, kind)| match kind {
            Kind::Program(change) => Some(change),
            Kind::Tests | Kind::Mixed | Kind::Nothing => None,
        })
        .collect()
}

/// How many hunks of a file change the program and its tests in lines that cannot be
/// told apart: nothing is said of them.
pub(crate) fn mixed(current: &str, hunks: &[Hunk]) -> usize {
    classify(current, hunks, false)
        .iter()
        .filter(|(_, kind)| matches!(kind, Kind::Mixed))
        .count()
}

/// The hunks of a file that change its tests and nothing else.
pub(crate) fn test_hunks(current: &str, hunks: &[Hunk]) -> Vec<Hunk> {
    classify(current, hunks, false)
        .into_iter()
        .filter_map(|(hunk, kind)| matches!(kind, Kind::Tests).then_some(hunk))
        .collect()
}

/// The file with all of these changes undone; `None` when that is not Rust.
/// Sub-hunks are skipped because their ranges overlap the whole hunks they were split from.
pub(crate) fn all_undone(current: &str, changes: &[Change]) -> Option<String> {
    // Each as it is undone alone: the tests stay as they are, here too.
    let mut ordered: Vec<&Change> = changes
        .iter()
        .filter(|change| !change.is_subhunk())
        .collect();
    ordered.sort_by_key(|change| std::cmp::Reverse(change.range));
    let mut undone = current.to_string();
    let mut before = current.len();
    for change in ordered {
        let (from, to) = change.range;
        if to > before {
            return None;
        }
        undone.replace_range(from..to, &change.put_back);
        before = from;
    }
    instrument::parse_source(&undone).ok().map(|_| undone)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LogicalOp {
    And,
    Or,
}

fn logical_op(op: &syn::BinOp) -> Option<LogicalOp> {
    match op {
        syn::BinOp::And(_) => Some(LogicalOp::And),
        syn::BinOp::Or(_) => Some(LogicalOp::Or),
        _ => None,
    }
}

fn flatten_logical<'a>(expr: &'a syn::Expr, op: LogicalOp, out: &mut Vec<&'a syn::Expr>) {
    if let syn::Expr::Binary(bin) = expr
        && logical_op(&bin.op) == Some(op)
    {
        flatten_logical(&bin.left, op, out);
        flatten_logical(&bin.right, op, out);
    } else {
        out.push(expr);
    }
}

fn token_sig(text: &str) -> String {
    fn push_stream(stream: proc_macro2::TokenStream, out: &mut Vec<String>) {
        for tree in stream {
            match tree {
                TokenTree::Group(group) => {
                    let (open, close) = match group.delimiter() {
                        proc_macro2::Delimiter::Parenthesis => ("(", ")"),
                        proc_macro2::Delimiter::Brace => ("{", "}"),
                        proc_macro2::Delimiter::Bracket => ("[", "]"),
                        proc_macro2::Delimiter::None => ("", ""),
                    };
                    if !open.is_empty() {
                        out.push(open.to_string());
                    }
                    push_stream(group.stream(), out);
                    if !close.is_empty() {
                        out.push(close.to_string());
                    }
                }
                TokenTree::Ident(ident) => out.push(ident.to_string()),
                TokenTree::Punct(punct) => out.push(punct.as_char().to_string()),
                TokenTree::Literal(lit) => out.push(lit.to_string()),
            }
        }
    }
    let Ok(stream) = text.parse::<proc_macro2::TokenStream>() else {
        return text.trim().to_string();
    };
    let mut parts = Vec::new();
    push_stream(stream, &mut parts);
    parts.join(" ")
}

/// Matches `old_sigs` as an in-order subsequence of `new_sigs` and returns the indices in
/// `new_sigs` that were added. Returns `None` if `old_sigs` is not a subsequence or if the
/// subsequence alignment is ambiguous (leftmost and rightmost matches disagree), so existing
/// elements are never misclassified as newly added.
fn subsequence_added_indices(old_sigs: &[String], new_sigs: &[String]) -> Option<Vec<usize>> {
    if old_sigs.is_empty() {
        return Some((0..new_sigs.len()).collect());
    }
    if old_sigs.len() > new_sigs.len() {
        return None;
    }
    let mut left_matched = Vec::with_capacity(old_sigs.len());
    let mut oi = 0;
    for (ni, sig) in new_sigs.iter().enumerate() {
        if oi < old_sigs.len() && sig == &old_sigs[oi] {
            left_matched.push(ni);
            oi += 1;
        }
    }
    if oi != old_sigs.len() {
        return None;
    }
    let mut right_matched = Vec::with_capacity(old_sigs.len());
    let mut oi = old_sigs.len();
    for (ni, sig) in new_sigs.iter().enumerate().rev() {
        if oi > 0 && sig == &old_sigs[oi - 1] {
            right_matched.push(ni);
            oi -= 1;
        }
    }
    right_matched.reverse();
    if left_matched != right_matched {
        return None;
    }
    let matched: HashSet<usize> = left_matched.into_iter().collect();
    Some(
        (0..new_sigs.len())
            .filter(|i| !matched.contains(i))
            .collect(),
    )
}

fn span_range(source: &str, span: proc_macro2::Span) -> Option<(usize, usize)> {
    let range = span.byte_range();
    (!range.is_empty() && source.get(range.clone()).is_some()).then_some((range.start, range.end))
}

fn overlaps(range: (usize, usize), window: (usize, usize)) -> bool {
    range.0 < window.1 && window.0 < range.1
}

fn contained_in(range: (usize, usize), window: (usize, usize)) -> bool {
    window.0 <= range.0 && range.1 <= window.1
}

fn touches_any(range: (usize, usize), blocked: &[(usize, usize)]) -> bool {
    blocked.iter().any(|&b| overlaps(range, b))
}

fn needs_separator_space(source: &str, del_start: usize, del_end: usize) -> bool {
    let before = source[..del_start].chars().next_back();
    let after = source[del_end..].chars().next();
    let glues =
        |c: char| !c.is_whitespace() && !matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';');
    before.is_some_and(glues) && after.is_some_and(glues)
}

fn expr_attrs(expr: &syn::Expr) -> &[syn::Attribute] {
    match expr {
        syn::Expr::Array(e) => &e.attrs,
        syn::Expr::Assign(e) => &e.attrs,
        syn::Expr::Async(e) => &e.attrs,
        syn::Expr::Await(e) => &e.attrs,
        syn::Expr::Binary(e) => &e.attrs,
        syn::Expr::Block(e) => &e.attrs,
        syn::Expr::Break(e) => &e.attrs,
        syn::Expr::Call(e) => &e.attrs,
        syn::Expr::Cast(e) => &e.attrs,
        syn::Expr::Closure(e) => &e.attrs,
        syn::Expr::Const(e) => &e.attrs,
        syn::Expr::Continue(e) => &e.attrs,
        syn::Expr::Field(e) => &e.attrs,
        syn::Expr::ForLoop(e) => &e.attrs,
        syn::Expr::Group(e) => &e.attrs,
        syn::Expr::If(e) => &e.attrs,
        syn::Expr::Index(e) => &e.attrs,
        syn::Expr::Infer(e) => &e.attrs,
        syn::Expr::Let(e) => &e.attrs,
        syn::Expr::Lit(e) => &e.attrs,
        syn::Expr::Loop(e) => &e.attrs,
        syn::Expr::Macro(e) => &e.attrs,
        syn::Expr::Match(e) => &e.attrs,
        syn::Expr::MethodCall(e) => &e.attrs,
        syn::Expr::Paren(e) => &e.attrs,
        syn::Expr::Path(e) => &e.attrs,
        syn::Expr::Range(e) => &e.attrs,
        syn::Expr::RawAddr(e) => &e.attrs,
        syn::Expr::Reference(e) => &e.attrs,
        syn::Expr::Repeat(e) => &e.attrs,
        syn::Expr::Return(e) => &e.attrs,
        syn::Expr::Struct(e) => &e.attrs,
        syn::Expr::Try(e) => &e.attrs,
        syn::Expr::TryBlock(e) => &e.attrs,
        syn::Expr::Tuple(e) => &e.attrs,
        syn::Expr::Unary(e) => &e.attrs,
        syn::Expr::Unsafe(e) => &e.attrs,
        syn::Expr::While(e) => &e.attrs,
        syn::Expr::Yield(e) => &e.attrs,
        _ => &[],
    }
}

fn expr_has_skip_or_test(expr: &syn::Expr) -> bool {
    let attrs = expr_attrs(expr);
    if instrument::is_test_only(attrs) || instrument::is_skipped(attrs) {
        return true;
    }
    match expr {
        syn::Expr::Assign(e) => expr_has_skip_or_test(&e.left),
        syn::Expr::Binary(e) => expr_has_skip_or_test(&e.left),
        syn::Expr::Field(e) => expr_has_skip_or_test(&e.base),
        syn::Expr::Index(e) => expr_has_skip_or_test(&e.expr),
        syn::Expr::Unary(e) => expr_has_skip_or_test(&e.expr),
        syn::Expr::Paren(e) => expr_has_skip_or_test(&e.expr),
        syn::Expr::Group(e) => expr_has_skip_or_test(&e.expr),
        syn::Expr::Call(e) => expr_has_skip_or_test(&e.func),
        syn::Expr::MethodCall(e) => expr_has_skip_or_test(&e.receiver),
        _ => false,
    }
}

fn deletable_subhunk_stmt(stmt: &syn::Stmt, is_last: bool) -> bool {
    match stmt {
        syn::Stmt::Expr(expr, Some(_)) => {
            !matches!(expr, syn::Expr::Let(_)) && !stmt_has_skip_or_test(stmt)
        }
        syn::Stmt::Expr(syn::Expr::If(if_expr), None) => {
            !is_last && if_expr.else_branch.is_none() && !stmt_has_skip_or_test(stmt)
        }
        syn::Stmt::Macro(m) => m.semi_token.is_some() && !stmt_has_skip_or_test(stmt),
        _ => false,
    }
}

fn stmt_has_skip_or_test(stmt: &syn::Stmt) -> bool {
    match stmt {
        syn::Stmt::Local(l) => {
            instrument::is_test_only(&l.attrs) || instrument::is_skipped(&l.attrs)
        }
        syn::Stmt::Macro(m) => {
            instrument::is_test_only(&m.attrs) || instrument::is_skipped(&m.attrs)
        }
        syn::Stmt::Expr(expr, _) => expr_has_skip_or_test(expr),
        syn::Stmt::Item(_) => true,
    }
}

/// Collects byte ranges of all `#[cfg(test)]` / `#[mutants::skip]` subtrees in a file so
/// sub-hunks never delete a parent node containing protected code.
fn collect_blocked_ranges(
    source: &str,
    parsed: &syn::File,
    tests_now: &[(usize, usize)],
) -> Vec<(usize, usize)> {
    struct Blocked<'a> {
        source: &'a str,
        ranges: Vec<(usize, usize)>,
    }
    impl<'ast> Visit<'ast> for Blocked<'_> {
        fn visit_item(&mut self, node: &'ast syn::Item) {
            let attrs: &[syn::Attribute] = match node {
                syn::Item::Fn(i) => &i.attrs,
                syn::Item::Mod(i) => &i.attrs,
                syn::Item::Impl(i) => &i.attrs,
                syn::Item::Const(i) => &i.attrs,
                syn::Item::Static(i) => &i.attrs,
                syn::Item::Struct(i) => &i.attrs,
                syn::Item::Enum(i) => &i.attrs,
                syn::Item::Trait(i) => &i.attrs,
                _ => &[],
            };
            if instrument::is_test_only(attrs) || instrument::is_skipped(attrs) {
                if let Some(r) = span_range(self.source, node.span()) {
                    self.ranges.push(r);
                }
                return;
            }
            visit::visit_item(self, node);
        }
        fn visit_stmt(&mut self, node: &'ast syn::Stmt) {
            if stmt_has_skip_or_test(node) {
                if let Some(r) = span_range(self.source, node.span()) {
                    self.ranges.push(r);
                }
                return;
            }
            visit::visit_stmt(self, node);
        }
        fn visit_expr(&mut self, node: &'ast syn::Expr) {
            if expr_has_skip_or_test(node) {
                if let Some(r) = span_range(self.source, node.span()) {
                    self.ranges.push(r);
                }
                return;
            }
            visit::visit_expr(self, node);
        }
        fn visit_field_value(&mut self, node: &'ast syn::FieldValue) {
            if instrument::is_test_only(&node.attrs) || instrument::is_skipped(&node.attrs) {
                if let Some(r) = span_range(self.source, node.span()) {
                    self.ranges.push(r);
                }
                return;
            }
            visit::visit_field_value(self, node);
        }
        fn visit_arm(&mut self, node: &'ast syn::Arm) {
            if instrument::is_test_only(&node.attrs) || instrument::is_skipped(&node.attrs) {
                if let Some(r) = span_range(self.source, node.span()) {
                    self.ranges.push(r);
                }
                return;
            }
            visit::visit_arm(self, node);
        }
    }
    let mut collector = Blocked {
        source,
        ranges: tests_now.to_vec(),
    };
    collector.visit_file(parsed);
    collector.ranges
}

fn field_identity(source: &str, field: &syn::FieldValue) -> (String, String) {
    let name = match &field.member {
        syn::Member::Named(id) => id.to_string(),
        syn::Member::Unnamed(idx) => idx.index.to_string(),
    };
    let mut cfgs: Vec<String> = field
        .attrs
        .iter()
        .filter(|attr| attr.path().is_ident("cfg") || attr.path().is_ident("cfg_attr"))
        .filter_map(|attr| span_range(source, attr.span()).map(|(s, e)| token_sig(&source[s..e])))
        .collect();
    cfgs.sort();
    (name, cfgs.join(";"))
}

struct OldLogical {
    op: Option<LogicalOp>,
    terms: Vec<String>,
}

struct OldStruct {
    path_sig: String,
    fields: HashSet<(String, String)>,
}

#[derive(Default)]
struct OldStructures {
    logicals: HashMap<String, Vec<OldLogical>>,
    structs: HashMap<String, Vec<OldStruct>>,
    blocks: HashMap<String, Vec<Vec<String>>>,
}

struct OldCollector<'a> {
    source: &'a str,
    window: (usize, usize),
    scope: Vec<String>,
    in_logical: Option<LogicalOp>,
    found: OldStructures,
}

impl<'a> OldCollector<'a> {
    fn key(&self) -> String {
        self.scope.join("::")
    }

    fn record_single_value_expr(&mut self, expr: &syn::Expr) {
        if expr_has_skip_or_test(expr) {
            return;
        }
        if let Some(range) = span_range(self.source, expr.span())
            && overlaps(range, self.window)
            && !matches!(
                expr,
                syn::Expr::Binary(bin) if logical_op(&bin.op).is_some()
            )
            && !matches!(
                expr,
                syn::Expr::Block(_) | syn::Expr::Struct(_) | syn::Expr::Return(_)
            )
        {
            self.found
                .logicals
                .entry(self.key())
                .or_default()
                .push(OldLogical {
                    op: None,
                    terms: vec![token_sig(&self.source[range.0..range.1])],
                });
        }
    }
}

impl<'ast> Visit<'ast> for OldCollector<'_> {
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if instrument::is_test_only(&node.attrs) || instrument::is_skipped(&node.attrs) {
            return;
        }
        self.scope.push(format!("mod:{}", node.ident));
        visit::visit_item_mod(self, node);
        self.scope.pop();
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        if instrument::is_test_only(&node.attrs) || instrument::is_skipped(&node.attrs) {
            return;
        }
        let ty = span_range(self.source, node.self_ty.span())
            .map(|(s, e)| token_sig(&self.source[s..e]))
            .unwrap_or_default();
        self.scope.push(format!("impl:{ty}"));
        visit::visit_item_impl(self, node);
        self.scope.pop();
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if instrument::is_test_only(&node.attrs) || instrument::is_skipped(&node.attrs) {
            return;
        }
        self.scope.push(format!("fn:{}", node.sig.ident));
        visit::visit_item_fn(self, node);
        self.scope.pop();
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        if instrument::is_test_only(&node.attrs) || instrument::is_skipped(&node.attrs) {
            return;
        }
        self.scope.push(format!("fn:{}", node.sig.ident));
        visit::visit_impl_item_fn(self, node);
        self.scope.pop();
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        if instrument::is_test_only(&node.attrs) || instrument::is_skipped(&node.attrs) {
            return;
        }
        self.scope.push(format!("fn:{}", node.sig.ident));
        visit::visit_trait_item_fn(self, node);
        self.scope.pop();
    }

    fn visit_stmt(&mut self, node: &'ast syn::Stmt) {
        if stmt_has_skip_or_test(node) {
            return;
        }
        visit::visit_stmt(self, node);
    }

    fn visit_expr(&mut self, node: &'ast syn::Expr) {
        if expr_has_skip_or_test(node) {
            return;
        }
        let prev = self.in_logical;
        if !matches!(node, syn::Expr::Binary(_)) {
            self.in_logical = None;
        }
        visit::visit_expr(self, node);
        self.in_logical = prev;
    }

    fn visit_field_value(&mut self, node: &'ast syn::FieldValue) {
        if instrument::is_test_only(&node.attrs) || instrument::is_skipped(&node.attrs) {
            return;
        }
        visit::visit_field_value(self, node);
    }

    fn visit_arm(&mut self, node: &'ast syn::Arm) {
        if instrument::is_test_only(&node.attrs) || instrument::is_skipped(&node.attrs) {
            return;
        }
        let pat_sig = span_range(self.source, node.pat.span())
            .map(|(s, e)| token_sig(&self.source[s..e]))
            .unwrap_or_default();
        self.scope.push(format!("arm:{pat_sig}"));
        visit::visit_arm(self, node);
        self.scope.pop();
    }

    fn visit_block(&mut self, node: &'ast syn::Block) {
        let prev_logical = self.in_logical;
        self.in_logical = None;
        if let Some(range) = span_range(self.source, node.span())
            && overlaps(range, self.window)
        {
            // Collect ALL statements in the block for subsequence matching so a statement
            // moving from tail to non-tail is never misclassified as newly added.
            let sigs: Vec<String> = node
                .stmts
                .iter()
                .filter_map(|stmt| {
                    let (s, e) = span_range(self.source, stmt.span())?;
                    Some(token_sig(&self.source[s..e]))
                })
                .collect();
            self.found.blocks.entry(self.key()).or_default().push(sigs);
            if let Some(syn::Stmt::Expr(tail, None)) = node.stmts.last() {
                self.record_single_value_expr(tail);
            }
        }
        visit::visit_block(self, node);
        self.in_logical = prev_logical;
    }

    fn visit_expr_if(&mut self, node: &'ast syn::ExprIf) {
        self.record_single_value_expr(&node.cond);
        self.visit_expr(&node.cond);
        let cond_sig = span_range(self.source, node.cond.span())
            .map(|(s, e)| token_sig(&self.source[s..e]))
            .unwrap_or_default();
        self.scope.push(format!("if_then:{cond_sig}"));
        self.visit_block(&node.then_branch);
        self.scope.pop();
        if let Some((_, else_branch)) = &node.else_branch {
            self.scope.push(format!("if_else:{cond_sig}"));
            self.visit_expr(else_branch);
            self.scope.pop();
        }
    }

    fn visit_expr_while(&mut self, node: &'ast syn::ExprWhile) {
        self.record_single_value_expr(&node.cond);
        self.visit_expr(&node.cond);
        let cond_sig = span_range(self.source, node.cond.span())
            .map(|(s, e)| token_sig(&self.source[s..e]))
            .unwrap_or_default();
        self.scope.push(format!("while:{cond_sig}"));
        self.visit_block(&node.body);
        self.scope.pop();
    }

    fn visit_expr_for_loop(&mut self, node: &'ast syn::ExprForLoop) {
        self.visit_expr(&node.expr);
        let sig = span_range(self.source, node.expr.span())
            .map(|(s, e)| token_sig(&self.source[s..e]))
            .unwrap_or_default();
        self.scope.push(format!("for:{sig}"));
        self.visit_block(&node.body);
        self.scope.pop();
    }

    fn visit_expr_loop(&mut self, node: &'ast syn::ExprLoop) {
        self.scope.push("loop".to_string());
        self.visit_block(&node.body);
        self.scope.pop();
    }

    fn visit_expr_block(&mut self, node: &'ast syn::ExprBlock) {
        self.scope.push("expr_block".to_string());
        self.visit_block(&node.block);
        self.scope.pop();
    }

    fn visit_expr_closure(&mut self, node: &'ast syn::ExprClosure) {
        self.scope.push("closure".to_string());
        visit::visit_expr_closure(self, node);
        self.scope.pop();
    }

    fn visit_expr_return(&mut self, node: &'ast syn::ExprReturn) {
        if let Some(expr) = &node.expr {
            self.record_single_value_expr(expr);
        }
        visit::visit_expr_return(self, node);
    }

    fn visit_local(&mut self, node: &'ast syn::Local) {
        if instrument::is_test_only(&node.attrs) || instrument::is_skipped(&node.attrs) {
            return;
        }
        if let Some(init) = &node.init {
            self.record_single_value_expr(&init.expr);
        }
        visit::visit_local(self, node);
    }

    fn visit_expr_binary(&mut self, node: &'ast syn::ExprBinary) {
        let prev = self.in_logical;
        if let Some(op) = logical_op(&node.op) {
            if prev != Some(op)
                && let Some(range) = span_range(self.source, node.span())
                && overlaps(range, self.window)
            {
                let expr = syn::Expr::Binary(node.clone());
                let mut terms = Vec::new();
                flatten_logical(&expr, op, &mut terms);
                let term_sigs: Option<Vec<String>> = terms
                    .iter()
                    .map(|t| {
                        span_range(self.source, t.span())
                            .map(|(s, e)| token_sig(&self.source[s..e]))
                    })
                    .collect();
                if let Some(term_sigs) = term_sigs {
                    self.found
                        .logicals
                        .entry(self.key())
                        .or_default()
                        .push(OldLogical {
                            op: Some(op),
                            terms: term_sigs,
                        });
                }
            }
            self.in_logical = Some(op);
            visit::visit_expr_binary(self, node);
            self.in_logical = prev;
        } else {
            self.in_logical = None;
            visit::visit_expr_binary(self, node);
            self.in_logical = prev;
        }
    }

    fn visit_expr_struct(&mut self, node: &'ast syn::ExprStruct) {
        if node.rest.is_some()
            && let Some(range) = span_range(self.source, node.span())
            && overlaps(range, self.window)
            && let Some((ps, pe)) = span_range(self.source, node.path.span())
        {
            let fields = node
                .fields
                .iter()
                .map(|f| field_identity(self.source, f))
                .collect();
            self.found
                .structs
                .entry(self.key())
                .or_default()
                .push(OldStruct {
                    path_sig: token_sig(&self.source[ps..pe]),
                    fields,
                });
        }
        visit::visit_expr_struct(self, node);
    }
}

type DedupKey = (String, Option<Vec<(usize, usize)>>);

struct SubhunkBuilder<'a> {
    current: &'a str,
    whole_undone: &'a str,
    whole: &'a Change,
    blocked_now: &'a [(usize, usize)],
    has_old: bool,
    asked: Asks,
    old: &'a OldStructures,
    scope: Vec<String>,
    in_logical: Option<LogicalOp>,
    seen_keys: HashSet<DedupKey>,
    out: Vec<Change>,
}

impl<'a> SubhunkBuilder<'a> {
    fn key(&self) -> String {
        self.scope.join("::")
    }

    fn dedup_key(&self, text: &str) -> DedupKey {
        let loc = if self.asked == Asks::Nothing {
            None
        } else {
            places(text, &[], self.asked)
        };
        (token_sig(text), loc)
    }

    fn node_in_hunk(&self, anchor: (usize, usize)) -> bool {
        if self.has_old {
            overlaps(anchor, self.whole.range)
        } else {
            contained_in(anchor, self.whole.range)
        }
    }

    fn try_emit(
        &mut self,
        del_start: usize,
        del_end: usize,
        anchor: (usize, usize),
        description: String,
    ) {
        if self.out.len() >= MAX_SUBHUNKS_PER_HUNK {
            return;
        }
        if del_start >= del_end
            || del_end > self.current.len()
            || !self.current.is_char_boundary(del_start)
            || !self.current.is_char_boundary(del_end)
            || touches_any((del_start, del_end), self.blocked_now)
            || touches_any(anchor, self.blocked_now)
        {
            return;
        }
        let put_back = if needs_separator_space(self.current, del_start, del_end) {
            " ".to_string()
        } else {
            String::new()
        };
        let mut candidate =
            String::with_capacity(self.current.len() - (del_end - del_start) + put_back.len());
        candidate.push_str(&self.current[..del_start]);
        candidate.push_str(&put_back);
        candidate.push_str(&self.current[del_end..]);
        if instrument::parse_source(&candidate).is_err() {
            return;
        }
        let same_as_current = if self.asked == Asks::Nothing {
            instrument::same_tokens(self.current, &candidate) == Some(true)
        } else {
            same_build(self.current, &[], &candidate, &[])
        };
        let same_as_whole = if self.asked == Asks::Nothing {
            instrument::same_tokens(self.whole_undone, &candidate) == Some(true)
        } else {
            same_build(self.whole_undone, &[], &candidate, &[])
        };
        if same_as_current || same_as_whole {
            return;
        }
        let key = self.dedup_key(&candidate);
        if !self.seen_keys.insert(key) {
            return;
        }
        let line = self.current[..anchor.0].matches('\n').count() + 1;
        let code = crate::report::one_line(&self.current[anchor.0..anchor.1]);
        self.out.push(Change {
            hunk: self.whole.hunk.clone(),
            line,
            code,
            undone: Some(candidate),
            range: (del_start, del_end),
            put_back,
            outside: false,
            subhunk: Some(description),
        });
    }
}

impl<'ast> Visit<'ast> for SubhunkBuilder<'_> {
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if instrument::is_test_only(&node.attrs) || instrument::is_skipped(&node.attrs) {
            return;
        }
        self.scope.push(format!("mod:{}", node.ident));
        visit::visit_item_mod(self, node);
        self.scope.pop();
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        if instrument::is_test_only(&node.attrs) || instrument::is_skipped(&node.attrs) {
            return;
        }
        let ty = span_range(self.current, node.self_ty.span())
            .map(|(s, e)| token_sig(&self.current[s..e]))
            .unwrap_or_default();
        self.scope.push(format!("impl:{ty}"));
        visit::visit_item_impl(self, node);
        self.scope.pop();
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if instrument::is_test_only(&node.attrs) || instrument::is_skipped(&node.attrs) {
            return;
        }
        self.scope.push(format!("fn:{}", node.sig.ident));
        visit::visit_item_fn(self, node);
        self.scope.pop();
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        if instrument::is_test_only(&node.attrs) || instrument::is_skipped(&node.attrs) {
            return;
        }
        self.scope.push(format!("fn:{}", node.sig.ident));
        visit::visit_impl_item_fn(self, node);
        self.scope.pop();
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        if instrument::is_test_only(&node.attrs) || instrument::is_skipped(&node.attrs) {
            return;
        }
        self.scope.push(format!("fn:{}", node.sig.ident));
        visit::visit_trait_item_fn(self, node);
        self.scope.pop();
    }

    fn visit_stmt(&mut self, node: &'ast syn::Stmt) {
        if stmt_has_skip_or_test(node) {
            return;
        }
        visit::visit_stmt(self, node);
    }

    fn visit_expr(&mut self, node: &'ast syn::Expr) {
        if expr_has_skip_or_test(node) {
            return;
        }
        let prev = self.in_logical;
        if !matches!(node, syn::Expr::Binary(_)) {
            self.in_logical = None;
        }
        visit::visit_expr(self, node);
        self.in_logical = prev;
    }

    fn visit_field_value(&mut self, node: &'ast syn::FieldValue) {
        if instrument::is_test_only(&node.attrs) || instrument::is_skipped(&node.attrs) {
            return;
        }
        visit::visit_field_value(self, node);
    }

    fn visit_local(&mut self, node: &'ast syn::Local) {
        if instrument::is_test_only(&node.attrs) || instrument::is_skipped(&node.attrs) {
            return;
        }
        visit::visit_local(self, node);
    }

    fn visit_arm(&mut self, node: &'ast syn::Arm) {
        if instrument::is_test_only(&node.attrs) || instrument::is_skipped(&node.attrs) {
            return;
        }
        let pat_sig = span_range(self.current, node.pat.span())
            .map(|(s, e)| token_sig(&self.current[s..e]))
            .unwrap_or_default();
        self.scope.push(format!("arm:{pat_sig}"));
        visit::visit_arm(self, node);
        self.scope.pop();
    }

    fn visit_expr_if(&mut self, node: &'ast syn::ExprIf) {
        self.visit_expr(&node.cond);
        let cond_sig = span_range(self.current, node.cond.span())
            .map(|(s, e)| token_sig(&self.current[s..e]))
            .unwrap_or_default();
        self.scope.push(format!("if_then:{cond_sig}"));
        self.visit_block(&node.then_branch);
        self.scope.pop();
        if let Some((_, else_branch)) = &node.else_branch {
            self.scope.push(format!("if_else:{cond_sig}"));
            self.visit_expr(else_branch);
            self.scope.pop();
        }
    }

    fn visit_expr_while(&mut self, node: &'ast syn::ExprWhile) {
        self.visit_expr(&node.cond);
        let cond_sig = span_range(self.current, node.cond.span())
            .map(|(s, e)| token_sig(&self.current[s..e]))
            .unwrap_or_default();
        self.scope.push(format!("while:{cond_sig}"));
        self.visit_block(&node.body);
        self.scope.pop();
    }

    fn visit_expr_for_loop(&mut self, node: &'ast syn::ExprForLoop) {
        self.visit_expr(&node.expr);
        let sig = span_range(self.current, node.expr.span())
            .map(|(s, e)| token_sig(&self.current[s..e]))
            .unwrap_or_default();
        self.scope.push(format!("for:{sig}"));
        self.visit_block(&node.body);
        self.scope.pop();
    }

    fn visit_expr_loop(&mut self, node: &'ast syn::ExprLoop) {
        self.scope.push("loop".to_string());
        self.visit_block(&node.body);
        self.scope.pop();
    }

    fn visit_expr_block(&mut self, node: &'ast syn::ExprBlock) {
        self.scope.push("expr_block".to_string());
        self.visit_block(&node.block);
        self.scope.pop();
    }

    fn visit_expr_closure(&mut self, node: &'ast syn::ExprClosure) {
        self.scope.push("closure".to_string());
        visit::visit_expr_closure(self, node);
        self.scope.pop();
    }

    fn visit_expr_binary(&mut self, node: &'ast syn::ExprBinary) {
        let prev = self.in_logical;
        if let Some(op) = logical_op(&node.op) {
            if prev != Some(op)
                && let Some(range) = span_range(self.current, node.span())
                && overlaps(range, self.whole.range)
            {
                let expr = syn::Expr::Binary(node.clone());
                let mut terms = Vec::new();
                flatten_logical(&expr, op, &mut terms);
                let has_let = terms.iter().any(|t| matches!(t, syn::Expr::Let(_)));
                if terms.len() >= 2 && !has_let {
                    let spans: Option<Vec<(usize, usize)>> = terms
                        .iter()
                        .map(|t| span_range(self.current, t.span()))
                        .collect();
                    if let Some(spans) = spans
                        && spans.windows(2).all(|w| w[0].1 <= w[1].0)
                    {
                        let term_sigs: Vec<String> = spans
                            .iter()
                            .map(|&(s, e)| token_sig(&self.current[s..e]))
                            .collect();
                        let added_indices = if !self.has_old {
                            Some((0..terms.len()).collect::<Vec<_>>())
                        } else {
                            let old_list = self.old.logicals.get(&self.key());
                            match old_list.map(Vec::as_slice) {
                                Some([old_expr])
                                    if old_expr.op.is_none() || old_expr.op == Some(op) =>
                                {
                                    subsequence_added_indices(&old_expr.terms, &term_sigs)
                                        .filter(|added| !added.is_empty())
                                }
                                _ => None,
                            }
                        };
                        if let Some(indices) = added_indices {
                            for i in indices {
                                let anchor = spans[i];
                                if !self.node_in_hunk(anchor) {
                                    continue;
                                }
                                let (del_start, del_end) = if i == 0 {
                                    (spans[0].0, spans[1].0)
                                } else {
                                    (spans[i - 1].1, spans[i].1)
                                };
                                let desc = format!(
                                    "without added condition `{}`",
                                    crate::report::one_line(&self.current[anchor.0..anchor.1])
                                );
                                self.try_emit(del_start, del_end, anchor, desc);
                            }
                        }
                    }
                }
            }
            self.in_logical = Some(op);
            visit::visit_expr_binary(self, node);
            self.in_logical = prev;
        } else {
            self.in_logical = None;
            visit::visit_expr_binary(self, node);
            self.in_logical = prev;
        }
    }

    fn visit_expr_struct(&mut self, node: &'ast syn::ExprStruct) {
        if node.rest.is_some()
            && let Some(range) = span_range(self.current, node.span())
            && overlaps(range, self.whole.range)
            && let Some((ps, pe)) = span_range(self.current, node.path.span())
        {
            let path_sig = token_sig(&self.current[ps..pe]);
            let new_field_ids: HashSet<(String, String)> = node
                .fields
                .iter()
                .map(|f| field_identity(self.current, f))
                .collect();
            let old_fields = if !self.has_old {
                Some(HashSet::new())
            } else {
                let matching: Vec<&OldStruct> = self
                    .old
                    .structs
                    .get(&self.key())
                    .into_iter()
                    .flatten()
                    .filter(|s| s.path_sig == path_sig)
                    .collect();
                // Only decompose when every previously present field (including its `cfg`
                // attributes) is still present, so rewriting a field's `cfg` never deletes it.
                match matching.as_slice() {
                    [old_struct] if old_struct.fields.is_subset(&new_field_ids) => {
                        Some(old_struct.fields.clone())
                    }
                    _ => None,
                }
            };
            if let Some(old_fields) = old_fields {
                for pair in node.fields.pairs() {
                    let field = pair.value();
                    if instrument::is_test_only(&field.attrs)
                        || instrument::is_skipped(&field.attrs)
                    {
                        continue;
                    }
                    let id = field_identity(self.current, field);
                    if old_fields.contains(&id) {
                        continue;
                    }
                    let Some((fs, fe)) = span_range(self.current, field.span()) else {
                        continue;
                    };
                    if !self.node_in_hunk((fs, fe)) {
                        continue;
                    }
                    let del_end = pair
                        .punct()
                        .and_then(|comma| span_range(self.current, comma.span()))
                        .map_or(fe, |(_, ce)| ce);
                    let desc = format!(
                        "without added field `{}`",
                        crate::report::one_line(&self.current[fs..fe])
                    );
                    self.try_emit(fs, del_end, (fs, fe), desc);
                }
            }
        }
        visit::visit_expr_struct(self, node);
    }

    fn visit_block(&mut self, node: &'ast syn::Block) {
        let prev_logical = self.in_logical;
        self.in_logical = None;
        if let Some(range) = span_range(self.current, node.span())
            && overlaps(range, self.whole.range)
        {
            // Match all statements in order first, then check deletability only on the added ones.
            let stmts_info: Vec<_> = node
                .stmts
                .iter()
                .enumerate()
                .map(|(idx, stmt)| {
                    let is_last = idx + 1 == node.stmts.len();
                    let range = span_range(self.current, stmt.span());
                    let sig = range
                        .map(|(s, e)| token_sig(&self.current[s..e]))
                        .unwrap_or_default();
                    let can_delete = deletable_subhunk_stmt(stmt, is_last);
                    (range, sig, can_delete)
                })
                .collect();
            if stmts_info.iter().filter(|(_, _, del)| *del).count() >= 2 {
                let sigs: Vec<String> = stmts_info.iter().map(|(_, sig, _)| sig.clone()).collect();
                let added_indices = if !self.has_old {
                    Some((0..stmts_info.len()).collect::<Vec<_>>())
                } else {
                    match self.old.blocks.get(&self.key()).map(Vec::as_slice) {
                        Some([old_sigs]) => {
                            subsequence_added_indices(old_sigs, &sigs).filter(|a| !a.is_empty())
                        }
                        _ => None,
                    }
                };
                if let Some(indices) = added_indices {
                    for i in indices {
                        let (Some(anchor), _, true) = stmts_info[i] else {
                            continue;
                        };
                        if !self.node_in_hunk(anchor) {
                            continue;
                        }
                        let desc = format!(
                            "without added statement `{}`",
                            crate::report::one_line(&self.current[anchor.0..anchor.1])
                        );
                        self.try_emit(anchor.0, anchor.1, anchor, desc);
                    }
                }
            }
        }
        visit::visit_block(self, node);
        self.in_logical = prev_logical;
    }
}

/// Decomposes a multi-addition diff hunk into independent AST sub-hunk reverts:
/// added `&&`/`||` terms, added struct literal fields (with `..base`), and added
/// standalone statements.
fn subhunk_changes(
    current: &str,
    parsed: &syn::File,
    tests_now: &[(usize, usize)],
    undone_text: &str,
    undone_tree: &syn::File,
    whole: &Change,
) -> Vec<Change> {
    if whole.range.0 >= whole.range.1 {
        return vec![];
    }
    let blocked_now = collect_blocked_ranges(current, parsed, tests_now);
    let has_old = !whole.put_back.trim().is_empty();
    let old_window = (whole.range.0, whole.range.0 + whole.put_back.len());
    let mut old_collector = OldCollector {
        source: undone_text,
        window: old_window,
        scope: Vec::new(),
        in_logical: None,
        found: OldStructures::default(),
    };
    if has_old {
        old_collector.visit_file(undone_tree);
    }
    let asked = asks_where_it_is(current).max(asks_where_it_is(undone_text));
    let mut builder = SubhunkBuilder {
        current,
        whole_undone: undone_text,
        whole,
        blocked_now: &blocked_now,
        has_old,
        asked,
        old: &old_collector.found,
        scope: Vec::new(),
        in_logical: None,
        seen_keys: HashSet::new(),
        out: Vec::new(),
    };
    let cur_key = builder.dedup_key(current);
    let undone_key = builder.dedup_key(undone_text);
    builder.seen_keys.insert(cur_key);
    builder.seen_keys.insert(undone_key);
    builder.visit_file(parsed);
    builder.out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The changes between two texts, as git sees them.
    fn between(old: &str, new: &str) -> Vec<Change> {
        let dir = std::env::temp_dir().join(format!(
            "tmt-changes-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("old"), old).unwrap();
        std::fs::write(dir.join("new"), new).unwrap();
        let output = std::process::Command::new("git")
            .current_dir(&dir)
            .args(["diff", "--no-index", "-U0", "--no-color", "--no-ext-diff"])
            .args(["--src-prefix=a/", "--dst-prefix=b/", "old", "new"])
            .output()
            .unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        changes(
            new,
            &scope::hunks(&String::from_utf8(output.stdout).unwrap()),
        )
    }

    const OLD: &str = "\
/// Whether a session is still good.
pub fn valid(created: u64, used: u64, now: u64) -> bool {
    // thirty minutes
    now - created < 30
}

pub fn label(n: u32) -> u32 {
    n
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh() {
        assert!(valid(0, 0, 10));
    }
}
";

    #[test]
    fn a_change_of_the_program_is_tried_undone() {
        let new = OLD.replace("now - created < 30", "now - used < 30");
        let found = between(OLD, &new);
        assert_eq!(found.len(), 1, "{found:?}");
        let change = &found[0];
        assert_eq!(change.line, 4);
        assert_eq!(change.code, "now - used < 30");
        assert_eq!(change.undone.as_deref(), Some(OLD));
        assert_eq!(
            change.describe(),
            "with the code as it was: `now - created < 30`"
        );
        assert_eq!(all_undone(&new, &found).as_deref(), Some(OLD));
    }

    #[test]
    fn what_changes_no_token_or_only_the_tests_is_left_out() {
        for new in [
            // A comment, the documentation, the spacing.
            OLD.replace("// thirty minutes", "// half an hour"),
            OLD.replace("/// Whether a session is still good.", "/// Says if it is."),
            OLD.replace("    n\n", "        n\n"),
            OLD.replace("    // thirty minutes\n", ""),
            OLD.replace("pub fn label", "\n\npub fn label"),
            // A test that is new, changed, or gone, and a helper of the tests.
            OLD.replace("valid(0, 0, 10)", "valid(0, 0, 20)"),
            OLD.replace(
                "    #[test]\n    fn fresh() {",
                "    #[test]\n    fn other() {\n        assert!(!valid(0, 0, 40));\n    }\n\n    #[test]\n    fn fresh() {",
            ),
            OLD.replace(
                "    #[test]\n    fn fresh() {\n        assert!(valid(0, 0, 10));\n    }\n",
                "",
            ),
            OLD.replace("    use super::*;\n", "    use super::*;\n\n    fn ten() -> u64 { 10 }\n"),
            format!("{OLD}\n#[test]\nfn outside() {{\n    assert!(valid(1, 1, 2));\n}}\n"),
        ] {
            assert_ne!(new, OLD);
            assert_eq!(between(OLD, &new), [], "forwards: {new}");
            // The other way round: the same holds for what was there before.
            assert_eq!(between(&new, OLD), [], "backwards: {new}");
        }
    }

    #[test]
    fn the_changes_of_the_tests_are_told_from_the_rest() {
        let weaker = OLD.replace("assert!(valid(0, 0, 10));", "let _ = valid(0, 0, 10);");
        let both = weaker.replace("now - created < 30", "now - used < 30");
        let diff = |old: &str, new: &str| {
            let dir = std::env::temp_dir().join(format!(
                "tmt-test-hunks-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("old"), old).unwrap();
            std::fs::write(dir.join("new"), new).unwrap();
            let output = std::process::Command::new("git")
                .current_dir(&dir)
                .args(["diff", "--no-index", "-U0", "--no-color", "--no-ext-diff"])
                .args(["--src-prefix=a/", "--dst-prefix=b/", "old", "new"])
                .output()
                .unwrap();
            std::fs::remove_dir_all(&dir).unwrap();
            scope::hunks(&String::from_utf8(output.stdout).unwrap())
        };
        let hunks = diff(OLD, &both);
        assert_eq!(hunks.len(), 2);
        let of_tests = test_hunks(&both, &hunks);
        assert_eq!(of_tests.len(), 1, "{of_tests:?}");
        assert_eq!(of_tests[0].old.trim(), "assert!(valid(0, 0, 10));");
        assert_eq!(changes(&both, &hunks).len(), 1);
        // With the tests' change undone the program's change stays.
        let hunk_refs: Vec<&Hunk> = of_tests.iter().collect();
        assert_eq!(
            scope::undo(&both, &hunk_refs).unwrap(),
            OLD.replace("now - created < 30", "now - used < 30")
        );
        // A comment in the tests is no change of them.
        let commented = OLD.replace(
            "        assert!(valid",
            "        // sure\n        assert!(valid",
        );
        assert_eq!(test_hunks(&commented, &diff(OLD, &commented)), []);
        assert_eq!(test_hunks("fn f( {", &hunks), []);
    }

    /// A limit and the test of it on the lines next to it: one hunk for git.
    const NEXT_TO: &str = "\
pub fn limit() -> u32 { 41 }
#[test] fn the_limit() { assert_eq!(limit(), 41); }
";

    #[test]
    fn the_program_is_put_back_and_the_tests_are_kept() {
        let new = NEXT_TO.replace("41", "42");
        let found = between(NEXT_TO, &new);
        assert_eq!(found.len(), 1, "{found:?}");
        // The old limit with the test as it is now: that is what the tests are asked.
        let wanted = NEXT_TO.replace("limit(), 41", "limit(), 42");
        assert_eq!(found[0].undone.as_deref(), Some(wanted.as_str()));
        // Undone together with others, the test is kept as well.
        assert_eq!(all_undone(&new, &found).as_deref(), Some(wanted.as_str()));
        let renamed = NEXT_TO.to_string()
            + "\npub fn kept() {}\n\nfn helper() -> u32 { 1 }\n\npub fn uses() -> u32 { helper() }\n";
        let new = renamed.replace("41", "42").replace("helper", "one");
        let found = between(&renamed, &new);
        // The definition and the use do not build one without the other: all three
        // changes go back together, and the test stays.
        assert_eq!(found.len(), 3, "{found:?}");
        assert_eq!(
            all_undone(&new, &found).as_deref(),
            Some(renamed.replace("limit(), 41", "limit(), 42").as_str())
        );
        // The test first and the program after it: the same.
        let (first, second) = NEXT_TO.split_once('\n').unwrap();
        let old = format!("{second}{first}\n");
        let new = old.replace("41", "42");
        let found = between(&old, &new);
        assert_eq!(
            found[0].undone.as_deref(),
            Some(old.replace("limit(), 41", "limit(), 42").as_str())
        );
    }

    #[test]
    fn what_cannot_be_told_apart_is_not_tried() {
        // The same change with a line more on one side: the old lines no longer stand
        // for the new ones one by one.
        let old = NEXT_TO.to_string();
        let new = old.replace("{ 41 }", "{ 42 }").replace(
            "{ assert_eq!(limit(), 41); }",
            "{\n    assert_eq!(limit(), 42); }",
        );
        let dir_hunks = |old: &str, new: &str| {
            let dir = std::env::temp_dir().join(format!(
                "tmt-mixed-{}-{:?}",
                std::process::id(),
                std::thread::current().id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("old"), old).unwrap();
            std::fs::write(dir.join("new"), new).unwrap();
            let output = std::process::Command::new("git")
                .current_dir(&dir)
                .args(["diff", "--no-index", "-U0", "--no-color", "--no-ext-diff"])
                .args(["--src-prefix=a/", "--dst-prefix=b/", "old", "new"])
                .output()
                .unwrap();
            std::fs::remove_dir_all(&dir).unwrap();
            scope::hunks(&String::from_utf8(output.stdout).unwrap())
        };
        let hunks = dir_hunks(&old, &new);
        assert_eq!(hunks.len(), 1, "{hunks:?}");
        assert_eq!(changes(&new, &hunks), []);
        assert_eq!(test_hunks(&new, &hunks), []);
        assert_eq!(mixed(&new, &hunks), 1);
        // And the other way round.
        let back = dir_hunks(&new, &old);
        assert_eq!(changes(&old, &back), []);
        assert_eq!(mixed(&old, &back), 1);
        // A change of the program alone, or of the tests alone, is not mixed.
        let program = old.replace("{ 41 }", "{ 42 }");
        assert_eq!(mixed(&program, &dir_hunks(&old, &program)), 0);
        assert_eq!(changes(&program, &dir_hunks(&old, &program)).len(), 1);
        let tests = old.replace("limit(), 41", "limit(), 40 + 1");
        assert_eq!(mixed(&tests, &dir_hunks(&old, &tests)), 0);
        assert_eq!(test_hunks(&tests, &dir_hunks(&old, &tests)).len(), 1);
    }

    /// Whether the one change between two texts is outside of a build with these
    /// features, now and undone.
    fn outside(old: &str, new: &str, features: &[&str]) -> bool {
        let features: std::collections::BTreeSet<String> =
            features.iter().map(|name| name.to_string()).collect();
        let off = |text: &str| {
            instrument::parse_source(text)
                .map(|parsed| instrument::inactive_ranges(text, &parsed, &features))
                .ok()
        };
        let found = between(old, new);
        assert_eq!(found.len(), 1, "{found:?}");
        let then = found[0].undone.as_deref().and_then(off);
        found[0].outside_the_build(new, &off(new).unwrap(), then.as_deref())
    }

    #[test]
    fn a_change_is_outside_the_build_only_if_it_is_so_undone_too() {
        let code = "\
#[cfg(feature = \"extra\")]
pub fn extra() -> u32 { 41 }

#[cfg(feature = \"extra\")]
pub const LIMIT: u32 = 41;

#[cfg(feature = \"basic\")]
pub fn basic() -> u32 {
    41
}
";
        // In code the feature leaves out, whatever the shape of the item: a function
        // on one line, a constant, the body of a function of several lines.
        for (from, to) in [
            (
                "pub fn extra() -> u32 { 41 }",
                "pub fn extra() -> u32 { 42 }",
            ),
            ("pub const LIMIT: u32 = 41;", "pub const LIMIT: u32 = 42;"),
        ] {
            assert!(outside(code, &code.replace(from, to), &["basic"]), "{to}");
            assert!(
                !outside(code, &code.replace(from, to), &["basic", "extra"]),
                "{to}"
            );
        }
        let body = code.replace("    41\n", "    42\n");
        assert!(!outside(code, &body, &["basic"]));
        assert!(outside(code, &body, &["extra"]));
        // The attribute itself: left out now and built before, or the other way round.
        let moved = code.replace(
            "#[cfg(feature = \"basic\")]\npub fn basic",
            "#[cfg(feature = \"extra\")]\npub fn basic",
        );
        assert!(!outside(code, &moved, &["basic"]));
        assert!(!outside(&moved, code, &["basic"]));
        // Off under either name: nothing changes for this build.
        assert!(outside(code, &moved, &[]));
        // An attribute that is new, and one that is gone.
        let added = code.replace("#[cfg(feature = \"basic\")]\n", "");
        assert!(!outside(&added, code, &["extra"]));
        assert!(!outside(code, &added, &["extra"]));
        // Lines added to code that is left out, and taken from it.
        let longer = code.replace(
            "pub fn extra() -> u32 { 41 }",
            "pub fn extra() -> u32 {\n    let n = 41;\n    n\n}",
        );
        assert!(outside(code, &longer, &["basic"]));
        assert!(outside(&longer, code, &["basic"]));
        // What the build was before is not known: tried.
        let found = between(code, &body);
        assert!(!found[0].outside_the_build(&body, &[(0, body.len())], None));
    }

    #[test]
    fn a_comment_after_a_test_is_of_the_line_of_the_test() {
        let old =
            "pub const X: u32 = 41;\n#[test] fn check() { assert_eq!(X, 41); } // regression\n";
        let new = old.replace("41", "42");
        let found = between(old, &new);
        assert_eq!(found.len(), 1, "{found:?}");
        // The constant goes back, the test and its comment stay.
        assert_eq!(
            found[0].undone.as_deref(),
            Some(new.replace("X: u32 = 42", "X: u32 = 41").as_str())
        );
    }

    #[test]
    fn whole_items_that_are_left_out_change_nothing_and_lines_may() {
        let base = "pub fn kept() -> u32 { 1 }\n";
        let item = "\n#[cfg(feature = \"extra\")]\npub const X: u32 = 42;\n";
        // An item that is left out, new at the end of the file or between others, one
        // or two of them, and the same taken away.
        for added in [
            format!("{base}{item}"),
            format!("{base}{item}{item}"),
            format!("{item}{base}"),
        ] {
            assert!(outside(base, &added, &["basic"]), "{added}");
            assert!(outside(&added, base, &["basic"]), "{added}");
            assert!(!outside(base, &added, &["extra"]), "{added}");
        }
        // With an item that is built among them, it is a change.
        let mixed = format!("{base}{item}pub fn more() {{}}\n");
        assert!(!outside(base, &mixed, &["basic"]));
        // Lines that move what `line!` gives are a change of what is built.
        let counted = "#[cfg(feature = \"extra\")]\nfn off() {\n}\npub const N: u32 = line!();\n";
        let longer = counted.replace("fn off() {\n", "fn off() {\n    let _ = 2;\n");
        assert!(!outside(counted, &longer, &["basic"]));
        assert!(outside(
            &counted.replace("line!()", "4"),
            &longer.replace("line!()", "4"),
            &["basic"]
        ));
        // With every token that is kept where it was, nothing can be told: a number of
        // the same length in code that is left out. One digit more moves nothing either
        // when nothing that is kept comes after it on its line.
        let same = longer.replace("let _ = 2;", "let _ = 3;");
        assert!(outside(&longer, &same, &["basic"]));
        let wider = longer.replace("let _ = 2;", "let _ = 200;");
        assert!(outside(&longer, &wider, &["basic"]));
        assert!(outside(
            &longer.replace("line!()", "4"),
            &same.replace("line!()", "4"),
            &["basic"]
        ));
    }

    #[test]
    fn the_same_build_is_the_same_tokens_in_what_is_built() {
        // Two attributes on one statement, both reworded: either alone leaves it out,
        // both together put it back.
        let old = "fn f() -> u8 {\n    #[cfg(feature = \"basic\")]\n    #[allow(unreachable_code)]\n    #[cfg(feature = \"basic\")]\n    return 1;\n    2\n}\n";
        let new = old.replace("basic", "extra");
        let features: std::collections::BTreeSet<String> = ["basic".to_string()].into();
        let off = |text: &str| {
            instrument::inactive_ranges(text, &instrument::parse_source(text).unwrap(), &features)
        };
        let found = between(old, &new);
        assert_eq!(found.len(), 2, "{found:?}");
        for change in &found {
            let undone = change.undone.as_deref().unwrap();
            assert!(change.outside_the_build(&new, &off(&new), Some(&off(undone))));
        }
        let both = all_undone(&new, &found).unwrap();
        assert_eq!(both, old);
        assert!(!same_build(&new, &off(&new), &both, &off(&both)));
        // An item that is left out, with a comment after it, is no part of the build.
        let base = "pub const N: u8 = 7;\n";
        let with =
            format!("{base}#[cfg(feature = \"extra\")] pub const OFF: u8 = 42; // optional\n");
        assert!(same_build(&with, &off(&with), base, &off(base)));
        // Where code asks for its line, however that is spaced, lines count.
        for asking in [
            "line !()",
            "line!()",
            "std::panic::Location::caller().line()",
        ] {
            let base = format!("pub fn n() -> u32 {{ {asking} }}\n");
            let with = format!("#[cfg(feature = \"extra\")]\npub const OFF: u8 = 42;\n{base}");
            assert!(
                !same_build(&with, &off(&with), &base, &off(&base)),
                "{asking}"
            );
        }
        // The column of a line that only moved down is the same.
        let base = "pub fn n() -> u32 { column ! () }\n";
        let with = format!("#[cfg(feature = \"extra\")]\npub const OFF: u8 = 42;\n{base}");
        assert!(same_build(&with, &off(&with), base, &off(base)));
        // A caller's location is asked for somewhere: a change of code that is left
        // out after it, or of the same length, moves no token that is kept.
        let tracked = "#[track_caller]\npub fn checked() {}\n#[cfg(feature = \"extra\")]\nfn off() -> u8 { 41 }\npub fn after() {}\n";
        let edited = tracked.replace("41", "42");
        assert!(same_build(&edited, &off(&edited), tracked, &off(tracked)));
        let longer = tracked.replace("fn off() -> u8 { 41 }", "fn off() -> u8 {\n    41\n}");
        assert!(!same_build(&longer, &off(&longer), tracked, &off(tracked)));
        // On one line, what is left out before it moves the column.
        let base =
            "#[cfg(feature = \"extra\")] const OFF: u32 = 1; pub const N: u32 = column!();\n";
        let with = base.replace("= 1;", "= 100;");
        assert!(!same_build(&with, &off(&with), base, &off(base)));
        // In code that is left out it asks nothing of this build.
        let base = "#[cfg(feature = \"extra\")]\nfn off() -> u32 {\n    line!()\n}\npub fn n() -> u32 { 1 }\n";
        let with = base.replace("    line!()\n", "    let _ = 2;\n    line!()\n");
        assert!(same_build(&with, &off(&with), base, &off(base)));
        assert_eq!(
            asks_where_it_is("#[track_caller] fn f() {}"),
            Asks::APlaceOfACall
        );
        assert_eq!(asks_where_it_is("fn f( {"), Asks::APlaceOfACall);
        assert_eq!(
            asks_where_it_is("fn f() -> u32 { std::panic::Location::caller().line() }"),
            Asks::APlaceOfACall
        );
        assert_eq!(
            asks_where_it_is("const N: u32 = line!() + column ! ();"),
            Asks::ItsOwnPlace
        );
        // Names alone ask nothing: a function `line`, a type `Location`.
        assert_eq!(
            asks_where_it_is(
                "struct Location; fn line() { let column = 1; let _ = !column; let _ = Location; }"
            ),
            Asks::Nothing
        );
        // `line!` above code that is left out stays where it is whatever comes after
        // it: what moves below is not asked for.
        let above = "pub const START: u32 = line!();\n#[cfg(feature = \"extra\")]\nfn off() -> u8 {\n    41\n}\npub fn answer() -> u32 { 7 }\n";
        let longer = above.replace("    41\n", "    let value = 41;\n    value + 1\n");
        assert!(same_build(&longer, &off(&longer), above, &off(above)));
        // A variant, a field or an arm that is left out goes with its comma: new in
        // the middle of a list, at its end with or without a comma of its own.
        for (without, with) in [
            (
                "pub enum F { Json, Toml }",
                "pub enum F { Json, #[cfg(feature = \"extra\")] Yaml, Toml }",
            ),
            (
                "pub enum F { Json, Toml, }",
                "pub enum F { Json, Toml, #[cfg(feature = \"extra\")] Yaml, }",
            ),
            (
                "pub enum F { Json, Toml }",
                "pub enum F { Json, Toml, #[cfg(feature = \"extra\")] Yaml }",
            ),
            (
                "pub enum F { Json }",
                "pub enum F { #[cfg(feature = \"extra\")] Yaml, Json }",
            ),
            (
                "pub struct O { pub depth: u8 }",
                "pub struct O { pub depth: u8, #[cfg(feature = \"extra\")] pub strict: bool }",
            ),
            (
                "fn f(n: u8) -> u8 { match n { 1 => 2, _ => 0 } }",
                "fn f(n: u8) -> u8 { match n { 1 => 2, #[cfg(feature = \"extra\")] 3 => 4, _ => 0 } }",
            ),
        ] {
            assert!(
                same_build(with, &off(with), without, &off(without)),
                "{with}"
            );
            // Built, it is another program.
            let all: std::collections::BTreeSet<String> =
                ["basic".to_string(), "extra".to_string()].into();
            let none = |text: &str| {
                instrument::inactive_ranges(text, &instrument::parse_source(text).unwrap(), &all)
            };
            assert!(
                !same_build(with, &none(with), without, &none(without)),
                "{with}"
            );
        }
        // However the list is laid out: a last comma here and not there, a comment
        // before the comma, an arm in braces that has no comma of its own, a field of a
        // tuple struct on lines of its own.
        for (without, with) in [
            (
                "enum E { A }",
                "enum E { A, #[cfg(feature = \"extra\")] B, }",
            ),
            (
                "enum E { A, }",
                "enum E { A, #[cfg(feature = \"extra\")] B }",
            ),
            (
                "enum E { A // explanation\n}",
                "enum E { A, // explanation\n#[cfg(feature = \"extra\")] B\n}",
            ),
            (
                "enum E { A }",
                "enum E { #[cfg(feature = \"extra\")] X /* note */, A }",
            ),
            (
                "fn f(n: u8) -> u8 { match n { 0 => 1, _ => 0, } }",
                "fn f(n: u8) -> u8 { match n { 0 => 1, #[cfg(feature = \"extra\")] 2 => { let x = n.saturating_add(1); x } _ => 0, } }",
            ),
            (
                "pub struct S(pub u8);",
                "pub struct S(\n    pub u8,\n    #[cfg(feature = \"extra\")] pub std::collections::BTreeMap<String, std::path::PathBuf>,\n);",
            ),
        ] {
            assert!(
                same_build(with, &off(with), without, &off(without)),
                "{with}"
            );
        }
        // A `cfg` that is on in the build is no part of it: reworded to another that
        // is on, put on, or taken off, the build is the same. One that is off is not.
        let settled = |text: &str| {
            let parsed = instrument::parse_source(text).unwrap();
            let mut ranges = instrument::inactive_ranges(text, &parsed, &features);
            ranges.extend(instrument::settled_cfg_attributes(text, &parsed, &features));
            ranges
        };
        let gated = "#[cfg(feature = \"basic\")]\npub const N: u8 = 1;\n";
        for other in [
            "#[cfg(any(feature = \"basic\", feature = \"extra\"))]\npub const N: u8 = 1;\n",
            "pub const N: u8 = 1;\n",
            "#[cfg(not(feature = \"extra\"))]\npub const N: u8 = 1;\n",
        ] {
            assert!(
                same_build(other, &settled(other), gated, &settled(gated)),
                "{other}"
            );
        }
        let off_now = "#[cfg(feature = \"extra\")]\npub const N: u8 = 1;\n";
        assert!(!same_build(
            off_now,
            &settled(off_now),
            gated,
            &settled(gated)
        ));
        // An entry of an array that is left out, and a tuple of one after a statement
        // that is: the first is the same build, the second another program.
        let plain = "pub const FORMATS: &[&str] = &[\"json\"];\n";
        let more =
            "pub const FORMATS: &[&str] = &[\"json\", #[cfg(feature = \"extra\")] \"yaml\"];\n";
        assert!(same_build(more, &off(more), plain, &off(plain)));
        let one =
            "fn f() -> impl Sized {\n    #[cfg(feature = \"extra\")]\n    trace();\n    (1u8)\n}\n";
        let tuple = one.replace("(1u8)", "(1u8,)");
        assert!(!same_build(&tuple, &off(&tuple), one, &off(one)));
        // In a function that tells its caller's place, a statement that is left out in
        // place of a blank line moves nothing that is kept, its semicolon included.
        let blank = "#[track_caller]\npub fn checked() {\n\n    a();\n}\n";
        let stated = blank.replace("{\n\n", "{\n    #[cfg(feature = \"extra\")] extra();\n");
        assert!(same_build(&stated, &off(&stated), blank, &off(blank)));
        // A comma that is kept and moved is told.
        let moved = "#[track_caller]\npub fn checked() {\n    a(1,\n      2);\n}\n";
        let wider = moved.replace("a(1,", "a(1  ,");
        assert!(!same_build(&wider, &off(&wider), moved, &off(moved)));
        // A call that is left out goes with its semicolon.
        let plain = "fn f() {\n    a();\n    b();\n}\n";
        let extra = "fn f() {\n    a();\n    #[cfg(feature = \"extra\")]\n    enable_extra();\n    b();\n}\n";
        assert!(same_build(extra, &off(extra), plain, &off(plain)));
        // An attribute that is on at the head of what another turns off: all of it is
        // left out.
        let both = "fn f() -> u8 {\n    #[cfg(feature = \"basic\")]\n    #[cfg(feature = \"extra\")]\n    return 1;\n    2\n}\n";
        let neither = both.replace("basic", "extra");
        assert!(same_build(
            both,
            &settled(both),
            &neither,
            &settled(&neither)
        ));
        // One that depends on something else stays a token.
        let target = "#[cfg(unix)]\npub const N: u8 = 1;\n";
        assert!(!same_build(
            target,
            &settled(target),
            gated,
            &settled(gated)
        ));

        // A line more above a `column!` moves nothing it tells; a column more does.
        let col = "#[cfg(feature = \"extra\")]\nfn off() -> u8 {\n    41\n}\npub const COL: u32 = column!();\n";
        let longer = col.replace("    41\n", "    let value = 41;\n    value + 1\n");
        assert!(same_build(&longer, &off(&longer), col, &off(col)));
        // Below it, it moves.
        let below = "#[cfg(feature = \"extra\")]\nfn off() -> u8 {\n    41\n}\npub const START: u32 = line!();\n";
        let longer = below.replace("    41\n", "    let value = 41;\n    value + 1\n");
        assert!(!same_build(&longer, &off(&longer), below, &off(below)));
        // Documentation that grows before code that is left out moves nothing that is
        // asked for: written as an attribute of the file or of an item on the line of
        // the code, or as a comment above it.
        for (short, long) in [
            ("#![doc = \"a\"]", "#![doc = \"long\"]"),
            ("#[doc = \"a\"]", "#[doc = \"long\"]"),
        ] {
            let before = format!(
                "{short} #[cfg(feature = \"extra\")] fn off() {{ 1 }}\n#[track_caller]\npub fn n() -> u32 {{ line!() }}\n"
            );
            let after = before.replace(short, long).replace("{ 1 }", "{ 2 }");
            assert!(
                same_build(&after, &off(&after), &before, &off(&before)),
                "{short}"
            );
        }
        let before = "//! Old docs.\n#[cfg(feature = \"extra\")]\nfn off() {}\n#[track_caller]\npub fn n() -> u32 { line!() }\n";
        let after = before.replace("//! Old docs.", "//! Much longer documentation.");
        assert!(same_build(&after, &off(&after), before, &off(before)));
    }

    #[test]
    fn code_that_is_new_or_gone_is_described_as_such() {
        let new = OLD.replace("    n\n", "    let n = n + 1;\n    n\n");
        let found = between(OLD, &new);
        assert_eq!(found.len(), 1, "{found:?}");
        assert_eq!(found[0].describe(), "without this line (it is new)");
        assert_eq!(
            (found[0].line, found[0].code.as_str()),
            (8, "let n = n + 1;")
        );
        let back = between(&new, OLD);
        assert_eq!(
            back[0].describe(),
            "with the code that was removed here: `let n = n + 1;`"
        );
        assert_eq!((back[0].line, back[0].code.as_str()), (8, "n"));
        let two = OLD.replace("    n\n", "    let n = n + 1;\n    let n = n * 2;\n    n\n");
        assert_eq!(
            between(OLD, &two)[0].describe(),
            "without these 2 lines (they are new)"
        );
    }

    #[test]
    fn a_change_that_is_not_rust_alone_is_kept_for_the_whole() {
        // The parameter and its use change together: neither alone is a program that
        // parses differently, but one alone does not build. A brace that moves does not
        // even parse alone.
        let new = OLD.replace("    n\n}", "    if n > 1 {\n    n\n    }\n}");
        let found = between(OLD, &new);
        assert!(found.len() >= 2, "{found:?}");
        assert!(
            found.iter().any(|change| change.undone.is_none()),
            "{found:?}"
        );
        assert_eq!(all_undone(&new, &found).as_deref(), Some(OLD));
        // A file that is not Rust now has nothing to try.
        assert_eq!(changes("fn f( {", &[]), []);
    }

    #[test]
    fn subhunks_decompose_added_logical_terms_without_duplicating_whole_hunk() {
        let old = "\
pub fn allow(a: bool, b: bool, c: bool) -> bool {
    a
}
";
        // Adding two terms yields whole hunk (`a`) plus two sub-hunks (`a && c`, `a && b`).
        let new = "\
pub fn allow(a: bool, b: bool, c: bool) -> bool {
    a && b && c
}
";
        let found = between(old, new);
        let wholes: Vec<_> = found.iter().filter(|c| !c.is_subhunk()).collect();
        let subs: Vec<_> = found.iter().filter(|c| c.is_subhunk()).collect();
        assert_eq!(wholes.len(), 1, "{found:?}");
        assert_eq!(subs.len(), 2, "{found:?}");
        assert_eq!(subs[0].describe(), "without added condition `b`");
        assert_eq!(
            subs[0].undone.as_deref(),
            Some("pub fn allow(a: bool, b: bool, c: bool) -> bool {\n    a && c\n}\n")
        );
        assert_eq!(subs[1].describe(), "without added condition `c`");
        assert_eq!(
            subs[1].undone.as_deref(),
            Some("pub fn allow(a: bool, b: bool, c: bool) -> bool {\n    a && b\n}\n")
        );
        // `all_undone` ignores sub-hunks and restores the original file cleanly.
        assert_eq!(all_undone(new, &found).as_deref(), Some(old));

        // Adding only ONE term (`a` -> `a && b`) deduplicates against the whole hunk.
        let one_added = "\
pub fn allow(a: bool, b: bool, c: bool) -> bool {
    a && b
}
";
        let one_found = between(old, one_added);
        assert_eq!(one_found.len(), 1, "{one_found:?}");
        assert!(!one_found[0].is_subhunk());
    }

    #[test]
    fn subhunks_decompose_added_struct_fields_with_base_only() {
        let old = "\
#[derive(Default)]
pub struct Cfg { pub a: u8, pub b: u8, pub c: u8 }
pub fn make(base: Cfg) -> Cfg {
    Cfg { a: 1, ..base }
}
";
        let new = "\
#[derive(Default)]
pub struct Cfg { pub a: u8, pub b: u8, pub c: u8 }
pub fn make(base: Cfg) -> Cfg {
    Cfg { a: 1, b: 2, c: 3, ..base }
}
";
        let found = between(old, new);
        let subs: Vec<_> = found.iter().filter(|c| c.is_subhunk()).collect();
        assert_eq!(subs.len(), 2, "{found:?}");
        assert_eq!(subs[0].describe(), "without added field `b: 2`");
        assert_eq!(subs[1].describe(), "without added field `c: 3`");
        assert!(
            subs[0]
                .undone
                .as_deref()
                .unwrap()
                .contains("a: 1,  c: 3, ..base")
                || subs[0]
                    .undone
                    .as_deref()
                    .unwrap()
                    .contains("a: 1, c: 3, ..base")
        );
        assert!(
            subs[1]
                .undone
                .as_deref()
                .unwrap()
                .contains("a: 1, b: 2,  ..base")
                || subs[1]
                    .undone
                    .as_deref()
                    .unwrap()
                    .contains("a: 1, b: 2, ..base")
        );
        assert_eq!(all_undone(new, &found).as_deref(), Some(old));
    }

    #[test]
    fn subhunks_decompose_added_guard_and_call_statements() {
        let old = "\
pub fn validate(x: i32) -> Result<i32, &'static str> {
    Ok(x)
}
";
        let new = "\
pub fn validate(x: i32) -> Result<i32, &'static str> {
    if x < 0 { return Err(\"neg\"); }
    if x > 100 { return Err(\"big\"); }
    Ok(x)
}
";
        let found = between(old, new);
        let subs: Vec<_> = found.iter().filter(|c| c.is_subhunk()).collect();
        assert_eq!(subs.len(), 2, "{found:?}");
        assert_eq!(
            subs[0].describe(),
            "without added statement `if x < 0 { return Err(\"neg\"); }`"
        );
        assert_eq!(
            subs[1].describe(),
            "without added statement `if x > 100 { return Err(\"big\"); }`"
        );
        assert_eq!(all_undone(new, &found).as_deref(), Some(old));
    }

    #[test]
    fn subhunks_respect_containment_order_skip_attrs_nested_blocks_return_and_line_macros() {
        // 1. Pure insertion inside an existing `if` must not emit a sub-hunk deleting the `if`.
        let old_guard = "\
pub fn f(denied: bool) {
    if denied {
        return;
    }
    work();
}
";
        let new_guard = "\
pub fn f(denied: bool) {
    if denied {
        audit();
        return;
    }
    work();
}
";
        let found = between(old_guard, new_guard);
        assert!(
            !found
                .iter()
                .any(|c| c.is_subhunk() && c.code.starts_with("if denied")),
            "{found:?}"
        );

        // 2. Ordered subsequence matching: prepending `check() && extra()` before
        // `gate() && check()` deletes only the prepended terms, never the existing trailing `check()`.
        let old_seq = "pub fn f() -> bool {\n    gate() && check()\n}\n";
        let new_seq = "pub fn f() -> bool {\n    check() && extra() && gate() && check()\n}\n";
        let subs: Vec<_> = between(old_seq, new_seq)
            .into_iter()
            .filter(|c| c.is_subhunk())
            .collect();
        assert_eq!(subs.len(), 2, "{subs:?}");
        assert_eq!(
            subs[0].undone.as_deref(),
            Some("pub fn f() -> bool {\n    extra() && gate() && check()\n}\n")
        );
        assert_eq!(
            subs[1].undone.as_deref(),
            Some("pub fn f() -> bool {\n    check() && gate() && check()\n}\n")
        );

        // 3. `#[cfg(test)]` block and `#[mutants::skip]` field/expr suppress descendants.
        let old_cfg = "\
pub fn f() {
    #[cfg(test)]
    {
        keep();
    }
}
";
        let new_cfg = "\
pub fn f() {
    #[cfg(test)]
    {
        keep();
        probe_a();
        probe_b();
    }
}
";
        let cfg_subs: Vec<_> = between(old_cfg, new_cfg)
            .into_iter()
            .filter(|c| c.is_subhunk())
            .collect();
        assert!(cfg_subs.is_empty(), "{cfg_subs:?}");

        let old_skip = "pub fn f(base: S) -> S {\n    S { ..base }\n}\n";
        let new_skip =
            "pub fn f(base: S) -> S {\n    S { #[mutants::skip] flag: a && b && c, ..base }\n}\n";
        let skip_subs: Vec<_> = between(old_skip, new_skip)
            .into_iter()
            .filter(|c| c.is_subhunk())
            .collect();
        assert!(skip_subs.is_empty(), "{skip_subs:?}");

        // 4. Nested block replacement decomposes added statements inside the inner `if`.
        let old_nested = "pub fn f(ok: bool) {\n    if ok { foo(); }\n}\n";
        let new_nested = "pub fn f(ok: bool) {\n    if ok { foo(); bar(); baz(); }\n}\n";
        let nested_subs: Vec<_> = between(old_nested, new_nested)
            .into_iter()
            .filter(|c| c.is_subhunk())
            .collect();
        assert_eq!(nested_subs.len(), 2, "{nested_subs:?}");
        assert_eq!(
            nested_subs[0].describe(),
            "without added statement `bar();`"
        );
        assert_eq!(
            nested_subs[1].describe(),
            "without added statement `baz();`"
        );

        // 5. Tail `return a` -> `return a && b && c` decomposes `b` and `c`.
        let old_ret = "pub fn f(a: bool, b: bool, c: bool) -> bool {\n    return a\n}\n";
        let new_ret = "pub fn f(a: bool, b: bool, c: bool) -> bool {\n    return a && b && c\n}\n";
        let ret_subs: Vec<_> = between(old_ret, new_ret)
            .into_iter()
            .filter(|c| c.is_subhunk())
            .collect();
        assert_eq!(ret_subs.len(), 2, "{ret_subs:?}");

        // 6. `line!()`-sensitive duplicates on different lines are both kept.
        let old_line = "pub fn f() {\n    init();\n}\n";
        let new_line = "pub fn f() {\n    init();\n    record(line!());\n    record(line!());\n}\n";
        let line_subs: Vec<_> = between(old_line, new_line)
            .into_iter()
            .filter(|c| c.is_subhunk())
            .collect();
        assert_eq!(line_subs.len(), 2, "{line_subs:?}");
        assert_ne!(line_subs[0].line, line_subs[1].line);

        // 7. `#[cfg(...)]` struct field of the same name is treated as distinct.
        let old_cfg_field =
            "pub fn f(base: S) -> S {\n    S { #[cfg(not(feature = \"x\"))] a: 1, ..base }\n}\n";
        let new_cfg_field = "pub fn f(base: S) -> S {\n    S { #[cfg(not(feature = \"x\"))] a: 1, #[cfg(feature = \"x\")] a: 2, b: 3, ..base }\n}\n";
        let cfg_field_subs: Vec<_> = between(old_cfg_field, new_cfg_field)
            .into_iter()
            .filter(|c| c.is_subhunk())
            .collect();
        assert_eq!(cfg_field_subs.len(), 2, "{cfg_field_subs:?}");

        // 8. Rewriting an existing field's `cfg` does not delete that field.
        let old_cfg_rw = "pub fn f(base: S) -> S {\n    S { #[cfg(all())] a: 1, ..base }\n}\n";
        let new_cfg_rw = "pub fn f(base: S) -> S {\n    S { #[cfg(not(any()))] a: 1, ..base }\n}\n";
        let rw_subs: Vec<_> = between(old_cfg_rw, new_cfg_rw)
            .into_iter()
            .filter(|c| c.is_subhunk())
            .collect();
        assert!(rw_subs.is_empty(), "{rw_subs:?}");

        // 9. Tail `if` moving to non-tail when a statement is appended must not delete the `if`.
        let old_tail = "pub fn f(flag: bool) {\n    if flag { audit(); }\n}\n";
        let new_tail = "pub fn f(flag: bool) {\n    if flag { audit(); } work();\n}\n";
        let tail_subs: Vec<_> = between(old_tail, new_tail)
            .into_iter()
            .filter(|c| c.is_subhunk())
            .collect();
        assert!(
            !tail_subs.iter().any(|c| c.code.starts_with("if flag")),
            "{tail_subs:?}"
        );

        // 10. Prepending a sibling `if` must not misalign existing `if` blocks.
        let old_sib = "pub fn f(p: bool, q: bool) {\n    if p { a(); b(); } if q { a(); }\n}\n";
        let new_sib = "pub fn f(p: bool, q: bool, r: bool) {\n    if r { c(); } if p { a(); b(); } if q { a(); }\n}\n";
        let sib_subs: Vec<_> = between(old_sib, new_sib)
            .into_iter()
            .filter(|c| c.is_subhunk())
            .collect();
        assert!(!sib_subs.iter().any(|c| c.code == "b();"), "{sib_subs:?}");

        // 11. `#[cfg(test)]` on compound assignment suppresses RHS block, and `#[mutants::skip]`
        // inside a parent statement suppresses deleting that parent statement.
        let old_assign = "pub fn f(x: &mut i32) {\n    #[cfg(test)] x += { init(); 1 };\n}\n";
        let new_assign = "pub fn f(x: &mut i32) {\n    #[cfg(test)] x += {\n        init();\n        probe_a();\n        probe_b();\n        1\n    };\n}\n";
        let assign_subs: Vec<_> = between(old_assign, new_assign)
            .into_iter()
            .filter(|c| c.is_subhunk())
            .collect();
        assert!(assign_subs.is_empty(), "{assign_subs:?}");

        let old_prot = "pub fn f() {\n    init();\n}\n";
        let new_prot = "pub fn f(c: bool) {\n    init();\n    if c { #[mutants::skip] protected(); }\n    extra();\n}\n";
        let prot_subs: Vec<_> = between(old_prot, new_prot)
            .into_iter()
            .filter(|c| c.is_subhunk())
            .collect();
        assert!(
            !prot_subs.iter().any(|c| c.code.contains("protected")),
            "{prot_subs:?}"
        );

        // 12. Same logical operator nested inside a block resets `in_logical`.
        let old_inner = "pub fn f(a: bool, b: bool) -> bool {\n    a && {\n        b\n    }\n}\n";
        let new_inner = "pub fn f(a: bool, b: bool, c: bool, d: bool) -> bool {\n    a && {\n        b\n        && c\n        && d\n    }\n}\n";
        let inner_subs: Vec<_> = between(old_inner, new_inner)
            .into_iter()
            .filter(|c| c.is_subhunk())
            .collect();
        assert_eq!(inner_subs.len(), 2, "{inner_subs:?}");
    }
}
