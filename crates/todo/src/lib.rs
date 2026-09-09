//! The worked example: a to-do list, in two files.
//!
//! [`storage`] is the model — what a row is, and what the tables are.
//! [`functions`] is every mutation and every query, each written once as an
//! ordinary Rust function. Everything else on this page is generated from them.
//!
//! The server and the linked peers call these directly. `crates/todo-wasm`
//! compiles the same functions to wasm, so `petros-wasm-host`'s conformance
//! test can drive every verb through both builds and compare the rows.
//!
//! The `storage` feature is what the wasm build turns off. It has no SQLite of
//! its own, only a channel to the host's, so it wants the mutations and none of
//! the rest.

pub mod functions;
#[cfg(feature = "storage")]
pub mod storage;

pub use functions::*;
#[cfg(feature = "storage")]
pub use storage::*;

#[cfg(feature = "storage")]
petros::app!(TodoApp {
    schema: crate::storage::SCHEMA,
    apply: crate::functions::apply,
    fill_auto: crate::functions::fill_auto,
});
