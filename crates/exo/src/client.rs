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
    out: Vec<ClientMsg<A::Mutation>>,
    rejections: Vec<Rejection>,
    _app: PhantomData<A>,
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
            out: Vec::new(),
            rejections: Vec::new(),
            _app: PhantomData,
        };
        client.rebuild()?;
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
        mutation.fill_auto(&mut self.auto);
        let entry = Entry::new(self.auto.uuid(), self.actor.clone(), mutation);
        store::put_pending(&self.conn, &entry)?;
        if let Err(e) = self.rebuild() {
            store::drop_pending(&self.conn, &entry.id)?;
            self.rebuild()?;
            return Err(e);
        }
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
        self.out.push(ClientMsg::Hello {
            since: self.cursor,
        });
        let pending: Vec<Entry<A::Mutation>> = store::pending(&self.conn)?;
        if !pending.is_empty() {
            self.out.push(ClientMsg::Push { entries: pending });
        }
        Ok(())
    }

    /// Handle one message from the server.
    pub fn recv(&mut self, msg: ServerMsg<A::Mutation>) -> Result<()> {
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
        self.advance()
    }

    /// Drain messages the client wants to send.
    pub fn take_outgoing(&mut self) -> Vec<ClientMsg<A::Mutation>> {
        std::mem::take(&mut self.out)
    }

    /// Drain mutations the server refused.
    pub fn take_rejections(&mut self) -> Vec<Rejection> {
        std::mem::take(&mut self.rejections)
    }

    /// Move the cursor over every contiguous confirmed entry we now hold, then
    /// rebuild the view.
    fn advance(&mut self) -> Result<()> {
        let next = self.next_cursor()?;
        if next != self.cursor {
            self.cursor = next;
            store::set_cursor(&self.conn, next)?;
        }
        self.rebuild()
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

    /// Naive materialisation: throw the app's tables away and replay the whole
    /// log, then the pending mutations, on top.
    fn rebuild(&mut self) -> Result<()> {
        let tx = self.conn.transaction()?;
        // DROP TABLE fires the foreign key actions of a full DELETE; deferring
        // them lets us drop a whole schema in any order inside one transaction.
        tx.execute_batch("PRAGMA defer_foreign_keys = ON")?;
        drop_app_objects(&tx)?;
        A::migrate(&tx)?;
        let confirmed: Vec<Entry<A::Mutation>> = store::entries_after(&tx, 0, usize::MAX)?;
        for entry in &confirmed {
            if entry.require_seq()? > self.cursor {
                break;
            }
            entry.mutation.apply(&Transaction::new(&tx), &entry.actor)?;
        }
        let mut rejected = Vec::new();
        for entry in store::pending::<A::Mutation>(&tx)? {
            match entry.mutation.apply(&Transaction::new(&tx), &entry.actor) {
                Ok(()) => {}
                // A pending mutation can become invalid once confirmed entries
                // land under it. The server would reject it for the same
                // reason, so drop it now rather than push something doomed.
                Err(MutationError::Rejected(reason)) => {
                    rejected.push(Rejection {
                        id: entry.id,
                        reason,
                    });
                }
                Err(MutationError::Sqlite(e)) => return Err(e.into()),
            }
        }
        for r in &rejected {
            store::drop_pending(&tx, &r.id)?;
        }
        tx.commit()?;
        if !rejected.is_empty() {
            self.rejections.extend(rejected);
            return self.rebuild();
        }
        Ok(())
    }
}

/// Everything in the schema that Exo does not own. `sqlite_master` rows with a
/// NULL `sql` are indexes SQLite created for us and cannot be dropped directly.
fn drop_app_objects(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT type, name FROM sqlite_master
         WHERE sql IS NOT NULL
           AND name NOT LIKE 'exo!_%' ESCAPE '!'
           AND name NOT LIKE 'sqlite!_%' ESCAPE '!'
         ORDER BY CASE type
             WHEN 'trigger' THEN 0 WHEN 'view' THEN 1 WHEN 'index' THEN 2 ELSE 3 END, name",
    )?;
    let objects: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    for (kind, name) in objects {
        conn.execute_batch(&format!("DROP {kind} IF EXISTS \"{name}\""))?;
    }
    Ok(())
}
