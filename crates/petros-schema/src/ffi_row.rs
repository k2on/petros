//! One row, declared once, on both sides of a UniFFI boundary.
//!
//! An app's read model wants Rust types: `petros::Id`, `Option<i64>`, whatever
//! else says what it means. UniFFI wants types it can lower, and it cannot be
//! taught a foreign one — carrying `Id` across needs `impl FfiConverter for Id`,
//! a foreign trait on a foreign type, which the orphan rule refuses.
//! `uniffi::custom_type!` expands to exactly that impl and does not compile, and
//! `#[uniffi::remote]` re-declares a shape rather than mapping one.
//!
//! So there are two structs. What there does not have to be is two
//! declarations, drifting apart a field at a time.
//!
//! This macro emits nothing that mentions uniffi except the derive, so the
//! calling crate supplies it. That is also why this is `macro_rules!` rather
//! than a proc macro: no dependency, and it does not run for a crate that is
//! not building the FFI at all.

/// Declare a row and the record that carries it across UniFFI.
///
/// Fields are the same type on both sides unless told otherwise. A field that
/// differs states the far type and how to get there; a field that exists only
/// on the far side states how to compute it from the whole row.
///
/// ```
/// uniffi::setup_scaffolding!();
/// petros_schema::ffi_row! {
///     /// A song, and where it sits in the playlist.
///     Song => FfiSong {
///         /// Sixteen bytes here; the canonical string over there.
///         id: u64 => String { |id| id.to_string() },
///         title: String,
///         /// `Some(n)` if favourited. `-1` over there: positions start at 1,
///         /// so the sentinel is unambiguous and the record stays flat.
///         favorite_pos: Option<i64> => i64 { |p| p.unwrap_or(-1) },
///     } and {
///         /// Convenient on the far side, derivable here.
///         favorited: bool = |row| row.favorite_pos.is_some(),
///     }
/// }
///
///
/// fn main() {
///     let song = Song { id: 7, title: "Glue".into(), favorite_pos: Some(2) };
///     let crossed: FfiSong = song.into();
///     assert_eq!(crossed.id, "7");
///     assert_eq!(crossed.favorite_pos, 2);
///     assert!(crossed.favorited);
///
///     let unhearted = Song { id: 8, title: "Opal".into(), favorite_pos: None };
///     let crossed: FfiSong = unhearted.into();
///     assert_eq!(crossed.favorite_pos, -1, "the sentinel");
///     assert!(!crossed.favorited);
/// }
/// ```
///
/// `fn main` is spelled out because rustdoc otherwise wraps the whole example
/// in one, and `setup_scaffolding!` has to be at a crate root.
///
/// Add a field to the row and the record gains it too, so the two cannot drift.
// The doctest's `fn main` is not needless: rustdoc otherwise wraps the example
// in one, and `setup_scaffolding!` has to be at a crate root to define the
// `UniFfiTag` the derive resolves against.
#[allow(clippy::needless_doctest_main)]
#[macro_export]
macro_rules! ffi_row {
    (
        $(#[$meta:meta])*
        $src:ident => $ffi:ident {
            $(
                $(#[$fmeta:meta])*
                $field:ident : $ty:ty $(=> $fty:ty { $conv:expr })?
            ),* $(,)?
        }
        $(and {
            $(
                $(#[$dmeta:meta])*
                $derived:ident : $dty:ty = $dexpr:expr
            ),* $(,)?
        })?
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone)]
        pub struct $src {
            $(
                $(#[$fmeta])*
                pub $field: $ty,
            )*
        }

        $(#[$meta])*
        ///
        /// The record that crosses to a foreign caller. Generated from the row
        /// above by [`petros_schema::ffi_row!`](ffi_row) — add a field there and
        /// it appears here.
        #[derive(Debug, Clone, PartialEq, uniffi::Record)]
        pub struct $ffi {
            $(
                $(#[$fmeta])*
                pub $field: $crate::ffi_row!(@ty $ty $(, $fty)?),
            )*
            $($(
                $(#[$dmeta])*
                pub $derived: $dty,
            )*)?
        }

        impl ::core::convert::From<$src> for $ffi {
            fn from(row: $src) -> Self {
                $($(
                    // Through a fn pointer so the closure's parameter is
                    // inferred. A closure handed to a macro has nothing to
                    // infer from, and annotating one at every call site is a
                    // worse tax than this line.
                    let $derived: $dty = {
                        let f: fn(&$src) -> $dty = $dexpr;
                        f(&row)
                    };
                )*)?
                $ffi {
                    $(
                        $field: $crate::ffi_row!(@val row.$field, $ty $(, $fty, $conv)?),
                    )*
                    $($($derived,)*)?
                }
            }
        }
    };

    // The far type is the near one unless a second was given.
    (@ty $ty:ty) => { $ty };
    (@ty $ty:ty, $fty:ty) => { $fty };

    // …and the value crosses unchanged unless a conversion was given. The
    // conversion goes through a fn pointer for the same reason as above: it
    // gives the closure both its types.
    (@val $v:expr, $ty:ty) => { $v };
    (@val $v:expr, $ty:ty, $fty:ty, $conv:expr) => {{
        let f: fn($ty) -> $fty = $conv;
        f($v)
    }};
}
