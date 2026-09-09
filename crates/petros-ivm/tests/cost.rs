//! Is maintaining a view actually cheaper than running the query again?
//!
//! `docs/ivm.md` says this is the measurement that decides whether any of it is
//! worth having, so here it is rather than an argument. A measurement, not an
//! assertion:
//!
//!     cargo test -p petros-ivm --release --test cost -- --ignored --nocapture

use diesel::connection::SimpleConnection;
use petros::backend::SqliteStore;
use petros_ivm::View;
use petros_schema::{Rows, Store};
use std::time::Instant;

const SCHEMA: &str = include_str!("../schema.sql");
petros_sql::tables!();

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn id(n: usize) -> Vec<u8> {
    let mut out = vec![0u8; 16];
    out[..8].copy_from_slice(&(n as u64).to_be_bytes());
    out
}

/// One write, then the answer — the loop a client is actually in.
///
/// The comparison is deliberately generous to the re-run: it is one indexed
/// statement over a table SQLite has in memory, which is the best case for it.
#[test]
#[ignore = "a measurement, not an assertion"]
fn maintained_against_a_re_run() {
    println!("\n  one write, then the top 20 of N rows:");
    for indexed in [true, false] {
        println!(
            "\n    ordering column {}:",
            if indexed { "indexed" } else { "not indexed" }
        );
        println!(
            "    {:>8}  {:>12}  {:>12}  {:>7}",
            "rows", "re-run", "maintained", "ratio"
        );
        sweep(indexed);
    }
}

fn sweep(indexed: bool) {
    for n in [100usize, 1_000, 10_000, 100_000] {
        let mut conn = petros::open_memory().unwrap();
        conn.batch_execute(SCHEMA).unwrap();
        if indexed {
            conn.batch_execute("CREATE INDEX song_pos ON song(pos, id)")
                .unwrap();
        }
        let mut store = SqliteStore::new(&mut conn);
        for i in 0..n {
            store
                .put(&Song {
                    id: id(i),
                    title: format!("song {i}"),
                    done: false,
                    pos: i as i64,
                })
                .unwrap();
        }
        store.take_changes();

        let query = || {
            Song::all()
                .order_by(Song::pos.asc())
                .order_by(Song::id.asc())
                .limit(20)
        };
        let mut view = View::<Song>::new(query());
        view.hydrate(&mut store);

        let (mut fresh, mut kept) = (Vec::new(), Vec::new());
        for i in 0..50 {
            // Append past the window: the case a growing list is made of.
            let row = Song {
                id: id(n + i),
                title: "new".into(),
                done: false,
                pos: (n + i) as i64,
            };

            store.put(&row).unwrap();
            let changes = store.take_changes();
            let t = Instant::now();
            view.apply(&mut store, &changes);
            let _ = view.rows();
            kept.push(t.elapsed().as_secs_f64() * 1000.0);

            let t = Instant::now();
            let _ = store.select(query());
            fresh.push(t.elapsed().as_secs_f64() * 1000.0);
            store.take_changes();
        }

        let (f, k) = (median(fresh), median(kept));
        println!(
            "    {:>8}  {:>9.4} ms  {:>9.4} ms  {:>6.1}x",
            n,
            f,
            k,
            f / k
        );
    }
}

/// What a write costs now that it reports what it changed.
///
/// `put_row` reads the old row before writing so it can report an edit with
/// both versions. `docs/ivm.md` asks whether that extra point lookup is
/// material; this is the answer.
#[test]
#[ignore = "a measurement, not an assertion"]
fn the_extra_read_on_a_write() {
    println!("\n  one write into a table of N rows:");
    println!("    {:>8}  {:>12}  {:>12}", "rows", "insert", "update");

    for n in [100usize, 10_000, 100_000] {
        let mut conn = petros::open_memory().unwrap();
        conn.batch_execute(SCHEMA).unwrap();
        let mut store = SqliteStore::new(&mut conn);
        for i in 0..n {
            store
                .put(&Song {
                    id: id(i),
                    title: format!("song {i}"),
                    done: false,
                    pos: i as i64,
                })
                .unwrap();
        }
        store.take_changes();

        let mut inserts = Vec::new();
        let mut updates = Vec::new();
        for i in 0..200 {
            let fresh = Song {
                id: id(n + i),
                title: "new".into(),
                done: false,
                pos: (n + i) as i64,
            };
            let t = Instant::now();
            store.put(&fresh).unwrap();
            inserts.push(t.elapsed().as_secs_f64() * 1000.0);

            // An update, which is the case that pays for the lookup: it has an
            // old row to find and report.
            let mut existing = store.get::<Song>(&Song::key_of(&id(i))).unwrap();
            existing.done = !existing.done;
            let t = Instant::now();
            store.put(&existing).unwrap();
            updates.push(t.elapsed().as_secs_f64() * 1000.0);
            store.take_changes();
        }
        println!(
            "    {:>8}  {:>9.4} ms  {:>9.4} ms",
            n,
            median(inserts),
            median(updates)
        );
    }
}

/// The join: hearting one song, against re-running the tree query.
///
/// This is the case a flat view cannot express at all, and the one a library
/// screen is actually made of.
#[test]
#[ignore = "a measurement, not an assertion"]
fn a_maintained_tree_against_a_re_run() {
    println!("\n  one heart, then the library of N songs with their favourites:");
    println!(
        "    {:>8}  {:>12}  {:>12}  {:>7}",
        "songs", "re-run", "maintained", "ratio"
    );

    for n in [100usize, 1_000, 10_000] {
        let mut conn = petros::open_memory().unwrap();
        conn.batch_execute(SCHEMA).unwrap();
        let mut store = SqliteStore::new(&mut conn);
        for i in 0..n {
            store
                .put(&Song {
                    id: id(i),
                    title: format!("song {i}"),
                    done: false,
                    pos: i as i64,
                })
                .unwrap();
        }
        store.take_changes();

        let query = || {
            Song::all()
                .order_by(Song::pos.asc())
                .order_by(Song::id.asc())
                .limit(20)
        };
        let children = || Favorite::all().order_by(Favorite::pos.asc());
        let mut view = View::<Song>::related(query(), Song::favorite, children());
        view.hydrate(&mut store);

        let (mut fresh, mut kept) = (Vec::new(), Vec::new());
        for i in 0..50 {
            // Heart a song outside the window: the change the view must judge
            // and then decline, which is the common case on a long list.
            store
                .put(&Favorite {
                    song_id: id(n - 1 - i),
                    pos: i as i64,
                })
                .unwrap();
            let changes = store.take_changes();
            let t = Instant::now();
            view.apply(&mut store, &changes);
            let _ = view.with::<Favorite>();
            kept.push(t.elapsed().as_secs_f64() * 1000.0);

            let t = Instant::now();
            let _ = store.select_with(query(), Song::favorite, children());
            fresh.push(t.elapsed().as_secs_f64() * 1000.0);
            store.take_changes();
        }
        let (f, k) = (median(fresh), median(kept));
        println!(
            "    {:>8}  {:>9.4} ms  {:>9.4} ms  {:>6.1}x",
            n,
            f,
            k,
            f / k
        );
    }
}
