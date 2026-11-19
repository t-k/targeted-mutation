//! Targeted mutation testing: find the wrong implementations your tests would let through.

pub mod plan;
pub mod trace;

/// The runtime injected into the crate under test, compiled here so that its logic can be
/// checked against the planner.
#[cfg(test)]
#[path = "runtime/tmt_rt.rs"]
mod tmt_rt;

/// Source of the injected runtime module.
pub const RUNTIME_SOURCE: &str = include_str!("runtime/tmt_rt.rs");
