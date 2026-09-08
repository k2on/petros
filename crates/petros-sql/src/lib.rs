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
        quote! { ::petros_schema::Cell::to_value(&(#a)) }
    });
    quote! {{
        const _: &str = ::core::include_str!(#schema_str);
        ::petros_schema::Store::exec(&mut #store, #sql, &[#(#values),*])
    }}
    .into()
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
