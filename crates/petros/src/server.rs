//! The server state machine. Sans-io: feed it messages, drain its outbox.
//!
//! The server owns the one true order. It assigns sequence numbers, never
//! reorders, never rewrites. Everything a client does is provisional until it
//! appears here.

use std::collections::BTreeMap;
use std::marker::PhantomData;

use diesel::connection::SimpleConnection;

use crate::live::{Erased, Live, Peer, Posted, Room, Rooms, Roster};
use crate::{
    store, ActorId, App, ClientMsg, Connection, Entry, Error, Mutation, MutationError, Result, Seq,
    ServerMsg, Transaction,
};

/// How many entries one [`ServerMsg::Batch`] carries. A client that sees
/// `has_more` sends another `Hello`.
const BATCH_LIMIT: usize = 256;

/// Identifies one connected client. Assigned by the transport, meaningless to
/// Petros beyond "messages tagged with this go back down the same pipe".
pub type ConnId = u64;

/// Who a connection proved it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// The user, which is what every entry's `actor` has to be.
    pub user: ActorId,
    /// The login, which is what an entry's `session` usually is — an entry
    /// authored under an earlier login of the same user is fine too, and
    /// [`Authenticate::owns`] is how that is checked.
    pub session: String,
}

/// How a server decides who is talking to it.
///
/// The engine is sans-io and has no idea what a token is; this is the one
/// question it asks about one, at every `Hello`. `petros-auth` answers it
/// with sessions it issued after an OpenID Connect login; a test answers it
/// with a table. [`Trusting`] answers it with whatever the client said, which
/// is the engine's behaviour before it asked at all.
pub trait Authenticate: Send + 'static {
    /// The identity `token` proves, or `None` for one that proves nothing.
    fn authenticate(&mut self, token: Option<&str>) -> Option<Identity>;

    /// Whether `session` is, or was, a login of `user`. An entry carries the
    /// session it was authored under, and a client that signed in again
    /// since still has entries from before — they are the same person's.
    fn owns(&mut self, user: &ActorId, session: &str) -> bool {
        let _ = (user, session);
        false
    }

    /// Whether every connection is whoever its entries say. Only
    /// [`Trusting`] answers yes; an authenticator that has no answer for a
    /// token turns the connection away, and does not fall back to this.
    fn trusts_everyone(&self) -> bool {
        false
    }
}

/// No authentication: a client is whoever its entries say. For a server
/// behind something else that has already decided, a simulation, or a test.
#[derive(Debug, Default, Clone, Copy)]
pub struct Trusting;

impl Authenticate for Trusting {
    fn authenticate(&mut self, _token: Option<&str>) -> Option<Identity> {
        None
    }

    fn trusts_everyone(&self) -> bool {
        true
    }
}

/// What the server keeps about one connection.
#[derive(Debug)]
struct Conn {
    /// The highest sequence number already sent to it.
    cursor: Seq,
    /// Who it is, once its `Hello` has been answered. `None` under
    /// [`Trusting`], where nobody is anybody in particular.
    who: Option<Identity>,
}

/// The server half of the sync engine.
pub struct Server<A: App> {
    conn: Connection,
    conns: BTreeMap<ConnId, Conn>,
    auth: Box<dyn Authenticate>,
    /// Whether `auth` is [`Trusting`]: the one authenticator that answers
    /// nobody and means it.
    trusting: bool,
    head: Seq,
    /// The realtime half, if the app has one. Behind a `dyn` so that the
    /// server stays generic over the app's *mutation* alone: a protocol
    /// nothing ever replays has no business in the types the log is written
    /// with. See [`crate::live`].
    rooms: Option<Box<dyn Rooms>>,
    /// Who is in which room. Beside `conns` rather than derived from it,
    /// because a peer the server [`stand`](Server::stand)s in for has no
    /// connection of its own.
    roster: Roster,
    out: Vec<(ConnId, ServerMsg<A::Mutation>)>,
    /// `fn() -> A` rather than `A`: the marker should not drag the app's
    /// auto traits into ours.
    _app: PhantomData<fn() -> A>,
}

impl<A: App> std::fmt::Debug for Server<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Server")
            .field("head", &self.head)
            .field("conns", &self.conns)
            .finish_non_exhaustive()
    }
}

impl<A: App> Server<A> {
    /// Open a server over an existing connection, running both Petros's migrations
    /// and the app's. [`Trusting`]: every client is who it says it is.
    pub fn open(conn: Connection) -> Result<Self> {
        Server::open_with(conn, Trusting)
    }

