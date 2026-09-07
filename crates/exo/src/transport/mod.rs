//! A minimal WebSocket transport, so the examples have something to talk over.
//!
//! This is not part of the core and is not meant to be good. `exo::client` and
//! `exo::server` never touch a socket; everything here is a thin loop that
//! moves CBOR frames between a socket and a state machine's queues. Replace it
//! with anything — QUIC, a Unix socket, a channel between two threads — without
//! touching a line of the engine.

pub mod ws;
