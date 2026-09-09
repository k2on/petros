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
use petros_schema::Rows;

use crate::schema::Todo as TodoRow;

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
    if db.exists::<TodoRow>(&TodoRow::key_of(&id)) {
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
    db.put(&TodoRow {
        id,
        text: text.trim().to_string(),
        done: false,
        pos: last + 1,
        created_ms,
        actor: actor.to_string(),
    });
    Ok(())
}

/// One entry rather than one per row, so it covers rows another peer added in
/// the meantime. That is what makes it an intent — and one *statement*, which
/// is what checked SQL buys over a query builder.
/// One entry rather than one per row, so it covers rows another peer added in
/// the meantime — that is what makes it an intent.
///
/// A row at a time, and that is the price of typed writes: a view can only be
/// maintained from changes it is told about, and `UPDATE … WHERE` tells nobody
/// which rows moved. Reading the set is still one statement.
#[mutation]
pub fn mark_all_done(db: &mut Db) -> Result {
    let ids = petros_sql::query!(db, "SELECT id FROM todo WHERE done = 0");
    for row in ids {
        if let Some(mut todo) = db.get::<TodoRow>(&TodoRow::key_of(&row.id)) {
            todo.done = true;
            db.put(&todo);
        }
    }
    Ok(())
}

/// Updating a row that is gone is a no-op, not an error: an entry earlier in
/// the log may have removed it.
#[mutation]
pub fn set_done(db: &mut Db, id: Id, done: bool) -> Result {
    if let Some(mut todo) = db.get::<TodoRow>(&TodoRow::key_of(&id)) {
        todo.done = done;
        db.put(&todo);
    }
    Ok(())
}

#[mutation]
pub fn remove(db: &mut Db, id: Id) -> Result {
    db.delete::<TodoRow>(&TodoRow::key_of(&id));
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
