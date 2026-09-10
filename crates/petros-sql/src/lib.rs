//! Raw SQL in a mutation, checked before it ships.
//!
//! Reads are SQL, and checked. Writes are not: they go through the typed store,
//! because a view can only be maintained from changes it is told about and
//! `UPDATE … WHERE` says nothing about which rows moved. `INSERT ... SELECT` over a
//! whole table is one statement, and a scan plus a write per row otherwise,
//! which on a phone is a boundary crossing per row.
//!
//! So this is the escape hatch, and it is not an unchecked one.
//!
//! # How it is checked
//!
//! The same way sqlx does it, and for the same reason: **SQLite is a better SQL
//! parser than anything we would write.** At expansion time this opens an
//! in-memory database, applies the app's schema, and calls `prepare` on your
//! statement. A misspelled table, an unknown column, a syntax error or the
//! wrong number of `?` placeholders is a compile error carrying SQLite's own
//! message.
//!
//! What it emits is a `&'static str` and a `Vec<Value>` — no database handle,
//! no driver, nothing that needs SQLite present. That is why this works inside
//! a mutation compiled to `wasm32-unknown-unknown`, where sqlx cannot go: the
//! checking happens on the host toolchain at build time, and the module ships
//! only the result.
//!
//! # Where the schema comes from
//!
//! `PETROS_SCHEMA`, or `schema.sql` beside the crate's `Cargo.toml`. sqlx
//! answers the same question with a live `DATABASE_URL` or a checked-in
//! `.sqlx/` cache; this is the offline half of that idea.
//!
//! The file has to agree with the `tables!` declaration that generates the
//! DDL at runtime, and one line in a test is what holds them together:
//!
//! ```ignore
//! #[test]
//! fn the_schema_file_matches_the_declaration() {
//!     assert_eq!(include_str!("../schema.sql").trim(), ddl().trim());
//! }
//! ```

use proc_macro::TokenStream;
use proc_macro2::Ident;
use quote::{format_ident, quote};
use std::cell::RefCell;
use std::path::PathBuf;

thread_local! {
    /// The database the schema is read from, opened once per crate.
    ///
    /// A proc macro runs once for a whole crate, not once per call site, so
    /// this is opened and the schema applied a single time.
    static SCHEMA_DB: RefCell<Option<(String, rusqlite::Connection)>> =
        const { RefCell::new(None) };
}

/// Run `f` against a database holding `ddl`, opening one only if needed.
fn with_schema<T>(
    ddl: &str,
    f: impl FnOnce(&rusqlite::Connection) -> Result<T, String>,
) -> Result<T, String> {
    SCHEMA_DB.with(|cell| {
        let mut slot = cell.borrow_mut();
        let fresh = match slot.as_ref() {
            Some((cached, _)) => cached != ddl,
            None => true,
        };
        if fresh {
            let conn = rusqlite::Connection::open_in_memory()
                .map_err(|e| format!("could not open a database to read the schema: {e}"))?;
            conn.execute_batch(ddl)
                .map_err(|e| format!("the schema does not apply: {e}"))?;
            *slot = Some((ddl.to_string(), conn));
        }
        let (_, conn) = slot.as_ref().expect("just filled");
        f(conn)
    })
}

/// Where the schema is: `PETROS_SCHEMA`, or `schema.sql` beside `Cargo.toml`.
fn schema_path() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("PETROS_SCHEMA") {
        return Ok(PathBuf::from(path));
    }
    let root = std::env::var("CARGO_MANIFEST_DIR")
        .map_err(|_| "CARGO_MANIFEST_DIR is not set; this must run under cargo".to_string())?;
    Ok(PathBuf::from(root).join("schema.sql"))
}

/// Declare a struct per table, read out of the schema itself.
///
/// Takes nothing. It opens the same `schema.sql` every statement is checked
/// against, asks SQLite what is in it, and emits a row type and a
/// `petros_schema::Table` impl per table.
///
/// That is the point: the tables were described twice before — once as DDL and
/// once as a Rust declaration — with a test to hold them together. SQLite
/// already parsed the DDL to check the statements, so it can answer
/// `PRAGMA table_info` at the same time and there is nothing to keep in step.
///
/// ```ignore
/// petros_sql::tables!();          // schema.sql -> struct Song { … }, struct Favorite { … }
/// ```
#[proc_macro]
pub fn tables(_input: TokenStream) -> TokenStream {
    match expand_tables() {
        Ok(t) => t.into(),
        Err(message) => {
            let message = message.to_string();
            quote!(compile_error!(#message);).into()
        }
    }
}

/// The single-column primary key of a table, for a `REFERENCES` that named no
/// column.
fn primary_key(conn: &rusqlite::Connection, table: &str) -> Result<String, String> {
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({table})"))
        .map_err(|e| e.to_string())?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, i64>(5)?)))
        .map_err(|e| e.to_string())?;
    for row in rows {
        let (name, pk) = row.map_err(|e| e.to_string())?;
        if pk == 1 {
            return Ok(name);
        }
    }
    Err(format!(
        "`REFERENCES {table}` names no column and `{table}` has no primary key."
    ))
}

