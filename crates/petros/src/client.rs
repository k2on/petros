//! The client state machine. Sans-io: feed it messages, drain its outbox.

use std::marker::PhantomData;

use diesel::connection::SimpleConnection;

use crate::{
    store, ActorId, App, AutoCtx, ClientMsg, Connection, Entry, Error, Id, Mutation, MutationError,
    Result, Seq, ServerMsg, Transaction,
};

/// How many confirmed entries are applied in one transaction. Bounds memory on
/// a client that has been away for a long time.
const APPLY_CHUNK: usize = 256;

/// A mutation that will never be in the log, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub id: Id,
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
///
/// # Transactions belong to Petros
///
/// Never call Diesel's `Connection::transaction` on a client's connection. The
/// optimistic savepoint outlives any single call, so Petros drives every boundary
/// itself with raw SQL and Diesel's transaction manager is deliberately left
/// out of it.
pub struct Client<A: App> {
    conn: Connection,
    actor: ActorId,
    auto: AutoCtx,
    /// Highest confirmed sequence number applied. Contiguous from 1 by
    /// construction: a gap stops us until the missing entry arrives.
    cursor: Seq,
    /// Whether the optimistic savepoint is currently held. It is held exactly
    /// when there is something pending.
    /// Pending intents, in their own file. Committed independently of the
    /// optimistic transaction on `conn`, which is what keeps a tap flat.
    intents: Connection,
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
    pub fn open(mut conn: Connection, actor: impl Into<ActorId>, auto: AutoCtx) -> Result<Self> {
        store::migrate(&mut conn)?;
        A::migrate(&mut conn)?;

        // Intents live in their own file so they can be committed while the
        // optimistic transaction on this one stays open. See `INTENTS_DDL`.
        let mut intents = match store::main_file(&mut conn)? {
            // `open_path`, not `open_named`: WAL and `synchronous = NORMAL`,
            // exactly as the state database gets them. Without WAL an intent
            // commit is a rollback-journal fsync, which measured at 12ms a tap
            // — flat, but flat and slow.
            Some(file) => crate::open_path(format!("{file}-intents"))?,
            // `:memory:` — a test, or a browser. Nothing here was durable to
            // begin with, so the intents are not either.
            None => crate::open_memory()?,
        };
        store::migrate_intents(&mut intents)?;

        let cursor = store::cursor(&mut conn)?;
        let mut client = Client {
            conn,
            intents,
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
        // An intent the server confirmed before we crashed is in the log now and
        // is not pending any more. Two files means two commits, and a crash can
        // land between them — this is the one place that shows, and dropping the
        // duplicate is cheaper than requiring every mutation to be idempotent.
        client.forget_confirmed_intents()?;
        client.open_optimistic()?;
        Ok(client)
    }

    /// The materialised view. Read-only by convention: mutations are the only
    /// supported way to change it. Diesel needs `&mut` even to read.
    pub fn conn(&mut self) -> &mut Connection {
        &mut self.conn
    }

    /// The connection as a store, which is what a query takes.
    ///
    /// A query is generic over `Store` so it can run inside the sandbox as well
    /// as here; this saves every call site wrapping the connection itself.
    pub fn store(&mut self) -> crate::backend::SqliteStore<'_> {
        crate::backend::SqliteStore::new(&mut self.conn)
    }

    pub fn actor(&self) -> &ActorId {
        &self.actor
    }

    /// Highest confirmed sequence number this client has applied.
    pub fn cursor(&self) -> Seq {
        self.cursor
    }

    /// How many of this client's own mutations are still unconfirmed.
    pub fn pending_len(&mut self) -> usize {
        store::pending_len(&mut self.intents).unwrap_or(0)
    }

