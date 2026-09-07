//! A shared to-do list, expressed as intents rather than facts.

use exo::rusqlite::OptionalExtension;
use exo::{App, AutoCtx, Connection, Mutation, MutationError, Transaction, Uuid};
use serde::{Deserialize, Serialize};

/// The app's mutation enum. `tag = "t"` because the log is permanent: an
/// internally tagged representation lets us add fields to a variant without
/// invalidating bytes already written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum TodoMutation {
    Add {
        id: Uuid,
        text: String,
        created_ms: i64,
    },
    SetDone {
        id: Uuid,
        done: bool,
    },
    Rename {
        id: Uuid,
        text: String,
    },
    Remove {
        id: Uuid,
    },
    /// Claim an item for the acting actor. Rejects if someone else got there
    /// first — a verdict two replicas can disagree about until the log settles
    /// it, which is exactly what makes it worth testing.
    Claim {
        id: Uuid,
    },
}

impl TodoMutation {
    /// `id` and `created_ms` are placeholders; `fill_auto` replaces them at the
    /// originating client and they are frozen from then on.
    pub fn add(text: &str) -> Self {
        TodoMutation::Add {
            id: Uuid::nil(),
            text: text.to_string(),
            created_ms: 0,
        }
    }

    pub fn set_done(id: Uuid, done: bool) -> Self {
        TodoMutation::SetDone { id, done }
    }

    pub fn rename(id: Uuid, text: &str) -> Self {
        TodoMutation::Rename {
            id,
            text: text.to_string(),
        }
    }

    pub fn remove(id: Uuid) -> Self {
        TodoMutation::Remove { id }
    }

    pub fn claim(id: Uuid) -> Self {
        TodoMutation::Claim { id }
    }
}

impl Mutation for TodoMutation {
    fn fill_auto(&mut self, ctx: &mut AutoCtx) {
        if let TodoMutation::Add { id, created_ms, .. } = self {
            *id = ctx.uuid();
            *created_ms = ctx.now_ms();
        }
    }

    fn apply(&self, tx: &Transaction, actor: &exo::ActorId) -> Result<(), MutationError> {
        match self {
            TodoMutation::Add {
                id,
                text,
                created_ms,
            } => {
                // A deterministic rejection: every client and the server reach
                // the same verdict from the same arguments.
                if text.is_empty() {
                    return Err(MutationError::rejected("a to-do needs some text"));
                }
                // `pos` is read out of current state rather than carried in the
                // mutation: this is an intent ("put it at the end"), not a
                // fact ("put it at 3"). It is also what makes a rebase
                // visible — an entry that lands ahead of ours pushes us down.
                tx.execute(
                    "INSERT OR IGNORE INTO todo (id, text, done, pos, created_ms, actor)
                     SELECT ?1, ?2, 0, COALESCE(MAX(pos), 0) + 1, ?3, ?4 FROM todo",
                    rusqlite::params![id, text, created_ms, actor.as_str()],
                )?;
            }
            // Updates to a row that is gone are no-ops rather than errors: the
            // row may have been removed by a mutation earlier in the log.
            TodoMutation::SetDone { id, done } => {
                tx.execute(
                    "UPDATE todo SET done = ?2 WHERE id = ?1",
                    rusqlite::params![id, done],
                )?;
            }
            TodoMutation::Rename { id, text } => {
                tx.execute(
                    "UPDATE todo SET text = ?2 WHERE id = ?1",
                    rusqlite::params![id, text],
                )?;
            }
            TodoMutation::Remove { id } => {
                tx.execute("DELETE FROM todo WHERE id = ?1", rusqlite::params![id])?;
            }
            TodoMutation::Claim { id } => {
                let held: Option<Option<String>> = tx
                    .query_row(
                        "SELECT claimed_by FROM todo WHERE id = ?1",
                        rusqlite::params![id],
                        |r| r.get(0),
                    )
                    .optional()?;
                match held {
                    // The row is gone; nothing to claim.
                    None => {}
                    Some(Some(who)) if who != actor.as_str() => {
                        return Err(MutationError::rejected(format!("already claimed by {who}")))
                    }
                    Some(_) => {
                        tx.execute(
                            "UPDATE todo SET claimed_by = ?2 WHERE id = ?1",
                            rusqlite::params![id, actor.as_str()],
                        )?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// The app. Owns the `todo` table; Exo owns everything prefixed `exo_`.
pub struct Todo;

impl App for Todo {
    type Mutation = TodoMutation;

    fn migrate(conn: &Connection) -> exo::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS todo (
                 id         BLOB PRIMARY KEY,
                 text       TEXT NOT NULL,
                 done       INTEGER NOT NULL DEFAULT 0,
                 pos        INTEGER NOT NULL,
                 created_ms INTEGER NOT NULL,
                 actor      TEXT NOT NULL,
                 claimed_by TEXT
             );",
        )?;
        Ok(())
    }
}

/// One materialised row, for assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: Uuid,
    pub text: String,
    pub done: bool,
    pub claimed_by: Option<String>,
}

/// Always `ORDER BY` explicitly: SQLite's natural order is not a contract.
pub fn items(conn: &Connection) -> Vec<Item> {
    let mut stmt = conn
        .prepare("SELECT id, text, done, claimed_by FROM todo ORDER BY pos, id")
        .expect("query todo");
    let rows = stmt
        .query_map([], |r| {
            Ok(Item {
                id: r.get(0)?,
                text: r.get(1)?,
                done: r.get::<_, i64>(2)? != 0,
                claimed_by: r.get(3)?,
            })
        })
        .expect("map todo rows");
    rows.map(|r| r.expect("read todo row")).collect()
}

#[allow(dead_code)]
pub fn texts(conn: &Connection) -> Vec<String> {
    items(conn).into_iter().map(|i| i.text).collect()
}
