//! A Petros server, as an axum handler.
//!
//! The engine is sans-io on purpose — `petros::Server` owns no socket, no
//! runtime and no thread — so putting it behind HTTP is an adapter, not a port.
//! This is that adapter, and it is deliberately the smaller half: the app keeps
//! its own `Router`, its own path, and its own authentication.
//!
//! ```ignore
//! let hub = petros_axum::Hub::<TodoApp>::open(petros::open_path("server.db")?, sessions)?;
//! let app = Router::new()
//!     .route("/sync", get(petros_axum::sync::<TodoApp>))
//!     .route("/healthz", get(healthz))
//!     .with_state(hub);
//! ```
//!
//! # Who a client is
//!
//! Is decided by the [`Authenticate`] the hub is opened with, at every
//! `Hello`: the engine asks it what the client's token proves and refuses
//! entries from anyone else. `petros-auth` is one — sessions it issued after
//! an OpenID Connect login — and [`petros::Trusting`] is none, for a server
//! behind something that has already decided. Nothing here reads a header:
//! the token travels in the frame, so a browser, a phone and a desktop all
//! prove themselves the same way.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use petros::{
    decode, encode, App, Authenticate, ClientMsg, ConnId, Connection, Identity, Result, Server,
    ServerMsg,
};
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};

/// How often the server pings a quiet connection.
///
/// A WebSocket that says nothing is a WebSocket something between the two
/// ends will eventually close: nginx gives an idle proxied connection 60
/// seconds by default, and a NAT table or a tailnet is no more patient. Sync
/// is quiet whenever nobody is mutating, which is nearly always, so without
/// this every client spends its life reconnecting — and *looks* to the person
/// using it like a server that keeps dropping.
///
/// A browser cannot send a ping from JavaScript, but it answers one, and that
/// answer is traffic in the other direction. So one ping from here keeps both
/// halves of the path alive, which is why the keepalive is the server's and
/// no client has to have one.
const PING: Duration = Duration::from_secs(20);

/// How many pings may go unanswered before the connection is treated as gone.
///
/// The point is not tidiness: a socket that is dead at the network level and
/// open as far as the operating system is concerned is exactly the state in
/// which a server goes on believing a device is listening. Three is about a
/// minute, which is longer than any pause a live connection takes and shorter
/// than anyone will wait.
const MISSED: u64 = 3;

/// The server, plus who is currently connected to it.
///
/// One of these per database. Clone the `Arc` into axum's state; every
/// connection shares it, which is what makes a push from one peer reach the
/// others.
pub struct Hub<A: App> {
    server: Mutex<Server<A>>,
    peers: Mutex<HashMap<ConnId, UnboundedSender<Vec<u8>>>>,
    /// Peers the server stands in for: a speaker in the house, a bridge to
    /// something that will never hold a replica. They are in a room and not
    /// in the log, so what reaches them is the realtime payload alone and
    /// never an encoded [`ServerMsg`]. See [`Hub::stand`].
    standing: Mutex<HashMap<ConnId, UnboundedSender<Vec<u8>>>>,
    next: AtomicU64,
}

impl<A: App> Hub<A> {
    /// Open the server's database, running Petros' migrations and the app's,
    /// with `auth` deciding who each connection is.
    pub fn open(conn: Connection, auth: impl Authenticate) -> Result<Arc<Self>> {
        Ok(Arc::new(Hub {
            server: Mutex::new(Server::open_with(conn, auth)?),
            peers: Mutex::new(HashMap::new()),
            standing: Mutex::new(HashMap::new()),
            next: AtomicU64::new(1),
        }))
    }

