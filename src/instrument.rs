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
    /// A call or assignment statement that can be deleted.
    Stmt,
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

#[derive(Debug, Default)]
pub struct Discovery {
    pub sites: Vec<Site>,
    /// Out-of-line modules that only exist under `cfg(test)` (`#[cfg(test)] mod tests;`).
    pub test_modules: Vec<String>,
}

/// Finds the sites in one source file, numbering them from `first_id`.
pub fn discover(source: &str, first_id: u32) -> Result<Discovery> {
    let file = syn::parse_file(source).context("not parseable as a Rust source file")?;
    let mut finder = Finder { source, next_id: first_id, found: Discovery::default() };
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
        (range.start < range.end && range.end <= self.source.len()
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

fn is_assign_op(op: &syn::BinOp) -> bool {
    use syn::BinOp::*;
    matches!(
        op,
        AddAssign(_) | SubAssign(_) | MulAssign(_) | DivAssign(_) | RemAssign(_) | BitXorAssign(_)
            | BitAndAssign(_) | BitOrAssign(_) | ShlAssign(_) | ShrAssign(_)
    )
}

fn int_literal(expr: &syn::Expr) -> Option<i128> {
    match expr {
        syn::Expr::Paren(e) => int_literal(&e.expr),
        syn::Expr::Group(e) => int_literal(&e.expr),
        syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Int(lit), .. }) => lit.base10_parse().ok(),
        syn::Expr::Unary(syn::ExprUnary { op: syn::UnOp::Neg(_), expr, .. }) => {
            int_literal(expr).and_then(i128::checked_neg)
        }
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
        let syn::Meta::List(list) = &attr.meta else { return false };
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
    fn visit_macro(&mut self, _: &'ast syn::Macro) {}

    fn visit_expr_repeat(&mut self, node: &'ast syn::ExprRepeat) {
        self.visit_expr(&node.expr); // the length is a const expression
    }

    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        if is_test_only(&node.attrs) {
            if node.content.is_none() {
                self.found.test_modules.push(node.ident.to_string());
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
        }
        visit::visit_expr_binary(self, node);
    }
}

/// Instrumented text plus the mapping back to the original, used to blame a site for a
/// compile error in the rewritten file.
#[derive(Debug, PartialEq)]
pub struct Rendered {
    pub text: String,
    /// `(output offset, original offset, length, copied)` per segment, in output order.
    segments: Vec<(usize, usize, usize, bool)>,
}

impl Rendered {
    /// Maps a byte offset in the instrumented text to the original. Offsets inside inserted
    /// text map to the point of insertion.
    pub fn to_original(&self, offset: usize) -> usize {
        let idx = self.segments.partition_point(|(out, _, _, _)| *out <= offset);
        let Some((out, orig, len, copied)) = idx.checked_sub(1).map(|i| self.segments[i]) else {
            return 0;
        };
        if copied { orig + (offset - out).min(len) } else { orig }
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
                edits.push(Edit { pos: gap.0, end: gap.1, phase: 1, order: 0, text: "), &(".into() });
                (
                    "(match (&(".to_string(),
                    format!(
                        ")) {{ (__tmt_l, __tmt_r) => {{ #[allow(unused_imports)] use {rt}::{{ValAny as _, ValInt as _}}; \
                         {rt}::{func}({id}u32, {code}u8, __tmt_l, __tmt_r, (&{rt}::W(__tmt_l)).tmt_val(), (&{rt}::W(__tmt_r)).tmt_val()) }} }})",
                        id = site.id,
                        code = op.code(),
                    ),
                )
            }
            SiteKind::Stmt => (format!("if {rt}::live({}u32) {{ ", site.id), " }".to_string()),
        };
        edits.push(Edit { pos: site.start, end: site.start, phase: 2, order: -len, text: open });
        edits.push(Edit { pos: site.end, end: site.end, phase: 0, order: len, text: close });
    }
    edits.sort_by_key(|e| (e.pos, e.phase, e.order));

    let mut text = String::with_capacity(source.len() + edits.iter().map(|e| e.text.len()).sum::<usize>());
    let mut segments = vec![];
    let mut cursor = 0;
    for edit in edits {
        if edit.pos > cursor {
            segments.push((text.len(), cursor, edit.pos - cursor, true));
            text.push_str(&source[cursor..edit.pos]);
        }
        segments.push((text.len(), edit.pos, edit.text.len(), false));
        text.push_str(&edit.text);
        cursor = cursor.max(edit.end);
    }
    segments.push((text.len(), cursor, source.len() - cursor, true));
    text.push_str(&source[cursor..]);
    Rendered { text, segments }
}

