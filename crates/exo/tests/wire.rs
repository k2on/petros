//! The log is permanent, so the encoding is a contract.
//!
//! If a change makes this file fail, that change would have made every existing
//! installation unable to read its own history. See `docs/decisions.md`.

mod common;

use common::todo::TodoMutation;
use exo::{ActorId, Id, ServerMsg};
use serde::{Deserialize, Serialize};

const FIXTURE: &str = include_str!("fixtures/wire-v1.hex");

/// The fixture is hex, wrapped, with `#` comment lines.
fn unhex(s: &str) -> Vec<u8> {
    let s: String = s
        .lines()
        .filter(|l| !l.starts_with('#'))
        .flat_map(|l| l.chars())
        .filter(|c| c.is_ascii_hexdigit())
        .collect();
    (0..s.len() / 2)
        .map(|i| u8::from_str_radix(&s[i * 2..i * 2 + 2], 16).expect("hex"))
        .collect()
}

#[test]
fn old_wire_bytes_still_deserialize() {
    let msg: ServerMsg<TodoMutation> = exo::decode(&unhex(FIXTURE)).expect("decode fixture");
    let ServerMsg::Batch { entries, has_more } = msg else {
        panic!("the fixture is a Batch");
    };
    assert!(!has_more);
    assert_eq!(entries.len(), 5);
    assert_eq!(entries[0].seq, Some(1));
    assert_eq!(entries[0].actor, ActorId::from("alice"));
    assert_eq!(
        entries[0].mutation,
        TodoMutation::Add {
            id: Id::from_u128(0x11),
            text: "buy milk".into(),
            created_ms: 1_577_836_800_000,
        }
    );
    assert_eq!(
        entries[4].mutation,
        TodoMutation::Remove {
            id: Id::from_u128(0x11)
        }
    );
}

/// The compatible change: a new field, defaulted. Old bytes must still decode.
#[derive(Serialize)]
#[serde(tag = "t")]
enum Before {
    Add { id: Id, text: String },
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "t")]
enum After {
    Add {
        id: Id,
        text: String,
        #[serde(default)]
        note: Option<String>,
    },
    /// Variants may be added; existing ones are never renamed or removed.
    Star { id: Id },
}

#[test]
fn a_field_added_with_serde_default_reads_old_bytes() {
    let old = exo::encode(&Before::Add {
        id: Id::from_u128(9),
        text: "written before the field existed".into(),
    })
    .unwrap();

    let new: After = exo::decode(&old).expect("decode old bytes with the new type");
    assert_eq!(
        new,
        After::Add {
            id: Id::from_u128(9),
            text: "written before the field existed".into(),
            note: None,
        }
    );
}
