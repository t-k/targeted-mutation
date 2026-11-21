//! Targeted mutation testing: find the wrong implementations your tests would let through.

pub mod baseline;
pub mod driver;
mod git_index;
pub mod instrument;
pub mod plan;
mod progress;
pub mod propagation;
pub mod report;
pub mod scope;
pub mod trace;
mod witness;

/// The runtime injected into the crate under test, compiled here so that its logic can be
/// checked against the planner.
// The runtime reaches `std` through this alias (see `runtime/tmt_rt.rs`). It is declared
// unconditionally so that the tool can also be run on itself.
#[doc(hidden)]
extern crate std as __tmt_std;

#[cfg(test)]
#[path = "runtime/tmt_rt.rs"]
mod tmt_rt;

/// Source of the injected runtime module.
pub const RUNTIME_SOURCE: &str = include_str!("runtime/tmt_rt.rs");
