//! Finds mutation sites in Rust source and rewrites them into probes.
//!
//! Every site stays in the compiled code (mutant schemata): a comparison `a >= b` becomes a
//! call that records what it saw and, when a mutant is switched on, returns the mutated
//! result. One build serves the trace run and every mutant.
//!
//! The rewrite only inserts text on the line where the site is, so line numbers in compiler
//! diagnostics still match the original file.

mod body_values;
mod cfg;
mod discover;
mod items;
mod model;
mod render;
mod source_mutants;
mod tokens;

pub use body_values::{body_values, iterator_values};
#[cfg(test)]
use cfg::cfg_value;
pub(crate) use cfg::{
    cfg_possible_without_test, inactive_ranges, is_skipped, is_test_only, reach,
    settled_cfg_attributes, test_only_ranges,
};
pub use discover::discover;
#[allow(unused_imports)]
pub(crate) use discover::{discover_parsed, discover_with_errors, parse_source};
pub(crate) use items::ItemIndex;
pub use items::{NamedItem, enclosing_item, named_items, offset_of};
pub use model::{
    ArithOp, ArmGuard, Discovery, LiteralSide, NO_ID, RUNTIME_MOD, Site, SiteKind, TestModule,
};
pub use render::{Rendered, blame, render};
pub use source_mutants::{SourceMutant, keep_line_ends, source_mutants};
pub(crate) use source_mutants::{literal_operator_mutants, source_boundary_mutant};
pub(crate) use tokens::{LEFT_OUT, same_built_tokens, same_tokens};

#[cfg(test)]
mod tests;
