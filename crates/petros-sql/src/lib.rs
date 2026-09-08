//! Raw SQL in a mutation, checked before it ships.
//!
//! The typed store covers point operations — get, exists, max, put, delete —
//! and loses exactly one thing: set operations. `INSERT ... SELECT` over a
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
use quote::quote;
use std::path::PathBuf;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Expr, LitStr, Token};

/// A statement, its store, and the values for its placeholders.
struct Stmt {
    store: Expr,
    sql: LitStr,
    args: Vec<Expr>,
}

impl Parse for Stmt {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let store: Expr = input.parse()?;
        input.parse::<Token![,]>()?;
        let sql: LitStr = input.parse()?;
        let mut args = Vec::new();
        if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
            let rest = Punctuated::<Expr, Token![,]>::parse_terminated(input)?;
            args = rest.into_iter().collect();
        }
        Ok(Stmt { store, sql, args })
    }
}

/// Run a checked statement against a [`petros_schema::Store`].
///
/// ```ignore
/// petros_sql::exec!(
///     store,
///     "INSERT INTO favorite (song_id, pos, favorited_ms, actor)
///      SELECT s.id,
///             (SELECT COALESCE(MAX(pos), 0) FROM favorite)
///               + ROW_NUMBER() OVER (ORDER BY s.pos, s.id),
///             ?, ?
///        FROM song s
///       WHERE NOT EXISTS (SELECT 1 FROM favorite f WHERE f.song_id = s.id)",
///     favorited_ms, actor
/// );
/// ```
#[proc_macro]
pub fn exec(input: TokenStream) -> TokenStream {
    let Stmt { store, sql, args } = syn::parse_macro_input!(input as Stmt);
    let text = sql.value();

    let schema = match schema_path() {
        Ok(path) => path,
        Err(message) => return error(&sql, &message),
    };
    let ddl = match std::fs::read_to_string(&schema) {
        Ok(ddl) => ddl,
        Err(e) => {
            return error(
                &sql,
                &format!(
                    "cannot read the schema at {}: {e}\n\
                     set PETROS_SCHEMA, or put a schema.sql beside Cargo.toml",
                    schema.display()
                ),
            )
        }
    };

    match check(&ddl, &text, args.len()) {
        Ok(()) => {}
        Err(message) => return error(&sql, &message),
    }

    // `include_str!` so rustc treats the schema as an input to this
    // compilation. Without it, editing the schema would not rebuild the call
    // sites it invalidates — the failure being a stale check, which is worse
    // than no check.
    let schema_str = schema.to_string_lossy().into_owned();
    let values = args.iter().map(|a| {
        quote! { ::petros_schema::Bind::to_value(&(#a)) }
    });
    quote! {{
        const _: &str = ::core::include_str!(#schema_str);
        // A method call rather than `Store::exec(&mut store, ..)`, so this
        // works whether the caller holds a store or a `&mut` to one.
        #[allow(unused_imports)]
        use ::petros_schema::Store as _;
        #store.exec(#sql, &[#(#values),*])
    }}
    .into()
}

/// Read rows, as a `Vec` of an anonymous struct with a field per column.
///
/// Field types come from SQLite: at build time the statement is prepared and
/// each column's declared type is read back. An expression has no declared type
/// — `COUNT(*)` is not a column — so those need naming, which is the same
/// annotation sqlx asks for and for the same reason:
///
/// ```ignore
/// let rows = petros_sql::query!(
///     store,
///     "SELECT id, title, COALESCE(MAX(pos), 0) AS \"last: Int\" FROM song WHERE artist = ?",
///     artist
/// );
/// for row in rows { row.id; row.title; row.last; }
/// ```
///
/// Add `?` — `"last?: Int"` — for a column that can be null, and the field is
/// an `Option`.
#[proc_macro]
pub fn query(input: TokenStream) -> TokenStream {
    rows(input, false)
}

/// As [`query!`], for a statement that returns at most one row.
#[proc_macro]
pub fn query_one(input: TokenStream) -> TokenStream {
    rows(input, true)
}

fn rows(input: TokenStream, single: bool) -> TokenStream {
    let Stmt { store, sql, args } = syn::parse_macro_input!(input as Stmt);
    let text = sql.value();

    let schema = match schema_path() {
        Ok(path) => path,
        Err(message) => return error(&sql, &message),
    };
    let ddl = match std::fs::read_to_string(&schema) {
        Ok(ddl) => ddl,
        Err(e) => {
            return error(
                &sql,
                &format!("cannot read the schema at {}: {e}", schema.display()),
            )
        }
    };

    let columns = match describe(&ddl, &text, args.len()) {
        Ok(columns) => columns,
        Err(message) => return error(&sql, &message),
    };

    let schema_str = schema.to_string_lossy().into_owned();
    let values = args
        .iter()
        .map(|a| quote! { ::petros_schema::Bind::to_value(&(#a)) });
    let tys = columns.iter().map(|c| c.ty.token());
    let fields = columns.iter().map(|c| {
        let name = syn::Ident::new(&c.name, sql.span());
        let rust = c.ty.rust();
        if c.nullable {
            quote! { pub #name: ::core::option::Option<#rust> }
        } else {
            quote! { pub #name: #rust }
        }
    });
    let reads = columns.iter().enumerate().map(|(i, c)| {
        let name = syn::Ident::new(&c.name, sql.span());
        let rust = c.ty.rust();
        if c.nullable {
            quote! {
                #name: match row.get(#i) {
                    ::core::option::Option::Some(::petros_schema::Value::Null)
                    | ::core::option::Option::None => ::core::option::Option::None,
                    ::core::option::Option::Some(v) =>
                        <#rust as ::petros_schema::Cell>::from_value(v),
                }
            }
        } else {
            quote! {
                #name: row
                    .get(#i)
                    .and_then(<#rust as ::petros_schema::Cell>::from_value)
                    .unwrap_or_default()
            }
        }
    });
    let take = if single {
        quote! { __rows.into_iter().next() }
    } else {
        quote! { __rows }
    };

    quote! {{
        const _: &str = ::core::include_str!(#schema_str);
        #[derive(Debug, Clone, PartialEq)]
        struct __Row { #(#fields),* }
        #[allow(unused_imports)]
        use ::petros_schema::Store as _;
        let __raw = #store.query(#sql, &[#(#values),*], &[#(#tys),*]);
        let __rows: ::std::vec::Vec<__Row> = __raw
            .iter()
            .map(|row| __Row { #(#reads),* })
            .collect();
        #take
    }}
    .into()
}

/// One result column, as the checker worked it out.
struct Col {
    name: String,
    ty: Kind,
    nullable: bool,
}

#[derive(Clone, Copy)]
enum Kind {
    Blob,
    Text,
    Int,
    Bool,
}

impl Kind {
    fn parse(name: &str) -> Option<Kind> {
        match name.trim().to_ascii_uppercase().as_str() {
            "BLOB" => Some(Kind::Blob),
            "TEXT" => Some(Kind::Text),
            "INT" | "INTEGER" | "BIGINT" => Some(Kind::Int),
            "BOOL" | "BOOLEAN" => Some(Kind::Bool),
            _ => None,
        }
    }
    fn token(self) -> proc_macro2::TokenStream {
        match self {
            Kind::Blob => quote! { ::petros_schema::ColumnTy::Blob },
            Kind::Text => quote! { ::petros_schema::ColumnTy::Text },
            Kind::Int => quote! { ::petros_schema::ColumnTy::Int },
            Kind::Bool => quote! { ::petros_schema::ColumnTy::Bool },
        }
    }
    fn rust(self) -> proc_macro2::TokenStream {
        match self {
            Kind::Blob => quote! { ::std::vec::Vec<u8> },
            Kind::Text => quote! { ::std::string::String },
            Kind::Int => quote! { i64 },
            Kind::Bool => quote! { bool },
        }
    }
}

/// Prepare the statement and ask SQLite what comes back.
fn describe(ddl: &str, sql: &str, args: usize) -> Result<Vec<Col>, String> {
    let conn = rusqlite::Connection::open_in_memory()
        .map_err(|e| format!("could not open a database to check against: {e}"))?;
    conn.execute_batch(ddl)
        .map_err(|e| format!("the schema does not apply: {e}"))?;
    let stmt = conn.prepare(sql).map_err(|e| format!("{e}"))?;

    let wanted = stmt.parameter_count();
    if wanted != args {
        return Err(format!(
            "this statement has {wanted} placeholder{} and was given {args} value{}",
            if wanted == 1 { "" } else { "s" },
            if args == 1 { "" } else { "s" },
        ));
    }
    if stmt.column_count() == 0 {
        return Err("this statement returns no columns; `exec!` is for writes".into());
    }

    let mut out = Vec::new();
    for column in stmt.columns() {
        let raw = column.name().to_string();
        // `AS "last: Int"` or `AS "last?: Int"` — the annotation, when SQLite
        // has nothing to declare.
        let (name, annotated) = match raw.split_once(':') {
            Some((left, right)) => (left.trim().to_string(), Kind::parse(right)),
            None => (raw.clone(), None),
        };
        let nullable = name.ends_with('?');
        let name = name.trim_end_matches('?').to_string();

        let ty = match annotated {
            Some(kind) => kind,
            None => match column.decl_type().and_then(Kind::parse) {
                Some(kind) => kind,
                // SQLite declares nothing for an expression, and guessing here
                // would be a type error that only shows up as a wrong value.
                None => {
                    // The column's own name is only a usable suggestion when it
                    // is already an identifier; `COUNT(*)` is not.
                    let suggestion = if !name.is_empty()
                        && name.chars().all(|c| c.is_alphanumeric() || c == '_')
                    {
                        name.clone()
                    } else {
                        "value".to_string()
                    };
                    return Err(format!(
                        "column `{raw}` is an expression, so SQLite has no declared type for it — \
                         name it, as in `AS \"{suggestion}: Int\"` \
                         (Blob, Text, Int or Bool; add `?` for a column that can be null)"
                    ));
                }
            },
        };
        if !name.chars().all(|c| c.is_alphanumeric() || c == '_') || name.is_empty() {
            return Err(format!(
                "column `{raw}` is not a usable field name — give it one with `AS`"
            ));
        }
        out.push(Col { name, ty, nullable });
    }
    Ok(out)
}

fn schema_path() -> Result<PathBuf, String> {
    if let Ok(path) = std::env::var("PETROS_SCHEMA") {
        return Ok(PathBuf::from(path));
    }
    let root = std::env::var("CARGO_MANIFEST_DIR")
        .map_err(|_| "CARGO_MANIFEST_DIR is not set; this must run under cargo".to_string())?;
    Ok(PathBuf::from(root).join("schema.sql"))
}

/// Prepare the statement against the schema, and say what SQLite says.
fn check(ddl: &str, sql: &str, args: usize) -> Result<(), String> {
    let conn = rusqlite::Connection::open_in_memory()
        .map_err(|e| format!("could not open a database to check against: {e}"))?;
    conn.execute_batch(ddl)
        .map_err(|e| format!("the schema does not apply: {e}"))?;

    let stmt = conn.prepare(sql).map_err(|e| format!("{e}"))?;

    let wanted = stmt.parameter_count();
    if wanted != args {
        return Err(format!(
            "this statement has {wanted} placeholder{} and was given {args} value{}",
            if wanted == 1 { "" } else { "s" },
            if args == 1 { "" } else { "s" },
        ));
    }
    // A `SELECT` here would run and discard its rows. That is never what was
    // meant, and it is the sort of thing that looks like it works.
    if stmt.readonly() {
        return Err("this statement reads and writes nothing; `exec!` is for writes".into());
    }
    Ok(())
}

fn error(at: &LitStr, message: &str) -> TokenStream {
    syn::Error::new(at.span(), message)
        .to_compile_error()
        .into()
}
