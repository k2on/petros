//! The model: what a row is, and what the tables are.
//!
//! `schema.sql` is what `migrate` runs and what `petros-sql` prepares every
//! statement in [`functions`](crate::functions) against at build time. The rows
//! below are what those statements read back.
//!
//! Behind the `storage` feature, because the wasm build applies mutations and
//! never reads a row.

// A row type per table, generated from `schema.sql` — the same file every
// statement is checked against and the one `migrate` runs. The tables were
// described twice before, once as DDL and once in Rust, with a test to hold
// them together. SQLite already parsed the DDL to check the SQL, so it can say
// what is in it and there is nothing to keep in step.
petros_sql::tables!();

/// The one description of this app's tables.
pub const SCHEMA: &str = include_str!("../schema.sql");

/// One row of the materialised view, as a reader wants it: an id rather than
/// sixteen bytes. Behind `storage`, like the read model that produces it.
#[cfg(feature = "storage")]
#[derive(Debug, Clone)]
pub struct Item {
    pub id: petros_schema::Id<Todo>,
    pub text: String,
    pub done: bool,
    pub pos: i64,
    pub created_ms: i64,
    pub actor: String,
}

// An id no longer needs recovering from a column: `tables!` types a key column
// as the `Id<Todo>` it is, so `Item.id` is the row's id rather than a
// conversion of it.
