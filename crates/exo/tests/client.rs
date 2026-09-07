//! The client state machine: optimistic apply, and the rebase that reconciles
//! it with the server's order.

mod common;

use common::todo::{texts, Todo, TodoMutation};
use exo::{ActorId, AutoCtx, Client, ClientMsg, Entry, Mutation, Seq, ServerMsg, Uuid};

type Msg = ServerMsg<TodoMutation>;

fn client(actor: &str) -> Client<Todo> {
    Client::open(exo::open_memory().unwrap(), actor, AutoCtx::seeded(42)).unwrap()
}

/// An entry as it would come back from the server: authored elsewhere, its
/// auto arguments already frozen, sequenced.
fn confirmed(seq: Seq, actor: &str, mut m: TodoMutation) -> Entry<TodoMutation> {
    m.fill_auto(&mut AutoCtx::seeded(seq));
    let mut e = Entry::new(Uuid::from_u128(seq as u128), ActorId::from(actor), m);
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

#[test]
fn ack_clears_outbox_and_releases_savepoint() {
    let mut c = client("alice");
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
    assert!(
        c.conn().is_autocommit(),
        "with nothing pending there must be no transaction left open"
    );
}

#[test]
fn pending_state_is_held_in_an_open_savepoint() {
    let mut c = client("alice");
    assert!(c.conn().is_autocommit(), "steady state holds nothing open");

    c.mutate(TodoMutation::add("buy milk")).unwrap();
    assert!(
        !c.conn().is_autocommit(),
        "optimistic state lives in a transaction that can be rolled back"
    );

    let id = pushed(&mut c)[0].id;
    c.recv(Msg::Ack {
        ids: vec![id],
        seqs: vec![1],
    })
    .unwrap();
    assert!(c.conn().is_autocommit(), "and is released on the last ack");
}

#[test]
fn reject_rolls_back_pending_and_reports() {
    let mut c = client("alice");
    c.mutate(TodoMutation::add("doomed")).unwrap();
    c.mutate(TodoMutation::add("survivor")).unwrap();
    let ids: Vec<Uuid> = pushed(&mut c).iter().map(|e| e.id).collect();

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
