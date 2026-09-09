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
    store.put(&one).unwrap();
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

    store.put(&song(1, "Glue", 1)).unwrap();
    match store.take_changes().as_slice() {
        [Change::Add { table, row }] => {
            assert_eq!(table, "song");
            assert_eq!(row[1], Value::Text("Glue".into()));
        }
        other => panic!("expected one add, got {other:?}"),
    }

    // An overwrite carries both versions. A view that sorts on `pos` needs the
    // old one to know where the row was.
    store.put(&song(1, "Glue (remastered)", 7)).unwrap();
    match store.take_changes().as_slice() {
        [Change::Edit { old, new, .. }] => {
            assert_eq!(old[1], Value::Text("Glue".into()));
            assert_eq!(old[3], Value::Int(1));
            assert_eq!(new[1], Value::Text("Glue (remastered)".into()));
            assert_eq!(new[3], Value::Int(7));
        }
        other => panic!("expected one edit, got {other:?}"),
    }

    store.delete::<Song>(&Song::key_of(&vec![1u8; 16])).unwrap();
    match store.take_changes().as_slice() {
        [Change::Remove { row, .. }] => {
            assert_eq!(row[1], Value::Text("Glue (remastered)".into()))
        }
        other => panic!("expected one remove, got {other:?}"),
    }

    // Deleting what is not there is a no-op, and a no-op is not a change: an
    // entry earlier in the log may have removed it already.
    store.delete::<Song>(&Song::key_of(&vec![1u8; 16])).unwrap();
    assert!(store.take_changes().is_empty());
}

/// Changes are drained, not accumulated. Whoever takes them owns them — they go
/// to a view, or they go nowhere because a savepoint rolled back.
#[test]
fn changes_are_drained() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    store.put(&song(1, "Glue", 1)).unwrap();
    assert_eq!(store.take_changes().len(), 1);
    assert!(store.take_changes().is_empty());
}

/// A read is a query, not a string, and its values are bound.
#[test]
fn a_read_is_a_query() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    for (i, title) in ["it's a quote", "'; DROP TABLE song; --", "unicode ✓ ♥ 漢"]
        .iter()
        .enumerate()
    {
        store.put(&song(i as u8, title, i as i64)).unwrap();
    }

    let all = store.select(Song::all().order_by(Song::pos.asc()));
    assert_eq!(all.len(), 3, "the table is still there");
    assert_eq!(all[1].title, "'; DROP TABLE song; --");

    let one = store.select(
        Song::all()
            .filter(Song::pos.ge(1))
            .order_by(Song::pos.asc()),
    );
    assert_eq!(one.len(), 2);
    assert_eq!(one[0].pos, 1);
}

/// A cursor seeks past a row in the query's order. It is what an incrementally
/// maintained `limit` uses to find the row that replaces a deleted one, and it
/// is here from the start because retrofitting it would mean rewriting every
/// source.
#[test]
fn a_query_can_start_after_a_row() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    for i in 0..5u8 {
        store.put(&song(i, &format!("song {i}"), i as i64)).unwrap();
    }

    let page = store.select(Song::all().order_by(Song::pos.asc()).limit(2));
    assert_eq!(page.iter().map(|s| s.pos).collect::<Vec<_>>(), vec![0, 1]);

    let next = store.select(
        Song::all()
            .order_by(Song::pos.asc())
            .start(&page[1])
            .limit(2),
    );
    assert_eq!(next.iter().map(|s| s.pos).collect::<Vec<_>>(), vec![2, 3]);

    // Descending flips the comparison, or a seek would run the wrong way.
    let down = store.select(Song::all().order_by(Song::pos.desc()).limit(2));
    assert_eq!(down.iter().map(|s| s.pos).collect::<Vec<_>>(), vec![4, 3]);
    let after = store.select(
        Song::all()
            .order_by(Song::pos.desc())
            .start(&down[1])
            .limit(2),
    );
    assert_eq!(after.iter().map(|s| s.pos).collect::<Vec<_>>(), vec![2, 1]);
}

fn favorite(song: u8, pos: i64) -> Favorite {
    Favorite {
        song_id: vec![song; 16],
        pos,
        favorited_ms: 0,
        actor: "alice".into(),
    }
}

/// `REFERENCES` is the declaration. Both directions of it are generated, so
/// neither the relationship nor its inverse is written down twice.
#[test]
fn a_foreign_key_generates_a_relationship_both_ways() {
    assert_eq!(Song::favorite.from, "id");
    assert_eq!(Song::favorite.to, "song_id");
    assert_eq!(Favorite::song.from, "song_id");
    assert_eq!(Favorite::song.to, "id");
}

/// A result is a tree: each song arrives with its own favourites hanging off
/// it, already grouped. A flat join would repeat the song once per favourite
/// and leave the caller to do this.
#[test]
fn related_rows_hang_off_their_parent() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    for (i, title) in ["Glue", "Apricots", "Opal"].iter().enumerate() {
        store.put(&song(i as u8, title, i as i64)).unwrap();
    }
    // Two on the first song, none on the second, one on the third.
    store.put(&favorite(0, 1)).unwrap();
    store.put(&favorite(2, 2)).unwrap();

    let rows = store.select_with(
        Song::all().order_by(Song::pos.asc()),
        Song::favorite,
        Favorite::all(),
    );

    let shape: Vec<(&str, usize)> = rows
        .iter()
        .map(|r| (r.row.title.as_str(), r.related.len()))
        .collect();
    assert_eq!(shape, vec![("Glue", 1), ("Apricots", 0), ("Opal", 1)]);
    assert_eq!(rows[0].one().unwrap().pos, 1);
    assert_eq!(rows[2].one().unwrap().pos, 2);
    // A parent with no children is still a row. This is the LEFT JOIN, and
    // dropping it would silently hide every unfavourited song from a library.
    assert!(rows[1].one().is_none());
}

/// The relationship reads the other way too, from the child to its parent,
/// which is the INNER JOIN a playlist screen wants: favourites, in playlist
/// order, each carrying the song it points at.
#[test]
fn a_relationship_reads_from_either_end() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    store.put(&song(0, "Glue", 0)).unwrap();
    store.put(&song(1, "Apricots", 1)).unwrap();
    store.put(&favorite(1, 1)).unwrap();
    store.put(&favorite(0, 2)).unwrap();

    let rows = store.select_with(
        Favorite::all().order_by(Favorite::pos.asc()),
        Favorite::song,
        Song::all(),
    );

    let titles: Vec<&str> = rows
        .iter()
        .map(|r| r.one().unwrap().title.as_str())
        .collect();
    assert_eq!(titles, vec!["Apricots", "Glue"]);
}

/// The related side is a query like any other, so it filters and orders. It is
/// fetched for the whole page at once — one statement, not one per parent —
/// which is why the child rows have to be grouped rather than merely appended.
#[test]
fn the_related_side_is_a_query() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    store.put(&song(0, "Glue", 0)).unwrap();
    store.put(&song(1, "Apricots", 1)).unwrap();
    store.put(&favorite(0, 5)).unwrap();
    store.put(&favorite(1, 3)).unwrap();

    let rows = store.select_with(
        Song::all().order_by(Song::pos.asc()),
        Song::favorite,
        Favorite::all().filter(Favorite::pos.gt(4)),
    );

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].related.len(), 1);
    assert_eq!(rows[0].one().unwrap().pos, 5);
    assert!(rows[1].related.is_empty());
}
