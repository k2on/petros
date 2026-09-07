//! Convergence: the property the whole crate exists to provide.

mod common;

use common::sim::Sim;
use common::todo::{texts, TodoMutation};
use proptest::prelude::*;

#[test]
fn out_of_order_and_duplicate_delivery_is_idempotent() {
    // Seed 7's network drops, duplicates and reorders; the assertion is that
    // none of that is visible in the answer.
    let mut sim = Sim::new(7, 2);
    for i in 0..2 {
        for n in 0..5 {
            sim.mutate(i, TodoMutation::add(&format!("c{i}-{n}")));
            sim.step();
        }
    }
    sim.settle();

    assert_eq!(sim.state_hash(0), sim.state_hash(1));
    assert_eq!(sim.state_hash(0), sim.server_hash());
    assert_eq!(
        texts(sim.conn(0)).len(),
        10,
        "every mutation survived exactly once"
    );
}

#[test]
fn two_clients_converge_after_partition() {
    let mut sim = Sim::new(11, 2);
    sim.mutate(0, TodoMutation::add("shared"));
    sim.settle();
    assert_eq!(sim.state_hash(0), sim.state_hash(1));

    // Three weeks apart, both still usable.
    sim.partition(0);
    sim.partition(1);
    for n in 0..8 {
        sim.mutate(0, TodoMutation::add(&format!("alice-{n}")));
        sim.mutate(1, TodoMutation::add(&format!("bob-{n}")));
        sim.step();
    }
    assert_ne!(
        sim.state_hash(0),
        sim.state_hash(1),
        "they really did diverge while apart"
    );
    assert_eq!(texts(sim.conn(0)).len(), 9, "and each stayed usable alone");

    sim.heal(0);
    sim.heal(1);
    sim.settle();

    assert_eq!(sim.state_hash(0), sim.state_hash(1));
    assert_eq!(sim.state_hash(0), sim.server_hash());
    assert_eq!(
        texts(sim.conn(0)).len(),
        17,
        "nothing lost, nothing duplicated"
    );
}

/// What a client is told to do next. Kept small: the interesting variety comes
/// from the network, not from the app.
#[derive(Debug, Clone)]
enum Act {
    Add(usize),
    ClaimSomething(usize),
    RemoveSomething(usize),
    AddInvalid(usize),
    Step,
    Partition(usize),
    Heal(usize),
}

fn acts(n_clients: usize) -> impl Strategy<Value = Act> {
    let c = 0..n_clients;
    prop_oneof![
        6 => c.clone().prop_map(Act::Add),
        3 => c.clone().prop_map(Act::ClaimSomething),
        2 => c.clone().prop_map(Act::RemoveSomething),
        1 => c.clone().prop_map(Act::AddInvalid),
        8 => Just(Act::Step),
        2 => c.clone().prop_map(Act::Partition),
        2 => c.prop_map(Act::Heal),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 48, ..ProptestConfig::default() })]

    /// N clients, random mutations, random partitions and heals. Once the
    /// network is whole again every replica must agree — including the server.
    #[test]
    fn clients_converge_under_an_adversarial_network(
        seed in any::<u64>(),
        n_clients in 2usize..4,
        script in prop::collection::vec(acts(3), 1..120),
    ) {
        let mut sim = Sim::new(seed, n_clients);
        for act in script {
            match act {
                Act::Add(i) => sim.mutate(i % n_clients, TodoMutation::add("item")),
                Act::AddInvalid(i) => sim.mutate(i % n_clients, TodoMutation::add("")),
                Act::ClaimSomething(i) => {
                    let i = i % n_clients;
                    if let Some(id) = some_item(&sim, i) {
                        sim.mutate(i, TodoMutation::claim(id));
                    }
                }
                Act::RemoveSomething(i) => {
                    let i = i % n_clients;
                    if let Some(id) = some_item(&sim, i) {
                        sim.mutate(i, TodoMutation::remove(id));
                    }
                }
                Act::Step => sim.step(),
                Act::Partition(i) => sim.partition(i % n_clients),
                Act::Heal(i) => sim.heal(i % n_clients),
            }
        }
        sim.settle();

        let expected = sim.server_hash();
        for i in 0..sim.n_clients() {
            prop_assert_eq!(
                sim.state_hash(i),
                expected,
                "client {} diverged from the server (seed {})",
                i,
                seed
            );
            prop_assert_eq!(sim.client(i).pending_len(), 0, "seed {}", seed);
        }
    }
}

/// The first item a client can see, if any. Deliberately reads the client's own
/// view: that is what a real UI would offer the user to act on.
fn some_item(sim: &Sim, i: usize) -> Option<exo::Uuid> {
    common::todo::items(sim.conn(i)).first().map(|it| it.id)
}
