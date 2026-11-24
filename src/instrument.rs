//! Finds mutation sites in Rust source and rewrites them into probes.
//!
//! Every site stays in the compiled code (mutant schemata): a comparison `a >= b` becomes a
//! call that records what it saw and, when a mutant is switched on, returns the mutated
//! result. One build serves the trace run and every mutant.
//!
//! The rewrite only inserts text on the line where the site is, so line numbers in compiler
//! diagnostics still match the original file.

use std::collections::{BTreeSet, HashSet};

use anyhow::{Context, Result};
use proc_macro2::{Span, TokenStream, TokenTree};
use quote::ToTokens;
use serde::Serialize;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use crate::plan::CmpOp;

/// Name of the module injected into the crate root.
pub const RUNTIME_MOD: &str = "__tmt_rt";

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LiteralSide {
    None,
    Rhs(i128),
    Lhs(i128),
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub enum SiteKind {
    Cmp {
        op: CmpOp,
        lhs: String,
        rhs: String,
        literal: LiteralSide,
        /// Byte range of the operator and the whitespace around it.
        gap: (usize, usize),
    },
    /// `lhs && rhs` or `lhs || rhs`.
    Logic {
        and: bool,
        lhs: String,
        rhs: String,
        gap: (usize, usize),
    },
    /// `lhs + rhs` and friends. Only primitive numbers are mutated; for other operand
    /// types the probe performs the original operation and reports nothing.
    Arith {
        op: ArithOp,
        /// `lhs op= rhs` rather than `lhs op rhs`.
        assign: bool,
        lhs: String,
        rhs: String,
        gap: (usize, usize),
    },
    /// `!operand`. Only `bool` is mutated (the `!` is dropped).
    Not {
        /// Byte range of the `!` and whatever separates it from the operand.
        bang: (usize, usize),
        operand: String,
        /// Deleting `!` would glue the preceding and following tokens (`return!x`).
        needs_space: bool,
    },
    /// The result of a function whose exits reach the probe. The site's range is the
    /// inside of the body's braces and its text is the function signature.
    Return {
        name: String,
        /// `None` for a function returning `bool`; for one returning `Option<T>`, the
        /// return type as written.
        option: Option<String>,
        /// Byte range of the body when it is a single expression: the function's result
        /// is then exactly that expression's value.
        sole_expr: Option<(usize, usize)>,
    },
    /// `receiver.max(argument)` or `.min(..)`: the other of the two.
    MinMax {
        max: bool,
        receiver: String,
        argument: String,
        /// Byte ranges of `.max(` and of the closing `)`.
        call: (usize, usize),
        close: (usize, usize),
    },
    /// `receiver.first()` or `.last()` of a slice, a `Vec`, an array or a `BTreeSet`: the
    /// other end. The two are the same element while there is at most one.
    SliceEnd {
        first: bool,
        receiver: String,
        /// Byte range of `.first()`.
        call: (usize, usize),
        /// Byte range of the method's name.
        method: (usize, usize),
    },
    /// `lhs..rhs` or `lhs..=rhs` on integers: the end moves by one.
    Range {
        inclusive: bool,
        lhs: String,
        rhs: String,
        gap: (usize, usize),
    },
    /// An arm of a `match` that ends in a catch-all arm: without this arm the value falls
    /// through to the arms below.
    Arm {
        pattern: String,
        /// Where to put the probe: the start of the `=>`, or the range of an existing guard.
        guard: ArmGuard,
        removal: (usize, usize),
    },
    /// A call or assignment statement that can be deleted.
    Stmt,
    /// `-operand`: the minus is dropped.
    Neg {
        /// Byte range of the `-` and whatever separates it from the operand.
        minus: (usize, usize),
        operand: String,
        /// The operand is a literal: its type is whatever the context asks for.
        literal: bool,
        /// Deleting `-` would glue the preceding and following tokens (`return-1`).
        needs_space: bool,
    },
    /// The guard of a match arm: replaced by `true` or `false`.
    Guard {
        /// Whether `false` is a mutant of its own. It is not where the arm can be removed,
        /// which is the same thing.
        can_be_false: bool,
    },
    /// A field of a struct literal with a base (`..base`): the field is left to the base.
    Field {
        name: String,
        /// Written without its name (`S { name, ..base }`).
        shorthand: bool,
        /// The expression used by a probe, where the base can safely be read again.
        from_base: Option<String>,
        /// The field and its trailing comma, including any attributes.
        removal: (usize, usize),
    },
    /// The whole body of a function, replaced by a value of its return type. The site's
    /// range is the inside of the body's braces and its text is the function signature.
    Body {
        name: String,
        value: String,
        /// The body when it is one expression that only computes its value (no call,
        /// no macro, nothing assigned, no early exit): replacing the body by the value
        /// that expression always had is then the same program for the tests.
        sole_expr: Option<(usize, usize)>,
    },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ArmGuard {
    /// No guard: one is added before the `=>` at this offset.
    None(usize),
    /// `if <expr>` with the expression at this byte range.
    Existing(usize, usize),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

impl ArithOp {
    /// How tightly the operator binds: a higher number before a lower one.
    pub fn binding(self) -> u8 {
        match self {
            ArithOp::Mul | ArithOp::Div | ArithOp::Rem => 5,
            ArithOp::Add | ArithOp::Sub => 4,
            ArithOp::Shl | ArithOp::Shr => 3,
            ArithOp::BitAnd => 2,
            ArithOp::BitXor => 1,
            ArithOp::BitOr => 0,
        }
    }

    pub const ALL: [ArithOp; 10] = [
        ArithOp::Add,
        ArithOp::Sub,
        ArithOp::Mul,
        ArithOp::Div,
        ArithOp::Rem,
        ArithOp::BitAnd,
        ArithOp::BitOr,
        ArithOp::BitXor,
        ArithOp::Shl,
        ArithOp::Shr,
    ];

    pub fn symbol(self) -> &'static str {
        match self {
            ArithOp::Add => "+",
            ArithOp::Sub => "-",
            ArithOp::Mul => "*",
            ArithOp::Div => "/",
            ArithOp::Rem => "%",
            ArithOp::BitAnd => "&",
            ArithOp::BitOr => "|",
            ArithOp::BitXor => "^",
            ArithOp::Shl => "<<",
            ArithOp::Shr => ">>",
        }
    }

    /// The operator the mutant uses instead.
    pub fn swapped(self) -> ArithOp {
        match self {
            ArithOp::Add => ArithOp::Sub,
            ArithOp::Sub => ArithOp::Add,
            ArithOp::Mul => ArithOp::Div,
            ArithOp::Div => ArithOp::Mul,
            ArithOp::Rem => ArithOp::Div,
            ArithOp::BitAnd => ArithOp::BitOr,
            ArithOp::BitOr => ArithOp::BitAnd,
            ArithOp::BitXor => ArithOp::BitAnd,
            ArithOp::Shl => ArithOp::Shr,
            ArithOp::Shr => ArithOp::Shl,
        }
    }

    /// The second operator a mutant uses instead. The shifts have none.
    pub fn swapped2(self) -> Option<ArithOp> {
        Some(match self {
            ArithOp::Add => ArithOp::Mul,
            ArithOp::Sub => ArithOp::Div,
            ArithOp::Mul => ArithOp::Add,
            ArithOp::Div => ArithOp::Rem,
            ArithOp::Rem => ArithOp::Add,
            ArithOp::BitAnd | ArithOp::BitOr => ArithOp::BitXor,
            ArithOp::BitXor => ArithOp::BitOr,
            ArithOp::Shl | ArithOp::Shr => return None,
        })
    }

    /// The second replacement where the operator is written as an assignment or not.
    /// Bits collected with `|=` from zero are the same bits with `^=`, and `&=` with
    /// `^=` is rarely a slip of the pen: as in cargo-mutants, those two are left out.
    pub fn second(self, assign: bool) -> Option<ArithOp> {
        if assign && matches!(self, ArithOp::BitAnd | ArithOp::BitOr) {
            return None;
        }
        self.swapped2()
    }

    /// Bit operators exist for integers only, the others for floats too.
    pub fn is_bitwise(self) -> bool {
        matches!(
            self,
            ArithOp::BitAnd | ArithOp::BitOr | ArithOp::BitXor | ArithOp::Shl | ArithOp::Shr
        )
    }

    /// The name the runtime uses for this operator in its probe methods and traits.
    fn stem(self) -> (&'static str, &'static str) {
        match self {
            ArithOp::Add => ("add", "Add"),
            ArithOp::Sub => ("sub", "Sub"),
            ArithOp::Mul => ("mul", "Mul"),
            ArithOp::Div => ("div", "Div"),
            ArithOp::Rem => ("rem", "Rem"),
            ArithOp::BitAnd => ("bitand", "BitAnd"),
            ArithOp::BitOr => ("bitor", "BitOr"),
            ArithOp::BitXor => ("bitxor", "BitXor"),
            ArithOp::Shl => ("shl", "Shl"),
            ArithOp::Shr => ("shr", "Shr"),
        }
    }

    /// Name of the probe method in the runtime.
    pub(crate) fn method(self, assign: bool) -> String {
        format!(
            "tmt_{}{}",
            self.stem().0,
            if assign { "_assign" } else { "" }
        )
    }

    /// Trait with the probe for every type that is not a primitive number.
    pub(crate) fn fallback_trait(self, assign: bool) -> String {
        format!("Any{}{}", self.stem().1, if assign { "Assign" } else { "" })
    }

    /// Trait with the probe for primitive numbers.
    fn prim_trait(self, assign: bool) -> &'static str {
        match (self.is_bitwise(), assign) {
            (false, false) => "ArithPrim",
            (true, false) => "BitPrim",
            (false, true) => "ArithAssignPrim",
            (true, true) => "BitAssignPrim",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Site {
    pub id: u32,
    pub kind: SiteKind,
    /// 1-based line and column of the start of the site.
    pub line: usize,
    pub column: usize,
    /// Byte range in the original source.
    pub start: usize,
    pub end: usize,
    pub text: String,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TestModule {
    pub name: String,
    /// The `#[path = "..."]` of the declaration, relative to the declaring file's directory.
    pub path: Option<String>,
}

#[derive(Debug, Default)]
pub struct Discovery {
    pub sites: Vec<Site>,
    /// Sites that cannot be switched while the program runs: inside a `const fn`, the
    /// body of a function that returns `impl Iterator`, a compound assignment to a place
    /// that is more than a variable. Their mutants are built one by one
    /// (`source_mutants`). They have no number.
    pub rebuild_sites: Vec<Site>,
    /// Functions that return nothing and consist of one statement that is a site: an
    /// empty body is the mutant that deletes the statement, so it is not made twice.
    pub single_statement_bodies: Vec<String>,
    /// Out-of-line modules that only exist under `cfg(test)` (`#[cfg(test)] mod tests;`).
    pub test_modules: Vec<TestModule>,
}

/// Finds the sites in one source file, numbering them from `first_id`.
pub fn discover(source: &str, first_id: u32) -> Result<Discovery> {
    discover_with_errors(source, first_id, &[])
}

pub(crate) fn discover_with_errors(
    source: &str,
    first_id: u32,
    error_values: &[&str],
) -> Result<Discovery> {
    let file = parse_source(source)?;
    Ok(discover_parsed(source, &file, first_id, error_values))
}

pub(crate) fn parse_source(source: &str) -> Result<syn::File> {
    // The parser drops a byte order mark and a `#!` line before it numbers bytes, so its
    // offsets would not be offsets into `source`.
    if source.starts_with('\u{feff}') || (source.starts_with("#!") && !source.starts_with("#![")) {
        anyhow::bail!("starts with a byte order mark or a `#!` line");
    }
    syn::parse_file(source).context("not parseable as a Rust source file")
}

pub(crate) fn discover_parsed(
    source: &str,
    file: &syn::File,
    first_id: u32,
    error_values: &[&str],
) -> Discovery {
    // A macro defined here under the name of a standard one (`macro_rules! vec`) can do
    // anything with its arguments.
    let local_macros = file
        .items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Macro(m) => m.ident.as_ref().map(|i| i.to_string()),
            _ => None,
        })
        .collect();
    let mut finder = Finder {
        source,
        next_id: first_id,
        found: Discovery::default(),
        local_macros,
        const_depth: 0,
        error_values,
        for_ranges: std::collections::HashSet::new(),
    };
    finder.visit_file(file);
    finder.found
}

struct Finder<'s> {
    error_values: &'s [&'s str],
    /// Names of the macros this file defines at its top level.
    local_macros: Vec<String>,
    source: &'s str,
    next_id: u32,
    found: Discovery,
    /// How many `const fn` the visitor is inside of.
    const_depth: usize,
    /// Where the ranges start that a `for` runs through directly.
    for_ranges: std::collections::HashSet<usize>,
}

impl Finder<'_> {
    /// Registers the sites of a function as a whole: its result (for `bool` and
    /// `Option<T>`), and its body replaced by each value its return type suggests.
    fn function_sites(
        &mut self,
        attrs: &[syn::Attribute],
        sig: &syn::Signature,
        body: &syn::Block,
    ) {
        if body.stmts.is_empty() {
            return;
        }
        let braces = body.brace_token.span;
        let (Some((_, mut start)), Some((end, _)), Some((sig_start, sig_end))) = (
            self.range(braces.open()),
            self.range(braces.close()),
            self.range(sig.span()),
        ) else {
            return;
        };
        // Inner attributes (`#![allow(...)]`) must stay at the very start of the body block,
        // before any statement inserted by a Body or Return probe.
        for attr in attrs {
            if matches!(attr.style, syn::AttrStyle::Inner(_))
                && let Some((_, attr_end)) = self.range(attr.span())
            {
                start = start.max(attr_end);
            }
        }
        if !in_order(start, end) {
            return;
        }
        let at = sig.span().start();
        let text = self.source[sig_start..sig_end].to_string();
        let (sole_expr, sole_plain) = match &body.stmts[..] {
            [syn::Stmt::Expr(expr, None)] => {
                let range = self.range(expr.span());
                (range, range.filter(|_| does_nothing_else(expr)))
            }
            _ => (None, None),
        };
        let push = |finder: &mut Self, kind: SiteKind| {
            finder.add(Site {
                id: 0,
                kind,
                line: at.line,
                column: at.column + 1,
                start,
                end,
                text: text.clone(),
            });
        };
        let name = sig.ident.to_string();
        if let Some(option) = self.result_kind(&sig.output)
            && !may_bypass_result_probe(body)
        {
            let kind = SiteKind::Return {
                name: name.clone(),
                option,
                sole_expr,
            };
            push(self, kind);
        }
        // Deleting the only statement of a function that returns nothing is the same
        // mutant as an empty body.
        let one_deletable_statement = matches!(sig.output, syn::ReturnType::Default)
            && matches!(&body.stmts[..], [syn::Stmt::Expr(expr, Some(_))] if deletable(expr));
        if sig.unsafety.is_some() {
            return;
        }
        if one_deletable_statement {
            self.found.single_statement_bodies.push(name);
            return;
        }
        let written = sole_expr.map(|(from, to)| tokens_of(&self.source[from..to]));
        let is_written = |value: &str| {
            written
                .as_deref()
                .is_some_and(|text| text == tokens_of(value))
        };
        let probe_values = body_values_with_errors(&sig.output, self.error_values, false);
        // tmt:ignore performance only: the set answers what `probe_values.contains` answers
        let probe_index = (probe_values.len() > 32).then(|| {
            probe_values
                .iter()
                .map(String::as_str)
                .collect::<HashSet<_>>()
        });
        for value in body_values_with_errors(&sig.output, self.error_values, true) {
            let is_probe = probe_index.as_ref().map_or_else(
                || probe_values.contains(&value),
                |index| index.contains(value.as_str()),
            );
            if !is_probe && !is_written(&value) {
                self.add_rebuild(Site {
                    id: NO_ID,
                    kind: SiteKind::Body {
                        name: name.clone(),
                        value,
                        sole_expr: sole_plain,
                    },
                    line: at.line,
                    column: at.column + 1,
                    start,
                    end,
                    text: text.clone(),
                });
            }
        }
        for value in probe_values {
            if !is_written(&value) {
                push(
                    self,
                    SiteKind::Body {
                        name: name.clone(),
                        value,
                        sole_expr: sole_plain,
                    },
                );
            }
        }
    }

    /// `Some(None)` for a function that returns `bool`, `Some(Some(type))` for one that
    /// returns `Option<T>` written in a way a `let` can repeat.
    fn result_kind(&self, output: &syn::ReturnType) -> Option<Option<String>> {
        let syn::ReturnType::Type(_, ty) = output else {
            return None;
        };
        match &**ty {
            // `<T>::bool` and `<T as Tr>::bool` are not a single identifier either.
            syn::Type::Path(p) if p.path.is_ident("bool") => Some(None),
            syn::Type::Path(p)
                if p.qself.is_none()
                    && p.path.segments.last().is_some_and(|s| {
                        s.ident == "Option"
                            && matches!(s.arguments, syn::PathArguments::AngleBracketed(_))
                    }) =>
            {
                match self
                    .range(ty.span())
                    .map(|(from, to)| &self.source[from..to])
                {
                    // `impl Trait` cannot be written in a `let`.
                    Some(written) if !written.contains("impl ") => Some(Some(written.to_string())),
                    _ => None,
                }
            }
            _ => None,
        }
    }
    fn range(&self, span: Span) -> Option<(usize, usize)> {
        let range = span.byte_range();
        // Spans produced by macro hygiene tricks or empty token streams carry no location.
        // tmt:ignore defensive: every span of a file parsed from `source` lies inside it
        let located = !range.is_empty() && self.source.get(range.clone()).is_some();
        located.then_some((range.start, range.end))
    }

    fn push(&mut self, kind: SiteKind, start: usize, end: usize, span: Span) {
        let at = span.start();
        self.add(Site {
            id: 0,
            kind,
            line: at.line,
            column: at.column + 1,
            start,
            end,
            text: self.source[start..end].to_string(),
        });
    }

    /// The value of a `const` or `static`. The compiler computes it: like the code of a
    /// `const fn`, it is mutated by building.
    fn constant(&mut self, attrs: &[syn::Attribute], value: &syn::Expr) {
        if !is_test_only(attrs) && !is_skipped(attrs) {
            self.const_depth += 1;
            self.visit_expr(value);
            self.const_depth -= 1;
        }
    }

    fn typed_constant(
        &mut self,
        attrs: &[syn::Attribute],
        ty: &syn::Type,
        value: Option<&syn::Expr>,
    ) {
        if !is_test_only(attrs) && !is_skipped(attrs) {
            self.visit_type(ty);
            if let Some(value) = value {
                self.constant(&[], value);
            }
        }
    }

    /// Registers a site: numbered, or among those that need a build of their own.
    fn add_rebuild(&mut self, mut site: Site) {
        site.id = NO_ID;
        self.found.rebuild_sites.push(site);
    }

    fn add(&mut self, mut site: Site) {
        if self.const_depth > 0 {
            self.add_rebuild(site);
        } else {
            site.id = self.next_id;
            self.found.sites.push(site);
            self.next_id += 1;
        }
    }
}

