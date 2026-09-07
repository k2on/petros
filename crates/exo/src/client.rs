//! The client state machine. Sans-io: feed it messages, drain its outbox.

use std::marker::PhantomData;

use rusqlite::Connection;

use crate::{
    store, ActorId, App, AutoCtx, ClientMsg, Entry, Error, Mutation, MutationError, Result, Seq,
    ServerMsg, Transaction, Uuid,
};

/// A mutation that will never be in the log, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub id: Uuid,
    pub reason: String,
}

/// The client half of the sync engine.
///
/// The view it presents is always
///
/// ```text
/// replay(confirmed) then replay(pending)
/// ```
///
/// Confirmed entries only ever move forward, because the server's log is
/// append-only and never reordered. The only thing ever undone is this client's
/// own pending mutations, and they are replayed on top afterwards.
pub struct Client<A: App> {
    conn: Connection,
    actor: ActorId,
    auto: AutoCtx,
    /// Highest confirmed sequence number applied. Contiguous from 1 by
    /// construction: a gap stops us until the missing entry arrives.
    cursor: Seq,
    /// Whether the optimistic savepoint is currently held. It is held exactly
    /// when there is something pending.
    savepoint_open: bool,
    out: Vec<ClientMsg<A::Mutation>>,
    rejections: Vec<Rejection>,
    /// `fn() -> A` rather than `A`: the marker should not drag the app's
    /// auto traits into ours.
    _app: PhantomData<fn() -> A>,
}

impl<A: App> std::fmt::Debug for Client<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("actor", &self.actor)
            .field("cursor", &self.cursor)
            .finish_non_exhaustive()
    }
}

impl<A: App> Client<A> {
    /// Open a client over an existing connection.
    pub fn open(conn: Connection, actor: impl Into<ActorId>, auto: AutoCtx) -> Result<Self> {
        store::migrate(&conn)?;
        A::migrate(&conn)?;
        let cursor = store::cursor(&conn)?;
        let mut client = Client {
            conn,
            actor: actor.into(),
            auto,
            cursor,
            savepoint_open: false,
            out: Vec::new(),
            rejections: Vec::new(),
            _app: PhantomData,
        };
        // Whatever optimistic state a previous session left behind died with its
        // uncommitted transaction. Finish anything the log is ahead on, then
        // rebuild the view from the pending mutations that did survive.
        client.advance()?;
        client.open_optimistic()?;
        Ok(client)
    }

    /// The materialised view. Read-only by convention: mutations are the only
    /// supported way to change it.
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    pub fn actor(&self) -> &ActorId {
        &self.actor
    }

    /// Highest confirmed sequence number this client has applied.
    pub fn cursor(&self) -> Seq {
        self.cursor
    }

    /// How many of this client's own mutations are still unconfirmed.
    pub fn pending_len(&self) -> usize {
        store::pending_len(&self.conn).unwrap_or(0)
    }

    /// Author a mutation: fill its non-deterministic arguments, apply it
    /// optimistically, and queue it for the server.
    pub fn mutate(&mut self, mut mutation: A::Mutation) -> Result<Uuid> {
        // Exactly once, here at the origin. From now on these arguments are
        // frozen: no replay of this entry will ever regenerate them.
        mutation.fill_auto(&mut self.auto);
        let entry = Entry::new(self.auto.uuid(), self.actor.clone(), mutation);

        // Try it against the view the caller is actually looking at. An intent
        // that is invalid here is invalid everywhere, so it should never reach
        // the log, the pending queue, or the outbox.
        self.probe(&entry)?;

        // The optimistic view is scratch, but the intent is not: an offline
        // client that loses a week of edits to a crash is not offline-first. We
        // cannot commit anything while holding the savepoint open, so drop the
        // savepoint, commit the intent on its own, then rebuild the view.
        self.discard_optimistic()?;
        store::put_pending(&self.conn, &entry)?;
        self.open_optimistic()?;

        let id = entry.id;
        self.out.push(ClientMsg::Push {
            entries: vec![entry],
        });
        Ok(id)
    }

