//! A deterministic, in-process simulation of a network.
//!
//! Seeded, no sockets, no threads, no sleeps. A three-week partition is a few
//! [`step`](Sim::step) calls. Every message is a value in a queue, so dropping,
//! duplicating and reordering are one line each — and a failing seed is a
//! failing seed forever, which is the property that makes a convergence bug
//! something you can sit down and fix.
//!
//! This is the machinery Petros tests itself with, made generic so an app can
//! point it at its own mutations:
//!
//! ```ignore
//! let mut sim = Sim::<TodoApp>::new(7, 3);
//! for i in 0..3 {
//!     sim.mutate(i, todo::add("something"));
//! }
//! sim.partition(1);
//! for _ in 0..50 { sim.step(); }
//! sim.settle();
//! assert_eq!(sim.state_hash(0), sim.state_hash(1));
//! ```
//!
//! What it does *not* simulate is a second implementation. Every client here
//! runs the same `apply` in the same process, so this finds ordering and rebase
//! bugs, not the kind where two builds of a domain disagree — that is what a
//! conformance test is for.

use std::collections::VecDeque;

use diesel::deserialize::QueryableByName;
use diesel::sql_types::Text;
use diesel::{sql_query, RunQueryDsl};
use petros::{App, AutoCtx, Client, ClientMsg, ConnId, Connection, Server, ServerMsg};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

/// One simulated client and the two wires attached to it.
struct Node<A: App> {
    conn: ConnId,
    client: Client<A>,
    partitioned: bool,
    /// Messages in flight towards the server, and towards this client.
    up: VecDeque<ClientMsg<A::Mutation>>,
    down: VecDeque<ServerMsg<A::Mutation>>,
}

/// A server and some clients, joined by a network you control.
pub struct Sim<A: App> {
    rng: StdRng,
    server: Server<A>,
    nodes: Vec<Node<A>>,
}

impl<A: App> Sim<A>
where
    A::Mutation: Clone,
{
    /// `seed` fixes everything: the network's choices and each client's clock
    /// and uuids. The same seed is the same run, on any machine, forever.
    pub fn new(seed: u64, clients: usize) -> Self {
        let server = Server::open(petros::open_memory().expect("open server db")).expect("server");
        let nodes = (0..clients)
            .map(|i| {
                let client = Client::open(
                    petros::open_memory().expect("open client db"),
                    format!("c{i}"),
                    // Each client gets its own seeded clock and RNG, so ids and
                    // timestamps differ between them but never between runs.
                    AutoCtx::seeded(seed.wrapping_mul(31).wrapping_add(i as u64)),
                )
                .expect("client");
                Node {
                    conn: i as ConnId + 1,
                    client,
                    partitioned: false,
                    up: VecDeque::new(),
                    down: VecDeque::new(),
                }
            })
            .collect();
        let mut sim = Sim {
            rng: StdRng::seed_from_u64(seed),
            server,
            nodes,
        };
        for i in 0..clients {
            sim.nodes[i].client.connected().expect("hello");
        }
        sim
    }

    pub fn clients(&self) -> usize {
        self.nodes.len()
    }

    pub fn client(&mut self, i: usize) -> &mut Client<A> {
        &mut self.nodes[i].client
    }

    /// Diesel needs `&mut` even to read, so a test that inspects a client's
    /// view borrows it mutably.
    pub fn conn(&mut self, i: usize) -> &mut Connection {
        self.nodes[i].client.conn()
    }

    pub fn server_conn(&mut self) -> &mut Connection {
        self.server.conn()
    }

    /// Author a mutation at one client. A locally invalid intent is the app's
    /// business, not the simulation's.
    /// Takes anything that becomes the app's mutation, like `Client::mutate`,
    /// so an authoring function's result goes straight in.
    pub fn mutate(&mut self, i: usize, m: impl Into<A::Mutation>) {
        let _ = self.nodes[i].client.mutate(m);
    }

    /// Cut this client off. Anything already in flight either way is lost, as
    /// it would be with a dropped socket.
    pub fn partition(&mut self, i: usize) {
        let node = &mut self.nodes[i];
        node.partitioned = true;
        node.up.clear();
        node.down.clear();
        self.server.disconnect(node.conn);
    }

    /// Reconnect. The client says hello and re-offers everything still pending;
    /// the server dedupes whatever it has already seen.
    pub fn heal(&mut self, i: usize) {
        if self.nodes[i].partitioned {
            self.nodes[i].partitioned = false;
            self.nodes[i].client.connected().expect("hello");
        }
    }

    /// One tick of an unreliable network: move what each side wants to send
    /// onto the wire, then deliver a random subset — possibly out of order,
    /// possibly twice, possibly not at all.
    pub fn step(&mut self) {
        self.pump();
        for i in 0..self.nodes.len() {
            if self.nodes[i].partitioned {
                continue;
            }
            if let Some(msg) = self.pick_up(i) {
                self.server
                    .recv(self.nodes[i].conn, msg)
                    .expect("server recv");
            }
            if let Some(msg) = self.pick_down(i) {
                self.nodes[i].client.recv(msg).expect("client recv");
            }
        }
    }

    /// Heal everything and run to quiescence with a perfect network.
    ///
    /// Every client reconnects, not just the partitioned ones: a message the
    /// network ate is only recovered because a real client says hello and
    /// re-offers what it still has pending, and the settled state has to depend
    /// on that rather than on the network having been kind.
    ///
    /// # Panics
    /// If the simulation does not settle. That is a bug in the engine or the
    /// app, and hanging would be a worse way to report it.
    pub fn settle(&mut self) {
        for i in 0..self.nodes.len() {
            self.nodes[i].partitioned = false;
            self.nodes[i].client.connected().expect("hello");
        }
        for _ in 0..10_000 {
            self.pump();
            if self.quiescent() {
                return;
            }
            for i in 0..self.nodes.len() {
                while let Some(msg) = self.nodes[i].up.pop_front() {
                    self.server
                        .recv(self.nodes[i].conn, msg)
                        .expect("server recv");
                }
                self.pump();
                while let Some(msg) = self.nodes[i].down.pop_front() {
                    self.nodes[i].client.recv(msg).expect("client recv");
                }
            }
        }
        panic!("the simulation never settled");
    }

    /// A hash of one client's materialised app state. Two clients that agree
    /// have the same hash; the point of the whole engine is that they do.
    pub fn state_hash(&mut self, i: usize) -> u64 {
        state_hash(self.nodes[i].client.conn())
    }

    pub fn server_hash(&mut self) -> u64 {
        state_hash(self.server.conn())
    }

    /// Move outgoing messages onto the wire. A partitioned client's messages go
    /// nowhere, which is what makes it a partition rather than a delay.
    fn pump(&mut self) {
        for node in &mut self.nodes {
            let out = node.client.take_outgoing();
            if !node.partitioned {
                node.up.extend(out);
            }
        }
        for (conn, msg) in self.server.take_outgoing() {
            if let Some(node) = self.nodes.iter_mut().find(|n| n.conn == conn) {
                if !node.partitioned {
                    node.down.push_back(msg);
                }
            }
        }
    }

    fn quiescent(&self) -> bool {
        self.nodes
            .iter()
            .all(|n| n.up.is_empty() && n.down.is_empty())
    }

    fn pick_up(&mut self, i: usize) -> Option<ClientMsg<A::Mutation>> {
        let (idx, dup, drop_it) = self.delivery(self.nodes[i].up.len())?;
        let msg = self.nodes[i].up.remove(idx)?;
        if dup {
            self.nodes[i].up.push_back(msg.clone());
        }
        if drop_it {
            return None;
        }
        Some(msg)
    }

    fn pick_down(&mut self, i: usize) -> Option<ServerMsg<A::Mutation>> {
        let (idx, dup, drop_it) = self.delivery(self.nodes[i].down.len())?;
        let msg = self.nodes[i].down.remove(idx)?;
        if dup {
            self.nodes[i].down.push_back(msg.clone());
        }
        if drop_it {
            return None;
        }
        Some(msg)
    }

    /// Which queued message to touch, and what to do to it.
    fn delivery(&mut self, len: usize) -> Option<(usize, bool, bool)> {
        if len == 0 {
            return None;
        }
        let idx = self.rng.gen_range(0..len);
        let dup = self.rng.gen_ratio(1, 8);
        let drop_it = self.rng.gen_ratio(1, 16);
        Some((idx, dup, drop_it))
    }
}