fn cmp_op(op: &syn::BinOp) -> Option<CmpOp> {
    Some(match op {
        syn::BinOp::Lt(_) => CmpOp::Lt,
        syn::BinOp::Le(_) => CmpOp::Le,
        syn::BinOp::Gt(_) => CmpOp::Gt,
        syn::BinOp::Ge(_) => CmpOp::Ge,
        syn::BinOp::Eq(_) => CmpOp::Eq,
        syn::BinOp::Ne(_) => CmpOp::Ne,
        _ => return None,
    })
}

/// The arithmetic or bit operator, and whether it is the compound assignment form.
fn arith_op(op: &syn::BinOp) -> Option<(ArithOp, bool)> {
    use syn::BinOp as B;
    Some(match op {
        B::Add(_) => (ArithOp::Add, false),
        B::Sub(_) => (ArithOp::Sub, false),
        B::Mul(_) => (ArithOp::Mul, false),
        B::Div(_) => (ArithOp::Div, false),
        B::Rem(_) => (ArithOp::Rem, false),
        B::BitAnd(_) => (ArithOp::BitAnd, false),
        B::BitOr(_) => (ArithOp::BitOr, false),
        B::BitXor(_) => (ArithOp::BitXor, false),
        B::Shl(_) => (ArithOp::Shl, false),
        B::Shr(_) => (ArithOp::Shr, false),
        B::AddAssign(_) => (ArithOp::Add, true),
        B::SubAssign(_) => (ArithOp::Sub, true),
        B::MulAssign(_) => (ArithOp::Mul, true),
        B::DivAssign(_) => (ArithOp::Div, true),
        B::RemAssign(_) => (ArithOp::Rem, true),
        B::BitAndAssign(_) => (ArithOp::BitAnd, true),
        B::BitOrAssign(_) => (ArithOp::BitOr, true),
        B::BitXorAssign(_) => (ArithOp::BitXor, true),
        B::ShlAssign(_) => (ArithOp::Shl, true),
        B::ShrAssign(_) => (ArithOp::Shr, true),
        _ => return None,
    })
}

/// A place whose evaluation has no effect and cannot fail: a variable, a field of one, or
/// a dereference of one.
///
/// For numbers `place op= value` evaluates `value` first, for other types `place` first.
/// The probe always takes the place first, which is the same thing only when evaluating
/// the place cannot be observed. Indexing can panic and calls can do anything, so
/// `v[i] += 1` and `f().x += 1` are not instrumented as operators (they can still be
/// deleted as statements).
fn is_plain_place(expr: &syn::Expr) -> bool {
    match expr {
        syn::Expr::Path(e) => e.attrs.is_empty(),
        syn::Expr::Field(e) => is_plain_place(&e.base),
        syn::Expr::Unary(syn::ExprUnary {
            op: syn::UnOp::Deref(_),
            expr,
            ..
        }) => is_plain_place(expr),
        syn::Expr::Paren(e) => is_plain_place(&e.expr),
        syn::Expr::Group(e) => is_plain_place(&e.expr),
        _ => false,
    }
}

/// The value of an expression that is the word `true` or `false`, in parentheses or not.
fn written_bool(expr: &syn::Expr) -> Option<bool> {
    match expr {
        syn::Expr::Paren(e) => written_bool(&e.expr),
        syn::Expr::Group(e) => written_bool(&e.expr),
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Bool(value),
            ..
        }) => Some(value.value),
        _ => None,
    }
}

fn is_literal(expr: &syn::Expr) -> bool {
    match expr {
        syn::Expr::Paren(e) => is_literal(&e.expr),
        syn::Expr::Group(e) => is_literal(&e.expr),
        syn::Expr::Lit(_) => true,
        syn::Expr::Unary(syn::ExprUnary {
            op: syn::UnOp::Neg(_),
            expr,
            ..
        }) => is_literal(expr),
        _ => false,
    }
}

/// The spans of the parts of one parsed expression come in source order. The edits are
/// built on that, so it is checked rather than assumed; no source makes it false.
// tmt:ignore defensive: never false, so no test can make it return false
fn in_order(earlier: usize, later: usize) -> bool {
    earlier <= later // tmt:ignore defensive: as above
}

/// `if let P = e && cond` is a let chain, not a boolean expression: it cannot be wrapped.
fn in_let_chain(expr: &syn::Expr) -> bool {
    match expr {
        syn::Expr::Let(_) => true,
        syn::Expr::Binary(syn::ExprBinary {
            op: syn::BinOp::And(_),
            left,
            right,
            ..
        }) => in_let_chain(left) || matches!(**right, syn::Expr::Let(_)),
        _ => false,
    }
}

/// Macros whose arguments are ordinary expressions separated by commas. Macros that turn
/// their argument into text (`assert!`, `debug_assert!`, `dbg!`, `stringify!`) are left
/// alone: rewriting the argument would change the message a test may expect.
const EXPR_MACROS: [&str; 18] = [
    "assert_eq",
    "assert_ne",
    "debug_assert_eq",
    "debug_assert_ne",
    "vec",
    "format",
    "print",
    "println",
    "eprint",
    "eprintln",
    "write",
    "writeln",
    "panic",
    "unreachable",
    "todo",
    "unimplemented",
    "format_args",
    "matches",
];

fn is_assign_op(op: &syn::BinOp) -> bool {
    use syn::BinOp::*;
    matches!(
        op,
        AddAssign(_)
            | SubAssign(_)
            | MulAssign(_)
            | DivAssign(_)
            | RemAssign(_)
            | BitXorAssign(_)
            | BitAndAssign(_)
            | BitOrAssign(_)
            | ShlAssign(_)
            | ShrAssign(_)
    )
}

fn int_literal(expr: &syn::Expr) -> Option<i128> {
    match expr {
        syn::Expr::Paren(e) => int_literal(&e.expr),
        syn::Expr::Group(e) => int_literal(&e.expr),
        syn::Expr::Lit(syn::ExprLit {
            lit: syn::Lit::Int(lit),
            ..
        }) => lit.base10_parse().ok(),
        syn::Expr::Unary(syn::ExprUnary {
            op: syn::UnOp::Neg(_),
            expr,
            ..
        }) => int_literal(expr).and_then(i128::checked_neg),
        _ => None,
    }
}

/// Statements whose removal is a plausible bug: a forgotten call, check or update.
fn deletable(expr: &syn::Expr) -> bool {
    match expr {
        syn::Expr::Call(e) => e.attrs.is_empty(),
        syn::Expr::MethodCall(e) => e.attrs.is_empty(),
        syn::Expr::Assign(e) => e.attrs.is_empty(),
        // An attribute in front of `a += 1` belongs to `a`, never to the operation.
        syn::Expr::Binary(e) => is_assign_op(&e.op),
        syn::Expr::Try(e) => e.attrs.is_empty() && deletable_call(&e.expr),
        syn::Expr::Await(e) => e.attrs.is_empty() && deletable_call(&e.base),
        _ => false,
    }
}

/// Whether the receiver of a `last()` is written as an iterator: a call of one of the
/// methods of the standard library that make or adapt one. `Iterator::last` takes the
/// iterator itself, which a probe that only borrows its receiver could not give it.
fn yields_iterator(receiver: &syn::Expr) -> bool {
    const MAKERS: &[&str] = &[
        "iter",
        "iter_mut",
        "into_iter",
        "chars",
        "char_indices",
        "bytes",
        "lines",
        "keys",
        "values",
        "values_mut",
        "into_keys",
        "into_values",
        "drain",
        "windows",
        "chunks",
        "chunks_exact",
        "rchunks",
        "split",
        "rsplit",
        "splitn",
        "rsplitn",
        "split_terminator",
        "split_whitespace",
        "split_ascii_whitespace",
        "split_inclusive",
        "matches",
        "match_indices",
        "components",
        "ancestors",
        "range",
        "map",
        "filter",
        "filter_map",
        "flat_map",
        "flatten",
        "rev",
        "skip",
        "skip_while",
        "take",
        "take_while",
        "step_by",
        "zip",
        "chain",
        "enumerate",
        "peekable",
        "cloned",
        "copied",
        "inspect",
        "scan",
        "fuse",
        "by_ref",
        "map_while",
        "cycle",
        "args",
        "encode_utf16",
    ];
    match receiver {
        syn::Expr::Paren(e) => yields_iterator(&e.expr),
        syn::Expr::Group(e) => yields_iterator(&e.expr),
        syn::Expr::MethodCall(e) => MAKERS.contains(&e.method.to_string().as_str()),
        syn::Expr::Range(_) => true,
        _ => false,
    }
}