    /// Announce a fresh connection: ask for everything since our cursor and
    /// re-offer everything still pending. Both are safe to repeat — the server
    /// dedupes pushes on entry id.
    pub fn connected(&mut self) -> Result<()> {
        self.out.push(ClientMsg::Hello { since: self.cursor });
        let pending: Vec<Entry<A::Mutation>> = store::pending(&self.conn)?;
        if !pending.is_empty() {
            self.out.push(ClientMsg::Push { entries: pending });
        }
        Ok(())
    }

    /// Handle one message from the server.
    pub fn recv(&mut self, msg: ServerMsg<A::Mutation>) -> Result<()> {
        // Undo our optimistic view before touching anything durable: the
        // bookkeeping below has to be committed, and it cannot be committed
        // underneath a savepoint we still intend to roll back.
        self.discard_optimistic()?;
        match msg {
            ServerMsg::Batch { entries, has_more } => {
                for entry in &entries {
                    store::put_confirmed(&self.conn, entry)?;
                    // Our own entry coming back confirmed: it is no longer ours
                    // to replay.
                    store::drop_pending(&self.conn, &entry.id)?;
                }
                if has_more {
                    self.out.push(ClientMsg::Hello {
                        since: self.next_cursor()?,
                    });
                }
            }
            ServerMsg::Ack { ids, seqs } => {
                if ids.len() != seqs.len() {
                    return Err(Error::Protocol("Ack ids and seqs differ in length".into()));
                }
                let pending: Vec<Entry<A::Mutation>> = store::pending(&self.conn)?;
                for (id, seq) in ids.iter().zip(seqs) {
                    // The ack tells us where in the order our entry landed, so
                    // we can promote it from pending to confirmed without
                    // waiting for it to come back around in a Batch.
                    if let Some(entry) = pending.iter().find(|e| &e.id == id) {
                        let confirmed = Entry {
                            seq: Some(seq),
                            ..Entry::new(entry.id, entry.actor.clone(), &entry.mutation)
                        };
                        store::put_confirmed(&self.conn, &confirmed)?;
                    }
                    store::drop_pending(&self.conn, id)?;
                }
            }
            ServerMsg::Reject { id, reason } => {
                store::drop_pending(&self.conn, &id)?;
                self.rejections.push(Rejection { id, reason });
            }
        }
        self.advance()?;
        // ...and replay whatever is still ours on top of the new confirmed
        // state. If nothing is pending this opens nothing: steady state holds
        // no transaction at all.
        self.open_optimistic()
    }

    /// Drain messages the client wants to send.
    pub fn take_outgoing(&mut self) -> Vec<ClientMsg<A::Mutation>> {
        std::mem::take(&mut self.out)
    }

    /// Drain mutations the server refused.
    pub fn take_rejections(&mut self) -> Vec<Rejection> {
        std::mem::take(&mut self.rejections)
    }

    /// Move the cursor over every contiguous confirmed entry we now hold,
    /// applying each one. The cursor and the state it describes are committed
    /// together, so a crash mid-batch simply replays from the old cursor.
    fn advance(&mut self) -> Result<()> {
        let next = self.next_cursor()?;
        if next == self.cursor {
            return Ok(());
        }
        let count = (next - self.cursor) as usize;
        let entries: Vec<Entry<A::Mutation>> =
            store::entries_after(&self.conn, self.cursor, count)?;
        let tx = self.conn.transaction()?;
        for entry in &entries {
            // A confirmed entry that will not apply means this client and the
            // server disagree about what the same arguments mean — a
            // determinism bug. Fail loudly rather than diverge quietly.
            entry.mutation.apply(&Transaction::new(&tx), &entry.actor)?;
        }
        store::set_cursor(&tx, next)?;
        tx.commit()?;
        self.cursor = next;
        Ok(())
    }

