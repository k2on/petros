//! Everything that needs a real SQLite: the table, the rows, the Diesel host
//! and the `petros::App` the native peers run.
//!
//! Behind the `storage` feature, because the wasm build wants
//! [`domain`](crate::domain) and none of this.

use ciborium::value::Value;
use diesel::connection::SimpleConnection;
use diesel::deserialize::QueryableByName;
use diesel::prelude::*;
use diesel::sql_query;
use diesel::sql_types::BigInt;
use diesel::sqlite::Sqlite as SqliteBackend;
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
    }
}

/// One row of the materialised view. Read-only: rows are produced by the
/// module's `apply`, never by this crate.
#[derive(Debug, Clone, Queryable, Selectable)]
#[diesel(table_name = todo, check_for_backend(SqliteBackend))]
pub struct Item {
    pub id: Id,
    pub text: String,
    pub done: bool,
    pub pos: i64,
    pub created_ms: i64,
    pub actor: String,
}

/// Always ordered explicitly.
pub fn list(conn: &mut Connection) -> petros::Result<Vec<Item>> {
    Ok(todo::table
        .select(Item::as_select())
        .order((todo::pos.asc(), todo::id.asc()))
        .load(conn)?)
}

// ---------------------------------------------------------------- the writes

/// One mutation, as the bytes the log stores.
///
/// Not a Rust enum mirroring the variants, and that is deliberate: a peer that
/// has never heard of a variant still carries it through the log intact, and
/// applies it as soon as it has a build that knows what it means.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Payload(pub Value);

/// The database, handed to `apply` and nothing else.
struct Sqlite<'a>(&'a mut Connection);

impl crate::domain::Host for Sqlite<'_> {
    fn query_int(&mut self, sql: &str) -> i64 {
        #[derive(QueryableByName)]
        struct Row {
            #[diesel(sql_type = BigInt)]
            v: i64,
        }
        sql_query(format!("SELECT ({sql}) AS v"))
            .load::<Row>(&mut *self.0)
            .ok()
            .and_then(|rows| rows.first().map(|r| r.v))
            .unwrap_or(0)
    }

    fn query_exists(&mut self, sql: &str) -> bool {
        #[derive(QueryableByName)]
        struct Row {
            #[diesel(sql_type = BigInt)]
            v: i64,
        }
        sql_query(format!("SELECT EXISTS({sql}) AS v"))
            .load::<Row>(&mut *self.0)
            .ok()
            .and_then(|rows| rows.first().map(|r| r.v != 0))
            .unwrap_or(false)
    }

    fn exec(&mut self, sql: &str) {
        let _ = self.0.batch_execute(sql);
    }
}

impl Mutation for Payload {
    fn fill_auto(&mut self, ctx: &mut AutoCtx) {
        let uuid = ctx.uuid().as_uuid().as_bytes().to_vec();
        crate::domain::fill_auto(&mut self.0, uuid, ctx.now_ms());
    }

    fn apply(&self, tx: &mut Transaction, actor: &ActorId) -> Result<(), MutationError> {
        crate::domain::apply(&mut Sqlite(tx.conn()), &self.0, actor.as_str())
            .map_err(MutationError::rejected)
    }
}

/// The app: Petros's tables plus this one.
pub struct TodoApp;

impl App for TodoApp {
    type Mutation = Payload;

    fn migrate(conn: &mut Connection) -> petros::Result<()> {
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

// ------------------------------------------------------------------ authoring

/// Build a mutation from a verb name and its arguments, without knowing what
/// either means.
///
/// The payload is just `{ "t": kind, ...args }`. Auto-filled fields are not
/// this function's problem: `fill_auto` appends whatever the verb needs
/// afterwards, so `Add` here is `{"text": "..."}` and the id and the timestamp
/// arrive later, chosen by the domain.
///
/// One convention, and it is protocol rather than domain: a field named `id`
/// holding a canonical uuid becomes the sixteen-byte string the log uses.
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

/// As [`from_value`], for a caller that has the arguments as JSON text — which
/// is every foreign one, since it has no CBOR encoder.
pub fn from_json(kind: &str, args_json: &str) -> Result<Payload, String> {
    let args: serde_json::Value = if args_json.trim().is_empty() {
        serde_json::Value::Object(Default::default())
    } else {
        serde_json::from_str(args_json).map_err(|e| format!("the arguments are not json: {e}"))?
    };
    from_value(kind, args)
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
            petros::uuid::Uuid::parse_str(&s)
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

// The three verbs the Rust peers spell out. Conveniences over [`from_value`],
// not a second encoder. The `expect`s cannot fire: the only fallible step is
// parsing a uuid, and these format one rather than taking it from a caller.

pub fn add(text: &str) -> Payload {
    from_value("Add", serde_json::json!({ "text": text })).expect("a text is always encodable")
}

pub fn set_done(id: &[u8; 16], done: bool) -> Payload {
    let id = petros::uuid::Uuid::from_bytes(*id).to_string();
    from_value("SetDone", serde_json::json!({ "id": id, "done": done }))
        .expect("a formatted uuid always parses")
}

pub fn remove(id: &[u8; 16]) -> Payload {
    let id = petros::uuid::Uuid::from_bytes(*id).to_string();
    from_value("Remove", serde_json::json!({ "id": id })).expect("a formatted uuid always parses")
}