/// The name of a receiver that is a variable, with or without parentheses around it.
/// A name that starts with a capital is taken for a constant, which is a new value each
/// time it is named.
fn variable_name(receiver: &str) -> Option<String> {
    let mut expr = &syn::parse_str::<syn::Expr>(receiver).ok()?;
    while let syn::Expr::Paren(inner) = expr {
        if !inner.attrs.is_empty() {
            return None;
        }
        expr = &inner.expr;
    }
    let syn::Expr::Path(path) = expr else {
        return None;
    };
    if path.qself.is_some() || !path.attrs.is_empty() {
        return None;
    }
    let name = path.path.get_ident()?.to_string();
    let first = name.trim_start_matches("r#").chars().next()?;
    (!first.is_uppercase()).then_some(name)
}

fn deletable_call(expr: &syn::Expr) -> bool {
    match expr {
        syn::Expr::Call(_) | syn::Expr::MethodCall(_) => true,
        syn::Expr::Await(e) => deletable_call(&e.base),
        _ => false,
    }
}

fn has_ident(tokens: TokenStream, name: &str) -> bool {
    tokens.into_iter().any(|tree| match tree {
        TokenTree::Ident(ident) => ident == name,
        TokenTree::Group(group) => has_ident(group.stream(), name),
        _ => false,
    })
}

/// The number of a site that has none.
pub const NO_ID: u32 = u32::MAX;

/// A function or constant of a file with the stretch of source it takes up, named by the
/// modules, impls and functions around it: `Store::commit`, `<Display for Id>::fmt`.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct NamedItem {
    pub start: usize,
    pub end: usize,
    pub name: String,
}

/// The functions and constants of a source file. A finding is told from another one on
/// the same code by the item it is in, which stays the same when lines move.
pub fn named_items(source: &str) -> Vec<NamedItem> {
    struct Namer {
        path: Vec<String>,
        items: Vec<NamedItem>,
    }
    impl Namer {
        fn inside(&mut self, name: String, span: Option<Span>, visit: impl FnOnce(&mut Self)) {
            self.path.push(name);
            if let Some(span) = span {
                let range = span.byte_range();
                self.items.push(NamedItem {
                    start: range.start,
                    end: range.end,
                    name: self.path.join("::"),
                });
            }
            visit(self);
            self.path.pop();
        }
    }
    impl<'ast> Visit<'ast> for Namer {
        fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
            self.inside(node.ident.to_string(), None, |namer| {
                visit::visit_item_mod(namer, node)
            });
        }
        fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
            self.inside(node.ident.to_string(), None, |namer| {
                visit::visit_item_trait(namer, node)
            });
        }
        fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
            let of = squeezed(&node.self_ty.to_token_stream().to_string());
            let name = match &node.trait_ {
                Some((_, path, _)) => {
                    format!(
                        "<{} for {of}>",
                        squeezed(&path.to_token_stream().to_string())
                    )
                }
                None => of,
            };
            self.inside(name, None, |namer| visit::visit_item_impl(namer, node));
        }
        fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
            self.inside(node.sig.ident.to_string(), Some(node.span()), |namer| {
                visit::visit_item_fn(namer, node)
            });
        }
        fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
            self.inside(node.sig.ident.to_string(), Some(node.span()), |namer| {
                visit::visit_impl_item_fn(namer, node)
            });
        }
        fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
            self.inside(node.sig.ident.to_string(), Some(node.span()), |namer| {
                visit::visit_trait_item_fn(namer, node)
            });
        }
        fn visit_item_const(&mut self, node: &'ast syn::ItemConst) {
            self.inside(node.ident.to_string(), Some(node.span()), |_| {});
        }
        fn visit_item_static(&mut self, node: &'ast syn::ItemStatic) {
            self.inside(node.ident.to_string(), Some(node.span()), |_| {});
        }
        fn visit_impl_item_const(&mut self, node: &'ast syn::ImplItemConst) {
            self.inside(node.ident.to_string(), Some(node.span()), |_| {});
        }
    }
    let Ok(file) = syn::parse_file(source) else {
        return vec![];
    };
    let mut namer = Namer {
        path: vec![],
        items: vec![],
    };
    namer.visit_file(&file);
    namer.items
}

/// The name of the innermost item that `offset` lies in; empty outside of all of them.
pub fn enclosing_item(items: &[NamedItem], offset: usize) -> &str {
    items
        .iter()
        .filter(|item| item.start <= offset && offset < item.end)
        .min_by_key(|item| item.end - item.start)
        .map_or("", |item| item.name.as_str())
}

/// Where the 1-based `line` and `column` (in characters) of a source text are, in bytes.
pub fn offset_of(source: &str, line: usize, column: usize) -> Option<usize> {
    let start: usize = source
        .split_inclusive('\n')
        .take(line.checked_sub(1)?)
        .map(str::len)
        .sum();
    let rest = source.get(start..)?;
    let within = rest
        .char_indices()
        .nth(column.checked_sub(1)?)
        .map_or(rest.len(), |(at, _)| at);
    Some(start + within)
}

/// The innermost item's name for each interval between item boundaries.
pub(crate) struct ItemIndex<'a>(Vec<(usize, &'a str)>);

impl<'a> ItemIndex<'a> {
    pub(crate) fn new(items: &'a [NamedItem]) -> Self {
        use std::{cmp::Reverse, collections::BinaryHeap};

        let mut starts: Vec<_> = items.iter().enumerate().collect();
        starts.sort_unstable_by_key(|&(_, item)| item.start);
        let mut boundaries: Vec<_> = items
            .iter()
            .flat_map(|item| [item.start, item.end])
            .collect();
        boundaries.sort_unstable();

        let mut starts = starts.into_iter().peekable();
        let mut active = BinaryHeap::new();
        let mut intervals = Vec::new();
        for boundary in boundaries {
            while let Some(&(index, item)) =
                starts.peek().filter(|(_, item)| item.start <= boundary)
            {
                starts.next();
                // Span length chooses the innermost item. Equal spans keep visitor order.
                active.push(Reverse((item.end - item.start, index)));
            }
            while active
                .peek()
                .is_some_and(|&Reverse((_, index))| items[index].end <= boundary)
            {
                active.pop();
            }
            let name = active
                .peek()
                .map_or("", |&Reverse((_, index))| items[index].name.as_str());
            if intervals
                .last()
                .is_none_or(|&(_, previous)| previous != name)
            {
                intervals.push((boundary, name));
            }
        }
        Self(intervals)
    }

    pub(crate) fn at(&self, offset: usize) -> &'a str {
        let end = self.0.partition_point(|&(start, _)| start <= offset);
        self.0[..end].last().map_or("", |&(_, name)| name)
    }
}

#[cfg(test)]
#[test]
fn indexed_item_boundaries_agree_with_the_previous_search() {
    let items = [
        (6, 10, "first tie"),
        (0, 16, "outer"),
        (2, 6, "before tie"),
        (6, 10, "second tie"),
        (4, 13, "overlap"),
        (7, 9, "inner"),
        (7, 7, "empty"),
        (17, 18, "after outer"),
    ]
    .map(|(start, end, name)| NamedItem {
        start,
        end,
        name: name.to_string(),
    });
    let indexed = ItemIndex::new(&items);
    for offset in 0..=20 {
        assert_eq!(
            indexed.at(offset),
            enclosing_item(&items, offset),
            "{offset}"
        );
        assert_eq!(ItemIndex::new(&[]).at(offset), "");
    }
}

/// A mutant as a change of the source text: `replacement` in place of `start..end`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SourceMutant {
    pub start: usize,
    pub end: usize,
    pub replacement: String,
    /// What is replaced and by what, in the words of cargo-mutants: (`<`, `==`),
    /// (`-`, ``) for a deleted minus, (`guard`, `true`), (`body`, `0`).
    pub what: String,
    pub into: String,
}

/// The mutants of a site as changes of the source, for the sites that are mutated by
/// building them. The replacements of an operator are those of cargo-mutants. Sites that
/// only exist as probes (a result replaced after the body ran, the other of `min` /
/// `max`) have none.
/// A strict/non-strict boundary replacement for runs without operand observations.
pub(crate) fn source_boundary_mutant(site: &Site) -> Option<SourceMutant> {
    let SiteKind::Cmp { op, gap, .. } = &site.kind else {
        return None;
    };
    let replacement = match op {
        CmpOp::Le => "<",
        CmpOp::Ge => ">",
        _ => return None,
    };
    Some(SourceMutant {
        start: gap.0,
        end: gap.1,
        replacement: format!(" {replacement} "),
        what: op.symbol().to_string(),
        into: replacement.to_string(),
    })
}

/// Gives each mutant the line ends of what it replaces: the lines after it are the lines
/// they were, so `line!()` and the place of a panic say the same in the mutant.
pub fn keep_line_ends(source: &str, mutants: &mut [SourceMutant]) {
    for mutant in mutants {
        let Some(replaced) = source.get(mutant.start..mutant.end) else {
            continue;
        };
        let lines = replaced.matches('\n').count();
        let kept = mutant.replacement.matches('\n').count();
        mutant
            .replacement
            .push_str(&"\n".repeat(lines.saturating_sub(kept)));
    }
}

pub fn source_mutants(site: &Site) -> Vec<SourceMutant> {
    let change =
        |start: usize, end: usize, replacement: String, what: &str, into: &str| SourceMutant {
            start,
            end,
            replacement,
            what: what.to_string(),
            into: into.to_string(),
        };
    // A replacement that binds otherwise than the original keeps the operands it had:
    // `2 * 3` in `8 - 2 * 3` becomes `(2 + 3)`, and `8 - 2 * 3` becomes `(8) / (2 * 3)`.
    let operator = |gap: &(usize, usize), from: String, to: String, binds: std::cmp::Ordering| {
        // The operands as they are written, on either side of the operator.
        let operands = gap
            .0
            .checked_sub(site.start)
            .zip(gap.1.checked_sub(site.start))
            .and_then(|(left, right)| {
                Some((
                    site.text.get(..left)?.trim_end(),
                    site.text.get(right..)?.trim_start(),
                    // The line ends between the operands stay between them: each
                    // operand is on the lines it was on.
                    "\n".repeat(site.text.get(left..right)?.matches('\n').count()),
                ))
            });
        match (binds, operands) {
            (std::cmp::Ordering::Less, Some((left, right, lines))) => change(
                site.start,
                site.end,
                format!("({left} {to} {lines}{right})"),
                &from,
                &to,
            ),
            (std::cmp::Ordering::Greater, Some((left, right, lines))) => change(
                site.start,
                site.end,
                format!("({left}) {to} {lines}({right})"),
                &from,
                &to,
            ),
            _ => change(gap.0, gap.1, format!(" {to} "), &from, &to),
        }
    };
    let same = std::cmp::Ordering::Equal;
    match &site.kind {
        SiteKind::Cmp { op, gap, .. } => {
            let others: &[CmpOp] = match op {
                CmpOp::Lt => &[CmpOp::Eq, CmpOp::Gt, CmpOp::Le],
                CmpOp::Gt => &[CmpOp::Eq, CmpOp::Lt, CmpOp::Ge],
                CmpOp::Le => &[CmpOp::Gt],
                CmpOp::Ge => &[CmpOp::Lt],
                CmpOp::Eq => &[CmpOp::Ne],
                CmpOp::Ne => &[CmpOp::Eq],
            };
            others
                .iter()
                .map(|other| operator(gap, op.symbol().into(), other.symbol().into(), same))
                .collect()
        }
        SiteKind::Logic { and, gap, .. } => {
            // `&&` binds tighter than `||`.
            let (from, to, binds) = if *and {
                ("&&", "||", std::cmp::Ordering::Less)
            } else {
                ("||", "&&", std::cmp::Ordering::Greater)
            };
            vec![operator(gap, from.into(), to.into(), binds)]
        }
        SiteKind::Arith {
            op, assign, gap, ..
        } => {
            let eq = if *assign { "=" } else { "" };
            [Some(op.swapped()), op.second(*assign)]
                .into_iter()
                .flatten()
                .map(|other| {
                    operator(
                        gap,
                        format!("{}{eq}", op.symbol()),
                        format!("{}{eq}", other.symbol()),
                        // An assignment has nothing around it and its operands to itself.
                        if *assign {
                            same
                        } else {
                            other.binding().cmp(&op.binding())
                        },
                    )
                })
                .collect()
        }
        SiteKind::Not {
            bang, needs_space, ..
        } => vec![change(
            bang.0,
            bang.1,
            if *needs_space { " " } else { "" }.to_string(),
            "!",
            "",
        )],
        SiteKind::Neg {
            minus, needs_space, ..
        } => vec![change(
            minus.0,
            minus.1,
            if *needs_space { " " } else { "" }.to_string(),
            "-",
            "",
        )],
        SiteKind::Body { value, .. } => {
            vec![change(
                site.start,
                site.end,
                format!(" {value} "),
                "body",
                value,
            )]
        }
        SiteKind::Arm {
            pattern,
            guard,
            removal,
        } => vec![match guard {
            ArmGuard::None(_) => change(removal.0, removal.1, String::new(), "arm", pattern),
            ArmGuard::Existing(from, to) => change(*from, *to, "false".into(), "arm", pattern),
        }],
        SiteKind::Guard { can_be_false } => [true, false]
            .into_iter()
            .filter(|value| *value || *can_be_false)
            // Not the value the guard is written as.
            .filter(|value| {
                syn::parse_str::<syn::Expr>(&site.text)
                    .ok()
                    .and_then(|guard| written_bool(&guard))
                    != Some(*value)
            })
            .map(|value| {
                change(
                    site.start,
                    site.end,
                    value.to_string(),
                    "guard",
                    &value.to_string(),
                )
            })
            .collect(),
        SiteKind::Field { name, removal, .. } => {
            vec![change(removal.0, removal.1, String::new(), "field", name)]
        }
        SiteKind::Stmt => vec![change(site.start, site.end, String::new(), "statement", "")],
        // The end taken in or left out: the two dots and the `=` are all that changes.
        SiteKind::Range { inclusive, gap, .. } => {
            let (from, to) = if *inclusive {
                ("..=", "..")
            } else {
                ("..", "..=")
            };
            vec![change(gap.0, gap.1, to.to_string(), from, to)]
        }
        // The other method's name in place of the first: whatever is written around
        // it (a line that ends, a comment) stays.
        SiteKind::SliceEnd { first, method, .. } => {
            let (from, to) = if *first {
                ("first", "last")
            } else {
                ("last", "first")
            };
            vec![change(method.0, method.1, to.to_string(), from, to)]
        }
        SiteKind::Return { .. } | SiteKind::MinMax { .. } => vec![],
    }
}