    /// As [`open`](Self::open), with something deciding who each connection
    /// is. From then on a `Hello` whose token proves nothing is answered with
    /// [`ServerMsg::Denied`], and a pushed entry whose `actor` is not the
    /// connection's user, or whose `session` was never that user's, is
    /// refused with a [`ServerMsg::Reject`] rather than appended.
    pub fn open_with(mut conn: Connection, auth: impl Authenticate) -> Result<Self> {
        store::migrate(&mut conn)?;
        A::migrate(&mut conn)?;

        // If the app's tables predate its current schema, rebuild them from the
        // log: drop and recreate them empty, then replay every confirmed entry
        // through today's `apply`. The server's state is a function of the log
        // just as a client's is.
        if crate::migrate::is_stale::<A>(&mut conn)? {
            crate::migrate::reset_tables::<A>(&mut conn)?;
            crate::migrate::replay_all::<A>(&mut conn)?;
        }

        let head = store::head(&mut conn)?;
        let trusting = auth.trusts_everyone();
        Ok(Server {
            conn,
            conns: BTreeMap::new(),
            auth: Box::new(auth),
            trusting,
            head,
            rooms: None,
            roster: Roster::default(),
            out: Vec::new(),
            _app: PhantomData,
        })
    }

    /// Give this server a realtime channel: state that is true *now*, carried
    /// on the same socket as the log and written to none of it.
    ///
    /// Without one, a [`ClientMsg::Say`] is a frame from a peer whose server
    /// has nothing to say it to, and is dropped. See [`crate::live`].
    pub fn with_live(mut self, live: impl Live) -> Self {
        self.rooms = Some(Box::new(Erased(live)));
        self
    }

