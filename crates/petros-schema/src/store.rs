//! The store, as a mutation sees it.
//!
//! Two traits, and the split is the whole point. [`Store`] is generic and typed
//! — what `apply` calls, where a column belongs to a table and a value has a
//! type the compiler checks. [`Backend`] is dynamic — table names, column names
//! and [`Value`]s — because it has to cross a wasm boundary, and generics do
//! not cross boundaries.
//!
//! A blanket impl bridges them, so a backend implements four dynamic methods
//! and gets the whole typed surface. SQL, where there is any, is built inside
//! one backend and never by a domain.
//!
//! # Why this exists
//!
//! `apply` used to take a `Host` of three methods that accepted SQL strings, so
//! every mutation built its own statement with `format!` and inlined its own
//! literals. That gave up Diesel's `check_for_backend` on the entire write
//! path: rename a column and nothing complains until a row goes missing on
//! somebody's phone.

use core::marker::PhantomData;

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
/// What a backend needs to bind and read a value without
/// knowing the Rust type behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnTy {
    Blob,
    Text,
    Int,
    Bool,
}

/// Everything the dynamic side needs to know about a table.
///
/// Carried by value into every [`Backend`] call, so a backend can bind
/// parameters and read rows without generics — which is what lets the same
/// trait be implemented on the far side of a wasm boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableDef {
    pub name: &'static str,
    /// Column names in declaration order. A row is always this shape.
    pub columns: &'static [&'static str],
    pub types: &'static [ColumnTy],
    /// The primary key's columns, a subset of the above.
    pub key: &'static [&'static str],
}

impl TableDef {
    /// The type of a column, for a backend that has only its name.
    pub fn ty(&self, column: &str) -> Option<ColumnTy> {
        self.columns
            .iter()
            .position(|c| *c == column)
            .map(|i| self.types[i])
    }
}

/// A table, as `tables!` generates it.
pub trait Table: Sized {
    const DEF: TableDef;

    fn to_row(&self) -> Vec<Value>;
    fn from_row(row: &[Value]) -> Option<Self>;
    /// This row's key, in `DEF.key` order.
    fn key(&self) -> Vec<Value>;
}

/// One column of one table, carrying both in the type.
///
/// `store.max(song::POS)` will not compile against `favorite`, and will not
/// compile if `pos` is not an integer. That is the whole point of the exercise.
pub struct Column<T, V> {
    pub name: &'static str,
    marker: PhantomData<fn() -> (T, V)>,
}

impl<T, V> Column<T, V> {
    pub const fn new(name: &'static str) -> Self {
        Column {
            name,
            marker: PhantomData,
        }
    }
}

impl<T, V> Clone for Column<T, V> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T, V> Copy for Column<T, V> {}

/// One change to the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Write {
    /// Insert, or replace a row with the same key.
    Put {
        table: TableDef,
        row: Vec<Value>,
    },
    Delete {
        table: TableDef,
        key: Vec<Value>,
    },
}

/// What a backend actually implements: four methods, no generics, no SQL in
/// the signature.
///
/// Dynamic because this is what crosses into wasm, where a generic method has
/// nothing to monomorphise against. Everything typed is built on top.
pub trait Backend {
    fn get(&mut self, table: &TableDef, key: &[Value]) -> Option<Vec<Value>>;
    /// Every row of a table, in primary-key order. Explicit, because SQLite's
    /// natural order is not a contract and two replicas must agree.
    fn scan(&mut self, table: &TableDef) -> Vec<Vec<Value>>;
    /// The largest value of an integer column, or zero for an empty table —
    /// which is what `MAX(pos)` over nothing should mean here.
    fn max_int(&mut self, table: &TableDef, column: &str) -> i64;
    fn write(&mut self, changes: &[Write]);

    /// Run a statement the typed surface cannot express, with its values bound.
    ///
    /// The escape hatch, and a narrow one: it takes SQL that something has
    /// already checked. `petros-sql` is what checks it — at build time, by
    /// preparing it against a real SQLite that has this app's schema — so by
    /// the time a string reaches here its tables, its columns and its parameter
    /// count are known good. Reaching this directly with a runtime string gives
    /// all of that up.
    ///
    /// Exists because set operations are the one thing the typed surface loses:
    /// `INSERT ... SELECT` over a whole table is one statement here and a scan
    /// plus a write per row otherwise.
    fn exec(&mut self, sql: &str, params: &[Value]);
}

/// What `apply` sees. Typed, and implemented for every [`Backend`].
pub trait Store {
    fn get<T: Table>(&mut self, key: &[Value]) -> Option<T>;
    fn exists<T: Table>(&mut self, key: &[Value]) -> bool;
    fn scan<T: Table>(&mut self) -> Vec<T>;
    fn max<T: Table>(&mut self, column: Column<T, i64>) -> i64;
    fn put<T: Table>(&mut self, row: &T);
    fn delete<T: Table>(&mut self, key: &[Value]);
    /// Several changes as one call. Worth reaching for in a loop: on the phone
    /// each of these is a boundary crossing, and host frames accumulate inside
    /// the interpreter rather than unwinding between calls.
    fn write_all(&mut self, changes: &[Write]);
    /// A checked statement. Use `petros_sql::exec!` rather than calling this.
    fn exec(&mut self, sql: &str, params: &[Value]);
}

impl<B: Backend> Store for B {
    fn get<T: Table>(&mut self, key: &[Value]) -> Option<T> {
        Backend::get(self, &T::DEF, key)
            .as_deref()
            .and_then(T::from_row)
    }

    fn exists<T: Table>(&mut self, key: &[Value]) -> bool {
        Backend::get(self, &T::DEF, key).is_some()
    }

