//! The log freezes a mutation's *arguments*. It does not freeze its meaning.
//!
//! `docs/decisions.md` says the values `fill_auto` chose are in the log forever,
//! "so a replay years later on a different machine produces the same state".
//! That is true of the arguments and not of the code: `apply` is whatever the
//! peer was built with, and a peer replaying entry 1 today runs today's version
//! of it.
//!
//! This is a demonstration rather than a guard. It passes when the hazard is
//! present, which is the point — it is here so the claim in the decisions log is
//! evidence rather than an argument, and so that anything built to close it has
//! something to fail against.

mod common;

use common::todo::{items, TodoMutation};
use petros::{ActorId, App, AutoCtx, Client, Mutation, MutationError, Transaction};

/// The same app, a release later, with one mutation's meaning changed.
///
/// Deliberately the mildest kind of change — the *gap* between positions, not
/// the shape of anything. The wire format is untouched, so every entry already
/// in the log still decodes perfectly and nothing anywhere reports a problem.
struct Todo2;

impl App for Todo2 {
    type Mutation = Mutation2;
    const SCHEMA: &'static str = <common::todo::Todo as App>::SCHEMA;
}

/// A newtype over the original, so the bytes in the log are identical and only
/// the behaviour differs.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
struct Mutation2(TodoMutation);

impl Mutation for Mutation2 {
    fn fill_auto(&mut self, ctx: &mut AutoCtx) {
        self.0.fill_auto(ctx)
    }

    fn apply(&self, tx: &mut Transaction, actor: &ActorId) -> Result<(), MutationError> {
        match &self.0 {
            // The change: positions now step by ten, so a later insert can be
            // slid between two without renumbering. An entirely reasonable
            // thing to want, and the reason it is worth knowing what it does to
            // entries already written.
            TodoMutation::Add {
                id,
                text,
                created_ms,
            } => {
                use diesel::prelude::*;
                let conn = tx.conn();
                if text.is_empty() {
                    return Err(MutationError::rejected("a to-do needs some text"));
                }
                let last: Option<i64> = common::todo::todo::table
                    .select(diesel::dsl::max(common::todo::todo::pos))
                    .first(conn)?;
                diesel::insert_into(common::todo::todo::table)
                    .values(common::todo::Item {
                        id: *id,
                        text: text.clone(),
                        done: false,
                        pos: last.unwrap_or(0) + 10,
                        created_ms: *created_ms,
                        actor: actor.as_str().to_string(),
                        claimed_by: None,
                    })
                    .on_conflict_do_nothing()
                    .execute(conn)?;
                Ok(())
            }
            other => other.apply(tx, actor),
        }
    }
}

/// The entries a peer authored, as the server receives them and fans them out.
fn authored(client: &mut Client<common::todo::Todo>) -> Vec<petros::Entry<TodoMutation>> {
    client
        .take_outgoing()
        .into_iter()
        .flat_map(|msg| match msg {
            petros::ClientMsg::Push { entries } => entries,
            _ => Vec::new(),
        })
        .enumerate()
        .map(|(i, e)| petros::Entry {
            id: e.id,
            seq: Some(i as u64 + 1),
            actor: e.actor,
            mutation: e.mutation,
        })
        .collect()
}

/// Two peers on the same log, one built before the change and one after, end up
/// in different states — with no error, no warning, and nothing in the log that
/// could have told either of them.
#[test]
fn a_peer_built_later_replays_history_differently() {
    let mut author = Client::<common::todo::Todo>::open(
        petros::open_memory().unwrap(),
        "alice",
        AutoCtx::seeded(1),
    )
    .unwrap();
    for text in ["first", "second", "third"] {
        author.mutate(TodoMutation::add(text)).unwrap();
    }
    let log = authored(&mut author);
    assert_eq!(log.len(), 3);

    // The peer that was already running when these were written.
    let mut before = Client::<common::todo::Todo>::open(
        petros::open_memory().unwrap(),
        "bob",
        AutoCtx::seeded(2),
    )
    .unwrap();
    before
        .recv(petros::ServerMsg::Batch {
            entries: log.clone(),
            has_more: false,
        })
        .unwrap();
    let old: Vec<i64> = items(before.conn()).iter().map(|i| i.pos).collect();
    assert_eq!(old, vec![1, 2, 3]);

    // A peer that installed the next release and is syncing from scratch. The
    // same bytes; nothing about the entries has changed.
    let mut after =
        Client::<Todo2>::open(petros::open_memory().unwrap(), "carol", AutoCtx::seeded(3)).unwrap();
    after
        .recv(petros::ServerMsg::Batch {
            entries: log
                .into_iter()
                .map(|e| petros::Entry {
                    id: e.id,
                    seq: e.seq,
                    actor: e.actor,
                    mutation: Mutation2(e.mutation),
                })
                .collect(),
            has_more: false,
        })
        .unwrap();
    let new: Vec<i64> = items(after.conn()).iter().map(|i| i.pos).collect();
    assert_eq!(new, vec![10, 20, 30]);

    assert_ne!(
        old, new,
        "two peers, one log, two answers — and neither was told"
    );
}
