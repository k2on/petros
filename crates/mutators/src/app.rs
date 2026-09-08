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

/// Build a mutation from a name and its arguments, without knowing what either
/// means.
///
/// This is the entry point that makes the design pay off. Every other way to
/// author a mutation needs a Rust function per verb, and therefore a new uniffi
/// export, a native rebuild and a trip through an app store — which defeats the
/// point of `apply` being hot-swappable. Through here, adding a verb to the
/// domain is a module rebuild and a call site: both of them things Metro can
/// push, and both of them things an over-the-air update can carry.
///
/// It needs no domain knowledge because the module supplies all of it. The
/// payload is just `{ "t": kind, ...args }`, and the auto-filled fields are not
/// this function's problem: `fill_auto` runs inside the module afterwards and
/// *appends* whatever the verb needs, so `add` here is `{"text": "..."}` and the
/// id and the timestamp appear later, chosen by the module.
///
/// One convention, and it is protocol rather than domain: a field named `id`
/// holding a canonical uuid becomes the sixteen-byte string the log uses.
/// Nothing else in this wire format is anything but JSON's own types.
pub fn from_json(kind: &str, args_json: &str) -> Result<Payload, String> {
    let args: serde_json::Value = if args_json.trim().is_empty() {
        serde_json::Value::Object(Default::default())
    } else {
        serde_json::from_str(args_json).map_err(|e| format!("the arguments are not json: {e}"))?
    };
    from_value(kind, args)
}

/// As [`from_json`], for a caller that already has the arguments as values —
/// which is every Rust peer. One encoder, so the phone and the terminal cannot
/// disagree about what an `Add` looks like on the wire.
pub fn from_value(kind: &str, args: serde_json::Value) -> Result<Payload, String> {
    let serde_json::Value::Object(args) = args else {
        return Err("the arguments should be a json object".into());
    };

    let mut fields = vec![(Value::Text("t".into()), Value::Text(kind.to_string()))];
    for (name, value) in args {
        let is_id = name == "id" || name.ends_with("_id");
        fields.push((Value::Text(name), json_to_cbor(value, is_id)?));
    }
    Ok(Payload(Value::Map(fields)))
}

fn json_to_cbor(value: serde_json::Value, is_id: bool) -> Result<Value, String> {
    use serde_json::Value as J;
    Ok(match value {
        J::Null => Value::Null,
        J::Bool(b) => Value::Bool(b),
        J::Number(n) => match n.as_i64() {
            Some(i) => Value::Integer(i.into()),
            // `docs/decisions.md`: no floats anywhere near the log.
            None => return Err(format!("{n} is not an integer")),
        },
        J::String(s) if is_id => Value::Bytes(
            exo::uuid::Uuid::parse_str(&s)
                .map_err(|e| format!("not an id: {e}"))?
                .as_bytes()
                .to_vec(),
        ),
        J::String(s) => Value::Text(s),
        J::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|v| json_to_cbor(v, false))
                .collect::<Result<_, _>>()?,
        ),
        J::Object(entries) => Value::Map(
            entries
                .into_iter()
                .map(|(k, v)| {
                    let is_id = k == "id" || k.ends_with("_id");
                    Ok((Value::Text(k), json_to_cbor(v, is_id)?))
                })
                .collect::<Result<Vec<_>, String>>()?,
        ),
    })
}

// The three verbs the Rust peers spell out. They are conveniences over
// [`from_value`], not a second encoder: a call site reads better with a name,
// and the wire format still has exactly one definition.
//
// The `expect` cannot fire. The only fallible step in `from_value` is parsing a
// uuid, and these format one rather than taking it from a caller.

pub fn add(text: &str) -> Payload {
    from_value("Add", serde_json::json!({ "text": text })).expect("a text is always encodable")
}

pub fn set_done(id: &[u8; 16], done: bool) -> Payload {
    let id = exo::uuid::Uuid::from_bytes(*id).to_string();
    from_value("SetDone", serde_json::json!({ "id": id, "done": done }))
        .expect("a formatted uuid always parses")
}

pub fn remove(id: &[u8; 16]) -> Payload {
    let id = exo::uuid::Uuid::from_bytes(*id).to_string();
    from_value("Remove", serde_json::json!({ "id": id })).expect("a formatted uuid always parses")
}

/// Mark every unfinished to-do done, in one entry rather than one per row.
pub fn mark_all_done() -> Payload {
    from_value("MarkAllDone", serde_json::json!({})).expect("no arguments to encode")
}
