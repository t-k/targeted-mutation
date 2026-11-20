//! Finds mutation sites in Rust source and rewrites them into probes.
//!
//! Every site stays in the compiled code (mutant schemata): a comparison `a >= b` becomes a
//! call that records what it saw and, when a mutant is switched on, returns the mutated
//! result. One build serves the trace run and every mutant.
//!
//! The rewrite only inserts text on the line where the site is, so line numbers in compiler
//! diagnostics still match the original file.

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
    },
    /// The body of a function that returns `bool`. The site's range is the inside of the
    /// body's braces and its text is the function signature.
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
        /// The expression that reads the field from the base.
        from_base: String,
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
    // The parser drops a byte order mark and a `#!` line before it numbers bytes, so its
    // offsets would not be offsets into `source`.
    if source.starts_with('\u{feff}') || (source.starts_with("#!") && !source.starts_with("#![")) {
        anyhow::bail!("starts with a byte order mark or a `#!` line");
    }
    let file = syn::parse_file(source).context("not parseable as a Rust source file")?;
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
    };
    finder.visit_file(&file);
    Ok(finder.found)
}

struct Finder<'s> {
    /// Names of the macros this file defines at its top level.
    local_macros: Vec<String>,
    source: &'s str,
    next_id: u32,
    found: Discovery,
    /// How many `const fn` the visitor is inside of.
    const_depth: usize,
}

