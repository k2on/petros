//! The domain, compiled to wasm for the phone.
//!
//! There is no logic here. `apply` and `fill_auto` live in `crates/todo`, which
//! the server and the terminal peers link directly — this crate exists to give
//! that same code a [`Host`] made of imported functions, and to expose it
//! across the ABI the interpreter calls.
//!
//! Which is what makes the arrangement honest: the phone runs a file it can
//! replace without a rebuild, and it is the same file, from the same source,
//! that every other peer compiled in.
//!
//! Nothing here can reach a clock, a random number generator, the network or
//! the filesystem, because the module imports three functions and none of them
//! is any of those. On the native side that is a promise the `Host` trait
//! makes; here the sandbox enforces it.

use ciborium::value::Value;
use todo::domain::{self, Host};

// Without this the imports land in a module called `env`, and the host — which
// names them `petros` — cannot satisfy them.
#[link(wasm_import_module = "petros")]
extern "C" {
    #[link_name = "query_int"]
    fn host_query_int(sql: *const u8, sql_len: u32) -> i64;
    #[link_name = "query_exists"]
    fn host_query_exists(sql: *const u8, sql_len: u32) -> i32;
    #[link_name = "exec"]
    fn host_exec(sql: *const u8, sql_len: u32) -> i64;
}

/// The host's SQLite, reached across the sandbox boundary.
struct Imports;

impl Host for Imports {
    fn query_int(&mut self, sql: &str) -> i64 {
        unsafe { host_query_int(sql.as_ptr(), sql.len() as u32) }
    }

    fn query_exists(&mut self, sql: &str) -> bool {
        unsafe { host_query_exists(sql.as_ptr(), sql.len() as u32) != 0 }
    }

    fn exec(&mut self, sql: &str) {
        unsafe {
            host_exec(sql.as_ptr(), sql.len() as u32);
        }
    }
}

// Carry the schema in a custom section, so a tool holding only this file knows
// what the module accepts. `emit-mutators` reads it from here rather than
// linking `todo`, which is not a stylistic preference: linking the domain into
// the generator put 0.31s on a 0.45s loop, because it then relinks whenever
// `apply` changes.
petros_schema::embed!(todo::schema::SCHEMA_TEXT);

// ------------------------------------------------------------------- the ABI

/// Give the host a buffer in our linear memory.
#[no_mangle]
pub extern "C" fn petros_alloc(len: u32) -> *mut u8 {
    let mut buf = Vec::<u8>::with_capacity(len as usize);
    let ptr = buf.as_mut_ptr();
    core::mem::forget(buf);
    ptr
}

/// `(ptr << 32) | len`, the one convention everything crossing here uses.
fn packed(bytes: Vec<u8>) -> u64 {
    let len = bytes.len() as u64;
    let ptr = bytes.leak().as_ptr() as u64;
    (ptr << 32) | len
}

/// Apply one mutation, as the CBOR payload the log stores, verbatim.
/// Zero for success; otherwise a packed refusal reason.
///
/// # Safety
/// The host must pass pointer/length pairs describing live buffers in this
/// module's linear memory — the only thing its `write_bytes` produces.
#[no_mangle]
pub unsafe extern "C" fn petros_apply(
    mutation: *const u8,
    mutation_len: u32,
    actor: *const u8,
    actor_len: u32,
) -> u64 {
    let payload = unsafe { core::slice::from_raw_parts(mutation, mutation_len as usize) };
    let who = unsafe { core::slice::from_raw_parts(actor, actor_len as usize) };
    let who = core::str::from_utf8(who).unwrap_or("");

    let outcome = match ciborium::from_reader::<Value, _>(payload) {
        Ok(v) => domain::apply(&mut Imports, &v, who),
        Err(e) => Err(format!("could not decode a mutation: {e}")),
    };
    match outcome {
        Ok(()) => 0,
        Err(reason) => packed(reason.into_bytes()),
    }
}

/// Fill the auto values and hand back the rewritten payload.
///
/// # Safety
/// As [`petros_apply`]: the pointers must describe live buffers here, and `uuid`
/// must address sixteen readable bytes.
#[no_mangle]
pub unsafe extern "C" fn petros_fill_auto(
    mutation: *const u8,
    mutation_len: u32,
    uuid: *const u8,
    now_ms: i64,
) -> u64 {
    let payload = unsafe { core::slice::from_raw_parts(mutation, mutation_len as usize) };
    let uuid = unsafe { core::slice::from_raw_parts(uuid, 16) }.to_vec();
    let Ok(mut v) = ciborium::from_reader::<Value, _>(payload) else {
        return packed(Vec::new());
    };
    domain::fill_auto(&mut v, uuid, now_ms);
    let mut out = Vec::new();
    if ciborium::into_writer(&v, &mut out).is_err() {
        return packed(Vec::new());
    }
    packed(out)
}

/// Bumped when the host/guest contract changes, so a mismatched pair says so.
#[no_mangle]
pub extern "C" fn petros_abi_version() -> u32 {
    1
}
