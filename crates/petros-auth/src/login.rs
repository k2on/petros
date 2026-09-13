//! What a sign-in hands a client, and what a client keeps.

use serde::{Deserialize, Serialize};

/// A signed-in person, as the server knows them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Account {
    /// The stable id — the provider's `sub`, or the name given to a server
    /// in dev mode. What every entry this person authors carries as its
    /// actor.
    pub id: String,
    /// A display name, as the provider had it. May be empty.
    #[serde(default)]
    pub name: String,
    /// May be empty.
    #[serde(default)]
    pub email: String,
}

/// What the server hands back for a login code: everything a client needs to
/// open its database as the right person and prove itself on the socket.
///
/// Keep it — the token is not shown twice. A client that loses it signs in
/// again and gets a new session, which is fine: entries authored under the
/// old one are still this person's, and the server knows that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Login {
    /// Proves the session. Sent in every `Hello`; never in a URL.
    pub token: String,
    /// The session's id, which entries authored under it carry.
    pub session: String,
    pub user: Account,
    /// When the token stops working, as milliseconds since the epoch.
    pub expires_ms: i64,
}
