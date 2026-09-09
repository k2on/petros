//! The store, as a mutation sees it: two methods, and no SQL builder.
//!
//! `apply` used to take a `Host` of three methods that accepted SQL strings, so
//! every mutation built its own statement with `format!` and inlined its own
//! literals. That gave up every compile-time check on the entire write path:
//! rename a column and nothing complains until a row goes missing on somebody's
//! phone.
//!
//! The fix is not a typed query builder. It is to keep the SQL and check it —
//! `petros_sql::exec!` and `petros_sql::query!` prepare each statement against
//! the app's schema at build time, using SQLite itself as the judge. What
//! reaches this trait is already verified, which is why the trait can be this
//! small and why it crosses a wasm boundary without generics.

/// A value a column can hold.
///
/// Deliberately small, and deliberately without floats: `apply` must not branch
/// on one, and a float in a key makes ordering incoherent the moment a `NaN`
/// appears.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Value {
    Null,
    Int(i64),
    Text(String),
    Blob(Vec<u8>),
}

impl Value {
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Value::Text(s) => Some(s),
            _ => None,
        }
    }
    pub fn as_blob(&self) -> Option<&[u8]> {
        match self {
            Value::Blob(b) => Some(b),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        self.as_int().map(|i| i != 0)
    }
}

/// A value a column can be decoded into.
///
/// Only one direction. Binding is [`Bind`], which is a separate trait because
/// `&str` can be bound and cannot be decoded into — there is nothing for the
/// borrow to point at.
pub trait Cell: Sized {
    fn from_value(v: &Value) -> Option<Self>;
}

/// A value that can be bound to a placeholder.
///
/// Implemented one type at a time rather than blanket, so `&str` and `&[u8]`
/// can be bound without an owned copy at every call site.
pub trait Bind {
    fn to_value(&self) -> Value;
}

impl Bind for i64 {
    fn to_value(&self) -> Value {
        Value::Int(*self)
    }
}
impl Bind for i32 {
    fn to_value(&self) -> Value {
        Value::Int(*self as i64)
    }
}
impl Bind for bool {
    fn to_value(&self) -> Value {
        Value::Int(*self as i64)
    }
}
impl Bind for str {
    fn to_value(&self) -> Value {
        Value::Text(self.to_string())
    }
}
impl Bind for String {
    fn to_value(&self) -> Value {
        Value::Text(self.clone())
    }
}
impl Bind for [u8] {
    fn to_value(&self) -> Value {
        Value::Blob(self.to_vec())
    }
}
impl Bind for Vec<u8> {
    fn to_value(&self) -> Value {
        Value::Blob(self.clone())
    }
}
impl Bind for Value {
    fn to_value(&self) -> Value {
        self.clone()
    }
}
impl<T: Bind + ?Sized> Bind for &T {
    fn to_value(&self) -> Value {
        (**self).to_value()
    }
}

impl Cell for i64 {
    fn from_value(v: &Value) -> Option<Self> {
        v.as_int()
    }
}

impl Cell for bool {
    fn from_value(v: &Value) -> Option<Self> {
        v.as_bool()
    }
}

impl Cell for String {
    fn from_value(v: &Value) -> Option<Self> {
        v.as_text().map(str::to_string)
    }
}

/// Sixteen bytes in the log and in SQLite. A `Vec<u8>` here rather than a uuid
/// type, so this crate stays free of dependencies.
impl Cell for Vec<u8> {
    fn from_value(v: &Value) -> Option<Self> {
        v.as_blob().map(<[u8]>::to_vec)
    }
}

/// A column's storage type. Distinct from [`crate::Ty`], which is the type of
/// a *mutation argument* — the two overlap but are not the same list, and a
/// column is not always something a caller may pass.
///
/// What a backend needs to bind a value and read one back without knowing the
/// Rust type behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum ColumnTy {
    Blob,
    Text,
    Int,
    Bool,
}

/// A column's place in a table, for the dynamic side of the boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableDef {
    pub name: &'static str,
    /// Column names in declaration order. A row is always this shape.
    pub columns: &'static [&'static str],
    pub types: &'static [ColumnTy],
    /// The primary key's columns, a subset of the above.
    pub key: &'static [&'static str],
}

