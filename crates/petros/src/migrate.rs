//! Bringing an app's derived tables up to the current schema.
//!
//! An app's tables are a pure function of the log: `apply` is their only
//! writer, and every replica holds the whole log. So a schema change is not a
//! data-preserving `ALTER` with a back-fill to get subtly wrong — it is a
//! rebuild. Drop the tables, recreate them at the new shape, and replay every
//! confirmed entry through today's `apply`. The result is exactly what a fresh
//! install would have materialised, because a fresh install does the same
//! replay.
//!
//! This is only for the app's own tables. Petros's `petros_` tables — the log
//! most of all — are *not* derived from anything and cannot be rebuilt from
//! themselves, so they carry real, data-preserving migrations of their own in
//! [`crate::store`].
//!
//! It says nothing about whether an *older* peer may talk to a *newer* server:
//! that is a question about the log's meaning and the mutation format, not
//! about one device's tables. See `docs/decisions.md`.

use diesel::connection::SimpleConnection;

use crate::mutation::App;
use crate::{store, Connection, Entry, Mutation, Result, Transaction};

/// Whether the database at `conn` predates `A::SCHEMA_VERSION` and so needs its
/// app tables rebuilt. Always false when the app leaves `SCHEMA_VERSION` at 0,
/// which is what keeps this inert for an app that never asks for it.
pub(crate) fn is_stale<A: App>(conn: &mut Connection) -> Result<bool> {
    Ok(store::app_version(conn)? < A::SCHEMA_VERSION)
}

/// Drop the app's tables and recreate them empty at the current shape, then
/// stamp the new version. The caller replays the log into them — a client by
/// resetting its cursor and letting `advance` run, a server with
/// [`replay_all`].
pub(crate) fn reset_tables<A: App>(conn: &mut Connection) -> Result<()> {
    store::drop_app_tables(conn)?;
    A::migrate(conn)?;
    store::set_app_version(conn, A::SCHEMA_VERSION)?;
    Ok(())
}

/// Replay the whole confirmed log through `apply`, in sequence order, into the
/// tables [`reset_tables`] just recreated. This is how a server rematerialises;
/// a client reuses its ordinary catch-up path instead.
///
/// A confirmed entry that will not apply is a determinism bug — the server
/// accepted it once — so it aborts rather than skipping.
pub(crate) fn replay_all<A: App>(conn: &mut Connection) -> Result<()> {
    let head = store::head(conn)?;
    if head == 0 {
        return Ok(());
    }
    let entries: Vec<Entry<A::Mutation>> = store::entries_after(conn, 0, head as usize)?;
    conn.batch_execute("BEGIN")?;
    for entry in &entries {
        if let Err(e) = entry
            .mutation
            .apply(&mut Transaction::new(conn), &entry.ctx())
        {
            conn.batch_execute("ROLLBACK")?;
            return Err(e.into());
        }
    }
    conn.batch_execute("COMMIT")?;
    Ok(())
}
