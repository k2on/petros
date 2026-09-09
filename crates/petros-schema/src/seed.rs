//! Turning one non-deterministic value into as many as a mutation needs.
//!
//! `fill_auto` is the only place a domain may look at a clock or a random
//! number, and it runs exactly once, at the originating client — everything
//! after that is a pure function of what it wrote, because every other replica
//! replays the entry rather than re-deciding it.
//!
//! That is fine while a verb needs one id. A verb that adds five rows needs
//! five, and `apply` cannot invent them: all it can reach is the store. So the
//! one uuid the caller supplied is expanded here, in the single place
//! non-determinism is allowed, and the log freezes the result.
//!
//! # This is not a random number generator
//!
//! It must not become a better one, either. Whatever this produces is written
//! into the log and replayed forever, so changing the algorithm changes what
//! old entries mean — the same class of break as renaming a mutation variant.
//! `tests/seed.rs` pins the output against a checked-in fixture for that
//! reason, the way `tests/wire.rs` pins the encoding.

/// A deterministic expansion of one seed, as xorshift128+.
///
/// The unpredictability is entirely in the bytes handed to [`Seed::from`];
/// everything after that is a pure function of them, which is what makes
/// replaying an entry reproduce its rows.
#[derive(Debug, Clone)]
pub struct Seed(u64, u64);

impl Seed {
    /// Seed from the uuid `fill_auto` was given.
    ///
    /// FNV-1a to get from bytes to a state, then `| 1` on both halves because
    /// xorshift is stuck at zero and a nil uuid is a real input.
    pub fn from(bytes: &[u8]) -> Self {
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x1000_0000_01b3);
        }
        Seed(h | 1, h.rotate_left(31) | 1)
    }

    /// The next value in the expansion.
    ///
    /// Not `next`: this is not an iterator, and a public method by that name
    /// reads like one at every call site.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        let y = self.1;
        self.0 = y;
        x ^= x << 23;
        x ^= x >> 17;
        x ^= y ^ (y >> 26);
        self.1 = x;
        x.wrapping_add(y)
    }

    /// Sixteen bytes, the shape of an id in the log.
    ///
    /// Big-endian so the bytes read the same on any target — the module runs on
    /// wasm and the server on whatever it runs on, and they have to agree.
    pub fn id(&mut self) -> Vec<u8> {
        let (a, b) = (self.next_u64(), self.next_u64());
        let mut out = Vec::with_capacity(16);
        out.extend_from_slice(&a.to_be_bytes());
        out.extend_from_slice(&b.to_be_bytes());
        out
    }
}
