//! Replacement value synthesis for function body and return mutations.

use proc_macro2::TokenStream;
use quote::ToTokens;
use syn::visit::{self, Visit};

use super::discover::has_ident;

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

pub(super) fn iterator_type_values(
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
pub(super) fn squeezed(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// The tokens of an expression, to compare two ways of writing it: the white space
/// between them does not count, the one inside a literal does (`" "` is not `""`).
pub(super) fn tokens_of(text: &str) -> String {
    match text.parse::<TokenStream>() {
        Ok(tokens) => tokens.to_string(),
        Err(_) => text.to_string(),
    }
}

/// The result probe runs after the original body has evaluated. Early exits and
/// unexpanded macros in that body's control-flow scope could bypass it.
pub(super) fn may_bypass_result_probe(body: &syn::Block) -> bool {
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
pub(super) fn does_nothing_else(expr: &syn::Expr) -> bool {
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

pub(super) fn body_values_with_errors(
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
