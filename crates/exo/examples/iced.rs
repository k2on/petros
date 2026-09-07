//! The same to-do list as the terminal examples, in iced, talking to the same
//! server — on the desktop and in a browser.
//!
//!   terminal 1:  just serve
//!   terminal 2:  just peer alice     # the TUI
//!   terminal 3:  just iced bob       # this, on the desktop
//!   browser:     just web            # this, at http://localhost:8080
//!
//! All of them are peers of one server, so an item added in the browser appears
//! in the TUI and the other way round. Press the offline button in any of them,
//! mutate on both sides, come back online, and watch the rebase.
//!
//! The engine does not know which of the three it is running in. What differs
//! is two lines: where the database lives, and which transport carries the
//! bytes.

#[path = "shared/todo.rs"]
mod todo;

use std::time::Duration;

use exo::{AutoCtx, Client, Id};
use iced::widget::{button, checkbox, column, container, row, scrollable, text, text_input};
use iced::{Element, Length, Subscription, Task};
use todo::{list, Item, Todo, TodoApp};

#[cfg(target_arch = "wasm32")]
use exo::transport::web::Link;
#[cfg(not(target_arch = "wasm32"))]
use exo::transport::ws::Link;

const DEFAULT_SERVER: &str = "127.0.0.1:8787";

/// Who we are and where the server is. On the desktop, flags; in a browser, the
/// query string, so two tabs can be two peers.
fn config() -> (String, String) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let args: Vec<String> = std::env::args().collect();
        let flag = |name: &str| {
            args.iter()
                .position(|a| a == name)
                .and_then(|i| args.get(i + 1))
                .cloned()
        };
        (
            flag("--user").unwrap_or_else(|| "iced".into()),
            flag("--server").unwrap_or_else(|| DEFAULT_SERVER.into()),
        )
    }
    #[cfg(target_arch = "wasm32")]
    {
        let query = web_sys::window()
            .and_then(|w| w.location().search().ok())
            .unwrap_or_default();
        let param = |name: &str| {
            query
                .trim_start_matches('?')
                .split('&')
                .find_map(|kv| kv.strip_prefix(&format!("{name}="))?.into())
                .map(|v: &str| v.to_string())
        };
        (
            param("user").unwrap_or_else(|| "browser".into()),
            param("server").unwrap_or_else(|| DEFAULT_SERVER.into()),
        )
    }
}

/// Where the database lives is the only storage difference between the targets.
#[cfg(not(target_arch = "wasm32"))]
fn open(user: &str) -> exo::Result<exo::Connection> {
    exo::open_path(std::env::temp_dir().join(format!("exo-demo-{user}.db")))
}

/// In the browser, in memory: `sqlite-wasm-rs` registers a memory VFS by
/// default. It can also persist to OPFS, which needs an async handshake before
/// the first query — worth doing, and not what this example is for.
#[cfg(target_arch = "wasm32")]
fn open(_user: &str) -> exo::Result<exo::Connection> {
    exo::open_memory()
}

#[derive(Debug, Clone)]
enum Message {
    Typed(String),
    Add,
    Toggle(Id, bool),
    Remove(Id),
    ToggleLink,
    /// Pump the transport. Nothing else drives a sans-io client.
    Tick,
}

struct App {
    client: Client<TodoApp>,
    link: Option<Link<Todo>>,
    server: String,
    user: String,
    /// The materialised view and the pending count, refreshed after anything
    /// that could change them.
    ///
    /// iced's `view` takes `&self` and Diesel needs `&mut` even to read, so the
    /// query cannot happen during rendering. Keeping them here is the right
    /// shape for iced anyway — and it is the seam reactive queries would fill.
    items: Vec<Item>,
    pending: usize,
    input: String,
    note: String,
}

impl App {
    fn boot() -> Self {
        let (user, server) = config();
        let client = Client::<TodoApp>::open(
            open(&user).expect("open the database"),
            user.clone(),
            AutoCtx::system(),
        )
        .expect("open the exo client");
        let mut app = App {
            client,
            link: None,
            server,
            user,
            items: Vec::new(),
            pending: 0,
            input: String::new(),
            note: String::new(),
        };
        app.connect();
        app.refresh();
        app
    }

    fn connect(&mut self) {
        match Link::connect(&format!("ws://{}", self.server)) {
            Ok(link) => {
                let _ = self.client.connected();
                self.link = Some(link);
                self.note = format!("connected to {}", self.server);
            }
            Err(e) => {
                self.link = None;
                self.note = format!("cannot reach {} ({e}) — working offline", self.server);
            }
        }
    }

