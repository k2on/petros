//! Wrap the mutator module as a TypeScript file, so Metro can carry it.
//!
//! Metro's fast refresh moves *modules*, not assets, so the fastest way onto a
//! running phone is to stop treating the wasm as a file at all: it becomes a
//! base64 string in a `.ts` file, Metro notices the change like any other edit,
//! and the app hands the bytes to `loadMutators`. No dev server of our own, no
//! asset pipeline, no fetch — the mechanism that already reloads your React
//! components reloads your `apply`.
//!
//! Base64 costs a third in size and about a millisecond to decode, and buys the
//! whole delivery path for nothing.

use std::io::Write;

// The verb declaration, compiled in rather than parsed. `include!` keeps this
// crate free of dependencies — which is why it is its own crate — while making
// `crates/todo/src/verbs.rs` the single place a verb is named. Change it there
// and the native peers, the wasm module and these types all move together.
include!("../../todo/src/verbs.rs");

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let wasm = args
        .next()
        .unwrap_or_else(|| "target/wasm32-unknown-unknown/mutators/todo_wasm.wasm".to_string());
    let out = args
        .next()
        .unwrap_or_else(|| "clients/expo/src/mutators.gen.ts".to_string());

    let bytes = std::fs::read(&wasm)?;
    let encoded = base64(&bytes);

    let body = format!(
        "// Generated from crates/todo-wasm by `just mutators`. Do not edit.\n\
         //\n\
         // The domain — every mutation and every query — as a wasm module Metro can\n\
         // hot-reload. Editing the Rust rewrites this file, Metro pushes it, and the\n\
         // running app swaps `apply` without a native rebuild.\n\
         \n\
         /** {} bytes of wasm, {} of base64. */\n\
         export const MUTATORS_WASM_B64 =\n  \"{}\";\n\
         \n\
         /** Changes whenever the module does, so a reload can be noticed. */\n\
         export const MUTATORS_BUILD = \"{}\";\n\
         {}",
        bytes.len(),
        encoded.len(),
        encoded,
        fingerprint(&bytes),
        typescript(),
    );

    // Write only on change: an identical file still wakes Metro, and a reload
    // that swaps a module for itself is one you sat through for nothing.
    if std::fs::read_to_string(&out)
        .map(|old| old == body)
        .unwrap_or(false)
    {
        eprintln!("mutators: unchanged");
        return Ok(());
    }
    let tmp = format!("{out}.tmp");
    std::fs::File::create(&tmp)?.write_all(body.as_bytes())?;
    std::fs::rename(&tmp, &out)?;
    eprintln!("mutators: {} bytes wasm -> {out}", bytes.len());
    Ok(())
}

/// The verbs, as a discriminated union TypeScript can check call sites against.
///
/// `mutate` on the engine takes a plain string, because the engine genuinely
/// does not know what verbs exist — that is what lets a new one ship without a
/// native build. This is the other half of that bargain: the names are unknown
/// at runtime and known at compile time, from the same declaration the module
/// dispatches on.
fn typescript() -> String {
    let mut lines: Vec<String> = vec![
        String::new(),
        "/** A log entry's identity: the canonical 8-4-4-4-12 form. Sixteen bytes on".into(),
        " *  the wire — `from_json` converts any field named `id` or `*_id`. */".into(),
        "export type Id = string;".into(),
        String::new(),
        "/** Every verb the module understands, and what authoring one takes.".into(),
        " *".into(),
        " *  Generated from crates/todo-wasm/src/verbs.rs. Auto-filled fields — ids,".into(),
        " *  timestamps — are absent on purpose: the caller does not choose them,".into(),
        " *  the module does. */".into(),
        "export type MutationArgs = {".into(),
    ];
    for verb in VERBS {
        if verb.args.is_empty() {
            // `Record<string, never>` rather than `{}`, which in TypeScript
            // means "anything except null" and would check nothing at all.
            lines.push(format!("  {}: Record<string, never>;", verb.name));
            continue;
        }
        let fields: Vec<String> = verb
            .args
            .iter()
            .map(|a| format!("{}: {}", a.name, a.ty.typescript()))
            .collect();
        lines.push(format!("  {}: {{ {} }};", verb.name, fields.join("; ")));
    }
    lines.push("};".into());
    lines.push(String::new());
    lines.push("/** The name of any verb the module handles. */".into());
    lines.push("export type Verb = keyof MutationArgs;".into());
    lines.push(String::new());
    lines.join("\n")
}

/// FNV-1a. Enough to say "these are different bytes", which is all it is for.
fn fingerprint(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x1000_0000_01b3);
    }
    format!("{hash:016x}")
}

fn base64(bytes: &[u8]) -> String {
    const SET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(SET[(n >> 18) as usize & 63] as char);
        out.push(SET[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            SET[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            SET[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}
