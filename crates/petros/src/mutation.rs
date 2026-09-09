//! The two traits an app implements, and the transaction handle `apply` gets.

use std::ops::{Deref, DerefMut};

use serde::{de::DeserializeOwned, Serialize};

use crate::Connection;

use crate::{ActorId, AutoCtx, MutationError, Result};

/// A database handle that is guaranteed to be inside a transaction Petros opened.
///
/// It derefs to [`Connection`], so `apply` can use the full rusqlite API, but
/// the distinct type is a reminder: do not commit, roll back, or open nested
/// transactions here. Petros owns the transaction boundaries — they are what makes
/// the optimistic rebase possible.
pub struct Transaction<'a> {
    conn: &'a mut Connection,
}

impl std::fmt::Debug for Transaction<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Transaction")
    }
}

impl<'a> Transaction<'a> {
    pub(crate) fn new(conn: &'a mut Connection) -> Self {
        Transaction { conn }
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
/// # use petros::{ActorId, AutoCtx, Mutation, MutationError, Transaction};
/// # use serde::{Deserialize, Serialize};
/// # diesel::table! { counter (n) { n -> BigInt } }
/// #[derive(Serialize, Deserialize)]
/// #[serde(tag = "t")]
/// enum Counter {
///     Bump { by: i64 },
/// }
///
/// impl Mutation for Counter {
///     fn apply(&self, tx: &mut Transaction, _actor: &ActorId) -> Result<(), MutationError> {
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
    fn apply(
        &self,
        tx: &mut Transaction,
        actor: &ActorId,
    ) -> std::result::Result<(), MutationError>;
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
}
