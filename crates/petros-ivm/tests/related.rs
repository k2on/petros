//! A maintained tree: a row with its children, kept right without re-running.
//!
//! The case this exists for is the one a flat view cannot express. Hearting a
//! song does not add, remove or edit any *song* — and yet a library screen has
//! to move. That is Zero's fourth kind of change, and everything here is a way
//! of doubting that it works.

use diesel::connection::SimpleConnection;
use petros::backend::SqliteStore;
use petros_ivm::View;
use petros_schema::{Rows, Store, With};

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

fn heart(song: u8, pos: i64) -> Favorite {
    Favorite {
        song_id: vec![song; 16],
        pos,
    }
}

/// The same query, run for real. Every test compares against this rather than
/// against a list written by hand.
fn rerun(store: &mut SqliteStore) -> Vec<With<Song, Favorite>> {
    store.select_with(
        Song::all()
            .order_by(Song::pos.asc())
            .order_by(Song::id.asc()),
        Song::favorite,
        Favorite::all().order_by(Favorite::pos.asc()),
    )
}

fn view() -> View<Song> {
    View::related(
        Song::all()
            .order_by(Song::pos.asc())
            .order_by(Song::id.asc()),
        Song::favorite,
        Favorite::all().order_by(Favorite::pos.asc()),
    )
}

fn settle(view: &mut View<Song>, store: &mut SqliteStore) -> usize {
    let changes = store.take_changes();
    view.apply(store, &changes)
}

/// What the screen shows: a title, and whether it is hearted.
fn shape(rows: &[With<Song, Favorite>]) -> Vec<(&str, bool)> {
    rows.iter()
        .map(|r| (r.row.title.as_str(), r.one().is_some()))
        .collect()
}

/// The whole point. A write to the *child* table is not a change to any parent
/// row, and the view still has to move.
#[test]
fn hearting_a_song_moves_a_view_of_songs() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    store.put(&song(1, "Glue", 1));
    store.put(&song(2, "Opal", 2));
    store.take_changes();

    let mut view = view();
    view.hydrate(&mut store);
    assert_eq!(shape(&view.with()), vec![("Glue", false), ("Opal", false)]);

    store.put(&heart(2, 1));
    assert!(settle(&mut view, &mut store) > 0, "the view heard about it");
    assert_eq!(shape(&view.with()), vec![("Glue", false), ("Opal", true)]);
    assert_eq!(view.with::<Favorite>(), rerun(&mut store));

    // And back off again.
    store.delete::<Favorite>(&Favorite::key_of(&vec![2u8; 16]));
    settle(&mut view, &mut store);
    assert_eq!(shape(&view.with()), vec![("Glue", false), ("Opal", false)]);
    assert_eq!(view.with::<Favorite>(), rerun(&mut store));
}

/// A parent that enters the view arrives with its children already on it,
/// rather than blank for a frame and then filled in by a second change.
///
/// It enters by being edited across the filter, because a foreign key means a
/// child cannot exist before its parent — the obvious version of this test was
/// written first and passed for the wrong reason: the orphan was never written.
#[test]
fn a_parent_arrives_hydrated() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    let mut hidden = song(5, "Sundial", 1);
    hidden.done = true;
    store.put(&hidden);
    store.put(&heart(5, 1));
    store.take_changes();

    let mut view = View::<Song>::related(
        Song::all()
            .filter(Song::done.eq(false))
            .order_by(Song::pos.asc())
            .order_by(Song::id.asc()),
        Song::favorite,
        Favorite::all().order_by(Favorite::pos.asc()),
    );
    view.hydrate(&mut store);
    assert!(view.is_empty());

    hidden.done = false;
    store.put(&hidden);
    settle(&mut view, &mut store);

    let rows = view.with::<Favorite>();
    assert_eq!(shape(&rows), vec![("Sundial", true)]);
    assert_eq!(
        rows[0].one().unwrap().pos,
        1,
        "the heart came with it, in one change"
    );
}

/// Editing a child in place is an edit, not a remove and an add: the parent
/// keeps its identity and its position.
#[test]
fn a_child_edited_in_place_stays_under_its_parent() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    store.put(&song(1, "Glue", 1));
    store.put(&heart(1, 1));
    store.take_changes();

    let mut view = view();
    view.hydrate(&mut store);

    let mut moved = store
        .get::<Favorite>(&Favorite::key_of(&vec![1u8; 16]))
        .unwrap();
    moved.pos = 9;
    store.put(&moved);
    settle(&mut view, &mut store);

    let rows = view.with::<Favorite>();
    assert_eq!(rows.len(), 1, "the song did not leave and come back");
    assert_eq!(rows[0].one().unwrap().pos, 9);
    assert_eq!(rows, rerun(&mut store));
}

/// A child moved onto a different parent leaves one and joins the other. An
/// edit passed through whole would leave the old parent holding it.
#[test]
fn a_child_that_changes_parent_leaves_the_old_one() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    store.put(&song(1, "Glue", 1));
    store.put(&song(2, "Opal", 2));
    store.take_changes();

    let mut view = view();
    view.hydrate(&mut store);

    // `song_id` is the key here, so moving the relationship is a delete and an
    // insert at the store — which is exactly what the view must survive.
    store.put(&heart(1, 1));
    settle(&mut view, &mut store);
    assert_eq!(shape(&view.with()), vec![("Glue", true), ("Opal", false)]);

    store.delete::<Favorite>(&Favorite::key_of(&vec![1u8; 16]));
    store.put(&heart(2, 1));
    settle(&mut view, &mut store);
    assert_eq!(shape(&view.with()), vec![("Glue", false), ("Opal", true)]);
    assert_eq!(view.with::<Favorite>(), rerun(&mut store));
}

