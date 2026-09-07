//! The only clock and the only source of randomness in the crate.
//!
//! [`Mutation::apply`](crate::Mutation::apply) must be a pure function of
//! `(transaction state, mutation args)`. Every non-deterministic value a
//! mutation needs — an id, a timestamp — is therefore hoisted out of `apply`
//! and into the mutation's arguments by
//! [`fill_auto`](crate::Mutation::fill_auto), which runs exactly once, at the
//! originating client, before the entry is written anywhere. From that moment
//! the value is frozen in the log forever, so replaying the log a year later on
//! a different machine produces the same state.

use rand::rngs::StdRng;
use rand::{RngCore, SeedableRng};

// `std::time::SystemTime::now` panics on `wasm32-unknown-unknown` — there is no
// clock in the target, only in the host. `web-time` is the same API backed by
// `performance.now`/`Date` in a browser and by `std` everywhere else.
#[cfg(not(target_arch = "wasm32"))]
use std::time::{SystemTime, UNIX_EPOCH};
#[cfg(target_arch = "wasm32")]
use web_time::{SystemTime, UNIX_EPOCH};

use crate::Id;

/// Where wall-clock time comes from.
enum Clock {
    System,
    /// A virtual clock, so tests are reproducible.
    Virtual {
        now_ms: i64,
        step_ms: i64,
    },
}

/// Handed to [`fill_auto`](crate::Mutation::fill_auto). Seedable, so a test can
/// replay a whole session byte-for-byte.
///
/// ```
/// # use exo::AutoCtx;
/// let mut a = AutoCtx::seeded(7);
/// let mut b = AutoCtx::seeded(7);
/// assert_eq!(a.uuid(), b.uuid());
/// assert_eq!(a.now_ms(), b.now_ms());
/// ```
pub struct AutoCtx {
    rng: StdRng,
    clock: Clock,
}

impl AutoCtx {
    /// Real time, real randomness. What a shipping client uses.
    pub fn system() -> Self {
        AutoCtx {
            rng: StdRng::from_entropy(),
            clock: Clock::System,
        }
    }

    /// Reproducible time and randomness from a seed. The virtual clock starts
    /// at 2020-01-01T00:00:00Z and advances one second per read.
    pub fn seeded(seed: u64) -> Self {
        Self::seeded_at(seed, 1_577_836_800_000, 1_000)
    }

    /// Reproducible, with an explicit clock origin and step.
    pub fn seeded_at(seed: u64, start_ms: i64, step_ms: i64) -> Self {
        AutoCtx {
            rng: StdRng::seed_from_u64(seed),
            clock: Clock::Virtual {
                now_ms: start_ms,
                step_ms,
            },
        }
    }

    /// Milliseconds since the Unix epoch.
    pub fn now_ms(&mut self) -> i64 {
        match &mut self.clock {
            Clock::System => SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as i64)
                .unwrap_or_default(),
            Clock::Virtual { now_ms, step_ms } => {
                let t = *now_ms;
                *now_ms += *step_ms;
                t
            }
        }
    }

    /// A fresh random v4 UUID.
    pub fn uuid(&mut self) -> Id {
        let mut bytes = [0u8; 16];
        self.rng.fill_bytes(&mut bytes);
        Id(uuid::Builder::from_random_bytes(bytes).into_uuid())
    }

    /// A fresh random `u64`, for apps that need an opaque token.
    pub fn u64(&mut self) -> u64 {
        self.rng.next_u64()
    }
}

impl Default for AutoCtx {
    fn default() -> Self {
        Self::system()
    }
}

impl std::fmt::Debug for AutoCtx {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AutoCtx")
    }
}
