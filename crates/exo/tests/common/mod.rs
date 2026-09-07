//! Test support: a toy to-do domain and a deterministic simulation harness.
//!
//! A toy domain, and deliberately not part of the crate's public API — it
//! exists to prove Exo is app-agnostic.

// Each integration test binary uses a different slice of this module.
#![allow(dead_code)]

pub mod sim;
pub mod todo;