fn expand_tables() -> Result<proc_macro2::TokenStream, String> {
    let schema = schema_path()?;
    let ddl = std::fs::read_to_string(&schema)
        .map_err(|e| format!("cannot read the schema at {}: {e}", schema.display()))?;
    let schema_str = schema.to_string_lossy().into_owned();

    let tables = with_schema(&ddl, |conn| {
        let mut names: Vec<String> = Vec::new();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(|e| e.to_string())?;
        for name in rows {
            names.push(name.map_err(|e| e.to_string())?);
        }

        let mut out = Vec::new();
        for table in names {
            let mut columns = Vec::new();
            let mut stmt = conn
                .prepare(&format!("PRAGMA table_info({table})"))
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(1)?,   // name
                        r.get::<_, String>(2)?,   // declared type
                        r.get::<_, i64>(5)?,      // pk position, 0 for not-a-key
                        r.get::<_, i64>(3)? == 0, // nullable
                    ))
                })
                .map_err(|e| e.to_string())?;
            for row in rows {
                columns.push(row.map_err(|e| e.to_string())?);
            }
            // Relationships come from the DDL too. `PRAGMA foreign_key_list`
            // reports what `REFERENCES` declared, which means a relationship is
            // written once, in the schema, and both directions of it are
            // generated rather than typed.
            let mut keys = Vec::new();
            let mut stmt = conn
                .prepare(&format!("PRAGMA foreign_key_list({table})"))
                .map_err(|e| e.to_string())?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, i64>(1)?,            // seq, within a composite key
                        r.get::<_, String>(2)?,         // the table referenced
                        r.get::<_, String>(3)?,         // the column here
                        r.get::<_, Option<String>>(4)?, // the column there
                    ))
                })
                .map_err(|e| e.to_string())?;
            for row in rows {
                let (seq, parent, from, to) = row.map_err(|e| e.to_string())?;
                // A composite foreign key would need a relationship over
                // several columns, which `Relation` does not carry. Ignoring it
                // is better than generating half of one.
                if seq != 0 {
                    continue;
                }
                // An omitted `to` means the parent's primary key.
                let to = match to {
                    Some(to) => to,
                    None => primary_key(conn, &parent)?,
                };
                keys.push((parent, from, to));
            }

            out.push((table, columns, keys));
        }
        Ok(out)
    })?;

    // Both ends, indexed by the table the relationship is *read from*.
    let mut relations: std::collections::BTreeMap<String, Vec<(String, String, String, String)>> =
        std::collections::BTreeMap::new();
    for (child, _, keys) in &tables {
        for (parent, from, to) in keys {
            // Reading down: every favourite of this song.
            relations.entry(parent.clone()).or_default().push((
                child.clone(),
                to.clone(),
                child.clone(),
                from.clone(),
            ));
            // Reading up: the song of this favourite.
            relations.entry(child.clone()).or_default().push((
                parent.clone(),
                from.clone(),
                parent.clone(),
                to.clone(),
            ));
        }
    }

    let mut items = Vec::new();
    for (table, columns, _) in &tables {
        let (table, columns) = (table.clone(), columns.clone());
        if columns.is_empty() {
            continue;
        }
        let ty = format_ident!("{}", camel(&table));
        let mut fields = Vec::new();
        let mut names = Vec::new();
        let mut kinds = Vec::new();
        let mut key_names = Vec::new();
        let mut key_tys = Vec::new();
        for (name, decl, pk, null) in &columns {
            let ident = format_ident!("{}", name);
            let kind = column_kind(decl)?;
            let rust = optional(kind.rust(), *null);
            fields.push(quote! { pub #ident: #rust });
            names.push(name.clone());
            kinds.push(kind.token());
            if *pk > 0 {
                key_names.push(ident.clone());
                key_tys.push(rust);
            }
        }
        if key_names.is_empty() {
            return Err(format!(
                "table `{table}` has no primary key. A row the store can write \
                 has to be addressable, so every table needs one."
            ));
        }
        let idents: Vec<Ident> = names.iter().map(|n| format_ident!("{}", n)).collect();
        let column_consts: Vec<proc_macro2::TokenStream> = columns
            .iter()
            .map(|(name, decl, _, null)| {
                let ident = format_ident!("{}", name);
                let kind = column_kind(decl).expect("checked above");
                // The constant's value type follows the column's: comparing a
                // nullable column against a bare value would not compile, and
                // `Column::eq(None)` is how a caller asks for `IS NULL`.
                let rust = optional(kind.rust(), *null);
                let token = kind.token();
                quote! {
                    #[allow(non_upper_case_globals)]
                    pub const #ident: ::petros_schema::Column<Self, #rust> =
                        ::petros_schema::Column::new(#name, #token);
                }
            })
            .collect();
        // One constant per relationship this table can be read through.
        let mut relation_consts = Vec::new();
        for (name, from, far, to) in relations.get(&table).into_iter().flatten() {
            if names.contains(name) {
                return Err(format!(
                    "table `{table}` has both a column and a relationship named \
                     `{name}`. Rename the column, or the foreign key's table."
                ));
            }
            let ident = format_ident!("{}", name);
            let far_ty = format_ident!("{}", camel(far));
            let doc = format!("`{table}.{from}` to `{far}.{to}`, from `schema.sql`.");
            relation_consts.push(quote! {
                #[doc = #doc]
                #[allow(non_upper_case_globals)]
                pub const #ident: ::petros_schema::Relation<Self, #far_ty> =
                    ::petros_schema::Relation::new(#from, #to);
            });
        }

        let doc = format!("The `{table}` table, from `schema.sql`.");

        items.push(quote! {
            #[doc = #doc]
            #[derive(Debug, Clone, PartialEq)]
            pub struct #ty {
                #(#fields,)*
            }

            impl #ty {
                /// Every row of this table. The start of a read.
                pub fn all() -> ::petros_schema::Query<Self> {
                    ::petros_schema::all()
                }

                // One constant per column, carrying the table and the type. It
                // is what makes `Song::pos.eq("x")` a compile error, and
                // `Favorite::pos` unusable in a `Song` query.
                #(#column_consts)*

                #(#relation_consts)*

                /// A key, for `get` and `delete`.
                pub fn key_of(#(#key_names: &#key_tys),*) -> ::std::vec::Vec<::petros_schema::Value> {
                    ::std::vec![#(::petros_schema::Bind::to_value(#key_names)),*]
                }
            }

            // The leaf of a decoded tree: this table, with nothing under it.
            // Emitted per table rather than blanket over `Table`, because a
            // blanket impl would overlap the nesting one — nothing tells the
            // compiler a `With` will never be a `Table`.
            impl ::petros_schema::FromNode for #ty {
                const TABLE: &'static str = #table;

                fn from_node(node: &::petros_schema::Tree) -> ::core::option::Option<Self> {
                    <Self as ::petros_schema::Table>::from_row(&node.row)
                }
            }

            impl ::petros_schema::Table for #ty {
                const DEF: ::petros_schema::TableDef = ::petros_schema::TableDef {
                    name: #table,
                    columns: &[#(#names),*],
                    types: &[#(#kinds),*],
                    key: &[#(::core::stringify!(#key_names)),*],
                };

                fn to_row(&self) -> ::std::vec::Vec<::petros_schema::Value> {
                    ::std::vec![#(::petros_schema::Bind::to_value(&self.#idents)),*]
                }

                fn from_row(row: &[::petros_schema::Value]) -> ::core::option::Option<Self> {
                    let mut it = row.iter();
                    ::core::option::Option::Some(#ty {
                        #(#idents: ::petros_schema::Cell::from_value(it.next()?)?,)*
                    })
                }

                fn key(&self) -> ::std::vec::Vec<::petros_schema::Value> {
                    ::std::vec![#(::petros_schema::Bind::to_value(&self.#key_names)),*]
                }
            }
        });
    }

    Ok(quote! {
        // So a change to the schema rebuilds what was generated from it.
        const _: &str = ::core::include_str!(#schema_str);
        #(#items)*
    })
}

/// A column's type, as the four the store understands.
#[derive(Clone, Copy)]
enum Kind {
    Blob,
    Text,
    Int,
    Bool,
}

impl Kind {
    fn rust(self) -> proc_macro2::TokenStream {
        match self {
            Kind::Blob => quote!(::std::vec::Vec<u8>),
            Kind::Text => quote!(::std::string::String),
            Kind::Int => quote!(i64),
            Kind::Bool => quote!(bool),
        }
    }
    fn token(self) -> proc_macro2::TokenStream {
        match self {
            Kind::Blob => quote!(::petros_schema::ColumnTy::Blob),
            Kind::Text => quote!(::petros_schema::ColumnTy::Text),
            Kind::Int => quote!(::petros_schema::ColumnTy::Int),
            Kind::Bool => quote!(::petros_schema::ColumnTy::Bool),
        }
    }
}

/// SQLite's declared type, as the store's four.
fn column_kind(decl: &str) -> Result<Kind, String> {
    let d = decl.trim().to_ascii_uppercase();
    Ok(match d.as_str() {
        "BLOB" => Kind::Blob,
        "TEXT" => Kind::Text,
        "BOOL" | "BOOLEAN" => Kind::Bool,
        _ if d.contains("INT") => Kind::Int,
        other => {
            return Err(format!(
                "column type `{other}` is not one a mutation can write. The log is \
                 permanent and a foreign caller has to be able to write one, so a \
                 column is BLOB, TEXT, BOOL or an integer type."
            ))
        }
    })
}

/// A nullable column is an `Option`, because a `NULL` has to have somewhere to
/// go. Without this the field is `i64`, the `NULL` fails to decode, and the row
/// silently does not appear in the answer.
fn optional(ty: proc_macro2::TokenStream, null: bool) -> proc_macro2::TokenStream {
    if null {
        quote!(::core::option::Option<#ty>)
    } else {
        ty
    }
}

/// `song` -> `Song`. A table names a row type the way a type is spelled.
fn camel(snake: &str) -> String {
    let mut out = String::new();
    let mut up = true;
    for c in snake.chars() {
        if c == '_' {
            up = true;
        } else if up {
            out.extend(c.to_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}