impl<A: App> std::fmt::Debug for Sim<A> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sim")
            .field("clients", &self.nodes.len())
            .finish_non_exhaustive()
    }
}

// ------------------------------------------------------------- the state hash

#[derive(QueryableByName)]
struct Row {
    #[diesel(sql_type = Text, column_name = r)]
    r: String,
}

#[derive(QueryableByName)]
struct Name {
    #[diesel(sql_type = Text, column_name = name)]
    name: String,
}

/// A hash of everything the app owns, in a fully specified order.
///
/// Every table except Petros's own, every column in declaration order, every row
/// ordered by its own rendered text. Petros's tables are excluded on purpose:
/// two replicas agree about app state long before they agree about how much of
/// the log each has seen, and it is the app state the engine is promising
/// something about.
///
/// Rendering goes through SQLite's `quote()` so the hash is over values rather
/// than over a Rust model — which means it covers columns an app forgot to put
/// in its model, and does not have to be rewritten when the model changes.
pub fn state_hash(conn: &mut Connection) -> u64 {
    let mut h = FNV_OFFSET;
    for table in tables(conn) {
        eat(&mut h, table.as_bytes());
        for row in rendered(conn, &table) {
            eat(&mut h, row.as_bytes());
        }
    }
    h
}

/// The app's tables, never Petros's and never SQLite's own.
fn tables(conn: &mut Connection) -> Vec<String> {
    sql_query(
        "SELECT name FROM sqlite_schema WHERE type = 'table' \
         AND name NOT LIKE 'petros!_%' ESCAPE '!' \
         AND name NOT LIKE 'sqlite!_%' ESCAPE '!' ORDER BY name",
    )
    .load::<Name>(conn)
    .expect("read the schema")
    .into_iter()
    .map(|n| n.name)
    .collect()
}

/// Every row of one table as one string, ordered by that string.
fn rendered(conn: &mut Connection, table: &str) -> Vec<String> {
    let quoted = table.replace('\'', "''");
    let columns: Vec<String> = sql_query(format!(
        "SELECT name FROM pragma_table_info('{quoted}') ORDER BY cid"
    ))
    .load::<Name>(conn)
    .expect("read the columns")
    .into_iter()
    // `quote()` renders any value as the SQL literal for it — blobs as X'..',
    // NULL as the text NULL — so nothing is ambiguous and nothing is lossy.
    .map(|c| format!("quote(\"{}\")", c.name.replace('"', "\"\"")))
    .collect();
    if columns.is_empty() {
        return Vec::new();
    }
    sql_query(format!(
        "SELECT {} AS r FROM \"{}\" ORDER BY 1",
        columns.join(" || '|' || "),
        table.replace('"', "\"\"")
    ))
    .load::<Row>(conn)
    .expect("render the rows")
    .into_iter()
    .map(|r| r.r)
    .collect()
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn eat(h: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *h ^= u64::from(*b);
        *h = h.wrapping_mul(FNV_PRIME);
    }
}
