//! The realtime half: what is true *now*, and worth nothing tomorrow.
//!
//! A mutation is a fact about the world and is replayed forever. "Which
//! speaker is playing, and how far into the track" is not that: it is true at
//! three o'clock, false at four, and no peer should ever replay an afternoon
//! of it. Writing it to the log would be writing down the one thing that
//! *should* be lost.
//!
//! So there is a second channel, on the same socket, with none of the log's
//! properties. It is:
//!
//! - **server-owned.** One [`Live`] machine holds every room's state; peers
//!   say things to it and are told what came of that. There is no rebase, no
//!   ordering, and no optimistic copy the engine maintains — an app that
//!   wants one guesses locally and lets the next broadcast correct it.
//! - **room-scoped.** A room is one account: every connection the server
//!   authenticated as the same user is in the same room, and nothing crosses
//!   between rooms. Under [`Trusting`](crate::Trusting) there is one room,
//!   because there is one anybody.
//! - **not durable, except deliberately.** A room lives in memory and is
//!   dropped when the last peer leaves. What it leaves behind is at most one
//!   row in `petros_live` — a *snapshot*, written only when the app asks for
//!   one with [`Post::keep`], read back when the room next opens. One value
//!   per room, last write wins. It is not a log and is never replayed.
//!
//! # Why it rides the sync socket
//!
//! Because a second socket is a second thing to authenticate, a second thing
//! to reconnect, a second thing to keep alive through a proxy, and a second
//! answer to "am I online". An app that had one would find its two sockets
//! disagreeing about that at the worst moment — which is exactly what happens
//! when a phone wakes up.
//!
//! # What the engine does not know
//!
//! What is inside a frame. The payload is CBOR of the app's own types, and
//! the engine carries the bytes: [`Live::Say`] and [`Live::Hear`] are the
//! app's, decoded and encoded at the edge of its own machine. That is what
//! keeps the log's compatibility rules — which are absolute — off a protocol
//! that is allowed to change every week, because nothing ever replays a live
//! frame.

use std::collections::BTreeMap;

use serde::{de::DeserializeOwned, Serialize};

use crate::{ConnId, Identity};

/// Which room a peer is in. One account, as the server authenticated it.
pub type Room = String;

/// One peer of a room.
///
/// `who` is what names it *stably*, which is not the connection: a phone that
/// suspends and comes back is the same device and must not appear twice in a
/// picker. For an authenticated client that is the login — `petros-auth` says
/// a session is "one login on one device", which is exactly this. For a peer
/// the server stands in for, it is whatever the caller of
/// [`Server::stand`](crate::Server::stand) said, because a speaker in a
/// kitchen has no login and is still the same speaker tomorrow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// Which connection carries it. Changes across a reconnect.
    pub conn: ConnId,
    pub room: Room,
    /// What names it across reconnects.
    pub who: String,
}

impl Peer {
    pub(crate) fn of(conn: ConnId, who: &Identity) -> Peer {
        Peer {
            conn,
            room: who.user.as_str().to_string(),
            who: who.session.clone(),
        }
    }

    /// The one room a server that authenticates nobody has.
    pub(crate) fn anon(conn: ConnId) -> Peer {
        Peer {
            conn,
            room: String::new(),
            who: conn.to_string(),
        }
    }
}

/// What a [`Live`] machine says, and to whom.
///
/// Addressed rather than returned, so that "tell the room" and "tell the one
/// device that is playing" are the same sentence length. The roster is the
/// engine's — it is the half that knows who is connected — so the app never
/// keeps a list of sockets and cannot get one out of step with reality.
#[derive(Debug)]
pub struct Post<'a, H> {
    peers: &'a [Peer],
    out: Vec<(ConnId, H)>,
    keep: bool,
}

