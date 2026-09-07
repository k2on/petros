//! A deterministic, in-process simulation of a network.
//!
//! Seeded, no sockets, no threads, no sleeps. A three-week partition is a few
//! `step()` calls. Every message is a value in a queue, so dropping,
//! duplicating and reordering are one line each.

use std::collections::VecDeque;

use exo::{AutoCtx, Client, ClientMsg, ConnId, Connection, Server, ServerMsg};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

use super::todo::{items, Todo, TodoMutation};

type Up = ClientMsg<TodoMutation>;
type Down = ServerMsg<TodoMutation>;

/// One simulated client and the two wires attached to it.
struct Node {
    conn: ConnId,
    client: Client<Todo>,
    partitioned: bool,
    /// Messages in flight towards the server, and towards this client.
    up: VecDeque<Up>,
    down: VecDeque<Down>,
}

pub struct Sim {
    rng: StdRng,
    server: Server<Todo>,
    nodes: Vec<Node>,
}

impl Sim {
    pub fn new(seed: u64, n_clients: usize) -> Self {
        let server = Server::open(exo::open_memory().expect("open server db")).expect("server");
        let nodes = (0..n_clients)
            .map(|i| {
                let client = Client::open(
                    exo::open_memory().expect("open client db"),
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
        for i in 0..n_clients {
            sim.nodes[i].client.connected().expect("hello");
        }
        sim
    }

    pub fn n_clients(&self) -> usize {
        self.nodes.len()
    }

    pub fn client(&mut self, i: usize) -> &mut Client<Todo> {
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

    pub fn mutate(&mut self, i: usize, m: TodoMutation) {
        // A locally invalid intent is the app's business, not the sim's.
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
    /// have the same hash; the point of the whole crate is that they do.
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

    fn pick_up(&mut self, i: usize) -> Option<Up> {
        let len = self.nodes[i].up.len();
        let (idx, dup, drop_it) = self.delivery(len)?;
        let msg = self.nodes[i].up.remove(idx)?;
        if dup {
            self.nodes[i].up.push_back(msg.clone());
        }
        if drop_it {
            return None;
        }
        Some(msg)
    }

    fn pick_down(&mut self, i: usize) -> Option<Down> {
        let len = self.nodes[i].down.len();
        let (idx, dup, drop_it) = self.delivery(len)?;
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

impl std::fmt::Debug for Sim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sim")
            .field("clients", &self.nodes.len())
            .finish_non_exhaustive()
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// A hash of the app's materialised state, read through the model in a fully
/// specified order. Exo's own tables are excluded: two replicas agree about app
/// state long before they agree about how much of the log each has seen.
pub fn state_hash(conn: &mut Connection) -> u64 {
    let mut h = FNV_OFFSET;
    for item in items(conn) {
        eat(&mut h, item.id.as_uuid().as_bytes());
        eat(&mut h, item.text.as_bytes());
        eat(&mut h, &[item.done as u8]);
        eat(&mut h, &item.pos.to_le_bytes());
        eat(&mut h, &item.created_ms.to_le_bytes());
        eat(&mut h, item.actor.as_bytes());
        match &item.claimed_by {
            None => eat(&mut h, &[0]),
            Some(who) => {
                eat(&mut h, &[1]);
                eat(&mut h, who.as_bytes());
            }
        }
    }
    h
}

fn eat(h: &mut u64, bytes: &[u8]) {
    for b in bytes {
        *h ^= u64::from(*b);
        *h = h.wrapping_mul(FNV_PRIME);
    }
}
