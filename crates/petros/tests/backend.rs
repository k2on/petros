//! The typed store over real SQLite.
//!
//! What is being checked is that `apply` can read and write without ever
//! naming a type twice or building a statement — and that the values survive
//! the round trip through bound parameters intact, including the ones that
//! break naive escaping.

use diesel::connection::SimpleConnection;
use petros::backend::SqliteStore;
use petros_schema::Store;

petros_schema::tables! {
    /// A song in the library.
    Song "song" key (id: Blob) {
        id: Blob,
        title: Text,
        artist: Text,
        pos: Int,
        starred: Bool,
    }

    /// The favourites playlist, which is a playlist and not a flag.
    Favorite "favorite" key (song_id: Blob) {
        song_id: Blob,
        pos: Int,
    }
}

fn store() -> petros::Connection {
    let mut conn = petros::open_memory().unwrap();
    conn.batch_execute(&ddl()).expect("the generated DDL runs");
    conn
}

fn song(id: u8, title: &str, pos: i64) -> Song {
    Song {
        id: vec![id; 16],
        title: title.into(),
        artist: "Bicep".into(),
        pos,
        starred: false,
    }
}

#[test]
fn a_row_survives_the_round_trip() {
    let mut conn = store();
    let mut db = SqliteStore(&mut conn);

    let one = song(1, "Glue", 1);
    db.put(&one);

    let back: Song = db.get(&Song::key_of(&vec![1u8; 16])).expect("it is there");
    assert_eq!(back, one, "what went in is what comes out");
    assert!(db.exists::<Song>(&Song::key_of(&vec![1u8; 16])));
    assert!(!db.exists::<Song>(&Song::key_of(&vec![9u8; 16])));
}

/// The reason to bind rather than interpolate. Every one of these would need a
/// different escaping rule if the values were being pasted into SQL.
#[test]
fn values_that_would_break_escaping() {
    let mut conn = store();
    let mut db = SqliteStore(&mut conn);

    let nasty = [
        "it's a quote",
        "'; DROP TABLE song; --",
        "back\\slash",
        "new\nline",
        "unicode ✓ ♥ 漢",
        "",
    ];
    for (i, title) in nasty.iter().enumerate() {
        db.put(&song(i as u8, title, i as i64));
    }

    let all = db.scan::<Song>();
    assert_eq!(all.len(), nasty.len(), "the table is still there");
    let mut titles: Vec<&str> = all.iter().map(|s| s.title.as_str()).collect();
    titles.sort_unstable();
    let mut expected: Vec<&str> = nasty.to_vec();
    expected.sort_unstable();
    assert_eq!(titles, expected);
}

#[test]
fn scan_is_ordered_and_max_is_typed() {
    let mut conn = store();
    let mut db = SqliteStore(&mut conn);
    for (i, pos) in [(3u8, 30i64), (1, 10), (2, 20)] {
        db.put(&song(i, "t", pos));
    }

    // Primary-key order, explicitly, because SQLite's natural order is not a
    // contract and two replicas have to agree.
    let ids: Vec<u8> = db.scan::<Song>().iter().map(|s| s.id[0]).collect();
    assert_eq!(ids, vec![1, 2, 3]);

    assert_eq!(db.max(Song::pos), 30);
    // An empty table is zero, which is what `MAX(pos) + 1` wants to mean.
    assert_eq!(db.max(Favorite::pos), 0);
}

#[test]
fn put_replaces_and_delete_removes() {
    let mut conn = store();
    let mut db = SqliteStore(&mut conn);

    db.put(&song(1, "Glue", 1));
    // The same entry arriving twice has to be a no-op, not a constraint error.
    db.put(&song(1, "Glue (remastered)", 1));
    assert_eq!(db.scan::<Song>().len(), 1);
    assert_eq!(db.scan::<Song>()[0].title, "Glue (remastered)");

    db.delete::<Song>(&Song::key_of(&vec![1u8; 16]));
    assert!(db.scan::<Song>().is_empty());
    // Deleting what is not there is a no-op, because an earlier entry in the
    // log may have removed it already.
    db.delete::<Song>(&Song::key_of(&vec![1u8; 16]));
}

#[test]
fn booleans_and_blobs_keep_their_types() {
    let mut conn = store();
    let mut db = SqliteStore(&mut conn);
    let mut one = song(7, "Opal", 1);
    one.starred = true;
    db.put(&one);
    let back: Song = db.get(&Song::key_of(&vec![7u8; 16])).unwrap();
    assert!(back.starred);
    assert_eq!(back.id, vec![7u8; 16]);
}
