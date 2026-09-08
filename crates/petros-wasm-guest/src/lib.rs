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
    //
    // Two calls rather than one, because a host function cannot allocate in the
    // guest without calling back into an instance that is already running.
    // `__petros_store` leaves its answer with the host and says how long it is;
    // `__petros_take` copies it into a buffer this side just made.
    (@imports) => {
        #[link(wasm_import_module = "petros")]
        extern "C" {
            #[link_name = "store"]
            fn __petros_store(request: *const u8, len: u32) -> u32;
            #[link_name = "take"]
            fn __petros_take(into: *mut u8, len: u32);
        }

        /// The host's database, reached across the sandbox boundary.
        ///
        /// Two methods, because the SQL was checked at build time and there is
        /// nothing left to decide here.
        struct PetrosStore;

        impl PetrosStore {
            fn ask(request: &$crate::petros_schema::Request) -> ::std::vec::Vec<u8> {
                let encoded = $crate::encode_request(request);
                let len = unsafe { __petros_store(encoded.as_ptr(), encoded.len() as u32) };
                let mut answer = ::std::vec![0u8; len as usize];
                if len > 0 {
                    unsafe { __petros_take(answer.as_mut_ptr(), len) };
                }
                answer
            }
        }

        impl $crate::petros_schema::Store for PetrosStore {
            fn exec(&mut self, sql: &str, params: &[$crate::petros_schema::Value]) {
                let _ = PetrosStore::ask(&$crate::petros_schema::Request::Exec {
                    sql: sql.to_string(),
                    params: params.to_vec(),
                });
            }

            fn query(
                &mut self,
                sql: &str,
                params: &[$crate::petros_schema::Value],
                types: &[$crate::petros_schema::ColumnTy],
            ) -> ::std::vec::Vec<::std::vec::Vec<$crate::petros_schema::Value>> {
                let answer = PetrosStore::ask(&$crate::petros_schema::Request::Query {
                    sql: sql.to_string(),
                    params: params.to_vec(),
                    types: types.to_vec(),
                });
                $crate::decode_rows(&answer)
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
                    domain::apply(&mut PetrosStore, &value, who)
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

/// A request, as the bytes the host decodes. CBOR, like the log.
#[doc(hidden)]
pub fn encode_request(request: &petros_schema::Request) -> Vec<u8> {
    let mut out = Vec::new();
    // Nothing here can fail on a well-formed request, and there is no channel
    // to report it on if it did: an empty request is one the host rejects.
    let _ = ciborium::into_writer(request, &mut out);
    out
}

/// Rows, as the host encoded them. An unreadable answer is no rows, which the
/// domain already has to handle — a query can legitimately match nothing.
#[doc(hidden)]
pub fn decode_rows(bytes: &[u8]) -> Vec<Vec<petros_schema::Value>> {
    if bytes.is_empty() {
        return Vec::new();
    }
    ciborium::from_reader(bytes).unwrap_or_default()
}
