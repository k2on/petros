//! The client state machine: optimistic apply, and the rebase that reconciles
//! it with the server's order.

mod common;

use common::todo::{texts, Todo, TodoMutation};
use petros::{ActorId, AutoCtx, Client, ClientMsg, Entry, Id, Mutation, Seq, ServerMsg};

type Msg = ServerMsg<TodoMutation>;

fn client(actor: &str) -> Client<Todo> {
    Client::open(petros::open_memory().unwrap(), actor, AutoCtx::seeded(42)).unwrap()
}

/// An entry as it would come back from the server: authored elsewhere, its
/// auto arguments already frozen, sequenced.
fn confirmed(seq: Seq, actor: &str, mut m: TodoMutation) -> Entry<TodoMutation> {
    m.fill_auto(&mut AutoCtx::seeded(seq));
    let mut e = Entry::new(Id::from_u128(seq as u128), ActorId::from(actor), m);
    e.seq = Some(seq);
    e
}

fn pushed(c: &mut Client<Todo>) -> Vec<Entry<TodoMutation>> {
    c.take_outgoing()
        .into_iter()
        .flat_map(|m| match m {
            ClientMsg::Push { entries } => entries,
            ClientMsg::Hello { .. } => Vec::new(),
        })
        .collect()
}

#[test]
fn local_mutation_applies_optimistically() {
    let mut c = client("alice");
    c.mutate(TodoMutation::add("buy milk")).unwrap();

    assert_eq!(
        texts(c.conn()),
        vec!["buy milk"],
        "visible before any server saw it"
    );
    assert_eq!(c.pending_len(), 1);
    let out = pushed(&mut c);
    assert_eq!(out.len(), 1, "and queued for the server");
    assert_eq!(out[0].seq, None, "pushed entries are unsequenced");
}

#[test]
fn confirmed_batch_materializes_forward() {
    let mut c = client("alice");
    c.recv(Msg::Batch {
        entries: vec![
            confirmed(1, "bob", TodoMutation::add("first")),
            confirmed(2, "bob", TodoMutation::add("second")),
        ],
        has_more: false,
    })
    .unwrap();

    assert_eq!(texts(c.conn()), vec!["first", "second"]);
    assert_eq!(c.cursor(), 2);
    assert_eq!(c.pending_len(), 0);
}

#[test]
fn pending_rebases_over_confirmed() {
    let mut c = client("alice");
    c.mutate(TodoMutation::add("mine")).unwrap();
    assert_eq!(texts(c.conn()), vec!["mine"]);

    // Bob's entry was ordered first by the server. Ours has to move down: `pos`
    // is computed at apply time, so replaying our intent on top of his gives a
    // different — and correct — answer than the one we optimistically showed.
    c.recv(Msg::Batch {
        entries: vec![confirmed(1, "bob", TodoMutation::add("theirs"))],
        has_more: false,
    })
    .unwrap();

    assert_eq!(texts(c.conn()), vec!["theirs", "mine"]);
    assert_eq!(c.cursor(), 1);
    assert_eq!(c.pending_len(), 1, "still ours, still unconfirmed");
}

/// A second connection to the same file. Uncommitted work is invisible to it,
/// which is how these tests observe the savepoint without a `is_autocommit`
/// to ask — and it is the property that actually matters.
fn on_disk(dir: &std::path::Path) -> (Client<Todo>, petros::Connection) {
    let path = dir.join("client.db");
    let client = Client::<Todo>::open(
        petros::open_path(&path).unwrap(),
        "alice",
        AutoCtx::seeded(42),
    )
    .unwrap();
    let observer = petros::open_path(&path).unwrap();
    (client, observer)
}

