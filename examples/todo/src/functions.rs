//! Every mutation and every query, each written once.
//!
//! A mutation is an *intent* — "put this at the end of the list", not "put this
//! at position 3" — and it is applied by every replica from the log, not here.
//! `crates/petros-wasm-host/tests/conformance.rs` drives every verb through the
//! linked build and the wasm one and compares the rows.
//!
//! `&mut Db` is the store. `NewId` and `Now` are the only non-determinism a
//! mutation gets, chosen once at the originating client and frozen in the log.
//! `Actor` is who authored the entry. Which is which is decided by type, so
//! there is no list to keep in step.

use petros_schema::prelude::*;

#[cfg(feature = "storage")]
use crate::schema::Item;

// ------------------------------------------------------------------ mutations

/// Add a to-do at the end of the list.
#[mutation]
pub fn add(db: &mut Db, id: NewId, created_ms: Now, actor: Actor, text: String) -> Result {
    if text.trim().is_empty() {
        return Err("a to-do needs some text".into());
    }
    // The same entry arriving twice is a no-op, which is what makes redelivery
    // safe.
    if petros_sql::query!(db, "SELECT 1 AS \"found: Int\" FROM todo WHERE id = ?", id)
        .first()
        .is_some()
    {
        return Ok(());
    }
    // `pos` is read out of current state: an intent, not a fact. It is what
    // makes the rebase visible when an entry lands underneath yours.
    let last = petros_sql::query!(
        db,
        "SELECT COALESCE(MAX(pos), 0) AS \"last: Int\" FROM todo"
    )
    .first()
    .map(|r| r.last)
    .unwrap_or(0);
    let text = text.trim().to_string();
    petros_sql::exec!(
        db,
        "INSERT INTO todo (id, text, done, pos, created_ms, actor)
         VALUES (?, ?, 0, ?, ?, ?)",
        id,
        text,
        last + 1,
        created_ms,
        actor
    );
    Ok(())
}

/// One entry rather than one per row, so it covers rows another peer added in
/// the meantime. That is what makes it an intent — and one *statement*, which
/// is what checked SQL buys over a query builder.
#[mutation]
pub fn mark_all_done(db: &mut Db) -> Result {
    petros_sql::exec!(db, "UPDATE todo SET done = 1 WHERE done = 0");
    Ok(())
}

/// Updating a row that is gone is a no-op, not an error: an entry earlier in
/// the log may have removed it.
#[mutation]
pub fn set_done(db: &mut Db, id: Id, done: bool) -> Result {
    petros_sql::exec!(db, "UPDATE todo SET done = ? WHERE id = ?", done, id);
    Ok(())
}

#[mutation]
pub fn remove(db: &mut Db, id: Id) -> Result {
    petros_sql::exec!(db, "DELETE FROM todo WHERE id = ?", id);
    Ok(())
}

// -------------------------------------------------------------------- queries

/// The whole list, in order. Always ordered explicitly: SQLite's natural order
/// is not a contract, and two peers showing the same rows in different orders
/// is a bug that only appears on someone else's machine.
#[query]
pub fn list(db: &mut Db) -> Result<Vec<Item>> {
    Ok(petros_sql::query!(
        db,
        "SELECT id, text, done, pos, created_ms, actor FROM todo ORDER BY pos, id"
    )
    .into_iter()
    .map(|r| Item {
        id: crate::schema::id_of(&r.id),
        text: r.text,
        done: r.done,
        pos: r.pos,
        created_ms: r.created_ms,
        actor: r.actor,
    })
    .collect())
}

peer!(add, mark_all_done, set_done, remove);