    /// As [`open`](Hub::open), with the app's realtime machine attached. See
    /// [`petros::live`].
    pub fn open_live(
        conn: Connection,
        auth: impl Authenticate,
        live: impl petros::Live,
    ) -> Result<Arc<Self>> {
        Ok(Arc::new(Hub {
            server: Mutex::new(Server::open_with(conn, auth)?.with_live(live)),
            peers: Mutex::new(HashMap::new()),
            standing: Mutex::new(HashMap::new()),
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

    /// A connection id for a peer that is not a socket.
    ///
    /// From the same counter the sockets draw from, so an in-process peer can
    /// never collide with one that arrives later.
    pub fn local(&self) -> ConnId {
        self.next.fetch_add(1, Ordering::Relaxed)
    }

    /// Put a peer in a room without giving it a socket or a replica.
    ///
    /// For something that is a *device* and not a client: a speaker the
    /// server drives over somebody else's API, a bridge, a test. It receives
    /// only what the realtime channel addresses to it — the app's own `Hear`
    /// payload, undecorated, because there is no log here for it to be part
    /// of. Say things back with [`Hub::say`], and take it out with
    /// [`Hub::unstand`].
    pub fn stand(&self, who: Identity, hears: UnboundedSender<Vec<u8>>) -> Result<ConnId> {
        let conn = self.local();
        if let Ok(mut standing) = self.standing.lock() {
            standing.insert(conn, hears);
        }
        let outgoing = {
            let mut server = self.server();
            server.stand(conn, who)?;
            server.take_outgoing()
        };
        self.deliver(conn, outgoing);
        Ok(conn)
    }

    /// Take a standing peer out again — the same thing a dropped socket does.
    pub fn unstand(&self, conn: ConnId) {
        let outgoing = {
            let mut server = self.server();
            server.unstand(conn);
            server.take_outgoing()
        };
        self.deliver(conn, outgoing);
        if let Ok(mut standing) = self.standing.lock() {
            standing.remove(&conn);
        }
    }

    /// Say something on the realtime channel as a standing peer.
    pub fn say(&self, conn: ConnId, say: Vec<u8>) {
        self.route(conn, ClientMsg::Say { say });
    }

    /// Feed one message in from a peer that has no socket, and hand back what
    /// the server addressed to it.
    ///
    /// Everything the server said to *other* peers is delivered to their
    /// sockets on the way past, which is the part an application cannot do for
    /// itself: `peers` is private and the outgoing queue is shared, so a writer
    /// that only appended would leave the fan-out sitting there until some
    /// other peer happened to speak. A library scanner adding a file at three
    /// in the morning is exactly that case — nobody else is speaking.
    ///
    /// Pair it with [`Hub::local`] for the id. The peer on the other end is an
    /// ordinary [`petros::Client`]: hand it `take_outgoing`, give it back what
    /// this returns.
    pub fn exchange(
        &self,
        from: ConnId,
        msg: ClientMsg<A::Mutation>,
    ) -> Vec<ServerMsg<A::Mutation>> {
        let outgoing = {
            let mut server = self.server();
            if server.recv(from, msg).is_err() {
                return Vec::new();
            }
            server.take_outgoing()
        };
        let mut mine = Vec::new();
        let mut others = Vec::new();
        for (conn, msg) in outgoing {
            if conn == from {
                mine.push(msg);
            } else {
                others.push((conn, msg));
            }
        }
        self.deliver(from, others);
        mine
    }

    /// Send each message to whatever is carrying that connection, and say
    /// whether `from` is still welcome.
    fn deliver(&self, from: ConnId, outgoing: Vec<(ConnId, ServerMsg<A::Mutation>)>) -> bool {
        let peers = self.peers.lock().ok();
        let standing = self.standing.lock().ok();
        let mut keep = true;
        for (conn, msg) in outgoing {
            if conn == from && matches!(msg, ServerMsg::Denied { .. }) {
                keep = false;
            }
            if let Some(sink) = standing.as_ref().and_then(|s| s.get(&conn)) {
                // The one frame that means anything to a peer with no
                // replica. Everything else about the log is not its business.
                if let ServerMsg::Heard { hear } = msg {
                    let _ = sink.send(hear);
                }
                continue;
            }
            if let (Some(peers), Ok(frame)) = (peers.as_ref(), encode(&msg)) {
                if let Some(peer) = peers.get(&conn) {
                    let _ = peer.send(frame);
                }
            }
        }
        keep
    }

    /// Feed one message in and deliver everything that falls out. Says
    /// whether the connection is still welcome: a denial is the last frame
    /// it gets, and the socket is closed behind it.
    ///
    /// The lock is dropped before anything is sent, and nothing is awaited
    /// while it is held — the server's work is a few SQLite statements, tens of
    /// microseconds, so it runs inline rather than on a blocking pool.
    fn route(&self, from: ConnId, msg: ClientMsg<A::Mutation>) -> bool {
        let outgoing = {
            let mut server = self.server();
            if server.recv(from, msg).is_err() {
                return false;
            }
            server.take_outgoing()
        };
        self.deliver(from, outgoing)
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

    // How many pings have gone out since this peer last said anything at all.
    // The reader resets it on every frame, a pong included; the writer counts
    // up and gives up at `MISSED`. A counter rather than a clock because
    // neither end's clock is needed to answer "has it spoken since?", and a
    // clock that jumps would answer it wrongly.
    let quiet = Arc::new(AtomicU64::new(0));

    // One task writes, this one reads. The server may address a peer at any
    // time — that is the whole point of fan-out — so the write side cannot be
    // driven by this connection's own reads. Closing the channel ends the
    // task once everything queued — a denial, say — has been written.
    let writer = tokio::spawn({
        let quiet = quiet.clone();
        async move {
            let mut beat = tokio::time::interval(PING);
            // The first tick is immediate, and a ping before the client has
            // had time to say `Hello` is a ping at the wrong moment.
            beat.tick().await;
            beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    frame = rx.recv() => {
                        let Some(frame) = frame else { break };
                        if sink.send(Message::Binary(frame.into())).await.is_err() {
                            break;
                        }
                    }
                    _ = beat.tick() => {
                        if quiet.fetch_add(1, Ordering::Relaxed) >= MISSED {
                            // Nothing has come back for about a minute. The
                            // socket is open and the peer is not there, which
                            // is the state a server has to be able to leave
                            // on its own — nothing else will tell it.
                            break;
                        }
                        if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                            break;
                        }
                    }
                }
            }
            let _ = sink.close().await;
        }
    });

    while let Some(Ok(message)) = stream.next().await {
        // Anything at all is evidence the peer is there, which is what a pong
        // is for: it carries nothing and is the only thing a quiet connection
        // ever sends.
        quiet.store(0, Ordering::Relaxed);
        // Text, ping, pong and close carry nothing else for us; the protocol
        // is binary.
        let Message::Binary(bytes) = message else {
            continue;
        };
        match decode::<ClientMsg<A::Mutation>>(&bytes) {
            Ok(msg) => {
                if !hub.route(conn, msg) {
                    break;
                }
            }
            // A frame we cannot read is this peer's problem, not the server's.
            Err(_) => break,
        }
    }

    if let Ok(mut peers) = hub.peers.lock() {
        peers.remove(&conn);
    }
    // Everything the disconnect produced — a room saying this device has gone
    // — is addressed to the *other* peers, so it has to go out before this
    // task ends: it sits in the outbox and nothing else will collect it,
    // because nobody else is speaking. Under one lock, so no other connection
    // can take it in between and deliver it on a socket that is about to be
    // in the same position.
    let outgoing = {
        let mut server = hub.server();
        server.disconnect(conn);
        server.take_outgoing()
    };
    hub.deliver(conn, outgoing);
    // The writer is waiting on a channel this connection's sender has just
    // been dropped from, so it ends on its own.
    let _ = writer.await;
}
