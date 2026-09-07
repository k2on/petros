//! Exo — an offline-first sync engine.
//!
//! Exo syncs an append-only, totally ordered log of mutations owned by a
//! server. Mutations are *intents*, not facts: `AddTracks { playlist, tracks }`,
//! not `ItemInserted { pos: "a5" }`. A mutation's
//! [`apply`](Mutation::apply) may read the database to decide what to write.
//!
//! Because the log is append-only and never reordered, confirmed state only
//! moves forward. The only thing ever undone is a client's own pending
//! mutations, which are replayed on top after confirmed entries land. That is
//! the rebase, and it is the whole idea:
//!
//! ```text
//! view = replay(confirmed) then replay(pending)
//! ```
//!
//! Exo knows nothing about any particular app. An app supplies one mutation
//! enum and its schema through [`App`]; Exo owns the tables prefixed `exo_` and
//! nothing else.
//!
//! [`Client`] and [`Server`] are sans-io state machines — you feed them
//! messages and drain their outboxes. No sockets, no async, no runtime.
//!
//! ```
//! # use exo::{App, AutoCtx, Client, Connection, Mutation, MutationError, Transaction, ActorId, open_memory};
//! # use serde::{Deserialize, Serialize};
//! # #[derive(Serialize, Deserialize)]
//! # #[serde(tag = "t")]
//! # enum M { Note { text: String } }
//! # impl Mutation for M {
//! #     fn apply(&self, tx: &Transaction, _a: &ActorId) -> std::result::Result<(), MutationError> {
//! #         let M::Note { text } = self;
//! #         tx.execute("INSERT INTO note (text) VALUES (?1)", [text])?;
//! #         Ok(())
//! #     }
//! # }
//! # struct Notes;
//! # impl App for Notes {
//! #     type Mutation = M;
//! #     fn migrate(conn: &Connection) -> exo::Result<()> {
//! #         conn.execute_batch("CREATE TABLE IF NOT EXISTS note (text TEXT NOT NULL)")?;
//! #         Ok(())
//! #     }
//! # }
//! let mut client = Client::<Notes>::open(open_memory()?, "alice", AutoCtx::seeded(1))?;
//! client.mutate(M::Note { text: "buy milk".into() })?;
//! let n: i64 = client.conn().query_row("SELECT COUNT(*) FROM note", [], |r| r.get(0))?;
//! assert_eq!(n, 1); // applied optimistically, before any server has seen it
//! # Ok::<(), exo::Error>(())
//! ```
#![deny(warnings)]
#![deny(missing_debug_implementations)]

mod auto;
mod client;
mod error;
mod mutation;
mod proto;
mod server;
mod store;

#[cfg(feature = "ws")]
pub mod transport;

pub use auto::AutoCtx;
pub use client::{Client, Rejection};
pub use error::{Error, MutationError, Result};
pub use mutation::{App, Mutation, Transaction};
pub use proto::{decode, encode, ActorId, ClientMsg, Entry, Seq, ServerMsg};
pub use server::{ConnId, Server};

pub use rusqlite::{self, Connection};
pub use uuid::{self, Uuid};

/// An in-memory database, for tests and examples.
pub fn open_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    tune(&conn)?;
    Ok(conn)
}

/// A database on disk, with the pragmas Exo expects.
pub fn open_path(path: impl AsRef<std::path::Path>) -> Result<Connection> {
    let conn = Connection::open(path)?;
    conn.pragma_update(None, "journal_mode", "WAL")?;
    tune(&conn)?;
    Ok(conn)
}

fn tune(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "foreign_keys", true)?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    Ok(())
}
