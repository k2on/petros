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
use petros_schema::{Backend, ColumnTy, TableDef, Value, Write};

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

impl Backend for SqliteStore<'_> {
    fn get(&mut self, table: &TableDef, key: &[Value]) -> Option<Vec<Value>> {
        let mut q = Sql::new();
        q.sql(&format!(
            "SELECT {} FROM {}",
            columns(table),
            ident(table.name)
        ));
        where_key(&mut q, table, key);
        rows(self.0, q, table).into_iter().next()
    }

    fn scan(&mut self, table: &TableDef) -> Vec<Vec<Value>> {
        let mut q = Sql::new();
        // Explicit order, always: SQLite's natural order is not a contract and
        // two replicas replaying the same log have to see the same sequence.
        q.sql(&format!(
            "SELECT {} FROM {} ORDER BY {}",
            columns(table),
            ident(table.name),
            table
                .key
                .iter()
                .map(|k| ident(k))
                .collect::<Vec<_>>()
                .join(", ")
        ));
        rows(self.0, q, table)
    }

    fn max_int(&mut self, table: &TableDef, column: &str) -> i64 {
        let mut q = Sql::new();
        q.sql(&format!(
            "SELECT COALESCE(MAX({}), 0) FROM {}",
            ident(column),
            ident(table.name)
        ));
        let one = TableDef {
            name: table.name,
            columns: &["v"],
            types: &[ColumnTy::Int],
            key: &["v"],
        };
        rows(self.0, q, &one)
            .first()
            .and_then(|r| r.first())
            .and_then(Value::as_int)
            .unwrap_or(0)
    }

    fn exec(&mut self, sql: &str, params: &[Value]) {
        // The SQL arrives already checked — `petros-sql` prepared it against
        // this schema at build time — so the only work here is binding.
        let mut q = Sql::new();
        let mut rest = sql;
        for value in params {
            match rest.find('?') {
                Some(i) => {
                    q.sql(&rest[..i]);
                    q.bind(value.clone());
                    rest = &rest[i + 1..];
                }
                // More values than placeholders. `petros-sql` makes this
                // unreachable from a checked call site.
                None => break,
            }
        }
        q.sql(rest);
        let _ = q.execute(self.0);
    }

    fn write(&mut self, changes: &[Write]) {
        for change in changes {
            let mut q = Sql::new();
            match change {
                // `OR REPLACE` because a mutation redelivered has to be a
                // no-op, not a constraint violation.
                Write::Put { table, row } => {
                    q.sql(&format!(
                        "INSERT OR REPLACE INTO {} ({}) VALUES (",
                        ident(table.name),
                        columns(table)
                    ));
                    for (i, value) in row.iter().enumerate() {
                        if i > 0 {
                            q.sql(", ");
                        }
                        q.bind(value.clone());
                    }
                    q.sql(")");
                }
                Write::Delete { table, key } => {
                    q.sql(&format!("DELETE FROM {}", ident(table.name)));
                    where_key(&mut q, table, key);
                }
            }
            let _ = q.execute(self.0);
        }
    }
}

fn where_key(q: &mut Sql, table: &TableDef, key: &[Value]) {
    for (i, (name, value)) in table.key.iter().zip(key).enumerate() {
        q.sql(if i == 0 { " WHERE " } else { " AND " });
        q.sql(&format!("{} = ", ident(name)));
        q.bind(value.clone());
    }
}

fn columns(table: &TableDef) -> String {
    table
        .columns
        .iter()
        .map(|c| ident(c))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Quote an identifier. These come from a `tables!` declaration rather than
/// from anyone's input, so this is belt and braces — but a column called
/// `order` should not need a special case.
fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// Run a query and read the rows back against a table's declared types.
///
/// Diesel wants a static type per column and there is not one here, so this
/// drops to the row API: every column is read as the type the declaration says
/// it is, which is the same list the writes bind against.
fn rows(conn: &mut Connection, q: Sql, table: &TableDef) -> Vec<Vec<Value>> {
    let mut out = Vec::new();
    let Ok(cursor) = LoadConnection::load(conn, q) else {
        return out;
    };
    for row in cursor {
        let Ok(row) = row else { return out };
        let mut values = Vec::with_capacity(table.columns.len());
        for (i, ty) in table.types.iter().enumerate() {
            let Some(field) = row.get(i) else { break };
            let value = match field.value() {
                None => Value::Null,
                Some(mut raw) => match ty {
                    ColumnTy::Blob => Value::Blob(raw.read_blob().to_vec()),
                    ColumnTy::Text => Value::Text(raw.read_text().to_string()),
                    ColumnTy::Int | ColumnTy::Bool => Value::Int(raw.read_long()),
                },
            };
            values.push(value);
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