/// A table, as `petros_sql::tables!` generates it from `schema.sql`.
pub trait Table: Sized {
    const DEF: TableDef;
    fn to_row(&self) -> Vec<Value>;
    fn from_row(row: &[Value]) -> Option<Self>;
    /// This row's key, in `DEF.key` order.
    fn key(&self) -> Vec<Value>;
}

/// What a write did, at the granularity a view can be maintained from.
///
/// The reason typed writes are back. `exec!` told us a statement ran; this says
/// which rows moved and what they were — which is exactly what an incremental
/// view needs, and what SQL could never give us without a hook Diesel does not
/// expose.
///
/// `Edit` carries the old row as well as the new, because a view that sorts or
/// filters on a column has to know what the value *was* to know where the row
/// was.
#[derive(Debug, Clone, PartialEq)]
pub enum Change {
    Add {
        table: String,
        row: Vec<Value>,
    },
    Remove {
        table: String,
        row: Vec<Value>,
    },
    Edit {
        table: String,
        old: Vec<Value>,
        new: Vec<Value>,
    },
}

/// The database, as a mutation sees it.
///
/// Two methods. Not because a domain needs little, but because the SQL is
/// checked somewhere else: `petros_sql::exec!` and `petros_sql::query!` prepare
/// your statement against the app's schema at build time, so what arrives here
/// is a string whose tables, columns, placeholder count and result types are
/// already known good.
///
/// That is the whole reason this is small. A typed query builder would have to
/// re-encode everything SQL already expresses — joins, aggregates, window
/// functions, `INSERT ... SELECT` — and would still be a second thing to keep
/// in step with the schema. Letting SQLite do the checking costs one trait with
/// two methods.
///
/// Calling either of these with a runtime string gives all of that up. Don't.
pub trait Store {
    /// Read rows matching a plan, in the plan's order.
    ///
    /// The source contract. A plan carries a filter, an order, a limit and a
    /// cursor, which is everything one SQL statement needs — and everything an
    /// incrementally maintained view needs to seek when a delete opens a gap in
    /// a limit.
    fn fetch(&mut self, plan: &crate::Plan) -> Vec<Vec<Value>>;

    /// Write a row, and say what changed.
    ///
    /// `row` is in the table's declared column order. Both sides of a wasm
    /// boundary learn that order from `schema.sql` — the guest through
    /// `tables!`, the host by asking the database — so only the name has to
    /// cross.
    ///
    /// Reads the old row first, so an overwrite reports [`Change::Edit`] with
    /// both versions rather than an add that a view cannot place. That is one
    /// extra point lookup per write, against an index.
    /// Write a row, reporting a refusal rather than swallowing one.
    ///
    /// A constraint the database enforces — a foreign key, a check — is a
    /// *deterministic verdict*: every replica applying this entry reaches it,
    /// so it belongs in the same channel as a mutation's own refusal and not in
    /// a log nobody reads. This returned `()` at first, and a write that a
    /// foreign key refused simply did not happen, in silence.
    fn put_row(&mut self, table: &str, row: &[Value]) -> Result<(), String>;

    /// Remove a row by key, and say what changed. A key that is not there is a
    /// no-op and no change: an entry earlier in the log may have removed it.
    /// Remove a row, reporting a refusal — a foreign key with `ON DELETE
    /// RESTRICT` is the usual one.
    fn delete_row(&mut self, table: &str, key: &[Value]) -> Result<(), String>;

    /// One row by key.
    fn get_row(&mut self, table: &str, key: &[Value]) -> Option<Vec<Value>>;

    /// What has been written since this was last called, in order.
    ///
    /// Drained rather than accumulated, because the caller that takes them owns
    /// them: they go to a view, or they are dropped when a savepoint rolls back.
    fn take_changes(&mut self) -> Vec<Change>;
}

/// The typed half, in terms of the four above.
pub trait Rows: Store {
    /// Run a query and decode its rows.
    fn select<T: Table>(&mut self, query: crate::Query<T>) -> Vec<T> {
        self.fetch(query.plan())
            .iter()
            .filter_map(|row| T::from_row(row))
            .collect()
    }

