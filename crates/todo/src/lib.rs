//! The demo to-do list's *read* model: the table, the row, and how to order it.
//!
//! The write model is not here. `apply` — the one thing every replica must
//! agree on — lives in `crates/todo-wasm` and is compiled to a wasm module that
//! the server, the terminal peers, the iced window and the phone all interpret.
//! One artifact, so there is nothing to keep in step.
//!
//! Reads stay in Rust because nothing depends on them being identical
//! everywhere: a query is a way of looking at state, not a way of producing it.
//! Diesel's `check_for_backend` still holds this side to the schema, which is
//! the check the module's hand-written SQL gives up.

use diesel::prelude::*;
use diesel::sqlite::Sqlite;
use exo::{Connection, Id};

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
#[diesel(table_name = todo, check_for_backend(Sqlite))]
pub struct Item {
    pub id: Id,
    pub text: String,
    pub done: bool,
    pub pos: i64,
    pub created_ms: i64,
    pub actor: String,
}

/// Always ordered explicitly.
pub fn list(conn: &mut Connection) -> exo::Result<Vec<Item>> {
    Ok(todo::table
        .select(Item::as_select())
        .order((todo::pos.asc(), todo::id.asc()))
        .load(conn)?)
}