/// The sites to blame for a compile error at `offset` (original coordinates): the smallest
/// enabled sites that contain it.
pub fn blame(sites: &[Site], enabled: &dyn Fn(u32) -> bool, offset: usize) -> Vec<u32> {
    let containing = || sites.iter().filter(|s| enabled(s.id) && s.start <= offset && offset <= s.end);
    let Some(smallest) = containing().map(|s| s.end - s.start).min() else {
        return vec![];
    };
    containing().filter(|s| s.end - s.start == smallest).map(|s| s.id).collect()
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
        assert_eq!((site.id, site.line, site.column, site.text.as_str()), (5, 2, 5, "age >= 18"));
        let SiteKind::Cmp { op, lhs, rhs, literal, .. } = &site.kind else { panic!() };
        assert_eq!((*op, lhs.as_str(), rhs.as_str(), *literal), (CmpOp::Ge, "age", "18", LiteralSide::Rhs(18)));
    }

    #[test]
    fn literal_sides() {
        let src = "fn f(x: i32, y: i32) { let _ = (-5 < x, x == (3), 1 < 2, x <= y, x > 1.5 as i32); }";
        let literals: Vec<LiteralSide> = discover(src, 0)
            .unwrap()
            .sites
            .iter()
            .map(|s| match &s.kind {
                SiteKind::Cmp { literal, .. } => *literal,
                SiteKind::Stmt => panic!(),
            })
            .collect();
        assert_eq!(
            literals,
            [LiteralSide::Lhs(-5), LiteralSide::Rhs(3), LiteralSide::None, LiteralSide::None, LiteralSide::None]
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
             use crate::__tmt_rt::{ValAny as _, ValInt as _}; crate::__tmt_rt::ord(3u32, 3u8, __tmt_l, __tmt_r, \
             (&crate::__tmt_rt::W(__tmt_l)).tmt_val(), (&crate::__tmt_rt::W(__tmt_r)).tmt_val()) } }) }"
        );
        assert_eq!(render(src, &sites, &|_| false).text, src);
    }

    #[test]
    fn renders_statement_guards_and_nested_sites() {
        let src = "fn f(v: &mut Vec<u8>, a: u8) -> Result<(), ()> {\n    check((a < 3) == true)?;\n    v.push(a);v.clear();\n    Ok(())\n}";
        let sites = discover(src, 0).unwrap().sites;
        let kinds: Vec<&str> = sites.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(kinds, ["check((a < 3) == true)?;", "v.push(a);", "v.clear();", "(a < 3) == true", "a < 3"]);
        let out = render(src, &sites, &all);
        assert_eq!(out.text.lines().count(), src.lines().count(), "line numbers are preserved");
        assert!(out.text.contains("if crate::__tmt_rt::live(0u32) { check((match (&(((match (&(a), &(3))"), "{}", out.text);
        assert!(out.text.contains("if crate::__tmt_rt::live(1u32) { v.push(a); }if crate::__tmt_rt::live(2u32) { v.clear(); }"), "{}", out.text);
        syn::parse_file(&out.text).expect("instrumented source parses");

        // Only the inner comparison enabled.
        let inner = render(src, &sites, &|id| id == 4);
        assert!(inner.text.contains("check(((match (&(a), &(3))"), "{}", inner.text);
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
        assert_eq!(texts, ["a == 100", "a == 101", "a == 102"]);
        assert_eq!(found.test_modules, ["more_tests"]);
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
            ["s.validate()?;", "s.total += 1;", "s.flag = true;", "helper(s);", "s.flush().await?;", "s.sync().await;"]
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
            assert!(!hits.is_empty(), "original offset {orig} ({ch}) is unreachable");
        }
        // An error inside the probe of `a < b` blames it, not the enclosing statement.
        let probe = out.text.find("ord(1u32").unwrap();
        let orig = out.to_original(probe);
        assert_eq!(blame(&sites, &all, orig), [1]);
        // With the inner site disabled, the statement is next.
        assert_eq!(blame(&sites, &|id| id != 1, orig), [0]);
        assert_eq!(blame(&sites, &all, src.find("fn").unwrap()), Vec::<u32>::new());
        assert_eq!(out.to_original(0), 0);
        assert_eq!(out.to_original(out.text.len()), src.len());
    }

    #[test]
    fn non_ascii_source_keeps_byte_offsets() {
        let src = "fn f(a: u8) -> bool { let s = \"日本語\"; s.len() > 3 && a < 2 }";
        let sites = discover(src, 0).unwrap().sites;
        let texts: Vec<&str> = sites.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, ["s.len() > 3", "a < 2"]);
        syn::parse_file(&render(src, &sites, &all).text).unwrap();
    }

    #[test]
    fn unparseable_source_is_an_error() {
        assert!(discover("fn f( {", 0).is_err());
    }
}
