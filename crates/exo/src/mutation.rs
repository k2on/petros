//! The two traits an app implements, and the transaction handle `apply` gets.

use std::ops::Deref;

use rusqlite::Connection;
use serde::{de::DeserializeOwned, Serialize};

use crate::{ActorId, AutoCtx, MutationError, Result};

/// A database handle that is guaranteed to be inside a transaction Exo opened.
///
/// It derefs to [`Connection`], so `apply` can use the full rusqlite API, but
/// the distinct type is a reminder: do not commit, roll back, or open nested
/// transactions here. Exo owns the transaction boundaries — they are what makes
/// the optimistic rebase possible.
#[derive(Debug)]
pub struct Transaction<'a> {
    conn: &'a Connection,
}

impl<'a> Transaction<'a> {
    pub(crate) fn new(conn: &'a Connection) -> Self {
        Transaction { conn }
    }

    /// The underlying connection.
    pub fn conn(&self) -> &Connection {
        self.conn
    }
}

impl Deref for Transaction<'_> {
    type Target = Connection;

    fn deref(&self) -> &Connection {
        self.conn
    }
}

/// One intent from the app's mutation enum.
///
/// Mutations are intents, not facts: `AddTracks { playlist, tracks }`, never
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
/// # use exo::{ActorId, AutoCtx, Mutation, MutationError, Transaction};
/// # use serde::{Deserialize, Serialize};
/// #[derive(Serialize, Deserialize)]
/// #[serde(tag = "t")]
/// enum Counter {
///     Bump { by: i64 },
/// }
///
/// impl Mutation for Counter {
///     fn apply(&self, tx: &Transaction, _actor: &ActorId) -> Result<(), MutationError> {
///         let Counter::Bump { by } = self;
///         tx.execute("UPDATE counter SET n = n + ?1", [by])?;
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
    fn apply(&self, tx: &Transaction, actor: &ActorId) -> std::result::Result<(), MutationError>;
}

/// The single extension point. An app supplies one mutation enum and its
/// schema; Exo is generic over it.
///
/// Exo owns every table prefixed `exo_`. The app owns everything else.
pub trait App: 'static {
    /// The app's mutation enum.
    type Mutation: Mutation;

    /// Create the app's tables. Must be idempotent: Exo calls it on every open,
    /// and again whenever it rebuilds state from the log.
    fn migrate(conn: &Connection) -> Result<()>;
}
