//! A client with no server in existence.
//!
//! There is no network here and no server process to start. Mutations apply
//! immediately, the list is usable straight away, and everything survives
//! quitting and starting again — which is the whole claim behind
//! "offline-first". Quit with `q` and run it a second time to see that.
//!
//! Run with: `just offline`

#[path = "shared/tui.rs"]
mod tui;

use std::time::Duration;

use petros::{AutoCtx, Client};
use todo::{self as mutators, list, TodoApp};
use tui::{action_for, Action, Status, Tui, Ui};

fn main() -> petros::Result<()> {
    let path = std::env::temp_dir().join("petros-offline-demo.db");
    let mut client =
        Client::<TodoApp>::open(petros::open_path(&path)?, "alice", AutoCtx::system())?;

    let mut ui = Ui::new();
    let restored = list(&mut client.store())?.len();
    ui.note(format!("opened {}", path.display()));
    ui.note(match restored {
        0 => "a fresh database — nothing to restore".to_string(),
        n => format!("{n} item(s) restored from the last run"),
    });

    let mut term = Tui::start()?;
    loop {
        let items = list(&mut client.store())?;
        ui.clamp(items.len());
        let status = Status {
            title: " petros · offline ".into(),
            cursor: client.cursor(),
            pending: client.pending_len(),
            // There is no server, so there is nothing to be online with. Every
            // mutation stays pending forever, and the list works anyway.
            online: None,
        };
        term.draw(&items, &ui, &status)?;

        let Some(key) = Tui::key(Duration::from_millis(100))? else {
            continue;
        };
        match action_for(key, &mut ui, items.len()) {
            Action::Quit => return Ok(()),
            Action::Add(text) => {
                // A mutation the app itself refuses never reaches the pending
                // queue, and would never have reached a log either.
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
            Action::ToggleLink | Action::Nothing => {}
        }
    }
}
