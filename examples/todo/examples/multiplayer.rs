//! Two or more clients, one server, live sync over a WebSocket.
//!
//!   terminal 1:  just serve
//!   terminal 2:  just peer alice
//!   terminal 3:  just peer bob
//!
//! Press `o` in both peers to go offline, add something in each, then `o`
//! again. Watch the rebase: your optimistic entries roll back, the confirmed
//! ones land underneath them, and yours replay on top — so an item you added
//! while alone moves down the list as the other peer's entries arrive.
//!
//! Each peer keeps its own database in the temp directory, so state survives
//! quitting and starting again.

#[path = "shared/tui.rs"]
mod tui;

use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use petros::transport::ws::{serve, Link};
use petros::{AutoCtx, Client, Server};
use todo::{self as mutators, list, TodoApp};
use tui::{action_for, Action, Status, Tui, Ui};

fn main() -> petros::Result<()> {
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

fn db(name: &str) -> petros::Result<petros::Connection> {
    petros::open_path(std::env::temp_dir().join(format!("petros-demo-{name}.db")))
}

/// The server has no UI: it is a log and a socket.
fn run_server(addr: &str) -> petros::Result<()> {
    // The server applies every mutation before it appends it, with the same
    // `apply` the terminal peers link and the phone interprets. That is what
    // makes a rejection a verdict rather than one machine's opinion.
    let server = Server::<TodoApp>::open(db("server")?)?;
    let listener = TcpListener::bind(addr)?;
    println!("petros server on ws://{addr} (head {})", server.head());
    serve(listener, Arc::new(Mutex::new(server)))
}

fn run_peer(user: &str, addr: &str) -> petros::Result<()> {
    let mut client = Client::<TodoApp>::open(db(user)?, user, AutoCtx::system())?;
    let mut ui = Ui::new();
    let mut link = connect(&mut client, addr, &mut ui);

    let mut term = Tui::start()?;
    loop {
        // Drain the outbox onto the wire. While offline we drop it on the
        // floor; reconnecting re-offers everything still pending, and the
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
                ui.note("the link dropped — press o to reconnect");
                link = None;
            }
        }
        for r in client.take_rejections() {
            ui.note(format!("the server refused a change: {}", r.reason));
        }

        let items = list(&mut client.store())?;
        ui.clamp(items.len());
        let status = Status {
            title: format!(" petros · {user} "),
            cursor: client.cursor(),
            pending: client.pending_len(),
            online: Some(link.is_some()),
        };
        term.draw(&items, &ui, &status)?;

        let Some(key) = Tui::key(Duration::from_millis(50))? else {
            continue;
        };
        match action_for(key, &mut ui, items.len()) {
            Action::Quit => return Ok(()),
            Action::Add(text) => {
                if let Err(e) = client.mutate(mutators::add(text)) {
                    ui.note(format!("refused: {e}"));
                }
            }
            Action::ToggleDone => {
                if let Some(item) = items.get(ui.selected) {
                    client.mutate(mutators::set_done(
                        item.id.as_uuid().as_bytes().to_vec(),
                        !item.done,
                    ))?;
                }
            }
            Action::Delete => {
                if let Some(item) = items.get(ui.selected) {
                    client.mutate(mutators::remove(item.id.as_uuid().as_bytes().to_vec()))?;
                }
            }
            Action::ToggleLink => {
                link = match link {
                    Some(_) => {
                        ui.note("gone offline — edits pile up locally");
                        None
                    }
                    None => connect(&mut client, addr, &mut ui),
                };
            }
            Action::Nothing => {}
        }
    }
}

/// Connect and say hello. A failure is not fatal: the peer keeps working
/// offline, which is rather the point.
fn connect(client: &mut Client<TodoApp>, addr: &str, ui: &mut Ui) -> Option<Link<todo::Payload>> {
    match Link::connect(&format!("ws://{addr}")) {
        Ok(link) => {
            let _ = client.connected();
            ui.note(format!("connected to ws://{addr}"));
            Some(link)
        }
        Err(e) => {
            ui.note(format!("cannot reach ws://{addr} ({e}) — staying offline"));
            None
        }
    }
}
