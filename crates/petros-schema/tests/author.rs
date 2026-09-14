//! Authoring a mutation from JSON.
//!
//! The shape here is a contract with generated TypeScript: `petros-codegen`
//! reads a module's schema and emits calls that arrive as JSON. What decides
//! how an argument is carried is the *declaration* — the same one the
//! generator read — rather than anything about the argument's name.

use petros_schema::author::{from_json, from_value};
use petros_schema::cbor::Value;
use petros_schema::{AppSchema, Ty, Verb};
use serde_json::json;

/// Deliberately perverse, and that is the test: `media` is an id and is not
/// called one, `note_id` is text and is. A name-based rule gets both wrong.
fn schema() -> AppSchema {
    AppSchema::new([
        Verb::new("AddSong").arg("title", Ty::Text),
        Verb::new("Favorite")
            .arg("media", Ty::Id)
            .arg("note_id", Ty::Text),
        Verb::new("SetGain").arg("gain", Ty::Integer),
        Verb::new("FavoriteAll"),
    ])
}

fn field<'a>(v: &'a Value, name: &str) -> Option<&'a Value> {
    let Value::Map(entries) = v else { return None };
    entries.iter().find_map(|(k, val)| match k {
        Value::Text(t) if t == name => Some(val),
        _ => None,
    })
}

const UUID: &str = "67e55084-765d-446c-9191-4ff9861f6d8e";

fn uuid_bytes() -> Vec<u8> {
    vec![
        0x67, 0xe5, 0x50, 0x84, 0x76, 0x5d, 0x44, 0x6c, 0x91, 0x91, 0x4f, 0xf9, 0x86, 0x1f, 0x6d,
        0x8e,
    ]
}

#[test]
fn the_verb_travels_as_t() {
    let m = from_value(&schema(), "AddSong", json!({ "title": "Glue" })).unwrap();
    assert_eq!(field(&m, "t"), Some(&Value::Text("AddSong".into())));
    assert_eq!(field(&m, "title"), Some(&Value::Text("Glue".into())));
}

/// The declared type decides, and nothing else does. Both arguments here hold
/// the same uuid and only the one declared `Id` becomes bytes.
#[test]
fn the_declared_type_decides_not_the_name() {
    let m = from_value(
        &schema(),
        "Favorite",
        json!({ "media": UUID, "note_id": UUID }),
    )
    .unwrap();
    assert_eq!(field(&m, "media"), Some(&Value::Bytes(uuid_bytes())));
    assert_eq!(
        field(&m, "note_id"),
        Some(&Value::Text(UUID.into())),
        "declared Text, so it stays text however much it looks like an id"
    );
}

#[test]
fn an_id_that_is_not_one_is_refused() {
    let e = from_value(&schema(), "Favorite", json!({ "media": "nope" })).unwrap_err();
    assert!(e.contains("not an id"), "{e}");
}

/// The failure this exists to stop: a misspelled argument decodes at its
/// default, so the mutation runs and silently does nothing — on every peer,
/// with no error anywhere. Authoring is the last place that can tell.
#[test]
fn an_argument_the_verb_does_not_take_is_refused() {
    let e = from_value(&schema(), "Favorite", json!({ "medai": UUID })).unwrap_err();
    assert!(e.contains("medai"), "{e}");
    assert!(
        e.contains("media"),
        "it should say what the verb does take: {e}"
    );

    let e = from_value(&schema(), "FavoriteAll", json!({ "id": UUID })).unwrap_err();
    assert!(e.contains("takes no arguments"), "{e}");
}

#[test]
fn a_verb_the_app_does_not_have_is_refused() {
    let e = from_value(&schema(), "Frobnicate", json!({})).unwrap_err();
    assert!(e.contains("Frobnicate"), "{e}");
    assert!(e.contains("AddSong"), "it should list what there is: {e}");
}

/// No floats anywhere near the log: `apply` must not branch on one, and two
/// peers need not agree on how one prints.
#[test]
fn floats_are_refused() {
    let e = from_value(&schema(), "SetGain", json!({ "gain": 0.5 })).unwrap_err();
    assert!(e.contains("not an integer"), "{e}");
}

#[test]
fn json_text_and_the_empty_case() {
    let a = from_json(&schema(), "AddSong", r#"{"title":"Glue"}"#).unwrap();
    let b = from_value(&schema(), "AddSong", json!({ "title": "Glue" })).unwrap();
    assert_eq!(a, b);
    // A verb with no arguments should not make its caller invent a `{}`.
    assert_eq!(
        from_json(&schema(), "FavoriteAll", "").unwrap(),
        from_value(&schema(), "FavoriteAll", json!({})).unwrap()
    );
}

#[test]
fn arguments_have_to_be_an_object() {
    let e = from_json(&schema(), "AddSong", "[1,2,3]").unwrap_err();
    assert!(e.contains("should be a json object"), "{e}");
    let e = from_json(&schema(), "AddSong", "not json").unwrap_err();
    assert!(e.contains("not json"), "{e}");
}