/// A child of a parent the filter refuses is not this view's business.
#[test]
fn a_child_of_an_excluded_parent_costs_nothing() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    store.put(&song(1, "Glue", 1));
    let mut hidden = song(2, "Opal", 2);
    hidden.done = true;
    store.put(&hidden);
    store.take_changes();

    let mut view = View::<Song>::related(
        Song::all()
            .filter(Song::done.eq(false))
            .order_by(Song::pos.asc())
            .order_by(Song::id.asc()),
        Song::favorite,
        Favorite::all().order_by(Favorite::pos.asc()),
    );
    view.hydrate(&mut store);
    assert_eq!(view.len(), 1);

    // Hearting the hidden song reaches nothing: the parent is not in the view,
    // so there is no node to change. Asserting the answer alone would pass
    // even if the whole tree were rebuilt.
    store.put(&heart(2, 1));
    assert_eq!(settle(&mut view, &mut store), 0);
    assert_eq!(shape(&view.with()), vec![("Glue", false)]);
}

/// The property, over a long random session: whatever the mutations were, the
/// maintained tree is the tree.
#[test]
fn a_maintained_tree_agrees_with_a_re_run_over_a_random_session() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    let mut view = view();
    view.hydrate(&mut store);

    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        seed
    };

    for step in 0..2000u64 {
        let id = (next() % 12) as u8;
        match next() % 6 {
            0 => store.delete::<Song>(&Song::key_of(&vec![id; 16])),
            1 => store.delete::<Favorite>(&Favorite::key_of(&vec![id; 16])),
            2 => store.put(&heart(id, (next() % 20) as i64)),
            3 => {
                if let Some(mut s) = store.get::<Song>(&Song::key_of(&vec![id; 16])) {
                    s.pos = (next() % 20) as i64;
                    store.put(&s);
                }
            }
            4 => {
                if let Some(mut f) = store.get::<Favorite>(&Favorite::key_of(&vec![id; 16])) {
                    f.pos = (next() % 20) as i64;
                    store.put(&f);
                }
            }
            _ => store.put(&song(id, &format!("song {id}"), (next() % 20) as i64)),
        }
        settle(&mut view, &mut store);
        assert_eq!(
            view.with::<Favorite>(),
            rerun(&mut store),
            "diverged at step {step}"
        );
    }
}

fn note(id: u8, song: u8, text: &str) -> Note {
    Note {
        id: vec![id; 16],
        song_id: vec![song; 16],
        text: text.into(),
    }
}

fn notes(store: &mut SqliteStore) -> Vec<With<Song, Note>> {
    store.select_with(
        Song::all()
            .order_by(Song::pos.asc())
            .order_by(Song::id.asc()),
        Song::note,
        Note::all().order_by(Note::id.asc()),
    )
}

fn note_view() -> View<Song> {
    View::related(
        Song::all()
            .order_by(Song::pos.asc())
            .order_by(Song::id.asc()),
        Song::note,
        Note::all().order_by(Note::id.asc()),
    )
}

/// A child edited onto a *different* parent leaves one and joins the other.
///
/// Passing it through as a single edit would leave the old parent holding a
/// child it no longer has, and the new one never hearing about it. A favourite
/// cannot show this — its link to its song is its primary key, so moving one is
/// a delete and an insert — which is why there is a second child table here.
#[test]
fn a_child_edited_onto_another_parent_moves() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    store.put(&song(1, "Glue", 1));
    store.put(&song(2, "Opal", 2));
    store.put(&note(7, 1, "live version"));
    store.take_changes();

    let mut view = note_view();
    view.hydrate(&mut store);
    let counts: Vec<usize> = view
        .with::<Note>()
        .iter()
        .map(|r| r.related.len())
        .collect();
    assert_eq!(counts, vec![1, 0]);

    let mut moved = store.get::<Note>(&Note::key_of(&vec![7u8; 16])).unwrap();
    moved.song_id = vec![2u8; 16];
    store.put(&moved);
    settle(&mut view, &mut store);

    let rows = view.with::<Note>();
    let counts: Vec<usize> = rows.iter().map(|r| r.related.len()).collect();
    assert_eq!(counts, vec![0, 1], "it left Glue and joined Opal");
    assert_eq!(rows, notes(&mut store));
}

/// Hydrating a view a second time has to give the current answer.
///
/// It reads through the pipeline rather than from what the view accumulated, so
/// this is what catches an operator whose own copy of a node has gone stale —
/// the take holds the window, and a child that moved under a row in it must
/// reach that copy too.
#[test]
fn a_view_hydrated_again_is_still_right() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    for i in 1..=5 {
        store.put(&song(i, &format!("song {i}"), i as i64));
    }
    store.take_changes();

    let mut view = View::<Song>::related(
        Song::all()
            .order_by(Song::pos.asc())
            .order_by(Song::id.asc())
            .limit(3),
        Song::note,
        Note::all().order_by(Note::id.asc()),
    );
    view.hydrate(&mut store);

    store.put(&note(7, 2, "a note on song 2"));
    settle(&mut view, &mut store);
    let after_push = view.with::<Note>();
    assert_eq!(after_push[1].related.len(), 1);

    view.hydrate(&mut store);
    assert_eq!(
        view.with::<Note>(),
        after_push,
        "the second hydrate read a stale node out of the window"
    );
}
