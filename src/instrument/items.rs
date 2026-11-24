//! Named item index for stable finding identifiers.

use proc_macro2::Span;
use quote::ToTokens;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};

use super::body_values::squeezed;

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
