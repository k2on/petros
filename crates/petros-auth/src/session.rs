//! The sessions a server has issued, in SQLite.
//!
//! A session is one login on one device. The client keeps its token; this
//! keeps a hash of it, who it belongs to, and when it stops working — and
//! never forgets one, because an entry authored under a session long expired
//! is still that person's and the engine will ask.

use diesel::prelude::*;
use petros::{ActorId, Authenticate, Connection, Identity};
use sha2::{Digest, Sha256};

use crate::{Account, Login};

diesel::table! {
    petros_sessions (id) {
        id -> Text,
        token_hash -> Text,
        user -> Text,
        name -> Text,
        email -> Text,
        issued_ms -> BigInt,
        expires_ms -> BigInt,
        revoked -> Bool,
    }
}

const DDL: &str = "
    CREATE TABLE IF NOT EXISTS petros_sessions (
        id         TEXT PRIMARY KEY NOT NULL,
        token_hash TEXT NOT NULL UNIQUE,
        user       TEXT NOT NULL,
        name       TEXT NOT NULL,
        email      TEXT NOT NULL,
        issued_ms  BIGINT NOT NULL,
        expires_ms BIGINT NOT NULL,
        revoked    BOOLEAN NOT NULL DEFAULT 0
    );
    CREATE INDEX IF NOT EXISTS petros_sessions_user ON petros_sessions (user);
";

#[derive(Debug, Queryable, Selectable, Insertable)]
#[diesel(table_name = petros_sessions, check_for_backend(diesel::sqlite::Sqlite))]
struct Row {
    id: String,
    token_hash: String,
    user: String,
    name: String,
    email: String,
    issued_ms: i64,
    expires_ms: i64,
    revoked: bool,
}

/// Thirty days. A phone that has not spoken to the server in a month signs
/// in again, and loses nothing by it.
pub const DEFAULT_TTL_MS: i64 = 30 * 24 * 60 * 60 * 1000;

/// The store. One per server, behind whatever lock the server keeps it under.
pub struct SessionStore {
    conn: Connection,
    ttl_ms: i64,
    /// The clock, replaceable so a test can move it.
    now: Box<dyn FnMut() -> i64 + Send>,
}

impl std::fmt::Debug for SessionStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStore")
            .field("ttl_ms", &self.ttl_ms)
            .finish_non_exhaustive()
    }
}

impl SessionStore {
    /// Open over a connection of its own — a file beside the log, usually.
    /// Runs the one migration it has.
    pub fn open(mut conn: Connection) -> petros::Result<Self> {
        petros::batch(&mut conn, DDL)?;
        Ok(SessionStore {
            conn,
            ttl_ms: DEFAULT_TTL_MS,
            now: Box::new(now_ms),
        })
    }

    /// How long a session lasts from issue.
    pub fn with_ttl(mut self, ttl_ms: i64) -> Self {
        self.ttl_ms = ttl_ms;
        self
    }

    /// A clock of the test's choosing.
    pub fn with_clock(mut self, now: impl FnMut() -> i64 + Send + 'static) -> Self {
        self.now = Box::new(now);
        self
    }

    /// Sign `account` in: a new session, and the token that proves it.
    pub fn issue(&mut self, account: &Account) -> petros::Result<Login> {
        let token = random_token();
        let issued_ms = (self.now)();
        let row = Row {
            id: random_id(),
            token_hash: hash(&token),
            user: account.id.clone(),
            name: account.name.clone(),
            email: account.email.clone(),
            issued_ms,
            expires_ms: issued_ms + self.ttl_ms,
            revoked: false,
        };
        diesel::insert_into(petros_sessions::table)
            .values(&row)
            .execute(&mut self.conn)?;
        Ok(Login {
            token,
            session: row.id,
            user: account.clone(),
            expires_ms: row.expires_ms,
        })
    }

    /// The login a token proves, if it is live.
    pub fn lookup(&mut self, token: &str) -> petros::Result<Option<Login>> {
        let now = (self.now)();
        let row: Option<Row> = petros_sessions::table
            .select(Row::as_select())
            .filter(petros_sessions::token_hash.eq(hash(token)))
            .first(&mut self.conn)
            .optional()?;
        Ok(row
            .filter(|r| !r.revoked && r.expires_ms > now)
            .map(|r| Login {
                token: token.to_string(),
                session: r.id,
                user: Account {
                    id: r.user,
                    name: r.name,
                    email: r.email,
                },
                expires_ms: r.expires_ms,
            }))
    }

