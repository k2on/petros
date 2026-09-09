//! The worked example: a to-do list, in two files.
//!
//! [`schema`] is the model — what a row is, and what the tables are.
//! [`functions`] is every mutation and every query, each written once as an
//! ordinary Rust function. Everything else on this page is generated from them.
//!
//! The server and the linked peers call these directly. The same functions
//! compile to wasm, so `petros-wasm-host`'s conformance
//! test can drive every verb through both builds and compare the rows.
//!
//! The `storage` feature is what the wasm build turns off. It has no SQLite of
//! its own, only a channel to the host's, so it wants the mutations and none of
//! the rest.

pub mod functions;
/// The model. Only where there is a database: the sandbox applies mutations and
/// never reads a row back.
#[cfg(feature = "storage")]
pub mod schema;

pub use functions::*;
#[cfg(feature = "storage")]
pub use schema::*;

// The module a peer loads, and the whole of what a separate `todo-wasm` crate
// used to be.
//
// `not(feature = "storage")` is doing real work, not tidiness. There are two
// wasm builds of this crate: the mutator module, built with
// `--no-default-features`, and the browser client, which links it with storage
// on. Only the first should carry the guest ABI — its imports come from a module
// called `petros` that a host supplies, and in a browser there is no host, so
// wasm-bindgen emits `import * as … from "petros"` and the page dies on a bare
// specifier before it renders anything.
//
// The module is by definition the storage-less build, so that is the condition.
#[cfg(all(target_arch = "wasm32", not(feature = "storage")))]
petros_wasm_guest::export!(functions);

#[cfg(feature = "storage")]
petros::app!(TodoApp {
    schema: crate::schema::SCHEMA,
    apply: crate::functions::apply,
    fill_auto: crate::functions::fill_auto,
});
