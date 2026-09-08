//! The typed store, backed by SQLite.
//!
//! `apply` sees [`petros_schema::Store`] — typed columns, typed rows, no SQL.
//! This is where that becomes SQL, once, with **bound parameters** rather than
//! inlined literals. It is the only place in a Petros app that builds a
//! statement, so it is the only place that can get the escaping wrong.
//!
//! Identifiers are interpolated and values never are. Table and column names
//! come from a `tables!` declaration — they are program text, not input — and
//! every value goes through `push_bind_param`, which is why there is no
//! escaping routine here at all.

use diesel::connection::LoadConnection;
use diesel::query_builder::{AstPass, Query, QueryFragment, QueryId};
use diesel::row::{Field, Row as _};
use diesel::sql_types::{BigInt, Binary, Text, Untyped};
use diesel::sqlite::Sqlite;
use diesel::{QueryResult, RunQueryDsl};
use petros_schema::{ColumnTy, Value};

use crate::Connection;

/// A [`petros_schema::Store`] over a real SQLite connection.
///
/// What the server and every linked peer hand to `apply`. The phone's
/// equivalent lives on the far side of the wasm ABI and answers the same four
/// questions.
pub struct SqliteStore<'a>(pub &'a mut Connection);

impl std::fmt::Debug for SqliteStore<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SqliteStore")
    }
}

impl petros_schema::Store for SqliteStore<'_> {
    fn exec(&mut self, sql: &str, params: &[Value]) {
        let _ = bind(sql, params).execute(self.0);
    }

    fn query(&mut self, sql: &str, params: &[Value], types: &[ColumnTy]) -> Vec<Vec<Value>> {
        rows(self.0, bind(sql, params), types)
    }
}

/// Split a checked statement on its placeholders and bind a value at each.
///
/// The SQL arrives already verified — `petros-sql` prepared it against this
/// schema at build time and counted the placeholders — so the only work here is
/// binding.
fn bind(sql: &str, params: &[Value]) -> Sql {
    let mut q = Sql::new();
    let mut rest = sql;
    for value in params {
        match rest.find('?') {
            Some(i) => {
                q.sql(&rest[..i]);
                q.bind(value.clone());
                rest = &rest[i + 1..];
            }
            None => break,
        }
    }
    q.sql(rest);
    q
}

/// Run a query and read each column as the type the checker reported.
///
/// Diesel wants a static type per column and there is not one here, so this
/// drops to the row API. The types are not guessed: `petros-sql` asked SQLite
/// what each column is declared as, at build time, and passed the answer in.
fn rows(conn: &mut Connection, q: Sql, types: &[ColumnTy]) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    let Ok(cursor) = LoadConnection::load(conn, q) else {
        return out;
    };
    for row in cursor {
        let Ok(row) = row else { return out };
        let mut values = Vec::with_capacity(types.len());
        for (i, ty) in types.iter().enumerate() {
            let Some(field) = row.get(i) else { break };
            values.push(match field.value() {
                None => Value::Null,
                Some(mut raw) => match ty {
                    ColumnTy::Blob => Value::Blob(raw.read_blob().to_vec()),
                    ColumnTy::Text => Value::Text(raw.read_text().to_string()),
                    ColumnTy::Int | ColumnTy::Bool => Value::Int(raw.read_long()),
                },
            });
        }
        out.push(values);
    }
    out
}

// ------------------------------------------------- a query built at runtime

/// SQL text and bound values, interleaved in order.
#[derive(Debug)]
struct Sql {
    parts: Vec<Part>,
}

#[derive(Debug)]
enum Part {
    Sql(String),
    Bind(Value),
}

impl Sql {
    fn new() -> Self {
        Sql { parts: Vec::new() }
    }
    fn sql(&mut self, text: &str) {
        self.parts.push(Part::Sql(text.to_string()));
    }
    fn bind(&mut self, value: Value) {
        self.parts.push(Part::Bind(value));
    }
}

impl QueryFragment<Sqlite> for Sql {
    fn walk_ast<'b>(&'b self, mut out: AstPass<'_, 'b, Sqlite>) -> QueryResult<()> {
        for part in &self.parts {
            match part {
                Part::Sql(text) => out.push_sql(text),
                Part::Bind(Value::Int(i)) => out.push_bind_param::<BigInt, _>(i)?,
                Part::Bind(Value::Text(s)) => out.push_bind_param::<Text, _>(s)?,
                Part::Bind(Value::Blob(b)) => out.push_bind_param::<Binary, _>(b)?,
                // Nothing to bind, and no type to bind it as.
                Part::Bind(Value::Null) => out.push_sql("NULL"),
            }
        }
        Ok(())
    }
}

impl QueryId for Sql {
    type QueryId = ();
    // Built fresh every time, so it must not be looked up in the prepared
    // statement cache by type.
    const HAS_STATIC_QUERY_ID: bool = false;
}

impl Query for Sql {
    type SqlType = Untyped;
}

impl RunQueryDsl<Connection> for Sql {}
