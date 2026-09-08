//! Petros — an offline-first sync engine.
//!
//! Petros syncs an append-only, totally ordered log of mutations owned by a
//! server. Mutations are *intents*, not facts: `AddToList { list, items }`,
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
//! Petros knows nothing about any particular app. An app supplies one mutation
//! enum and its schema through [`App`]; Petros owns the tables prefixed `petros_` and
//! nothing else.
//!
//! [`Client`] and [`Server`] are sans-io state machines — you feed them
//! messages and drain their outboxes. No sockets, no async, no runtime.
//!
//! ```
//! # use diesel::prelude::*;
//! # use petros::{App, AutoCtx, Client, Connection, Mutation, MutationError, Transaction, ActorId, open_memory};
//! # use serde::{Deserialize, Serialize};
//! # diesel::table! { note (text) { text -> Text } }
//! # #[derive(Serialize, Deserialize)]
//! # #[serde(tag = "t")]
//! # enum M { Note { text: String } }
//! # impl Mutation for M {
//! #     fn apply(&self, tx: &mut Transaction, _a: &ActorId) -> std::result::Result<(), MutationError> {
//! #         let M::Note { text } = self;
//! #         diesel::insert_into(note::table).values(note::text.eq(text)).execute(tx.conn())?;
//! #         Ok(())
//! #     }
//! # }
//! # struct Notes;
//! # impl App for Notes {
//! #     type Mutation = M;
//! #     fn migrate(conn: &mut Connection) -> petros::Result<()> {
//! #         diesel::connection::SimpleConnection::batch_execute(
//! #             conn, "CREATE TABLE IF NOT EXISTS note (text TEXT NOT NULL)")?;
//! #         Ok(())
//! #     }
//! # }
//! let mut client = Client::<Notes>::open(open_memory()?, "alice", AutoCtx::seeded(1))?;
//! client.mutate(M::Note { text: "buy milk".into() })?;
//! let n: i64 = note::table.count().get_result(client.conn())?;
//! assert_eq!(n, 1); // applied optimistically, before any server has seen it
//! # Ok::<(), petros::Error>(())
//! ```
#![deny(warnings)]
#![deny(missing_debug_implementations)]

mod auto;
mod client;
mod error;
mod id;
mod mutation;
mod proto;
pub mod schema;
mod server;
mod store;

/// The typed store an app's `apply` writes through.
pub mod backend;

#[cfg(feature = "ws")]
pub mod transport;

pub use auto::AutoCtx;
pub use client::{Client, Rejection};
pub use error::{Error, MutationError, Result};
pub use id::Id;
pub use mutation::{App, Mutation, Transaction};
pub use proto::{decode, encode, ActorId, ClientMsg, Entry, Seq, ServerMsg};
pub use server::{ConnId, Server};

pub use diesel::{self, SqliteConnection as Connection};
pub use uuid;

use diesel::connection::SimpleConnection;
use diesel::Connection as _;

/// An in-memory database, for tests and examples.
pub fn open_memory() -> Result<Connection> {
    let mut conn = Connection::establish(":memory:")?;
    tune(&mut conn)?;
    Ok(conn)
}

/// A database on disk, with the pragmas Petros expects.
pub fn open_path(path: impl AsRef<std::path::Path>) -> Result<Connection> {
    let mut conn = Connection::establish(&path.as_ref().to_string_lossy())?;
    conn.batch_execute("PRAGMA journal_mode = WAL;")?;
    tune(&mut conn)?;
    Ok(conn)
}

fn tune(conn: &mut Connection) -> Result<()> {
    conn.batch_execute("PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;")?;
    Ok(())
}