/// Replace only the operator tokens, preserving their surrounding source.
pub(crate) fn literal_operator_mutants(source: &str, site: &Site, mutants: &mut [SourceMutant]) {
    let gap = match &site.kind {
        SiteKind::Cmp { gap, .. } | SiteKind::Logic { gap, .. } | SiteKind::Arith { gap, .. } => {
            gap
        }
        _ => return,
    };
    if let Ok(operator) = syn::parse_str::<syn::BinOp>(&source[gap.0..gap.1]) {
        let span = operator.span().byte_range();
        for mutant in mutants {
            mutant.start = gap.0 + span.start;
            mutant.end = gap.0 + span.end;
            mutant.replacement.clone_from(&mutant.into);
        }
    }
}

/// The iterators to return in place of the body of a function that returns
/// `impl Iterator<Item = T>`: an empty one, and one of each value of `T`.
pub fn iterator_values(output: &syn::ReturnType) -> Vec<String> {
    let syn::ReturnType::Type(_, ty) = output else {
        return vec![];
    };
    let syn::Type::ImplTrait(opaque) = &**ty else {
        return vec![];
    };
    iterator_type_values(opaque, &[], false)
}

fn iterator_type_values(
    opaque: &syn::TypeImplTrait,
    error_values: &[&str],
    extended: bool,
) -> Vec<String> {
    let item = opaque.bounds.iter().find_map(|bound| {
        let syn::TypeParamBound::Trait(bound) = bound else {
            return None;
        };
        let last = bound.path.segments.last()?;
        let syn::PathArguments::AngleBracketed(list) = &last.arguments else {
            return None;
        };
        (last.ident == "Iterator").then_some(())?;
        list.args.iter().find_map(|arg| match arg {
            syn::GenericArgument::AssocType(item) if item.ident == "Item" => Some(&item.ty),
            _ => None,
        })
    });
    let Some(item) = item else {
        return vec![];
    };
    if !extended && has_ident(item.to_token_stream(), "impl") {
        return vec!["::core::iter::empty()".to_string()];
    }
    let mut values = vec!["::core::iter::empty()".to_string()];
    values.extend(
        values_of(item, error_values, extended)
            .iter()
            .map(|value| format!("::core::iter::once({value})")),
    );
    values
}