    /// End a session. Its token proves nothing from here; entries authored
    /// under it are still its owner's.
    pub fn revoke(&mut self, token: &str) -> petros::Result<bool> {
        let n = diesel::update(
            petros_sessions::table
                .filter(petros_sessions::token_hash.eq(hash(token)))
                .filter(petros_sessions::revoked.eq(false)),
        )
        .set(petros_sessions::revoked.eq(true))
        .execute(&mut self.conn)?;
        Ok(n > 0)
    }

    /// Whether `session` is or was `user`'s — live, expired or revoked.
    pub fn owned_by(&mut self, user: &str, session: &str) -> petros::Result<bool> {
        let n: i64 = petros_sessions::table
            .filter(petros_sessions::id.eq(session))
            .filter(petros_sessions::user.eq(user))
            .count()
            .get_result(&mut self.conn)?;
        Ok(n > 0)
    }
}

/// What the engine asks. A store that fails to answer answers no.
impl Authenticate for SessionStore {
    fn authenticate(&mut self, token: Option<&str>) -> Option<Identity> {
        let login = self.lookup(token?).ok()??;
        Some(Identity {
            user: ActorId::new(login.user.id),
            session: login.session,
        })
    }

    fn owns(&mut self, user: &ActorId, session: &str) -> bool {
        self.owned_by(user.as_str(), session).unwrap_or(false)
    }
}

/// 128 bits from the OS, as hex: a session id, which entries carry and
/// which is therefore worth keeping short.
pub(crate) fn random_id() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex(&bytes)
}

/// 256 bits from the OS, as hex. Long enough that guessing one is not a
/// thing, short enough to sit in a header.
pub(crate) fn random_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex(&bytes)
}

pub(crate) fn hash(token: &str) -> String {
    hex(&Sha256::digest(token.as_bytes()))
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;

    fn alice() -> Account {
        Account {
            id: "sub-alice".into(),
            name: "Alice".into(),
            email: "alice@example".into(),
        }
    }

    #[test]
    fn a_token_proves_its_session_until_it_does_not() {
        let clock = Arc::new(AtomicI64::new(1_000));
        let tick = clock.clone();
        let mut store = SessionStore::open(petros::open_memory().unwrap())
            .unwrap()
            .with_ttl(100)
            .with_clock(move || tick.load(Ordering::SeqCst));
        let login = store.issue(&alice()).unwrap();
        assert_eq!(login.expires_ms, 1_100);

        let who = store.authenticate(Some(&login.token)).expect("live");
        assert_eq!(who.user.as_str(), "sub-alice");
        assert_eq!(who.session, login.session);
        assert!(store.authenticate(Some("not a token")).is_none());
        assert!(store.authenticate(None).is_none());

        clock.store(1_100, Ordering::SeqCst);
        assert!(store.authenticate(Some(&login.token)).is_none(), "expired");
        // Expired is not forgotten: what was authored under it is still hers.
        assert!(store.owns(&ActorId::from("sub-alice"), &login.session));
        assert!(!store.owns(&ActorId::from("sub-bob"), &login.session));
    }

    #[test]
    fn revoking_ends_the_token_and_keeps_the_ownership() {
        let mut store = SessionStore::open(petros::open_memory().unwrap()).unwrap();
        let login = store.issue(&alice()).unwrap();
        assert!(store.revoke(&login.token).unwrap());
        assert!(!store.revoke(&login.token).unwrap(), "already gone");
        assert!(store.authenticate(Some(&login.token)).is_none());
        assert!(store.owns(&ActorId::from("sub-alice"), &login.session));
    }

    #[test]
    fn the_token_itself_is_not_stored() {
        let mut store = SessionStore::open(petros::open_memory().unwrap()).unwrap();
        let login = store.issue(&alice()).unwrap();
        let hashes: Vec<String> = petros_sessions::table
            .select(petros_sessions::token_hash)
            .load(&mut store.conn)
            .unwrap();
        assert_eq!(hashes, [hash(&login.token)]);
        assert_ne!(hashes[0], login.token);
    }
}
