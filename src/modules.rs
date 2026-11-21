//! Resolve explicit Rust module declarations from a Cargo target's source root.

use std::path::{Path, PathBuf};

use syn::ext::IdentExt;
use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::{Attribute, LitStr, Meta, Token};

use crate::instrument::{cfg_possible_without_test, is_skipped, is_test_only};

pub(crate) struct Module {
    pub(crate) paths: Vec<PathBuf>,
    pub(crate) directory: PathBuf,
    pub(crate) production: bool,
}

fn production(attrs: &[Attribute]) -> bool {
    !is_skipped(attrs) && !is_test_only(attrs)
}

pub(crate) fn external(source: &str, file: &Path, directory: &Path) -> Vec<Module> {
    // An explicit module or a recognized include must contain its keyword or macro name.
    // Matches in comments and literals still go through the parser.
    if !source.contains("mod") && !source.contains("include") {
        return vec![];
    }
    let Ok(parsed) = syn::parse_file(source) else {
        return vec![];
    };
    struct Modules<'a> {
        file_dir: &'a Path,
        inline: Vec<PathBuf>,
        directory: &'a Path,
        active: bool,
        result: Vec<Module>,
    }

    impl Modules<'_> {
        fn visit_in(&mut self, attrs: &[Attribute], child: impl FnOnce(&mut Self)) {
            let previous = self.active;
            self.active &= production(attrs);
            child(self);
            self.active = previous;
        }
    }

    macro_rules! visit_with_attrs {
        ($method:ident, $ty:ident) => {
            fn $method(&mut self, node: &'ast syn::$ty) {
                self.visit_in(&node.attrs, |finder| visit::$method(finder, node));
            }
        };
    }

    impl<'ast> Visit<'ast> for Modules<'_> {
        visit_with_attrs!(visit_item_const, ItemConst);
        visit_with_attrs!(visit_item_fn, ItemFn);
        visit_with_attrs!(visit_item_impl, ItemImpl);
        visit_with_attrs!(visit_item_macro, ItemMacro);
        visit_with_attrs!(visit_item_static, ItemStatic);
        visit_with_attrs!(visit_item_trait, ItemTrait);
        visit_with_attrs!(visit_impl_item_const, ImplItemConst);
        visit_with_attrs!(visit_impl_item_fn, ImplItemFn);
        visit_with_attrs!(visit_trait_item_const, TraitItemConst);
        visit_with_attrs!(visit_trait_item_fn, TraitItemFn);

        fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
            let mut alternatives = Vec::new();
            let mut default = true;
            let mut defaults_in_production = true;
            for attr in &node.attrs {
                default &= !attr.path().is_ident("path");
                defaults_in_production &=
                    !path_alternatives(&attr.meta, true, true, &mut alternatives);
            }
            if default {
                alternatives.push((None, defaults_in_production));
            }
            let name = node.ident.unraw().to_string();
            self.visit_in(&node.attrs, |finder| {
                for (named, production) in alternatives {
                    if node.content.is_some() {
                        finder
                            .inline
                            .push(named.unwrap_or_else(|| PathBuf::from(&name)));
                        let active = finder.active;
                        finder.active &= production;
                        visit::visit_item_mod(finder, node);
                        finder.active = active;
                        finder.inline.pop();
                        continue;
                    }

                    let base = if named.is_some() && finder.inline.is_empty() {
                        finder.file_dir.to_path_buf()
                    } else {
                        let mut base = finder.directory.to_path_buf();
                        for part in &finder.inline {
                            base.push(part);
                        }
                        base
                    };
                    let (paths, directory) = if let Some(relative) = named {
                        let file = base.join(relative);
                        let directory = file.parent().unwrap_or(&base).to_path_buf();
                        (vec![file], directory)
                    } else {
                        let directory = base.join(&name);
                        (
                            vec![base.join(format!("{name}.rs")), directory.join("mod.rs")],
                            directory,
                        )
                    };
                    finder.result.push(Module {
                        paths,
                        directory,
                        production: finder.active && production,
                    });
                }
            });
        }

        fn visit_macro(&mut self, node: &'ast syn::Macro) {
            if node.path.is_ident("include")
                && let Ok(path) = node.parse_body::<LitStr>()
            {
                let file = self.file_dir.join(path.value());
                let directory = file.parent().unwrap_or(self.file_dir).to_path_buf();
                self.result.push(Module {
                    paths: vec![file],
                    directory,
                    production: self.active,
                });
            }
        }
    }

    let mut modules = Modules {
        file_dir: file.parent().unwrap_or(Path::new(".")),
        inline: vec![],
        directory,
        active: production(&parsed.attrs),
        result: vec![],
    };
    modules.visit_file(&parsed);
    modules.result
}

