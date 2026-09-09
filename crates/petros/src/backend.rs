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
use petros_schema::{Change, ColumnTy, Value};

use crate::Connection;

/// A [`petros_schema::Store`] over a real SQLite connection.
///
/// What the server and every linked peer hand to `apply`. The phone's
/// equivalent lives on the far side of the wasm ABI and answers the same four
/// questions.
pub struct SqliteStore<'a> {
    conn: &'a mut Connection,
    /// What has been written since the last drain. See `Store::take_changes`.
    changes: Vec<Change>,
    /// Table shapes, asked of the database once each.
    ///
    /// Only the table's name crosses a wasm boundary; its columns, their types
    /// and its key are read from the live schema. Both sides therefore agree by
    /// construction rather than by keeping two declarations in step.
    shapes: std::collections::HashMap<String, Shape>,
}

/// A table as the database describes it.
struct Shape {
    name: String,
    columns: Vec<String>,
    types: Vec<ColumnTy>,
    key: Vec<String>,
}

impl<'a> SqliteStore<'a> {
    pub fn new(conn: &'a mut Connection) -> Self {
        SqliteStore {
            conn,
            changes: Vec::new(),
            shapes: std::collections::HashMap::new(),
        }
    }

    /// Ask the database what a table looks like, once.
    fn shape(&mut self, table: &str) -> Option<&Shape> {
        if !self.shapes.contains_key(table) {
            #[derive(diesel::QueryableByName)]
            struct Info {
                #[diesel(sql_type = diesel::sql_types::Text)]
                name: String,
                #[diesel(sql_type = diesel::sql_types::Text)]
                #[diesel(column_name = "type_")]
                ty: String,
                #[diesel(sql_type = diesel::sql_types::BigInt)]
                pk: i64,
            }
            let rows: Vec<Info> = diesel::sql_query(format!(
                "SELECT name, type AS type_, pk FROM pragma_table_info({}) ORDER BY cid",
                lit(table)
            ))
            .load(&mut *self.conn)
            .ok()?;
            if rows.is_empty() {
                return None;
            }
            let shape = Shape {
                name: table.to_string(),
                columns: rows.iter().map(|r| r.name.clone()).collect(),
                types: rows.iter().map(|r| column_ty(&r.ty)).collect(),
                key: rows
                    .iter()
                    .filter(|r| r.pk > 0)
                    .map(|r| r.name.clone())
                    .collect(),
            };
            self.shapes.insert(table.to_string(), shape);
        }
        self.shapes.get(table)
    }
}

/// SQLite's declared type, as the store's four. Anything unrecognised is an
/// integer, which is what SQLite itself does with an unknown affinity.
fn column_ty(decl: &str) -> ColumnTy {
    let d = decl.trim().to_ascii_uppercase();
    match d.as_str() {
        "BLOB" => ColumnTy::Blob,
        "TEXT" => ColumnTy::Text,
        "BOOL" | "BOOLEAN" => ColumnTy::Bool,
        _ => ColumnTy::Int,
    }
}

/// A string as a SQL literal. Only ever a table name from `schema.sql`, and
/// only because `pragma_table_info` is a function rather than a statement that
/// takes a bound parameter.
fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

impl std::fmt::Debug for SqliteStore<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SqliteStore")
    }
}

impl petros_schema::Store for SqliteStore<'_> {
    fn query(&mut self, sql: &str, params: &[Value], types: &[ColumnTy]) -> Vec<Vec<Value>> {
        rows(self.conn, bind(sql, params), types)
    }

    fn get_row(&mut self, table: &str, key: &[Value]) -> Option<Vec<Value>> {
        let shape = self.shape(table)?;
        let mut q = Sql::new();
        q.sql(&format!(
            "SELECT {} FROM {}",
            columns(&shape.columns),
            ident(&shape.name)
        ));
        where_key(&mut q, &shape.key, key);
        let types = shape.types.clone();
        rows(self.conn, q, &types).into_iter().next()
    }

    fn put_row(&mut self, table: &str, row: &[Value]) {
        let Some(shape) = self.shape(table) else {
            return;
        };
        let (name, columns_list) = (shape.name.clone(), shape.columns.clone());
        // The old row first. An overwrite has to report what it replaced, or a
        // view that sorts on a column cannot tell where the row used to be.
        let key: Vec<Value> = shape
            .key
            .iter()
            .filter_map(|k| shape.columns.iter().position(|c| c == k))
            .filter_map(|i| row.get(i).cloned())
            .collect();
        let old = self.get_row(table, &key);

        let mut q = Sql::new();
        q.sql(&format!(
            "INSERT OR REPLACE INTO {} ({}) VALUES (",
            ident(&name),
            columns(&columns_list)
        ));
        for (i, value) in row.iter().enumerate() {
            if i > 0 {
                q.sql(", ");
            }
            q.bind(value.clone());
        }
        q.sql(")");
        if q.execute(&mut *self.conn).is_err() {
            return;
        }
        self.changes.push(match old {
            Some(old) => Change::Edit {
                table: name,
                old,
                new: row.to_vec(),
            },
            None => Change::Add {
                table: name,
                row: row.to_vec(),
            },
        });
    }

    fn delete_row(&mut self, table: &str, key: &[Value]) {
        // Nothing to report if nothing was there, and a redelivered entry
        // removing a row twice is normal.
        let Some(old) = self.get_row(table, key) else {
            return;
        };
        let Some(shape) = self.shape(table) else {
            return;
        };
        let (name, key_cols) = (shape.name.clone(), shape.key.clone());
        let mut q = Sql::new();
        q.sql(&format!("DELETE FROM {}", ident(&name)));
        where_key(&mut q, &key_cols, key);
        if q.execute(&mut *self.conn).is_err() {
            return;
        }
        self.changes.push(Change::Remove {
            table: name,
            row: old,
        });
    }

    fn take_changes(&mut self) -> Vec<Change> {
        std::mem::take(&mut self.changes)
    }
}

fn where_key(q: &mut Sql, key_cols: &[String], key: &[Value]) {
    for (i, (name, value)) in key_cols.iter().zip(key).enumerate() {
        q.sql(if i == 0 { " WHERE " } else { " AND " });
        q.sql(&format!("{} = ", ident(name)));
        q.bind(value.clone());
    }
}

fn columns(cols: &[String]) -> String {
    cols.iter().map(|c| ident(c)).collect::<Vec<_>>().join(", ")
}

/// Quote an identifier. These come from `schema.sql` rather than from anyone's
/// input, so this is belt and braces — but a column called `order` should not
/// need a special case.
fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
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
