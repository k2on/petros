//! The two traits an app implements, and the transaction handle `apply` gets.

use std::ops::{Deref, DerefMut};

use serde::{de::DeserializeOwned, Serialize};

use crate::Connection;

use crate::{AutoCtx, Ctx, MutationError, Result};

/// A database handle that is guaranteed to be inside a transaction Petros opened.
///
/// It derefs to [`Connection`], so `apply` can use the full rusqlite API, but
/// the distinct type is a reminder: do not commit, roll back, or open nested
/// transactions here. Petros owns the transaction boundaries — they are what makes
/// the optimistic rebase possible.
pub struct Transaction<'a> {
    conn: &'a mut Connection,
    /// What the rows did, for anything maintained from changes rather than
    /// re-read. Collected here because a mutation's store is created and
    /// dropped inside `apply`, and the client outlives it.
    changes: Vec<petros_schema::Change>,
}

impl std::fmt::Debug for Transaction<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Transaction")
    }
}

impl<'a> Transaction<'a> {
    pub(crate) fn new(conn: &'a mut Connection) -> Self {
        Transaction {
            conn,
            changes: Vec::new(),
        }
    }

    /// Report what a store did. Called by the `apply` an app generates, once
    /// per mutation, with what the store recorded.
    pub fn record(&mut self, changes: Vec<petros_schema::Change>) {
        self.changes.extend(changes);
    }

    pub(crate) fn take_recorded(&mut self) -> Vec<petros_schema::Change> {
        std::mem::take(&mut self.changes)
    }

    /// The underlying connection. Diesel needs `&mut` for every query, hence
    /// the mutable borrow all the way down.
    pub fn conn(&mut self) -> &mut Connection {
        self.conn
    }
}

impl Deref for Transaction<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.conn
    }
}

impl DerefMut for Transaction<'_> {
    fn deref_mut(&mut self) -> &mut Connection {
        self.conn
    }
}

/// One intent from the app's mutation enum.
///
/// Mutations are intents, not facts: `AddToList { list, items }`, never
/// `ItemInserted { pos: "a5" }`. `apply` may read the database to decide what to
/// write, which is what lets a mutation mean the same thing when it lands after
/// entries it has never seen.
///
/// # Rules for `apply`
///
/// These are not enforced by the type system. Break one and replicas diverge.
///
/// * No clock reads, no RNG, no network, no filesystem. Anything
///   non-deterministic belongs in the arguments, put there by [`fill_auto`].
/// * Always `ORDER BY` explicitly. SQLite's natural row order is not a
///   contract and changes with the query planner.
/// * No floats in anything that affects control flow.
///
/// ```
/// # use diesel::prelude::*;
/// # use petros::{Ctx, AutoCtx, Mutation, MutationError, Transaction};
/// # use serde::{Deserialize, Serialize};
/// # diesel::table! { counter (n) { n -> BigInt } }
/// #[derive(Serialize, Deserialize)]
/// #[serde(tag = "t")]
/// enum Counter {
///     Bump { by: i64 },
/// }
///
/// impl Mutation for Counter {
///     fn apply(&self, tx: &mut Transaction, _ctx: &Ctx) -> Result<(), MutationError> {
///         let Counter::Bump { by } = self;
///         diesel::update(counter::table)
///             .set(counter::n.eq(counter::n + by))
///             .execute(tx.conn())?;
///         Ok(())
///     }
/// }
/// ```
///
/// [`fill_auto`]: Mutation::fill_auto
pub trait Mutation: Serialize + DeserializeOwned + 'static {
    /// Fill in non-deterministic arguments — clock reads, generated ids.
    ///
    /// Called once, at the originating client, before the entry exists
    /// anywhere. The values it produces are then frozen in the log forever.
    fn fill_auto(&mut self, ctx: &mut AutoCtx) {
        let _ = ctx;
    }

    /// Apply the intent. Deterministic. May read. May reject.
    ///
    /// `ctx` is who authored the entry and under which login, as the log
    /// recorded them — frozen like the arguments, and for the same reason.
    fn apply(&self, tx: &mut Transaction, ctx: &Ctx) -> std::result::Result<(), MutationError>;
}

/// The single extension point. An app supplies one mutation enum and its
/// schema; Petros is generic over it.
///
/// Petros owns every table prefixed `petros_`. The app owns everything else.
pub trait App: 'static {
    /// The app's mutation enum.
    type Mutation: Mutation;

    /// The app's tables, as DDL.
    ///
    /// Petros runs this on every open, so every statement has to be idempotent
    /// — `CREATE TABLE IF NOT EXISTS`, and the same for indexes.
    ///
    /// Usually `include_str!("../schema.sql")`, which is also the file
    /// `petros-sql` prepares the app's statements against at build time. One
    /// description of the tables, checked and run from the same place.
    const SCHEMA: &'static str;

    /// Create the app's tables. The default runs [`SCHEMA`](App::SCHEMA), which
    /// is what an app wants; override it only for a migration that DDL cannot
    /// express.
    fn migrate(conn: &mut Connection) -> Result<()> {
        crate::batch(conn, Self::SCHEMA)
    }

    /// The version of the app's derived tables. Bump it whenever an existing
    /// table's *shape* changes — a column added, removed, renamed or retyped —
    /// or whenever a change to `apply` means the tables already on a device no
    /// longer hold what today's code would have written.
    ///
    /// When a peer opens a database stamped with an older version, Petros
    /// rebuilds the app's tables from the log: it drops them, recreates them at
    /// the current shape with [`migrate`](App::migrate), and replays every
    /// confirmed entry through the current `apply`. The tables are a pure
    /// function of the log — `apply` is their only writer — so there is no
    /// `ALTER` to write and no data to back-fill; the cost is a replay, bounded
    /// by the log's length, paid once when the version moves.
    ///
    /// A *new* table or a *new* index needs no bump: `CREATE … IF NOT EXISTS`
    /// in [`SCHEMA`](App::SCHEMA) adds it on the next open. Only a change to a
    /// table that already exists does.
    ///
    /// `0` — the default — turns this off: the tables are taken to match
    /// `SCHEMA` and are never rebuilt. Set it to `1` when you first need a
    /// migration, and count up from there.
    const SCHEMA_VERSION: u32 = 0;
}
