//! Reads are SQL, verified before they ship. Writes are typed, and say what
//! they changed.

use diesel::connection::SimpleConnection;
use petros::backend::SqliteStore;
use petros_schema::{Change, Rows, Store, Value};

const SCHEMA: &str = include_str!("../schema.sql");
petros_sql::tables!();

fn db() -> petros::Connection {
    let mut conn = petros::open_memory().unwrap();
    conn.batch_execute(SCHEMA).unwrap();
    conn
}

fn song(id: u8, title: &str, pos: i64) -> Song {
    Song {
        id: vec![id; 16],
        title: title.into(),
        artist: "Bicep".into(),
        pos,
    }
}

/// The row types come out of the schema, so the columns and their types are
/// whatever `schema.sql` says and there is nothing to keep in step.
#[test]
fn the_row_type_matches_the_table() {
    use petros_schema::Table;
    assert_eq!(Song::DEF.name, "song");
    assert_eq!(Song::DEF.columns, &["id", "title", "artist", "pos"]);
    assert_eq!(Song::DEF.key, &["id"]);
    assert_eq!(Favorite::DEF.key, &["song_id"]);
}

#[test]
fn a_row_survives_the_round_trip() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);

    let one = song(1, "Glue", 1);
    store.put(&one);
    assert_eq!(store.get::<Song>(&Song::key_of(&vec![1u8; 16])), Some(one));
    assert!(store.exists::<Song>(&Song::key_of(&vec![1u8; 16])));
    assert!(!store.exists::<Song>(&Song::key_of(&vec![9u8; 16])));
}

/// The reason typed writes are back: a write says which row moved and what it
/// was. `UPDATE … WHERE` could never say either.
#[test]
fn a_write_says_what_changed() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);

    store.put(&song(1, "Glue", 1));
    match store.take_changes().as_slice() {
        [Change::Add { table, row }] => {
            assert_eq!(table, "song");
            assert_eq!(row[1], Value::Text("Glue".into()));
        }
        other => panic!("expected one add, got {other:?}"),
    }

    // An overwrite carries both versions. A view that sorts on `pos` needs the
    // old one to know where the row was.
    store.put(&song(1, "Glue (remastered)", 7));
    match store.take_changes().as_slice() {
        [Change::Edit { old, new, .. }] => {
            assert_eq!(old[1], Value::Text("Glue".into()));
            assert_eq!(old[3], Value::Int(1));
            assert_eq!(new[1], Value::Text("Glue (remastered)".into()));
            assert_eq!(new[3], Value::Int(7));
        }
        other => panic!("expected one edit, got {other:?}"),
    }

    store.delete::<Song>(&Song::key_of(&vec![1u8; 16]));
    match store.take_changes().as_slice() {
        [Change::Remove { row, .. }] => {
            assert_eq!(row[1], Value::Text("Glue (remastered)".into()))
        }
        other => panic!("expected one remove, got {other:?}"),
    }

    // Deleting what is not there is a no-op, and a no-op is not a change: an
    // entry earlier in the log may have removed it already.
    store.delete::<Song>(&Song::key_of(&vec![1u8; 16]));
    assert!(store.take_changes().is_empty());
}

/// Changes are drained, not accumulated. Whoever takes them owns them — they go
/// to a view, or they go nowhere because a savepoint rolled back.
#[test]
fn changes_are_drained() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    store.put(&song(1, "Glue", 1));
    assert_eq!(store.take_changes().len(), 1);
    assert!(store.take_changes().is_empty());
}

/// A read is still SQL, still checked, and still binds rather than pastes.
#[test]
fn reads_are_sql_and_values_are_bound() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    for (i, title) in ["it's a quote", "'; DROP TABLE song; --", "unicode ✓ ♥ 漢"]
        .iter()
        .enumerate()
    {
        store.put(&song(i as u8, title, i as i64));
    }

    let all = petros_sql::query!(store, "SELECT title, pos FROM song ORDER BY pos, title");
    assert_eq!(all.len(), 3, "the table is still there");
    assert_eq!(all[1].title, "'; DROP TABLE song; --");
    assert_eq!(all[2].pos, 2);
}
