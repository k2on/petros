//! The three tables Petros owns, and the primitives that touch them.
//!
//! Everything here is deliberately dumb: no policy, no state machine. The
//! interesting decisions live in [`crate::client`] and [`crate::server`].

use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use diesel::sqlite::Sqlite;
use diesel::upsert::excluded;
use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::schema::{petros_log, petros_meta, petros_pending};
use crate::{proto, ActorId, Connection, Entry, Id, Result, Seq};

/// The DDL behind [`crate::schema`]. Idempotent: it runs on every open.
///
/// `payload` holds the CBOR encoding of the mutation rather than exploded
/// columns. That is what lets an old log stay readable — decoding is the app's
/// versioning problem, and it gets the original bytes to solve it with.
/// The state database: the confirmed log and the cursor into it.
const DDL: &str = "
    CREATE TABLE IF NOT EXISTS petros_log (
        seq     BIGINT PRIMARY KEY NOT NULL,
        id      BLOB NOT NULL UNIQUE,
        actor   TEXT NOT NULL,
        payload BLOB NOT NULL
    );
    CREATE TABLE IF NOT EXISTS petros_meta (
        k TEXT PRIMARY KEY NOT NULL,
        v BIGINT NOT NULL
    );
";

/// The intents database, which is a *separate file* on a *separate connection*.
///
/// Not tidiness. A client holds its optimistic view in an open transaction on
/// the state database, and a mutation has to record its intent durably — which
/// means committing, which means closing that transaction and replaying every
/// pending mutation to rebuild the view. That made a tap cost O(pending), and a
/// burst of taps O(n²): 390ms at forty pending on a phone.
///
/// Two files, two write locks. The intent commits while the optimistic
/// transaction stays open, so a new mutation applies on top of the view instead
/// of rebuilding it, and a tap is flat.
const INTENTS_DDL: &str = "
    CREATE TABLE IF NOT EXISTS petros_pending (
        ord     INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
        id      BLOB NOT NULL UNIQUE,
        actor   TEXT NOT NULL,
        payload BLOB NOT NULL
    );
";

const CURSOR: &str = "cursor";

/// One row of the confirmed log.
#[derive(Debug, Queryable, Selectable, Insertable)]
#[diesel(table_name = petros_log, check_for_backend(Sqlite))]
struct LogRow {
    seq: i64,
    id: Id,
    actor: String,
    payload: Vec<u8>,
}

/// One row of the pending queue. `ord` is assigned by SQLite and only ever
/// used to preserve the order the mutations were made in.
#[derive(Debug, Queryable, Selectable)]
#[diesel(table_name = petros_pending, check_for_backend(Sqlite))]
struct PendingRow {
    /// Assigned by SQLite; only ever used to preserve the order the mutations
    /// were made in, never read.
    #[allow(dead_code)]
    ord: i64,
    id: Id,
    actor: String,
    payload: Vec<u8>,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = petros_pending)]
struct NewPending {
    id: Id,
    actor: String,
    payload: Vec<u8>,
}

pub(crate) fn migrate(conn: &mut Connection) -> Result<()> {
    conn.batch_execute(DDL)?;
    Ok(())
}

pub(crate) fn migrate_intents(conn: &mut Connection) -> Result<()> {
    conn.batch_execute(INTENTS_DDL)?;
    Ok(())
}

/// Where a connection's main database lives, if it is a file.
///
/// `None` for `:memory:`, which is what tests and the browser use — their
/// intents go in memory too, since nothing about them was durable anyway.
pub(crate) fn main_file(conn: &mut Connection) -> Result<Option<String>> {
    #[derive(QueryableByName)]
    struct Row {
        #[diesel(sql_type = diesel::sql_types::Text)]
        file: String,
    }
    let rows: Vec<Row> =
        diesel::sql_query("SELECT file FROM pragma_database_list WHERE name = 'main'")
            .load(conn)?;
    Ok(rows
        .into_iter()
        .next()
        .map(|r| r.file)
        .filter(|f| !f.is_empty()))
}

/// The highest sequence number in the log, or 0 for an empty log.
pub(crate) fn head(conn: &mut Connection) -> Result<Seq> {
    let head: Option<i64> = petros_log::table
        .select(diesel::dsl::max(petros_log::seq))
        .first(conn)?;
    Ok(head.unwrap_or(0) as Seq)
}

/// Where a client has applied up to. Persisted, so a restart resumes rather
/// than resyncing from the beginning.
pub(crate) fn cursor(conn: &mut Connection) -> Result<Seq> {
    let v: Option<i64> = petros_meta::table
        .select(petros_meta::v)
        .filter(petros_meta::k.eq(CURSOR))
        .first(conn)
        .optional()?;
    Ok(v.unwrap_or(0) as Seq)
}

