//! The exo app whose `apply` is a wasm module — the phone's, and only the
//! phone's.
//!
//! Every other peer links `todo` and calls `apply` directly; see
//! `docs/decisions.md` for why the indirection is confined to here. What it
//! buys is the thing only this peer wants: a domain it can replace over Metro,
//! or one day over the air, without a native build.
//!
//! `exo` calls `Mutation::apply` during a rebase and hands it no context of
//! ours, so the module lives in a process-wide slot and this looks it up. One
//! domain per process is the same assumption a linked `apply` makes; this only
//! makes it replaceable while the process runs.

use ciborium::value::Value;
use exo::{ActorId, App, AutoCtx, Connection, Mutation, MutationError, Transaction};
use serde::{Deserialize, Serialize};

use crate::MUTATORS;

/// One mutation, as the bytes the log stores.
///
/// Wire-identical to [`todo::Payload`] — both are `#[serde(transparent)]` over
/// the same CBOR value — and separate only because a crate may not implement a
/// foreign trait for a foreign type. `conformance.rs` checks they agree.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Payload(pub Value);

impl From<todo::Payload> for Payload {
    fn from(p: todo::Payload) -> Self {
        Payload(p.0)
    }
}

impl Payload {
    fn bytes(&self) -> Result<Vec<u8>, MutationError> {
        let mut out = Vec::new();
        ciborium::into_writer(&self.0, &mut out)
            .map_err(|e| MutationError::rejected(format!("could not re-encode a mutation: {e}")))?;
        Ok(out)
    }
}

fn missing() -> MutationError {
    // Not a rejection about the mutation: "this peer has not loaded a module"
    // is a fact about this peer only. It travels as one because that is the
    // only channel `apply` has, which is a wart worth knowing about.
    MutationError::rejected("no mutator module is loaded")
}

impl Mutation for Payload {
    fn fill_auto(&mut self, ctx: &mut AutoCtx) {
        let Ok(guard) = MUTATORS.read() else { return };
        let Some(mutators) = guard.as_ref() else {
            return;
        };
        let Ok(payload) = self.bytes() else { return };
        if let Ok(filled) = mutators.fill_auto(&payload, ctx) {
            if let Ok(value) = ciborium::from_reader::<Value, _>(filled.as_slice()) {
                self.0 = value;
            }
        }
    }

    fn apply(&self, tx: &mut Transaction, actor: &ActorId) -> Result<(), MutationError> {
        let guard = MUTATORS.read().map_err(|_| missing())?;
        let mutators = guard.as_ref().ok_or_else(missing)?;
        let payload = self.bytes()?;
        match mutators.apply(tx.conn(), &payload, actor.as_str()) {
            // The module said no: a deterministic verdict, and the entry will
            // never be in the log.
            Ok(Err(reason)) => Err(MutationError::rejected(reason)),
            Ok(Ok(())) => Ok(()),
            // The host or the module broke — a trap, a bad query. That says
            // nothing about the mutation, so it must not be a rejection.
            Err(e) => Err(MutationError::rejected(format!("the mutator failed: {e}"))),
        }
    }
}

/// The app, for a peer whose `apply` arrives as a file.
pub struct WasmTodo;

impl App for WasmTodo {
    type Mutation = Payload;

    fn migrate(conn: &mut Connection) -> exo::Result<()> {
        // One schema. Migrations are the one thing that should not arrive over
        // the air, so they stay where every peer can see them.
        <todo::TodoApp as App>::migrate(conn)
    }
}

/// Author a mutation by name, through the same encoder every peer uses.
pub fn from_json(kind: &str, args_json: &str) -> Result<Payload, String> {
    todo::from_json(kind, args_json).map(Payload::from)
}