/// Text without its white space.
fn squeezed(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// The tokens of an expression, to compare two ways of writing it: the white space
/// between them does not count, the one inside a literal does (`" "` is not `""`).
fn tokens_of(text: &str) -> String {
    match text.parse::<TokenStream>() {
        Ok(tokens) => tokens.to_string(),
        Err(_) => text.to_string(),
    }
}

/// The result probe runs after the original body has evaluated. Early exits and
/// unexpanded macros in that body's control-flow scope could bypass it.
fn may_bypass_result_probe(body: &syn::Block) -> bool {
    struct EarlyExit(bool);

    impl<'ast> Visit<'ast> for EarlyExit {
        fn visit_expr(&mut self, expr: &'ast syn::Expr) {
            if self.0 {
                return;
            }
            match expr {
                syn::Expr::Return(_) | syn::Expr::Try(_) | syn::Expr::Verbatim(_) => self.0 = true,
                syn::Expr::Async(_) | syn::Expr::Closure(_) | syn::Expr::Const(_) => {}
                _ => visit::visit_expr(self, expr),
            }
        }

        fn visit_macro(&mut self, _: &'ast syn::Macro) {
            self.0 = true;
        }

        fn visit_item(&mut self, _: &'ast syn::Item) {}
        fn visit_pat(&mut self, _: &'ast syn::Pat) {}
        fn visit_type(&mut self, _: &'ast syn::Type) {}
    }

    let mut exits = EarlyExit(false);
    exits.visit_block(body);
    exits.0
}

/// Whether evaluating the expression does nothing but give its value, as far as the
/// source says: operators on a user's type can do anything, which is not assumed.
fn does_nothing_else(expr: &syn::Expr) -> bool {
    struct Effects(bool);
    impl<'ast> Visit<'ast> for Effects {
        fn visit_expr(&mut self, expr: &'ast syn::Expr) {
            match expr {
                syn::Expr::Binary(_)
                | syn::Expr::Unary(_)
                | syn::Expr::Paren(_)
                | syn::Expr::Group(_)
                | syn::Expr::Path(_)
                | syn::Expr::Lit(_)
                | syn::Expr::Field(_)
                | syn::Expr::Reference(_)
                | syn::Expr::Cast(_)
                | syn::Expr::Tuple(_) => syn::visit::visit_expr(self, expr),
                _ => self.0 = true,
            }
        }
        fn visit_bin_op(&mut self, op: &'ast syn::BinOp) {
            use syn::BinOp::*;
            if !matches!(
                op,
                Add(_)
                    | Sub(_)
                    | Mul(_)
                    | Div(_)
                    | Rem(_)
                    | And(_)
                    | Or(_)
                    | BitXor(_)
                    | BitAnd(_)
                    | BitOr(_)
                    | Shl(_)
                    | Shr(_)
                    | Eq(_)
                    | Lt(_)
                    | Le(_)
                    | Ne(_)
                    | Ge(_)
                    | Gt(_)
            ) {
                self.0 = true; // an assignment
            }
        }
    }
    let mut effects = Effects(false);
    effects.visit_expr(expr);
    !effects.0
}

/// The values to replace a function's body with, by its return type. The table is that
/// of cargo-mutants (https://mutants.rs/mutants.html), applied to the type as written.
pub fn body_values(output: &syn::ReturnType) -> Vec<String> {
    body_values_with_errors(output, &[], false)
}

fn body_values_with_errors(
    output: &syn::ReturnType,
    error_values: &[&str],
    extended: bool,
) -> Vec<String> {
    match output {
        syn::ReturnType::Default => vec!["()".to_string()],
        syn::ReturnType::Type(_, ty) => {
            // What an `impl Trait` stands for is the type of the body: no other value
            // can be returned beside it.
            if !extended && has_ident(ty.to_token_stream(), "impl") {
                return vec![];
            }
            values_of(ty, error_values, extended)
        }
    }
}

fn product(parts: &[Vec<String>], format: impl Fn(&[&str]) -> String) -> Vec<String> {
    if parts.iter().any(Vec::is_empty) {
        return vec![];
    }
    let mut positions = vec![0; parts.len()];
    let mut row: Vec<_> = parts.iter().map(|part| part[0].as_str()).collect();
    let mut results = vec![];
    loop {
        results.push(format(&row));
        let Some(next) = (0..parts.len())
            .rev()
            .find(|&i| positions[i] + 1 < parts[i].len())
        else {
            return results;
        };
        positions[next] += 1;
        row[next] = &parts[next][positions[next]];
        for i in next + 1..parts.len() {
            positions[i] = 0;
            row[i] = &parts[i][0];
        }
    }
}

fn values_of(ty: &syn::Type, error_values: &[&str], extended: bool) -> Vec<String> {
    let each = |inner: &syn::Type, wrap: &dyn Fn(&str) -> String| -> Vec<String> {
        values_of(inner, error_values, extended)
            .iter()
            .map(|v| wrap(v))
            .collect()
    };
    match ty {
        syn::Type::Paren(t) => values_of(&t.elem, error_values, extended),
        syn::Type::Group(t) => values_of(&t.elem, error_values, extended),
        syn::Type::Never(_) => vec![],
        syn::Type::ImplTrait(opaque) => iterator_type_values(opaque, error_values, extended),
        // Also the empty one: one row of no values, written `()`.
        syn::Type::Tuple(t) => {
            let parts: Vec<Vec<String>> = t
                .elems
                .iter()
                .map(|ty| values_of(ty, error_values, extended))
                .collect();
            product(&parts, |row| match row {
                [one] => format!("({one},)"),
                _ => format!("({})", row.join(", ")),
            })
        }
        syn::Type::Array(t) => {
            let len = t.len.to_token_stream().to_string();
            each(&t.elem, &|v| format!("[{v}; {len}]"))
        }
        syn::Type::Reference(t) => match &*t.elem {
            syn::Type::Path(p) if p.path.is_ident("str") && t.mutability.is_none() => {
                vec!["\"\"".to_string(), "\"xyzzy\"".to_string()]
            }
            syn::Type::Slice(slice) => {
                let mut values = vec!["Vec::leak(Vec::new())".to_string()];
                values.extend(each(&slice.elem, &|v| format!("Vec::leak(vec![{v}])")));
                values
            }
            inner => each(inner, &|v| format!("Box::leak(Box::new({v}))")),
        },
        syn::Type::Path(p) if p.qself.is_none() => {
            let Some(last) = p.path.segments.last() else {
                return vec!["Default::default()".to_string()];
            };
            // The type as written, without its generic arguments: `std::rc::Rc`.
            let mut bare = p.path.clone();
            if let Some(segment) = bare.segments.last_mut() {
                segment.arguments = syn::PathArguments::None;
            }
            let bare = squeezed(&bare.to_token_stream().to_string());
            let arguments: Vec<&syn::Type> = match &last.arguments {
                syn::PathArguments::AngleBracketed(list) => list
                    .args
                    .iter()
                    .filter_map(|arg| match arg {
                        syn::GenericArgument::Type(ty) => Some(ty),
                        _ => None,
                    })
                    .collect(),
                _ => vec![],
            };
            let name = last.ident.to_string();
            let texts = |list: &[&str]| list.iter().map(|v| v.to_string()).collect::<Vec<_>>();
            let one = |text: String| "1.try_into().unwrap()".replace('1', &text);
            match (name.as_str(), &arguments[..]) {
                ("bool", []) => texts(&["true", "false"]),
                ("i8" | "i16" | "i32" | "i64" | "i128" | "isize", []) => texts(&["0", "1", "-1"]),
                ("u8" | "u16" | "u32" | "u64" | "u128" | "usize", []) => texts(&["0", "1"]),
                ("f32" | "f64", []) => texts(&["0.0", "1.0", "-1.0"]),
                (
                    "NonZeroI8" | "NonZeroI16" | "NonZeroI32" | "NonZeroI64" | "NonZeroI128"
                    | "NonZeroIsize",
                    [],
                ) => vec![one("1".into()), one("(-1)".into())],
                (
                    "NonZeroU8" | "NonZeroU16" | "NonZeroU32" | "NonZeroU64" | "NonZeroU128"
                    | "NonZeroUsize",
                    [],
                ) => vec![one("1".into())],
                ("NonZero", [inner]) => {
                    let unsigned = squeezed(&inner.to_token_stream().to_string()).starts_with('u');
                    let mut values = vec![one("1".into())];
                    if !unsigned {
                        values.push(one("(-1)".into()));
                    }
                    values
                }
                ("String", []) => texts(&["String::new()", "\"xyzzy\".into()"]),
                ("Result", arguments) => {
                    let mut values = match arguments.first() {
                        Some(ok) => each(ok, &|v| format!("Ok({v})")),
                        // `fmt::Result`, or an alias of the crate's own.
                        None => vec!["Ok(Default::default())".to_string()],
                    };
                    values.extend(error_values.iter().map(|expression| {
                        // A trailing line comment must not swallow the closing parenthesis.
                        let end = if expression.rsplit('\n').next().unwrap_or("").contains("//") {
                            "\n"
                        } else {
                            ""
                        };
                        format!("Err({expression}{end})")
                    }));
                    values
                }
                ("Option", [inner]) => {
                    let mut values = vec!["None".to_string()];
                    values.extend(each(inner, &|v| format!("Some({v})")));
                    values
                }
                ("Vec", [inner]) => {
                    let mut values = vec!["vec![]".to_string()];
                    values.extend(each(inner, &|v| format!("vec![{v}]")));
                    values
                }
                ("Box" | "Rc" | "Arc" | "Cell" | "RefCell" | "Mutex", [inner]) => {
                    each(inner, &|v| format!("{bare}::new({v})"))
                }
                ("BinaryHeap" | "BTreeSet" | "HashSet" | "LinkedList" | "VecDeque", [inner]) => {
                    let mut values = vec![format!("{bare}::new()")];
                    values.extend(each(inner, &|v| format!("{bare}::from_iter([{v}])")));
                    values
                }
                ("BTreeMap" | "HashMap", [key, value]) => {
                    let mut values = vec![format!("{bare}::new()")];
                    values.extend(product(
                        &[
                            values_of(key, error_values, extended),
                            values_of(value, error_values, extended),
                        ],
                        |row| format!("{bare}::from_iter([({})])", row.join(", ")),
                    ));
                    values
                }
                ("Cow", [inner]) => {
                    let borrowed: syn::Type = syn::parse_quote!(&#inner);
                    values_of(&borrowed, error_values, extended)
                        .iter()
                        .flat_map(|v| {
                            [
                                format!("{bare}::Borrowed({v})"),
                                format!("{bare}::Owned({v}.to_owned())"),
                            ]
                        })
                        .collect()
                }
                ("HttpResponse", []) => vec![format!("{bare}::Ok().finish()")],
                (_, [inner]) if extended => {
                    let mut values = vec![format!("{bare}::new()")];
                    for value in values_of(inner, error_values, extended) {
                        values.extend([
                            format!("{bare}::from_iter([{value}])"),
                            format!("{bare}::new({value})"),
                            format!("{bare}::from({value})"),
                        ]);
                    }
                    values
                }
                _ => vec!["Default::default()".to_string()],
            }
        }
        _ => vec!["Default::default()".to_string()],
    }
}

/// `#[mutants::skip]`, also inside `cfg_attr`: the mark cargo-mutants uses for code that
/// is not to be mutated.
pub(crate) fn is_skipped(attrs: &[syn::Attribute]) -> bool {
    fn marked(meta: &syn::Meta) -> bool {
        let path = meta.path();
        if path.segments.len() == 2
            && path.segments[0].ident == "mutants"
            && path.segments[1].ident == "skip"
        {
            return true;
        }
        if path.is_ident("cfg_attr")
            && let syn::Meta::List(list) = meta
            && let Ok(parts) = list.parse_args_with(
                syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
            )
        {
            return parts.iter().skip(1).any(marked);
        }
        false
    }

    attrs.iter().any(|attr| marked(&attr.meta))
}

/// Whether the predicate may have the desired value while `test` is false.
pub(crate) fn cfg_possible_without_test(meta: &syn::Meta, desired: bool) -> bool {
    if meta.path().is_ident("test") {
        return !desired;
    }
    let syn::Meta::List(list) = meta else {
        return true;
    };
    let Ok(parts) = list.parse_args_with(
        syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
    ) else {
        return true;
    };
    if list.path.is_ident("not") && parts.len() == 1 {
        return cfg_possible_without_test(&parts[0], !desired);
    }
    if list.path.is_ident("all") || list.path.is_ident("any") {
        return if list.path.is_ident("all") == desired {
            parts
                .iter()
                .all(|part| cfg_possible_without_test(part, desired))
        } else {
            parts
                .iter()
                .any(|part| cfg_possible_without_test(part, desired))
        };
    }
    true
}

/// What a `cfg` predicate is, as far as the features that are on decide it: `None` when
/// it depends on something else (the target, `test`, a `--cfg` of the build).
fn cfg_value(meta: &syn::Meta, features: &BTreeSet<String>) -> Option<bool> {
    match meta {
        syn::Meta::NameValue(pair) if pair.path.is_ident("feature") => match &pair.value {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(name),
                ..
            }) => Some(features.contains(&name.value())),
            _ => None,
        },
        syn::Meta::List(list) => {
            let parts = list
                .parse_args_with(
                    syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                )
                .ok()?;
            let mut values = parts.iter().map(|part| cfg_value(part, features));
            if list.path.is_ident("not") && parts.len() == 1 {
                values.next()?.map(|value| !value)
            } else if list.path.is_ident("all") {
                // One part that is off settles it; otherwise every part must be known.
                values
                    .try_fold(Some(true), |all, value| match (all, value) {
                        (_, Some(false)) => Err(()),
                        (Some(true), Some(true)) => Ok(Some(true)),
                        _ => Ok(None),
                    })
                    .unwrap_or(Some(false))
            } else if list.path.is_ident("any") {
                values
                    .try_fold(Some(false), |any, value| match (any, value) {
                        (_, Some(true)) => Err(()),
                        (Some(false), Some(false)) => Ok(Some(false)),
                        _ => Ok(None),
                    })
                    .unwrap_or(Some(true))
            } else {
                None
            }
        }
        _ => None,
    }
}

fn cfg_is_off(attrs: &[syn::Attribute], features: &BTreeSet<String>) -> bool {
    attrs.iter().any(|attr| {
        attr.path().is_ident("cfg")
            && attr
                .parse_args::<syn::Meta>()
                .is_ok_and(|meta| cfg_value(&meta, features) == Some(false))
    })
}

/// Byte ranges of the `cfg` attributes that are on whatever else holds, given the
/// features that are on: with or without one of them the build is the same.
pub(crate) fn settled_cfg_attributes(
    source: &str,
    file: &syn::File,
    features: &BTreeSet<String>,
) -> Vec<(usize, usize)> {
    struct Settled<'a> {
        source: &'a str,
        features: &'a BTreeSet<String>,
        found: Vec<(usize, usize)>,
    }
    impl<'ast> Visit<'ast> for Settled<'_> {
        fn visit_attribute(&mut self, attr: &'ast syn::Attribute) {
            let range = attr.span().byte_range();
            if attr.path().is_ident("cfg")
                && attr
                    .parse_args::<syn::Meta>()
                    .is_ok_and(|meta| cfg_value(&meta, self.features) == Some(true))
                && self.source.get(range.clone()).is_some()
            {
                self.found.push((range.start, range.end));
            }
        }
    }
    let mut settled = Settled {
        source,
        features,
        found: vec![],
    };
    settled.visit_file(file);
    settled.found
}

/// Byte ranges of the code that the features that are on leave out of the build: what is
/// under a `cfg` that is off whatever the target is. Its mutants would change nothing.
pub(crate) fn inactive_ranges(
    source: &str,
    file: &syn::File,
    features: &BTreeSet<String>,
) -> Vec<(usize, usize)> {
    struct Inactive<'a> {
        source: &'a str,
        features: &'a BTreeSet<String>,
        found: Vec<(usize, usize)>,
        /// The file defines a `vec!` of its own, which may do anything.
        own_vec: bool,
    }
    impl Inactive<'_> {
        /// Whether the node is left out; then its range is noted and it is not entered.
        fn off(&mut self, attrs: &[syn::Attribute], span: Span) -> bool {
            if !cfg_is_off(attrs, self.features) {
                return false;
            }
            let range = span.byte_range();
            if self.source.get(range.clone()).is_some() {
                self.found.push((range.start, range.end));
            }
            true
        }
    }
    macro_rules! nodes {
        ($($visit:ident, $default:path, $node:ty;)*) => {
            impl<'ast> Visit<'ast> for Inactive<'_> {
                $(fn $visit(&mut self, node: &'ast $node) {
                    if !self.off(&node.attrs, node.span()) {
                        $default(self, node);
                    }
                })*
                fn visit_item(&mut self, node: &'ast syn::Item) {
                    if !self.off(item_attrs(node), node.span()) {
                        visit::visit_item(self, node);
                    }
                }
                fn visit_impl_item(&mut self, node: &'ast syn::ImplItem) {
                    let attrs = match node {
                        syn::ImplItem::Const(item) => &item.attrs[..],
                        syn::ImplItem::Fn(item) => &item.attrs,
                        syn::ImplItem::Type(item) => &item.attrs,
                        syn::ImplItem::Macro(item) => &item.attrs,
                        _ => &[],
                    };
                    if !self.off(attrs, node.span()) {
                        visit::visit_impl_item(self, node);
                    }
                }
                fn visit_foreign_item(&mut self, node: &'ast syn::ForeignItem) {
                    let attrs = match node {
                        syn::ForeignItem::Fn(item) => &item.attrs[..],
                        syn::ForeignItem::Static(item) => &item.attrs,
                        syn::ForeignItem::Type(item) => &item.attrs,
                        syn::ForeignItem::Macro(item) => &item.attrs,
                        _ => &[],
                    };
                    if !self.off(attrs, node.span()) {
                        visit::visit_foreign_item(self, node);
                    }
                }
                // The elements of `vec![a, b]` are those of an array: one under a
                // `cfg` that is off is left out of it.
                fn visit_macro(&mut self, node: &'ast syn::Macro) {
                    type Args = syn::punctuated::Punctuated<syn::Expr, syn::Token![,]>;
                    if node.path.segments.last().is_some_and(|name| name.ident == "vec")
                        && !self.own_vec
                        && let Ok(args) = node.parse_body_with(Args::parse_terminated)
                    {
                        for arg in &args {
                            self.visit_expr(arg);
                        }
                    }
                }
                fn visit_trait_item(&mut self, node: &'ast syn::TraitItem) {
                    let attrs = match node {
                        syn::TraitItem::Const(item) => &item.attrs[..],
                        syn::TraitItem::Fn(item) => &item.attrs,
                        syn::TraitItem::Type(item) => &item.attrs,
                        syn::TraitItem::Macro(item) => &item.attrs,
                        _ => &[],
                    };
                    if !self.off(attrs, node.span()) {
                        visit::visit_trait_item(self, node);
                    }
                }
            }
        };
    }
    nodes! {
        visit_local, visit::visit_local, syn::Local;
        visit_arm, visit::visit_arm, syn::Arm;
        visit_field_value, visit::visit_field_value, syn::FieldValue;
        visit_variant, visit::visit_variant, syn::Variant;
        visit_field, visit::visit_field, syn::Field;
        visit_stmt_macro, visit::visit_stmt_macro, syn::StmtMacro;
        visit_expr_block, visit::visit_expr_block, syn::ExprBlock;
        visit_expr_if, visit::visit_expr_if, syn::ExprIf;
        visit_expr_call, visit::visit_expr_call, syn::ExprCall;
        visit_expr_method_call, visit::visit_expr_method_call, syn::ExprMethodCall;
        visit_expr_macro, visit::visit_expr_macro, syn::ExprMacro;
        visit_expr_match, visit::visit_expr_match, syn::ExprMatch;
        visit_expr_assign, visit::visit_expr_assign, syn::ExprAssign;
        visit_expr_return, visit::visit_expr_return, syn::ExprReturn;
        visit_expr_for_loop, visit::visit_expr_for_loop, syn::ExprForLoop;
        visit_expr_while, visit::visit_expr_while, syn::ExprWhile;
        visit_expr_loop, visit::visit_expr_loop, syn::ExprLoop;
        visit_expr_unsafe, visit::visit_expr_unsafe, syn::ExprUnsafe;
        visit_expr_binary, visit::visit_expr_binary, syn::ExprBinary;
        visit_expr_lit, visit::visit_expr_lit, syn::ExprLit;
        visit_expr_path, visit::visit_expr_path, syn::ExprPath;
        visit_expr_array, visit::visit_expr_array, syn::ExprArray;
        visit_expr_tuple, visit::visit_expr_tuple, syn::ExprTuple;
        visit_expr_reference, visit::visit_expr_reference, syn::ExprReference;
        visit_expr_unary, visit::visit_expr_unary, syn::ExprUnary;
        visit_expr_field, visit::visit_expr_field, syn::ExprField;
        visit_expr_index, visit::visit_expr_index, syn::ExprIndex;
        visit_expr_cast, visit::visit_expr_cast, syn::ExprCast;
        visit_expr_closure, visit::visit_expr_closure, syn::ExprClosure;
        visit_expr_struct, visit::visit_expr_struct, syn::ExprStruct;
        visit_expr_repeat, visit::visit_expr_repeat, syn::ExprRepeat;
        visit_expr_range, visit::visit_expr_range, syn::ExprRange;
        visit_expr_paren, visit::visit_expr_paren, syn::ExprParen;
        visit_expr_try, visit::visit_expr_try, syn::ExprTry;
        visit_expr_await, visit::visit_expr_await, syn::ExprAwait;
        visit_expr_break, visit::visit_expr_break, syn::ExprBreak;
        visit_expr_continue, visit::visit_expr_continue, syn::ExprContinue;
        visit_expr_let, visit::visit_expr_let, syn::ExprLet;
        visit_expr_async, visit::visit_expr_async, syn::ExprAsync;
        visit_expr_const, visit::visit_expr_const, syn::ExprConst;
        visit_expr_yield, visit::visit_expr_yield, syn::ExprYield;
        visit_expr_try_block, visit::visit_expr_try_block, syn::ExprTryBlock;
        visit_expr_raw_addr, visit::visit_expr_raw_addr, syn::ExprRawAddr;
    }
    if cfg_is_off(&file.attrs, features) {
        return vec![(0, source.len())];
    }
    let mut inactive = Inactive {
        source,
        features,
        found: vec![],
        // Anywhere in it: in a module, in a function.
        own_vec: source.split("macro_rules").skip(1).any(|after| {
            after
                .trim_start()
                .strip_prefix('!')
                .is_some_and(|name| name.trim_start().starts_with("vec"))
        }),
    };
    inactive.visit_file(file);
    inactive.found
}

/// What an integer expression can be, as far as its last operation with a literal says:
/// a multiple (`x * 8`, `x << 3`), the bits of a mask (`x & 0xF0`), a remainder
/// (`x % 10`). Only what also holds when the arithmetic wraps: a product by a number
/// that is not a power of two can then be anything.
pub(crate) fn reach(expr: &str) -> Option<(crate::plan::Reach, Option<u32>)> {
    use crate::plan::Reach;
    fn literal(expr: &syn::Expr) -> Option<i128> {
        match expr {
            syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Int(integer),
                ..
            }) => integer.base10_parse().ok(),
            syn::Expr::Paren(inner) => literal(&inner.expr),
            _ => None,
        }
    }
    let mut expr = &syn::parse_str::<syn::Expr>(expr).ok()?;
    while let syn::Expr::Paren(inner) = expr {
        expr = &inner.expr;
    }
    let syn::Expr::Binary(binary) = expr else {
        return None;
    };
    let (left, right) = (literal(&binary.left), literal(&binary.right));
    let power_of_two = |k: i128| k >= 2 && k & (k - 1) == 0;
    // With the number of bits of a shift: it is a multiple only when the type is wider.
    let reach = match binary.op {
        syn::BinOp::Mul(_) => left
            .or(right)
            .filter(|k| power_of_two(*k))
            .map(Reach::Multiples),
        syn::BinOp::Shl(_) => {
            let bits = right.filter(|bits| (1..=100).contains(bits))?;
            return Some((Reach::Multiples(1 << bits), Some(bits as u32)));
        }
        syn::BinOp::BitAnd(_) => left.or(right).filter(|mask| *mask >= 0).map(Reach::Bits),
        syn::BinOp::Rem(_) => right.filter(|limit| *limit >= 1).map(Reach::Below),
        _ => None,
    };
    reach.map(|reach| (reach, None))
}