pub(crate) fn set_cursor(conn: &mut Connection, seq: Seq) -> Result<()> {
    diesel::insert_into(petros_meta::table)
        .values((petros_meta::k.eq(CURSOR), petros_meta::v.eq(seq as i64)))
        .on_conflict(petros_meta::k)
        .do_update()
        .set(petros_meta::v.eq(excluded(petros_meta::v)))
        .execute(conn)?;
    Ok(())
}

/// The sequence number already assigned to this entry id, if any. This single
/// lookup is the whole of the dedupe story.
pub(crate) fn seq_of(conn: &mut Connection, id: &Id) -> Result<Option<Seq>> {
    let seq: Option<i64> = petros_log::table
        .select(petros_log::seq)
        .filter(petros_log::id.eq(id))
        .first(conn)
        .optional()?;
    Ok(seq.map(|s| s as Seq))
}

/// Write a sequenced entry into the log. Ignores an id already present, so
/// delivering the same entry twice is harmless.
pub(crate) fn put_confirmed<M: Serialize>(conn: &mut Connection, entry: &Entry<M>) -> Result<()> {
    let row = LogRow {
        seq: entry.require_seq()? as i64,
        id: entry.id,
        actor: entry.actor.as_str().to_string(),
        payload: proto::encode(&entry.mutation)?,
    };
    diesel::insert_into(petros_log::table)
        .values(&row)
        .on_conflict_do_nothing()
        .execute(conn)?;
    Ok(())
}

/// Confirmed entries with `seq > after`, in log order, at most `limit` of them.
pub(crate) fn entries_after<M: DeserializeOwned>(
    conn: &mut Connection,
    after: Seq,
    limit: usize,
) -> Result<Vec<Entry<M>>> {
    let rows: Vec<LogRow> = petros_log::table
        .select(LogRow::as_select())
        .filter(petros_log::seq.gt(after as i64))
        .order(petros_log::seq.asc())
        .limit(limit as i64)
        .load(conn)?;
    rows.into_iter()
        .map(|r| {
            Ok(Entry {
                id: r.id,
                actor: ActorId::new(r.actor),
                seq: Some(r.seq as Seq),
                mutation: proto::decode(&r.payload)?,
            })
        })
        .collect()
}

/// Pending entries, in the order they were made. Unsequenced by definition.
pub(crate) fn pending<M: DeserializeOwned>(conn: &mut Connection) -> Result<Vec<Entry<M>>> {
    let rows: Vec<PendingRow> = petros_pending::table
        .select(PendingRow::as_select())
        .order(petros_pending::ord.asc())
        .load(conn)?;
    rows.into_iter()
        .map(|r| {
            Ok(Entry {
                id: r.id,
                actor: ActorId::new(r.actor),
                seq: None,
                mutation: proto::decode(&r.payload)?,
            })
        })
        .collect()
}

pub(crate) fn pending_len(conn: &mut Connection) -> Result<usize> {
    let n: i64 = petros_pending::table.count().get_result(conn)?;
    Ok(n as usize)
}

pub(crate) fn put_pending<M: Serialize>(conn: &mut Connection, entry: &Entry<M>) -> Result<()> {
    let row = NewPending {
        id: entry.id,
        actor: entry.actor.as_str().to_string(),
        payload: proto::encode(&entry.mutation)?,
    };
    diesel::insert_into(petros_pending::table)
        .values(&row)
        .on_conflict_do_nothing()
        .execute(conn)?;
    Ok(())
}

pub(crate) fn drop_pending(conn: &mut Connection, id: &Id) -> Result<()> {
    diesel::delete(petros_pending::table.filter(petros_pending::id.eq(id))).execute(conn)?;
    Ok(())
}

/// The sequence numbers of the contiguous run of confirmed entries starting at
/// `after + 1`, at most `limit` of them. A gap means an entry is still in
/// flight, and everything past it has to wait: applying out of order would not
/// be the log.
pub(crate) fn contiguous_after(conn: &mut Connection, after: Seq, limit: usize) -> Result<Seq> {
    let seqs: Vec<i64> = petros_log::table
        .select(petros_log::seq)
        .filter(petros_log::seq.gt(after as i64))
        .order(petros_log::seq.asc())
        .limit(limit as i64)
        .load(conn)?;
    let mut end = after;
    for seq in seqs {
        if seq as Seq != end + 1 {
            break;
        }
        end = seq as Seq;
    }
    Ok(end)
}
