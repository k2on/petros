//! The two builds of one `apply` have to agree.
//!
//! `crates/todo` holds the domain. The server and the terminal peers link it
//! and call it; the phone loads it compiled to wasm so a new mutation reaches
//! it over Metro without a native build. That is two builds of one source,
//! which is not two implementations — but "not two implementations" is a claim,
//! and this is what makes it a checked one.
//!
//! Every verb, through both, against real SQLite, comparing the rows.

use diesel::connection::SimpleConnection;
use diesel::deserialize::QueryableByName;
use diesel::sql_types::{BigInt, Text};
use diesel::{sql_query, RunQueryDsl};
use petros::{AutoCtx, Connection};
use petros_mutators::Mutators;

const MODULE: &[u8] = petros_mutators::BUNDLED;

#[derive(QueryableByName, Debug, PartialEq)]
struct Row {
    #[diesel(sql_type = Text)]
    id: String,
    #[diesel(sql_type = Text)]
    text: String,
    #[diesel(sql_type = BigInt)]
    done: i64,
    #[diesel(sql_type = BigInt)]
    pos: i64,
    #[diesel(sql_type = Text)]
    actor: String,
}

fn database() -> Connection {
    let mut conn = petros::open_memory().expect("open");
    conn.batch_execute(
        "CREATE TABLE todo (
             id BLOB PRIMARY KEY NOT NULL, text TEXT NOT NULL,
             done BOOL NOT NULL DEFAULT 0, pos BIGINT NOT NULL,
             created_ms BIGINT NOT NULL, actor TEXT NOT NULL);",
    )
    .expect("migrate");
    conn
}

fn rows(conn: &mut Connection) -> Vec<Row> {
    sql_query("SELECT hex(id) AS id, text, done, pos, actor FROM todo ORDER BY pos, id")
        .load(conn)
        .expect("read back")
}

/// The native side, exactly as `todo::Payload`'s `Mutation::apply` runs it.
struct Native<'a>(&'a mut Connection);

impl todo::domain::Host for Native<'_> {
    fn query_int(&mut self, sql: &str) -> i64 {
        #[derive(QueryableByName)]
        struct V {
            #[diesel(sql_type = BigInt)]
            v: i64,
        }
        sql_query(format!("SELECT ({sql}) AS v"))
            .load::<V>(&mut *self.0)
            .ok()
            .and_then(|r| r.first().map(|r| r.v))
            .unwrap_or(0)
    }
    fn query_exists(&mut self, sql: &str) -> bool {
        #[derive(QueryableByName)]
        struct V {
            #[diesel(sql_type = BigInt)]
            v: i64,
        }
        sql_query(format!("SELECT EXISTS({sql}) AS v"))
            .load::<V>(&mut *self.0)
            .ok()
            .and_then(|r| r.first().map(|r| r.v != 0))
            .unwrap_or(false)
    }
    fn exec(&mut self, sql: &str) {
        let _ = self.0.batch_execute(sql);
    }
}

fn encode(p: &todo::Payload) -> Vec<u8> {
    let mut out = Vec::new();
    ciborium::into_writer(&p.0, &mut out).unwrap();
    out
}

/// A session of mutations, run both ways from the same authored payloads.
///
/// The payloads are filled once and handed to both, so that `apply` is compared
/// against the same input rather than against two different rolls of the dice.
/// That leaves `fill_auto` itself uncovered, which
/// [`fill_auto_agrees_between_the_two_builds`] exists to close — a gap found by
/// trying to make this test fail and watching it pass.
fn both_ways(script: &[(&str, serde_json::Value)]) -> (Vec<Row>, Vec<Row>) {
    let mut auto = AutoCtx::seeded(4);
    let payloads: Vec<todo::Payload> = script
        .iter()
        .map(|(kind, args)| {
            let mut p = todo::from_value(kind, args.clone()).expect("author");
            <todo::Payload as petros::Mutation>::fill_auto(&mut p, &mut auto);
            p
        })
        .collect();

    let mut native_db = database();
    for p in &payloads {
        // A refusal is a legitimate outcome; both sides must reach the same one.
        let _ = todo::domain::apply(&mut Native(&mut native_db), &p.0, "alice");
    }

    let module = Mutators::load(MODULE).expect("load the module");
    let mut wasm_db = database();
    for p in &payloads {
        let _ = module
            .apply(&mut wasm_db, &encode(p), "alice")
            .expect("the host ran");
    }

    (rows(&mut native_db), rows(&mut wasm_db))
}

