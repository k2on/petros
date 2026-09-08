//! Petros's own three tables.
//!
//! Diesel's `table!` describes a schema, it does not create one: the DDL lives
//! in [`crate::store::migrate`] and this is its mirror. Keep the two in step —
//! `check_for_backend` on the models will catch a type that drifts, but not a
//! column that only exists on one side.

diesel::table! {
    /// The confirmed, server-ordered log. `payload` is the CBOR encoding of the
    /// mutation, exactly as it went over the wire.
    petros_log (seq) {
        seq -> BigInt,
        id -> Binary,
        actor -> Text,
        payload -> Binary,
    }
}

diesel::table! {
    /// A client's own unconfirmed mutations, in the order they were made.
    petros_pending (ord) {
        ord -> BigInt,
        id -> Binary,
        actor -> Text,
        payload -> Binary,
    }
}

diesel::table! {
    /// Key/value scratchpad. Currently just the sync cursor.
    petros_meta (k) {
        k -> Text,
        v -> BigInt,
    }
}

diesel::allow_tables_to_appear_in_same_query!(petros_log, petros_pending, petros_meta);
