//! The three tables Exo owns, and the primitives that touch them.
//!
//! Everything here is deliberately dumb: no policy, no state machine. The
//! interesting decisions live in [`crate::client`] and [`crate::server`].

use rusqlite::{params, Connection, OptionalExtension};
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::{proto, ActorId, Entry, Result, Seq, Uuid};

/// `exo_log` is the confirmed, server-ordered log. `exo_pending` is a client's
/// own unconfirmed mutations, in the order they were made. `exo_meta` is a
/// key/value scratchpad — currently just the sync cursor.
///
/// `payload` holds the CBOR encoding of the mutation, exactly as it went over
/// the wire. Storing the encoded form rather than exploded columns is what lets
/// an old log stay readable: decoding is the app's versioning problem, and it
/// gets the original bytes to solve it with.
const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS exo_log (
        seq     INTEGER PRIMARY KEY,
        id      BLOB NOT NULL UNIQUE,
        actor   TEXT NOT NULL,
        payload BLOB NOT NULL
    );
    CREATE TABLE IF NOT EXISTS exo_pending (
        ord     INTEGER PRIMARY KEY AUTOINCREMENT,
        id      BLOB NOT NULL UNIQUE,
        actor   TEXT NOT NULL,
        payload BLOB NOT NULL
    );
    CREATE TABLE IF NOT EXISTS exo_meta (
        k TEXT PRIMARY KEY,
        v INTEGER NOT NULL
    );
";

const CURSOR: &str = "cursor";

pub(crate) fn migrate(conn: &Connection) -> Result<()> {
    conn.execute_batch(SCHEMA)?;
    Ok(())
}

/// The highest sequence number in the log, or 0 for an empty log.
pub(crate) fn head(conn: &Connection) -> Result<Seq> {
    let head = conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM exo_log", [], |r| {
        r.get::<_, i64>(0)
    })?;
    Ok(head as Seq)
}

/// Where a client has applied up to. Persisted, so a restart resumes rather
/// than resyncing from the beginning.
pub(crate) fn cursor(conn: &Connection) -> Result<Seq> {
    let v: Option<i64> = conn
        .query_row("SELECT v FROM exo_meta WHERE k = ?1", [CURSOR], |r| r.get(0))
        .optional()?;
    Ok(v.unwrap_or(0) as Seq)
}

pub(crate) fn set_cursor(conn: &Connection, seq: Seq) -> Result<()> {
    conn.execute(
        "INSERT INTO exo_meta (k, v) VALUES (?1, ?2)
         ON CONFLICT(k) DO UPDATE SET v = excluded.v",
        params![CURSOR, seq as i64],
    )?;
    Ok(())
}

/// The sequence number already assigned to this entry id, if any. This single
/// lookup is the whole of the dedupe story.
pub(crate) fn seq_of(conn: &Connection, id: &Uuid) -> Result<Option<Seq>> {
    let seq: Option<i64> = conn
        .query_row("SELECT seq FROM exo_log WHERE id = ?1", params![id], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(seq.map(|s| s as Seq))
}

/// Write a sequenced entry into the log. Ignores an id already present, so
/// delivering the same entry twice is harmless.
pub(crate) fn put_confirmed<M: Serialize>(conn: &Connection, entry: &Entry<M>) -> Result<()> {
    let seq = entry.require_seq()?;
    conn.execute(
        "INSERT OR IGNORE INTO exo_log (seq, id, actor, payload) VALUES (?1, ?2, ?3, ?4)",
        params![
            seq as i64,
            entry.id,
            entry.actor.as_str(),
            proto::encode(&entry.mutation)?
        ],
    )?;
    Ok(())
}

/// Confirmed entries with `seq > after`, in log order, at most `limit` of them.
pub(crate) fn entries_after<M: DeserializeOwned>(
    conn: &Connection,
    after: Seq,
    limit: usize,
) -> Result<Vec<Entry<M>>> {
    let mut stmt = conn.prepare(
        "SELECT seq, id, actor, payload FROM exo_log
         WHERE seq > ?1 ORDER BY seq LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![after as i64, limit as i64], row_to_parts)?;
    rows.map(|r| parts_to_entry(r?)).collect()
}

/// Pending entries, in the order they were made. Unsequenced by definition.
pub(crate) fn pending<M: DeserializeOwned>(conn: &Connection) -> Result<Vec<Entry<M>>> {
    let mut stmt = conn.prepare("SELECT NULL, id, actor, payload FROM exo_pending ORDER BY ord")?;
    let rows = stmt.query_map([], row_to_parts)?;
    rows.map(|r| parts_to_entry(r?)).collect()
}

pub(crate) fn pending_len(conn: &Connection) -> Result<usize> {
    let n: i64 = conn.query_row("SELECT COUNT(*) FROM exo_pending", [], |r| r.get(0))?;
    Ok(n as usize)
}

pub(crate) fn put_pending<M: Serialize>(conn: &Connection, entry: &Entry<M>) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO exo_pending (id, actor, payload) VALUES (?1, ?2, ?3)",
        params![
            entry.id,
            entry.actor.as_str(),
            proto::encode(&entry.mutation)?
        ],
    )?;
    Ok(())
}

pub(crate) fn drop_pending(conn: &Connection, id: &Uuid) -> Result<()> {
    conn.execute("DELETE FROM exo_pending WHERE id = ?1", params![id])?;
    Ok(())
}

type Parts = (Option<i64>, Uuid, String, Vec<u8>);

fn row_to_parts(r: &rusqlite::Row<'_>) -> rusqlite::Result<Parts> {
    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
}

fn parts_to_entry<M: DeserializeOwned>((seq, id, actor, payload): Parts) -> Result<Entry<M>> {
    Ok(Entry {
        id,
        actor: ActorId::new(actor),
        seq: seq.map(|s| s as Seq),
        mutation: proto::decode(&payload)?,
    })
}
