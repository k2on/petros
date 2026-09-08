//! Raw SQL in a mutation, verified before it ships.
//!
//! Reads and writes both, which is the point: one mechanism, and SQLite is the
//! thing that checks it. There is no second typed surface to keep in step.

use diesel::connection::SimpleConnection;
use petros::backend::SqliteStore;

const SCHEMA: &str = include_str!("../schema.sql");

fn db() -> petros::Connection {
    let mut conn = petros::open_memory().unwrap();
    conn.batch_execute(SCHEMA).unwrap();
    conn
}

fn add(store: &mut SqliteStore, id: u8, title: &str, pos: i64) {
    let id = vec![id; 16];
    let title = title.to_string();
    petros_sql::exec!(
        store,
        "INSERT INTO song (id, title, artist, pos) VALUES (?, ?, 'Bicep', ?)",
        id,
        title,
        pos
    );
}

/// A row comes back as a struct with a field per column, typed from what
/// SQLite says the column is declared as.
#[test]
fn a_query_returns_typed_rows() {
    let mut conn = db();
    let mut store = SqliteStore(&mut conn);
    add(&mut store, 1, "Glue", 1);
    add(&mut store, 2, "Opal", 2);

    let songs = petros_sql::query!(store, "SELECT id, title, pos FROM song ORDER BY pos, id");
    assert_eq!(songs.len(), 2);
    // `title` is a String and `pos` an i64 because the schema declares them so.
    let titles: Vec<&str> = songs.iter().map(|s| s.title.as_str()).collect();
    assert_eq!(titles, vec!["Glue", "Opal"]);
    assert_eq!(songs[0].pos, 1);
    assert_eq!(songs[0].id, vec![1u8; 16]);
}

/// An expression has no declared type, so it is named at the call site — the
/// same annotation sqlx asks for, for the same reason.
#[test]
fn an_expression_is_typed_by_its_alias() {
    let mut conn = db();
    let mut store = SqliteStore(&mut conn);
    add(&mut store, 1, "Glue", 7);

    let last = petros_sql::query_one!(
        store,
        "SELECT COALESCE(MAX(pos), 0) AS \"last: Int\" FROM song"
    )
    .map(|r| r.last)
    .unwrap_or(0);
    assert_eq!(last, 7);

    // And over nothing at all it is zero, which is what `MAX(pos) + 1` wants.
    let mut empty = db();
    let mut store = SqliteStore(&mut empty);
    let none = petros_sql::query_one!(
        store,
        "SELECT COALESCE(MAX(pos), 0) AS \"last: Int\" FROM song"
    )
    .map(|r| r.last)
    .unwrap_or(0);
    assert_eq!(none, 0);
}

/// A left join makes a column nullable and SQLite will not say so, so `?` does.
#[test]
fn a_nullable_column_is_an_option() {
    let mut conn = db();
    let mut store = SqliteStore(&mut conn);
    add(&mut store, 1, "Glue", 1);
    add(&mut store, 2, "Opal", 2);
    let id = vec![1u8; 16];
    petros_sql::exec!(
        store,
        "INSERT INTO favorite (song_id, pos, favorited_ms, actor) VALUES (?, 1, 0, 'alice')",
        id
    );

    let rows = petros_sql::query!(
        store,
        "SELECT s.title, f.pos AS \"place?: Int\"
           FROM song s LEFT JOIN favorite f ON f.song_id = s.id
          ORDER BY s.pos, s.id"
    );
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].place, Some(1), "on the playlist");
    assert_eq!(rows[1].place, None, "not on it");
}

/// The set operation the whole escape hatch is for: one statement, over
/// however many rows there are.
#[test]
fn a_set_operation_in_one_statement() {
    let mut conn = db();
    let mut store = SqliteStore(&mut conn);
    for i in 1..=5u8 {
        add(&mut store, i, "t", i as i64);
    }
    let three = vec![3u8; 16];
    petros_sql::exec!(
        store,
        "INSERT INTO favorite (song_id, pos, favorited_ms, actor) VALUES (?, 1, 10, 'bob')",
        three
    );

    let now = 1_700_000_000i64;
    let actor = "alice".to_string();
    petros_sql::exec!(
        store,
        "INSERT INTO favorite (song_id, pos, favorited_ms, actor)
         SELECT s.id,
                (SELECT COALESCE(MAX(pos), 0) FROM favorite)
                  + ROW_NUMBER() OVER (ORDER BY s.pos, s.id),
                ?, ?
           FROM song s
          WHERE NOT EXISTS (SELECT 1 FROM favorite f WHERE f.song_id = s.id)
          ORDER BY s.pos, s.id",
        now,
        actor
    );

    let playlist = petros_sql::query!(
        store,
        "SELECT song_id, pos, actor FROM favorite ORDER BY pos"
    );
    assert_eq!(playlist.len(), 5, "the four missing ones were added");
    let places: Vec<i64> = playlist.iter().map(|f| f.pos).collect();
    assert_eq!(places, vec![1, 2, 3, 4, 5], "each holds a distinct place");
    // The one already there kept its place and its author.
    assert_eq!((playlist[0].pos, playlist[0].actor.as_str()), (1, "bob"));
}

/// Values are bound, never pasted. The macro checks the statement; SQLite binds
/// the values.
#[test]
fn values_are_bound_not_pasted() {
    let mut conn = db();
    let mut store = SqliteStore(&mut conn);
    add(&mut store, 1, "Glue", 1);
    let nasty = "'; DROP TABLE song; --".to_string();
    petros_sql::exec!(
        store,
        "UPDATE song SET title = ? WHERE pos = ?",
        nasty,
        1i64
    );
    let all = petros_sql::query!(store, "SELECT title FROM song");
    assert_eq!(all.len(), 1, "the table is still there");
    assert_eq!(all[0].title, "'; DROP TABLE song; --");
}
