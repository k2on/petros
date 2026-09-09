//! A write the database refuses has to reach the module that asked for it.
//!
//! The two halves are written in different crates — the host encodes, the guest
//! decodes — and nothing links them at compile time, so this is what holds them
//! together.

use petros_wasm_guest::decode_outcome;
use petros_wasm_host::wasm::encode_outcome;

#[test]
fn a_refusal_survives_the_round_trip() {
    let sent = encode_outcome(Err("writing to `favorite`: FOREIGN KEY".into())).unwrap();
    assert_eq!(
        decode_outcome(sent),
        Err("writing to `favorite`: FOREIGN KEY".to_string())
    );
}

/// Silence means it worked. That is also what an older host answers for every
/// write, so a guest built against this reads an old host exactly as it used
/// to — no version to bump and no build to keep in step.
#[test]
fn a_write_that_worked_says_nothing() {
    assert!(encode_outcome(Ok(())).unwrap().is_empty());
    assert_eq!(decode_outcome(Vec::new()), Ok(()));
}

/// A malformed answer is a refusal, not a success. Reading it as "fine" is the
/// exact failure this change exists to remove.
#[test]
fn an_answer_that_cannot_be_read_is_still_a_refusal() {
    assert!(decode_outcome(vec![0xff, 0xff, 0xff]).is_err());
}
