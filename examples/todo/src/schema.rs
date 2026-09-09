//! The model: what a row is, and what the tables are.
//!
//! `schema.sql` is what `migrate` runs and what `petros-sql` prepares every
//! statement in [`functions`](crate::functions) against at build time. The rows
//! below are what those statements read back.
//!
//! Behind the `storage` feature, because the wasm build applies mutations and
//! never reads a row.

use petros::Id;

/// One row of the materialised view. Read-only: rows are produced by `apply`,
/// never by this crate.
#[derive(Debug, Clone)]
pub struct Item {
    pub id: Id,
    pub text: String,
    pub done: bool,
    pub pos: i64,
    pub created_ms: i64,
    pub actor: String,
}

/// The one description of this app's tables.
pub const SCHEMA: &str = include_str!("../schema.sql");

/// Sixteen bytes out of a BLOB column. A row whose id is not sixteen bytes did
/// not come from a mutation, and there is nothing useful to do with it.
pub(crate) fn id_of(bytes: &[u8]) -> Id {
    Id(petros::uuid::Uuid::from_slice(bytes).unwrap_or(petros::uuid::Uuid::nil()))
}
