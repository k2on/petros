//! What the client tells a caller who is maintaining a view.
//!
//! Reading the whole list back after every tap is the thing an incremental view
//! exists to avoid, and it needs to be told what changed. Most of that is an
//! ordinary list — but a rebase is not, and this is where that is pinned down.
//!
//! These run against the example app rather than `tests/common`, because the
//! example writes through the store. A mutation that writes around it — with
//! diesel, as the fixture does to demonstrate the low-level API — reports
//! nothing, and that is a real property of the contract rather than an
//! oversight.

use petros::{AutoCtx, Changes, Client};
use todo::TodoApp;

fn client() -> Client<TodoApp> {
    Client::open(petros::open_memory().unwrap(), "alice", AutoCtx::seeded(42)).unwrap()
}

/// The fast path: a local tap reports what the rows did, as a plain list.
#[test]
fn a_local_mutation_reports_what_it_changed() {
    let mut c = client();
    assert_eq!(c.take_changes(), Changes::Applied(vec![]));

    c.mutate(todo::Payload(todo::add("mine".into()))).unwrap();
    let Changes::Applied(changes) = c.take_changes() else {
        panic!("a local tap rolls nothing back");
    };
    assert_eq!(changes.len(), 1, "one row added: {changes:?}");

    // Drained, not repeated. A view told twice would show the row twice.
    assert_eq!(c.take_changes(), Changes::Applied(vec![]));
}

/// A refused mutation is rolled back, so it did not change anything and must
/// not say it did.
#[test]
fn a_refused_mutation_reports_nothing() {
    let mut c = client();
    c.mutate(todo::Payload(todo::add("   ".into())))
        .expect_err("a blank to-do is refused");
    assert_eq!(c.take_changes(), Changes::Applied(vec![]));
}

/// A rebase cannot be described as a list of changes, and says so instead of
/// handing out ones that have been undone.
///
/// Replaying pending work begins with `ROLLBACK TO pending`, and a rollback
/// reverts rows without reporting anything. "Mine was added at position 1" is
/// no longer true, and there is no forward change that makes it true.
#[test]
fn a_rebase_asks_for_a_re_hydrate_rather_than_lying() {
    let mut c = client();
    c.mutate(todo::Payload(todo::add("mine".into()))).unwrap();
    let _ = c.take_changes();

    c.recv(petros::ServerMsg::Batch {
        entries: vec![confirmed(1, "bob")],
        has_more: false,
    })
    .unwrap();

    assert_eq!(
        c.take_changes(),
        Changes::Rebuilt,
        "ours was rolled back and replayed on top of bob's"
    );
    // The flag clears, so the next tap is an ordinary list again.
    assert_eq!(c.take_changes(), Changes::Applied(vec![]));
}

/// A confirmed entry arriving with nothing pending rolls nothing back, so it is
/// an ordinary change. This is the common case while online, and re-hydrating
/// for it would waste the whole exercise.
#[test]
fn a_confirmed_entry_with_nothing_pending_is_an_ordinary_change() {
    let mut c = client();
    c.recv(petros::ServerMsg::Batch {
        entries: vec![confirmed(1, "bob")],
        has_more: false,
    })
    .unwrap();

    let Changes::Applied(changes) = c.take_changes() else {
        panic!("nothing was pending, so nothing was rolled back");
    };
    assert_eq!(changes.len(), 1);
}

fn confirmed(seq: u64, actor: &str) -> petros::Entry<todo::Payload> {
    let mut mutation = todo::Payload(todo::add(format!("from {actor}")));
    petros::Mutation::fill_auto(&mut mutation, &mut AutoCtx::seeded(seq));
    petros::Entry {
        id: petros::Id(petros::uuid::Uuid::from_u128(seq as u128)),
        seq: Some(seq),
        actor: actor.into(),
        mutation,
    }
}
