//! A Petros server, as an axum handler.
//!
//! The engine is sans-io on purpose — `petros::Server` owns no socket, no
//! runtime and no thread — so putting it behind HTTP is an adapter, not a port.
//! This is that adapter, and it is deliberately the smaller half: the app keeps
//! its own `Router`, its own path, and its own authentication.
//!
//! ```ignore
//! let hub = petros_axum::Hub::<TodoApp>::open(petros::open_path("server.db")?)?;
//! let app = Router::new()
//!     .route("/sync", get(petros_axum::sync::<TodoApp>))
//!     .route("/healthz", get(healthz))
//!     .layer(your_auth_layer)
//!     .with_state(hub);
//! ```
//!
//! # What this does not do
//!
//! It does not authenticate. A client asserts its own actor inside each entry,
//! and the engine takes that at face value — so anything that matters has to be
//! enforced by a layer above, which is why this is a handler you mount rather
//! than a server you start. Give it a route behind your own middleware and the
//! middleware runs first.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use petros::{decode, encode, App, ClientMsg, ConnId, Connection, Result, Server};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

/// The server, plus who is currently connected to it.
///
/// One of these per database. Clone the `Arc` into axum's state; every
/// connection shares it, which is what makes a push from one peer reach the
/// others.
pub struct Hub<A: App> {
    server: Mutex<Server<A>>,
    peers: Mutex<HashMap<ConnId, UnboundedSender<Vec<u8>>>>,
    next: AtomicU64,
}

impl<A: App> Hub<A> {
    /// Open the server's database, running Petros' migrations and the app's.
    pub fn open(conn: Connection) -> Result<Arc<Self>> {
        Ok(Arc::new(Hub {
            server: Mutex::new(Server::open(conn)?),
            peers: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        }))
    }

    /// The server itself, for anything the app wants to read — the head
    /// sequence number for a health check, the materialised state for a plain
    /// HTTP route beside the socket.
    ///
    /// Holding this blocks every connection, so do not hold it across an
    /// `.await`.
    pub fn server(&self) -> std::sync::MutexGuard<'_, Server<A>> {
        self.server.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// How many peers are connected right now.
    pub fn connected(&self) -> usize {
        self.peers.lock().map(|p| p.len()).unwrap_or(0)
    }

    /// Feed one message in and deliver everything that falls out.
    ///
    /// The lock is dropped before anything is sent, and nothing is awaited
    /// while it is held — the server's work is a few SQLite statements, tens of
    /// microseconds, so it runs inline rather than on a blocking pool.
    fn route(&self, from: ConnId, msg: ClientMsg<A::Mutation>) {
        let outgoing = {
            let mut server = self.server();
            if server.recv(from, msg).is_err() {
                return;
            }
            server.take_outgoing()
        };
        let Ok(peers) = self.peers.lock() else { return };
        for (conn, msg) in outgoing {
            if let (Some(peer), Ok(frame)) = (peers.get(&conn), encode(&msg)) {
                let _ = peer.send(frame);
            }
        }
    }
}

/// The sync endpoint: a WebSocket carrying encoded Petros frames.
///
/// Mount it wherever you like — `get(sync::<YourApp>)` — and put whatever
/// middleware you want in front of it.
pub async fn sync<A>(ws: WebSocketUpgrade, State(hub): State<Arc<Hub<A>>>) -> Response
where
    A: App + Send + 'static,
    A::Mutation: Send + 'static,
{
    ws.on_upgrade(move |socket| serve(socket, hub))
}

async fn serve<A>(socket: WebSocket, hub: Arc<Hub<A>>)
where
    A: App + Send + 'static,
    A::Mutation: Send + 'static,
{
    let conn = hub.next.fetch_add(1, Ordering::Relaxed);
    let (mut sink, mut stream) = socket.split();
    let (tx, mut rx) = unbounded_channel::<Vec<u8>>();
    if let Ok(mut peers) = hub.peers.lock() {
        peers.insert(conn, tx);
    }

    // One task writes, this one reads. The server may address a peer at any
    // time — that is the whole point of fan-out — so the write side cannot be
    // driven by this connection's own reads.
    let writer = tokio::spawn(async move {
        while let Some(frame) = rx.recv().await {
            if sink.send(Message::Binary(frame.into())).await.is_err() {
                break;
            }
        }
    });

    while let Some(Ok(message)) = stream.next().await {
        // Text, ping and close carry nothing for us; the protocol is binary.
        let Message::Binary(bytes) = message else {
            continue;
        };
        match decode::<ClientMsg<A::Mutation>>(&bytes) {
            Ok(msg) => hub.route(conn, msg),
            // A frame we cannot read is this peer's problem, not the server's.
            Err(_) => break,
        }
    }

    if let Ok(mut peers) = hub.peers.lock() {
        peers.remove(&conn);
    }
    hub.server().disconnect(conn);
    writer.abort();
}