#[test]
fn every_verb_produces_the_same_rows_natively_and_in_wasm() {
    use serde_json::json;

    let id = "67e55084-765d-446c-9191-4ff9861f6d8e";
    let script: Vec<(&str, serde_json::Value)> = vec![
        ("Add", json!({ "text": "buy milk" })),
        ("Add", json!({ "text": "  buy oats  " })),
        // Refused by both, and refused identically.
        ("Add", json!({ "text": "   " })),
        ("AddFive", json!({})),
        ("MarkAllDone", json!({})),
        ("Add", json!({ "text": "after the sweep" })),
        // A row nobody has: a no-op, not an error.
        ("SetDone", json!({ "id": id, "done": true })),
        ("Remove", json!({ "id": id })),
        // A verb neither build knows.
        ("Frobnicate", json!({})),
    ];

    let (native, wasm) = both_ways(&script);

    assert_eq!(native, wasm, "the two builds of `apply` disagree");
    assert!(
        !native.is_empty(),
        "the script should have written something"
    );
    // And the rows are the ones the script describes, so a shared bug that
    // wrote nothing at all could not pass.
    let texts: Vec<&str> = native.iter().map(|r| r.text.as_str()).collect();
    assert_eq!(
        texts,
        vec![
            "buy milk",
            "buy oats",
            "item 1",
            "item 2",
            "item 3",
            "item 4",
            "item 5",
            "after the sweep"
        ]
    );
    assert!(native.iter().take(7).all(|r| r.done == 1), "MarkAllDone");
    assert_eq!(native[7].done, 0, "added after the sweep");
}

#[test]
fn refusals_match_too() {
    let module = Mutators::load(MODULE).expect("load");
    let mut auto = AutoCtx::seeded(11);

    for (kind, args) in [
        ("Add", serde_json::json!({ "text": "" })),
        ("Frobnicate", serde_json::json!({})),
    ] {
        let mut p = todo::from_value(kind, args).expect("author");
        <todo::Payload as petros::Mutation>::fill_auto(&mut p, &mut auto);

        let mut a = database();
        let native = todo::domain::apply(&mut Native(&mut a), &p.0, "alice");
        let mut b = database();
        let wasm = module
            .apply(&mut b, &encode(&p), "alice")
            .expect("host ran");

        assert_eq!(
            native, wasm,
            "{kind} is refused differently by the two builds"
        );
        assert!(native.is_err(), "{kind} should be refused");
    }
}

/// `fill_auto` has to agree too, and the test above cannot see it.
///
/// It runs once, at the authoring peer, and both sides then apply whatever it
/// produced — so a difference there is invisible to a comparison of `apply`.
/// It is also the one place non-determinism is allowed, which makes it the
/// worst place for the two builds to drift: the log would freeze whichever
/// answer the authoring peer happened to be built with.
#[test]
fn fill_auto_agrees_between_the_two_builds() {
    let module = Mutators::load(MODULE).expect("load");

    for kind in ["Add", "AddFive", "MarkAllDone", "SetDone"] {
        let args = match kind {
            "SetDone" => {
                serde_json::json!({ "id": "67e55084-765d-446c-9191-4ff9861f6d8e", "done": true })
            }
            "Add" => serde_json::json!({ "text": "buy milk" }),
            _ => serde_json::json!({}),
        };

        // The same seed both ways: the uuid and the clock are the input, not
        // the thing being compared.
        let seeded = || {
            let mut a = AutoCtx::seeded(19);
            let id = a.uuid().as_uuid().as_bytes().to_vec();
            (id, a.now_ms())
        };
        let (uuid, now) = seeded();

        let mut native = todo::from_value(kind, args.clone()).expect("author").0;
        todo::domain::fill_auto(&mut native, uuid.clone(), now);

        let authored = todo::from_value(kind, args).expect("author");
        let from_wasm = module
            .fill_auto_with(&encode(&authored), &uuid, now)
            .expect("the module filled it");
        let from_wasm: ciborium::value::Value =
            ciborium::from_reader(from_wasm.as_slice()).expect("decode");

        assert_eq!(
            native, from_wasm,
            "`{kind}` is filled differently by the two builds"
        );
    }
}
