//! Three levels, maintained.
//!
//! `Child` carries a relationship's name and another `Delta`, so a change two
//! levels down arrives as a `Child` of the song whose inner change is a `Child`
//! of the note. Depth is a property of the query, not of the type.
//!
//! Songs have notes, notes have authors. Every test here changes something at
//! the bottom and checks that it reached the top.

use diesel::connection::SimpleConnection;
use petros::backend::SqliteStore;
use petros_ivm::{Pipeline, View};
use petros_schema::{Rows, Store, Table};

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

fn note(id: u8, song: u8, text: &str) -> Note {
    Note {
        id: vec![id; 16],
        song_id: vec![song; 16],
        text: text.into(),
    }
}

fn author(id: u8, note: u8, name: &str) -> Author {
    Author {
        id: vec![id; 16],
        note_id: vec![note; 16],
        name: name.into(),
    }
}

/// song -> note -> author, all three maintained.
fn view() -> View<Song> {
    View::over(
        Pipeline::of(
            Song::all()
                .order_by(Song::pos.asc())
                .order_by(Song::id.asc()),
        )
        .related(
            Song::note,
            Pipeline::of(Note::all().order_by(Note::id.asc())).related(
                Note::author,
                Pipeline::of(Author::all().order_by(Author::id.asc())),
            ),
        ),
    )
}

fn settle(view: &mut View<Song>, store: &mut SqliteStore) -> usize {
    let changes = store.take_changes();
    view.apply(store, &changes)
}

/// What the tree holds, two levels down, as names.
fn authors(view: &View<Song>) -> Vec<Vec<Vec<String>>> {
    view.nodes()
        .iter()
        .map(|s| {
            s.children(Note::DEF.name)
                .iter()
                .map(|n| {
                    n.children(Author::DEF.name)
                        .iter()
                        .filter_map(|a| Author::from_row(&a.row).map(|a| a.name))
                        .collect()
                })
                .collect()
        })
        .collect()
}

/// A pull has to build the whole tree, or there is nothing to maintain.
#[test]
fn a_three_level_query_hydrates() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    store.put(&song(1, "Glue", 1)).unwrap();
    store.put(&song(2, "Opal", 2)).unwrap();
    store.put(&note(1, 1, "live")).unwrap();
    store.put(&author(1, 1, "alice")).unwrap();
    store.put(&author(2, 1, "bob")).unwrap();
    store.take_changes();

    let mut view = view();
    view.hydrate(&mut store);
    assert_eq!(
        authors(&view),
        vec![vec![vec!["alice".to_string(), "bob".to_string()]], vec![]]
    );
}

/// The claim. A row inserted two levels down is not a change to any song and
/// not a change to any note — it is a change to an *author* — and the song at
/// the top of the view still has to learn about it.
#[test]
fn a_change_two_levels_down_reaches_the_top() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    store.put(&song(1, "Glue", 1)).unwrap();
    store.put(&note(1, 1, "live")).unwrap();
    store.take_changes();

    let mut view = view();
    view.hydrate(&mut store);
    assert_eq!(authors(&view), vec![vec![Vec::<String>::new()]]);

    store.put(&author(1, 1, "alice")).unwrap();
    assert!(settle(&mut view, &mut store) > 0, "it reached the view");
    assert_eq!(authors(&view), vec![vec![vec!["alice".to_string()]]]);

    // And away again.
    store
        .delete::<Author>(&Author::key_of(&vec![1u8; 16]))
        .unwrap();
    settle(&mut view, &mut store);
    assert_eq!(authors(&view), vec![vec![Vec::<String>::new()]]);
}

/// A node joining the view arrives with its whole subtree already on it, not
/// blank until something else happens to touch it.
///
/// The song enters by being edited across the filter, because a foreign key
/// means the notes and authors under it must exist first. Writing them in one
/// at a time and checking afterwards would pass whether or not anything was
/// ever hydrated, which is what the first version of this test did.
#[test]
fn a_node_joining_the_view_arrives_with_its_subtree() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    let mut hidden = song(1, "Glue", 1);
    hidden.done = true;
    store.put(&hidden).unwrap();
    store.put(&note(1, 1, "live")).unwrap();
    store.put(&author(1, 1, "alice")).unwrap();
    store.put(&author(2, 1, "bob")).unwrap();
    store.take_changes();

    let mut view = View::over(
        Pipeline::of(
            Song::all()
                .filter(Song::done.eq(false))
                .order_by(Song::pos.asc())
                .order_by(Song::id.asc()),
        )
        .related(
            Song::note,
            Pipeline::of(Note::all().order_by(Note::id.asc())).related(
                Note::author,
                Pipeline::of(Author::all().order_by(Author::id.asc())),
            ),
        ),
    );
    view.hydrate(&mut store);
    assert!(view.is_empty());

    hidden.done = false;
    store.put(&hidden).unwrap();
    settle(&mut view, &mut store);

    assert_eq!(
        authors(&view),
        vec![vec![vec!["alice".to_string(), "bob".to_string()]]],
        "both levels arrived with the song, in one change"
    );
}