    /// The authoritative materialised state. Read-only by convention: the log
    /// is the only way to change it. Diesel needs `&mut` even to read.
    pub fn conn(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// The highest assigned sequence number.
    pub fn head(&self) -> Seq {
        self.head
    }

    /// Handle one message from one connection.
    pub fn recv(&mut self, from: ConnId, msg: ClientMsg<A::Mutation>) -> Result<()> {
        match msg {
            // Resume and initial sync are the same path: `since` is 0 for a
            // client that has never synced and 4_211 for one resuming.
            ClientMsg::Hello { since, token } => {
                let who = if self.trusting {
                    None
                } else {
                    match self.auth.authenticate(token.as_deref()) {
                        Some(who) => Some(who),
                        None => {
                            self.deny(from, "not signed in");
                            return Ok(());
                        }
                    }
                };
                self.conns.insert(
                    from,
                    Conn {
                        cursor: since.min(self.head),
                        who,
                    },
                );
                // A `Hello` is where a connection becomes somebody, so it is
                // also where it enters a room — and a second one on the same
                // socket is a peer changing rooms, which means leaving the
                // first.
                self.enter(from)?;
            }
            ClientMsg::Push { entries } => {
                if !self.conns.contains_key(&from) {
                    if self.trusting {
                        self.conns.insert(
                            from,
                            Conn {
                                cursor: 0,
                                who: None,
                            },
                        );
                    } else {
                        // A push before a `Hello` is a push from nobody.
                        self.deny(from, "not signed in");
                        return Ok(());
                    }
                }
                self.append_all(from, entries)?;
            }
            // Not the log, so none of the log's machinery: no sequence
            // number, no dedupe, no durable record, and nothing to fan out.
            ClientMsg::Say { say } => return self.heard(from, &say),
        }
        self.fanout()
    }

    /// Put a peer in a room without a connection of its own.
    ///
    /// A speaker in the house, a bridge, a test — anything the server itself
    /// stands in for. `conn` is the id its [`ServerMsg::Heard`] frames will be
    /// addressed to; take it from wherever the transport hands ids out, so it
    /// cannot collide with a socket that arrives later.
    pub fn stand(&mut self, conn: ConnId, who: Identity) -> Result<()> {
        self.arrive(Peer::of(conn, &who))
    }

    /// Take one out again. The same thing a dropped socket does, said by the
    /// thing that was standing in for it.
    pub fn unstand(&mut self, conn: ConnId) {
        self.disconnect(conn);
    }

    /// A connection has entered whatever room its `Hello` put it in.
    fn enter(&mut self, conn: ConnId) -> Result<()> {
        let peer = match self.conns.get(&conn).and_then(|c| c.who.as_ref()) {
            Some(who) => Peer::of(conn, who),
            // A server that authenticates nobody has one room, because it has
            // one anybody.
            None => Peer::anon(conn),
        };
        self.arrive(peer)
    }

    /// Run one call into the app's machine, with the box lent out and put
    /// back. Nothing below re-enters the server, so lending it is safe and it
    /// is what keeps `rooms` a plain field rather than a second lock.
    fn with_rooms<T>(
        &mut self,
        f: impl FnOnce(&mut Self, &mut dyn Rooms) -> Result<T>,
        idle: T,
    ) -> Result<T> {
        let Some(mut rooms) = self.rooms.take() else {
            return Ok(idle);
        };
        let out = f(self, &mut *rooms);
        self.rooms = Some(rooms);
        out
    }

    fn arrive(&mut self, peer: Peer) -> Result<()> {
        self.with_rooms(move |me, rooms| me.arrive_in(rooms, peer), ())
    }

    fn arrive_in(&mut self, rooms: &mut dyn Rooms, peer: Peer) -> Result<()> {
        if let Some(was) = self.roster.remove(peer.conn) {
            self.depart_in(rooms, was)?;
        }
        // After the departure above, so that a peer which was the last one in
        // its old room has already had that room written down — and before
        // the join, so the machine wakes with yesterday's state rather than
        // being told about a device first.
        if self.roster.first_sight(&peer.room) {
            if let Some(state) = store::live(&mut self.conn, &peer.room)? {
                rooms.wake(&peer.room, &state);
            }
        }
        self.roster.insert(peer.clone());
        let peers = self.roster.room(&peer.room);
        let posted = rooms.join(&peer, &peers);
        self.posted(&peer.room, posted, rooms)
    }

    fn depart_in(&mut self, rooms: &mut dyn Rooms, peer: Peer) -> Result<()> {
        self.roster.remove(peer.conn);
        let peers = self.roster.room(&peer.room);
        let posted = rooms.part(&peer, &peers);
        self.posted(&peer.room, posted, rooms)?;
        if !self.roster.occupied(&peer.room) {
            // An empty room is written down and dropped rather than kept in
            // memory for ever — which is what bounds a server with a great
            // many accounts on it, and what makes the row on disk the one
            // answer about a room nobody is in.
            self.keep(&peer.room, rooms)?;
            rooms.close(&peer.room);
            self.roster.forget(&peer.room);
        }
        Ok(())
    }

    fn heard(&mut self, conn: ConnId, say: &[u8]) -> Result<()> {
        let Some(peer) = self.roster.get(conn).cloned() else {
            // A frame from a peer that never said `Hello`, or from one on a
            // server with no realtime half. Nothing to answer.
            return Ok(());
        };
        self.with_rooms(
            move |me, rooms| {
                let peers = me.roster.room(&peer.room);
                let posted = rooms.say(&peer, say, &peers);
                me.posted(&peer.room, posted, rooms)
            },
            (),
        )
    }

    /// Deliver what a room's machine produced, and write the room down if it
    /// asked to be.
    fn posted(&mut self, room: &Room, posted: Posted, rooms: &mut dyn Rooms) -> Result<()> {
        for (conn, hear) in posted.out {
            self.out.push((conn, ServerMsg::Heard { hear }));
        }
        if posted.keep {
            self.keep(room, rooms)?;
        }
        Ok(())
    }

    /// One row, last write wins. A machine with nothing worth keeping about a
    /// room has the row deleted rather than left there claiming otherwise.
    fn keep(&mut self, room: &Room, rooms: &mut dyn Rooms) -> Result<()> {
        match rooms.snapshot(room) {
            Some(state) => store::set_live(&mut self.conn, room, &state),
            None => store::drop_live(&mut self.conn, room),
        }
    }

    /// Turn a connection away. Whatever it says next is from nobody too.
    fn deny(&mut self, from: ConnId, reason: &str) {
        self.conns.remove(&from);
        self.out.push((
            from,
            ServerMsg::Denied {
                reason: reason.into(),
            },
        ));
    }

    /// Whether a connection may push this entry as the person it claims.
    /// Always, under [`Trusting`].
    fn permitted(
        &mut self,
        from: ConnId,
        entry: &Entry<A::Mutation>,
    ) -> std::result::Result<(), String> {
        let Some(who) = self.conns.get(&from).and_then(|c| c.who.clone()) else {
            return Ok(());
        };
        if entry.actor != who.user {
            return Err(format!(
                "authored as {} but signed in as {}",
                entry.actor, who.user
            ));
        }
        match &entry.session {
            None => Ok(()),
            Some(s) if *s == who.session || self.auth.owns(&who.user, s) => Ok(()),
            Some(s) => Err(format!("session {s} was never {}'s", who.user)),
        }
    }

    /// Forget a connection. Its cursor is not state worth keeping — the client
    /// tells us where it is when it comes back.
    ///
    /// It also leaves whatever room it was in, which is a *socket* going and
    /// not necessarily a device going: an app whose machine conflates the two
    /// is an app that loses the music when a phone sleeps.
    ///
    /// A snapshot that will not write is swallowed here and nowhere else,
    /// because there is no caller left to tell: the connection this is about
    /// has already gone. The room's state is in memory either way, and the
    /// next thing that asks to be kept writes it again.
    pub fn disconnect(&mut self, from: ConnId) {
        self.conns.remove(&from);
        if let Some(peer) = self.roster.get(from).cloned() {
            let _ = self.with_rooms(move |me, rooms| me.depart_in(rooms, peer), ());
        }
    }

    /// Drain messages the server wants to send.
    pub fn take_outgoing(&mut self) -> Vec<(ConnId, ServerMsg<A::Mutation>)> {
        std::mem::take(&mut self.out)
    }

    fn append_all(&mut self, from: ConnId, entries: Vec<Entry<A::Mutation>>) -> Result<()> {
        let mut ids = Vec::new();
        let mut seqs = Vec::new();
        for mut entry in entries {
            // Dedupe. A push whose Ack was lost gets retried verbatim; the
            // second attempt must be indistinguishable from the first, so we
            // answer with the sequence number the entry already has.
            if let Some(seq) = store::seq_of(&mut self.conn, &entry.id)? {
                ids.push(entry.id);
                seqs.push(seq);
                continue;
            }
            if let Err(reason) = self.permitted(from, &entry) {
                self.out.push((
                    from,
                    ServerMsg::Reject {
                        id: entry.id,
                        reason,
                    },
                ));
                continue;
            }
            entry.seq = Some(self.head + 1);
            match self.append_one(&entry) {
                Ok(seq) => {
                    self.head = seq;
                    ids.push(entry.id);
                    seqs.push(seq);
                }
                // A deterministic verdict: this entry will never be in the log,
                // and every replica would have reached the same conclusion.
                Err(Error::Mutation(MutationError::Rejected(reason))) => {
                    self.out.push((
                        from,
                        ServerMsg::Reject {
                            id: entry.id,
                            reason,
                        },
                    ));
                }
                Err(other) => return Err(other),
            }
        }
        if !ids.is_empty() {
            self.out.push((from, ServerMsg::Ack { ids, seqs }));
        }
        Ok(())
    }

    /// Apply and append atomically: the log row and the state it produced land
    /// together or not at all.
    ///
    /// The transaction is opened with raw SQL rather than Diesel's
    /// `transaction()` so that the server and the client drive their
    /// boundaries the same way — the client cannot use Diesel's transaction
    /// manager at all, because its savepoint outlives any single call.
    fn append_one(&mut self, entry: &Entry<A::Mutation>) -> Result<Seq> {
        let seq = entry.require_seq()?;
        self.conn.batch_execute("BEGIN")?;
        match self.append_within(entry) {
            Ok(()) => {
                self.conn.batch_execute("COMMIT")?;
                Ok(seq)
            }
            Err(e) => {
                self.conn.batch_execute("ROLLBACK")?;
                Err(e)
            }
        }
    }

    fn append_within(&mut self, entry: &Entry<A::Mutation>) -> Result<()> {
        entry
            .mutation
            .apply(&mut Transaction::new(&mut self.conn), &entry.ctx())?;
        store::put_confirmed(&mut self.conn, entry)
    }

    /// Send every connection everything it has not been sent yet. One rule,
    /// applied after every message, covers initial sync, resume and live
    /// broadcast alike.
    fn fanout(&mut self) -> Result<()> {
        let stale: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(_, c)| c.cursor < self.head)
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            let cursor = self.conns.get(&id).map(|c| c.cursor).unwrap_or(0);
            let entries: Vec<Entry<A::Mutation>> =
                store::entries_after(&mut self.conn, cursor, BATCH_LIMIT)?;
            let Some(last) = entries.last() else { continue };
            let sent_to = last.require_seq()?;
            if let Some(c) = self.conns.get_mut(&id) {
                c.cursor = sent_to;
            }
            self.out.push((
                id,
                ServerMsg::Batch {
                    entries,
                    has_more: sent_to < self.head,
                },
            ));
        }
        Ok(())
    }
}