/// Byte ranges of the code that only a test build has: what is under `cfg(test)` or
/// marked as a test.
pub(crate) fn test_only_ranges(source: &str, file: &syn::File) -> Vec<(usize, usize)> {
    if is_test_only(&file.attrs) {
        return vec![(0, source.len())];
    }
    let mut found = vec![];
    for item in &file.items {
        test_only_items(source, item, &mut found);
    }
    found
}

fn test_only_items(source: &str, item: &syn::Item, found: &mut Vec<(usize, usize)>) {
    let mut note = |attrs: &[syn::Attribute], span: Span| {
        let test_only = is_test_only(attrs);
        let range = span.byte_range();
        if test_only && source.get(range.clone()).is_some() {
            found.push((range.start, range.end));
        }
        test_only
    };
    if note(item_attrs(item), item.span()) {
        return;
    }
    match item {
        syn::Item::Mod(module) => {
            for inner in module.content.iter().flat_map(|(_, items)| items) {
                test_only_items(source, inner, found);
            }
        }
        syn::Item::Impl(block) => {
            for inner in &block.items {
                if let syn::ImplItem::Fn(function) = inner {
                    note(&function.attrs, inner.span());
                }
            }
        }
        _ => {}
    }
}

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

fn item_attrs(item: &syn::Item) -> &[syn::Attribute] {
    match item {
        syn::Item::Const(item) => &item.attrs,
        syn::Item::Enum(item) => &item.attrs,
        syn::Item::ExternCrate(item) => &item.attrs,
        syn::Item::Fn(item) => &item.attrs,
        syn::Item::ForeignMod(item) => &item.attrs,
        syn::Item::Impl(item) => &item.attrs,
        syn::Item::Macro(item) => &item.attrs,
        syn::Item::Mod(item) => &item.attrs,
        syn::Item::Static(item) => &item.attrs,
        syn::Item::Struct(item) => &item.attrs,
        syn::Item::Trait(item) => &item.attrs,
        syn::Item::TraitAlias(item) => &item.attrs,
        syn::Item::Type(item) => &item.attrs,
        syn::Item::Union(item) => &item.attrs,
        syn::Item::Use(item) => &item.attrs,
        _ => &[],
    }
}

/// Detect test attributes and cfg predicates that require `test` to be true.
pub(crate) fn is_test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        let path = attr.path();
        if path.segments.last().is_some_and(|s| s.ident == "test") {
            return true;
        }
        path.is_ident("cfg")
            && attr
                .parse_args::<syn::Meta>()
                .is_ok_and(|meta| !cfg_possible_without_test(&meta, true))
    })
}

impl<'ast> Visit<'ast> for Finder<'_> {
    fn visit_attribute(&mut self, _: &'ast syn::Attribute) {}
    fn visit_local(&mut self, node: &'ast syn::Local) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            visit::visit_local(self, node);
        }
    }
    fn visit_pat(&mut self, node: &'ast syn::Pat) {
        if let syn::Pat::Type(binding) = node {
            self.visit_type(&binding.ty);
        }
    }
    fn visit_type_macro(&mut self, _: &'ast syn::TypeMacro) {}
    fn visit_type_array(&mut self, node: &'ast syn::TypeArray) {
        self.visit_type(&node.elem);
        self.constant(&[], &node.len);
    }
    fn visit_const_param(&mut self, node: &'ast syn::ConstParam) {
        self.visit_type(&node.ty);
        if let Some(default) = &node.default {
            self.constant(&node.attrs, default);
        }
    }
    fn visit_generic_argument(&mut self, node: &'ast syn::GenericArgument) {
        match node {
            syn::GenericArgument::Const(value) => self.constant(&[], value),
            _ => visit::visit_generic_argument(self, node),
        }
    }
    fn visit_variant(&mut self, node: &'ast syn::Variant) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            self.visit_fields(&node.fields);
            if let Some((_, value)) = &node.discriminant {
                self.constant(&[], value);
            }
        }
    }
    fn visit_file(&mut self, node: &'ast syn::File) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            visit::visit_file(self, node);
        }
    }
    fn visit_item_enum(&mut self, node: &'ast syn::ItemEnum) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            visit::visit_item_enum(self, node);
        }
    }
    fn visit_item_struct(&mut self, node: &'ast syn::ItemStruct) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            visit::visit_item_struct(self, node);
        }
    }
    fn visit_item_union(&mut self, node: &'ast syn::ItemUnion) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            visit::visit_item_union(self, node);
        }
    }
    fn visit_item_trait(&mut self, node: &'ast syn::ItemTrait) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            visit::visit_item_trait(self, node);
        }
    }
    fn visit_item_type(&mut self, node: &'ast syn::ItemType) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            visit::visit_item_type(self, node);
        }
    }
    fn visit_field(&mut self, node: &'ast syn::Field) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            visit::visit_field(self, node);
        }
    }
    fn visit_impl_item_type(&mut self, node: &'ast syn::ImplItemType) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            visit::visit_impl_item_type(self, node);
        }
    }
    fn visit_trait_item_type(&mut self, node: &'ast syn::TraitItemType) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            visit::visit_trait_item_type(self, node);
        }
    }
    fn visit_item_const(&mut self, node: &'ast syn::ItemConst) {
        self.typed_constant(&node.attrs, &node.ty, Some(&node.expr));
    }
    fn visit_item_static(&mut self, node: &'ast syn::ItemStatic) {
        self.typed_constant(&node.attrs, &node.ty, Some(&node.expr));
    }
    fn visit_impl_item_const(&mut self, node: &'ast syn::ImplItemConst) {
        self.typed_constant(&node.attrs, &node.ty, Some(&node.expr));
    }
    fn visit_trait_item_const(&mut self, node: &'ast syn::TraitItemConst) {
        self.typed_constant(
            &node.attrs,
            &node.ty,
            node.default.as_ref().map(|(_, value)| value),
        );
    }
    fn visit_expr_const(&mut self, node: &'ast syn::ExprConst) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            self.const_depth += 1;
            self.visit_block(&node.block);
            self.const_depth -= 1;
        }
    }

    fn visit_macro(&mut self, node: &'ast syn::Macro) {
        let Some(name) = node.path.segments.last().map(|s| s.ident.to_string()) else {
            return;
        };
        if !EXPR_MACROS.contains(&name.as_str()) || self.local_macros.contains(&name) {
            return;
        }
        if name == "matches" {
            // matches!(expr, pattern): only the scrutinee is an expression.
            let scrutinee = node.parse_body_with(|input: syn::parse::ParseStream| {
                let expr: syn::Expr = input.parse()?;
                input.parse::<proc_macro2::TokenStream>()?;
                Ok(expr)
            });
            if let Ok(expr) = scrutinee {
                self.visit_expr(&expr);
            }
            return;
        }
        type Args = syn::punctuated::Punctuated<syn::Expr, syn::Token![,]>;
        // `vec![x; n]`, named format arguments with odd syntax and the like do not parse
        // as a plain argument list: skipped.
        if let Ok(args) = node.parse_body_with(Args::parse_terminated) {
            for arg in &args {
                // `name = value` in a format string parses as an assignment; nothing
                // treats it as one here, and only its value holds sites.
                self.visit_expr(arg);
            }
        }
    }

    fn visit_expr_repeat(&mut self, node: &'ast syn::ExprRepeat) {
        self.visit_expr(&node.expr);
        self.constant(&node.attrs, &node.len);
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        // The file of a module that is not analysed is left alone as a whole.
        if is_test_only(&node.attrs) || is_skipped(&node.attrs) {
            if node.content.is_none() {
                let path = node.attrs.iter().find_map(|attr| match &attr.meta {
                    syn::Meta::NameValue(nv) if nv.path.is_ident("path") => match &nv.value {
                        syn::Expr::Lit(syn::ExprLit {
                            lit: syn::Lit::Str(s),
                            ..
                        }) => Some(s.value()),
                        _ => None,
                    },
                    _ => None,
                });
                self.found.test_modules.push(TestModule {
                    name: node.ident.to_string(),
                    path,
                });
            }
            return;
        }
        visit::visit_item_mod(self, node);
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            // Nothing in a `const fn` can ask at run time which mutant is on.
            let constant = node.sig.constness.is_some() as usize;
            self.const_depth += constant;
            self.function_sites(&node.attrs, &node.sig, &node.block);
            visit::visit_item_fn(self, node);
            self.const_depth -= constant;
        }
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            // Nothing in a `const fn` can ask at run time which mutant is on.
            let constant = node.sig.constness.is_some() as usize;
            self.const_depth += constant;
            self.function_sites(&node.attrs, &node.sig, &node.block);
            visit::visit_impl_item_fn(self, node);
            self.const_depth -= constant;
        }
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            let constant = node.sig.constness.is_some() as usize;
            self.const_depth += constant;
            if let Some(body) = &node.default {
                self.function_sites(&node.attrs, &node.sig, body);
            }
            visit::visit_trait_item_fn(self, node);
            self.const_depth -= constant;
        }
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            visit::visit_item_impl(self, node);
        }
    }

    fn visit_expr_closure(&mut self, node: &'ast syn::ExprClosure) {
        if node.constness.is_none() {
            visit::visit_expr_closure(self, node);
        }
    }

    fn visit_block(&mut self, node: &'ast syn::Block) {
        for stmt in &node.stmts {
            if let syn::Stmt::Expr(expr, Some(semi)) = stmt
                && deletable(expr)
                && let (Some((start, _)), Some((_, end))) =
                    (self.range(expr.span()), self.range(semi.span()))
                && in_order(start, end)
            {
                self.push(SiteKind::Stmt, start, end, expr.span());
            }
        }
        visit::visit_block(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast syn::ExprMethodCall) {
        let name = node.method.to_string();
        if (name == "max" || name == "min")
            && node.args.len() == 1
            && node.turbofish.is_none()
            && node.attrs.is_empty()
            && let (
                Some((start, receiver_end)),
                Some((argument_start, argument_end)),
                Some((_, end)),
            ) = (
                self.range(node.receiver.span()),
                self.range(node.args[0].span()),
                self.range(node.paren_token.span.close()),
            )
            && in_order(receiver_end, argument_start)
            && in_order(argument_end, end)
        {
            let kind = SiteKind::MinMax {
                max: name == "max",
                receiver: self.source[start..receiver_end].to_string(),
                argument: self.source[argument_start..argument_end].to_string(),
                call: (receiver_end, argument_start),
                close: (argument_end, end),
            };
            self.push(kind, start, end, node.receiver.span());
        }
        if (name == "first" || name == "last")
            && node.args.is_empty()
            && node.turbofish.is_none()
            && node.attrs.is_empty()
            && !(name == "last" && yields_iterator(&node.receiver))
            && let (Some((start, receiver_end)), Some(method), Some((_, end))) = (
                self.range(node.receiver.span()),
                self.range(node.method.span()),
                self.range(node.paren_token.span.close()),
            )
            && in_order(receiver_end, method.0)
            && in_order(method.1, end)
        {
            let kind = SiteKind::SliceEnd {
                first: name == "first",
                receiver: self.source[start..receiver_end].to_string(),
                call: (receiver_end, end),
                method,
            };
            self.push(kind, start, end, node.receiver.span());
        }
        visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_for_loop(&mut self, node: &'ast syn::ExprForLoop) {
        let mut over = &*node.expr;
        while let syn::Expr::Paren(syn::ExprParen { expr, .. })
        | syn::Expr::Group(syn::ExprGroup { expr, .. }) = over
        {
            over = expr;
        }
        if let syn::Expr::Range(syn::ExprRange {
            start: Some(from), ..
        }) = over
            && let Some((start, _)) = self.range(from.span())
        {
            self.for_ranges.insert(start);
        }
        visit::visit_expr_for_loop(self, node);
    }

    fn visit_expr_range(&mut self, node: &'ast syn::ExprRange) {
        // With two literals nothing fixes the integer type before the probe is resolved,
        // and it would be taken for `i32` (`for i in 0..16 { words[i] }`).
        if let (Some(from), Some(to)) = (&node.start, &node.end)
            && node.attrs.is_empty()
            && let (Some((start, lhs_end)), Some((rhs_start, end))) =
                (self.range(from.span()), self.range(to.span()))
            && in_order(lhs_end, rhs_start)
        {
            let kind = SiteKind::Range {
                inclusive: matches!(node.limits, syn::RangeLimits::Closed(_)),
                lhs: self.source[start..lhs_end].to_string(),
                rhs: self.source[rhs_start..end].to_string(),
                gap: (lhs_end, rhs_start),
            };
            // `a..b + 1` in place of `a..=b` gives a `for` the same numbers, and that is
            // all a `for` takes from its range. Anywhere else the two are told apart (by
            // their type, by `end_bound`, by a field): there the mutant is the range as it
            // is written in the source, and it is built like the code of a `const fn`.
            // So is a range of two literals, wherever it is: nothing fixes their type
            // before a probe is resolved, and it would be taken for `i32`.
            let built = !self.for_ranges.contains(&start) || (is_literal(from) && is_literal(to));
            self.const_depth += built as usize;
            self.push(kind, start, end, from.span());
            self.const_depth -= built as usize;
        }
        visit::visit_expr_range(self, node);
    }

    fn visit_expr_match(&mut self, node: &'ast syn::ExprMatch) {
        // A binding or `_` takes everything: with such a last arm the match stays
        // exhaustive when an arm above it stops matching.
        let catches_all = |arm: &syn::Arm| {
            arm.guard.is_none()
                && match &arm.pat {
                    syn::Pat::Wild(_) => true,
                    syn::Pat::Ident(p) => {
                        p.subpat.is_none()
                            && p.ident
                                .to_string()
                                .starts_with(|c: char| c.is_lowercase() || c == '_')
                    }
                    _ => false,
                }
        };
        let mut removable = vec![];
        let has_wildcard = node
            .arms
            .iter()
            .any(|arm| matches!(arm.pat, syn::Pat::Wild(_)));
        let final_catches_all = node.arms.last().is_some_and(catches_all);
        for arm in &node.arms {
            if is_test_only(&arm.attrs) || is_skipped(&arm.attrs) {
                continue;
            }
            // The last arm is not probed: where it takes everything, it is a catch-all.
            let probe = final_catches_all && arm.attrs.is_empty() && !catches_all(arm);
            let literal =
                has_wildcard && !matches!(arm.pat, syn::Pat::Wild(_)) && arm.guard.is_none();
            if !probe && !literal {
                continue;
            }
            let (Some((start, pat_end)), Some((remove_start, remove_end)), Some((arrow, _))) = (
                self.range(arm.pat.span()),
                self.range(arm.span()),
                self.range(arm.fat_arrow_token.span()),
            ) else {
                continue;
            };
            let guard = match &arm.guard {
                None => ArmGuard::None(arrow),
                // The arm with `false` for its guard is the program as it is.
                Some((_, expr)) if written_bool(expr) == Some(false) => continue,
                Some((_, expr)) => match self.range(expr.span()) {
                    Some((from, to)) => ArmGuard::Existing(from, to),
                    None => continue,
                },
            };
            let pattern = match guard {
                ArmGuard::Existing(from, to) => format!(
                    "{} if {}",
                    &self.source[start..pat_end],
                    &self.source[from..to]
                ),
                ArmGuard::None(_) => self.source[start..pat_end].to_string(),
            };
            let end = arm
                .comma
                .as_ref()
                .and_then(|comma| self.range(comma.span()))
                .map_or(remove_end, |(_, end)| end);
            let at = arm.pat.span().start();
            removable.push(start);
            let site = Site {
                id: 0,
                kind: SiteKind::Arm {
                    pattern: pattern.clone(),
                    guard,
                    removal: (remove_start, end),
                },
                line: at.line,
                column: at.column + 1,
                start,
                end,
                text: format!("{pattern} => .."),
            };
            if probe {
                self.add(site);
            } else {
                self.add_rebuild(site);
            }
        }
        // Any guard can be `true` or `false` instead: a guarded arm never counts for
        // the match being exhaustive. A `let` in a guard binds names for the arm.
        for arm in node.arms.iter().filter(|arm| arm.attrs.is_empty()) {
            if let Some((_, guard)) = &arm.guard
                && !matches!(**guard, syn::Expr::Let(_))
                && !in_let_chain(guard)
                && let (Some((start, end)), Some((pattern, _))) =
                    (self.range(guard.span()), self.range(arm.pat.span()))
            {
                let can_be_false = !removable.contains(&pattern);
                let kind = SiteKind::Guard { can_be_false };
                if written_bool(guard) == Some(true) && !can_be_false {
                    // Written `true`, and `false` is the arm removed: no mutant is left.
                } else if written_bool(guard).is_some() {
                    // A guard written `true` or `false` is one of its two mutants: the
                    // other is built, as a probe would report the one that is the
                    // program for never being told from it.
                    let at = guard.span().start();
                    self.add_rebuild(Site {
                        id: 0,
                        kind,
                        line: at.line,
                        column: at.column + 1,
                        start,
                        end,
                        text: self.source[start..end].to_string(),
                    });
                } else {
                    self.push(kind, start, end, guard.span());
                }
            }
        }
        visit::visit_expr_match(self, node);
    }

    fn visit_arm(&mut self, arm: &'ast syn::Arm) {
        if !is_test_only(&arm.attrs) && !is_skipped(&arm.attrs) {
            visit::visit_arm(self, arm);
        }
    }

    fn visit_field_value(&mut self, field: &'ast syn::FieldValue) {
        if !is_test_only(&field.attrs) && !is_skipped(&field.attrs) {
            visit::visit_field_value(self, field);
        }
    }

    fn visit_expr_struct(&mut self, node: &'ast syn::ExprStruct) {
        if is_test_only(&node.attrs) || is_skipped(&node.attrs) {
            return;
        }
        // Only probes read the base again. Source mutants delete a field in place.
        let from_base = node.rest.as_deref().and_then(|base| {
            if !node.attrs.is_empty() || node.qself.is_some() {
                return None;
            }
            let (from, to) = self.range(base.span())?;
            let written = &self.source[from..to];
            match base {
                base if is_plain_place(base) => Some(format!("({written})")),
                syn::Expr::Call(call)
                    if call.args.is_empty() && matches!(*call.func, syn::Expr::Path(_)) =>
                {
                    let (from, to) = self.range(node.path.span())?;
                    Some(format!("({} {{ ..{written} }})", &self.source[from..to]))
                }
                _ => None,
            }
        });
        if node.rest.is_some() {
            for pair in node.fields.pairs() {
                let field = pair.value();
                if is_test_only(&field.attrs) || is_skipped(&field.attrs) {
                    continue;
                }
                let name = match &field.member {
                    syn::Member::Named(name) => name.to_string(),
                    syn::Member::Unnamed(index) => index.index.to_string(),
                };
                let (Some((start, _)), Some((value, end)), Some((remove_start, _))) = (
                    self.range(field.member.span()),
                    self.range(field.expr.span()),
                    self.range(field.span()),
                ) else {
                    continue;
                };
                let remove_end = pair
                    .punct()
                    .and_then(|comma| self.range(comma.span()))
                    .map_or(end, |(_, end)| end);
                let from_base = from_base
                    .as_ref()
                    .filter(|_| field.attrs.is_empty())
                    .map(|base| format!("{base}.{name}"));
                let probe = from_base.is_some();
                let kind = SiteKind::Field {
                    from_base,
                    name,
                    shorthand: field.colon_token.is_none(),
                    removal: (remove_start, remove_end),
                };
                let at = field.member.span().start();
                let site = Site {
                    id: 0,
                    kind,
                    line: at.line,
                    column: at.column + 1,
                    start: if probe { value } else { remove_start },
                    end,
                    text: self.source[start..end].to_string(),
                };
                if probe {
                    self.add(site);
                } else {
                    self.add_rebuild(site);
                }
            }
        }
        visit::visit_expr_struct(self, node);
    }

    fn visit_expr_unary(&mut self, node: &'ast syn::ExprUnary) {
        let joins = |start: usize, operand_start: usize| {
            let prev = self.source[..start].chars().next_back();
            let next = self.source[operand_start..].chars().next();
            // Any two adjacent non-delimiter characters can glue into a different token or
            // comment (`return!x` -> `returnx`, `return!℘` -> `return℘`, `a/!*b` -> `a/*b`).
            let glues = |c: char| {
                !c.is_whitespace() && !matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | ',' | ';')
            };
            prev.is_some_and(glues) && next.is_some_and(glues)
        };
        if let syn::UnOp::Not(bang) = &node.op
            && node.attrs.is_empty()
            && let (Some((start, _)), Some((operand_start, end))) =
                (self.range(bang.span()), self.range(node.expr.span()))
            && in_order(start, operand_start)
        {
            let kind = SiteKind::Not {
                bang: (start, operand_start),
                operand: self.source[operand_start..end].to_string(),
                needs_space: joins(start, operand_start),
            };
            self.push(kind, start, end, bang.span());
        }
        if let syn::UnOp::Neg(minus) = &node.op
            && node.attrs.is_empty()
            && let (Some((start, _)), Some((operand_start, end))) =
                (self.range(minus.span()), self.range(node.expr.span()))
            && in_order(start, operand_start)
        {
            let zero = match &*node.expr {
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Int(lit),
                    ..
                }) => lit.base10_parse::<u128>().is_ok_and(|v| v == 0),
                // Not a float: `-0.0` is not `0.0` (`1.0 / -0.0` is minus infinity).
                _ => false,
            };
            if !zero {
                let kind = SiteKind::Neg {
                    minus: (start, operand_start),
                    operand: self.source[operand_start..end].to_string(),
                    literal: matches!(*node.expr, syn::Expr::Lit(_)),
                    needs_space: joins(start, operand_start),
                };
                self.push(kind, start, end, minus.span());
            }
        }
        visit::visit_expr_unary(self, node);
    }

    fn visit_expr_binary(&mut self, node: &'ast syn::ExprBinary) {
        if let Some(op) = cmp_op(&node.op)
            && node.attrs.is_empty()
            && let (Some((start, lhs_end)), Some((rhs_start, end))) =
                (self.range(node.left.span()), self.range(node.right.span()))
            && in_order(lhs_end, rhs_start)
        {
            let literal = match (int_literal(&node.left), int_literal(&node.right)) {
                (None, Some(k)) => LiteralSide::Rhs(k),
                (Some(k), None) => LiteralSide::Lhs(k),
                _ => LiteralSide::None,
            };
            let kind = SiteKind::Cmp {
                op,
                lhs: self.source[start..lhs_end].to_string(),
                rhs: self.source[rhs_start..end].to_string(),
                literal,
                gap: (lhs_end, rhs_start),
            };
            self.push(kind, start, end, node.left.span());
        } else if node.attrs.is_empty()
            && let (Some((start, lhs_end)), Some((rhs_start, end))) =
                (self.range(node.left.span()), self.range(node.right.span()))
            && in_order(lhs_end, rhs_start)
        {
            let lhs = self.source[start..lhs_end].to_string();
            let rhs = self.source[rhs_start..end].to_string();
            let gap = (lhs_end, rhs_start);
            let logic = match &node.op {
                syn::BinOp::And(_) => Some(true),
                syn::BinOp::Or(_) => Some(false),
                _ => None,
            };
            if let Some(and) = logic {
                if !in_let_chain(&node.left) && !matches!(*node.right, syn::Expr::Let(_)) {
                    self.push(
                        SiteKind::Logic { and, lhs, rhs, gap },
                        start,
                        end,
                        node.left.span(),
                    );
                }
            } else if let Some((op, assign)) = arith_op(&node.op) {
                let kind = SiteKind::Arith {
                    op,
                    assign,
                    lhs,
                    rhs,
                    gap,
                };
                // Two literals have no type until the context gives them one, and a
                // probe is a context of its own.
                let literals = is_literal(&node.left) && is_literal(&node.right);
                if !literals && (!assign || is_plain_place(&node.left)) {
                    self.push(kind, start, end, node.left.span());
                } else {
                    // Which of the place and the value is evaluated first depends on
                    // the type, which a probe would have to know.
                    let at = node.left.span().start();
                    self.found.rebuild_sites.push(Site {
                        id: NO_ID,
                        kind,
                        line: at.line,
                        column: at.column + 1,
                        start,
                        end,
                        text: self.source[start..end].to_string(),
                    });
                }
            }
        }
        visit::visit_expr_binary(self, node);
    }
}

