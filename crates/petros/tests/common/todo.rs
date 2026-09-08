//! A shared to-do list, expressed as intents rather than facts.

use diesel::connection::SimpleConnection;
use diesel::prelude::*;
use diesel::sqlite::Sqlite;
use petros::{ActorId, App, AutoCtx, Connection, Id, Mutation, MutationError, Transaction};
use serde::{Deserialize, Serialize};

diesel::table! {
    todo (id) {
        id -> Binary,
        text -> Text,
        done -> Bool,
        pos -> BigInt,
        created_ms -> BigInt,
        actor -> Text,
        claimed_by -> Nullable<Text>,
    }
}

/// The model. One struct describes the row for both reading and writing.
#[derive(Debug, Clone, PartialEq, Eq, Queryable, Selectable, Insertable)]
#[diesel(table_name = todo, check_for_backend(Sqlite))]
pub struct Item {
    pub id: Id,
    pub text: String,
    pub done: bool,
    pub pos: i64,
    pub created_ms: i64,
    pub actor: String,
    pub claimed_by: Option<String>,
}

/// The app's mutation enum. `tag = "t"` because the log is permanent: an
/// internally tagged representation lets us add fields to a variant without
/// invalidating bytes already written.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t")]
pub enum TodoMutation {
    Add {
        id: Id,
        text: String,
        created_ms: i64,
    },
    SetDone {
        id: Id,
        done: bool,
    },
    Rename {
        id: Id,
        text: String,
    },
    Remove {
        id: Id,
    },
    /// Claim an item for the acting actor. Rejects if someone else got there
    /// first — a verdict two replicas can disagree about until the log settles
    /// it, which is exactly what makes it worth testing.
    Claim {
        id: Id,
    },
}

impl TodoMutation {
    /// `id` and `created_ms` are placeholders; `fill_auto` replaces them at the
    /// originating client and they are frozen from then on.
    pub fn add(text: &str) -> Self {
        TodoMutation::Add {
            id: Id::nil(),
            text: text.to_string(),
            created_ms: 0,
        }
    }

    pub fn set_done(id: Id, done: bool) -> Self {
        TodoMutation::SetDone { id, done }
    }

    pub fn rename(id: Id, text: &str) -> Self {
        TodoMutation::Rename {
            id,
            text: text.to_string(),
        }
    }

    pub fn remove(id: Id) -> Self {
        TodoMutation::Remove { id }
    }

    pub fn claim(id: Id) -> Self {
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

    fn apply(&self, tx: &mut Transaction, actor: &ActorId) -> Result<(), MutationError> {
        let conn = tx.conn();
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
                let last: Option<i64> = todo::table
                    .select(diesel::dsl::max(todo::pos))
                    .first(conn)?;
                diesel::insert_into(todo::table)
                    .values(Item {
                        id: *id,
                        text: text.clone(),
                        done: false,
                        pos: last.unwrap_or(0) + 1,
                        created_ms: *created_ms,
                        actor: actor.as_str().to_string(),
                        claimed_by: None,
                    })
                    .on_conflict_do_nothing()
                    .execute(conn)?;
            }
            // Updates to a row that is gone are no-ops rather than errors: the
            // row may have been removed by a mutation earlier in the log.
            TodoMutation::SetDone { id, done } => {
                diesel::update(todo::table.find(id))
                    .set(todo::done.eq(done))
                    .execute(conn)?;
            }
            TodoMutation::Rename { id, text } => {
                diesel::update(todo::table.find(id))
                    .set(todo::text.eq(text))
                    .execute(conn)?;
            }
            TodoMutation::Remove { id } => {
                diesel::delete(todo::table.find(id)).execute(conn)?;
            }
            TodoMutation::Claim { id } => {
                let held: Option<Option<String>> = todo::table
                    .find(id)
                    .select(todo::claimed_by)
                    .first(conn)
                    .optional()?;
                match held {
                    // The row is gone; nothing to claim.
                    None => {}
                    Some(Some(who)) if who != actor.as_str() => {
                        return Err(MutationError::rejected(format!("already claimed by {who}")))
                    }
                    Some(_) => {
                        diesel::update(todo::table.find(id))
                            .set(todo::claimed_by.eq(actor.as_str()))
                            .execute(conn)?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// The app. Owns the `todo` table; Petros owns everything prefixed `petros_`.
pub struct Todo;

impl App for Todo {
    type Mutation = TodoMutation;

    fn migrate(conn: &mut Connection) -> petros::Result<()> {
        conn.batch_execute(
            "CREATE TABLE IF NOT EXISTS todo (
                 id         BLOB PRIMARY KEY NOT NULL,
                 text       TEXT NOT NULL,
                 done       BOOL NOT NULL DEFAULT 0,
                 pos        BIGINT NOT NULL,
                 created_ms BIGINT NOT NULL,
                 actor      TEXT NOT NULL,
                 claimed_by TEXT
             );",
        )?;
        Ok(())
    }
}

/// Always `ORDER BY` explicitly: SQLite's natural order is not a contract.
pub fn items(conn: &mut Connection) -> Vec<Item> {
    todo::table
        .select(Item::as_select())
        .order((todo::pos.asc(), todo::id.asc()))
        .load(conn)
        .expect("load todo items")
}

pub fn texts(conn: &mut Connection) -> Vec<String> {
    items(conn).into_iter().map(|i| i.text).collect()
}