    /// Run a query and hang a related table off each row.
    ///
    /// Two statements, not one per parent: the children are fetched for the
    /// whole page with a single `IN`, then grouped. The result is a tree — a
    /// row with its children — rather than a flat join that repeats the parent
    /// and leaves the caller to regroup.
    fn select_with<P: Table, C: Table>(
        &mut self,
        query: crate::Query<P>,
        rel: crate::Relation<P, C>,
        children: crate::Query<C>,
    ) -> Vec<crate::With<P, C>> {
        let parents: Vec<P> = self.select(query);
        let Some(at) = P::DEF.columns.iter().position(|c| *c == rel.from) else {
            return Vec::new();
        };
        let keys: Vec<Value> = parents.iter().map(|p| p.to_row()[at].clone()).collect();
        if keys.is_empty() {
            return Vec::new();
        }

        let mut plan = children.into_plan();
        let constraint = crate::Node::In {
            column: rel.to.to_string(),
            values: keys,
        };
        plan.filter = Some(match plan.filter.take() {
            Some(existing) => crate::Node::All(vec![existing, constraint]),
            None => constraint,
        });
        // A limit belongs to each parent's children, not to all of them at
        // once, and one statement cannot say that. Refusing beats quietly
        // truncating somebody else's list.
        plan.limit = None;

        let Some(back) = C::DEF.columns.iter().position(|c| *c == rel.to) else {
            return Vec::new();
        };
        let mut grouped: Vec<(Value, Vec<C>)> = Vec::new();
        for row in self.fetch(&plan) {
            let Some(child) = C::from_row(&row) else {
                continue;
            };
            let key = row[back].clone();
            match grouped.iter_mut().find(|(k, _)| *k == key) {
                Some((_, list)) => list.push(child),
                None => grouped.push((key, vec![child])),
            }
        }

        parents
            .into_iter()
            .map(|p| {
                let key = p.to_row()[at].clone();
                let related = grouped
                    .iter_mut()
                    .find(|(k, _)| *k == key)
                    .map(|(_, list)| std::mem::take(list))
                    .unwrap_or_default();
                crate::With { row: p, related }
            })
            .collect()
    }

    fn get<T: Table>(&mut self, key: &[Value]) -> Option<T> {
        self.get_row(T::DEF.name, key)
            .as_deref()
            .and_then(T::from_row)
    }

    fn exists<T: Table>(&mut self, key: &[Value]) -> bool {
        self.get_row(T::DEF.name, key).is_some()
    }

    fn put<T: Table>(&mut self, row: &T) -> Result<(), String> {
        self.put_row(T::DEF.name, &row.to_row())
    }

    fn delete<T: Table>(&mut self, key: &[Value]) -> Result<(), String> {
        self.delete_row(T::DEF.name, key)
    }
}

impl<S: Store + ?Sized> Rows for S {}

/// So a `&mut Store` is a `Store`, and a helper taking one can pass it on.
impl<S: Store + ?Sized> Store for &mut S {
    fn fetch(&mut self, plan: &crate::Plan) -> Vec<Vec<Value>> {
        (**self).fetch(plan)
    }
    fn put_row(&mut self, table: &str, row: &[Value]) -> Result<(), String> {
        (**self).put_row(table, row)
    }
    fn delete_row(&mut self, table: &str, key: &[Value]) -> Result<(), String> {
        (**self).delete_row(table, key)
    }
    fn get_row(&mut self, table: &str, key: &[Value]) -> Option<Vec<Value>> {
        (**self).get_row(table, key)
    }
    fn take_changes(&mut self) -> Vec<Change> {
        (**self).take_changes()
    }
}

// ------------------------------------------------------- across the boundary

/// One question for the store, as it crosses into a sandbox.
///
/// The whole reason the trait above is two non-generic methods: this is what a
/// wasm guest can send. Both variants carry SQL that was checked at build time,
/// so nothing here needs to validate anything — it needs to bind and answer.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Request {
    Fetch { plan: crate::Plan },
    Get { table: String, key: Vec<Value> },
    Put { table: String, row: Vec<Value> },
    Delete { table: String, key: Vec<Value> },
}
