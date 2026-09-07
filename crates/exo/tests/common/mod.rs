//! Test support: a toy to-do domain and a deterministic simulation harness.
//!
//! Deliberately not musical and deliberately not part of the crate's public
//! API — it exists to prove Exo is app-agnostic.


pub mod todo;

#[allow(unused_imports)]
pub use todo::{Todo, TodoMutation};
