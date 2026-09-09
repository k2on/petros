//! What `#[mutation]` generates, exercised against a real store.

use petros_schema::cbor::Value;
use petros_schema::Store;

// Stand-ins for the engine's parameter types. In an app these come from
// `petros`; the macro recognises them by name, which is what lets a function be
// ordinary Rust with no attribute arguments — and what lets this test supply
// its own store without one.
#[allow(dead_code)]
type Db = Recorder;
#[allow(dead_code)]
type NewId = petros_schema::Id;
#[allow(dead_code)]
type Now = i64;
#[allow(dead_code)]
type Actor<'a> = &'a str;

/// A store that remembers what it was asked, so a test can see what `apply`
/// did without a database.
#[derive(Default)]
struct Recorder {
    writes: Vec<(String, Vec<petros_schema::Value>)>,
}

impl Store for Recorder {
    fn put_row(&mut self, table: &str, row: &[petros_schema::Value]) -> Result<(), String> {
        self.writes.push((table.to_string(), row.to_vec()));
        Ok(())
    }
    fn delete_row(&mut self, _table: &str, _key: &[petros_schema::Value]) -> Result<(), String> {
        Ok(())
    }
    fn get_row(
        &mut self,
        _table: &str,
        _key: &[petros_schema::Value],
    ) -> Option<Vec<petros_schema::Value>> {
        None
    }
    fn take_changes(&mut self) -> Vec<petros_schema::Change> {
        Vec::new()
    }
    fn fetch(&mut self, _plan: &petros_schema::Plan) -> Vec<Vec<petros_schema::Value>> {
        Vec::new()
    }
}

/// Put a song in the library.
#[petros_macros::mutation]
pub fn add_song(
    db: &mut Db,
    id: NewId,
    added_ms: Now,
    actor: Actor,
    title: String,
    artist: String,
) -> Result<(), String> {
    if title.trim().is_empty() {
        return Err("a song needs a title".into());
    }
    db.put_row(
        "song",
        &[
            petros_schema::Value::Blob(id),
            petros_schema::Value::Text(title),
            petros_schema::Value::Text(artist),
            petros_schema::Value::Int(added_ms),
            petros_schema::Value::Text(actor.to_string()),
        ],
    )
    .unwrap();
    Ok(())
}

fn field<'a>(v: &'a Value, name: &str) -> Option<&'a Value> {
    petros_schema::cbor::field(v, name)
}

/// The authoring half: a caller passes only its own arguments, and the verb
/// travels in `t`.
#[test]
fn authoring_writes_the_verb_and_the_arguments() {
    let m = add_song("Glue".to_string(), "Bicep".to_string());
    assert_eq!(field(&m, "t"), Some(&Value::Text("AddSong".into())));
    assert_eq!(field(&m, "title"), Some(&Value::Text("Glue".into())));
    assert_eq!(field(&m, "artist"), Some(&Value::Text("Bicep".into())));
    // `id` and `added_ms` are `fill_auto`'s to add, not the caller's.
    assert_eq!(field(&m, "id"), None);
    assert_eq!(field(&m, "added_ms"), None);
}

/// The declaration the module carries, which is what the code generator reads.
#[test]
fn the_declaration_is_recoverable() {
    assert_eq!(add_song::VERB, "AddSong");
    assert_eq!(
        add_song::ARGS,
        &[
            ("title", petros_schema::Ty::Text),
            ("artist", petros_schema::Ty::Text)
        ]
    );
    assert_eq!(add_song::AUTO, &[("id", true), ("added_ms", false)]);
}

