//! The realtime channel: rooms, who is in one, and what survives a restart.
//!
//! Everything here is against the sans-io server — values in, values out, no
//! socket — which is what makes "the peer went away and came back" a two-line
//! test rather than a network to simulate.

mod common;

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use common::todo::{Todo, TodoMutation};
use petros::live::{Live, Peer, Post, Room};
use petros::{ActorId, Authenticate, ClientMsg, Connection, Identity, Server, ServerMsg};

/// A token per user; the session is the device.
struct Table;

impl Authenticate for Table {
    fn authenticate(&mut self, token: Option<&str>) -> Option<Identity> {
        // "alice/phone" is alice, on the device called phone.
        let (user, device) = token?.split_once('/')?;
        Some(Identity {
            user: ActorId::from(user),
            session: device.to_string(),
        })
    }
}

/// A room of one word, said by whoever last said it, with the *last device to
/// say something* remembered — which is the shape harken's listening session
/// has, small enough to assert on.
#[derive(Default)]
struct Chalkboard {
    boards: BTreeMap<Room, Board>,
    /// Every call, in order. Shared with the test rather than kept here,
    /// because `with_live` takes the machine and the server does not hand it
    /// back — which is right, and means anything an app wants to watch it do
    /// it watches from outside, exactly as harken's bridge does.
    seen: Log,
}

type Log = Arc<Mutex<Vec<String>>>;

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
struct Board {
    word: String,
    /// The device that wrote it, which is remembered across that device
    /// leaving — the whole point of the thing.
    by: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Write(String);

#[derive(Clone, PartialEq, Eq, Debug, serde::Serialize, serde::Deserialize)]
struct Shown {
    word: String,
    by: String,
    here: Vec<String>,
}

impl Live for Chalkboard {
    type Say = Write;
    type Hear = Shown;

    fn join(&mut self, peer: &Peer, post: &mut Post<'_, Shown>) {
        self.note(format!("join {}", peer.who));
        let shown = self.shown(&peer.room, post);
        post.tell(&peer.who, shown);
    }

    fn say(&mut self, peer: &Peer, say: Write, post: &mut Post<'_, Shown>) {
        self.note(format!("say {}", peer.who));
        let board = self.boards.entry(peer.room.clone()).or_default();
        board.word = say.0;
        board.by = peer.who.clone();
        let shown = self.shown(&peer.room, post);
        post.tell_room(shown);
        post.keep();
    }

    fn part(&mut self, peer: &Peer, post: &mut Post<'_, Shown>) {
        self.note(format!("part {}", peer.who));
        let shown = self.shown(&peer.room, post);
        post.tell_room(shown);
    }

    fn snapshot(&mut self, room: &Room) -> Option<Vec<u8>> {
        let board = self.boards.get(room)?;
        (!board.word.is_empty()).then(|| petros::encode(board).unwrap())
    }

    fn wake(&mut self, room: &Room, snapshot: &[u8]) {
        self.note(format!("wake {room}"));
        if let Ok(board) = petros::decode::<Board>(snapshot) {
            self.boards.insert(room.clone(), board);
        }
    }

    fn close(&mut self, room: &Room) {
        self.note(format!("close {room}"));
        self.boards.remove(room);
    }
}

impl Chalkboard {
    fn note(&self, what: String) {
        if let Ok(mut seen) = self.seen.lock() {
            seen.push(what);
        }
    }

