//! Token-level equivalence checks ignoring comments, docs, and formatting.

use proc_macro2::{TokenStream, TokenTree};

/// Whether two sources are the same program text: the same tokens, whatever the
/// comments, the documentation and the spacing are. `None` when one does not lex.
/// What stands for code a build leaves out, where two texts are compared as that build
/// has them (`reverts::same_build`): no name a program has.
pub(crate) const LEFT_OUT: &str = "__tmt_left_out__";

pub(crate) fn same_tokens(a: &str, b: &str) -> Option<bool> {
    same_tokens_of(a, b, false)
}

/// The same for two texts in which what a build leaves out is [`LEFT_OUT`]: it goes with
/// the comma or the semicolon it stands before.
pub(crate) fn same_built_tokens(a: &str, b: &str) -> Option<bool> {
    same_tokens_of(a, b, true)
}

/// Words that cannot be the name of what is called: `let (a,)` is a pattern in
/// parentheses, not arguments.
const KEYWORDS: [&str; 39] = [
    "as", "async", "await", "box", "break", "const", "continue", "crate", "do", "dyn", "else",
    "enum", "extern", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
    "mut", "pub", "ref", "return", "static", "struct", "super", "trait", "try", "type", "unsafe",
    "use", "where", "while", "yield", "union",
];

fn same_tokens_of(a: &str, b: &str, holes: bool) -> Option<bool> {
    // `plain`: outside of what is given to a macro. A macro may tell any token from
    // another, also documentation (`stringify!`, a rule that matches an attribute).
    // `list`: the delimiter of the group this is the inside of, and whether a name
    // comes before it (a call, the fields of a tuple struct).
    fn tokens(
        stream: TokenStream,
        plain: bool,
        holes: bool,
        list: Option<(proc_macro2::Delimiter, bool)>,
    ) -> Vec<String> {
        let mut out = vec![];
        let mut trees = stream.into_iter().peekable();
        // Whether the last tokens were a name and `!`: what follows is a macro's.
        let (mut named, mut called) = (false, false);
        // Whether the last token was a name that something can be called by.
        let mut callable = false;
        // Whether the last token was the `'` of a lifetime or a label.
        let mut ticked = false;
        // Whether the last tokens were the `#` or `#!` of an attribute.
        let mut hashed = false;
        while let Some(tree) = trees.next() {
            let (was_named, was_called, was_callable) = (named, called, callable);
            let (was_ticked, was_hashed) = (ticked, hashed);
            (named, called, callable, ticked, hashed) = (false, false, false, false, false);
            match tree {
                // `#[doc = ".."]` and `#![doc = ".."]`: what a doc comment is read as.
                TokenTree::Punct(hash) if hash.as_char() == '#' => {
                    let bang =
                        matches!(trees.peek(), Some(TokenTree::Punct(p)) if p.as_char() == '!')
                            .then(|| trees.next());
                    // Given to a macro, an attribute written out is a token it may
                    // match on; a doc comment there is still prose (the documentation
                    // of what a macro makes).
                    let is_doc = matches!(trees.peek(), Some(TokenTree::Group(group))
                        if group.delimiter() == proc_macro2::Delimiter::Bracket
                            && matches!(group.stream().into_iter().next(),
                                Some(TokenTree::Ident(name)) if name == "doc")
                            && (plain
                                || group
                                    .span()
                                    .source_text()
                                    .is_some_and(|written| written.starts_with('/'))));
                    if is_doc {
                        trees.next();
                        continue;
                    }
                    out.push("#".to_string());
                    if bang.is_some() {
                        out.push("!".to_string());
                    }
                    hashed = true;
                }
                TokenTree::Group(group) => {
                    // `name!(..)`, and `macro_rules! name {..}` with its name between.
                    let inner = tokens(
                        group.stream(),
                        plain && !was_called,
                        holes,
                        Some((group.delimiter(), was_callable)),
                    );
                    out.push(format!("{:?}({})", group.delimiter(), inner.join(" ")));
                    // What comes in parentheses right after `(..)` or `[..]` is given
                    // to it: `(self.callback)(value,)`, `table[0](value,)`.
                    // Not the brackets of an attribute: `#[allow(..)] (a,)` is a tuple.
                    callable = !was_hashed
                        && matches!(
                            group.delimiter(),
                            proc_macro2::Delimiter::Parenthesis | proc_macro2::Delimiter::Bracket
                        );
                }
                TokenTree::Punct(mark) => {
                    called = was_named && mark.as_char() == '!';
                    ticked = mark.as_char() == '\'';
                    out.push(mark.to_string());
                }
                TokenTree::Ident(name) => {
                    named = true;
                    // The name of a macro that is being defined.
                    called = was_called;
                    let name = name.to_string();
                    // Not a keyword, not the name of a lifetime or a label
                    // (`&'a (u8,)`, `break 'outer (1,)`), and not what stands for
                    // code that is left out (a statement before a tuple). After `fn`
                    // come parameters: the type of a function.
                    callable = name == "fn"
                        || (!was_ticked
                            && !KEYWORDS.contains(&name.as_str())
                            && !(holes && name == LEFT_OUT));
                    out.push(name);
                }
                other => out.push(other.to_string()),
            }
        }
        if !plain {
            return out;
        }
        // What a build leaves out goes with its comma (the one after it, or, at the end
        // of a list, the one before it), or with the semicolon of a statement that is
        // an expression.
        let mut kept: Vec<String> = Vec::with_capacity(out.len());
        let mut items = out.into_iter().peekable();
        while let Some(item) = items.next() {
            if !holes || item != LEFT_OUT {
                kept.push(item);
                continue;
            }
            if items.peek().is_some_and(|next| next == "," || next == ";") {
                items.next();
            } else if items.peek().is_none() && kept.last().is_some_and(|last| last == ",") {
                // Inside parentheses that may be a tuple, a trailing comma before a left-out
                // element can be the sole comma of a 1-tuple (even when inner type arguments
                // contain commas, as in `(f::<A, B>(), #[cfg(off)] 2)`). Keep it conservatively.
                let may_be_tuple =
                    matches!(list, Some((proc_macro2::Delimiter::Parenthesis, false)));
                if !may_be_tuple {
                    kept.pop();
                }
            }
        }
        // A comma after the last of a list says nothing: of braces and brackets, and of
        // what follows a name that is called (arguments, the fields of a tuple struct).
        // Elsewhere `(a,)` is not `(a)`.
        if kept.last().is_some_and(|last| last == ",") {
            let says_nothing = match list {
                Some((proc_macro2::Delimiter::Brace | proc_macro2::Delimiter::Bracket, _)) => true,
                Some((proc_macro2::Delimiter::Parenthesis, after_a_name)) => after_a_name,
                _ => false,
            };
            if says_nothing {
                kept.pop();
            }
        }
        kept
    }
    let lex = |text: &str| Some(tokens(text.parse::<TokenStream>().ok()?, true, holes, None));
    Some(lex(a)? == lex(b)?)
}
