//! A client with no server in existence.
//!
//! There is no network here and no server process to start. Mutations apply
//! immediately, the state is queryable straight away, and everything survives
//! closing the database and opening it again — which is the whole claim behind
//! "offline-first".
//!
//! Run with: `just offline`

#[path = "shared/todo.rs"]
mod todo;

use exo::{AutoCtx, Client};
use todo::{list, render, Todo, TodoApp};

fn main() -> exo::Result<()> {
    let path = std::env::temp_dir().join("exo-offline-demo.db");
    let _ = std::fs::remove_file(&path);
    println!("database: {}\n", path.display());

    {
        let mut client =
            Client::<TodoApp>::open(exo::open_path(&path)?, "alice", AutoCtx::system())?;

        for text in ["buy oat milk", "book the ferry", "return the drill"] {
            client.mutate(Todo::add(text))?;
        }
        let items = list(client.conn())?;
        client.mutate(Todo::SetDone {
            id: items[1].id,
            done: true,
        })?;

        println!("after four mutations, with nothing to sync to:");
        println!("{}", render(&list(client.conn())?));
        println!(
            "\n  {} mutations are pending — no server has ever seen them,\n  \
             and the list is fully usable anyway.",
            client.pending_len()
        );

        // A mutation the app itself refuses. It never reaches the pending queue
        // and would never have reached the log.
        match client.mutate(Todo::add("   ")) {
            Err(e) => println!("\n  rejected locally, as it would be by the server: {e}"),
            Ok(_) => println!("\n  unexpectedly accepted an empty to-do"),
        }
        println!("  still {} pending\n", client.pending_len());
    } // the client is dropped here: process gone, connection closed

    println!("...reopening the database cold...\n");
    let client = Client::<TodoApp>::open(exo::open_path(&path)?, "alice", AutoCtx::system())?;
    println!("{}", render(&list(client.conn())?));
    println!(
        "\n  cursor {}, {} still pending. Nothing was lost.",
        client.cursor(),
        client.pending_len()
    );
    Ok(())
}
