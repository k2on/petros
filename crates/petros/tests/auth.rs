//! Who a connection is, and what that decides.
//!
//! The engine asks an [`Authenticate`] at every `Hello` and holds every entry
//! the connection pushes to the answer. Under [`Trusting`] it asks nothing and
//! holds nothing — which is the behaviour it had before it could ask.

mod common;

use common::todo::{Todo, TodoMutation};
use petros::{
    ActorId, Authenticate, AutoCtx, Client, ClientMsg, Entry, Identity, Server, ServerMsg,
};

/// Two tokens, one user, and a session that user had once.
struct Table;

impl Authenticate for Table {
    fn authenticate(&mut self, token: Option<&str>) -> Option<Identity> {
        match token? {
            "t-alice" => Some(Identity {
                user: ActorId::from("alice"),
                session: "alice-now".into(),
            }),
            "t-bob" => Some(Identity {
                user: ActorId::from("bob"),
                session: "bob-now".into(),
            }),
            _ => None,
        }
    }

    fn owns(&mut self, user: &ActorId, session: &str) -> bool {
        user.as_str() == "alice" && session == "alice-before"
    }
}

fn server() -> Server<Todo> {
    Server::open_with(petros::open_memory().unwrap(), Table).unwrap()
}

fn entry(actor: &str, session: Option<&str>, auto: &mut AutoCtx) -> Entry<TodoMutation> {
    let mut e = Entry::new(auto.uuid(), ActorId::from(actor), TodoMutation::add("x"));
    e.session = session.map(str::to_string);
    e
}

fn hello(token: Option<&str>) -> ClientMsg<TodoMutation> {
    ClientMsg::Hello {
        since: 0,
        token: token.map(str::to_string),
    }
}

fn push(entries: Vec<Entry<TodoMutation>>) -> ClientMsg<TodoMutation> {
    ClientMsg::Push { entries }
}

/// The verdicts one connection was sent, as a sentence each. Batches are
/// left out: they are the fan-out, which every connection with a cursor
/// behind the head gets, the pusher included.
fn sent(s: &mut Server<Todo>, to: u64) -> Vec<String> {
    s.take_outgoing()
        .into_iter()
        .filter(|(c, _)| *c == to)
        .filter_map(|(_, m)| match m {
            ServerMsg::Batch { .. } => None,
            ServerMsg::Ack { seqs, .. } => Some(format!("ack {seqs:?}")),
            ServerMsg::Reject { reason, .. } => Some(format!("reject: {reason}")),
            ServerMsg::Denied { reason } => Some(format!("denied: {reason}")),
        })
        .collect()
}

#[test]
fn a_hello_without_a_good_token_is_denied_and_nothing_else() {
    let mut auto = AutoCtx::seeded(1);
    let mut s = server();
    s.recv(1, hello(None)).unwrap();
    assert_eq!(sent(&mut s, 1), ["denied: not signed in"]);

    s.recv(2, hello(Some("t-nobody"))).unwrap();
    assert_eq!(sent(&mut s, 2), ["denied: not signed in"]);

    // Pushing without ever having been let in is pushing as nobody.
    s.recv(3, push(vec![entry("alice", None, &mut auto)]))
        .unwrap();
    assert_eq!(sent(&mut s, 3), ["denied: not signed in"]);
    assert_eq!(s.head(), 0, "nothing reached the log");
}

#[test]
fn an_entry_is_held_to_the_login_that_pushes_it() {
    let mut auto = AutoCtx::seeded(2);
    let mut s = server();
    s.recv(1, hello(Some("t-alice"))).unwrap();
    assert!(
        sent(&mut s, 1).is_empty(),
        "an empty log has nothing to send"
    );

    // Hers, under this login.
    s.recv(1, push(vec![entry("alice", Some("alice-now"), &mut auto)]))
        .unwrap();
    assert_eq!(sent(&mut s, 1), ["ack [1]"]);

    // Hers, under a login she had before — authored offline, signed in again
    // since. Still hers.
    s.recv(
        1,
        push(vec![entry("alice", Some("alice-before"), &mut auto)]),
    )
    .unwrap();
    assert_eq!(sent(&mut s, 1), ["ack [2]"]);

    // Hers, from a peer that never said which login. Allowed: the user is
    // what the server checks, the session is what it records.
    s.recv(1, push(vec![entry("alice", None, &mut auto)]))
        .unwrap();
    assert_eq!(sent(&mut s, 1), ["ack [3]"]);

    // Somebody else's, and hers under a login that was never hers.
    s.recv(
        1,
        push(vec![
            entry("bob", Some("bob-now"), &mut auto),
            entry("alice", Some("stolen"), &mut auto),
        ]),
    )
    .unwrap();
    assert_eq!(
        sent(&mut s, 1),
        [
            "reject: authored as bob but signed in as alice",
            "reject: session stolen was never alice's",
        ]
    );
    assert_eq!(s.head(), 3);
}

