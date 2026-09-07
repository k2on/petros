//! The server state machine. Sans-io: feed it messages, drain its outbox.
//!
//! The server owns the one true order. It assigns sequence numbers, never
//! reorders, never rewrites. Everything a client does is provisional until it
//! appears here.

use std::collections::BTreeMap;
use std::marker::PhantomData;

use rusqlite::Connection;

use crate::{
    store, App, ClientMsg, Entry, Error, Mutation, MutationError, Result, Seq, ServerMsg,
    Transaction,
};

/// How many entries one [`ServerMsg::Batch`] carries. A client that sees
/// `has_more` sends another `Hello`.
const BATCH_LIMIT: usize = 256;

/// Identifies one connected client. Assigned by the transport, meaningless to
/// Exo beyond "messages tagged with this go back down the same pipe".
pub type ConnId = u64;

/// The server half of the sync engine.
pub struct Server<A: App> {
    conn: Connection,
    /// For each connection, the highest sequence number already sent to it.
    conns: BTreeMap<ConnId, Seq>,
    head: Seq,
    out: Vec<(ConnId, ServerMsg<A::Mutation>)>,
    _app: PhantomData<A>,
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
    /// Open a server over an existing connection, running both Exo's migrations
    /// and the app's.
    pub fn open(conn: Connection) -> Result<Self> {
        store::migrate(&conn)?;
        A::migrate(&conn)?;
        let head = store::head(&conn)?;
        Ok(Server {
            conn,
            conns: BTreeMap::new(),
            head,
            out: Vec::new(),
            _app: PhantomData,
        })
    }

    /// The authoritative materialised state. Read-only by convention: the log
    /// is the only way to change it.
    pub fn conn(&self) -> &Connection {
        &self.conn
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
            ClientMsg::Hello { since } => {
                self.conns.insert(from, since.min(self.head));
            }
            ClientMsg::Push { entries } => {
                self.conns.entry(from).or_insert(0);
                self.append_all(from, entries)?;
            }
        }
        self.fanout()
    }

    /// Forget a connection. Its cursor is not state worth keeping — the client
    /// tells us where it is when it comes back.
    pub fn disconnect(&mut self, from: ConnId) {
        self.conns.remove(&from);
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
            if let Some(seq) = store::seq_of(&self.conn, &entry.id)? {
                ids.push(entry.id);
                seqs.push(seq);
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
    fn append_one(&mut self, entry: &Entry<A::Mutation>) -> Result<Seq> {
        let seq = entry.require_seq()?;
        let tx = self.conn.transaction()?;
        entry.mutation.apply(&Transaction::new(&tx), &entry.actor)?;
        store::put_confirmed(&tx, entry)?;
        tx.commit()?;
        Ok(seq)
    }

    /// Send every connection everything it has not been sent yet. One rule,
    /// applied after every message, covers initial sync, resume and live
    /// broadcast alike.
    fn fanout(&mut self) -> Result<()> {
        let stale: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(_, cursor)| **cursor < self.head)
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            let cursor = self.conns.get(&id).copied().unwrap_or(0);
            let entries: Vec<Entry<A::Mutation>> =
                store::entries_after(&self.conn, cursor, BATCH_LIMIT)?;
            let Some(last) = entries.last() else { continue };
            let sent_to = last.require_seq()?;
            self.conns.insert(id, sent_to);
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
