//! Rebuilding an app's tables from the log when its schema version moves.
//!
//! An app's tables are a function of the log, so a schema change is a rebuild
//! rather than an `ALTER` with a back-fill: [`App::SCHEMA_VERSION`] moves, and
//! the engine drops the tables, recreates them at the new shape, and replays
//! the log through the current `apply`. These check that a database written by
//! an older version comes back correct under a newer one — on the server, which
//! replays the whole log, and on a client, which rewinds its cursor and lets
//! its ordinary catch-up do it.

use diesel::prelude::*;
use petros::{
    ActorId, App, AutoCtx, Client, ClientMsg, Ctx, Entry, Id, Mutation, MutationError, Seq, Server,
    ServerMsg, Transaction,
};
use serde::{Deserialize, Serialize};

/// One mutation: put a thing in the table. The same bytes under both versions.
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

    fn apply(&self, tx: &mut Transaction, _ctx: &Ctx) -> Result<(), MutationError> {
        let ThingMutation::Add { id, name } = self;
        // Raw SQL, because the point of the test is a column the compiled row
        // type does not know about — the new one arrives by default.
        diesel::sql_query("INSERT OR IGNORE INTO thing (id, name) VALUES (?, ?)")
            .bind::<diesel::sql_types::Binary, _>(id.as_uuid().as_bytes().to_vec())
            .bind::<diesel::sql_types::Text, _>(name.clone())
            .execute(tx.conn())?;
        Ok(())
    }
}

/// Version 1: a thing has an id and a name.
struct V1;
impl App for V1 {
    type Mutation = ThingMutation;
    const SCHEMA: &'static str =
        "CREATE TABLE IF NOT EXISTS thing (id BLOB PRIMARY KEY NOT NULL, name TEXT NOT NULL);";
    const SCHEMA_VERSION: u32 = 1;
}

/// Version 2: the same table grew a `note` column, with a default for every row
/// already in the log.
struct V2;
impl App for V2 {
    type Mutation = ThingMutation;
    const SCHEMA: &'static str = "CREATE TABLE IF NOT EXISTS thing (id BLOB PRIMARY KEY NOT NULL, \
         name TEXT NOT NULL, note TEXT NOT NULL DEFAULT 'unset');";
    const SCHEMA_VERSION: u32 = 2;
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

fn has_note_column(conn: &mut petros::Connection) -> bool {
    #[derive(QueryableByName)]
    struct Col {
        #[diesel(sql_type = diesel::sql_types::Text)]
        name: String,
    }
    diesel::sql_query("SELECT name FROM pragma_table_info('thing')")
        .load::<Col>(conn)
        .unwrap()
        .iter()
        .any(|c| c.name == "note")
}

fn log(names: &[&str]) -> Vec<Entry<ThingMutation>> {
    names
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
        .collect()
}

/// The server replays the whole log into the rebuilt tables.
#[test]
fn a_server_rebuilds_its_tables_when_the_schema_version_moves() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("server.db");
    let file = path.to_string_lossy().into_owned();

    // A server on version 1, with a log of three things.
    {
        let mut server = Server::<V1>::open(petros::open_path(&file).unwrap()).unwrap();
        let entries: Vec<Entry<ThingMutation>> = log(&["apple", "pear", "plum"])
            .into_iter()
            .map(|mut e| {
                e.seq = None; // pushed entries are unsequenced
                e
            })
            .collect();
        server.recv(1, ClientMsg::Push { entries }).unwrap();
        let _ = server.take_outgoing();
        assert_eq!(names(server.conn()), vec!["apple", "pear", "plum"]);
        assert!(!has_note_column(server.conn()), "v1 has no note column");
    }

    // Reopened on version 2: the table is rebuilt from the log at the new shape.
    {
        let mut server = Server::<V2>::open(petros::open_path(&file).unwrap()).unwrap();
        assert!(has_note_column(server.conn()), "v2 added the note column");
        assert_eq!(
            names(server.conn()),
            vec!["apple", "pear", "plum"],
            "every row is back, replayed from the log"
        );
        let note: Vec<Name> = diesel::sql_query("SELECT note AS name FROM thing LIMIT 1")
            .load(server.conn())
            .unwrap();
        assert_eq!(note[0].name, "unset", "the new column took its default");
    }
}

/// A client rewinds its cursor and replays the confirmed log into rebuilt tables,
/// and its pending intents survive on top.
#[test]
fn a_client_rebuilds_its_tables_when_the_schema_version_moves() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.db");
    let file = path.to_string_lossy().into_owned();

    // Version 1: confirm two things from the server, and hold one pending.
    {
        let mut client = Client::<V1>::open(
            petros::open_path(&file).unwrap(),
            "alice",
            AutoCtx::seeded(1),
        )
        .unwrap();
        client.connected().unwrap();
        client
            .recv(ServerMsg::Batch {
                entries: log(&["apple", "pear"]),
                has_more: false,
            })
            .unwrap();
        client
            .mutate(ThingMutation::Add {
                id: Id::nil(),
                name: "quince".into(),
            })
            .unwrap();
        assert_eq!(names(client.conn()), vec!["apple", "pear", "quince"]);
        assert_eq!(client.cursor(), 2, "two confirmed");
        assert_eq!(client.pending_len(), 1, "one pending");
    }

    // Version 2: rebuilt from the confirmed log, with the pending thing still on
    // top and the new column present.
    {
        let mut client = Client::<V2>::open(
            petros::open_path(&file).unwrap(),
            "alice",
            AutoCtx::seeded(1),
        )
        .unwrap();
        assert!(has_note_column(client.conn()), "v2 added the note column");
        assert_eq!(
            names(client.conn()),
            vec!["apple", "pear", "quince"],
            "confirmed replayed and the pending intent still on top"
        );
        assert_eq!(
            client.cursor(),
            2,
            "cursor caught back up to the confirmed head"
        );
        assert_eq!(
            client.pending_len(),
            1,
            "the pending intent survived the rebuild"
        );
    }
}

/// Left at the default of 0, none of this runs: the tables are never dropped and
/// an app that never asks for a migration is untouched.
#[test]
fn version_zero_never_rebuilds() {
    struct Inert;
    impl App for Inert {
        type Mutation = ThingMutation;
        const SCHEMA: &'static str =
            "CREATE TABLE IF NOT EXISTS thing (id BLOB PRIMARY KEY NOT NULL, name TEXT NOT NULL);";
        // SCHEMA_VERSION defaults to 0.
    }
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("s.db").to_string_lossy().into_owned();
    let mut server = Server::<Inert>::open(petros::open_path(&file).unwrap()).unwrap();
    let entries: Vec<Entry<ThingMutation>> = log(&["apple"])
        .into_iter()
        .map(|mut e| {
            e.seq = None;
            e
        })
        .collect();
    server.recv(1, ClientMsg::Push { entries }).unwrap();
    let _ = server.take_outgoing();
    drop(server);
    // Reopen: nothing rebuilds, the row is simply still there.
    let mut server = Server::<Inert>::open(petros::open_path(&file).unwrap()).unwrap();
    assert_eq!(names(server.conn()), vec!["apple"]);
}
