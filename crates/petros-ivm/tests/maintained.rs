//! A maintained view has to give the same answer as running the query.
//!
//! That is the only property that matters and every test here is a way of
//! doubting it: the operators are told about a change and then checked against
//! the database they were told about.

use diesel::connection::SimpleConnection;
use petros::backend::SqliteStore;
use petros_ivm::View;
use petros_schema::{Rows, Store};

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
        done: false,
        pos,
    }
}

/// The query the view maintains, run for real. Every test compares against
/// this rather than against a list written by hand, so the two cannot agree by
/// both being wrong in the same way.
fn rerun(store: &mut SqliteStore, query: petros_schema::Query<Song>) -> Vec<Song> {
    store.select(query)
}

fn titles(songs: &[Song]) -> Vec<&str> {
    songs.iter().map(|s| s.title.as_str()).collect()
}

/// Hand a view everything the store recorded, which is what a client does after
/// applying an entry from the log.
fn settle(view: &mut View<Song>, store: &mut SqliteStore) -> usize {
    let changes = store.take_changes();
    view.apply(store, &changes)
}

#[test]
fn a_view_tracks_inserts_and_deletes() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    store.put(&song(1, "Glue", 1)).unwrap();
    store.take_changes();

    let query = || Song::all().order_by(Song::pos.asc());
    let mut view = View::<Song>::new(query());
    view.hydrate(&mut store);
    assert_eq!(titles(&view.rows()), vec!["Glue"]);

    store.put(&song(2, "Opal", 2)).unwrap();
    settle(&mut view, &mut store);
    assert_eq!(titles(&view.rows()), vec!["Glue", "Opal"]);

    // A row that sorts into the middle has to land in the middle.
    store.put(&song(3, "Apricots", 0)).unwrap();
    settle(&mut view, &mut store);
    assert_eq!(titles(&view.rows()), vec!["Apricots", "Glue", "Opal"]);
    assert_eq!(view.rows(), rerun(&mut store, query()));

    store.delete::<Song>(&Song::key_of(&vec![1u8; 16])).unwrap();
    settle(&mut view, &mut store);
    assert_eq!(titles(&view.rows()), vec!["Apricots", "Opal"]);
    assert_eq!(view.rows(), rerun(&mut store, query()));
}

/// A view of one table must not move because another one did.
#[test]
fn a_change_to_another_table_is_not_this_view() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    store.put(&song(1, "Glue", 1)).unwrap();
    store.take_changes();

    let mut view = View::<Song>::new(Song::all().order_by(Song::pos.asc()));
    view.hydrate(&mut store);

    store
        .put(&Other {
            id: vec![9; 16],
            pos: 1,
        })
        .unwrap();
    settle(&mut view, &mut store);
    assert_eq!(titles(&view.rows()), vec!["Glue"]);
}

/// An edit can cross the predicate in either direction, and then it is not an
/// edit any more. A row that stops qualifying has to leave the view.
#[test]
fn an_edit_across_the_filter_becomes_an_add_or_a_remove() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    for i in 1..=3 {
        store.put(&song(i, &format!("song {i}"), i as i64)).unwrap();
    }
    store.take_changes();

    let query = || {
        Song::all()
            .filter(Song::done.eq(false))
            .order_by(Song::pos.asc())
    };
    let mut view = View::<Song>::new(query());
    view.hydrate(&mut store);
    assert_eq!(view.len(), 3);

    // Out: it was in the view and no longer qualifies.
    let mut two = store.get::<Song>(&Song::key_of(&vec![2u8; 16])).unwrap();
    two.done = true;
    store.put(&two).unwrap();
    settle(&mut view, &mut store);
    assert_eq!(titles(&view.rows()), vec!["song 1", "song 3"]);
    assert_eq!(view.rows(), rerun(&mut store, query()));

    // Back in, at the right place rather than at the end.
    two.done = false;
    store.put(&two).unwrap();
    settle(&mut view, &mut store);
    assert_eq!(titles(&view.rows()), vec!["song 1", "song 2", "song 3"]);
    assert_eq!(view.rows(), rerun(&mut store, query()));

    // An edit that stays outside the filter is nothing at all.
    let mut three = store.get::<Song>(&Song::key_of(&vec![3u8; 16])).unwrap();
    three.done = true;
    store.put(&three).unwrap();
    settle(&mut view, &mut store);
    three.title = "renamed while hidden".into();
    store.put(&three).unwrap();
    settle(&mut view, &mut store);
    assert_eq!(titles(&view.rows()), vec!["song 1", "song 2"]);
    assert_eq!(view.rows(), rerun(&mut store, query()));
}

