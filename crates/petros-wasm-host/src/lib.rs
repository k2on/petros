//! Running a domain from a wasm module instead of from a linked symbol.
//!
//! This is the half a peer needs when it wants to *replace* `apply` without
//! being rebuilt — which in practice means a phone, where a native build is
//! four minutes and a module push is half a second. Every other peer links its
//! domain and never comes here.
//!
//! There is nothing app-specific in this crate. The `petros::App` that runs a
//! module, and the module bytes themselves, belong to whoever is being run;
//! see `crates/ffi` in the example workspace for what that looks like.
//!
//! [`MUTATORS`] is process-wide because `petros` calls `Mutation::apply` during
//! a rebase and hands it no context of ours. One domain per process is the same
//! assumption a linked `apply` makes; this just makes it replaceable at runtime.

use std::sync::RwLock;

pub mod wasm;

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
