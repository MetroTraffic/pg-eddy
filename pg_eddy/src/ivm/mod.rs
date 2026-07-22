//! Incremental graph-view integration.
//!
//! The custom AM remains the authoritative graph store. Typed heap mirrors
//! expose logical node and edge rows to PostgreSQL trigger machinery and to
//! the optional pg_trickle extension.

pub mod api;
pub mod catalog;
pub mod mirror;
pub mod pg_trickle;
