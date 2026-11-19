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
        lhs: String,
        rhs: String,
        gap: (usize, usize),
    },
    /// A call or assignment statement that can be deleted.
    Stmt,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

impl ArithOp {
    pub fn symbol(self) -> &'static str {
        match self {
            ArithOp::Add => "+",
            ArithOp::Sub => "-",
            ArithOp::Mul => "*",
            ArithOp::Div => "/",
            ArithOp::Rem => "%",
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
        }
    }

    /// Name of the probe method in the runtime.
    fn method(self) -> &'static str {
        match self {
            ArithOp::Add => "tmt_add",
            ArithOp::Sub => "tmt_sub",
            ArithOp::Mul => "tmt_mul",
            ArithOp::Div => "tmt_div",
            ArithOp::Rem => "tmt_rem",
        }
    }

    fn fallback_trait(self) -> &'static str {
        match self {
            ArithOp::Add => "AnyAdd",
            ArithOp::Sub => "AnySub",
            ArithOp::Mul => "AnyMul",
            ArithOp::Div => "AnyDiv",
            ArithOp::Rem => "AnyRem",
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
    let file = syn::parse_file(source).context("not parseable as a Rust source file")?;
    let mut finder = Finder {
        source,
        next_id: first_id,
        found: Discovery::default(),
    };
    finder.visit_file(&file);
    Ok(finder.found)
}

struct Finder<'s> {
    source: &'s str,
    next_id: u32,
    found: Discovery,
}

impl Finder<'_> {
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