#[test]
fn a_trusting_server_asks_nothing() {
    let mut auto = AutoCtx::seeded(3);
    let mut s: Server<Todo> = Server::open(petros::open_memory().unwrap()).unwrap();
    s.recv(1, hello(None)).unwrap();
    s.recv(
        1,
        push(vec![
            entry("alice", None, &mut auto),
            entry("bob", Some("whatever"), &mut auto),
        ]),
    )
    .unwrap();
    assert_eq!(sent(&mut s, 1), ["ack [1, 2]"]);
}

#[test]
fn what_apply_sees_is_what_the_entry_carried() {
    let mut s = server();
    s.recv(1, hello(Some("t-alice"))).unwrap();
    let mut auto = AutoCtx::seeded(4);
    let e = entry("alice", Some("alice-now"), &mut auto);
    s.recv(1, push(vec![e])).unwrap();
    let _ = s.take_outgoing();
    // The log keeps the session beside the actor, and hands both back.
    s.recv(2, hello(Some("t-bob"))).unwrap();
    let Some((_, ServerMsg::Batch { entries, .. })) = s.take_outgoing().pop() else {
        panic!("bob is caught up with a batch");
    };
    assert_eq!(entries[0].session.as_deref(), Some("alice-now"));
    let ctx = entries[0].ctx();
    assert_eq!(ctx.user.id, "alice");
    assert_eq!(ctx.session.id, "alice-now");
}

#[test]
fn the_client_carries_its_token_and_keeps_what_was_denied() {
    let mut c =
        Client::<Todo>::open(petros::open_memory().unwrap(), "alice", AutoCtx::seeded(5)).unwrap();
    c.set_session(Some("alice-now".into()));
    c.mutate(TodoMutation::add("offline")).unwrap();
    let _ = c.take_outgoing();

    c.set_token(Some("t-alice".into()));
    c.connected().unwrap();
    let out = c.take_outgoing();
    assert!(
        matches!(&out[0], ClientMsg::Hello { token: Some(t), .. } if t == "t-alice"),
        "the token rides the Hello: {out:?}"
    );
    let ClientMsg::Push { entries } = &out[1] else {
        panic!("the pending entry is re-offered");
    };
    assert_eq!(entries[0].session.as_deref(), Some("alice-now"));

    // Turned away: the reason is kept for someone to read, the entry is not
    // dropped, and the next connect offers it again.
    c.recv(ServerMsg::Denied {
        reason: "not signed in".into(),
    })
    .unwrap();
    assert_eq!(c.take_denial().as_deref(), Some("not signed in"));
    assert_eq!(c.take_denial(), None);
    assert_eq!(c.pending_len(), 1);
}

/// A first-run peer authors before it has ever connected, and is still let in.
///
/// This is the shape every new client has: open the database, seed whatever
/// the app needs to exist, *then* find a server. The seed is a mutation, so
/// it is pending before there is any connection to offer it on — and if that
/// `Push` is queued anyway it leaves ahead of the `Hello`, which a server
/// reads as a push from nobody and answers with `Denied`. A brand-new peer
/// with a perfectly good token could not sign in at all.
///
/// So the assertion is the frame *order*, and then the server's verdict on
/// the whole exchange. Opening the client as linked puts a `Push` at `out[0]`
/// and turns the last assertion into `denied: not signed in`.
#[test]
fn a_peer_that_authored_before_its_first_connect_still_says_hello_first() {
    let mut c =
        Client::<Todo>::open(petros::open_memory().unwrap(), "alice", AutoCtx::seeded(7)).unwrap();
    c.set_session(Some("alice-now".into()));
    c.set_token(Some("t-alice".into()));

    // The seed, authored with no transport in existence. Nothing is taken off
    // the client here — that is the point, a real peer has nowhere to put it.
    c.mutate(TodoMutation::add("seeded before connecting"))
        .unwrap();

    c.connected().unwrap();
    let out = c.take_outgoing();
    assert!(
        matches!(&out[0], ClientMsg::Hello { .. }),
        "the Hello is the first frame on the connection, not the seed: {out:?}"
    );
    assert_eq!(
        out.len(),
        2,
        "a Hello and one Push, not two Pushes: {out:?}"
    );

    // And the server agrees: this peer is signed in, and its seed lands.
    let mut s = server();
    for msg in out {
        s.recv(1, msg).unwrap();
    }
    assert_eq!(sent(&mut s, 1), ["ack [1]"]);
    assert_eq!(s.head(), 1, "the seed reached the log");
}
