//! Two or more clients, one server, live sync over a WebSocket.
//!
//!   terminal 1:  just serve
//!   terminal 2:  just peer alice
//!   terminal 3:  just peer bob
//!
//! Commands: `add <text>`, `done <n>`, `rm <n>`, `list`, `offline`, `online`,
//! `quit`. Type `offline` in both peers, mutate on each, then `online`, and
//! watch the rebase reorder the optimistic entries behind the confirmed ones.
//!
//! Each peer keeps its own database file in the temp directory, so state
//! survives quitting and restarting.

#[path = "shared/todo.rs"]
mod todo;

use std::net::TcpListener;
use std::sync::mpsc::{channel, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use exo::transport::ws::{serve, Link};
use exo::{AutoCtx, Client, Server};
use todo::{list, render, Todo, TodoApp};

fn main() -> exo::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let addr = flag(&args, "--server").unwrap_or_else(|| "127.0.0.1:8787".into());
    if args.iter().any(|a| a == "--serve") {
        return run_server(&addr);
    }
    let user = flag(&args, "--user").unwrap_or_else(|| "alice".into());
    run_peer(&user, &addr)
}

fn flag(args: &[String], name: &str) -> Option<String> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).cloned()
}

fn db(name: &str) -> exo::Result<exo::Connection> {
    exo::open_path(std::env::temp_dir().join(format!("exo-demo-{name}.db")))
}

fn run_server(addr: &str) -> exo::Result<()> {
    let server = Server::<TodoApp>::open(db("server")?)?;
    let listener = TcpListener::bind(addr)?;
    println!("exo server on ws://{addr} (head {})", server.head());
    serve(listener, Arc::new(Mutex::new(server)))
}

fn run_peer(user: &str, addr: &str) -> exo::Result<()> {
    let mut client = Client::<TodoApp>::open(db(user)?, user, AutoCtx::system())?;
    let mut link = connect(&mut client, addr);
    let input = stdin_lines();
    let mut shown = String::new();

    println!("{user}: type `help` for commands");
    loop {
        // Drain the client's outbox onto the wire. While offline we drop it on
        // the floor; reconnecting re-offers everything still pending, and the
        // server dedupes whatever it has already seen.
        for msg in client.take_outgoing() {
            if let Some(l) = &link {
                l.send(msg);
            }
        }
        if let Some(l) = &link {
            while let Some(msg) = l.try_recv() {
                client.recv(msg)?;
            }
            if !l.is_alive() {
                println!("  ! the link dropped; type `online` to reconnect");
                link = None;
            }
        }
        for r in client.take_rejections() {
            println!("  ! the server refused one of your changes: {}", r.reason);
        }

        // Redraw whenever anything observable moved — including the cursor
        // and the pending count, which is how an ack makes itself visible.
        let state = if link.is_some() { "online" } else { "offline" };
        let now = format!(
            "[{state}, cursor {}, {} pending]\n{}",
            client.cursor(),
            client.pending_len(),
            render(&list(client.conn())?)
        );
        if now != shown {
            println!("\n{now}\n");
            shown = now;
        }

        match input.try_recv() {
            Ok(line) => {
                if !command(&mut client, &mut link, addr, line.trim())? {
                    return Ok(());
                }
                shown.clear(); // force a redraw after anything the user did
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return Ok(()),
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                std::thread::sleep(Duration::from_millis(20))
            }
        }
    }
}

/// Returns false when the user wants to quit.
fn command(
    client: &mut Client<TodoApp>,
    link: &mut Option<Link<Todo>>,
    addr: &str,
    line: &str,
) -> exo::Result<bool> {
    let (cmd, rest) = line.split_once(' ').unwrap_or((line, ""));
    match cmd {
        "" | "list" => {}
        "add" => {
            if let Err(e) = client.mutate(Todo::add(rest)) {
                println!("  ! {e}");
            }
        }
        "done" | "rm" => match nth(client, rest)? {
            Some(id) if cmd == "rm" => {
                client.mutate(Todo::Remove { id })?;
            }
            Some(id) => {
                client.mutate(Todo::SetDone { id, done: true })?;
            }
            None => println!("  ! no such item"),
        },
        "offline" => *link = None,
        "online" => *link = connect(client, addr),
        "quit" | "exit" => return Ok(false),
        _ => {
            println!("  commands: add <text> | done <n> | rm <n> | list | offline | online | quit")
        }
    }
    Ok(true)
}

fn nth(client: &mut Client<TodoApp>, arg: &str) -> exo::Result<Option<exo::Id>> {
    let n: usize = arg.trim().parse().unwrap_or(0);
    Ok(list(client.conn())?.get(n.wrapping_sub(1)).map(|i| i.id))
}

/// Connect and say hello. A failure here is not fatal: the peer keeps working
/// offline, which is rather the point.
fn connect(client: &mut Client<TodoApp>, addr: &str) -> Option<Link<Todo>> {
    match Link::connect(&format!("ws://{addr}")) {
        Ok(link) => {
            let _ = client.connected();
            Some(link)
        }
        Err(e) => {
            println!("  ! cannot reach ws://{addr} ({e}); staying offline");
            None
        }
    }
}

fn stdin_lines() -> Receiver<String> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            let Ok(line) = line else { return };
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    rx
}