impl Finder<'_> {
    /// Registers the sites of a function as a whole: its result (for `bool` and
    /// `Option<T>`), and its body replaced by each value its return type suggests.
    fn function_sites(&mut self, sig: &syn::Signature, body: &syn::Block) {
        if body.stmts.is_empty() {
            return;
        }
        let braces = body.brace_token.span;
        let (Some((_, start)), Some((end, _)), Some((sig_start, sig_end))) = (
            self.range(braces.open()),
            self.range(braces.close()),
            self.range(sig.span()),
        ) else {
            return;
        };
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
        if let Some(option) = self.result_kind(&sig.output) {
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
        // An iterator of another type cannot be returned beside the body's own.
        for value in iterator_values(&sig.output) {
            self.found.rebuild_sites.push(Site {
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
        for value in body_values(&sig.output) {
            if written.as_deref() != Some(tokens_of(&value).as_str()) {
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

    /// Registers a site: numbered, or among those that need a build of their own.
    fn add(&mut self, mut site: Site) {
        if self.const_depth > 0 {
            site.id = NO_ID;
            self.found.rebuild_sites.push(site);
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
/// only exist as probes (a result replaced after the body ran, a deleted statement, the
/// other of `min` / `max`, the end of a range) have none.
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
        let inside =
            site.start <= gap.0 && gap.1 <= site.end && site.text.len() == site.end - site.start;
        let (left, right) = if inside {
            (
                site.text[..gap.0 - site.start].trim_end(),
                site.text[gap.1 - site.start..].trim_start(),
            )
        } else {
            ("", "")
        };
        match binds {
            std::cmp::Ordering::Less if inside => change(
                site.start,
                site.end,
                format!("({left} {to} {right})"),
                &from,
                &to,
            ),
            std::cmp::Ordering::Greater if inside => change(
                site.start,
                site.end,
                format!("({left}) {to} ({right})"),
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
        SiteKind::Not { bang, .. } => vec![change(bang.0, bang.1, String::new(), "!", "")],
        SiteKind::Neg { minus, .. } => vec![change(minus.0, minus.1, String::new(), "-", "")],
        SiteKind::Body { value, .. } => {
            vec![change(
                site.start,
                site.end,
                format!(" {value} "),
                "body",
                value,
            )]
        }
        // An arm whose guard is `false` is an arm that is not there.
        SiteKind::Arm { pattern, guard } => vec![match guard {
            ArmGuard::None(arrow) => change(*arrow, *arrow, "if false ".into(), "arm", pattern),
            ArmGuard::Existing(from, to) => change(*from, *to, "false".into(), "arm", pattern),
        }],
        SiteKind::Guard { can_be_false } => [true, false]
            .into_iter()
            .filter(|value| *value || *can_be_false)
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
        SiteKind::Field {
            name,
            shorthand,
            from_base,
        } => {
            let prefix = if *shorthand {
                format!("{name}: ")
            } else {
                String::new()
            };
            vec![change(
                site.start,
                site.end,
                format!("{prefix}{from_base}"),
                "field",
                name,
            )]
        }
        SiteKind::Return { .. }
        | SiteKind::MinMax { .. }
        | SiteKind::Range { .. }
        | SiteKind::Stmt => vec![],
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
    if has_ident(item.to_token_stream(), "impl") {
        return vec!["::core::iter::empty()".to_string()];
    }
    let mut values = vec!["::core::iter::empty()".to_string()];
    values.extend(
        values_of(item)
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
    match output {
        syn::ReturnType::Default => vec!["()".to_string()],
        syn::ReturnType::Type(_, ty) => {
            // What an `impl Trait` stands for is the type of the body: no other value
            // can be returned beside it.
            if has_ident(ty.to_token_stream(), "impl") {
                return vec![];
            }
            values_of(ty)
        }
    }
}

fn product(parts: &[Vec<String>]) -> Vec<Vec<String>> {
    parts.iter().fold(vec![vec![]], |rows, part| {
        rows.iter()
            .flat_map(|row| {
                part.iter().map(move |value| {
                    let mut row = row.clone();
                    row.push(value.clone());
                    row
                })
            })
            .collect()
    })
}

fn values_of(ty: &syn::Type) -> Vec<String> {
    let each = |inner: &syn::Type, wrap: &dyn Fn(&str) -> String| -> Vec<String> {
        values_of(inner).iter().map(|v| wrap(v)).collect()
    };
    match ty {
        syn::Type::Paren(t) => values_of(&t.elem),
        syn::Type::Group(t) => values_of(&t.elem),
        syn::Type::Never(_) => vec![],
        // Also the empty one: one row of no values, written `()`.
        syn::Type::Tuple(t) => {
            let parts: Vec<Vec<String>> = t.elems.iter().map(values_of).collect();
            product(&parts)
                .into_iter()
                .map(|row| match &row[..] {
                    [one] => format!("({one},)"),
                    _ => format!("({})", row.join(", ")),
                })
                .collect()
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
                ("Result", [ok, ..]) => each(ok, &|v| format!("Ok({v})")),
                // `fmt::Result`, or an alias of the crate's own.
                ("Result", []) => vec!["Ok(Default::default())".to_string()],
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
                    values.extend(
                        product(&[values_of(key), values_of(value)])
                            .into_iter()
                            .map(|row| format!("{bare}::from_iter([({})])", row.join(", "))),
                    );
                    values
                }
                ("Cow", [inner]) => {
                    let borrowed: syn::Type = syn::parse_quote!(&#inner);
                    values_of(&borrowed)
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
                _ => vec!["Default::default()".to_string()],
            }
        }
        _ => vec!["Default::default()".to_string()],
    }
}

/// `#[mutants::skip]`, also inside `cfg_attr`: the mark cargo-mutants uses for code that
/// is not to be mutated.
fn is_skipped(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        let tokens = attr.meta.to_token_stream();
        has_ident(tokens.clone(), "mutants") && has_ident(tokens, "skip")
    })
}

/// `#[test]`, `#[tokio::test]`, `#[cfg(test)]`, `#[cfg(all(test, ...))]`; not `#[cfg(not(test))]`.
fn is_test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        let path = attr.path();
        if path.segments.last().is_some_and(|s| s.ident == "test") {
            return true;
        }
        if !path.is_ident("cfg") {
            return false;
        }
        let syn::Meta::List(list) = &attr.meta else {
            return false;
        };
        has_ident(list.tokens.clone(), "test") && !has_ident(list.tokens.clone(), "not")
    })
}

impl<'ast> Visit<'ast> for Finder<'_> {
    // Nothing in these can call a non-const function or is worth mutating.
    fn visit_attribute(&mut self, _: &'ast syn::Attribute) {}
    fn visit_type(&mut self, _: &'ast syn::Type) {}
    fn visit_pat(&mut self, _: &'ast syn::Pat) {}
    fn visit_generics(&mut self, _: &'ast syn::Generics) {}
    fn visit_generic_argument(&mut self, _: &'ast syn::GenericArgument) {}
    fn visit_variant(&mut self, _: &'ast syn::Variant) {}
    fn visit_item_const(&mut self, node: &'ast syn::ItemConst) {
        self.constant(&node.attrs, &node.expr);
    }
    fn visit_item_static(&mut self, node: &'ast syn::ItemStatic) {
        self.constant(&node.attrs, &node.expr);
    }
    fn visit_impl_item_const(&mut self, node: &'ast syn::ImplItemConst) {
        self.constant(&node.attrs, &node.expr);
    }
    fn visit_trait_item_const(&mut self, node: &'ast syn::TraitItemConst) {
        if let Some((_, value)) = &node.default {
            self.constant(&node.attrs, value);
        }
    }
    fn visit_expr_const(&mut self, _: &'ast syn::ExprConst) {}

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
        self.visit_expr(&node.expr); // the length is a const expression
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if is_test_only(&node.attrs) {
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
        if !is_skipped(&node.attrs) {
            visit::visit_item_mod(self, node);
        }
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            // Nothing in a `const fn` can ask at run time which mutant is on.
            let constant = node.sig.constness.is_some() as usize;
            self.const_depth += constant;
            self.function_sites(&node.sig, &node.block);
            visit::visit_item_fn(self, node);
            self.const_depth -= constant;
        }
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            // Nothing in a `const fn` can ask at run time which mutant is on.
            let constant = node.sig.constness.is_some() as usize;
            self.const_depth += constant;
            self.function_sites(&node.sig, &node.block);
            visit::visit_impl_item_fn(self, node);
            self.const_depth -= constant;
        }
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        if !is_test_only(&node.attrs) && !is_skipped(&node.attrs) {
            let constant = node.sig.constness.is_some() as usize;
            self.const_depth += constant;
            if let Some(body) = &node.default {
                self.function_sites(&node.sig, body);
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
        visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_range(&mut self, node: &'ast syn::ExprRange) {
        // With two literals nothing fixes the integer type before the probe is resolved,
        // and it would be taken for `i32` (`for i in 0..16 { words[i] }`).
        if let (Some(from), Some(to)) = (&node.start, &node.end)
            && node.attrs.is_empty()
            && !(is_literal(from) && is_literal(to))
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
            self.push(kind, start, end, from.span());
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
        // Where the arms that can be removed start.
        let mut removable: Vec<usize> = vec![];
        if let Some((last, others)) = node.arms.split_last()
            && catches_all(last)
        {
            for arm in others
                .iter()
                .filter(|arm| arm.attrs.is_empty() && !catches_all(arm))
            {
                let (Some((start, pat_end)), Some((_, end)), Some((arrow, _))) = (
                    self.range(arm.pat.span()),
                    self.range(arm.body.span()),
                    self.range(arm.fat_arrow_token.span()),
                ) else {
                    continue;
                };
                let guard = match &arm.guard {
                    None => ArmGuard::None(arrow),
                    Some((_, expr)) => match self.range(expr.span()) {
                        Some((from, to)) => ArmGuard::Existing(from, to),
                        None => continue,
                    },
                };
                let pattern = match guard {
                    ArmGuard::Existing(from, to) => {
                        format!(
                            "{} if {}",
                            &self.source[start..pat_end],
                            &self.source[from..to]
                        )
                    }
                    ArmGuard::None(_) => self.source[start..pat_end].to_string(),
                };
                let at = arm.pat.span().start();
                removable.push(start);
                self.add(Site {
                    id: 0,
                    kind: SiteKind::Arm {
                        pattern: pattern.clone(),
                        guard,
                    },
                    line: at.line,
                    column: at.column + 1,
                    start,
                    end,
                    text: format!("{pattern} => .."),
                });
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
                let kind = SiteKind::Guard {
                    can_be_false: !removable.contains(&pattern),
                };
                self.push(kind, start, end, guard.span());
            }
        }
        visit::visit_expr_match(self, node);
    }

    fn visit_expr_struct(&mut self, node: &'ast syn::ExprStruct) {
        // The base is read a second time by the mutant, so it has to be something that
        // can be: a place, or a call without arguments (`Default::default()`).
        let from_base = node.rest.as_deref().and_then(|base| {
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
        if let Some(from_base) = from_base
            && node.attrs.is_empty()
            && node.qself.is_none()
        {
            for field in node.fields.iter().filter(|field| field.attrs.is_empty()) {
                let name = match &field.member {
                    syn::Member::Named(ident) => ident.to_string(),
                    syn::Member::Unnamed(index) => index.index.to_string(),
                };
                let (Some((start, _)), Some((_, end))) = (
                    self.range(field.member.span()),
                    self.range(field.expr.span()),
                ) else {
                    continue;
                };
                let Some((value, _)) = self.range(field.expr.span()) else {
                    continue;
                };
                let shorthand = field.colon_token.is_none();
                let kind = SiteKind::Field {
                    from_base: format!("{from_base}.{name}"),
                    name,
                    shorthand,
                };
                let at = field.member.span().start();
                self.add(Site {
                    id: 0,
                    kind,
                    line: at.line,
                    column: at.column + 1,
                    // The probe goes around the value; the report shows the whole field.
                    start: value,
                    end,
                    text: self.source[start..end].to_string(),
                });
            }
        }
        visit::visit_expr_struct(self, node);
    }

    fn visit_expr_unary(&mut self, node: &'ast syn::ExprUnary) {
        if let syn::UnOp::Not(bang) = &node.op
            && node.attrs.is_empty()
            && let (Some((start, _)), Some((operand_start, end))) =
                (self.range(bang.span()), self.range(node.expr.span()))
            && in_order(start, operand_start)
        {
            let kind = SiteKind::Not {
                bang: (start, operand_start),
                operand: self.source[operand_start..end].to_string(),
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
                syn::Expr::Lit(syn::ExprLit {
                    lit: syn::Lit::Float(lit),
                    ..
                }) => lit.base10_parse::<f64>().is_ok_and(|v| v == 0.0),
                _ => false,
            };
            if !zero {
                let kind = SiteKind::Neg {
                    minus: (start, operand_start),
                    operand: self.source[operand_start..end].to_string(),
                    literal: matches!(*node.expr, syn::Expr::Lit(_)),
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
                edits.push(Edit {
                    pos: call.0,
                    end: call.1,
                    phase: 1,
                    order: 0,
                    text: ", ".into(),
                    site: site.id,
                });
                // The call's own `)` (and a trailing comma) is replaced by the probe.
                let method = if *max { "tmt_max" } else { "tmt_min" };
                edits.push(Edit {
                    pos: close.0,
                    end: close.1,
                    phase: 1,
                    order: 0,
                    text: format!(
                        ") {{ (__tmt_a, __tmt_b) => {{ #[allow(unused_imports)] use {rt}::{{AnyMinMax as _, MinMaxPrim as _}}; \
                         (&{rt}::Ar::new(__tmt_a, __tmt_b)).{method}({}u32) }} }})",
                        site.id
                    ),
                    site: site.id,
                });
                ("(match (".to_string(), String::new())
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
                from_base,
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
        inserted.push((text.len(), edit.text.len(), edit.site));
        text.push_str(&edit.text);
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
                let _ = &v[1..=limit as usize];
                let _ = 'a'..'z';
                let _ = v.iter().max();
                None
            }
            fn exhaustive(k: Kind) -> u8 { match k { Kind::A => 1, Kind::B => 2 } }
            fn hidden() -> Option<impl Sized> { Some(1) }
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
                SiteKind::Arm { pattern, guard } => Some(format!(
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
                "return find Some(\"Option<u32>\")",
                "range 0 v.len() inclusive=false",
                "minmax v[i].min(limit) 1 max=true",
                "minmax v[i] limit max=false",
                // The catch-all arm and the last arm stay; a match without one is left alone.
                "arm 0 guarded=false",
                "arm n if n > 5 guarded=true",
                "arm 1..=3 guarded=false",
                // `1..=2` and `'a'..'z'` have literals at both ends.
                "range 1 limit as usize inclusive=true",
            ]
        );
        let out = render(src, &sites, &all).text;
        syn::parse_file(&out).expect("instrumented source parses");
        assert_eq!(out.lines().count(), src.lines().count());
        assert!(out.contains("let __tmt_v: Option<u32> = {"), "{out}");
        assert!(
            out.contains("crate::__tmt_rt::ret_opt(0u32, __tmt_v)"),
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
        assert!(
            out.contains(".tmt_min(") && out.contains(".tmt_max("),
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
                ("early".to_string(), None, None),
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
    fn min_max_and_ranges_that_are_left_alone() {
        assert_eq!(
            labelled(
                "fn f(a: u8, b: u8) -> u8 { let _ = a.max::<u8>(b); \
                 let _ = #[allow(unused)] a.min(b); let _ = a.max(b, 1); a.max(b) }"
            ),
            ["minmax a.max(b)"]
        );
        // One literal end is enough to fix the type; the ends are searched for sites.
        let src = "fn f(a: usize, b: usize, v: &[u8]) { let _ = &v[a..10]; let _ = &v[1..b]; \
                   let _ = (a + 1)..(b * 2); let _ = 3..4; let _ = (5)..(-6); }";
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
        assert!(out.contains("&v[(match (a, 10) {"), "{out}");
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
        let src = "fn f(a: usize, b: usize) { let _ = a..b + 1; }";
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
                    from_base,
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
                // `-0` and `-0.0` are 0 either way.
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
            !out.contains("{ -0 }") && !out.contains("{ -0.0 }"),
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
                "0..n"
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
                "const fn k(&self) -> u8: body -> 0",
                "const fn k(&self) -> u8: body -> 1",
                "-self.0: - -> ",
                "const fn d(a: u8) -> u8: body -> 0",
                "const fn d(a: u8) -> u8: body -> 1",
            ]
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
                // An arm goes by a guard that is never true.
                "arm -> K::A(n) if n >= 1 | `n >= 1` becomes `false`",
                "arm -> K::B | `` becomes `if false `",
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
                "field -> on | `x` becomes `(base).on`",
                "field -> a | `a` becomes `a: (base).a`",
            ]
        );
        // What exists only as a probe has no mutant of the source.
        let src = "fn f(a: u8, b: u8) -> bool { g(); for _ in a..b {} a.max(b) > 1 }";
        let sites = discover(src, 0).unwrap().sites;
        for site in &sites {
            let none = matches!(
                site.kind,
                SiteKind::Return { .. }
                    | SiteKind::Stmt
                    | SiteKind::Range { .. }
                    | SiteKind::MinMax { .. }
            );
            assert_eq!(source_mutants(site).is_empty(), none, "{:?}", site.kind);
        }
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
        ] {
            let src = format!("impl S {{ fn f(&mut self, n: usize) -> bool {{ {body} }} }}");
            let all = bodies(&src);
            assert_eq!(all.len(), 2, "{body}");
            assert!(all.iter().all(|b| b.1 == only_computes), "{body}: {all:?}");
        }
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
                // Two literals in ordinary code too: a probe would give them a type.
                "60 * 60: * -> /",
                "60 * 60: * -> +",
            ]
        );
        mutated(src, &found.rebuild_sites);
        // The rest of that function is probed as usual; an array length is left alone.
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
}
