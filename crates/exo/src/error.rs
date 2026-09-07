//! Error types. Library code never panics: everything fallible returns these.

use crate::Uuid;

/// What an [`apply`](crate::Mutation::apply) can go wrong with.
///
/// The distinction matters: [`MutationError::Rejected`] is a *verdict* about
/// the mutation itself and must be reached identically by every replica, since
/// the server rejecting an entry and the client rolling it back have to agree.
/// [`MutationError::Sqlite`] is an infrastructure failure and says nothing
/// about the mutation.
#[derive(Debug, thiserror::Error)]
pub enum MutationError {
    /// The mutation is not valid against this state. Deterministic.
    #[error("rejected: {0}")]
    Rejected(String),
    /// The database failed. Not a verdict about the mutation.
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
}

impl MutationError {
    /// Reject the mutation with a human-readable reason.
    ///
    /// ```
    /// # use exo::MutationError;
    /// let e = MutationError::rejected("a to-do needs some text");
    /// assert_eq!(e.to_string(), "rejected: a to-do needs some text");
    /// ```
    pub fn rejected(reason: impl Into<String>) -> Self {
        MutationError::Rejected(reason.into())
    }
}

/// Anything Exo can fail with.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Mutation(#[from] MutationError),
    #[error("could not encode payload: {0}")]
    Encode(String),
    #[error("could not decode payload: {0}")]
    Decode(String),
    /// The peer said something that cannot be true.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The transport broke. Only ever produced by `exo::transport`.
    #[error("transport: {0}")]
    Transport(String),
    /// A pushed entry arrived without the client-generated id the log needs.
    #[error("entry {0} is missing a sequence number")]
    MissingSeq(Uuid),
}

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, Error>;
