//! A shared to-do list. Nothing clever, and deliberately dull: the point is
//! that Exo has never heard of any of it.

use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use diesel::sqlite::Sqlite;
use exo::{ActorId, App, AutoCtx, Connection, Id, Mutation, MutationError, Transaction};
use serde::{Deserialize, Serialize};

diesel::table! {
    todo (id) {
        id -> Binary,
        text -> Text,
        done -> Bool,
        pos -> BigInt,
        created_ms -> BigInt,
        actor -> Text,
    }
}

/// The model. One struct describes the row for both reading and writing.
#[derive(Debug, Clone, Queryable, Selectable, Insertable)]
#[diesel(table_name = todo, check_for_backend(Sqlite))]
pub struct Item {
    pub id: Id,
    pub text: String,
    pub done: bool,
    pub pos: i64,
    pub created_ms: i64,
    pub actor: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum Todo {
    Add {
        id: Id,
        text: String,
        created_ms: i64,
    },
    SetDone {
        id: Id,
        done: bool,
    },
    Remove {
        id: Id,
    },
}

impl Todo {
    /// `id` and `created_ms` are placeholders until `fill_auto` runs.
    pub fn add(text: &str) -> Self {
        Todo::Add {
            id: Id::nil(),
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

    fn apply(&self, tx: &mut Transaction, actor: &ActorId) -> Result<(), MutationError> {
        let conn = tx.conn();
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
                let last: Option<i64> = todo::table
                    .select(diesel::dsl::max(todo::pos))
                    .first(conn)?;
                diesel::insert_into(todo::table)
                    .values(Item {
                        id: *id,
                        text: text.trim().to_string(),
                        done: false,
                        pos: last.unwrap_or(0) + 1,
                        created_ms: *created_ms,
                        actor: actor.as_str().to_string(),
                    })
                    .on_conflict_do_nothing()
                    .execute(conn)?;
            }
            // Updating a row that is gone is a no-op, not an error: an entry
            // earlier in the log may have removed it.
            Todo::SetDone { id, done } => {
                diesel::update(todo::table.find(id))
                    .set(todo::done.eq(done))
                    .execute(conn)?;
            }
            Todo::Remove { id } => {
                diesel::delete(todo::table.find(id)).execute(conn)?;
            }
        }
        Ok(())
    }
}

pub struct TodoApp;

impl App for TodoApp {
    type Mutation = Todo;

    fn migrate(conn: &mut Connection) -> exo::Result<()> {
        conn.batch_execute(
            "CREATE TABLE IF NOT EXISTS todo (
                 id         BLOB PRIMARY KEY NOT NULL,
                 text       TEXT NOT NULL,
                 done       BOOL NOT NULL DEFAULT 0,
                 pos        BIGINT NOT NULL,
                 created_ms BIGINT NOT NULL,
                 actor      TEXT NOT NULL
             );",
        )?;
        Ok(())
    }
}

/// Always ordered explicitly.
pub fn list(conn: &mut Connection) -> exo::Result<Vec<Item>> {
    Ok(todo::table
        .select(Item::as_select())
        .order((todo::pos.asc(), todo::id.asc()))
        .load(conn)?)
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