/// The applying half: the body runs against whatever the entry froze.
#[test]
fn applying_runs_the_body_against_the_store() {
    let mut m = add_song("Glue".to_string(), "Bicep".to_string());
    petros_schema::cbor::set(&mut m, "id", Value::Bytes(vec![7; 16]));
    petros_schema::cbor::set(&mut m, "added_ms", Value::Integer(1234.into()));

    let mut db = Recorder::default();
    __petros_apply_add_song(&mut db, &m, "alice").expect("applies");

    assert_eq!(db.writes.len(), 1);
    let (table, params) = &db.writes[0];
    assert_eq!(table, "song");
    assert_eq!(params[0], petros_schema::Value::Blob(vec![7; 16]));
    assert_eq!(params[1], petros_schema::Value::Text("Glue".into()));
    assert_eq!(params[3], petros_schema::Value::Int(1234));
    assert_eq!(params[4], petros_schema::Value::Text("alice".into()));
}

/// A refusal is a deterministic verdict, not an error: every replica reaches
/// it identically, so it has to come out of the body the same way.
#[test]
fn a_refusal_comes_back_from_the_body() {
    let mut m = add_song("   ".to_string(), "nobody".to_string());
    petros_schema::cbor::set(&mut m, "id", Value::Bytes(vec![1; 16]));
    petros_schema::cbor::set(&mut m, "added_ms", Value::Integer(1.into()));

    let mut db = Recorder::default();
    let refused = __petros_apply_add_song(&mut db, &m, "alice");
    assert_eq!(refused, Err("a song needs a title".to_string()));
    assert!(db.writes.is_empty(), "nothing was written");
}

/// Take a song back out of the playlist.
#[petros_macros::mutation]
pub fn unfavorite(db: &mut Db, id: petros_schema::Id) -> Result<(), String> {
    db.delete_row("favorite", &[petros_schema::Value::Blob(id)])
        .unwrap();
    Ok(())
}

petros_macros::peer!(add_song, unfavorite);

/// Dispatch turns a verb read out of the log into a call, and says what it
/// knows when it cannot.
#[test]
fn dispatch_routes_by_verb() {
    let mut m = add_song("Glue".into(), "Bicep".into());
    fill_auto(&mut m, vec![3; 16], 99);
    let mut db = Recorder::default();
    apply(&mut db, &m, "alice").expect("routed to add_song");
    assert_eq!(db.writes.len(), 1);

    let mut m = unfavorite(vec![4; 16]);
    fill_auto(&mut m, vec![0; 16], 0);
    apply(&mut db, &m, "alice").expect("routed to unfavorite");
    // `unfavorite` deletes, and a delete is not a write this recorder keeps.
    assert_eq!(db.writes.len(), 1);

    let unknown = Value::Map(vec![(
        Value::Text("t".into()),
        Value::Text("Frobnicate".into()),
    )]);
    let e = apply(&mut db, &unknown, "alice").unwrap_err();
    assert!(e.contains("Frobnicate"), "{e}");
    assert!(e.contains("AddSong"), "it says what it does know: {e}");
}

/// What a verb wants filled is not written anywhere: it is what the function
/// asked for by taking a `NewId` or a `Now`.
#[test]
fn fill_auto_follows_the_signature() {
    let mut m = add_song("Glue".into(), "Bicep".into());
    fill_auto(&mut m, vec![9; 16], 4242);
    assert_eq!(field(&m, "id"), Some(&Value::Bytes(vec![9; 16])));
    assert_eq!(field(&m, "added_ms"), Some(&Value::Integer(4242.into())));

    // `unfavorite` takes neither, so neither is added.
    let mut m = unfavorite(vec![1; 16]);
    fill_auto(&mut m, vec![9; 16], 4242);
    assert_eq!(
        field(&m, "id"),
        Some(&Value::Bytes(vec![1; 16])),
        "the argument, untouched"
    );
    assert_eq!(field(&m, "added_ms"), None);
}

#[test]
fn the_schema_is_every_verb_and_its_arguments() {
    let s = schema();
    assert_eq!(s.names(), ["AddSong", "Unfavorite"]);
    assert_eq!(s.verb("AddSong").unwrap().args.len(), 2);
}