    /// Author a mutation: fill its non-deterministic arguments, apply it
    /// optimistically, and queue it for the server.
    /// Takes anything that becomes the app's mutation, so an authoring
    /// function can hand back the payload value and the newtype is applied
    /// here rather than at every call site.
    pub fn mutate(&mut self, mutation: impl Into<A::Mutation>) -> Result<Id> {
        let mut mutation = mutation.into();
        // Exactly once, here at the origin. From now on these arguments are
        // frozen: no replay of this entry will ever regenerate them.
        mutation.fill_auto(&mut self.auto);
        let entry = Entry::new(self.auto.uuid(), self.actor.clone(), mutation);

        // Try it against the view the caller is actually looking at, and record
        // the intent — both inside one transaction, so a tap costs one commit.
        //
        // It used to cost two. The probe committed on its own (a `RELEASE` of
        // the outermost savepoint is a commit) or the optimistic savepoint did,
        // and then the insert committed again as its own implicit transaction.
        // At `synchronous = FULL` that is two fsyncs, which measured as the
        // entire cost of a tap: 8.0ms of 8.2ms, against 4.0ms for one fsync on
        // the same machine.
        self.apply_and_record(&entry)?;

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
        let pending: Vec<Entry<A::Mutation>> = store::pending(&mut self.intents)?;
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
                    store::put_confirmed(&mut self.conn, entry)?;
                    // Our own entry coming back confirmed: it is no longer ours
                    // to replay.
                    store::drop_pending(&mut self.intents, &entry.id)?;
                }
                if has_more {
                    let since = store::contiguous_after(&mut self.conn, self.cursor, APPLY_CHUNK)?;
                    self.out.push(ClientMsg::Hello { since });
                }
            }
            ServerMsg::Ack { ids, seqs } => {
                if ids.len() != seqs.len() {
                    return Err(Error::Protocol("Ack ids and seqs differ in length".into()));
                }
                let pending: Vec<Entry<A::Mutation>> = store::pending(&mut self.intents)?;
                for (id, seq) in ids.iter().zip(seqs) {
                    // The ack tells us where in the order our entry landed, so
                    // we can promote it from pending to confirmed without
                    // waiting for it to come back around in a Batch.
                    if let Some(entry) = pending.iter().find(|e| &e.id == id) {
                        let confirmed = Entry {
                            seq: Some(seq),
                            ..Entry::new(entry.id, entry.actor.clone(), &entry.mutation)
                        };
                        store::put_confirmed(&mut self.conn, &confirmed)?;
                    }
                    store::drop_pending(&mut self.intents, id)?;
                }
            }
            ServerMsg::Reject { id, reason } => {
                store::drop_pending(&mut self.intents, &id)?;
                self.rejections.push(Rejection { id, reason });
            }
        }
        self.advance()?;
        // ...and replay whatever is still ours on top of the new confirmed
        // state. If nothing is pending this opens nothing: steady state holds
        // no transaction at all.
        self.open_optimistic()
    }

    /// Drain messages the client wants to send. A caller with no connection
    /// should drain and discard rather than let the queue grow: reconnecting
    /// with [`connected`](Self::connected) re-offers everything still pending,
    /// and the server dedupes what it has already seen.
    pub fn take_outgoing(&mut self) -> Vec<ClientMsg<A::Mutation>> {
        std::mem::take(&mut self.out)
    }

    /// Drain mutations the server refused.
    pub fn take_rejections(&mut self) -> Vec<Rejection> {
        std::mem::take(&mut self.rejections)
    }

    /// Apply every contiguous confirmed entry we now hold, a chunk at a time.
    /// The cursor and the state it describes are committed together, so a crash
    /// mid-chunk simply replays from the old cursor.
    fn advance(&mut self) -> Result<()> {
        loop {
            let next = store::contiguous_after(&mut self.conn, self.cursor, APPLY_CHUNK)?;
            if next == self.cursor {
                return Ok(());
            }
            let count = (next - self.cursor) as usize;
            let entries: Vec<Entry<A::Mutation>> =
                store::entries_after(&mut self.conn, self.cursor, count)?;
            self.conn.batch_execute("BEGIN")?;
            match apply_confirmed(&mut self.conn, &entries, next) {
                Ok(()) => {
                    self.conn.batch_execute("COMMIT")?;
                    self.cursor = next;
                }
                Err(e) => {
                    self.conn.batch_execute("ROLLBACK")?;
                    return Err(e);
                }
            }
        }
    }

    /// Apply a mutation to find out whether it would be accepted, undo it, and
    /// — if it was — record the intent durably. One transaction, one commit,
    /// one fsync.
    ///
    /// The probe has to run against the *optimistic* view, because an intent
    /// that is invalid against what the caller is looking at is invalid
    /// everywhere and should never reach the log, the queue or the outbox. So
    /// `probe` nests inside the pending savepoint when one is held, and opens
    /// a transaction of its own when one is not.
    ///
    /// The optimistic view is scratch, but the intent is not: an offline client
    /// that loses a week of edits to a crash is not offline-first. Rolling back
    /// to `pending` leaves the transaction holding nothing but the insert, so
    /// the commit that follows carries exactly the durable part.
    /// Apply the mutation on top of the view, then record the intent.
    ///
    /// Both used to be one transaction, and recording had to commit — so the
    /// optimistic savepoint was rolled back first and every pending mutation
    /// replayed afterwards to rebuild the view. That is what made a tap cost
    /// O(pending).
    ///
    /// The intent goes to its own database now, so committing it touches
    /// nothing here. The savepoint stays open across mutations and this applies
    /// one more thing to it.
    fn apply_and_record(&mut self, entry: &Entry<A::Mutation>) -> Result<()> {
        if !self.savepoint_open {
            // Rebuild the view first if a `recv` closed it. Usually a no-op:
            // between mutations the savepoint simply stays open, which is the
            // whole point.
            self.open_optimistic()?;
        }
        if !self.savepoint_open {
            // Nothing was pending, so nothing opened a transaction. Open one for
            // this mutation to live in.
            self.conn.batch_execute("BEGIN; SAVEPOINT pending;")?;
            self.savepoint_open = true;
        }

        // Its own savepoint, so a refusal undoes this mutation and leaves every
        // earlier pending one where it was.
        self.conn.batch_execute("SAVEPOINT one")?;
        match entry
            .mutation
            .apply(&mut Transaction::new(&mut self.conn), &entry.actor)
        {
            Ok(()) => self.conn.batch_execute("RELEASE one")?,
            Err(rejected) => {
                self.conn.batch_execute("ROLLBACK TO one; RELEASE one;")?;
                return Err(rejected.into());
            }
        }

        // A different connection and a different file: this commits on its own
        // and the transaction above knows nothing about it.
        store::put_pending(&mut self.intents, entry)
    }

    /// Drop intents the log already has. See the call in `open`.
    fn forget_confirmed_intents(&mut self) -> Result<()> {
        let pending: Vec<Entry<A::Mutation>> = store::pending(&mut self.intents)?;
        for entry in &pending {
            if store::seq_of(&mut self.conn, &entry.id)?.is_some() {
                store::drop_pending(&mut self.intents, &entry.id)?;
            }
        }
        Ok(())
    }

    /// Throw the optimistic view away. Afterwards the database holds confirmed
    /// state only and no transaction is open, so anything written next is
    /// durable.
    fn discard_optimistic(&mut self) -> Result<()> {
        if self.savepoint_open {
            // ROLLBACK TO undoes everything since the savepoint but leaves the
            // savepoint (and the transaction) alive; RELEASE discards it;
            // COMMIT closes the transaction that has nothing left in it.
            self.conn
                .batch_execute("ROLLBACK TO pending; RELEASE pending; COMMIT;")?;
            self.savepoint_open = false;
        }
        Ok(())
    }

    /// Rebuild the optimistic view: open a transaction, mark it with a
    /// savepoint, and replay every pending mutation into it. With nothing
    /// pending this does nothing at all — steady state holds no transaction.
    fn open_optimistic(&mut self) -> Result<()> {
        loop {
            let pending: Vec<Entry<A::Mutation>> = store::pending(&mut self.intents)?;
            if pending.is_empty() {
                return Ok(());
            }
            self.conn.batch_execute("BEGIN; SAVEPOINT pending;")?;
            self.savepoint_open = true;

            match replay(&mut self.conn, &pending) {
                Ok(rejected) if rejected.is_empty() => return Ok(()),
                // A pending mutation can be invalidated by confirmed entries
                // that landed underneath it. The server would reject it for the
                // same reason, so drop it now — and start the replay over,
                // because the mutations after it were applied on top of state
                // it produced.
                Ok(rejected) => {
                    self.discard_optimistic()?;
                    for r in &rejected {
                        store::drop_pending(&mut self.intents, &r.id)?;
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

/// Apply confirmed entries and move the cursor, inside a transaction the caller
/// opened.
fn apply_confirmed<M: Mutation>(
    conn: &mut Connection,
    entries: &[Entry<M>],
    next: Seq,
) -> Result<()> {
    for entry in entries {
        // A confirmed entry that will not apply means this client and the
        // server disagree about what the same arguments mean — a determinism
        // bug. Fail loudly rather than diverge quietly.
        entry
            .mutation
            .apply(&mut Transaction::new(conn), &entry.actor)?;
    }
    store::set_cursor(conn, next)
}

/// Apply each pending mutation in order, collecting the ones that no longer
/// hold. A database failure aborts; a rejection does not.
fn replay<M: Mutation>(conn: &mut Connection, pending: &[Entry<M>]) -> Result<Vec<Rejection>> {
    let mut rejected = Vec::new();
    for entry in pending {
        match entry
            .mutation
            .apply(&mut Transaction::new(conn), &entry.actor)
        {
            Ok(()) => {}
            Err(MutationError::Rejected(reason)) => rejected.push(Rejection {
                id: entry.id,
                reason,
            }),
            Err(MutationError::Database(e)) => return Err(e.into()),
        }
    }
    Ok(rejected)
}
