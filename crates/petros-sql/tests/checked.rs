//! What the macro is for: the set operation the typed store cannot express,
//! written as SQL and verified before it ships.

use diesel::connection::SimpleConnection;
use petros::backend::SqliteStore;
use petros_schema::Store;

petros_schema::tables! {
    Song "song" key (id: Blob) {
        id: Blob, title: Text, artist: Text, pos: Int,
    }
    Favorite "favorite" key (song_id: Blob) {
        song_id: Blob, pos: Int, favorited_ms: Int, actor: Text,
    }
}

/// The declaration and the file the macro checks against have to agree, or the
/// check is against a schema nobody runs. One line, and it cannot drift.
#[test]
fn the_schema_file_matches_the_declaration() {
    // Compared without whitespace, because the file is meant to be readable
    // and the generated DDL is meant to be one line. What must not differ is a
    // table, a column or a type.
    fn normal(sql: &str) -> String {
        sql.lines()
            .filter(|l| !l.trim_start().starts_with("--"))
            .collect::<String>()
            .split_whitespace()
            .collect()
    }
    assert_eq!(
        normal(&ddl()),
        normal(include_str!("../schema.sql")),
        "schema.sql and the tables! declaration disagree — the macro would be \
         checking call sites against a schema nobody runs"
    );
}

fn db() -> petros::Connection {
    let mut conn = petros::open_memory().unwrap();
    conn.batch_execute(&ddl()).unwrap();
    conn
}

fn song(id: u8, pos: i64) -> Song {
    Song {
        id: vec![id; 16],
        title: format!("track {id}"),
        artist: "Bicep".into(),
        pos,
    }
}

/// `FavoriteAll`: one statement, over however many rows there are.
///
/// Through the typed store this is a scan plus a write per row, which on a
/// phone is a boundary crossing per row.
#[test]
fn a_set_operation_in_one_statement() {
    let mut conn = db();
    let mut store = SqliteStore(&mut conn);
    for i in 1..=5u8 {
        store.put(&song(i, i as i64));
    }
    // One already on the playlist, so the anti-join has something to skip.
    store.put(&Favorite {
        song_id: vec![3u8; 16],
        pos: 1,
        favorited_ms: 10,
        actor: "bob".into(),
    });

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

    let playlist = store.scan::<Favorite>();
    assert_eq!(playlist.len(), 5, "the four missing ones were added");
    let mut places: Vec<i64> = playlist.iter().map(|f| f.pos).collect();
    places.sort_unstable();
    assert_eq!(places, vec![1, 2, 3, 4, 5], "each holds a distinct place");
    // The one that was already there kept its place and its author.
    let kept = playlist
        .iter()
        .find(|f| f.song_id == vec![3u8; 16])
        .unwrap();
    assert_eq!((kept.pos, kept.actor.as_str()), (1, "bob"));
}

/// Values still bind. The macro checks the statement; it does not paste.
#[test]
fn values_are_bound_not_pasted() {
    let mut conn = db();
    let mut store = SqliteStore(&mut conn);
    store.put(&song(1, 1));
    let nasty = "'; DROP TABLE song; --".to_string();
    petros_sql::exec!(
        store,
        "UPDATE song SET title = ? WHERE pos = ?",
        nasty,
        1i64
    );
    let all = store.scan::<Song>();
    assert_eq!(all.len(), 1, "the table is still there");
    assert_eq!(all[0].title, "'; DROP TABLE song; --");
}
