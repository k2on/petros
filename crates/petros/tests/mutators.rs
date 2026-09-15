//! A peer whose `apply` arrives at runtime must install it **before** it opens.
//!
//! Most peers link `apply`, so it is there before anything else is. One does
//! not: the phone loads it as a wasm module, installed by a call. That makes
//! the order a real question, and there is only one answer — because opening a
//! client replays. `Client::open` finishes any confirmed entries the log is
//! ahead on, and replaying a confirmed entry *is* calling `apply`.
//!
//! Get the order wrong and it does not merely fail once. The first run stores
//! entries it cannot apply; every run after that meets them at open and fails
//! there, before the app has a chance to install anything. The database is
//! sound, the module is in the bundle, and the peer can never be opened again.
//!
//! So this walks that exact sequence: no module, a batch from the server, a
//! reopen that fails, and the same reopen succeeding once the module is
//! installed first.

use std::sync::atomic::{AtomicBool, Ordering};

use diesel::prelude::*;
use petros::{
    ActorId, App, AutoCtx, Client, Ctx, Entry, Id, Mutation, MutationError, Seq, ServerMsg,
    Transaction,
};
use serde::{Deserialize, Serialize};

/// Whether the module is installed. A `static` for the same reason the real one
/// is: the interpreter is the process's, not the peer's.
static LOADED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t")]
enum ThingMutation {
    Add { id: Id, name: String },
}

impl Mutation for ThingMutation {
    fn fill_auto(&mut self, ctx: &mut AutoCtx) {
        let ThingMutation::Add { id, .. } = self;
        if id.is_nil() {
            *id = ctx.uuid();
        }
    }

    /// What `petros::foreign_peer!` does: look the module up, and refuse when
    /// there is not one.
    fn apply(&self, tx: &mut Transaction, _ctx: &Ctx) -> Result<(), MutationError> {
        if !LOADED.load(Ordering::SeqCst) {
            return Err(MutationError::rejected(
                "no mutator module is loaded; install one before opening a peer \
                 or authoring a mutation",
            ));
        }
        let ThingMutation::Add { id, name } = self;
        diesel::sql_query("INSERT OR IGNORE INTO thing (id, name) VALUES (?, ?)")
            .bind::<diesel::sql_types::Binary, _>(id.as_uuid().as_bytes().to_vec())
            .bind::<diesel::sql_types::Text, _>(name.clone())
            .execute(tx.conn())?;
        Ok(())
    }
}

struct Thing;
impl App for Thing {
    type Mutation = ThingMutation;
    const SCHEMA: &'static str =
        "CREATE TABLE IF NOT EXISTS thing (id BLOB PRIMARY KEY NOT NULL, name TEXT NOT NULL);";
}

#[derive(QueryableByName)]
struct Name {
    #[diesel(sql_type = diesel::sql_types::Text)]
    name: String,
}

fn names(conn: &mut petros::Connection) -> Vec<String> {
    diesel::sql_query("SELECT name FROM thing ORDER BY name")
        .load::<Name>(conn)
        .unwrap()
        .into_iter()
        .map(|r| r.name)
        .collect()
}

fn batch(names: &[&str]) -> ServerMsg<ThingMutation> {
    ServerMsg::Batch {
        entries: names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let mut e = Entry::new(
                    Id::from_u128(i as u128 + 1),
                    ActorId::from("alice"),
                    ThingMutation::Add {
                        id: Id::from_u128(i as u128 + 1),
                        name: (*n).to_string(),
                    },
                );
                e.seq = Some(i as Seq + 1);
                e
            })
            .collect(),
        has_more: false,
    }
}

/// One test rather than three, because they share the `static` and the point is
/// the sequence.
#[test]
fn a_peer_whose_apply_is_installed_must_install_it_before_opening() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("peer.db").to_string_lossy().into_owned();
    let open = |file: &str| {
        Client::<Thing>::open(
            petros::open_path(file).unwrap(),
            "alice",
            AutoCtx::seeded(1),
        )
    };

    // The first run, with the module not yet installed — which is what an app
    // that installs after opening looks like for as long as that takes.
    LOADED.store(false, Ordering::SeqCst);
    {
        let mut client = open(&file).expect("an empty database opens without a module");
        client.connected().unwrap();
        // The server speaks. The entries are stored, and they do not apply.
        assert!(
            client.recv(batch(&["apple", "pear"])).is_err(),
            "nothing can apply them"
        );
        assert_eq!(client.cursor(), 0, "so the cursor did not move");
    }

    // …and from here the peer is bricked. Not once: every time, for ever,
    // because the failure is in `open` and the app installs after `open`.
    LOADED.store(false, Ordering::SeqCst);
    let again = open(&file);
    assert!(
        again.is_err(),
        "a log the peer is behind on is replayed at open, so open needs apply"
    );

    // The fix is the order and nothing else: the same database, the same
    // entries, the module installed first.
    LOADED.store(true, Ordering::SeqCst);
    let mut client = open(&file).expect("with a module, the stored entries replay");
    assert_eq!(names(client.conn()), vec!["apple", "pear"]);
    assert_eq!(client.cursor(), 2, "and the peer is caught up");
}