fn arith_op(op: &syn::BinOp) -> Option<ArithOp> {
    Some(match op {
        syn::BinOp::Add(_) => ArithOp::Add,
        syn::BinOp::Sub(_) => ArithOp::Sub,
        syn::BinOp::Mul(_) => ArithOp::Mul,
        syn::BinOp::Div(_) => ArithOp::Div,
        syn::BinOp::Rem(_) => ArithOp::Rem,
        _ => return None,
    })
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
        if !EXPR_MACROS.contains(&name.as_str()) {
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
            visit::visit_item_fn(self, node);
        }
    }

    fn visit_impl_item_fn(&mut self, node: &'ast syn::ImplItemFn) {
        if !is_test_only(&node.attrs) && node.sig.constness.is_none() {
            visit::visit_impl_item_fn(self, node);
        }
    }

    fn visit_trait_item_fn(&mut self, node: &'ast syn::TraitItemFn) {
        if !is_test_only(&node.attrs) && node.sig.constness.is_none() {
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
            } else if let Some(op) = arith_op(&node.op)
                && !(is_literal(&node.left) && is_literal(&node.right))
            {
                self.push(
                    SiteKind::Arith { op, lhs, rhs, gap },
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
            SiteKind::Logic { and, gap, .. } => {
                // The right operand stays in place, so it is evaluated exactly when the
                // (possibly mutated) left operand asks for it.
                let id = site.id;
                edits.push(Edit {
                    pos: gap.0,
                    end: gap.1,
                    phase: 1,
                    order: 0,
                    text: format!("); if __tmt_c == 2u8 {{ {rt}::rhs({id}u32, {and}, "),
                });
                (
                    format!("({{ let __tmt_c = {rt}::lhs({id}u32, {and}, "),
                    ") } else { __tmt_c == 1u8 } })".to_string(),
                )
            }
            SiteKind::Arith { op, gap, .. } => {
                edits.push(Edit {
                    pos: gap.0,
                    end: gap.1,
                    phase: 1,
                    order: 0,
                    text: ", ".into(),
                });
                (
                    format!(
                        "({{ #[allow(unused_imports)] use {rt}::{{{any} as _, ArithPrim as _}}; (&{rt}::Ar::new(",
                        any = op.fallback_trait()
                    ),
                    format!(")).{}({}u32) }})", op.method(), site.id),
                )
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

    #[test]
    fn finds_a_comparison_with_a_literal() {
        let src = "pub fn is_eligible(age: u32) -> bool {\n    age >= 18\n}\n";
        let found = discover(src, 5).unwrap();
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
        let literals: Vec<LiteralSide> = discover(src, 0)
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
        let sites = discover(src, 3).unwrap().sites;
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
        let sites = discover(src, 0).unwrap().sites;
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
        let found = discover(src, 0).unwrap();
        let texts: Vec<&str> = found.sites.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "a == 100",
                "a == 101",
                "matches!(a, 1..=2) || match a { 0..=9 => a == 102, _ => false }",
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
        let texts: Vec<String> = discover(src, 0)
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
        let sites = discover(src, 0).unwrap().sites;
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
        let sites = discover(src, 0).unwrap().sites;
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
        let texts: Vec<String> = discover(src, 0)
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
        let found = discover(src, 0).unwrap().sites;
        let kinds: Vec<String> = found
            .iter()
            .map(|s| match &s.kind {
                SiteKind::Logic { and, lhs, rhs, .. } => format!("logic and={and} [{lhs}] [{rhs}]"),
                SiteKind::Arith { op, lhs, rhs, .. } => {
                    format!("arith {} [{lhs}] [{rhs}]", op.symbol())
                }
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "logic and=false [a && b] [!a]",
                "logic and=true [a] [b]",
                "arith + [x] [y * 2]",
                "arith * [y] [2]",
                "arith - [x] [1 % y]",
                "arith % [1] [y]",
            ]
        );
        let out = render(src, &found, &all).text;
        syn::parse_file(&out).expect("instrumented source parses");
        assert!(out.contains("({ let __tmt_c = crate::__tmt_rt::lhs(1u32, true, a); if __tmt_c == 2u8 { crate::__tmt_rt::rhs(1u32, true, b) } else { __tmt_c == 1u8 } })"), "{out}");
        assert!(out.contains("use crate::__tmt_rt::{AnyMul as _, ArithPrim as _}; (&crate::__tmt_rt::Ar::new(y, 2)).tmt_mul(3u32) })"), "{out}");
        for op in [
            ArithOp::Add,
            ArithOp::Sub,
            ArithOp::Mul,
            ArithOp::Div,
            ArithOp::Rem,
        ] {
            assert_ne!(op.swapped(), op);
            assert!(
                crate::RUNTIME_SOURCE.contains(&format!("fn {}(", op.method())),
                "{op:?}"
            );
            assert!(
                crate::RUNTIME_SOURCE.contains(&format!(
                    "{} {} ",
                    op.fallback_trait(),
                    op.method()
                )),
                "{op:?}"
            );
        }
    }

    #[test]
    fn let_chains_literal_arithmetic_and_bit_operators_are_left_alone() {
        let src = r#"
            fn f(o: Option<u8>, a: bool, n: u32) -> u32 {
                if let Some(x) = o && a && x > 1 { return 1; }
                let k = 60 * 60;
                (n & 3) | (n << 1) ^ k
            }
        "#;
        let texts: Vec<String> = discover(src, 0)
            .unwrap()
            .sites
            .into_iter()
            .map(|s| s.text)
            .collect();
        assert_eq!(texts, ["x > 1"]);
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
        let texts: Vec<String> = discover(src, 0)
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
        let found = discover(src, 0).unwrap().sites;
        syn::parse_file(&render(src, &found, &all).text).expect("instrumented source parses");
    }

    #[test]
    fn non_ascii_source_keeps_byte_offsets() {
        let src = "fn f(a: u8) -> bool { let s = \"日本語\"; s.len() > 3 && a < 2 }";
        let sites = discover(src, 0).unwrap().sites;
        let texts: Vec<&str> = sites.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["s.len() > 3 && a < 2", "s.len() > 3", "a < 2"]);
        syn::parse_file(&render(src, &sites, &all).text).unwrap();
    }

    #[test]
    fn unparseable_source_is_an_error() {
        assert!(discover("fn f( {", 0).is_err());
    }
}
