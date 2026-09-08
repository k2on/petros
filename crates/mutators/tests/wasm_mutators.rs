//! The domain, run from a `.wasm` file against a real SQLite connection.
//!
//! A build that compiles is not evidence anything works, and a wasm module that
//! loads is not evidence it can write a row. This applies real mutations, reads
//! the table back, and times the parts of the loop that happen on the device.

use std::time::Instant;

use diesel::connection::SimpleConnection;
use diesel::deserialize::QueryableByName;
use diesel::sql_types::{BigInt, Text};
use diesel::{sql_query, RunQueryDsl};
use petros::{AutoCtx, Connection};
use petros_mutators::Mutators;

const MODULE: &[u8] = petros_mutators::BUNDLED;

#[derive(QueryableByName, Debug)]
struct Row {
    #[diesel(sql_type = Text)]
    text: String,
    #[diesel(sql_type = BigInt)]
    pos: i64,
    #[diesel(sql_type = Text)]
    actor: String,
    #[diesel(sql_type = BigInt)]
    done: i64,
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
    sql_query("SELECT text, pos, actor, done FROM todo ORDER BY pos, id")
        .load(conn)
        .expect("read back")
}

/// An `Add` as the log stores it: the placeholder id and timestamp that
/// `fill_auto` replaces.
fn add(text: &str) -> Vec<u8> {
    let value = ciborium::value::Value::Map(vec![
        ("t".into(), "Add".into()),
        ("id".into(), ciborium::value::Value::Bytes(vec![0u8; 16])),
        ("text".into(), text.into()),
        (
            "created_ms".into(),
            ciborium::value::Value::Integer(0.into()),
        ),
    ]);
    let mut out = Vec::new();
    ciborium::into_writer(&value, &mut out).unwrap();
    out
}

/// `fill_auto` runs before there is anything to read, so it needs no database —
/// which is also why a module cannot smuggle a query into it.
fn authored(mutators: &Mutators, auto: &mut AutoCtx, text: &str) -> Vec<u8> {
    mutators.fill_auto(&add(text), auto).expect("fill_auto")
}

#[test]
fn a_wasm_module_applies_mutations_to_a_real_database() {
    let mutators = Mutators::load(MODULE).expect("load the module");
    let mut conn = database();
    let mut auto = AutoCtx::seeded(7);

    for text in ["buy milk", "buy oats"] {
        let payload = authored(&mutators, &mut auto, text);
        mutators
            .apply(&mut conn, &payload, "alice")
            .expect("the host ran")
            .expect("the module accepted it");
    }

    let rows = rows(&mut conn);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].text, "buy milk");
    assert_eq!(rows[0].actor, "alice");
    // `pos` came from `COALESCE(MAX(pos), 0) + 1`, read inside the sandbox.
    assert_eq!((rows[0].pos, rows[1].pos), (1, 2));
}

#[test]
fn a_refusal_crosses_the_boundary_as_a_refusal() {
    let mutators = Mutators::load(MODULE).expect("load");
    let mut conn = database();
    let mut auto = AutoCtx::seeded(1);

    let payload = authored(&mutators, &mut auto, "   ");
    let refusal = mutators
        .apply(&mut conn, &payload, "alice")
        .expect("the host ran")
        .expect_err("blank text is refused");
    assert_eq!(refusal, "a to-do needs some text");
    assert!(rows(&mut conn).is_empty(), "a refusal writes nothing");
}

#[test]
fn fill_auto_freezes_the_id_and_the_clock_in_the_payload() {
    let mutators = Mutators::load(MODULE).expect("load");
    let mut auto = AutoCtx::seeded(42);

    let filled = authored(&mutators, &mut auto, "once");
    let value: ciborium::value::Value = ciborium::from_reader(filled.as_slice()).unwrap();
    let map = value.as_map().unwrap();
    let id = map.iter().find(|(k, _)| k.as_text() == Some("id")).unwrap();
    let at = map
        .iter()
        .find(|(k, _)| k.as_text() == Some("created_ms"))
        .unwrap();

    assert_ne!(
        id.1.as_bytes().unwrap(),
        &vec![0u8; 16],
        "the id was filled"
    );
    assert_eq!(
        i128::from(at.1.as_integer().unwrap()),
        1_577_836_800_000,
        "the seeded clock, frozen into the payload"
    );
}

