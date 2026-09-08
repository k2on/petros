//! The demo to-do list: the table, the rows, and the mutations that produce
//! them.
//!
//! [`domain`] holds `apply` and `fill_auto` and knows nothing about where it
//! runs. This module gives it a [`Host`](domain::Host) backed by Diesel, which
//! is what the server, the terminal peers and the iced window use — they are
//! ordinary Rust programs and a mutation is an ordinary function call.
//!
//! The phone is the exception. It loads the same domain compiled to wasm
//! (`crates/todo-wasm`) so a new mutation reaches it over Metro without a
//! native build. Two builds of one source, held to that by
//! `tests/conformance.rs`, which runs the same mutations through both and
//! compares the rows.
//!
//! The `storage` feature is what the wasm build turns off: it has no SQLite of
//! its own, only a channel to the host's, so it wants the domain and none of
//! this.

pub mod domain;
pub mod schema;

#[cfg(feature = "storage")]
mod storage;
#[cfg(feature = "storage")]
pub use storage::*;
