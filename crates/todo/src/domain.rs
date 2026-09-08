//! The to-do domain: `apply`, `fill_auto`, and nothing that knows where it runs.
//!
//! This is the one definition. It is compiled twice — natively into the server
//! and the terminal peers, and to wasm for the phone, which loads it as a file
//! it can replace without a rebuild. Two builds of one source is not two
//! implementations, and `tests/conformance.rs` holds it to that by driving the
//! same mutations through both and comparing the rows.
//!
//! The verbs are declared and dispatched by one macro, so an argument's name
//! is written once. It used to be written twice — in the schema and again in
//! whatever field `apply` reached for — and nothing checked the two agreed.
//!
//! Everything it can reach is [`Host`]: three methods, no clock, no randomness,
//! no network, no filesystem. On wasm that is enforced by the sandbox, because
//! the module imports nothing else. Natively it is enforced by this trait being
//! the only argument `apply` gets.

use petros_schema::cbor::{set, Value};

// The contract, not a copy of it: the same three methods the wasm module
// imports and the same escaping both builds go through.
pub use petros_schema::{lit, Host, Lit};

petros_schema::mutations! {
    /// Add a to-do at the end of the list.
    Add { text: Text } auto { id: Id, created_ms: Integer } => |host, actor| {
        if text.trim().is_empty() {
            return Err("a to-do needs some text".into());
        }
        // The same entry arriving twice is a no-op, which is what makes
        // redelivery safe.
        if host.query_exists(&format!(
            "SELECT 1 FROM todo WHERE id = {}",
            lit(Lit::Blob(&id))
        )) {
            return Ok(());
        }
        // `pos` is read out of current state: "put it at the end", an intent,
        // not "put it at 3", a fact. It is what makes the rebase visible when
        // an entry lands underneath yours.
        let last = host.query_int("SELECT COALESCE(MAX(pos), 0) FROM todo");
        host.exec(&format!(
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

    /// Five to-dos as one entry.
    ///
    /// The ids are already in the payload — `fill_auto` put them there at the
    /// originating client — so this is as deterministic as any other apply.
    AddFive {} auto { items: Array, created_ms: Integer } => |host, actor| {
        // Read the end of the list once, then count up. Re-reading between
        // inserts would give the same answer and cost five more round trips.
        let mut pos = host.query_int("SELECT COALESCE(MAX(pos), 0) FROM todo");
        for item in items {
            let Some(id) = petros_schema::cbor::field(item, "id")
                .and_then(petros_schema::cbor::as_bytes)
            else {
                return Err("an item has no id".into());
            };
            let text = petros_schema::cbor::opt_text(item, "text");
            if text.trim().is_empty() {
                continue;
            }
            if host.query_exists(&format!(
                "SELECT 1 FROM todo WHERE id = {}",
                lit(Lit::Blob(&id))
            )) {
                continue;
            }
            pos += 1;
            host.exec(&format!(
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

    /// One entry rather than one per row, so it covers rows another peer added
    /// in the meantime. That is what makes it an intent.
    MarkAllDone {} => |host, actor| {
        let _ = actor;
        host.exec("UPDATE todo SET done = 1 WHERE done = 0");
        Ok(())
    }

    /// Updating a row that is gone is a no-op, not an error: an entry earlier
    /// in the log may have removed it.
    SetDone { id: Id, done: Bool } => |host, actor| {
        let _ = actor;
        host.exec(&format!(
            "UPDATE todo SET done = {} WHERE id = {}",
            lit(Lit::Int(done as i64)),
            lit(Lit::Blob(&id))
        ));
        Ok(())
    }

    Remove { id: Id } => |host, actor| {
        let _ = actor;
        host.exec(&format!(
            "DELETE FROM todo WHERE id = {}",
            lit(Lit::Blob(&id))
        ));
        Ok(())
    }
}

/// Hoist the non-deterministic arguments in. Runs exactly once, at the
/// originating client; from here the values are frozen in the log forever.
///
/// The caller supplies the uuid and the clock, because those are the two things
/// it is allowed to have. Which fields they belong in is decided here, so that
/// knowledge lives with the mutation rather than with the engine.
pub fn fill_auto(mutation: &mut Value, uuid: Vec<u8>, now_ms: i64) {
    match petros_schema::cbor::field(mutation, "t")
        .and_then(petros_schema::cbor::as_text)
        .as_deref()
    {
        Some("Add") => {
            set(mutation, "id", Value::Bytes(uuid));
            set(mutation, "created_ms", Value::Integer(now_ms.into()));
        }
        // Five rows out of one seed.
        //
        // The names are just "item 1".."item 5", but the *ids* cannot be: they
        // have to be unique and `apply` may not invent them, because the only
        // thing it can reach is [`Host`]. So the one uuid is expanded here, in
        // the single place non-determinism is allowed, and the log freezes it.
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

/// xorshift128+, seeded from the uuid the caller supplied.
///
/// Deliberately not a good random number generator — it is a *deterministic
/// expansion* of one non-deterministic seed, which is the only shape the log
/// can hold. The unpredictability is the uuid; everything after it is a pure
/// function of that, which is why replaying the entry reproduces the rows.
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
