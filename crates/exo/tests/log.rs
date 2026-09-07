//! The server's log: totally ordered, append-only, deduped by entry id.

mod common;

use common::{Todo, TodoMutation};
use exo::{ActorId, AutoCtx, ClientMsg, Entry, Server, ServerMsg, Uuid};

fn entry(actor: &str, m: TodoMutation, auto: &mut AutoCtx) -> Entry<TodoMutation> {
    Entry::new(auto.uuid(), ActorId::from(actor), m)
}

fn server() -> Server<Todo> {
    Server::open(exo::open_memory().unwrap()).unwrap()
}

#[test]
fn append_assigns_monotonic_seq() {
    let mut auto = AutoCtx::seeded(1);
    let mut s = server();
    let entries: Vec<_> = ["a", "b", "c"]
        .iter()
        .map(|t| entry("alice", TodoMutation::add(t), &mut auto))
        .collect();

    s.recv(1, ClientMsg::Push { entries }).unwrap();

    let acked = acks(&mut s);
    assert_eq!(acked.len(), 3);
    assert_eq!(acked.iter().map(|(_, seq)| *seq).collect::<Vec<_>>(), vec![1, 2, 3]);
    assert_eq!(s.head(), 3);
}

#[test]
fn duplicate_uuid_is_deduped() {
    let mut auto = AutoCtx::seeded(2);
    let mut s = server();
    let e = entry("alice", TodoMutation::add("only once"), &mut auto);

    s.recv(1, ClientMsg::Push { entries: vec![e.clone()] }).unwrap();
    let first = acks(&mut s);
    // A push whose response was lost, retried. The call site should not have to
    // reason about this at all.
    s.recv(1, ClientMsg::Push { entries: vec![e] }).unwrap();
    let second = acks(&mut s);

    assert_eq!(first, second, "a replayed push must ack the same seq");
    assert_eq!(s.head(), 1, "and must not append a second log row");
}

/// Drain the server's outgoing queue and collect (entry id, assigned seq).
fn acks(s: &mut Server<Todo>) -> Vec<(Uuid, u64)> {
    let mut out = Vec::new();
    for (_conn, msg) in s.take_outgoing() {
        if let ServerMsg::Ack { ids, seqs } = msg {
            out.extend(ids.into_iter().zip(seqs));
        }
    }
    out
}
