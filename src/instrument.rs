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
}

impl Finder<'_> {
    /// Registers the body of a function that returns `bool` or `Option<T>` as a site.
    fn result_function(&mut self, sig: &syn::Signature, body: &syn::Block) {
        let syn::ReturnType::Type(_, ty) = &sig.output else {
            return;
        };
        let option = match &**ty {
            syn::Type::Path(p) if p.qself.is_none() && p.path.is_ident("bool") => None,
            syn::Type::Path(p)
                if p.qself.is_none()
                    && p.path.segments.last().is_some_and(|s| {
                        s.ident == "Option"
                            && matches!(s.arguments, syn::PathArguments::AngleBracketed(_))
                    }) =>
            {
                match self.range(ty.span()) {
                    // `impl Trait` cannot be written in a `let`.
                    Some((from, to)) if !self.source[from..to].contains("impl ") => {
                        Some(self.source[from..to].to_string())
                    }
                    _ => return,
                }
            }
            _ => return,
        };
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
        if start > end {
            return;
        }
        let at = sig.span().start();
        self.found.sites.push(Site {
            id: self.next_id,
            kind: SiteKind::Return {
                name: sig.ident.to_string(),
                option,
                sole_expr: match &body.stmts[..] {
                    [syn::Stmt::Expr(expr, None)] => self.range(expr.span()),
                    _ => None,
                },
            },
            line: at.line,
            column: at.column + 1,
            start,
            end,
            text: self.source[sig_start..sig_end].to_string(),
        });
        self.next_id += 1;
    }
    fn range(&self, span: Span) -> Option<(usize, usize)> {
        let range = span.byte_range();
        // Spans produced by macro hygiene tricks or empty token streams carry no location.
        (range.start < range.end
            && range.end <= self.source.len()
            && self.source.is_char_boundary(range.start)
            && self.source.is_char_boundary(range.end))
        .then_some((range.start, range.end))
    }

    fn push(&mut self, kind: SiteKind, start: usize, end: usize, span: Span) {
        let at = span.start();
        self.found.sites.push(Site {
            id: self.next_id,
            kind,
            line: at.line,
            column: at.column + 1,
            start,
            end,
            text: self.source[start..end].to_string(),
        });
        self.next_id += 1;
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
        syn::Expr::Binary(e) => e.attrs.is_empty() && is_assign_op(&e.op),
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
    fn visit_item_const(&mut self, _: &'ast syn::ItemConst) {}
    fn visit_item_static(&mut self, _: &'ast syn::ItemStatic) {}
    fn visit_impl_item_const(&mut self, _: &'ast syn::ImplItemConst) {}
    fn visit_trait_item_const(&mut self, _: &'ast syn::TraitItemConst) {}
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
                match arg {
                    // `name = value` in a format string is not an assignment.
                    syn::Expr::Assign(assign) => self.visit_expr(&assign.right),
                    other => self.visit_expr(other),
                }
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
        visit::visit_item_mod(self, node);
    }

    fn visit_item_fn(&mut self, node: &'ast syn::ItemFn) {
        if !is_test_only(&node.attrs) && node.sig.constness.is_none() {
            self.result_function(&node.sig, &node.block);
            visit::visit_item_fn(self, node);
        }
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        if !is_test_only(&node.attrs) && node.sig.constness.is_none() {
            self.result_function(&node.sig, &node.block);
            visit::visit_impl_item_fn(self, node);
        }
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        if !is_test_only(&node.attrs) && node.sig.constness.is_none() {
            if let Some(body) = &node.default {
                self.result_function(&node.sig, body);
            }
            visit::visit_trait_item_fn(self, node);
        }
    }

    fn visit_item_impl(&mut self, node: &'ast syn::ItemImpl) {
        if !is_test_only(&node.attrs) {
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
                && start < end
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
            && receiver_end <= argument_start
            && argument_end < end
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
            && lhs_end <= rhs_start
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
                self.found.sites.push(Site {
                    id: self.next_id,
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
                self.next_id += 1;
            }
        }
        visit::visit_expr_match(self, node);
    }

    fn visit_expr_unary(&mut self, node: &'ast syn::ExprUnary) {
        if let syn::UnOp::Not(bang) = &node.op
            && node.attrs.is_empty()
            && let (Some((start, _)), Some((operand_start, end))) =
                (self.range(bang.span()), self.range(node.expr.span()))
            && start < operand_start
        {
            let kind = SiteKind::Not {
                bang: (start, operand_start),
                operand: self.source[operand_start..end].to_string(),
            };
            self.push(kind, start, end, bang.span());
        }
        visit::visit_expr_unary(self, node);
    }

    fn visit_expr_binary(&mut self, node: &'ast syn::ExprBinary) {
        if let Some(op) = cmp_op(&node.op)
            && node.attrs.is_empty()
            && let (Some((start, lhs_end)), Some((rhs_start, end))) =
                (self.range(node.left.span()), self.range(node.right.span()))
            && lhs_end <= rhs_start
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
            && lhs_end <= rhs_start
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
            } else if let Some((op, assign)) = arith_op(&node.op)
                && !(is_literal(&node.left) && is_literal(&node.right))
                && (!assign || is_plain_place(&node.left))
            {
                self.push(
                    SiteKind::Arith {
                        op,
                        assign,
                        lhs,
                        rhs,
                        gap,
                    },
                    start,
                    end,
                    node.left.span(),
                );
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
}

impl Rendered {
    /// Maps a byte offset in the instrumented text to the original. Offsets inside inserted
    /// text map to the point of insertion.
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
}

/// Rewrites `source` with probes for the sites that `enabled` accepts.
pub fn render(source: &str, sites: &[Site], enabled: &dyn Fn(u32) -> bool) -> Rendered {
    let rt = format!("crate::{RUNTIME_MOD}");
    let mut edits = vec![];
    for site in sites.iter().filter(|s| enabled(s.id)) {
        let len = (site.end - site.start) as i64;
        let (open, close) = match &site.kind {
            SiteKind::Cmp { op, gap, .. } => {
                let func = if op.is_ordering() { "ord" } else { "eq" };
                edits.push(Edit {
                    pos: gap.0,
                    end: gap.1,
                    phase: 1,
                    order: 0,
                    text: "), &(".into(),
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
                    }),
                    ArmGuard::Existing(from, to) => {
                        let wider = (to - from + 1) as i64;
                        edits.push(Edit {
                            pos: *from,
                            end: *from,
                            phase: 2,
                            order: -wider,
                            text: format!("{rt}::arm({}u32) && (", site.id),
                        });
                        edits.push(Edit {
                            pos: *to,
                            end: *to,
                            phase: 0,
                            order: wider,
                            text: ")".into(),
                        });
                    }
                }
                (String::new(), String::new())
            }
            SiteKind::Stmt => (
                format!("if {rt}::live({}u32) {{ ", site.id),
                " }".to_string(),
            ),
        };
        edits.push(Edit {
            pos: site.start,
            end: site.start,
            phase: 2,
            order: -len,
            text: open,
        });
        edits.push(Edit {
            pos: site.end,
            end: site.end,
            phase: 0,
            order: len,
            text: close,
        });
    }
    edits.sort_by_key(|e| (e.pos, e.phase, e.order));

    let mut text =
        String::with_capacity(source.len() + edits.iter().map(|e| e.text.len()).sum::<usize>());
    let mut segments = vec![];
    let mut cursor = 0;
    for edit in edits {
        if edit.pos > cursor {
            segments.push((text.len(), cursor, edit.pos - cursor));
            text.push_str(&source[cursor..edit.pos]);
        }
        text.push_str(&edit.text);
        cursor = cursor.max(edit.end);
    }
    segments.push((text.len(), cursor, source.len() - cursor));
    text.push_str(&source[cursor..]);
    Rendered { text, segments }
}

/// The sites to blame for a compile error at `offset` (original coordinates): the smallest
/// enabled sites that contain it.
pub fn blame(sites: &[Site], enabled: &dyn Fn(u32) -> bool, offset: usize) -> Vec<u32> {
    let containing = || {
        sites
            .iter()
            .filter(|s| enabled(s.id) && s.start <= offset && offset <= s.end)
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
            .retain(|s| !matches!(s.kind, SiteKind::Return { .. }));
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
            .map(|s| match &s.kind {
                SiteKind::Cmp { literal, .. } => *literal,
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
            out.contains("pub fn is_ok(a: u8) -> bool { let __tmt_v: bool = { let b = a; (match"),
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
        assert_eq!(blame(&sites, &all, rendered.to_original(wrapper)), [0]);
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
        let texts: Vec<&str> = sites.iter().map(|s| s.text.as_str()).collect();
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
}