/// Instrumented text plus the mapping back to the original, used to blame a site for a
/// compile error in the rewritten file.
#[derive(Debug, PartialEq)]
pub struct Rendered {
    pub text: String,
    /// `(output offset, original offset, length)` of each stretch copied from the original,
    /// in output order.
    segments: Vec<(usize, usize, usize)>,
    /// `(output offset, length, site)` of each piece of text a probe added, in output order.
    inserted: Vec<(usize, usize, u32)>,
}

impl Rendered {
    /// Where a stretch of the original that no probe touches is in the instrumented
    /// text. `None` when a probe is in it or next to its ends.
    pub fn copied(&self, from: usize, to: usize) -> Option<(usize, usize)> {
        self.segments
            .iter()
            .find(|(_, start, len)| *start <= from && to <= start + len)
            .map(|(out, start, _)| (out + from - start, out + to - start))
    }

    /// Maps a byte offset in the instrumented text to the original. Offsets inside inserted
    /// text map to the point of insertion.
    /// The site whose probe text contains the byte at `offset` of the instrumented text.
    pub fn inserted_by(&self, offset: usize) -> Option<u32> {
        let idx = self.inserted.partition_point(|(out, _, _)| *out <= offset);
        let (out, len, site) = self.inserted[idx.checked_sub(1)?];
        (offset - out < len).then_some(site)
    }

    pub fn to_original(&self, offset: usize) -> usize {
        let idx = self.segments.partition_point(|(out, _, _)| *out <= offset);
        let Some((out, orig, len)) = idx.checked_sub(1).map(|i| self.segments[i]) else {
            return 0;
        };
        // Past the end of the copied stretch is inserted text, which sits at its end.
        orig + (offset - out).min(len)
    }
}

struct Edit {
    pos: usize,
    end: usize,
    /// 0: closes a site, 1: replaces an operator, 2: opens a site.
    phase: u8,
    /// Closers: innermost first. Openers: outermost first.
    order: i64,
    text: String,
    /// The site the text belongs to.
    site: u32,
}