    fn shown(&self, room: &Room, post: &Post<'_, Shown>) -> Shown {
        let board = self.boards.get(room).cloned().unwrap_or_default();
        Shown {
            word: board.word,
            by: board.by,
            here: post.peers().iter().map(|p| p.who.clone()).collect(),
        }
    }
}

fn hello(token: &str) -> ClientMsg<TodoMutation> {
    ClientMsg::Hello {
        since: 0,
        token: Some(token.to_string()),
    }
}

fn say(word: &str) -> ClientMsg<TodoMutation> {
    ClientMsg::Say {
        say: petros::encode(&Write(word.to_string())).unwrap(),
    }
}

/// What each connection was told on the realtime channel, decoded.
fn heard(server: &mut Server<Todo>) -> Vec<(u64, Shown)> {
    server
        .take_outgoing()
        .into_iter()
        .filter_map(|(conn, msg)| match msg {
            ServerMsg::Heard { hear } => Some((conn, petros::decode::<Shown>(&hear).unwrap())),
            _ => None,
        })
        .collect()
}

/// The last thing one connection was told.
fn last(server: &mut Server<Todo>, conn: u64) -> Option<Shown> {
    heard(server)
        .into_iter()
        .filter(|(c, _)| *c == conn)
        .map(|(_, s)| s)
        .next_back()
}

fn open(db: Connection) -> Server<Todo> {
    watched(db).0
}

/// A server, and the log of everything its machine was asked to do.
fn watched(db: Connection) -> (Server<Todo>, Log) {
    let seen: Log = Arc::default();
    let live = Chalkboard {
        boards: BTreeMap::new(),
        seen: seen.clone(),
    };
    (Server::open_with(db, Table).unwrap().with_live(live), seen)
}

/// Everything the machine has been asked, since it was last asked.
fn calls(seen: &Log) -> Vec<String> {
    std::mem::take(&mut seen.lock().unwrap())
}

/// Two devices of one account are in one room; a third account is in another,
/// and hears none of it.
#[test]
fn a_room_is_an_account_and_not_a_connection() {
    let mut s = open(petros::open_memory().unwrap());
    s.recv(1, hello("alice/phone")).unwrap();
    s.recv(2, hello("alice/laptop")).unwrap();
    s.recv(3, hello("bob/phone")).unwrap();
    heard(&mut s);

    s.recv(1, say("gigue")).unwrap();
    let told = heard(&mut s);
    let to: Vec<u64> = told.iter().map(|(c, _)| *c).collect();
    assert_eq!(to, [1, 2], "alice's two devices, and nobody else's");
    assert_eq!(told[1].1.word, "gigue");
    assert_eq!(told[1].1.by, "phone");
    assert_eq!(told[1].1.here, ["phone", "laptop"]);
}

/// A live frame is not a log entry: it gets no sequence number, is not
/// deduped, and leaves the log exactly as it was.
#[test]
fn saying_something_writes_nothing_to_the_log() {
    let mut s = open(petros::open_memory().unwrap());
    s.recv(1, hello("alice/phone")).unwrap();
    s.recv(1, say("gigue")).unwrap();
    s.recv(1, say("gigue")).unwrap();
    assert_eq!(s.head(), 0, "nothing was appended");
}

/// The thing the whole design is for: a device's socket going does not take
/// what it was doing with it, and does not close a room somebody else is in.
///
/// The second half has to be asserted by what a *later* reader sees. Telling
/// the room on the way out says nothing about it, because that frame is built
/// before anything is cleared — which is exactly how an earlier version of
/// this test passed against a server that dropped the room on every part.
#[test]
fn a_departed_device_is_still_remembered() {
    let (mut s, log) = watched(petros::open_memory().unwrap());
    s.recv(1, hello("alice/phone")).unwrap();
    s.recv(2, hello("alice/laptop")).unwrap();
    s.recv(1, say("gigue")).unwrap();
    heard(&mut s);
    calls(&log);

    s.disconnect(1);
    let seen = last(&mut s, 2).expect("the laptop is told");
    assert_eq!(seen.here, ["laptop"], "the phone has gone");
    let said = calls(&log);
    assert_eq!(
        said,
        ["part phone"],
        "a room with somebody in it is not closed"
    );

    // And the board is still there to be read, which is the half that a
    // frame built on the way out cannot show.
    s.recv(3, hello("alice/tablet")).unwrap();
    let seen = last(&mut s, 3).expect("the tablet is told on joining");
    assert_eq!(
        (seen.word.as_str(), seen.by.as_str()),
        ("gigue", "phone"),
        "what the phone was doing outlived its socket"
    );
}

/// A reconnect is the same device: it does not appear twice, which is what
/// keeps a picker from filling up with a phone that slept.
#[test]
fn a_reconnect_replaces_a_device_rather_than_adding_one() {
    let mut s = open(petros::open_memory().unwrap());
    s.recv(1, hello("alice/phone")).unwrap();
    s.recv(1, say("gigue")).unwrap();
    s.disconnect(1);
    // Same device, new connection — a suspended phone waking up.
    s.recv(7, hello("alice/phone")).unwrap();
    let seen = last(&mut s, 7).expect("told on joining");
    assert_eq!(seen.here, ["phone"]);
    assert_eq!(seen.word, "gigue", "it resumed rather than started over");
}

/// An empty room is written down and dropped, and comes back off the disk.
/// Both halves are the point: dropping it is what bounds the memory, and the
/// row is what survives a restart.
#[test]
fn an_empty_room_is_kept_on_disk_and_not_in_memory() {
    let db = tempfile::NamedTempFile::new().unwrap();
    let path = db.path().to_str().unwrap().to_string();

    let (mut s, log) = watched(petros::open_path(&path).unwrap());
    s.recv(1, hello("alice/phone")).unwrap();
    s.recv(1, say("gigue")).unwrap();
    s.disconnect(1);
    let said = calls(&log);
    assert!(
        said.contains(&"close alice".to_string()),
        "the room was let go: {said:?}"
    );

    // A different server over the same file, which is what a restart is.
    let (mut s, log) = watched(petros::open_path(&path).unwrap());
    s.recv(1, hello("alice/phone")).unwrap();
    let said = calls(&log);
    assert_eq!(
        said.first().map(String::as_str),
        Some("wake alice"),
        "the room read the disk before it was told about a device: {said:?}"
    );
    let seen = last(&mut s, 1).expect("told on joining");
    assert_eq!(seen.word, "gigue", "yesterday's board");
    assert_eq!(seen.by, "phone");
}

/// …and reading it back happens once per room per run. Without that, a room
/// that empties and reopens inside one run would read a row it has since
/// moved past — a memory of a memory.
#[test]
fn a_room_is_woken_once_while_it_stays_occupied() {
    let db = tempfile::NamedTempFile::new().unwrap();
    let path = db.path().to_str().unwrap().to_string();

    // Write a room down, so there *is* something on disk to wake from — which
    // is what makes the count below mean anything.
    let (mut s, _) = watched(petros::open_path(&path).unwrap());
    s.recv(1, hello("alice/phone")).unwrap();
    s.recv(1, say("gigue")).unwrap();
    s.disconnect(1);

    let (mut s, log) = watched(petros::open_path(&path).unwrap());
    s.recv(1, hello("alice/phone")).unwrap();
    s.recv(2, hello("alice/laptop")).unwrap();
    s.recv(1, say("air")).unwrap();
    s.recv(3, hello("alice/tablet")).unwrap();
    let woke: Vec<String> = calls(&log)
        .into_iter()
        .filter(|c| c.starts_with("wake"))
        .collect();
    assert_eq!(woke, ["wake alice"], "once, on the first device in");
}

/// A server with no machine carries the log and drops the live frames, rather
/// than failing on them. That is what lets a client built against a newer
/// server talk to an older one at all.
#[test]
fn a_server_with_no_realtime_half_ignores_the_channel() {
    let mut s = Server::<Todo>::open_with(petros::open_memory().unwrap(), Table).unwrap();
    s.recv(1, hello("alice/phone")).unwrap();
    s.recv(1, say("gigue")).unwrap();
    assert!(heard(&mut s).is_empty());
    assert_eq!(s.head(), 0);
}

/// A peer the server stands in for is in a room like anything else, and is
/// told what the room says — which is how a speaker with no login and no
/// replica takes part at all.
#[test]
fn a_standing_peer_is_in_the_room() {
    let mut s = open(petros::open_memory().unwrap());
    s.recv(1, hello("alice/phone")).unwrap();
    s.stand(
        99,
        Identity {
            user: ActorId::from("alice"),
            session: "media_player.kitchen".into(),
        },
    )
    .unwrap();
    heard(&mut s);

    s.recv(1, say("gigue")).unwrap();
    let told = heard(&mut s);
    assert!(
        told.iter().any(|(c, _)| *c == 99),
        "the speaker heard it: {told:?}"
    );
    assert_eq!(told[0].1.here, ["phone", "media_player.kitchen"]);
}