    fn scan<T: Table>(&mut self) -> Vec<T> {
        Backend::scan(self, &T::DEF)
            .iter()
            .filter_map(|row| T::from_row(row))
            .collect()
    }

    fn max<T: Table>(&mut self, column: Column<T, i64>) -> i64 {
        Backend::max_int(self, &T::DEF, column.name)
    }

    fn put<T: Table>(&mut self, row: &T) {
        Backend::write(
            self,
            &[Write::Put {
                table: T::DEF,
                row: row.to_row(),
            }],
        );
    }

    fn delete<T: Table>(&mut self, key: &[Value]) {
        Backend::write(
            self,
            &[Write::Delete {
                table: T::DEF,
                key: key.to_vec(),
            }],
        );
    }

    fn write_all(&mut self, changes: &[Write]) {
        Backend::write(self, changes);
    }

    fn exec(&mut self, sql: &str, params: &[Value]) {
        Backend::exec(self, sql, params);
    }
}

/// Declare the app's tables once, and get the row types, the typed columns and
/// the DDL from the same place.
///
/// The `CREATE TABLE` used to be written by hand beside a Diesel `table!` that
/// described it, with nothing checking the two agreed — `docs/decisions.md`
/// called that out as the one place the ORM gave less than it looked like it
/// should. Both now come from here.
///
/// The key clause repeats its columns' types so `key_of` can be typed. A
/// generated check makes the repetition safe: disagree with the column
/// declaration and it does not compile.
///
/// ```
/// petros_schema::tables! {
///     /// A song in the library.
///     Song "song" key (id: Blob) {
///         id: Blob,
///         title: Text,
///         pos: Int,
///     }
/// }
/// use petros_schema::{Store, Table};
/// assert_eq!(Song::DEF.name, "song");
/// assert_eq!(Song::pos.name, "pos");
/// assert!(Song::ddl().contains("PRIMARY KEY (id)"));
/// assert_eq!(Song::key_of(&vec![1u8]), vec![petros_schema::Value::Blob(vec![1])]);
/// ```
#[macro_export]
macro_rules! tables {
    ($(
        $(#[$meta:meta])*
        $name:ident $table:literal key ($($k:ident : $kty:ident),+ $(,)?) {
            $($col:ident : $ty:ident),* $(,)?
        }
    )*) => {
        $(
            $(#[$meta])*
            #[derive(Debug, Clone, PartialEq, Eq)]
            pub struct $name {
                $(pub $col: $crate::tables!(@rust $ty),)*
            }

            // The key clause names its columns' types a second time. This is
            // what stops that from being a place they can disagree.
            const _: () = {
                #[allow(unused)]
                fn key_columns_match_the_declaration(row: &$name) {
                    $(let _: &$crate::tables!(@rust $kty) = &row.$k;)+
                }
            };

            impl $name {
                /// A key, for `get`, `exists` and `delete`.
                pub fn key_of($($k: &$crate::tables!(@rust $kty)),+)
                    -> ::std::vec::Vec<$crate::Value>
                {
                    ::std::vec![$($crate::Cell::to_value($k)),+]
                }

                /// `CREATE TABLE` for this table. The other half of the
                /// declaration above, so the two cannot drift.
                pub fn ddl() -> ::std::string::String {
                    let columns: ::std::vec::Vec<::std::string::String> = ::std::vec![
                        $(::std::format!("{} {}", ::core::stringify!($col), $crate::tables!(@sql $ty))),*
                    ];
                    ::std::format!(
                        "CREATE TABLE IF NOT EXISTS {} ({}, PRIMARY KEY ({}));",
                        $table,
                        columns.join(", "),
                        [$(::core::stringify!($k)),+].join(", "),
                    )
                }

                // One constant per column, carrying the table in its type. This
                // is what makes `store.max(Song::pos)` refuse to compile against
                // another table, or against a column that is not an integer.
                $(
                    #[allow(non_upper_case_globals)]
                    pub const $col: $crate::Column<$name, $crate::tables!(@rust $ty)> =
                        $crate::Column::new(::core::stringify!($col));
                )*
            }

            impl $crate::Table for $name {
                const DEF: $crate::TableDef = $crate::TableDef {
                    name: $table,
                    columns: &[$(::core::stringify!($col)),*],
                    types: &[$($crate::tables!(@ty $ty)),*],
                    key: &[$(::core::stringify!($k)),+],
                };

                fn to_row(&self) -> ::std::vec::Vec<$crate::Value> {
                    ::std::vec![$($crate::Cell::to_value(&self.$col)),*]
                }

                fn from_row(row: &[$crate::Value]) -> ::core::option::Option<Self> {
                    let mut it = row.iter();
                    ::core::option::Option::Some($name {
                        $($col: $crate::Cell::from_value(it.next()?)?,)*
                    })
                }

                fn key(&self) -> ::std::vec::Vec<$crate::Value> {
                    ::std::vec![$($crate::Cell::to_value(&self.$k)),+]
                }
            }
        )*

        /// Every table's DDL, for `App::migrate`.
        pub fn ddl() -> ::std::string::String {
            [$($name::ddl()),*].join("\n")
        }
    };

    (@rust Blob) => { ::std::vec::Vec<u8> };
    (@rust Text) => { ::std::string::String };
    (@rust Int)  => { i64 };
    (@rust Bool) => { bool };

    (@ty Blob) => { $crate::ColumnTy::Blob };
    (@ty Text) => { $crate::ColumnTy::Text };
    (@ty Int)  => { $crate::ColumnTy::Int };
    (@ty Bool) => { $crate::ColumnTy::Bool };

    (@sql Blob) => { "BLOB NOT NULL" };
    (@sql Text) => { "TEXT NOT NULL" };
    (@sql Int)  => { "BIGINT NOT NULL" };
    (@sql Bool) => { "BOOL NOT NULL" };
}