/// The case the design exists for: a delete inside the top N. The row that
/// should replace it was never in memory, so the operator seeks for it.
#[test]
fn a_delete_inside_a_limit_pulls_in_a_replacement() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    for i in 1..=10 {
        store.put(&song(i, &format!("song {i}"), i as i64)).unwrap();
    }
    store.take_changes();

    let query = || Song::all().order_by(Song::pos.asc()).limit(3);
    let mut view = View::<Song>::new(query());
    view.hydrate(&mut store);
    assert_eq!(titles(&view.rows()), vec!["song 1", "song 2", "song 3"]);

    store.delete::<Song>(&Song::key_of(&vec![2u8; 16])).unwrap();
    settle(&mut view, &mut store);
    // song 4 was outside the window and is now in it.
    assert_eq!(titles(&view.rows()), vec!["song 1", "song 3", "song 4"]);
    assert_eq!(view.rows(), rerun(&mut store, query()));

    // And again, to prove the bound moved rather than being seeded once.
    store.delete::<Song>(&Song::key_of(&vec![1u8; 16])).unwrap();
    settle(&mut view, &mut store);
    assert_eq!(titles(&view.rows()), vec!["song 3", "song 4", "song 5"]);
    assert_eq!(view.rows(), rerun(&mut store, query()));
}

/// An insert that sorts inside a full window evicts the last row.
#[test]
fn an_insert_inside_a_full_limit_pushes_the_last_row_out() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    for i in 1..=5 {
        store
            .put(&song(i, &format!("song {i}"), (i as i64) * 10))
            .unwrap();
    }
    store.take_changes();

    let query = || Song::all().order_by(Song::pos.asc()).limit(3);
    let mut view = View::<Song>::new(query());
    view.hydrate(&mut store);

    store.put(&song(9, "queue jumper", 15)).unwrap();
    settle(&mut view, &mut store);
    assert_eq!(
        titles(&view.rows()),
        vec!["song 1", "queue jumper", "song 2"]
    );
    assert_eq!(view.rows(), rerun(&mut store, query()));

    // Then delete the jumper: song 3 comes back from outside the window.
    store.delete::<Song>(&Song::key_of(&vec![9u8; 16])).unwrap();
    settle(&mut view, &mut store);
    assert_eq!(titles(&view.rows()), vec!["song 1", "song 2", "song 3"]);
    assert_eq!(view.rows(), rerun(&mut store, query()));
}

/// An append past a full window is the common case in a long list, and it has
/// to cost nothing: the view does not move and the row is not held.
#[test]
fn an_append_past_a_full_limit_does_not_reach_the_view() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    for i in 1..=3 {
        store.put(&song(i, &format!("song {i}"), i as i64)).unwrap();
    }
    store.take_changes();

    let query = || Song::all().order_by(Song::pos.asc()).limit(3);
    let mut view = View::<Song>::new(query());
    view.hydrate(&mut store);
    let before = view.rows();

    for i in 4..=50 {
        store.put(&song(i, &format!("song {i}"), i as i64)).unwrap();
        // Nothing reached the view: not an add that was then evicted, but no
        // work at all. Asserting the *answer* here would pass either way, and
        // the claim is about cost.
        assert_eq!(
            settle(&mut view, &mut store),
            0,
            "song {i} reached the view"
        );
    }
    assert_eq!(view.rows(), before);
    assert_eq!(view.rows(), rerun(&mut store, query()));
}

/// The property, over a long random session: whatever the mutations were, the
/// maintained answer is the answer.
#[test]
fn a_maintained_view_agrees_with_a_re_run_over_a_random_session() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    let query = || {
        Song::all()
            .filter(Song::done.eq(false))
            .order_by(Song::pos.asc())
            .order_by(Song::id.asc())
            .limit(5)
    };
    let mut view = View::<Song>::new(query());
    view.hydrate(&mut store);

    // A cheap deterministic shuffle, so a failure is reproducible.
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };

    for step in 0..2000u64 {
        let id = (next() % 30) as u8;
        let key = Song::key_of(&vec![id; 16]);
        match next() % 4 {
            0 => store.delete::<Song>(&key).unwrap(),
            1 => {
                if let Some(mut s) = store.get::<Song>(&key) {
                    s.done = !s.done;
                    store.put(&s).unwrap();
                }
            }
            2 => {
                if let Some(mut s) = store.get::<Song>(&key) {
                    s.pos = (next() % 50) as i64;
                    store.put(&s).unwrap();
                }
            }
            _ => store
                .put(&Song {
                    id: vec![id; 16],
                    title: format!("song {id}"),
                    done: next() % 5 == 0,
                    pos: (next() % 50) as i64,
                })
                .unwrap(),
        }
        settle(&mut view, &mut store);
        assert_eq!(
            view.rows(),
            rerun(&mut store, query()),
            "diverged at step {step}"
        );
    }
}
