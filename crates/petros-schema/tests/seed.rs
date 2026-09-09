//! The expansion is part of the log, so it may not change.
//!
//! Whatever `Seed` produces at the originating client is written into the entry
//! and replayed by every other peer forever. "Improving" the generator would
//! change what old entries mean — the same class of break as renaming a
//! mutation variant — so this pins it against a fixture, the way `wire.rs` pins
//! the encoding.

use petros_schema::Seed;

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn uuid_bytes(s: &str) -> Vec<u8> {
    let hex: String = s.chars().filter(|c| *c != '-').collect();
    (0..16)
        .map(|i| u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).unwrap())
        .collect()
}

#[test]
fn the_expansion_is_a_pure_function_of_the_seed() {
    let uuid = [7u8; 16];
    let mut a = Seed::from(&uuid);
    let mut b = Seed::from(&uuid);
    for _ in 0..8 {
        assert_eq!(a.id(), b.id(), "same seed, same expansion");
    }
}

#[test]
fn different_seeds_expand_differently() {
    let mut a = Seed::from(&[1u8; 16]);
    let mut b = Seed::from(&[2u8; 16]);
    let one: Vec<Vec<u8>> = (0..4).map(|_| a.id()).collect();
    let two: Vec<Vec<u8>> = (0..4).map(|_| b.id()).collect();
    assert_ne!(one, two);
}

#[test]
fn ids_are_sixteen_bytes_and_distinct() {
    let mut s = Seed::from(&[3u8; 16]);
    let ids: Vec<Vec<u8>> = (0..64).map(|_| s.id()).collect();
    assert!(ids.iter().all(|i| i.len() == 16), "an id is sixteen bytes");
    let mut sorted = ids.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), ids.len(), "no collisions in one expansion");
}

/// A nil uuid is a real input — `AutoCtx::seeded` can produce one — and
/// xorshift is stuck at zero, which is what the `| 1` in `Seed::from` is for.
#[test]
fn a_nil_seed_still_expands() {
    let mut s = Seed::from(&[0u8; 16]);
    let ids: Vec<Vec<u8>> = (0..4).map(|_| s.id()).collect();
    assert!(ids.iter().all(|i| i != &vec![0u8; 16]), "not all zeroes");
    assert_ne!(ids[0], ids[1]);
}

#[test]
fn the_expansion_matches_the_fixture() {
    let uuid = uuid_bytes("67e55084-765d-446c-9191-4ff9861f6d8e");
    let mut s = Seed::from(&uuid);
    let got: Vec<String> = (0..4).map(|_| hex(&s.id())).collect();
    assert_eq!(
        got,
        std::fs::read_to_string("tests/seed.fixture")
            .expect("the fixture")
            .lines()
            .map(str::to_string)
            .collect::<Vec<_>>(),
        "the expansion changed — every entry already in a log would replay differently"
    );
}
