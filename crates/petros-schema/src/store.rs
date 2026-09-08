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

/// A value that can be a column, in both directions.
pub trait Cell: Sized {
    fn to_value(&self) -> Value;
    fn from_value(v: &Value) -> Option<Self>;
}

impl Cell for i64 {
    fn to_value(&self) -> Value {
        Value::Int(*self)
    }
    fn from_value(v: &Value) -> Option<Self> {
        v.as_int()
    }
}

impl Cell for bool {
    fn to_value(&self) -> Value {
        Value::Int(*self as i64)
    }
    fn from_value(v: &Value) -> Option<Self> {
        v.as_bool()
    }
}

impl Cell for String {
    fn to_value(&self) -> Value {
        Value::Text(self.clone())
    }
    fn from_value(v: &Value) -> Option<Self> {
        v.as_text().map(str::to_string)
    }
}

/// Sixteen bytes in the log and in SQLite. A `Vec<u8>` here rather than a uuid
/// type, so this crate stays free of dependencies.
impl Cell for Vec<u8> {
    fn to_value(&self) -> Value {
        Value::Blob(self.clone())
    }
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
pub enum ColumnTy {
    Blob,
    Text,
    Int,
    Bool,
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
    /// A checked statement. Use `petros_sql::exec!`.
    fn exec(&mut self, sql: &str, params: &[Value]);
    /// A checked query, decoding each column as the type given.
    /// Use `petros_sql::query!`.
    fn query(&mut self, sql: &str, params: &[Value], types: &[ColumnTy]) -> Vec<Vec<Value>>;
}

/// So a `&mut Store` is a `Store`, and a helper taking one can pass it on.
impl<S: Store + ?Sized> Store for &mut S {
    fn exec(&mut self, sql: &str, params: &[Value]) {
        (**self).exec(sql, params)
    }
    fn query(&mut self, sql: &str, params: &[Value], types: &[ColumnTy]) -> Vec<Vec<Value>> {
        (**self).query(sql, params, types)
    }
}