    /// The end of the contiguous run of confirmed entries starting at
    /// `cursor + 1`. A gap means an entry is still in flight, and everything
    /// after it has to wait: applying out of order would not be the log.
    fn next_cursor(&self) -> Result<Seq> {
        let next: Option<i64> = self.conn.query_row(
            "SELECT MIN(l.seq) FROM exo_log l
             WHERE l.seq > ?1
               AND NOT EXISTS (SELECT 1 FROM exo_log n WHERE n.seq = l.seq + 1)
               AND EXISTS (SELECT 1 FROM exo_log s WHERE s.seq = ?1 + 1)",
            [self.cursor as i64],
            |r| r.get(0),
        )?;
        Ok(next.map(|s| s as Seq).unwrap_or(self.cursor))
    }

    /// Apply a mutation and immediately undo it, to find out whether it would
    /// be accepted. `SAVEPOINT` outside a transaction starts one, and
    /// `RELEASE` on the outermost savepoint commits it, so this works the same
    /// whether or not the optimistic savepoint is currently held.
    fn probe(&self, entry: &Entry<A::Mutation>) -> Result<()> {
        self.conn.execute_batch("SAVEPOINT probe")?;
        let verdict = entry
            .mutation
            .apply(&Transaction::new(&self.conn), &entry.actor);
        self.conn
            .execute_batch("ROLLBACK TO probe; RELEASE probe;")?;
        Ok(verdict?)
    }

    /// Throw the optimistic view away. Afterwards the database holds confirmed
    /// state only and the connection is back in autocommit, so anything written
    /// next is durable.
    fn discard_optimistic(&mut self) -> Result<()> {
        if self.savepoint_open {
            // ROLLBACK TO undoes everything since the savepoint but leaves the
            // savepoint (and the transaction) alive; RELEASE discards it;
            // COMMIT closes the transaction that has nothing left in it.
            self.conn
                .execute_batch("ROLLBACK TO pending; RELEASE pending; COMMIT;")?;
            self.savepoint_open = false;
        }
        Ok(())
    }

    /// Rebuild the optimistic view: open a transaction, mark it with a
    /// savepoint, and replay every pending mutation into it. With nothing
    /// pending this does nothing at all — steady state holds no transaction.
    fn open_optimistic(&mut self) -> Result<()> {
        loop {
            let pending: Vec<Entry<A::Mutation>> = store::pending(&self.conn)?;
            if pending.is_empty() {
                return Ok(());
            }
            self.conn.execute_batch("BEGIN; SAVEPOINT pending;")?;
            self.savepoint_open = true;

            match replay(&self.conn, &pending) {
                Ok(rejected) if rejected.is_empty() => return Ok(()),
                // A pending mutation can be invalidated by confirmed entries
                // that landed underneath it. The server would reject it for the
                // same reason, so drop it now — and start the replay over,
                // because the mutations after it were applied on top of state
                // it produced.
                Ok(rejected) => {
                    self.discard_optimistic()?;
                    for r in &rejected {
                        store::drop_pending(&self.conn, &r.id)?;
                    }
                    self.rejections.extend(rejected);
                }
                Err(e) => {
                    self.discard_optimistic()?;
                    return Err(e);
                }
            }
        }
    }
}

/// Apply each pending mutation in order, collecting the ones that no longer
/// hold. A database failure aborts; a rejection does not.
fn replay<M: Mutation>(conn: &Connection, pending: &[Entry<M>]) -> Result<Vec<Rejection>> {
    let mut rejected = Vec::new();
    for entry in pending {
        match entry.mutation.apply(&Transaction::new(conn), &entry.actor) {
            Ok(()) => {}
            Err(MutationError::Rejected(reason)) => rejected.push(Rejection {
                id: entry.id,
                reason,
            }),
            Err(MutationError::Sqlite(e)) => return Err(e.into()),
        }
    }
    Ok(rejected)
}
