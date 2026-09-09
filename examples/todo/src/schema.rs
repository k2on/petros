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
    pub id: petros::Id,
    pub text: String,
    pub done: bool,
    pub pos: i64,
    pub created_ms: i64,
    pub actor: String,
}

/// Sixteen bytes out of a BLOB column. A row whose id is not sixteen bytes did
/// not come from a mutation, and there is nothing useful to do with it.
#[cfg(feature = "storage")]
pub(crate) fn id_of(bytes: &[u8]) -> petros::Id {
    petros::Id(petros::uuid::Uuid::from_slice(bytes).unwrap_or(petros::uuid::Uuid::nil()))
}
