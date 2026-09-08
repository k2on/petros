//! The to-do domain, compiled to wasm and hot-swapped at runtime.
//!
//! This is the point of the arrangement: `apply` is the one thing that must be
//! identical on every replica, and here it is a file the peer loads rather than
//! a symbol it was compiled with. The server interprets it, the phone
//! interprets it, and Metro replaces it on the phone in about the time it takes
//! to save this file.
//!
//! The SQL is written out rather than built with Diesel's DSL, because Diesel
//! needs a SQLite and there is none in here — only a channel to the host's.
//! That costs the compile-time `check_for_backend` check that a model still
//! matches its table, which the tests have to cover instead.

mod host;
pub mod verbs;

use ciborium::value::Value;
use host::{exec, lit, query_exists, query_int, Lit};

/// Applying a mutation: `Ok` or a deterministic refusal every replica reaches.
fn apply(mutation: &Value, actor: &str) -> Result<(), String> {
    let tag = field(mutation, "t").and_then(as_text).unwrap_or_default();
    match tag.as_str() {
        "Add" => {
            let id = field(mutation, "id")
                .and_then(as_bytes)
                .ok_or("Add has no id")?;
            let text = field(mutation, "text")
                .and_then(as_text)
                .unwrap_or_default();
            let created_ms = field(mutation, "created_ms").and_then(as_int).unwrap_or(0);
            if text.trim().is_empty() {
                return Err("a to-do needs some text".into());
            }
            // `on_conflict_do_nothing`: the same entry arriving twice is a
            // no-op, which is what makes redelivery safe.
            if query_exists(&format!(
                "SELECT 1 FROM todo WHERE id = {}",
                lit(Lit::Blob(&id))
            )) {
                return Ok(());
            }
            // `pos` is read out of current state: "put it at the end", an
            // intent, not "put it at 3", a fact. It is what makes the rebase
            // visible when an entry lands underneath yours.
            let last = query_int("SELECT COALESCE(MAX(pos), 0) FROM todo");
            exec(&format!(
                "INSERT INTO todo (id, text, done, pos, created_ms, actor) \
                 VALUES ({}, {}, 0, {}, {}, {})",
                lit(Lit::Blob(&id)),
                lit(Lit::Text(text.trim())),
                lit(Lit::Int(last + 1)),
                lit(Lit::Int(created_ms)),
                lit(Lit::Text(actor)),
            ));
            Ok(())
        }
        // Updating a row that is gone is a no-op, not an error: an entry
        // earlier in the log may have removed it.
        "SetDone" => {
            let id = field(mutation, "id")
                .and_then(as_bytes)
                .ok_or("SetDone has no id")?;
            let done = matches!(field(mutation, "done"), Some(Value::Bool(true)));
            exec(&format!(
                "UPDATE todo SET done = {} WHERE id = {}",
                lit(Lit::Int(done as i64)),
                lit(Lit::Blob(&id))
            ));
            Ok(())
        }
        // Five to-dos as one entry. The ids are already in the payload —
        // `fill_auto` put them there at the originating client — so this is as
        // deterministic as any other apply, and replaying it in a year produces
        // the same five rows.
        "AddFive" => {
            let Some(Value::Array(items)) = field(mutation, "items") else {
                return Err("AddFive has no items".into());
            };
            let created_ms = field(mutation, "created_ms").and_then(as_int).unwrap_or(0);
            // Read the end of the list once, then count up. Re-reading between
            // inserts would give the same answer and cost five more round trips
            // through the host.
            let mut pos = query_int("SELECT COALESCE(MAX(pos), 0) FROM todo");
            for item in items {
                let Some(id) = field(item, "id").and_then(as_bytes) else {
                    return Err("an item has no id".into());
                };
                let text = field(item, "text").and_then(as_text).unwrap_or_default();
                if text.trim().is_empty() {
                    continue;
                }
                if query_exists(&format!(
                    "SELECT 1 FROM todo WHERE id = {}",
                    lit(Lit::Blob(&id))
                )) {
                    continue;
                }
                pos += 1;
                exec(&format!(
                    "INSERT INTO todo (id, text, done, pos, created_ms, actor) \
                     VALUES ({}, {}, 0, {}, {}, {})",
                    lit(Lit::Blob(&id)),
                    lit(Lit::Text(text.trim())),
                    lit(Lit::Int(pos)),
                    lit(Lit::Int(created_ms)),
                    lit(Lit::Text(actor)),
                ));
            }
            Ok(())
        }
        // Marks everything done in one entry rather than one per row. That is
        // the intent — "I am finished" — and it is what makes it behave
        // correctly when it lands after entries its author never saw: rows
        // added by another peer in the meantime are covered too, which a
        // batch of per-row `SetDone`s would have missed.
        "MarkAllDone" => {
            exec("UPDATE todo SET done = 1 WHERE done = 0");
            Ok(())
        }
        "Remove" => {
            let id = field(mutation, "id")
                .and_then(as_bytes)
                .ok_or("Remove has no id")?;
            exec(&format!(
                "DELETE FROM todo WHERE id = {}",
                lit(Lit::Blob(&id))
            ));
            Ok(())
        }
        // A variant this module has never heard of. The log is permanent and
        // variants are only added, so this is a peer newer than us — and the
        // fix is to fetch a newer module, not to ship a new binary. Saying what
        // this one *does* know turns "why did nothing happen" into an answer.
        other => {
            let known: Vec<&str> = verbs::VERBS.iter().map(|v| v.name).collect();
            Err(format!(
                "unknown mutation \"{other}\"; this module knows {}",
                known.join(", ")
            ))
        }
    }
}

