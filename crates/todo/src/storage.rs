//! Everything that needs a real SQLite: the table, the rows, the Diesel host
//! and the `petros::App` the native peers run.
//!
//! Behind the `storage` feature, because the wasm build wants
//! [`domain`](crate::domain) and none of this.

use ciborium::value::Value;
use diesel::prelude::*;
use diesel::sqlite::Sqlite as SqliteBackend;
use petros::backend::SqliteStore;
use petros::{ActorId, App, AutoCtx, Connection, Id, Mutation, MutationError, Transaction};
use serde::{Deserialize, Serialize};

diesel::table! {
    todo (id) {
        id -> Binary,
        text -> Text,
        done -> Bool,
        pos -> BigInt,
        created_ms -> BigInt,
        actor -> Text,
    }
}

/// One row of the materialised view. Read-only: rows are produced by the
/// module's `apply`, never by this crate.
#[derive(Debug, Clone, Queryable, Selectable)]
#[diesel(table_name = todo, check_for_backend(SqliteBackend))]
pub struct Item {
    pub id: Id,
    pub text: String,
    pub done: bool,
    pub pos: i64,
    pub created_ms: i64,
    pub actor: String,
}

/// Always ordered explicitly.
pub fn list(conn: &mut Connection) -> petros::Result<Vec<Item>> {
    Ok(todo::table
        .select(Item::as_select())
        .order((todo::pos.asc(), todo::id.asc()))
        .load(conn)?)
}

// ---------------------------------------------------------------- the writes

/// One mutation, as the bytes the log stores.
///
/// Not a Rust enum mirroring the variants, and that is deliberate: a peer that
/// has never heard of a variant still carries it through the log intact, and
/// applies it as soon as it has a build that knows what it means.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Payload(pub Value);

/// The `petros::App`: this schema, and mutations that write through a checked
/// store.
impl Mutation for Payload {
    fn fill_auto(&mut self, ctx: &mut AutoCtx) {
        let uuid = ctx.uuid().as_uuid().as_bytes().to_vec();
        crate::domain::fill_auto(&mut self.0, uuid, ctx.now_ms());
    }

    fn apply(&self, tx: &mut Transaction, actor: &ActorId) -> Result<(), MutationError> {
        crate::domain::apply(&mut SqliteStore(tx.conn()), &self.0, actor.as_str())
            .map_err(MutationError::rejected)
    }
}

/// The app: Petros's tables plus this one.
pub struct TodoApp;

impl App for TodoApp {
    type Mutation = Payload;
    const SCHEMA: &'static str = SCHEMA;
}

/// The one description of this app's tables.
///
/// `migrate` runs it, and `petros-sql` prepares every statement in the domain
/// against it at build time. There is no second copy to drift from.
pub const SCHEMA: &str = include_str!("../schema.sql");

// ------------------------------------------------------------------ authoring

/// Author a mutation, as [`petros_schema::author`] does it, wrapped in this
/// app's payload type.
///
/// The conversion is protocol rather than domain — the verb goes in `t`, and a
/// field named `id` or ending `_id` becomes the sixteen bytes the log uses — so
/// it lives in the schema crate, beside the macro that declares the verbs and
/// the generator that emits the TypeScript calling them.
pub fn from_value(kind: &str, args: serde_json::Value) -> Result<Payload, String> {
    petros_schema::author::from_value(kind, args).map(Payload)
}

/// As [`from_value`], for a caller that has the arguments as JSON text — which
/// is every foreign one, since it has no CBOR encoder.
pub fn from_json(kind: &str, args_json: &str) -> Result<Payload, String> {
    petros_schema::author::from_json(kind, args_json).map(Payload)
}

// The three verbs the Rust peers spell out. Conveniences over [`from_value`],
// not a second encoder. The `expect`s cannot fire: the only fallible step is
// parsing a uuid, and these format one rather than taking it from a caller.

pub fn add(text: &str) -> Payload {
    from_value("Add", serde_json::json!({ "text": text })).expect("a text is always encodable")
}

pub fn set_done(id: &[u8; 16], done: bool) -> Payload {
    let id = petros::uuid::Uuid::from_bytes(*id).to_string();
    from_value("SetDone", serde_json::json!({ "id": id, "done": done }))
        .expect("a formatted uuid always parses")
}

pub fn remove(id: &[u8; 16]) -> Payload {
    let id = petros::uuid::Uuid::from_bytes(*id).to_string();
    from_value("Remove", serde_json::json!({ "id": id })).expect("a formatted uuid always parses")
}
