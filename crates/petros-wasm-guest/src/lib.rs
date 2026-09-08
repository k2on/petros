//! The guest half of the mutator ABI.
//!
//! An app's wasm crate has no decisions left to make: it decodes CBOR, calls
//! `apply`, hands back a packed pointer, and reaches the database through three
//! imported functions. That was a hundred and thirty lines of boilerplate in
//! the demo, and it would have been the same hundred and thirty in every app.
//!
//! So it is one macro:
//!
//! ```ignore
//! petros_wasm_guest::export!(todo::domain, todo::domain::SCHEMA_TEXT);
//! ```
//!
//! # What crosses the boundary
//!
//! Everything is `(ptr, len)` in the guest's linear memory, and everything
//! coming back is `(ptr << 32) | len` packed into a `u64` — one convention,
//! used in both directions, because the alternative is two.
//!
//! Allocation is one-way: [`petros_alloc`](export!) hands the host a buffer and
//! forgets it, and returned buffers are leaked the same way. That is sound only
//! because the host gives each call a fresh `Store` and drops the guest's whole
//! linear memory with it. A host that reuses an instance would leak — measured
//! at 1.1MiB after sixty calls — so [`ABI_VERSION`] has to move if that ever
//! changes.

#![forbid(unsafe_op_in_unsafe_fn)]

/// Bumped when the host/guest contract changes, so a mismatched pair says so
/// rather than corrupting a database.
pub const ABI_VERSION: u32 = 1;

/// The module the host satisfies the imports from.
pub const IMPORT_MODULE: &str = "petros";

/// Everything the host calls, and everything the guest calls back.
///
/// Takes the path to a module exporting `apply` and `fill_auto` — the two
/// halves of a domain — and the path to its `SCHEMA_TEXT`. Both come from one
/// declaration if the domain used `petros_schema::declare!`.
#[macro_export]
macro_rules! export {
    ($domain:path, $schema:path) => {
        $crate::export!(@imports);
        $crate::export!(@abi $domain);
        $crate::petros_schema::embed!($schema);
    };

    // Without this the imports land in a module called `env`, and the host —
    // which names them `petros` — cannot satisfy them.
    (@imports) => {
        #[link(wasm_import_module = "petros")]
        extern "C" {
            #[link_name = "query_int"]
            fn __petros_query_int(sql: *const u8, sql_len: u32) -> i64;
            #[link_name = "query_exists"]
            fn __petros_query_exists(sql: *const u8, sql_len: u32) -> i32;
            #[link_name = "exec"]
            fn __petros_exec(sql: *const u8, sql_len: u32) -> i64;
        }

        /// The host's SQLite, reached across the sandbox boundary.
        struct PetrosImports;

        impl $crate::petros_schema::Host for PetrosImports {
            fn query_int(&mut self, sql: &str) -> i64 {
                unsafe { __petros_query_int(sql.as_ptr(), sql.len() as u32) }
            }
            fn query_exists(&mut self, sql: &str) -> bool {
                unsafe { __petros_query_exists(sql.as_ptr(), sql.len() as u32) != 0 }
            }
            fn exec(&mut self, sql: &str) {
                unsafe {
                    __petros_exec(sql.as_ptr(), sql.len() as u32);
                }
            }
        }
    };

    (@abi $domain:path) => {
        /// Give the host a buffer in our linear memory.
        #[no_mangle]
        pub extern "C" fn petros_alloc(len: u32) -> *mut u8 {
            $crate::alloc(len)
        }

        /// Apply one mutation, as the CBOR payload the log stores, verbatim.
        /// Zero for success; otherwise a packed refusal reason.
        ///
        /// # Safety
        /// The host must pass pointer/length pairs describing live buffers in
        /// this module's linear memory — the only thing its `write_bytes`
        /// produces.
        #[no_mangle]
        pub unsafe extern "C" fn petros_apply(
            mutation: *const u8,
            mutation_len: u32,
            actor: *const u8,
            actor_len: u32,
        ) -> u64 {
            let payload = unsafe { ::core::slice::from_raw_parts(mutation, mutation_len as usize) };
            let who = unsafe { ::core::slice::from_raw_parts(actor, actor_len as usize) };
            let who = ::core::str::from_utf8(who).unwrap_or("");

            let outcome = match $crate::decode(payload) {
                Ok(value) => {
                    use $domain as domain;
                    domain::apply(&mut PetrosImports, &value, who)
                }
                Err(e) => Err(e),
            };
            match outcome {
                Ok(()) => 0,
                Err(reason) => $crate::packed(reason.into_bytes()),
            }
        }

        /// Fill the auto values and hand back the rewritten payload.
        ///
        /// # Safety
        /// As [`petros_apply`]: the pointers must describe live buffers here,
        /// and `uuid` must address sixteen readable bytes.
        #[no_mangle]
        pub unsafe extern "C" fn petros_fill_auto(
            mutation: *const u8,
            mutation_len: u32,
            uuid: *const u8,
            now_ms: i64,
        ) -> u64 {
            let payload = unsafe { ::core::slice::from_raw_parts(mutation, mutation_len as usize) };
            let uuid = unsafe { ::core::slice::from_raw_parts(uuid, 16) }.to_vec();
            let Ok(mut value) = $crate::decode(payload) else {
                return $crate::packed(::std::vec::Vec::new());
            };
            {
                use $domain as domain;
                domain::fill_auto(&mut value, uuid, now_ms);
            }
            match $crate::encode(&value) {
                Some(bytes) => $crate::packed(bytes),
                None => $crate::packed(::std::vec::Vec::new()),
            }
        }

        /// Bumped when the host/guest contract changes, so a mismatched pair
        /// says so.
        #[no_mangle]
        pub extern "C" fn petros_abi_version() -> u32 {
            $crate::ABI_VERSION
        }
    };
}

// The macro expands into the app's crate, so everything it calls has to be
// reachable from here rather than assumed present there.
#[doc(hidden)]
pub use ciborium;
#[doc(hidden)]
pub use petros_schema;

/// A buffer the host may write into. Forgotten on purpose: see the module note.
#[doc(hidden)]
pub fn alloc(len: u32) -> *mut u8 {
    let mut buf = Vec::<u8>::with_capacity(len as usize);
    let ptr = buf.as_mut_ptr();
    core::mem::forget(buf);
    ptr
}

/// `(ptr << 32) | len`, the one convention everything crossing here uses.
#[doc(hidden)]
pub fn packed(bytes: Vec<u8>) -> u64 {
    let len = bytes.len() as u64;
    let ptr = bytes.leak().as_ptr() as u64;
    (ptr << 32) | len
}

#[doc(hidden)]
pub fn decode(payload: &[u8]) -> Result<ciborium::value::Value, String> {
    ciborium::from_reader(payload).map_err(|e| format!("could not decode a mutation: {e}"))
}

/// `None` for a value that will not encode, which the caller reports as an
/// empty payload — the one signal the ABI has for "this went wrong".
#[doc(hidden)]
pub fn encode(value: &ciborium::value::Value) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    ciborium::into_writer(value, &mut out).ok()?;
    Some(out)
}
