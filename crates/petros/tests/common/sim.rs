//! The simulated network, pointed at the tests' own app.
//!
//! The machinery itself lives in `petros-testkit`, because an app needs it as
//! much as the engine does: the same partitions, drops, duplicates and
//! reordering, seeded the same way, against its own mutations.

#[allow(unused_imports)]
pub use petros_testkit::state_hash;

/// The tests' `Todo`, run across a simulated fleet.
pub type Sim = petros_testkit::Sim<super::todo::Todo>;
