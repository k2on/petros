//! Blocking WebSocket plumbing. Synchronous on purpose: there is no async
//! anywhere in this crate, and a transport is not a good reason to start.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tungstenite::{Message, WebSocket};

use crate::{decode, encode, App, ClientMsg, ConnId, Error, Result, Server, ServerMsg};

/// How long a read blocks before we go and look at the outgoing queue. Small
/// enough to feel live, large enough not to spin.
const TICK: Duration = Duration::from_millis(20);

impl From<tungstenite::Error> for Error {
    fn from(e: tungstenite::Error) -> Self {
        Error::Transport(e.to_string())
    }
}

/// A connection to an Exo server. Owns a thread; drop it to disconnect.
#[derive(Debug)]
pub struct Link<M> {
    outgoing: Sender<ClientMsg<M>>,
    incoming: Receiver<ServerMsg<M>>,
    alive: Arc<AtomicBool>,
}

impl<M: serde::Serialize + serde::de::DeserializeOwned + Send + 'static> Link<M> {
    /// Connect to `ws://host:port`.
    pub fn connect(url: &str) -> Result<Self> {
        let (socket, _) = tungstenite::connect(url)?;
        let (out_tx, out_rx) = channel();
        let (in_tx, in_rx) = channel();
        let alive = Arc::new(AtomicBool::new(true));
        let flag = alive.clone();
        std::thread::spawn(move || {
            pump(socket, out_rx, in_tx, &flag);
            flag.store(false, Ordering::SeqCst);
        });
        Ok(Link {
            outgoing: out_tx,
            incoming: in_rx,
            alive,
        })
    }

    /// Queue a message. Returns false once the link is gone.
    pub fn send(&self, msg: ClientMsg<M>) -> bool {
        self.outgoing.send(msg).is_ok() && self.is_alive()
    }

    /// Take one message the server sent, if any. Never blocks.
    pub fn try_recv(&self) -> Option<ServerMsg<M>> {
        self.incoming.try_recv().ok()
    }

    pub fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
}

impl<M> Drop for Link<M> {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::SeqCst);
    }
}

/// Serve an Exo server over WebSocket until the listener dies.
///
/// One thread per connection, all sharing the state machine behind a mutex.
/// After handling a message a thread drains the server's whole outbox and
/// routes each message to the connection it is addressed to, which is how a
/// push from one client reaches the others.
pub fn serve<A: App>(listener: TcpListener, server: Arc<Mutex<Server<A>>>) -> Result<()>
where
    A::Mutation: Send,
{
    let peers: Peers<A::Mutation> = Arc::new(Mutex::new(HashMap::new()));
    let mut next_conn: ConnId = 1;
    for stream in listener.incoming() {
        // One peer failing to connect is not a reason to stop serving the rest.
        let Ok(stream) = stream else { continue };
        let conn = next_conn;
        next_conn += 1;
        let (tx, rx) = channel();
        peers.lock().map_err(poisoned)?.insert(conn, tx);
        let server = server.clone();
        let peers = peers.clone();
        std::thread::spawn(move || {
            if let Ok(socket) = tungstenite::accept(stream) {
                let alive = Arc::new(AtomicBool::new(true));
                let inbound = |msg: ClientMsg<A::Mutation>| route(&server, &peers, conn, msg);
                serve_one(socket, rx, inbound, &alive);
            }
            if let Ok(mut s) = server.lock() {
                s.disconnect(conn);
            }
            if let Ok(mut p) = peers.lock() {
                p.remove(&conn);
            }
        });
    }
    Ok(())
}

type Peers<M> = Arc<Mutex<HashMap<ConnId, Sender<ServerMsg<M>>>>>;

/// Feed one message into the server and deliver everything that falls out.
fn route<A: App>(
    server: &Mutex<Server<A>>,
    peers: &Peers<A::Mutation>,
    from: ConnId,
    msg: ClientMsg<A::Mutation>,
) {
    let Ok(mut server) = server.lock() else {
        return;
    };
    if server.recv(from, msg).is_err() {
        return;
    }
    let outgoing = server.take_outgoing();
    drop(server);
    let Ok(peers) = peers.lock() else { return };
    for (conn, msg) in outgoing {
        if let Some(peer) = peers.get(&conn) {
            let _ = peer.send(msg);
        }
    }
}

/// The read/write loop, shared by both ends. `inbound` is called for each
/// decoded message; `outgoing` is drained onto the socket between reads.
fn serve_one<In, Out, S, F>(
    mut socket: WebSocket<S>,
    outgoing: Receiver<Out>,
    mut inbound: F,
    alive: &AtomicBool,
) where
    In: serde::de::DeserializeOwned,
    Out: serde::Serialize,
    S: Read + Write + Timeout,
    F: FnMut(In),
{
    socket.get_mut().set_tick(TICK);
    while alive.load(Ordering::SeqCst) {
        match socket.read() {
            Ok(Message::Binary(bytes)) => match decode::<In>(&bytes) {
                Ok(msg) => inbound(msg),
                Err(_) => break,
            },
            Ok(Message::Close(_)) => break,
            Ok(_) => {}
            // A read timeout is how we get a turn to write; anything else has
            // actually broken the connection.
            Err(tungstenite::Error::Io(e)) if would_block(&e) => {}
            Err(_) => break,
        }
        loop {
            match outgoing.try_recv() {
                Ok(msg) => match encode(&msg) {
                    Ok(bytes) => {
                        if socket.send(Message::Binary(bytes)).is_err() {
                            return;
                        }
                    }
                    Err(_) => return,
                },
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }
    }
    let _ = socket.close(None);
}

fn pump<M: serde::Serialize + serde::de::DeserializeOwned, S: Read + Write + Timeout>(
    socket: WebSocket<S>,
    outgoing: Receiver<ClientMsg<M>>,
    incoming: Sender<ServerMsg<M>>,
    alive: &AtomicBool,
) {
    serve_one(
        socket,
        outgoing,
        |msg: ServerMsg<M>| {
            let _ = incoming.send(msg);
        },
        alive,
    );
}

fn would_block(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

fn poisoned<T>(_: T) -> Error {
    Error::Transport("a transport lock was poisoned".into())
}

/// Lets both a plain and a maybe-TLS stream be given a read timeout.
pub trait Timeout {
    fn set_tick(&mut self, tick: Duration);
}

impl Timeout for TcpStream {
    fn set_tick(&mut self, tick: Duration) {
        let _ = self.set_read_timeout(Some(tick));
    }
}

impl Timeout for tungstenite::stream::MaybeTlsStream<TcpStream> {
    fn set_tick(&mut self, tick: Duration) {
        if let tungstenite::stream::MaybeTlsStream::Plain(s) = self {
            s.set_tick(tick);
        }
    }
}
