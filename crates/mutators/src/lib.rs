//! The domain, run from a wasm module.
//!
//! Every peer links this and none of them link an `apply`: the server, the two
//! terminal examples, the iced window and the phone all interpret the same
//! `crates/todo-wasm` build. That is the invariant at the top of `CLAUDE.md`
//! made structural rather than aspirational — there is one `apply` because
//! there is one artifact, and a peer that has not loaded it cannot mutate at
//! all.
//!
//! [`MUTATORS`] is process-wide because `exo` calls `Mutation::apply` during a
//! rebase and hands it no context of ours. One domain per process is the same
//! assumption a linked `apply` made; this just makes it replaceable at runtime.

use std::sync::RwLock;

pub mod app;
pub mod wasm;

pub use app::{from_json, Payload, WasmTodo};
pub use wasm::Mutators;

/// The module every peer in this process runs.
pub static MUTATORS: RwLock<Option<wasm::Mutators>> = RwLock::new(None);

/// Install a module, replacing whatever was running. Returns the generation,
/// which moves on every successful swap.
///
/// A module that does not export the ABI is rejected here rather than at the
/// first mutation, so a bad push fails loudly and the old one keeps running.
pub fn load(wasm: &[u8]) -> Result<u64, String> {
    let mut slot = MUTATORS
        .write()
        .map_err(|_| "the mutator lock was poisoned by an earlier panic".to_string())?;
    match slot.as_mut() {
        Some(existing) => existing.swap(wasm)?,
        None => *slot = Some(wasm::Mutators::load(wasm)?),
    }
    Ok(slot.as_ref().map(|m| m.generation).unwrap_or(0))
}

/// Which module is running, or zero if none has been installed.
pub fn generation() -> u64 {
    MUTATORS
        .read()
        .ok()
        .and_then(|s| s.as_ref().map(|m| m.generation))
        .unwrap_or(0)
}

/// The module this build was compiled against.
///
/// Every peer that is not hot-reloading — the server, the terminal examples —
/// wants exactly this and nothing else, so it is baked in rather than found at
/// runtime. `just mutators` is what puts it there.
pub const BUNDLED: &[u8] =
    include_bytes!("../../../target/wasm32-unknown-unknown/mutators/todo_wasm.wasm");

/// Install [`BUNDLED`]. What a peer with no Metro attached calls at startup.
pub fn load_bundled() -> Result<u64, String> {
    load(BUNDLED)
}