// Record every potential path, along with whether the non-test build can
// select it. Return whether this attribute overrides the default in all
// non-test builds.
fn path_alternatives(
    meta: &Meta,
    possible: bool,
    certain: bool,
    paths: &mut Vec<(Option<PathBuf>, bool)>,
) -> bool {
    match meta {
        Meta::NameValue(value) if value.path.is_ident("path") => {
            if let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(path),
                ..
            }) = &value.value
            {
                paths.push((Some(PathBuf::from(path.value())), possible));
                return certain;
            }
        }
        Meta::List(list) if list.path.is_ident("cfg_attr") => {
            if let Ok(parts) = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
                && let Some(condition) = parts.first()
            {
                let possible = possible && cfg_possible_without_test(condition, true);
                let certain = certain && !cfg_possible_without_test(condition, false);
                let mut overrides = false;
                for attr in parts.iter().skip(1) {
                    overrides |= path_alternatives(attr, possible, certain, paths);
                }
                return overrides;
            }
        }
        _ => {}
    }
    false
}

#[cfg(test)]
#[test]
fn module_edges_match_the_coverage_table() {
    for row in include_str!("../tests/data/shared-source-modules.pict.tsv")
        .lines()
        .skip(1)
    {
        let fields: Vec<_> = row.split('\t').collect();
        let [kind, visible, location] = fields.as_slice() else {
            panic!("invalid PICT row: {row}");
        };
        let contents = match *kind {
            "absent" => "fn sample() { let π = 1; }",
            "default" => "mod child;",
            "raw" => "mod r#type;",
            "path" => r#"#[path = "generated.rs"] mod child;"#,
            "include" => r#"include!(r"extra.rs");"#,
            "macro" => "macro_rules! sample { () => { mod hidden; include!(\"hidden.rs\"); } }",
            _ => panic!("{row}"),
        };
        let attr = if *visible == "test-only" {
            "#[cfg(test)]"
        } else {
            ""
        };
        let source = match *location {
            "top" => format!("{}\n{contents}", attr.replace("#[", "#![")),
            "inline" => format!("{attr} mod outer {{ {contents} }}"),
            "function" => format!("{attr} fn wrapper() {{ {contents} }}"),
            _ => panic!("{row}"),
        };
        let file = Path::new("src/nested.rs");
        let directory = Path::new("src/nested");
        let edges = external(&source, file, directory);
        if matches!(*kind, "absent" | "macro") {
            assert!(edges.is_empty(), "{row}");
            continue;
        }
        let base = if *location == "inline" {
            directory.join("outer")
        } else {
            directory.to_path_buf()
        };
        let paths = match *kind {
            "default" => vec![base.join("child.rs"), base.join("child/mod.rs")],
            "raw" => vec![base.join("type.rs"), base.join("type/mod.rs")],
            "path" if *location == "inline" => vec![base.join("generated.rs")],
            "path" => vec![PathBuf::from("src/generated.rs")],
            "include" => vec![PathBuf::from("src/extra.rs")],
            _ => panic!("{row}"),
        };
        assert_eq!(edges.len(), 1, "{row}");
        assert_eq!(edges[0].paths, paths, "{row}");
        assert_eq!(edges[0].production, *visible == "production", "{row}");
    }
}
