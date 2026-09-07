//! Resume, restart, and the determinism guarantees the log rests on.

mod common;

use common::sim::state_hash;
use common::todo::{items, texts, Todo, TodoMutation};
use exo::{ActorId, AutoCtx, Client, ClientMsg, Entry, Id, Mutation, Seq, Server, ServerMsg};

type Up = ClientMsg<TodoMutation>;
type Down = ServerMsg<TodoMutation>;

/// Build a confirmed log by hand: the ids and timestamps are frozen here, once,
/// exactly as `fill_auto` would have frozen them at an originating client.
fn log(len: usize) -> Vec<Entry<TodoMutation>> {
    let mut auto = AutoCtx::seeded(99);
    (0..len)
        .map(|i| {
            let mut m = TodoMutation::add(&format!("item-{i}"));
            m.fill_auto(&mut auto);
            let mut e = Entry::new(auto.uuid(), ActorId::from("bob"), m);
            e.seq = Some(i as Seq + 1);
            e
        })
        .collect()
}

fn batch(entries: Vec<Entry<TodoMutation>>) -> Down {
    Down::Batch {
        entries,
        has_more: false,
    }
}

#[test]
fn hello_since_returns_exactly_the_missing_entries() {
    let mut server = Server::<Todo>::open(exo::open_memory().unwrap()).unwrap();
    let entries: Vec<Entry<TodoMutation>> = log(5)
        .into_iter()
        .map(|mut e| {
            e.seq = None; // pushed entries are unsequenced
            e
        })
        .collect();
    server.recv(1, Up::Push { entries }).unwrap();
    let _ = server.take_outgoing();

    // A different client, resuming from the middle.
    server.recv(2, Up::Hello { since: 2 }).unwrap();

    let sent: Vec<Entry<TodoMutation>> = server
        .take_outgoing()
        .into_iter()
        .filter(|(conn, _)| *conn == 2)
        .flat_map(|(_, msg)| match msg {
            Down::Batch { entries, .. } => entries,
            _ => Vec::new(),
        })
        .collect();

    assert_eq!(
        sent.iter().map(|e| e.seq).collect::<Vec<_>>(),
        vec![Some(3), Some(4), Some(5)],
        "everything after the cursor, nothing before it"
    );
}

#[test]
fn a_full_batch_asks_the_client_to_come_back_for_more() {
    let mut client =
        Client::<Todo>::open(exo::open_memory().unwrap(), "alice", AutoCtx::seeded(1)).unwrap();
    client
        .recv(Down::Batch {
            entries: log(3),
            has_more: true,
        })
        .unwrap();

    let out = client.take_outgoing();
    assert!(
        out.iter().any(|m| matches!(m, Up::Hello { since: 3 })),
        "a partial batch is resumed from where it stopped, got {out:?}"
    );
}

#[test]
fn cursor_survives_restart() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("client.db");

    let pending_id = {
        let mut client =
            Client::<Todo>::open(exo::open_path(&path).unwrap(), "alice", AutoCtx::seeded(3))
                .unwrap();
        client.recv(batch(log(2))).unwrap();
        let id = client
            .mutate(TodoMutation::add("mine, unconfirmed"))
            .unwrap();
        assert_eq!(client.cursor(), 2);
        assert_eq!(texts(client.conn()).len(), 3);
        id
    };

    // Reopened cold. The confirmed log and the cursor were committed; the
    // optimistic state was not, and is rebuilt by replaying what is pending.
    let mut client =
        Client::<Todo>::open(exo::open_path(&path).unwrap(), "alice", AutoCtx::seeded(77)).unwrap();
    assert_eq!(
        client.cursor(),
        2,
        "resumes rather than resyncing from zero"
    );
    assert_eq!(
        client.pending_len(),
        1,
        "an offline edit is not lost to a restart"
    );
    assert_eq!(
        texts(client.conn()),
        vec!["item-0", "item-1", "mine, unconfirmed"]
    );
    assert!(!pending_id.is_nil());
}

#[test]
fn fill_auto_values_are_frozen() {
    let mut origin =
        Client::<Todo>::open(exo::open_memory().unwrap(), "alice", AutoCtx::seeded(5)).unwrap();
    origin.mutate(TodoMutation::add("buy milk")).unwrap();
    let mut entry = match origin.take_outgoing().remove(0) {
        Up::Push { mut entries } => entries.remove(0),
        other => panic!("expected a Push, got {other:?}"),
    };
    let frozen = items(origin.conn())[0].id;

    // A different machine, a different seed, years later.
    entry.seq = Some(1);
    let mut replica =
        Client::<Todo>::open(exo::open_memory().unwrap(), "bob", AutoCtx::seeded(123_456)).unwrap();
    replica.recv(batch(vec![entry])).unwrap();

    assert_eq!(
        items(replica.conn())[0].id,
        frozen,
        "a replay must never regenerate what fill_auto froze"
    );
    assert_eq!(state_hash(origin.conn()), state_hash(replica.conn()));
}

#[test]
fn replay_is_deterministic() {
    let mut entries = log(4);
    let first = items_of(&entries, 0).id;
    entries.push(sequenced(5, TodoMutation::set_done(first, true)));
    entries.push(sequenced(6, TodoMutation::rename(first, "renamed")));
    entries.push(sequenced(7, TodoMutation::claim(first)));
    entries.push(sequenced(8, TodoMutation::remove(items_of(&entries, 1).id)));

    let mut a = replay_into("alice", 1, entries.clone());
    let mut b = replay_into("bob", 2, entries);

    assert_eq!(
        state_hash(a.conn()),
        state_hash(b.conn()),
        "the same log in two fresh databases is the same state"
    );
    assert_eq!(texts(a.conn()), vec!["renamed", "item-2", "item-3"]);
}

fn replay_into(actor: &str, seed: u64, entries: Vec<Entry<TodoMutation>>) -> Client<Todo> {
    let mut c =
        Client::<Todo>::open(exo::open_memory().unwrap(), actor, AutoCtx::seeded(seed)).unwrap();
    c.recv(batch(entries)).unwrap();
    c
}

fn sequenced(seq: Seq, m: TodoMutation) -> Entry<TodoMutation> {
    let mut e = Entry::new(Id::from_u128(1_000 + seq as u128), ActorId::from("bob"), m);
    e.seq = Some(seq);
    e
}

/// The id `fill_auto` froze into the nth `Add` of a log.
fn items_of(entries: &[Entry<TodoMutation>], n: usize) -> Item {
    match &entries[n].mutation {
        TodoMutation::Add { id, .. } => Item { id: *id },
        other => panic!("entry {n} is not an Add: {other:?}"),
    }
}

struct Item {
    id: Id,
}
