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
//! #     const SCHEMA: &'static str = "CREATE TABLE IF NOT EXISTS note (text TEXT NOT NULL)";
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
mod foreign;

#[cfg(feature = "ws")]
pub mod transport;

pub use auto::AutoCtx;
pub use client::{Changes, Client, Rejection};

pub use error::{Error, MutationError, Result};
pub use id::Id;
pub use mutation::{App, Mutation, Transaction};
/// Incrementally maintained views: a query that stays right without being run
/// again. Re-exported so an app declares one dependency rather than two, and
/// so the version it gets is the one this engine was built against.
pub use petros_ivm as ivm;
pub use proto::{decode, encode, ActorId, ClientMsg, Entry, Seq, ServerMsg};
pub use server::{ConnId, Server};

// One definition per function. See `petros_macros`.
pub use petros_macros::{mutation, peer, query};
#[doc(hidden)]
pub use petros_schema;
/// The names those attributes recognise, and what a function returns.
pub use petros_schema::prelude;

pub use diesel::{self, SqliteConnection as Connection};
pub use uuid;

use diesel::connection::SimpleConnection;
use diesel::Connection as _;

/// An in-memory database, for tests and examples.
/// Run one or more statements that take no parameters — DDL, pragmas.
///
/// Here so an app can create its tables without depending on Diesel. Petros
/// owns the database library; nothing above it should have to name one.
///
/// Not for anything with a value in it. A statement with a value wants
/// `petros_sql::exec!`, which checks it and binds rather than interpolating.
pub fn batch(conn: &mut Connection, sql: &str) -> Result<()> {
    diesel::connection::SimpleConnection::batch_execute(conn, sql)?;
    Ok(())
}

pub fn open_memory() -> Result<Connection> {
    let mut conn = Connection::establish(":memory:")?;
    tune(&mut conn)?;
    Ok(conn)
}

/// A database on disk, with the pragmas Petros expects.
/// A database by name, without WAL.
///
/// For a browser. `sqlite-wasm-rs` keeps files in a memory VFS, and WAL wants
/// shared memory it does not provide — [`open_path`] would fail on the pragma.
/// The name is what makes the file exportable: a browser peer persists by
/// handing those bytes to the page and importing them back on the next load.
pub fn open_named(name: &str) -> Result<Connection> {
    let mut conn = Connection::establish(name)?;
    tune(&mut conn)?;
    Ok(conn)
}

pub fn open_path(path: impl AsRef<std::path::Path>) -> Result<Connection> {
    let mut conn = Connection::establish(&path.as_ref().to_string_lossy())?;
    // WAL, and `synchronous = NORMAL` with it.
    //
    // FULL fsyncs on every commit, and a commit is every mutation. Measured here
    // that is 4.03ms against 0.33ms — 92% of a mutation — and on phone flash an
    // fsync is one to two orders of magnitude slower again, which is where
    // 200-300ms taps came from.
    //
    // NORMAL in WAL is the documented-safe setting: a power cut can lose the
    // last transactions, and cannot corrupt the database. Losing them matters
    // less here than in most applications, because the log is what is true. A
    // confirmed entry is already on the server, and a pending one that never
    // reached disk is a mutation that did not happen — which is a state the
    // rebase already handles, since it is what being offline looks like.
    conn.batch_execute("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
    tune(&mut conn)?;
    Ok(conn)
}

fn tune(conn: &mut Connection) -> Result<()> {
    conn.batch_execute("PRAGMA foreign_keys = ON; PRAGMA busy_timeout = 5000;")?;
    Ok(())
}

/// Wire an app's functions to the engine.
///
/// `apply` and `fill_auto` come from `peer!`, and the schema from wherever the
/// app keeps its DDL. What this adds is the part that needs the engine itself
/// and so cannot live in a domain crate compiled to wasm: the payload type, its
/// `Mutation` impl, and the `App`.
///
/// ```ignore
/// petros::app!(HarkenApp {
///     schema: crate::schema::SCHEMA,
///     apply: crate::functions::apply,
///     fill_auto: crate::functions::fill_auto,
/// });
/// ```
#[macro_export]
macro_rules! app {
    ($app:ident {
        schema: $schema:expr,
        apply: $apply:path,
        fill_auto: $fill_auto:path $(,)?
    }) => {
        /// One mutation, as the bytes the log stores.
        ///
        /// Not a Rust enum mirroring the verbs, and that is deliberate: a peer
        /// that has never heard of a verb still carries it through the log
        /// intact, and applies it as soon as it has a build that knows what it
        /// means.
        #[derive(Debug, Clone, PartialEq, ::serde::Serialize, ::serde::Deserialize)]
        #[serde(transparent)]
        pub struct Payload(pub $crate::petros_schema::cbor::Value);

        impl ::core::convert::From<$crate::petros_schema::cbor::Value> for Payload {
            fn from(v: $crate::petros_schema::cbor::Value) -> Self {
                Payload(v)
            }
        }

        impl $crate::Mutation for Payload {
            fn fill_auto(&mut self, ctx: &mut $crate::AutoCtx) {
                let uuid = ctx.uuid().as_uuid().as_bytes().to_vec();
                $fill_auto(&mut self.0, uuid, ctx.now_ms());
            }

            fn apply(
                &self,
                tx: &mut $crate::Transaction,
                actor: &$crate::ActorId,
            ) -> ::core::result::Result<(), $crate::MutationError> {
                // The store is scoped so that its borrow of the connection
                // ends before the changes are handed to the transaction. They
                // are reported whether the mutation was accepted or refused —
                // a refused one is rolled back by the caller, and a change it
                // recorded on the way is not a change that happened.
                let (outcome, changes) = {
                    let mut store = $crate::backend::SqliteStore::new(tx.conn());
                    let outcome = $apply(&mut store, &self.0, actor.as_str());
                    let changes = $crate::petros_schema::Store::take_changes(&mut store);
                    (outcome, changes)
                };
                if outcome.is_ok() {
                    tx.record(changes);
                }
                outcome.map_err($crate::MutationError::rejected)
            }
        }

        /// As [`from_json`], for a caller that already has the arguments as a
        /// JSON value — a test, usually.
        pub fn from_value(
            kind: &str,
            args: ::serde_json::Value,
        ) -> ::core::result::Result<Payload, ::std::string::String> {
            $crate::petros_schema::author::from_value(kind, args).map(Payload)
        }

        /// Author a mutation by name, through the same encoder every peer
        /// uses.
        ///
        /// The generic entry point: a foreign caller has a verb and some JSON
        /// and no way to call a typed authoring function.
        pub fn from_json(
            kind: &str,
            args_json: &str,
        ) -> ::core::result::Result<Payload, ::std::string::String> {
            $crate::petros_schema::author::from_json(kind, args_json).map(Payload)
        }

        /// The app: Petros's tables plus this one's.
        #[derive(Debug)]
        pub struct $app;

        impl $crate::App for $app {
            type Mutation = Payload;
            const SCHEMA: &'static str = $schema;
        }
    };
}
