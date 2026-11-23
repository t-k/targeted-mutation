//! Targeted mutation testing: find the wrong implementations your tests would let through.

pub mod baseline;
mod campaign_identity;
mod checkpoint;
pub mod driver;
mod git_index;
pub mod instrument;
mod leftovers;
mod modules;
pub mod output_fixture;
pub mod plan;
mod progress;
pub mod propagation;
pub mod report;
mod reverts;
pub mod scope;
pub mod trace;
mod witness;
mod work_lock;

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