/// Rewrites `source` with probes for the sites that `enabled` accepts.
pub fn render(source: &str, sites: &[Site], enabled: &dyn Fn(u32) -> bool) -> Rendered {
    let rt = format!("crate::{RUNTIME_MOD}");
    let mut edits = vec![];
    for site in sites.iter().filter(|s| enabled(s.id)) {
        // Edits at one position are ordered by the length of their site, doubled to leave
        // room: a site that wraps a whole expression goes around a site of that very
        // expression (`if a > b` as a guard and as a comparison).
        let wraps = matches!(
            site.kind,
            SiteKind::Return { .. }
                | SiteKind::Body { .. }
                | SiteKind::Guard { .. }
                | SiteKind::Field { .. }
        );
        // tmt:ignore `2 +` orders the same: nothing longer starts or ends where a wrapping site does
        let len = 2 * (site.end - site.start) as i64 + wraps as i64;
        let (open, close) = match &site.kind {
            SiteKind::Cmp { op, gap, .. } => {
                let func = if op.is_ordering() { "ord" } else { "eq" };
                edits.push(Edit {
                    pos: gap.0,
                    end: gap.1,
                    phase: 1,
                    order: 0,
                    text: "), &(".into(),
                    site: site.id,
                });
                (
                    "(match (&(".to_string(),
                    format!(
                        ")) {{ (__tmt_l, __tmt_r) => {{ #[allow(unused_imports)] use {rt}::{{ValAny as _, ValNum as _}}; \
                         {rt}::{func}({id}u32, {code}u8, __tmt_l, __tmt_r, (&{rt}::W(__tmt_l)).tmt_val(), (&{rt}::W(__tmt_r)).tmt_val()) }} }})",
                        id = site.id,
                        code = op.code(),
                    ),
                )
            }
            // In the probes below the user's operands are evaluated where no name the probe
            // introduces is in scope: in the scrutinee of a `match`, in the initialiser of a
            // `let`, or in an arm that binds nothing. A scrutinee keeps its temporaries
            // until the end of the enclosing statement, as an arithmetic operator does.
            SiteKind::Logic { and, gap, .. } => {
                // The right operand stays in place, so it is evaluated exactly when the
                // (possibly mutated) left operand asks for it.
                let id = site.id;
                edits.push(Edit {
                    pos: gap.0,
                    end: gap.1,
                    phase: 1,
                    order: 0,
                    text: format!("); __tmt_c }} {{ 2u8 => {rt}::rhs({id}u32, {and}, "),
                    site: site.id,
                });
                (
                    // Each operand of `&&` / `||` drops its temporaries when it is done.
                    // The `let` ends the left operand's; the arm ends the right one's.
                    format!("(match {{ let __tmt_c: u8 = {rt}::lhs({id}u32, {and}, "),
                    "), __tmt_c => __tmt_c == 1u8 })".to_string(),
                )
            }
            SiteKind::Arith {
                op, assign, gap, ..
            } => {
                // A compound assignment hands the probe the place to update.
                let (holder, lhs_open, lhs_close) = if *assign {
                    ("Aa", "&mut (", ")")
                } else {
                    ("Ar", "", "")
                };
                edits.push(Edit {
                    pos: gap.0,
                    end: gap.1,
                    phase: 1,
                    order: 0,
                    text: format!("{lhs_close}, "),
                    site: site.id,
                });
                (
                    format!("(match ({lhs_open}"),
                    format!(
                        ") {{ (__tmt_a, __tmt_b) => {{ #[allow(unused_imports)] use {rt}::{{{any} as _, {prim} as _}}; \
                         (&{rt}::{holder}::new(__tmt_a, __tmt_b)).{method}({id}u32) }} }})",
                        any = op.fallback_trait(*assign),
                        prim = op.prim_trait(*assign),
                        method = op.method(*assign),
                        id = site.id,
                    ),
                )
            }
            SiteKind::Not { bang, .. } => {
                // Drop the `!`; the probe applies it (or not).
                edits.push(Edit {
                    pos: bang.0,
                    end: bang.1,
                    phase: 1,
                    order: 0,
                    text: String::new(),
                    site: site.id,
                });
                (
                    "(match (".to_string(),
                    format!(
                        ") {{ __tmt_a => {{ #[allow(unused_imports)] use {rt}::{{AnyNot as _, NotPrim as _}}; \
                         (&{rt}::Un::new(__tmt_a)).tmt_not({}u32) }} }})",
                        site.id
                    ),
                )
            }
            SiteKind::Return { option, .. } => match option {
                None => (
                    " let __tmt_v: bool = {".to_string(),
                    format!("}}; {rt}::ret({}u32, __tmt_v) ", site.id),
                ),
                Some(ty) => (
                    format!(" let __tmt_v: {ty} = {{"),
                    format!("}}; {rt}::ret_opt({}u32, __tmt_v) ", site.id),
                ),
            },
            SiteKind::MinMax {
                max, call, close, ..
            } => {
                // The receiver is borrowed, as a method call borrows it: a `max` that takes
                // `&self` leaves it where it is, and one that takes it by value copies it
                // out (or, for a type that cannot be copied, does not compile: no probe).
                edits.push(Edit {
                    pos: call.0,
                    end: call.1,
                    phase: 1,
                    order: 0,
                    text: "), ".into(),
                    site: site.id,
                });
                // The call's own `)` (and a trailing comma) is replaced by the probe.
                // The call is made as it was written, on the same operands; the probe only
                // decides which of the two it is.
                let (same, other) = if *max { ("max", "min") } else { ("min", "max") };
                edits.push(Edit {
                    pos: close.0,
                    end: close.1,
                    phase: 1,
                    order: 0,
                    text: format!(
                        ") {{ (__tmt_a, __tmt_b) => {{ #[allow(unused_imports)] use {rt}::{{AnyMinMax as _, MinMaxPrim as _}}; \
                         if (&{rt}::Mm(__tmt_a, &__tmt_b)).tmt_other({}u32) {{ (*__tmt_a).{other}(__tmt_b) }} else {{ (*__tmt_a).{same}(__tmt_b) }} }} }})",
                        site.id
                    ),
                    site: site.id,
                });
                ("(match (&(".to_string(), String::new())
            }
            SiteKind::SliceEnd {
                first,
                receiver,
                call,
                ..
            } => {
                // Both calls are made as they are written. Only where the runtime knows
                // the receiver's type does it ever ask for the other one; of anything else
                // it has nothing to say, whatever the call returns.
                let (same, other) = if *first {
                    ("first", "last")
                } else {
                    ("last", "first")
                };
                let id = site.id;
                let decide = format!(
                    "#[allow(unused_imports)] use {rt}::{{AnyEnd as _, SliceEnd as _}}; \
                     match (&{rt}::Ends(__TMT_BORROWED__)).tmt_other_end({id}u32, {first}) {{ \
                     {rt}::End::Other => {{ let __tmt_o = (__TMT_PLACE__).{other}(); \
                     if false {{ {rt}::same_type(&__tmt_o, &(__TMT_PLACE__).{same}()); }} __tmt_o }}, \
                     {rt}::End::Same(__tmt_s) => {{ \
                     let __tmt_r = (__TMT_PLACE__).{same}(); \
                     __tmt_s.tmt_end_seen({id}u32, &__tmt_r); __tmt_r }} }}"
                );
                // A variable is the same place each time it is named, so it is named
                // again for the call: the call then borrows it exactly as it did (a `&mut`
                // for as long as it is one). Any other receiver is evaluated once and
                // borrowed for the probe.
                let (open, close) = if let Some(name) = variable_name(receiver) {
                    // The receiver may have the name the probe would give.
                    let seen = if name == "__tmt_s" {
                        "__tmt_t"
                    } else {
                        "__tmt_s"
                    };
                    let decide = decide
                        .replace("__tmt_s", seen)
                        .replace("__TMT_PLACE__", &name);
                    let (head, tail) = decide
                        .split_once("__TMT_BORROWED__")
                        .expect("the receiver is borrowed once");
                    (format!("({{ {head}&("), format!("){tail} }})"))
                } else {
                    let decide = decide
                        .replace("__TMT_BORROWED__", "__tmt_a")
                        .replace("__TMT_PLACE__", "*__tmt_a");
                    (
                        "(match (&(".to_string(),
                        format!(")) {{ __tmt_a => {{ {decide} }} }})"),
                    )
                };
                edits.push(Edit {
                    pos: call.0,
                    end: call.1,
                    phase: 1,
                    order: 0,
                    text: close,
                    site: site.id,
                });
                (open, String::new())
            }
            SiteKind::Range { inclusive, gap, .. } => {
                edits.push(Edit {
                    pos: gap.0,
                    end: gap.1,
                    phase: 1,
                    order: 0,
                    text: ", ".into(),
                    site: site.id,
                });
                let method = if *inclusive {
                    "tmt_range_inclusive"
                } else {
                    "tmt_range"
                };
                (
                    "(match (".to_string(),
                    format!(
                        ") {{ (__tmt_a, __tmt_b) => {{ #[allow(unused_imports)] use {rt}::{{AnyRange as _, RangePrim as _}}; \
                         (&{rt}::Ar::new(__tmt_a, __tmt_b)).{method}({}u32) }} }})",
                        site.id
                    ),
                )
            }
            SiteKind::Arm { guard, .. } => {
                // The probe goes into the arm's guard, where it runs only when the
                // pattern matched.
                match guard {
                    ArmGuard::None(arrow) => edits.push(Edit {
                        pos: *arrow,
                        end: *arrow,
                        phase: 2,
                        order: 0,
                        text: format!("if {rt}::arm({}u32) ", site.id),
                        site: site.id,
                    }),
                    ArmGuard::Existing(from, to) => {
                        let wider = 2 * (to - from + 1) as i64;
                        edits.push(Edit {
                            pos: *from,
                            end: *from,
                            phase: 2,
                            order: -wider,
                            text: format!("{rt}::arm({}u32) && (", site.id),
                            site: site.id,
                        });
                        edits.push(Edit {
                            pos: *to,
                            end: *to,
                            phase: 0,
                            order: wider,
                            text: ")".into(),
                            site: site.id,
                        });
                    }
                }
                (String::new(), String::new())
            }
            SiteKind::Stmt => (
                format!("if {rt}::live({}u32) {{ ", site.id),
                " }".to_string(),
            ),
            // A literal takes its type from where it stands, so it stays where it is.
            SiteKind::Neg {
                operand,
                literal: true,
                ..
            } => (
                format!("(if {rt}::live({}u32) {{ ", site.id),
                format!(" }} else {{ {operand} }})"),
            ),
            SiteKind::Neg { minus, .. } => {
                edits.push(Edit {
                    pos: minus.0,
                    end: minus.1,
                    phase: 1,
                    order: 0,
                    text: String::new(),
                    site: site.id,
                });
                (
                    "(match (".to_string(),
                    format!(
                        ") {{ __tmt_a => if {rt}::live({}u32) {{ -__tmt_a }} else {{ __tmt_a }} }})",
                        site.id
                    ),
                )
            }
            SiteKind::Guard { .. } => (format!("{rt}::ret({}u32, ", site.id), ")".to_string()),
            SiteKind::Field {
                name,
                shorthand,
                from_base: Some(from_base),
                ..
            } => (
                format!(
                    "{}if {rt}::live({}u32) {{ ",
                    if *shorthand {
                        format!("{name}: ")
                    } else {
                        String::new()
                    },
                    site.id
                ),
                format!(" }} else {{ {from_base} }}"),
            ),
            SiteKind::Field {
                from_base: None, ..
            } => continue,
            SiteKind::Body { value, .. } => (
                format!(" if !{rt}::live({}u32) {{ return {value}; }}", site.id),
                String::new(),
            ),
        };
        edits.push(Edit {
            pos: site.start,
            end: site.start,
            phase: 2,
            order: -len,
            text: open,
            site: site.id,
        });
        edits.push(Edit {
            pos: site.end,
            end: site.end,
            phase: 0,
            order: len,
            text: close,
            site: site.id,
        });
    }
    edits.sort_by_key(|e| (e.pos, e.phase, e.order));

    // tmt:ignore the capacity is a hint: any number gives the same text
    let capacity = source.len() + edits.iter().map(|e| e.text.len()).sum::<usize>();
    let mut text = String::with_capacity(capacity);
    let mut segments = vec![];
    let mut inserted = vec![];
    let mut cursor = 0;
    for edit in edits {
        // Also when nothing lies between two edits: text inserted right after a replaced
        // stretch then maps to where it was inserted, not to where the stretch began.
        if edit.pos >= cursor {
            segments.push((text.len(), cursor, edit.pos - cursor));
            text.push_str(&source[cursor..edit.pos]);
        }
        // A line that ends inside a replaced stretch still ends: `line!()` and the place
        // of a panic after it are what they were.
        let replaced = &source[cursor.max(edit.pos)..cursor.max(edit.end)];
        let lines = "\n".repeat(replaced.matches('\n').count());
        inserted.push((text.len(), edit.text.len() + lines.len(), edit.site));
        text.push_str(&edit.text);
        text.push_str(&lines);
        cursor = cursor.max(edit.end);
    }
    segments.push((text.len(), cursor, source.len() - cursor));
    text.push_str(&source[cursor..]);
    Rendered {
        text,
        segments,
        inserted,
    }
}

/// The sites to blame for a compile error at `offset` (original coordinates): the smallest
/// enabled sites that contain it.
pub fn blame(sites: &[Site], enabled: &dyn Fn(u32) -> bool, offset: usize) -> Vec<u32> {
    // A replaced body is blamed for what is wrong in its own text only (`inserted_by`):
    // by position it would answer for everything in the function.
    let containing = || {
        sites.iter().filter(|s| {
            enabled(s.id)
                && s.start <= offset
                && offset <= s.end
                && !matches!(s.kind, SiteKind::Body { .. })
        })
    };
    let Some(smallest) = containing().map(|s| s.end - s.start).min() else {
        return vec![];
    };
    containing()
        .filter(|s| s.end - s.start == smallest)
        .map(|s| s.id)
        .collect()
}

#[cfg(test)]
mod tests {
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
        let src =
            "fn f(x: i32, y: i32) { let _ = (-5 < x, x == (3), 1 < 2, x <= y, x > 1.5 as i32); }";
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
            out.contains("(match { let __tmt_c: u8 = crate::__tmt_rt::lhs(0u32, true, (match (a) { __tmt_a => {"),
            "{out}"
        );
        // Names the probes introduce are never in scope where user code runs.
        let capture = "fn f(a: bool, __tmt_c: u8, __tmt_a: u8) -> bool { a && __tmt_c == 2 && __tmt_a + 1 > 0 }";
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
            labelled(
                "fn f(a: u8, o: Option<u8>) { if a > 1 && let Some(b) = o && b > 2 { g(b); } }"
            ),
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
            labelled(
                "trait T { const fn f(a: u8) -> bool { a > 1 } fn g(a: u8) -> bool { a > 2 } }"
            ),
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
                    let into: Vec<String> =
                        source_mutants(site).into_iter().map(|m| m.into).collect();
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
            let source =
                format!("{attribute}\nconst N: u8 = 6 * 7;\nfn f(a: u8) -> u8 {{ a + 1 }}");
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
}
