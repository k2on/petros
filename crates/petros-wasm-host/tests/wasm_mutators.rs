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
use petros_wasm_host::Mutators;

/// The module under test, read straight from where `just mutators` puts it.
/// A test fixture rather than part of the crate: `petros-wasm-host` runs
/// modules and has no idea which one you mean.
const MODULE: &[u8] = include_bytes!("../../../target/wasm32-unknown-unknown/mutators/todo.wasm");

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

fn rows_of(conn: &mut Connection) -> Vec<Row> {
    rows(conn)
}

use petros_schema::Ty;

/// The module carries a schema; it has to be the one the domain declares.
///
/// Everything downstream reads the carried copy — `petros-codegen` generates the
/// TypeScript from it without linking the domain at all — so if the two ever
/// drift, the types describe a module nobody is running and `tsc` blesses call
/// sites that will fail on a device.
#[test]
fn the_module_carries_the_schema_the_domain_declares() {
    let carried = petros_schema::from_wasm(MODULE).expect("the module carries a schema");
    assert_eq!(carried, todo::schema());
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

    for verb in &todo::schema().verbs {
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

/// An argument the schema names has to be the field `apply` actually reads.
///
/// This is the hole the declaration used to have. The schema said
/// `Add { text: Text }` and `apply` reached for a field it named itself; if the
/// two drifted, `petros-codegen` emitted TypeScript describing a module nobody
/// was running and the call site type-checked all the way to a device. Nothing
/// caught it — `every_declared_verb_is_one_the_module_handles` only asserts the
/// verb is not *unknown*, which an argument rename does not change.
///
/// `mutations!` makes it structural: the name is written once. This asserts the
/// property anyway, so that going back to hand-written dispatch fails here
/// rather than on someone's phone.
#[test]
fn a_renamed_argument_changes_what_the_module_does() {
    let mutators = Mutators::load(MODULE).expect("load");

    for verb in &todo::schema().verbs {
        for arg in &verb.args {
            // A payload the module would accept, with its auto fields already
            // hoisted in — the same route a real client takes.
            let mut fields = vec![(
                ciborium::value::Value::from("t"),
                ciborium::value::Value::from(verb.name.as_str()),
            )];
            for a in &verb.args {
                let value = match a.ty {
                    Ty::Id => ciborium::value::Value::Bytes(vec![9u8; 16]),
                    Ty::Text => "a real to-do".into(),
                    Ty::Integer => ciborium::value::Value::Integer(1.into()),
                    Ty::Bool => ciborium::value::Value::Bool(true),
                };
                fields.push((ciborium::value::Value::from(a.name.as_str()), value));
            }
            let mut raw = Vec::new();
            ciborium::into_writer(&ciborium::value::Value::Map(fields), &mut raw).unwrap();
            let filled = mutators
                .fill_auto_with(&raw, &[3u8; 16], 1_700_000_000)
                .unwrap();

            let straight = outcome(&mutators, &filled);
            let renamed = outcome(&mutators, &rename(&filled, &arg.name));
            assert_ne!(
                straight, renamed,
                "the schema declares `{}.{}`, but renaming it changed nothing — \
                 `apply` is reading some other field, and the generated \
                 TypeScript is describing a module that does not exist",
                verb.name, arg.name
            );
        }
    }
}

/// Apply one payload to a fresh database: the verdict, and what it left behind,
/// rendered so two runs can be compared.
fn outcome(mutators: &Mutators, payload: &[u8]) -> String {
    let mut conn = database();
    // A row with the id the payloads carry. Without it `SetDone` and `Remove`
    // match nothing and both spellings leave an empty table behind, which made
    // this test pass for the wrong reason the first time it ran.
    diesel::connection::SimpleConnection::batch_execute(
        &mut conn,
        "INSERT INTO todo (id, text, done, pos, created_ms, actor) \
         VALUES (X'09090909090909090909090909090909', 'already here', 0, 1, 0, 'bob')",
    )
    .expect("seed");
    let verdict = mutators
        .apply(&mut conn, payload, "alice")
        .expect("the host ran");
    let rows: Vec<String> = rows_of(&mut conn)
        .iter()
        .map(|r| format!("{}|{}|{}|{}", r.text, r.pos, r.actor, r.done))
        .collect();
    format!("{verdict:?} {}", rows.join(","))
}

/// The same payload with one key spelled differently.
fn rename(payload: &[u8], field: &str) -> Vec<u8> {
    let mut value: ciborium::value::Value = ciborium::from_reader(payload).unwrap();
    if let ciborium::value::Value::Map(entries) = &mut value {
        for (k, _) in entries.iter_mut() {
            if k.as_text() == Some(field) {
                *k = ciborium::value::Value::Text(format!("not_{field}"));
            }
        }
    }
    let mut out = Vec::new();
    ciborium::into_writer(&value, &mut out).unwrap();
    out
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
    for verb in &todo::schema().verbs {
        assert!(
            reason.contains(&verb.name),
            "should list {}: {reason}",
            verb.name
        );
    }
}

/// One instance serves every call, so the guest has to give its buffers back.
///
/// It used to leak on purpose — allocate and forget, in both directions — and
/// that was sound only while each call got a fresh instance and the whole linear
/// memory went with it. Reusing the instance is what made a mutation cheap; this
/// is the invariant that makes reusing it safe, and it would regress silently.
#[test]
fn a_reused_instance_does_not_grow() {
    let mutators = Mutators::load(MODULE).expect("load");
    let mut db = database();
    let mut auto = AutoCtx::seeded(3);

    // Big enough to matter. A hundred small buffers fit inside the slack the
    // guest's allocator already holds, so a leak would not show — the text is
    // 32KiB so a hundred calls would be megabytes if nothing came back.
    let bulk = "x".repeat(32 * 1024);
    let mut pages = Vec::new();
    for i in 0..120 {
        let raw = todo::add(format!("item {i} {bulk}"));
        let mut bytes = Vec::new();
        ciborium::into_writer(&raw, &mut bytes).unwrap();
        let filled = mutators.fill_auto(&bytes, &mut auto).unwrap();
        mutators.apply(&mut db, &filled, "alice").unwrap().unwrap();
        if i % 20 == 19 {
            pages.push(mutators.memory_pages());
        }
    }

    // Some growth on the way up is fine — the allocator claims pages and keeps
    // them. What must not happen is growth that tracks the call count.
    let first = pages.first().copied().unwrap_or(0);
    let last = pages.last().copied().unwrap_or(0);
    assert_eq!(
        first, last,
        "guest memory grew from {first} to {last} pages over 120 calls of 32KiB: {pages:?}"
    );
}

/// A module's writes have to report what they changed, or a phone cannot
/// maintain a view.
///
/// The host builds a store per request and drops it, so the changes had
/// nowhere to go and were lost the moment the request returned. They are
/// collected across the whole apply now, which is the unit a mutation is.
#[test]
fn a_module_reports_what_it_changed() {
    let m = Mutators::load(MODULE).unwrap();
    let mut conn = database();
    let mut auto = petros::AutoCtx::seeded(1);

    let payload = filled(&m, &mut auto, todo::add("first".into()));
    let changes = m.apply(&mut conn, &payload, "alice").unwrap().unwrap();
    assert_eq!(changes.len(), 1, "one row added: {changes:?}");
    assert!(
        matches!(&changes[0], petros_schema::Change::Add { table, .. } if table == "todo"),
        "{changes:?}"
    );

    // A verb that writes several rows reports several: the collection is per
    // apply, not per request.
    let payload = filled(&m, &mut auto, todo::add("second".into()));
    m.apply(&mut conn, &payload, "alice").unwrap().unwrap();
    let payload = filled(&m, &mut auto, todo::mark_all_done());
    let changes = m.apply(&mut conn, &payload, "alice").unwrap().unwrap();
    assert_eq!(changes.len(), 2, "both to-dos were edited: {changes:?}");
    assert!(
        changes
            .iter()
            .all(|c| matches!(c, petros_schema::Change::Edit { .. })),
        "{changes:?}"
    );

    // And a fresh apply does not repeat what an earlier one reported.
    let payload = filled(&m, &mut auto, todo::add("third".into()));
    let changes = m.apply(&mut conn, &payload, "alice").unwrap().unwrap();
    assert_eq!(changes.len(), 1);
}

fn filled(m: &Mutators, auto: &mut petros::AutoCtx, raw: petros_schema::cbor::Value) -> Vec<u8> {
    let mut bytes = Vec::new();
    ciborium::into_writer(&raw, &mut bytes).unwrap();
    m.fill_auto(&bytes, auto).unwrap()
}