#[test]
fn ack_clears_outbox_and_releases_savepoint() {
    let dir = tempfile::tempdir().unwrap();
    let (mut c, mut observer) = on_disk(dir.path());
    c.mutate(TodoMutation::add("buy milk")).unwrap();
    let id = pushed(&mut c)[0].id;

    c.recv(Msg::Ack {
        ids: vec![id],
        seqs: vec![1],
    })
    .unwrap();

    assert_eq!(c.pending_len(), 0);
    assert_eq!(c.cursor(), 1);
    assert_eq!(texts(c.conn()), vec!["buy milk"]);
    assert_eq!(
        texts(&mut observer),
        vec!["buy milk"],
        "with nothing pending the savepoint is released and the work is committed"
    );
}

#[test]
fn pending_state_is_held_in_an_open_savepoint() {
    let dir = tempfile::tempdir().unwrap();
    let (mut c, mut observer) = on_disk(dir.path());
    assert!(texts(&mut observer).is_empty());

    c.mutate(TodoMutation::add("buy milk")).unwrap();
    assert_eq!(
        texts(c.conn()),
        vec!["buy milk"],
        "we can see our own guess"
    );
    assert!(
        texts(&mut observer).is_empty(),
        "but it is uncommitted, so it can still be rolled back"
    );

    let id = pushed(&mut c)[0].id;
    c.recv(Msg::Ack {
        ids: vec![id],
        seqs: vec![1],
    })
    .unwrap();
    assert_eq!(
        texts(&mut observer),
        vec!["buy milk"],
        "and the last ack commits it"
    );
}

#[test]
fn reject_rolls_back_pending_and_reports() {
    let mut c = client("alice");
    c.mutate(TodoMutation::add("doomed")).unwrap();
    c.mutate(TodoMutation::add("survivor")).unwrap();
    let ids: Vec<Id> = pushed(&mut c).iter().map(|e| e.id).collect();

    c.recv(Msg::Reject {
        id: ids[0],
        reason: "nope".into(),
    })
    .unwrap();

    let rejections = c.take_rejections();
    assert_eq!(rejections.len(), 1);
    assert_eq!(rejections[0].id, ids[0]);
    assert_eq!(rejections[0].reason, "nope");
    assert_eq!(
        texts(c.conn()),
        vec!["survivor"],
        "the rejected mutation is undone; the ones after it are replayed without it"
    );
    assert_eq!(c.pending_len(), 1);
}

#[test]
fn connect_replays_hello_and_pending() {
    let mut c = client("alice");
    c.mutate(TodoMutation::add("offline edit")).unwrap();
    let _ = c.take_outgoing(); // dropped on the floor: we were partitioned

    c.connected().unwrap();
    let out = c.take_outgoing();
    assert!(matches!(out[0], ClientMsg::Hello { since: 0 }));
    match &out[1] {
        ClientMsg::Push { entries } => assert_eq!(entries.len(), 1),
        other => panic!("expected a Push, got {other:?}"),
    }
}

/// A mutation costs the same at any pending depth.
///
/// This is what "offline indefinitely" means, and it is a property of the
/// design rather than a happy accident: intents commit to their own database,
/// so the optimistic transaction on this one never closes and a new mutation
/// applies on top of the view instead of rebuilding it.
///
/// It used to rebuild. A tap was O(pending) and a burst O(n²) — 390ms at forty
/// pending on a phone.
#[test]
fn a_mutation_costs_the_same_at_any_depth() {
    use std::time::Instant;

    let mut client = client("alice");
    let at = |c: &mut Client<Todo>, n: usize| {
        while c.pending_len() < n {
            c.mutate(TodoMutation::add("filler")).unwrap();
        }
        let start = Instant::now();
        for i in 0..20 {
            c.mutate(TodoMutation::add(&format!("timed {i}"))).unwrap();
        }
        start.elapsed().as_secs_f64() * 1000.0 / 20.0
    };

    let shallow = at(&mut client, 5);
    let deep = at(&mut client, 400);

    // Ten times the depth of the old measurable slowdown. A ratio, not an
    // absolute, so a slow machine does not fail this — what is being asserted
    // is the shape of the curve.
    assert!(
        deep < shallow * 4.0 + 1.0,
        "a tap at 400 pending took {deep:.3}ms against {shallow:.3}ms at 5: \
         the cost is growing with the depth again"
    );
}
