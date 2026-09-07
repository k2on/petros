//! Test support: a toy to-do domain and a deterministic simulation harness.
//!
//! Deliberately not musical and deliberately not part of the crate's public
//! API — it exists to prove Exo is app-agnostic.

// Each integration test binary uses a different slice of this module.
#![allow(dead_code)]

pub mod todo;

pub use todo::{Todo, TodoMutation};
