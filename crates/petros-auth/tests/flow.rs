//! The whole way round, in dev mode: a login over HTTP, the token on the
//! socket, and what the engine then refuses.
//!
//! The provider is the one part this cannot drive — there is none on a test
//! machine — so `Mode::Dev` stands in for it. Everything after the account is
//! known is the same code either way: the session, the code, the exchange,
//! the `Hello`, the checks.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::routing::get;
use axum::Router;
use petros::transport::ws::Link;
use petros::{AutoCtx, Client};
use petros_auth::server::{Auth, Mode};
use petros_auth::session::SessionStore;
use todo::{Payload, TodoApp};

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

/// A server on a port, with the auth routes beside the socket.
fn serve() -> (tokio::runtime::Runtime, String) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let sessions = SessionStore::open(petros::open_memory().unwrap()).unwrap();
    let addr = runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let auth = Arc::new(Auth::new(sessions, Mode::Dev, &format!("http://{addr}")));
        let hub =
            petros_axum::Hub::<TodoApp>::open(petros::open_memory().unwrap(), auth.authenticator())
                .unwrap();
        let app = Router::new()
            .route("/sync", get(petros_axum::sync::<TodoApp>))
            .with_state(hub)
            .merge(petros_auth::server::router(auth));
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        addr
    });
    (runtime, format!("http://{addr}"))
}

#[test]
fn a_login_is_a_token_the_socket_accepts() {
    let (_runtime, server) = serve();

    // Headless: a dev server with a name answers with the code at once, so
    // no browser is asked for.
    let login = petros_auth::client::login(&server, Some("alice"), |url| {
        panic!("a browser was asked to open {url}")
    })
    .expect("signed in");
    assert_eq!(login.user.id, "alice");
    assert!(!login.token.is_empty());
    assert_eq!(
        petros_auth::client::whoami(&server, &login.token)
            .unwrap()
            .map(|l| l.session),
        Some(login.session.clone())
    );

    let mut alice =
        Client::<TodoApp>::open(petros::open_memory().unwrap(), "alice", AutoCtx::seeded(1))
            .unwrap();
    alice.set_session(Some(login.session.clone()));
    alice.set_token(Some(login.token.clone()));
    let link = Link::<Payload>::connect(&petros_auth::socket_url(&server)).unwrap();
    alice.connected().unwrap();
    alice.mutate(todo::add("hers".into())).unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && alice.pending_len() > 0 {
        pump(
            &mut alice,
            &link,
            Instant::now() + Duration::from_millis(20),
        );
    }
    assert_eq!(alice.pending_len(), 0, "confirmed");
    assert_eq!(alice.take_denial(), None);

    // Signed out: the same token is turned away, and nothing is lost.
    petros_auth::client::logout(&server, &login.token).unwrap();
    assert_eq!(
        petros_auth::client::whoami(&server, &login.token).unwrap(),
        None
    );
    let again = Link::<Payload>::connect(&petros_auth::socket_url(&server)).unwrap();
    alice.mutate(todo::add("after".into())).unwrap();
    alice.connected().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut denial = None;
    while Instant::now() < deadline && denial.is_none() {
        pump(
            &mut alice,
            &again,
            Instant::now() + Duration::from_millis(20),
        );
        denial = alice.take_denial();
    }
    assert_eq!(denial.as_deref(), Some("not signed in"));
    assert_eq!(alice.pending_len(), 1, "kept for the next login");
}

#[test]
fn nobody_else_can_write_as_her() {
    let (_runtime, server) = serve();
    let alice = petros_auth::client::login(&server, Some("alice"), |_| {}).unwrap();

    // Signed in as alice, authoring as bob.
    let mut mallory =
        Client::<TodoApp>::open(petros::open_memory().unwrap(), "bob", AutoCtx::seeded(2)).unwrap();
    mallory.set_token(Some(alice.token));
    let link = Link::<Payload>::connect(&petros_auth::socket_url(&server)).unwrap();
    mallory.connected().unwrap();
    mallory.mutate(todo::add("as bob".into())).unwrap();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut rejections = Vec::new();
    while Instant::now() < deadline && rejections.is_empty() {
        pump(
            &mut mallory,
            &link,
            Instant::now() + Duration::from_millis(20),
        );
        rejections = mallory.take_rejections();
    }
    assert_eq!(rejections.len(), 1);
    assert_eq!(
        rejections[0].reason,
        "authored as bob but signed in as alice"
    );
    assert_eq!(mallory.pending_len(), 0, "and it will not be offered again");
}

#[test]
fn a_code_cannot_go_somewhere_else() {
    let (_runtime, server) = serve();
    let url = petros_auth::login_url(&server, "https://elsewhere.example/", Some("alice"));
    let err = ureq::AgentBuilder::new()
        .redirects(0)
        .build()
        .get(&url)
        .call()
        .unwrap_err();
    let ureq::Error::Status(400, resp) = err else {
        panic!("refused with 400, not {err}");
    };
    assert!(resp.into_string().unwrap().contains("cannot be sent"));
}
