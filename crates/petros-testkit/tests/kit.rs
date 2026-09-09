//! The testkit, exercised against the demo domain — which is the point of it
//! existing as a crate rather than as a file inside the engine's tests.

use diesel::connection::SimpleConnection;
use petros_testkit::{state_hash, Sim};
use todo::TodoApp;

/// What an app actually wants from this: mutate everywhere, break the network,
/// and find every replica agreeing afterwards.
#[test]
fn a_real_domain_converges_across_a_broken_network() {
    let mut sim = Sim::<TodoApp>::new(7, 3);
    for round in 0..4 {
        for i in 0..sim.clients() {
            sim.mutate(i, todo::add(format!("c{i}-{round}")));
            sim.step();
        }
    }

    // Two of the three go dark and keep working, which is the case the whole
    // engine exists for.
    sim.partition(1);
    sim.partition(2);
    for round in 0..5 {
        sim.mutate(1, todo::add(format!("offline-b-{round}")));
        sim.mutate(2, todo::add(format!("offline-c-{round}")));
        sim.mutate(0, todo::add(format!("online-{round}")));
        sim.step();
    }
    // Deliberately no `heal` here. `settle` reconnects every client itself,
    // and the work authored while dark is only recovered because a reconnecting
    // client re-offers what it still has pending — so leaving it to `settle` is
    // what makes this test depend on that, rather than on the network having
    // been kind.
    sim.settle();

    let first = sim.state_hash(0);
    for i in 1..sim.clients() {
        assert_eq!(first, sim.state_hash(i), "client {i} disagrees");
    }
    assert_eq!(first, sim.server_hash(), "the server disagrees");
    assert_eq!(
        todo::list(&mut petros::backend::SqliteStore::new(sim.conn(0)))
            .unwrap()
            .len(),
        27,
        "12 online + 15 offline, none lost and none duplicated"
    );
}

/// The same seed has to be the same run, or a failure is not something you can
/// sit down and fix.
#[test]
fn a_seed_is_the_whole_run() {
    let hash = |seed: u64| {
        let mut sim = Sim::<TodoApp>::new(seed, 2);
        for n in 0..6 {
            sim.mutate(n % 2, todo::add(format!("item {n}")));
            sim.step();
        }
        sim.partition(0);
        sim.mutate(0, todo::add("while dark".into()));
        sim.step();
        sim.settle();
        (sim.state_hash(0), sim.state_hash(1))
    };
    assert_eq!(hash(42), hash(42), "same seed, same run");
    assert_ne!(
        hash(42),
        hash(43),
        "and a different seed is a different run"
    );
}

fn db(app_rows: &str, petros_rows: &str) -> petros::Connection {
    let mut conn = petros::open_memory().unwrap();
    conn.batch_execute(&format!(
        "CREATE TABLE thing (id INTEGER PRIMARY KEY, name TEXT, blob BLOB, note TEXT);
         CREATE TABLE petros_scratch (x TEXT);
         {app_rows} {petros_rows}"
    ))
    .unwrap();
    conn
}

/// The hash is over columns, not over a model — so a column an app forgot to
/// read is still part of what two replicas have to agree about.
#[test]
fn the_hash_sees_every_column() {
    let same = "INSERT INTO thing VALUES (1, 'a', X'00ff', 'note');";
    let differs_only_in_a_column_nothing_reads =
        "INSERT INTO thing VALUES (1, 'a', X'00ff', 'other');";
    assert_eq!(
        state_hash(&mut db(same, "")),
        state_hash(&mut db(same, "")),
        "identical databases hash identically"
    );
    assert_ne!(
        state_hash(&mut db(same, "")),
        state_hash(&mut db(differs_only_in_a_column_nothing_reads, "")),
        "a difference in any column is a difference"
    );
    // NULL and the text 'NULL' are different values and must not collide.
    assert_ne!(
        state_hash(&mut db("INSERT INTO thing VALUES (1,'a',NULL,'n');", "")),
        state_hash(&mut db("INSERT INTO thing VALUES (1,'a','NULL','n');", "")),
    );
}

/// Petros's own tables are not app state. Two replicas agree about what the app
/// holds long before they agree about how much of the log each has seen.
#[test]
fn the_engines_own_tables_are_not_part_of_it() {
    let rows = "INSERT INTO thing VALUES (1, 'a', X'00ff', 'note');";
    assert_eq!(
        state_hash(&mut db(rows, "INSERT INTO petros_scratch VALUES ('one');")),
        state_hash(&mut db(rows, "INSERT INTO petros_scratch VALUES ('two');")),
    );
}
