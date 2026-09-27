//! seal-core — domain logic for the seal distributed code review tool.
//!
//! This crate owns event model, append-log storage, projection queries,
//! SCM abstraction, and shared domain types.

pub mod approval_exempt;
pub mod core;
pub mod events;
pub mod jj;
pub mod log;
pub mod projection;
pub mod range;
pub mod scm;
pub mod sealignore;
pub mod version;