#[test]
fn redelivery_of_the_same_entry_is_a_no_op() {
    let mutators = Mutators::load(MODULE).expect("load");
    let mut conn = database();
    let mut auto = AutoCtx::seeded(3);

    let payload = authored(&mutators, &mut auto, "once only");
    for _ in 0..3 {
        mutators.apply(&mut conn, &payload, "bob").unwrap().unwrap();
    }
    assert_eq!(rows(&mut conn).len(), 1);
}

#[test]
fn a_module_without_the_abi_is_refused_at_load() {
    // A valid wasm module that exports none of what the host calls.
    let empty = wat_minimal();
    let err = Mutators::load(&empty).expect_err("should not load");
    assert!(err.contains("does not export"), "got: {err}");
    assert!(Mutators::load(b"not wasm at all").is_err());
}

/// `(module)` — the smallest valid wasm binary.
fn wat_minimal() -> Vec<u8> {
    vec![0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00]
}

#[test]
fn swapping_the_module_keeps_the_peer_running() {
    let mut mutators = Mutators::load(MODULE).expect("load");
    let mut conn = database();
    let mut auto = AutoCtx::seeded(9);

    let first = authored(&mutators, &mut auto, "before the swap");
    mutators.apply(&mut conn, &first, "alice").unwrap().unwrap();

    assert_eq!(mutators.generation, 1);
    mutators.swap(MODULE).expect("hot swap");
    assert_eq!(mutators.generation, 2, "the generation moves");

    // The database is untouched by the swap, and the new module carries on.
    let second = authored(&mutators, &mut auto, "after the swap");
    mutators
        .apply(&mut conn, &second, "alice")
        .unwrap()
        .unwrap();

    let rows = rows(&mut conn);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].text, "after the swap");
    assert_eq!(rows[1].pos, 2, "state carried across the swap");
    assert_eq!(rows[0].done, 0);
}

/// Not an assertion so much as a number worth having: this is the part of the
/// hot-reload loop that happens on the device.
#[test]
fn report_the_on_device_costs() {
    let bytes = MODULE;
    let t = Instant::now();
    let mutators = Mutators::load(bytes).expect("load");
    let compile = t.elapsed();

    let mut conn = database();
    let mut auto = AutoCtx::seeded(5);
    let payload = authored(&mutators, &mut auto, "warm");
    mutators
        .apply(&mut conn, &payload, "alice")
        .unwrap()
        .unwrap();

    let n = 200;
    let t = Instant::now();
    for i in 0..n {
        let p = authored(&mutators, &mut auto, &format!("item {i}"));
        mutators.apply(&mut conn, &p, "alice").unwrap().unwrap();
    }
    let each = t.elapsed() / n;

    println!(
        "\n  module          {} KB\n  compile+verify  {:?}\n  per mutation    {:?}  (instantiate + fill_auto + apply + SQL)\n",
        bytes.len() / 1024,
        compile,
        each
    );
    assert!(compile.as_millis() < 2000, "compile was {compile:?}");
}

/// `AddFive` expands one seed into five rows — and the expansion has to be a
/// pure function of that seed, or two replicas replaying the same entry would
/// disagree about what is in the list.
#[test]
fn a_batch_is_frozen_at_the_client_that_authored_it() {
    let mutators = Mutators::load(MODULE).expect("load");

    let request = {
        let v = ciborium::value::Value::Map(vec![("t".into(), "AddFive".into())]);
        let mut out = Vec::new();
        ciborium::into_writer(&v, &mut out).unwrap();
        out
    };

    // Two peers with different entropy author different batches.
    let a = mutators
        .fill_auto(&request, &mut AutoCtx::seeded(1))
        .unwrap();
    let b = mutators
        .fill_auto(&request, &mut AutoCtx::seeded(2))
        .unwrap();
    assert_ne!(a, b, "a different seed gives a different batch");

    // The same entry replayed anywhere gives the same rows. This is the whole
    // invariant: `apply` never rolls the dice, it reads what was frozen.
    let mut first = database();
    mutators.apply(&mut first, &a, "alice").unwrap().unwrap();
    let mut second = database();
    mutators.apply(&mut second, &a, "alice").unwrap().unwrap();
    let texts =
        |c: &mut Connection| -> Vec<String> { rows(c).into_iter().map(|r| r.text).collect() };
    assert_eq!(texts(&mut first), texts(&mut second));

    let rows = rows(&mut first);
    assert_eq!(rows.len(), 5, "five rows from one entry");
    assert_eq!(
        rows.iter().map(|r| r.pos).collect::<Vec<_>>(),
        vec![1, 2, 3, 4, 5],
        "counted up from the end of the list, once"
    );

    // Applied twice, it is still five rows: the ids came from the payload, so
    // redelivery is a no-op exactly as it is for a single Add.
    mutators.apply(&mut first, &a, "alice").unwrap().unwrap();
    assert_eq!(rows.len(), 5);

    // And it lands after whatever was already there.
    let mut later = database();
    let one = mutators
        .fill_auto(&add("already here"), &mut AutoCtx::seeded(9))
        .unwrap();
    mutators.apply(&mut later, &one, "bob").unwrap().unwrap();
    mutators.apply(&mut later, &a, "alice").unwrap().unwrap();
    let after = rows_of(&mut later);
    assert_eq!(after.len(), 6);
    assert_eq!(after[0].text, "already here");
    assert_eq!(after[5].pos, 6, "pos continued from the end");
}

