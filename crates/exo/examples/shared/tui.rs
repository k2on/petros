//! Terminal lifecycle and the shared to-do view.
//!
//! Both examples draw the same thing, so all the ratatui lives here and the
//! examples themselves stay about Exo.

use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use crossterm::ExecutableCommand;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph};
use ratatui::Terminal;

use todo::Item;

/// What the user is doing. Modal, so single letters can stay commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
}

/// The view state that is not Exo's business: which row is selected, what is
/// half-typed, and the last few things that happened.
pub struct Ui {
    pub mode: Mode,
    pub selected: usize,
    pub input: String,
    pub log: VecDeque<String>,
}

impl Ui {
    pub fn new() -> Self {
        Ui {
            mode: Mode::Normal,
            selected: 0,
            input: String::new(),
            log: VecDeque::new(),
        }
    }

    pub fn note(&mut self, msg: impl Into<String>) {
        self.log.push_back(msg.into());
        while self.log.len() > 4 {
            self.log.pop_front();
        }
    }

    pub fn move_by(&mut self, delta: isize, len: usize) {
        if len == 0 {
            self.selected = 0;
            return;
        }
        let next = self.selected as isize + delta;
        self.selected = next.clamp(0, len as isize - 1) as usize;
    }

    /// Keep the selection on a real row after the list changes under us — which
    /// it does, every time a rebase reorders things.
    pub fn clamp(&mut self, len: usize) {
        self.selected = self.selected.min(len.saturating_sub(1));
    }
}

/// Everything the status line reports.
pub struct Status {
    pub title: String,
    pub cursor: u64,
    pub pending: usize,
    /// `None` for the offline example, which has no server to be online with.
    pub online: Option<bool>,
}

/// The terminal, restored on drop however the program ends.
pub struct Tui {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Tui {
    pub fn start() -> io::Result<Self> {
        // A panic in raw mode leaves the terminal unusable, so put it back
        // before the message is printed.
        let hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let _ = restore();
            hook(info);
        }));
        enable_raw_mode()?;
        io::stdout().execute(EnterAlternateScreen)?;
        Ok(Tui {
            terminal: Terminal::new(CrosstermBackend::new(io::stdout()))?,
        })
    }

    /// Wait up to `timeout` for a keypress. Returning `None` is how the caller
    /// gets a turn to pump the network and redraw.
    pub fn key(timeout: Duration) -> io::Result<Option<KeyEvent>> {
        if !event::poll(timeout)? {
            return Ok(None);
        }
        match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => Ok(Some(k)),
            _ => Ok(None),
        }
    }

    pub fn draw(&mut self, items: &[Item], ui: &Ui, status: &Status) -> io::Result<()> {
        self.terminal.draw(|frame| {
            let [list, input, log, bar] = Layout::vertical([
                Constraint::Min(3),
                Constraint::Length(3),
                Constraint::Length(6),
                // Two lines: state on one, the keys that change it on the next,
                // so neither gets truncated on an 80-column terminal.
                Constraint::Length(2),
            ])
            .areas(frame.area());

            frame.render_stateful_widget(
                todo_list(items, &status.title),
                list,
                &mut ListState::default().with_selected(Some(ui.selected)),
            );
            frame.render_widget(input_box(ui), input);
            frame.render_widget(log_box(ui), log);
            frame.render_widget(status_bar(ui, status), bar);
        })?;
        Ok(())
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = restore();
    }
}

fn restore() -> io::Result<()> {
    disable_raw_mode()?;
    io::stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}

fn todo_list<'a>(items: &'a [Item], title: &'a str) -> List<'a> {
    let rows: Vec<ListItem> = items
        .iter()
        .map(|it| {
            let mark = if it.done { "[x]" } else { "[ ]" };
            let text = if it.done {
                Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::CROSSED_OUT)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {mark} "), Style::default().fg(Color::Cyan)),
                Span::styled(it.text.clone(), text),
                Span::styled(
                    format!("  ({})", it.actor),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();
    List::new(rows)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED))
}

fn input_box(ui: &Ui) -> Paragraph<'_> {
    let (title, body) = match ui.mode {
        Mode::Insert => (" new to-do ", format!("{}█", ui.input)),
        Mode::Normal => (" new to-do ", "press a to add".to_string()),
    };
    let style = match ui.mode {
        Mode::Insert => Style::default(),
        Mode::Normal => Style::default().fg(Color::DarkGray),
    };
    Paragraph::new(Span::styled(body, style))
        .block(Block::default().borders(Borders::ALL).title(title))
}

fn log_box(ui: &Ui) -> Paragraph<'_> {
    let lines: Vec<Line> = ui
        .log
        .iter()
        .map(|l| Line::from(Span::styled(l.clone(), Style::default().fg(Color::Yellow))))
        .collect();
    Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" what happened "),
    )
}

fn status_bar<'a>(ui: &Ui, status: &Status) -> Paragraph<'a> {
    let link = match status.online {
        None => Span::styled(" no server ", Style::default().fg(Color::DarkGray)),
        Some(true) => Span::styled(" online ", Style::default().fg(Color::Green)),
        Some(false) => Span::styled(" offline ", Style::default().fg(Color::Red)),
    };
    let keys = match (ui.mode, status.online.is_some()) {
        (Mode::Insert, _) => " enter add · esc cancel",
        (Mode::Normal, true) => {
            " ↑↓ move · space done · d delete · a add · o offline/online · q quit"
        }
        (Mode::Normal, false) => " ↑↓ move · space done · d delete · a add · q quit",
    };
    Paragraph::new(vec![
        Line::from(vec![
            link,
            Span::styled(
                format!("· cursor {} · {} pending", status.cursor, status.pending),
                Style::default().fg(Color::DarkGray),
            ),
        ]),
        Line::from(Span::styled(keys, Style::default().fg(Color::Blue))),
    ])
}

/// The keys both examples share. Returns false when the user wants out.
pub enum Action {
    Quit,
    Add(String),
    ToggleDone,
    Delete,
    ToggleLink,
    Nothing,
}

pub fn action_for(key: KeyEvent, ui: &mut Ui, len: usize) -> Action {
    match ui.mode {
        Mode::Insert => match key.code {
            KeyCode::Enter => {
                ui.mode = Mode::Normal;
                Action::Add(std::mem::take(&mut ui.input))
            }
            KeyCode::Esc => {
                ui.mode = Mode::Normal;
                ui.input.clear();
                Action::Nothing
            }
            KeyCode::Backspace => {
                ui.input.pop();
                Action::Nothing
            }
            KeyCode::Char(c) => {
                ui.input.push(c);
                Action::Nothing
            }
            _ => Action::Nothing,
        },
        Mode::Normal => match key.code {
            KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
            KeyCode::Char('a') => {
                ui.mode = Mode::Insert;
                Action::Nothing
            }
            KeyCode::Char(' ') => Action::ToggleDone,
            KeyCode::Char('d') => Action::Delete,
            KeyCode::Char('o') => Action::ToggleLink,
            KeyCode::Up | KeyCode::Char('k') => {
                ui.move_by(-1, len);
                Action::Nothing
            }
            KeyCode::Down | KeyCode::Char('j') => {
                ui.move_by(1, len);
                Action::Nothing
            }
            _ => Action::Nothing,
        },
    }
}
