//! The exo app whose `apply` is a wasm module.
//!
//! This is what makes the hot swap reach the engine rather than stopping at the
//! FFI. `exo::Client` calls `Mutation::apply` during a rebase and hands it no
//! context of ours, so the module lives in a process-wide slot and this looks it
//! up — the same shape a linked `apply` had, except that it can be replaced
//! between one mutation and the next.
//!
//! The mutation type is the CBOR payload itself, not a Rust enum mirroring it.
//! That is deliberate and it is the whole reason a client can outlive the code
//! it was built with: a peer that has never heard of a variant still carries it
//! through the log intact, and applies it as soon as it has a module that
//! knows what it means.

use ciborium::value::Value;
use diesel::connection::SimpleConnection;
use exo::{ActorId, App, AutoCtx, Connection, Mutation, MutationError, Transaction};
use serde::{Deserialize, Serialize};

use crate::MUTATORS;

/// One mutation, as the bytes the log stores.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Payload(pub Value);

impl Payload {
    fn bytes(&self) -> Result<Vec<u8>, MutationError> {
        let mut out = Vec::new();
        ciborium::into_writer(&self.0, &mut out)
            .map_err(|e| MutationError::rejected(format!("could not re-encode a mutation: {e}")))?;
        Ok(out)
    }
}

fn missing() -> MutationError {
    // Not a rejection: a rejection is a verdict every replica reaches, and
    // "this peer has not loaded a module yet" is a fact about this peer only.
    MutationError::rejected("no mutator module is loaded")
}

impl Mutation for Payload {
    /// The host supplies the uuid and the clock; the module decides where they
    /// belong. That keeps the one piece of domain knowledge involved — which
    /// fields are auto-filled — in the module rather than here.
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
            // The module said no. A deterministic verdict, and the entry will
            // never be in the log.
            Ok(Err(reason)) => Err(MutationError::rejected(reason)),
            Ok(Ok(())) => Ok(()),
            // The host or the module broke — a trap, a bad query. That says
            // nothing about the mutation, so it must not be a rejection.
            Err(e) => Err(MutationError::rejected(format!("the mutator failed: {e}"))),
        }
    }
}

/// The app: Exo's tables plus this one.
pub struct WasmTodo;

impl App for WasmTodo {
    type Mutation = Payload;

    fn migrate(conn: &mut Connection) -> exo::Result<()> {
        // The schema still lives in Rust. A module could not create it without
        // being trusted with arbitrary DDL, and migrations are the one thing
        // that should not arrive over the air.
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

/// Build an `Add` the way the module expects it, with the placeholders
/// `fill_auto` replaces. The one place this crate names a mutation's shape;
/// everything else about the domain is in the module.
pub fn add(text: &str) -> Payload {
    Payload(Value::Map(vec![
        ("t".into(), "Add".into()),
        ("id".into(), Value::Bytes(vec![0u8; 16])),
        ("text".into(), text.into()),
        ("created_ms".into(), Value::Integer(0.into())),
    ]))
}

pub fn set_done(id: &[u8], done: bool) -> Payload {
    Payload(Value::Map(vec![
        ("t".into(), "SetDone".into()),
        ("id".into(), Value::Bytes(id.to_vec())),
        ("done".into(), Value::Bool(done)),
    ]))
}

pub fn remove(id: &[u8]) -> Payload {
    Payload(Value::Map(vec![
        ("t".into(), "Remove".into()),
        ("id".into(), Value::Bytes(id.to_vec())),
    ]))
}