/// Hoist the non-deterministic arguments in. Runs exactly once, at the
/// originating client; from here the values are frozen in the log forever.
///
/// The host supplies the uuid and the clock, because those are the two things
/// it is allowed to have. Which fields they belong in is decided here, so that
/// knowledge lives with the mutation rather than with the engine.
fn fill_auto(mutation: &mut Value, uuid: Vec<u8>, now_ms: i64) {
    match field(mutation, "t").and_then(as_text).as_deref() {
        Some("Add") => {
            set(mutation, "id", Value::Bytes(uuid));
            set(mutation, "created_ms", Value::Integer(now_ms.into()));
        }
        // Five rows out of one seed.
        //
        // The names are just "item 1".."item 5", but the *ids* cannot be: they
        // have to be unique and `apply` may not invent them — `CLAUDE.md` asks
        // for that and the sandbox enforces it, since the host imports no
        // randomness. So the host's one uuid is expanded here, in the single
        // place non-determinism is allowed, and the log freezes the result.
        Some("AddFive") => {
            let mut seed = Seed::from(&uuid);
            let items = (1..=HOW_MANY)
                .map(|n| {
                    Value::Map(vec![
                        (Value::Text("id".into()), Value::Bytes(seed.id())),
                        (Value::Text("text".into()), Value::Text(format!("item {n}"))),
                    ])
                })
                .collect();
            set(mutation, "items", Value::Array(items));
            set(mutation, "created_ms", Value::Integer(now_ms.into()));
        }
        _ => {}
    }
}

const HOW_MANY: usize = 5;
/// xorshift128+, seeded from the uuid the host supplied.
///
/// Deliberately not a good random number generator — it is a *deterministic
/// expansion* of one non-deterministic seed, which is the only shape the log
/// can hold. The unpredictability is the host's uuid; everything after it is a
/// pure function of that, which is why replaying the entry reproduces the rows.
struct Seed(u64, u64);

impl Seed {
    fn from(bytes: &[u8]) -> Self {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
        Seed(h | 1, h.rotate_left(31) | 1)
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        let y = self.1;
        self.0 = y;
        x ^= x << 23;
        x ^= x >> 17;
        x ^= y ^ (y >> 26);
        self.1 = x;
        x.wrapping_add(y)
    }

