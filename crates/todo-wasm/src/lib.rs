//! The domain, compiled to wasm for the phone.
//!
//! There is no logic here, and now barely any code. `apply` and `fill_auto`
//! live in `crates/todo`, which the server and the terminal peers link
//! directly; this crate exists only to give that same code a [`Host`] made of
//! imported functions and expose it across the ABI the interpreter calls.
//!
//! Which is what makes the arrangement honest: the phone runs a file it can
//! replace without a rebuild, and it is the same file, from the same source,
//! that every other peer compiled in.
//!
//! [`Host`]: petros_schema::Host

petros_wasm_guest::export!(todo::domain, todo::schema::SCHEMA_TEXT);
