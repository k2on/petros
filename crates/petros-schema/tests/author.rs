//! Authoring a mutation from JSON.
//!
//! The shape here is a contract with generated TypeScript: `petros-codegen`
//! reads a module's schema and emits calls that arrive as JSON, and the id
//! convention is documented there and implemented here.

use petros_schema::author::{from_json, from_value};
use petros_schema::cbor::Value;
use serde_json::json;

fn field<'a>(v: &'a Value, name: &str) -> Option<&'a Value> {
    let Value::Map(entries) = v else { return None };
    entries.iter().find_map(|(k, val)| match k {
        Value::Text(t) if t == name => Some(val),
        _ => None,
    })
}

#[test]
fn the_verb_travels_as_t() {
    let m = from_value("AddSong", json!({ "title": "Glue" })).unwrap();
    assert_eq!(field(&m, "t"), Some(&Value::Text("AddSong".into())));
    assert_eq!(field(&m, "title"), Some(&Value::Text("Glue".into())));
}

/// The convention: `id` and anything ending `_id` become sixteen bytes.
/// Everything else stays a string, including something that looks like a uuid.
#[test]
fn id_fields_become_bytes_and_others_do_not() {
    let m = from_value(
        "Favorite",
        json!({
            "id": "67e55084-765d-446c-9191-4ff9861f6d8e",
            "song_id": "67e55084-765d-446c-9191-4ff9861f6d8e",
            "title": "67e55084-765d-446c-9191-4ff9861f6d8e",
        }),
    )
    .unwrap();
    let bytes = vec![
        0x67, 0xe5, 0x50, 0x84, 0x76, 0x5d, 0x44, 0x6c, 0x91, 0x91, 0x4f, 0xf9, 0x86, 0x1f, 0x6d,
        0x8e,
    ];
    assert_eq!(field(&m, "id"), Some(&Value::Bytes(bytes.clone())));
    assert_eq!(field(&m, "song_id"), Some(&Value::Bytes(bytes)));
    assert!(
        matches!(field(&m, "title"), Some(Value::Text(_))),
        "only the name makes a field an id, not the contents"
    );
}

#[test]
fn an_id_that_is_not_one_is_refused() {
    let e = from_value("Favorite", json!({ "id": "nope" })).unwrap_err();
    assert!(e.contains("not an id"), "{e}");
}

/// No floats anywhere near the log: `apply` must not branch on one, and two
/// peers need not agree on how one prints.
#[test]
fn floats_are_refused() {
    let e = from_value("Add", json!({ "gain": 0.5 })).unwrap_err();
    assert!(e.contains("not an integer"), "{e}");
    // Nested too, or the check would be trivial to slip past.
    let e = from_value("Add", json!({ "opts": { "gain": 0.5 } })).unwrap_err();
    assert!(e.contains("not an integer"), "{e}");
}

#[test]
fn nested_ids_are_converted_too() {
    let m = from_value(
        "AddAlbum",
        json!({ "tracks": [{ "id": "67e55084-765d-446c-9191-4ff9861f6d8e" }] }),
    )
    .unwrap();
    let Some(Value::Array(tracks)) = field(&m, "tracks") else {
        panic!("no tracks")
    };
    assert!(matches!(field(&tracks[0], "id"), Some(Value::Bytes(_))));
}

#[test]
fn json_text_and_the_empty_case() {
    let a = from_json("AddSong", r#"{"title":"Glue"}"#).unwrap();
    let b = from_value("AddSong", json!({ "title": "Glue" })).unwrap();
    assert_eq!(a, b);
    // A verb with no arguments should not make its caller invent a `{}`.
    assert_eq!(
        from_json("FavoriteAll", "").unwrap(),
        from_value("FavoriteAll", json!({})).unwrap()
    );
}

#[test]
fn arguments_have_to_be_an_object() {
    let e = from_json("Add", "[1,2,3]").unwrap_err();
    assert!(e.contains("should be a json object"), "{e}");
    let e = from_json("Add", "not json").unwrap_err();
    assert!(e.contains("not json"), "{e}");
}