    fn id(&mut self) -> Vec<u8> {
        let (a, b) = (self.next(), self.next());
        let mut out = Vec::with_capacity(16);
        out.extend_from_slice(&a.to_be_bytes());
        out.extend_from_slice(&b.to_be_bytes());
        out
    }
}

// ------------------------------------------------------------ CBOR accessors

fn field<'a>(v: &'a Value, name: &str) -> Option<&'a Value> {
    v.as_map()?
        .iter()
        .find(|(k, _)| k.as_text() == Some(name))
        .map(|(_, v)| v)
}

fn set(v: &mut Value, name: &str, to: Value) {
    if let Value::Map(entries) = v {
        for (k, existing) in entries.iter_mut() {
            if k.as_text() == Some(name) {
                *existing = to;
                return;
            }
        }
        entries.push((Value::Text(name.to_string()), to));
    }
}

fn as_text(v: &Value) -> Option<String> {
    v.as_text().map(str::to_string)
}

fn as_bytes(v: &Value) -> Option<Vec<u8>> {
    v.as_bytes().cloned()
}

fn as_int(v: &Value) -> Option<i64> {
    v.as_integer().and_then(|i| i128::from(i).try_into().ok())
}

// ------------------------------------------------------------------- the ABI

/// Give the host a buffer in our linear memory.
#[no_mangle]
pub extern "C" fn exo_alloc(len: u32) -> *mut u8 {
    let mut buf = Vec::<u8>::with_capacity(len as usize);
    let ptr = buf.as_mut_ptr();
    core::mem::forget(buf);
    ptr
}

/// `(ptr << 32) | len`, the one convention everything crossing here uses.
fn packed(bytes: Vec<u8>) -> u64 {
    let len = bytes.len() as u64;
    let ptr = bytes.leak().as_ptr() as u64;
    (ptr << 32) | len
}

/// Apply one mutation, as the CBOR payload the log stores, verbatim.
/// Zero for success; otherwise a packed refusal reason.
///
/// # Safety
/// The host must pass pointer/length pairs that describe live buffers in this
/// module's linear memory — which is the only thing `write_bytes` on the other
/// side produces.
#[no_mangle]
pub unsafe extern "C" fn exo_apply(
    mutation: *const u8,
    mutation_len: u32,
    actor: *const u8,
    actor_len: u32,
) -> u64 {
    let payload = unsafe { core::slice::from_raw_parts(mutation, mutation_len as usize) };
    let who = unsafe { core::slice::from_raw_parts(actor, actor_len as usize) };
    let who = core::str::from_utf8(who).unwrap_or("");

    let outcome = match ciborium::from_reader::<Value, _>(payload) {
        Ok(v) => apply(&v, who),
        Err(e) => Err(format!("could not decode a mutation: {e}")),
    };
    match outcome {
        Ok(()) => 0,
        Err(reason) => packed(reason.into_bytes()),
    }
}

/// Fill the auto values and hand back the rewritten payload.
///
/// # Safety
/// As [`exo_apply`]: the pointers must describe live buffers here, and `uuid`
/// must address sixteen readable bytes.
#[no_mangle]
pub unsafe extern "C" fn exo_fill_auto(
    mutation: *const u8,
    mutation_len: u32,
    uuid: *const u8,
    now_ms: i64,
) -> u64 {
    let payload = unsafe { core::slice::from_raw_parts(mutation, mutation_len as usize) };
    let uuid = unsafe { core::slice::from_raw_parts(uuid, 16) }.to_vec();
    let Ok(mut v) = ciborium::from_reader::<Value, _>(payload) else {
        return packed(Vec::new());
    };
    fill_auto(&mut v, uuid, now_ms);
    let mut out = Vec::new();
    if ciborium::into_writer(&v, &mut out).is_err() {
        return packed(Vec::new());
    }
    packed(out)
}

/// Bumped when the host/guest contract changes, so a mismatched pair says so.
#[no_mangle]
pub extern "C" fn exo_abi_version() -> u32 {
    1
}