    fn refresh(&mut self) {
        self.items = list(self.client.conn()).unwrap_or_default();
        self.pending = self.client.pending_len();
    }

    /// Move messages between the client and the wire. While offline the outbox
    /// is drained and dropped: reconnecting re-offers everything still pending,
    /// and the server dedupes what it has already seen.
    ///
    /// Reports whether anything arrived, so that the twenty ticks a second that
    /// find an empty socket cost a `try_recv` rather than a re-query of the
    /// whole list.
    fn pump(&mut self) -> bool {
        let mut moved = false;
        for msg in self.client.take_outgoing() {
            if let Some(link) = &self.link {
                link.send(msg);
            }
        }
        if let Some(link) = &self.link {
            while let Some(msg) = link.try_recv() {
                moved = true;
                if let Err(e) = self.client.recv(msg) {
                    self.note = e.to_string();
                }
            }
            if !link.is_alive() {
                self.note = "the link dropped".into();
                self.link = None;
            }
        }
        for r in self.client.take_rejections() {
            self.note = format!("the server refused a change: {}", r.reason);
            moved = true;
        }
        moved
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        // Typing, ticking and pulling the plug all leave the list alone.
        let edited = matches!(
            message,
            Message::Add | Message::Toggle(..) | Message::Remove(_)
        );
        let outcome = match message {
            Message::Typed(text) => {
                self.input = text;
                Ok(())
            }
            Message::Add => {
                let text = std::mem::take(&mut self.input);
                self.client.mutate(Todo::add(&text)).map(|_| ())
            }
            Message::Toggle(id, done) => self.client.mutate(Todo::SetDone { id, done }).map(|_| ()),
            Message::Remove(id) => self.client.mutate(Todo::Remove { id }).map(|_| ()),
            Message::ToggleLink => {
                match self.link {
                    Some(_) => {
                        self.link = None;
                        self.note = "gone offline — edits pile up locally".into();
                    }
                    None => self.connect(),
                }
                Ok(())
            }
            Message::Tick => Ok(()),
        };
        // A mutation the app itself refuses never reaches the pending queue.
        if let Err(e) = outcome {
            self.note = e.to_string();
        }
        // `refresh` reads the whole list back out of SQLite, so it waits for a
        // reason: either this message was an edit, or the wire brought one.
        let arrived = self.pump();
        if edited || arrived {
            self.refresh();
        }
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let rows = self.items.iter().fold(column![].spacing(6), |col, item| {
            col.push(
                row![
                    checkbox(item.done).on_toggle(move |done| Message::Toggle(item.id, done)),
                    text(item.text.clone()).width(Length::Fill),
                    text(item.actor.clone()).size(12),
                    button("remove").on_press(Message::Remove(item.id)),
                ]
                .spacing(12)
                .align_y(iced::Alignment::Center),
            )
        });

        let entry = row![
            text_input("a new to-do…", &self.input)
                .on_input(Message::Typed)
                .on_submit(Message::Add)
                .width(Length::Fill),
            button("add").on_press(Message::Add),
            button(if self.link.is_some() {
                "go offline"
            } else {
                "go online"
            })
            .on_press(Message::ToggleLink),
        ]
        .spacing(12);

        // The engine showing through: `cursor` is how much of the server's log
        // has been applied, `pending` is what this peer has done that no server
        // has confirmed yet.
        let status = text(format!(
            "{} · {} · cursor {} · {} pending{}",
            self.user,
            if self.link.is_some() {
                "online"
            } else {
                "offline"
            },
            self.client.cursor(),
            self.pending,
            if self.note.is_empty() {
                String::new()
            } else {
                format!("  ·  {}", self.note)
            }
        ))
        .size(13);

        container(
            column![
                text("exo · to-do").size(26),
                entry,
                scrollable(rows).height(Length::Fill),
                status,
            ]
            .spacing(16),
        )
        .padding(24)
        .into()
    }

    /// A sans-io client has to be pumped by someone. This is that someone.
    fn subscription(&self) -> Subscription<Message> {
        iced::time::every(Duration::from_millis(50)).map(|_| Message::Tick)
    }
}

pub fn main() -> iced::Result {
    #[cfg(target_arch = "wasm32")]
    console_error_panic_hook::set_once();

    iced::application(App::boot, App::update, App::view)
        .subscription(App::subscription)
        .title("exo · to-do")
        .window_size((860.0, 600.0))
        .run()
}