impl<'a, H: Clone> Post<'a, H> {
    fn new(peers: &'a [Peer]) -> Post<'a, H> {
        Post {
            peers,
            out: Vec::new(),
            keep: false,
        }
    }

    /// Everybody in this room, right now.
    pub fn peers(&self) -> &[Peer] {
        self.peers
    }

    /// Whether a peer named `who` is here. A device that has gone is a device
    /// that cannot be told to do anything.
    pub fn here(&self, who: &str) -> bool {
        self.peers.iter().any(|p| p.who == who)
    }

    /// To one peer, by the name that survives its reconnects.
    ///
    /// A name nobody here answers to is dropped. That is not an error: the
    /// device the app is addressing may have closed its laptop between the
    /// frame arriving and this running.
    pub fn tell(&mut self, who: &str, hear: H) {
        for peer in self.peers.iter().filter(|p| p.who == who) {
            self.out.push((peer.conn, hear.clone()));
        }
    }

    /// To everybody in the room, including whoever prompted it.
    pub fn tell_room(&mut self, hear: H) {
        for peer in self.peers {
            self.out.push((peer.conn, hear.clone()));
        }
    }

    /// Keep this room's state across a restart.
    ///
    /// Asked for rather than automatic, because the difference matters: a
    /// position report arrives every second and is worth nothing once it is a
    /// second old, while "the sound is on the kitchen speaker, at this point
    /// in this queue" is worth a disk write. The app says which it just did.
    pub fn keep(&mut self) {
        self.keep = true;
    }
}

/// The app's realtime machine: every room's state, and the rules that move it.
///
/// One instance per server, holding every room — so an app that wants a map
/// keyed by [`Room`] writes one, which is usually what the state is anyway.
/// The engine calls in on the connection's thread with the server's lock
/// held, so these are expected to be fast and to touch nothing outside
/// themselves. Nothing here is `async` for the same reason nothing else in
/// this engine is.
pub trait Live: Send + 'static {
    /// What a peer says.
    type Say: DeserializeOwned;
    /// What the server says back.
    type Hear: Serialize + Clone;

    /// A peer's connection opened. It is already in [`Post::peers`].
    ///
    /// The engine says nothing of its own here: whether a fresh peer is told
    /// the room's state at once, or waits until it has introduced itself, is
    /// the app's question and the two answers are both reasonable.
    fn join(&mut self, peer: &Peer, post: &mut Post<'_, Self::Hear>) {
        let _ = (peer, post);
    }

    /// A peer said something.
    fn say(&mut self, peer: &Peer, say: Self::Say, post: &mut Post<'_, Self::Hear>);

    /// A peer's connection closed. It is already out of [`Post::peers`].
    ///
    /// This is a *socket* going, which is not the same as a device going
    /// away, and an app that conflates the two is an app that loses your
    /// music when your phone sleeps.
    fn part(&mut self, peer: &Peer, post: &mut Post<'_, Self::Hear>) {
        let _ = (peer, post);
    }

    /// This room's state, to be handed back to [`wake`](Live::wake) after a
    /// restart. `None` for a room not worth keeping.
    ///
    /// Called when the app asked for it with [`Post::keep`], and once more
    /// when the room empties.
    fn snapshot(&mut self, room: &Room) -> Option<Vec<u8>> {
        let _ = room;
        None
    }

    /// Take back what [`snapshot`](Live::snapshot) wrote. Called before the
    /// first peer of a room joins, at most once per room per run.
    ///
    /// A snapshot this build cannot read is a snapshot to ignore: the room
    /// starts empty, which is always a correct thing for a room to be.
    fn wake(&mut self, room: &Room, snapshot: &[u8]) {
        let _ = (room, snapshot);
    }

    /// Nobody is in this room any more. Free whatever it held; the snapshot
    /// has already been taken.
    fn close(&mut self, room: &Room) {
        let _ = room;
    }
}

/// [`Live`], with the app's types encoded away.
///
/// The engine holds one of these behind a `dyn`, so that [`crate::Server`]
/// stays generic over the app's *mutation* alone and a live protocol cannot
/// leak into the types the log is written with.
pub(crate) trait Rooms: Send {
    fn join(&mut self, peer: &Peer, peers: &[Peer]) -> Posted;
    fn say(&mut self, peer: &Peer, say: &[u8], peers: &[Peer]) -> Posted;
    fn part(&mut self, peer: &Peer, peers: &[Peer]) -> Posted;
    fn snapshot(&mut self, room: &Room) -> Option<Vec<u8>>;
    fn wake(&mut self, room: &Room, snapshot: &[u8]);
    fn close(&mut self, room: &Room);
}

/// What one call produced: frames to deliver, and whether to write the
/// room down.
#[derive(Debug, Default)]
pub(crate) struct Posted {
    pub out: Vec<(ConnId, Vec<u8>)>,
    pub keep: bool,
}

/// The adapter that erases one.
pub(crate) struct Erased<L>(pub L);

impl<L: Live> Erased<L> {
    fn finish(post: Post<'_, L::Hear>) -> Posted {
        Posted {
            out: post
                .out
                .into_iter()
                // A frame that will not encode is a bug in the app's own
                // types, and dropping it is better than taking the whole
                // server down over one device's news.
                .filter_map(|(conn, hear)| crate::encode(&hear).ok().map(|bytes| (conn, bytes)))
                .collect(),
            keep: post.keep,
        }
    }
}

impl<L: Live> Rooms for Erased<L> {
    fn join(&mut self, peer: &Peer, peers: &[Peer]) -> Posted {
        let mut post = Post::new(peers);
        self.0.join(peer, &mut post);
        Self::finish(post)
    }

