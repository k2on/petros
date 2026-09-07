//! A shared to-do list. Nothing clever, and deliberately dull: the point is
//! that Exo has never heard of any of it.

use exo::{ActorId, App, AutoCtx, Connection, Mutation, MutationError, Transaction, Uuid};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Todo {
    Add {
        id: Uuid,
        text: String,
        created_ms: i64,
    },
    SetDone {
        id: Uuid,
        done: bool,
    },
    Remove {
        id: Uuid,
    },
}

impl Todo {
    /// `id` and `created_ms` are placeholders until `fill_auto` runs.
    pub fn add(text: &str) -> Self {
        Todo::Add {
            id: Uuid::nil(),
            text: text.to_string(),
            created_ms: 0,
        }
    }
}

impl Mutation for Todo {
    fn fill_auto(&mut self, ctx: &mut AutoCtx) {
        if let Todo::Add { id, created_ms, .. } = self {
            *id = ctx.uuid();
            *created_ms = ctx.now_ms();
        }
    }

    fn apply(&self, tx: &Transaction, actor: &ActorId) -> Result<(), MutationError> {
        match self {
            Todo::Add {
                id,
                text,
                created_ms,
            } => {
                if text.trim().is_empty() {
                    return Err(MutationError::rejected("a to-do needs some text"));
                }
                // `pos` is read out of current state: this is "put it at the
                // end", an intent, not "put it at 3", a fact. It is also what
                // makes the rebase visible in the demo.
                tx.execute(
                    "INSERT OR IGNORE INTO todo (id, text, done, pos, created_ms, actor)
                     SELECT ?1, ?2, 0, COALESCE(MAX(pos), 0) + 1, ?3, ?4 FROM todo",
                    exo::rusqlite::params![id, text.trim(), created_ms, actor.as_str()],
                )?;
            }
            // Updating a row that is gone is a no-op, not an error: an entry
            // earlier in the log may have removed it.
            Todo::SetDone { id, done } => {
                tx.execute(
                    "UPDATE todo SET done = ?2 WHERE id = ?1",
                    exo::rusqlite::params![id, done],
                )?;
            }
            Todo::Remove { id } => {
                tx.execute("DELETE FROM todo WHERE id = ?1", exo::rusqlite::params![id])?;
            }
        }
        Ok(())
    }
}

pub struct TodoApp;

impl App for TodoApp {
    type Mutation = Todo;

    fn migrate(conn: &Connection) -> exo::Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS todo (
                 id         BLOB PRIMARY KEY,
                 text       TEXT NOT NULL,
                 done       INTEGER NOT NULL DEFAULT 0,
                 pos        INTEGER NOT NULL,
                 created_ms INTEGER NOT NULL,
                 actor      TEXT NOT NULL
             );",
        )?;
        Ok(())
    }
}

pub struct Item {
    pub id: Uuid,
    pub text: String,
    pub done: bool,
    pub actor: String,
}

/// Always ordered explicitly.
pub fn list(conn: &Connection) -> exo::Result<Vec<Item>> {
    let mut stmt = conn.prepare("SELECT id, text, done, actor FROM todo ORDER BY pos, id")?;
    let rows = stmt.query_map([], |r| {
        Ok(Item {
            id: r.get(0)?,
            text: r.get(1)?,
            done: r.get::<_, i64>(2)? != 0,
            actor: r.get(3)?,
        })
    })?;
    Ok(rows.collect::<exo::rusqlite::Result<Vec<_>>>()?)
}

/// One line per item, as the demo prints it.
pub fn render(items: &[Item]) -> String {
    items
        .iter()
        .enumerate()
        .map(|(i, it)| {
            let mark = if it.done { "x" } else { " " };
            format!("  {:>2}. [{}] {}  ({})", i + 1, mark, it.text, it.actor)
        })
        .collect::<Vec<_>>()
        .join("\n")
}
