//! Who a Petros peer is.
//!
//! The engine is sans-io and asks one question about identity: at every
//! `Hello`, what does this token prove? This crate is the answer for a server
//! that signs people in with OpenID Connect, and the other half — how a
//! client gets a token to send — for the three kinds of client a Petros app
//! has: a desktop program, a page in a browser, and a phone.
//!
//! # The shape
//!
//! The server is the only OpenID Connect client. It has the client secret,
//! it talks to the provider, and it hands each signed-in peer a *session
//! token* of its own. That is what keeps every client the same size: none of
//! them speaks OpenID Connect, none holds a secret, and all three sign in by
//! opening one URL and receiving one code.
//!
//! ```text
//! client                          server                       provider
//!   |-- GET /auth/login?redirect=R -->|                            |
//!   |<-- 302 ------------------------|-- authorize?state&nonce -->|
//!   |            (the person signs in at the provider)           |
//!   |                                |<-- callback?code&state ----|
//!   |                                |-- token(code, secret) ---->|
//!   |                                |<-- id_token ---------------|
//!   |<-- 302 R?code=C ---------------|                            |
//!   |-- POST /auth/exchange {C} ---->|                            |
//!   |<-- {token, user, session} -----|                            |
//!   |-- Hello { token } ------------>|  (the engine, from here)   |
//! ```
//!
//! `R` is where the code goes back to: a loopback port the desktop is
//! listening on, the page's own origin, or the phone's URL scheme. The server
//! sends a code to no other place, which is what stands between a token and
//! anyone who can make a person click a link.
//!
//! `C` is single-use and lives a minute, so a URL in a browser history or a
//! log is worth nothing. The token it is exchanged for is what the client
//! keeps; the server keeps a hash of it beside the user it belongs to, and
//! the engine asks about it at every `Hello`.
//!
//! # Without a provider
//!
//! [`server::Mode::Dev`] signs anyone in as whatever name they give, and is
//! what `nix run .#serve` does on a laptop: two desktop peers named `alice`
//! and `bob` are still the whole demonstration of the rebase. A server has
//! to be told to run that way, and says so at startup.
//!
//! # What `apply` sees
//!
//! The user the server verified is the entry's `actor`, and the session it
//! was authored under is `ctx.session.id` — both frozen with the entry, both
//! checked by the server against the login that pushed it. A mutation that
//! writes `ctx.user.id` into a row is writing something a server vouched for.

#![deny(warnings)]
#![deny(missing_debug_implementations)]

mod login;
mod util;

#[cfg(feature = "server")]
pub mod oidc;
#[cfg(feature = "server")]
pub mod server;
#[cfg(feature = "server")]
pub mod session;

#[cfg(all(feature = "client", not(target_arch = "wasm32")))]
pub mod client;
#[cfg(all(feature = "client", target_arch = "wasm32"))]
pub mod web;

pub use login::{Account, Login};
pub use util::{login_url, socket_url};