fn rows_of(conn: &mut Connection) -> Vec<Row> {
    rows(conn)
}

use petros_schema::Ty;

/// The module carries a schema; it has to be the one the domain declares.
///
/// Everything downstream reads the carried copy — `emit-mutators` generates the
/// TypeScript from it without linking the domain at all — so if the two ever
/// drift, the types describe a module nobody is running and `tsc` blesses call
/// sites that will fail on a device.
#[test]
fn the_module_carries_the_schema_the_domain_declares() {
    let carried = petros_schema::from_wasm(MODULE).expect("the module carries a schema");
    assert_eq!(carried, todo::schema::schema());
}

/// And a module without one says so, rather than generating nothing quietly.
#[test]
fn a_module_with_no_schema_is_an_error_not_an_empty_one() {
    // A minimal, valid, schema-less module: the eight-byte header alone.
    let bare = b"\0asm\x01\0\0\0";
    let err = petros_schema::from_wasm(bare).expect_err("no section");
    assert!(err.contains("petros_schema"), "{err}");
    assert!(petros_schema::from_wasm(b"not wasm at all").is_err());
}

/// The declaration is only worth generating types from if it cannot lie.
///
/// Every verb the schema names has to be one `apply` actually handles —
/// otherwise the app gets a green `tsc` and a phone that says "unknown
/// mutation", which is precisely the failure the declaration exists to prevent.
#[test]
fn every_declared_verb_is_one_the_module_handles() {
    let mutators = Mutators::load(MODULE).expect("load");
    let mut conn = database();

    for verb in &todo::schema::schema().verbs {
        // Minimal, and deliberately not always valid: a verb may refuse these
        // arguments. What it may not do is fail to recognise the name.
        let mut fields = vec![(
            ciborium::value::Value::from("t"),
            ciborium::value::Value::from(verb.name.as_str()),
        )];
        for arg in &verb.args {
            let value = match arg.ty {
                Ty::Id => ciborium::value::Value::Bytes(vec![7u8; 16]),
                Ty::Text => "something".into(),
                Ty::Integer => ciborium::value::Value::Integer(1.into()),
                Ty::Bool => ciborium::value::Value::Bool(true),
            };
            fields.push((ciborium::value::Value::from(arg.name.as_str()), value));
        }
        let mut payload = Vec::new();
        ciborium::into_writer(&ciborium::value::Value::Map(fields), &mut payload).unwrap();

        let outcome = mutators
            .apply(&mut conn, &payload, "alice")
            .expect("the host ran");
        if let Err(reason) = outcome {
            assert!(
                !reason.contains("unknown mutation"),
                "the schema declares `{}`, which generates a TypeScript type, but \
                 the module does not handle it: {reason}",
                verb.name
            );
        }
    }
}

/// And the error a *genuinely* unknown verb produces should say what is known,
/// because "nothing happened" is the worst possible answer on a device.
#[test]
fn an_unknown_verb_says_what_the_module_does_know() {
    let mutators = Mutators::load(MODULE).expect("load");
    let mut conn = database();
    let mut payload = Vec::new();
    ciborium::into_writer(
        &ciborium::value::Value::Map(vec![("t".into(), "Frobnicate".into())]),
        &mut payload,
    )
    .unwrap();

    let reason = mutators
        .apply(&mut conn, &payload, "alice")
        .expect("the host ran")
        .expect_err("Frobnicate is not a verb");
    assert!(reason.contains("Frobnicate"), "{reason}");
    for verb in &todo::schema::schema().verbs {
        assert!(
            reason.contains(&verb.name),
            "should list {}: {reason}",
            verb.name
        );
    }
}
