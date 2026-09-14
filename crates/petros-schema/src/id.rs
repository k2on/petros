//! An id that knows what it identifies.
//!
//! Sixteen bytes on the wire, in the log and in SQLite — exactly what an
//! untagged id was — plus a marker saying which table they name. The marker is
//! a zero-sized `PhantomData`, so a tagged id costs nothing at runtime and the
//! stored bytes are unchanged; what it buys is that `Id<Playlist>` and
//! `Id<Media>` are different types, and the compiler refuses the swap.
//!
//! That swap is not hypothetical. `add_to_playlist(playlist, media)` took two
//! ids of the same type, so calling it with the arguments the wrong way round
//! compiled, ran, wrote a row, and produced a playlist entry pointing at
//! nothing — no error on any peer.
//!
//! No dependencies, deliberately. Row types compile into the wasm module, and
//! nothing here may drag `uuid` in behind them, so the canonical text form is
//! written and parsed by hand below.

use crate::store::{Bind, Cell, Value};
use core::marker::PhantomData;

/// The identity of a row in `T`.
///
/// `PhantomData<fn() -> T>` rather than `PhantomData<T>`: it makes `Id<T>`
/// unconditionally `Send`, `Sync` and `Copy` whatever `T` is, and `T` appears
/// only in the type, never in a value.
pub struct Id<T> {
    bytes: [u8; 16],
    of: PhantomData<fn() -> T>,
}

impl<T> Id<T> {
    /// The all-zero id, which is what a field holds before `fill_auto` runs.
    pub const fn nil() -> Self {
        Id {
            bytes: [0; 16],
            of: PhantomData,
        }
    }

    pub const fn from_bytes(bytes: [u8; 16]) -> Self {
        Id {
            bytes,
            of: PhantomData,
        }
    }

    /// Sixteen bytes, or `None`. The length is the whole check: any sixteen
    /// bytes are a valid id, and the log has carried some that no version of
    /// uuid would generate.
    pub fn from_slice(raw: &[u8]) -> Option<Self> {
        Some(Id::from_bytes(raw.try_into().ok()?))
    }

    pub const fn as_bytes(&self) -> &[u8; 16] {
        &self.bytes
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.bytes.to_vec()
    }

    pub fn is_nil(&self) -> bool {
        self.bytes == [0; 16]
    }

    /// The same bytes, naming a different table.
    ///
    /// Every use is a place the types stopped helping, so it is spelled out
    /// rather than available as a `From`. A join whose two sides are genuinely
    /// the same row is the honest case.
    pub fn retag<U>(&self) -> Id<U> {
        Id::from_bytes(self.bytes)
    }
}

/// The canonical 8-4-4-4-12 form, lowercase.
impl<T> core::fmt::Display for Id<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        const DASH: [usize; 4] = [4, 6, 8, 10];
        for (i, b) in self.bytes.iter().enumerate() {
            if DASH.contains(&i) {
                f.write_str("-")?;
            }
            write!(f, "{b:02x}")?;
        }
        Ok(())
    }
}

/// Shows the id rather than the marker, which has nothing to show.
impl<T> core::fmt::Debug for Id<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "Id({self})")
    }
}

/// Parsing the canonical form, with or without its dashes.
impl<T> core::str::FromStr for Id<T> {
    type Err = ParseIdError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut bytes = [0u8; 16];
        let mut hex = s.bytes().filter(|c| *c != b'-');
        for slot in bytes.iter_mut() {
            let hi = hex.next().ok_or(ParseIdError)?;
            let lo = hex.next().ok_or(ParseIdError)?;
            *slot = (nibble(hi).ok_or(ParseIdError)? << 4) | nibble(lo).ok_or(ParseIdError)?;
        }
        if hex.next().is_some() {
            return Err(ParseIdError);
        }
        Ok(Id::from_bytes(bytes))
    }
}

fn nibble(c: u8) -> Option<u8> {
    Some(match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => return None,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseIdError;

impl core::fmt::Display for ParseIdError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("not an id: expected thirty-two hex digits")
    }
}

impl std::error::Error for ParseIdError {}

// The derives are written out because `derive` would put a `T: Clone` bound on
// every one of them, and the marker is never a value — an `Id<T>` is `Copy`
// however un-`Copy` its table is.
impl<T> Clone for Id<T> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T> Copy for Id<T> {}
impl<T> PartialEq for Id<T> {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}
impl<T> Eq for Id<T> {}
impl<T> PartialOrd for Id<T> {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl<T> Ord for Id<T> {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.bytes.cmp(&other.bytes)
    }
}
impl<T> core::hash::Hash for Id<T> {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.bytes.hash(state);
    }
}
impl<T> Default for Id<T> {
    fn default() -> Self {
        Id::nil()
    }
}

impl<T> Bind for Id<T> {
    fn to_value(&self) -> Value {
        Value::Blob(self.bytes.to_vec())
    }
}

impl<T> Cell for Id<T> {
    fn from_value(v: &Value) -> Option<Self> {
        Id::from_slice(v.as_blob()?)
    }
}

/// Exactly as an untagged id encoded: sixteen bytes in a binary format, the
/// canonical text in a human-readable one. That is `uuid`'s own rule, and
/// matching it is what lets a log written before ids were tagged still decode
/// — the marker is not in the data and never was.
#[cfg(feature = "serde")]
impl<T> serde::Serialize for Id<T> {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        if s.is_human_readable() {
            s.collect_str(self)
        } else {
            s.serialize_bytes(&self.bytes)
        }
    }
}

#[cfg(feature = "serde")]
impl<'de, T> serde::Deserialize<'de> for Id<T> {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = [u8; 16];

            fn expecting(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.write_str("sixteen bytes, or the canonical id text")
            }

            fn visit_bytes<E: serde::de::Error>(self, v: &[u8]) -> Result<Self::Value, E> {
                v.try_into()
                    .map_err(|_| E::invalid_length(v.len(), &"sixteen bytes"))
            }

            // ciborium hands a byte string to `visit_seq` in some shapes, so
            // both are accepted rather than one working and the other not.
            fn visit_seq<A>(self, mut a: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                let mut out = [0u8; 16];
                for (i, slot) in out.iter_mut().enumerate() {
                    *slot = a
                        .next_element()?
                        .ok_or_else(|| serde::de::Error::invalid_length(i, &"sixteen bytes"))?;
                }
                Ok(out)
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
                use core::str::FromStr;
                Id::<()>::from_str(v)
                    .map(|id| *id.as_bytes())
                    .map_err(|_| E::custom("not an id"))
            }
        }
        if d.is_human_readable() {
            d.deserialize_str(V).map(Id::from_bytes)
        } else {
            d.deserialize_bytes(V).map(Id::from_bytes)
        }
    }
}
