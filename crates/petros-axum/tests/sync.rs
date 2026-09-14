//! The adapter, driven by a real client over a real socket.
//!
//! Compiling is not evidence. What this asserts is the property the crate
//! exists for: two peers on one axum server see each other's mutations, using
//! the same `Link` the terminal peers use and without either of them knowing
//! anything about axum.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::routing::get;
use axum::Router;
use petros::transport::ws::Link;
use petros::{AutoCtx, Client};
use todo::{Payload, TodoApp};

/// Drive one peer until `done`, or give up. A sans-io client has to be pumped
/// by someone; in a test that someone is a loop.
fn pump(client: &mut Client<TodoApp>, link: &Link<Payload>, until: Instant) {
    while Instant::now() < until {
        for msg in client.take_outgoing() {
            link.send(msg);
        }
        while let Some(msg) = link.try_recv() {
            client.recv(msg).expect("the server sent something valid");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn two_peers_on_an_axum_server_see_each_other() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let hub = petros_axum::Hub::<TodoApp>::open(petros::open_memory().unwrap(), petros::Trusting)
        .unwrap();
    let health = hub.clone();

    let addr = runtime.block_on(async move {
        let app = Router::new()
            .route("/sync", get(petros_axum::sync::<TodoApp>))
            .with_state(hub);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    });

    let mut alice =
        Client::<TodoApp>::open(petros::open_memory().unwrap(), "alice", AutoCtx::seeded(1))
            .unwrap();
    let mut bob =
        Client::<TodoApp>::open(petros::open_memory().unwrap(), "bob", AutoCtx::seeded(2)).unwrap();

    let a_link = Link::<Payload>::connect(&format!("ws://{addr}/sync")).expect("alice connects");
    let b_link = Link::<Payload>::connect(&format!("ws://{addr}/sync")).expect("bob connects");
    alice.connected().unwrap();
    bob.connected().unwrap();

    // Settle first, and this is the whole point of the test.
    //
    // A peer that is still catching up learns about entries by asking — its
    // `Hello` is answered with a `Batch` — so if alice mutates while bob is
    // still connecting, bob receives the entry whether or not the server ever
    // pushes anything to anyone. The first version of this test did exactly
    // that and passed with fan-out deliberately broken.
    let settle = Instant::now() + Duration::from_millis(300);
    while Instant::now() < settle {
        pump(
            &mut alice,
            &a_link,
            Instant::now() + Duration::from_millis(20),
        );
        pump(
            &mut bob,
            &b_link,
            Instant::now() + Duration::from_millis(20),
        );
    }
    assert!(
        todo::list(&mut bob.store()).unwrap().is_empty(),
        "bob is caught up and there is nothing to catch up on"
    );

    alice.mutate(todo::add("from alice".into())).unwrap();

    // From here bob sends nothing at all — no second `Hello`, no pending of his
    // own. Anything that reaches him is a push the server chose to make.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        pump(
            &mut alice,
            &a_link,
            Instant::now() + Duration::from_millis(20),
        );
        pump(
            &mut bob,
            &b_link,
            Instant::now() + Duration::from_millis(20),
        );
        seen = todo::list(&mut bob.store()).unwrap();
        if !seen.is_empty() && alice.pending_len() == 0 {
            break;
        }
    }

    assert_eq!(seen.len(), 1, "bob should have alice's to-do");
    assert_eq!(seen[0].text, "from alice");
    assert_eq!(seen[0].actor, "alice", "the entry carries its author");
    assert_eq!(
        alice.pending_len(),
        0,
        "and alice's is confirmed, not pending"
    );
    assert_eq!(health.server().head(), 1, "one entry in the log");
}

/// Dropping a peer has to unregister it, or the server accumulates senders
/// nobody reads and fans out into the void.
#[test]
fn a_disconnected_peer_is_forgotten() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let hub = petros_axum::Hub::<TodoApp>::open(petros::open_memory().unwrap(), petros::Trusting)
        .unwrap();
    let watch = hub.clone();

    let addr = runtime.block_on(async move {
        let app = Router::new()
            .route("/sync", get(petros_axum::sync::<TodoApp>))
            .with_state(hub);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    });

    let until = |want: usize, watch: &Arc<petros_axum::Hub<TodoApp>>| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && watch.connected() != want {
            std::thread::sleep(Duration::from_millis(10));
        }
        watch.connected()
    };

    let link = Link::<Payload>::connect(&format!("ws://{addr}/sync")).expect("connects");
    assert_eq!(until(1, &watch), 1, "the server counted the connection");
    drop(link);
    assert_eq!(until(0, &watch), 0, "and forgot it when the socket closed");
}

/// A peer with no socket — a library scanner, a cron job — writing into the
/// same log, and a peer with one seeing it.
///
/// This is the case `route` cannot serve: it delivers to `peers`, and an
/// in-process writer is not in there. Without `exchange` the fan-out sits in
/// the server's queue until some *other* peer happens to speak, which for a
/// scanner adding a file at three in the morning is "never".
#[test]
fn an_in_process_peer_reaches_one_on_a_socket() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();

    let hub = petros_axum::Hub::<TodoApp>::open(petros::open_memory().unwrap(), petros::Trusting)
        .unwrap();
    let inside = hub.clone();

    let addr = runtime.block_on(async move {
        let app = Router::new()
            .route("/sync", get(petros_axum::sync::<TodoApp>))
            .with_state(hub);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    });

    // The one on the socket, connected and then left alone. It says nothing
    // from here on, which is the whole point: nothing it does may be what
    // delivers the scanner's work.
    let mut watcher = Client::<TodoApp>::open(
        petros::open_memory().unwrap(),
        "watcher",
        AutoCtx::seeded(1),
    )
    .unwrap();
    let link = Link::<Payload>::connect(&format!("ws://{addr}/sync")).expect("it connects");
    watcher.connected().unwrap();
    pump(
        &mut watcher,
        &link,
        Instant::now() + Duration::from_millis(300),
    );

    // The one without a socket: an ordinary client, pumped by hand into the
    // hub rather than down a wire.
    let mut scanner = Client::<TodoApp>::open(
        petros::open_memory().unwrap(),
        "scanner",
        AutoCtx::seeded(2),
    )
    .unwrap();
    let conn = inside.local();
    scanner.connected().unwrap();
    scanner.mutate(todo::add("a file it found".into())).unwrap();
    for _ in 0..8 {
        for msg in scanner.take_outgoing() {
            for reply in inside.exchange(conn, msg) {
                scanner.recv(reply).expect("the server answers it too");
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }

    // The socket peer is pumped but never *sends* anything of its own; if the
    // fan-out had stayed in the queue there would be nothing to read.
    pump(
        &mut watcher,
        &link,
        Instant::now() + Duration::from_millis(600),
    );
    let seen = todo::list(&mut watcher.store()).unwrap();
    assert_eq!(
        seen.iter().map(|t| t.text.as_str()).collect::<Vec<_>>(),
        vec!["a file it found"],
        "what the scanner wrote reached the socket without anyone asking"
    );
    // And it is confirmed, not just optimistic, on the scanner's own side.
    assert_eq!(scanner.pending_len(), 0, "the server acked it back");
}
