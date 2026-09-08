//! The only things this module can reach.
//!
//! There is no clock here, no randomness, no network and no filesystem — not by
//! convention, as `CLAUDE.md` has to ask for in Rust, but because the host
//! imports three functions and nothing else exists to misuse. Determinism stops
//! being a rule and becomes a property of the sandbox.
//!
//! Values are inlined into the SQL rather than bound, because Diesel's
//! `sql_query` binds at compile time and this side is dynamic by construction.
//! [`lit`] is the whole of that: it is the one place a value becomes text, and
//! it is where a real version would grow proper parameter binding through
//! `libsqlite3-sys`.

// Without this the imports land in a module called `env`, and the host — which
// names them `exo` — cannot satisfy them.
#[link(wasm_import_module = "exo")]
extern "C" {
    /// First column of the first row, as an integer. Zero for no rows or NULL —
    /// which is exactly what `MAX(pos)` over an empty table should mean here.
    #[link_name = "query_int"]
    fn host_query_int(sql: *const u8, sql_len: u32) -> i64;
    /// Whether the query matched anything at all.
    #[link_name = "query_exists"]
    fn host_query_exists(sql: *const u8, sql_len: u32) -> i32;
    /// Rows affected. Negative means the host failed; the trap is raised there.
    #[link_name = "exec"]
    fn host_exec(sql: *const u8, sql_len: u32) -> i64;
}

pub fn query_int(sql: &str) -> i64 {
    unsafe { host_query_int(sql.as_ptr(), sql.len() as u32) }
}

pub fn query_exists(sql: &str) -> bool {
    unsafe { host_query_exists(sql.as_ptr(), sql.len() as u32) != 0 }
}

pub fn exec(sql: &str) -> i64 {
    unsafe { host_exec(sql.as_ptr(), sql.len() as u32) }
}

/// A SQLite literal, escaped the way SQLite defines them.
pub enum Lit<'a> {
    Int(i64),
    Text(&'a str),
    Blob(&'a [u8]),
}

pub fn lit(v: Lit<'_>) -> String {
    match v {
        Lit::Int(i) => i.to_string(),
        // A single quote is escaped by doubling it. That is the whole rule.
        Lit::Text(s) => format!("'{}'", s.replace('\'', "''")),
        Lit::Blob(b) => {
            let mut out = String::with_capacity(b.len() * 2 + 3);
            out.push_str("X'");
            for byte in b {
                out.push_str(&format!("{byte:02x}"));
            }
            out.push('\'');
            out
        }
    }
}