/// A note moved to another song takes its authors with it — the whole subtree
/// travels, because a `Child` carries a node and not a row.
#[test]
fn a_moved_middle_node_takes_its_children_along() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    store.put(&song(1, "Glue", 1)).unwrap();
    store.put(&song(2, "Opal", 2)).unwrap();
    store.put(&note(1, 1, "live")).unwrap();
    store.put(&author(1, 1, "alice")).unwrap();
    store.take_changes();

    let mut view = view();
    view.hydrate(&mut store);
    assert_eq!(
        authors(&view),
        vec![vec![vec!["alice".to_string()]], vec![]]
    );

    let mut moved = store.get::<Note>(&Note::key_of(&vec![1u8; 16])).unwrap();
    moved.song_id = vec![2u8; 16];
    store.put(&moved).unwrap();
    settle(&mut view, &mut store);

    assert_eq!(
        authors(&view),
        vec![vec![], vec![vec!["alice".to_string()]]],
        "the note left Glue for Opal, and alice went with it"
    );
}

/// A store that says how much was read through it.
///
/// The batching claim — that a page of parents costs *one* pull per level, not
/// one per parent — is about work rather than about the answer. Without a
/// counter the answer is identical either way, and a test that only checks the
/// rows passes against a pipeline that reads the whole table at every level.
struct Counting<'a> {
    inner: SqliteStore<'a>,
    fetches: usize,
    rows: usize,
}

impl petros_schema::Store for Counting<'_> {
    fn fetch(&mut self, plan: &petros_schema::Plan) -> Vec<Vec<petros_schema::Value>> {
        let rows = self.inner.fetch(plan);
        self.fetches += 1;
        self.rows += rows.len();
        rows
    }
    fn put_row(&mut self, table: &str, row: &[petros_schema::Value]) -> Result<(), String> {
        self.inner.put_row(table, row)
    }
    fn delete_row(&mut self, table: &str, key: &[petros_schema::Value]) -> Result<(), String> {
        self.inner.delete_row(table, key)
    }
    fn get_row(
        &mut self,
        table: &str,
        key: &[petros_schema::Value],
    ) -> Option<Vec<petros_schema::Value>> {
        self.inner.get_row(table, key)
    }
    fn take_changes(&mut self) -> Vec<petros_schema::Change> {
        self.inner.take_changes()
    }
}

/// Three levels cost three pulls, however many rows are at each level.
///
/// This is what the constraint in `Fetch` is for: the `IN` covering a whole
/// page of parents travels *through* the child pipeline. Take it away and the
/// answer is unchanged and the work is the whole table, once per level.
#[test]
fn a_page_costs_one_pull_per_level() {
    let mut conn = db();
    let mut store = SqliteStore::new(&mut conn);
    for s in 1..=6u8 {
        store.put(&song(s, &format!("song {s}"), s as i64)).unwrap();
        store.put(&note(s, s, "a note")).unwrap();
        store.put(&author(s, s, "someone")).unwrap();
    }
    store.take_changes();

    // Only the first two songs are wanted, so four songs, four notes and four
    // authors are rows a batched pull must not read.
    let mut view = View::over(
        Pipeline::of(
            Song::all()
                .order_by(Song::pos.asc())
                .order_by(Song::id.asc())
                .limit(2),
        )
        .related(
            Song::note,
            Pipeline::of(Note::all().order_by(Note::id.asc())).related(
                Note::author,
                Pipeline::of(Author::all().order_by(Author::id.asc())),
            ),
        ),
    );

    let mut counting = Counting {
        inner: SqliteStore::new(&mut conn),
        fetches: 0,
        rows: 0,
    };
    view.hydrate(&mut counting);

    assert_eq!(authors(&view).len(), 2);
    assert_eq!(
        counting.fetches, 3,
        "one pull per level, not one per parent"
    );
    assert!(
        counting.rows <= 10,
        "read {} rows for two songs and their subtrees",
        counting.rows
    );
}