    fn say(&mut self, peer: &Peer, say: &[u8], peers: &[Peer]) -> Posted {
        // A frame this build cannot read is a sentence a newer peer invented.
        // Ignoring it is right; dropping the socket over it is not, because
        // the same socket is carrying the log.
        let Ok(say) = crate::decode::<L::Say>(say) else {
            return Posted::default();
        };
        let mut post = Post::new(peers);
        self.0.say(peer, say, &mut post);
        Self::finish(post)
    }

    fn part(&mut self, peer: &Peer, peers: &[Peer]) -> Posted {
        let mut post = Post::new(peers);
        self.0.part(peer, &mut post);
        Self::finish(post)
    }

    fn snapshot(&mut self, room: &Room) -> Option<Vec<u8>> {
        self.0.snapshot(room)
    }

    fn wake(&mut self, room: &Room, snapshot: &[u8]) {
        self.0.wake(room, snapshot)
    }

    fn close(&mut self, room: &Room) {
        self.0.close(room)
    }
}

/// Who is in which room, as the engine sees it.
///
/// Kept beside the connection table rather than derived from it, because a
/// peer the server *stands in for* — a speaker, a bridge — has no connection
/// of its own and belongs to a room all the same.
#[derive(Debug, Default)]
pub(crate) struct Roster {
    by_conn: BTreeMap<ConnId, Peer>,
    /// Rooms whose snapshot has already been offered this run, so a room that
    /// empties and reopens does not resume an afternoon it has since left
    /// behind.
    woken: std::collections::BTreeSet<Room>,
}

impl Roster {
    /// Put a peer in. Returns the peer it replaced, if this connection was
    /// already somewhere — a `Hello` that re-authenticates as somebody else.
    pub fn insert(&mut self, peer: Peer) -> Option<Peer> {
        self.by_conn.insert(peer.conn, peer)
    }

    pub fn remove(&mut self, conn: ConnId) -> Option<Peer> {
        self.by_conn.remove(&conn)
    }

    pub fn get(&self, conn: ConnId) -> Option<&Peer> {
        self.by_conn.get(&conn)
    }

    /// Everybody in one room.
    pub fn room(&self, room: &str) -> Vec<Peer> {
        self.by_conn
            .values()
            .filter(|p| p.room == room)
            .cloned()
            .collect()
    }

    /// Whether this room has anybody in it.
    pub fn occupied(&self, room: &str) -> bool {
        self.by_conn.values().any(|p| p.room == room)
    }

    /// Whether this room still has to be offered its snapshot, marking it
    /// offered. True once per room per run.
    pub fn first_sight(&mut self, room: &Room) -> bool {
        self.woken.insert(room.clone())
    }

    /// A room that has emptied is a room to forget, snapshot and all — so
    /// that opening it tomorrow reads the disk again rather than a memory of
    /// a memory.
    pub fn forget(&mut self, room: &Room) {
        self.woken.remove(room);
    }
}
