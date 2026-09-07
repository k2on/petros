# Decisions

A short ADR log. One paragraph per decision, newest last. These are the choices
that are not obvious from reading the code.

## The log is permanent

The server's log is append-only, totally ordered, and never rewritten. Every
byte written into it must still be readable by every future version of the app,
because state is *defined* as the result of replaying it. Concretely: mutation
variants are never renamed and never removed, new fields are added with
`#[serde(default)]`, and the type of an existing field never changes. If a
variant becomes obsolete, its `apply` may become a no-op, but the variant stays.
`tests/wire.rs` pins this with a checked-in fixture of old bytes; if that test
fails, the change under it would have broken every existing installation.

## CBOR, internally tagged

Payloads are CBOR via `ciborium`, and the mutation enum is `#[serde(tag = "t")]`.
Internal tagging keeps a variant's fields at the top level of the encoded map, so
adding a field is a compatible change and the tag is a string rather than a
positional index that a reordering could silently shift. CBOR because it is
compact, self-describing (which internal tagging requires) and has no schema
registry to keep in sync. The log stores the encoded payload verbatim rather than
exploded columns, so a future decoder gets the original bytes to work with.

## Mutations are intents, not facts

`AddToList { list, items }`, never `ItemInserted { pos: "a5" }`. `apply` may
read the database to decide what to write. This is the Replicache/Zero model, not
event sourcing: it is what lets a mutation still mean the right thing when it
lands after entries its author never saw. The toy to-do app in the tests computes
a row's `pos` from `MAX(pos) + 1` at apply time for exactly this reason — it is
what makes a rebase observable.

## Determinism, and where non-determinism is allowed to live

`apply` must be a pure function of `(transaction state, mutation args)`: no clock
reads, no RNG, no network, no filesystem, explicit `ORDER BY` on every query, and
no floats in anything affecting control flow. Everything non-deterministic is
hoisted into the mutation's arguments by `fill_auto`, which runs exactly once at
the originating client and takes its values from `AutoCtx` — the only clock and
the only source of randomness in the crate. Once `fill_auto` has run, the values
are frozen in the log forever, so a replay years later on a different machine
produces the same state. `AutoCtx` is seedable, which is what makes the whole
test suite reproducible.

## The rebase

A client's view is `replay(confirmed) then replay(pending)`. Because the server's
log is append-only and never reordered, confirmed state only ever moves forward;
the only thing ever undone is the client's own pending mutations. When confirmed
entries arrive, the client rolls its pending mutations back, applies the new
confirmed entries, and replays whatever is still pending on top. That is the
entire concurrency story — no CRDTs, no vector clocks, no merge functions.

## The rebase is implemented with a SQLite savepoint

The optimistic view lives inside a transaction with a savepoint that is held open
only while `pending` is non-empty:

```
local write:        [close any open savepoint]; BEGIN; record intent; COMMIT;
                    BEGIN; SAVEPOINT pending; replay(pending)
confirmed arrives:  ROLLBACK TO pending; RELEASE; COMMIT;
                    BEGIN; apply(confirmed...); COMMIT;
                    BEGIN; SAVEPOINT pending; replay(remaining pending)
last ack:           RELEASE; COMMIT      -- steady state, nothing held open
```

This started life as a naive implementation that dropped the app's tables and
replayed the whole log on every change. The savepoint version replaced it with
the same tests still green, which is the payoff for writing the tests first.

## Pending intents are committed; the optimistic view is not

The dance above has one wrinkle the sketch does not: a client's pending mutations
have to survive a crash — an offline-first app that loses a week of offline edits
is not offline-first — but the optimistic state they produce must stay
rollback-able, which means staying uncommitted. On one connection those two are
incompatible: you cannot commit anything while holding an open transaction you
still intend to roll back. So a local write closes the savepoint (discarding the
optimistic view), commits the intent to `exo_pending` on its own, then reopens the
savepoint and replays all pending mutations. The cost is that authoring a
mutation is O(pending) rather than O(1); pending is the set of mutations awaiting
an ack, which is small in every case we care about, and the alternative — a
second database file for the queue — buys that back only in exchange for
two files to keep in sync and a subtler crash story. Nothing is lost on a crash:
the confirmed log and the cursor are committed independently, and anything the
server accepted but we failed to commit is re-fetched by the next `Hello`.

## Diesel, and why not any of the others

Storage goes through Diesel: models are structs with `Queryable`/`Selectable`/
`Insertable` derives, queries are built with the DSL, and `check_for_backend`
verifies at compile time that each model still matches its table. The field was
narrow. `exo::client` and `exo::server` are sync by design, which rules out
sqlx, SeaORM, ormlite and rbatis — all async-first. turbosql owns a global
connection singleton, which cannot coexist with a connection Exo hands to
`apply`. That leaves Diesel as the only real sync ORM, and `sea-query` as a
query builder that would have layered onto rusqlite without replacing it. We
took the ORM: Diesel has no way to wrap an existing `rusqlite::Connection` — its
only constructor is `establish(url)` and there is no interop — so rusqlite is
gone from the crate entirely, `exo::Connection` is `diesel::SqliteConnection`,
and Exo's own three tables go through models like everything else.

## Diesel describes schemas, it does not create them

`table!` is a description, not a generator: it produces no DDL. So the `CREATE
TABLE` statements still live in `store::DDL` for Exo's tables and in
`App::migrate` for the app's, and `crate::schema` mirrors them by hand.
`check_for_backend(Sqlite)` catches a model whose types drift from `table!`, but
nothing catches a `table!` that drifts from the DDL — the test suite does, only
because every query in it goes through these models. This is the one place the
ORM gives less than it looks like it should.

## `Id`, because Diesel's UUID support is PostgreSQL-only

Diesel implements `ToSql<Uuid, Pg>` and nothing for SQLite, and the orphan rule
stops us implementing it for `uuid::Uuid` ourselves. So [`Id`] is a newtype
carrying the mapping to `Binary`. It is `#[serde(transparent)]`, so it encodes
exactly as the bare UUID did — the checked-in wire fixture predates it and still
decodes, which is the test that proves the migration cost nothing on the wire.

## Exo drives every transaction itself, behind Diesel's back

Diesel has a transaction manager; Exo never uses it. The optimistic savepoint
outlives any single call, so it cannot be expressed as a closure the way
`Connection::transaction` wants, and every boundary — `BEGIN`, `SAVEPOINT
pending`, `ROLLBACK TO`, `COMMIT` — is raw SQL through `batch_execute`. Verified
that this leaves Diesel's own manager healthy: a `transaction()` call after the
dance still works. Apps must not call `transaction()` on a client's connection;
the `Client` docs say so.

## `apply` takes `&mut Transaction`, and reads need `&mut` too

Diesel requires `&mut SqliteConnection` for every query, reads included. So
`Mutation::apply` takes `&mut Transaction` rather than `&Transaction`, and
`Client::conn`, `Server::conn` and `Client::pending_len` all take `&mut self`.
This is pure Diesel tax — nothing about the engine wants a mutable borrow to
read — and it is the visible cost of the ORM.

## The savepoint is tested by what another connection can see

There is no `is_autocommit` on a Diesel connection to assert against, so the
savepoint tests open a second connection to the same file and check that
optimistic state is invisible to it until the last ack commits. That is a better
test than the one it replaced: it asserts the property that actually matters —
uncommitted, therefore still revocable — rather than a proxy for it.

## Exo owns the `exo_` prefix and nothing else

Exo's three tables are `exo_log`, `exo_pending` and `exo_meta`. The prefix (the
brief says "log", "pending", "meta") exists so an app can never collide with
them, and so "everything that is not `exo_`" is a precise description of the
app's own schema.

## Sans-io

`exo::client` and `exo::server` are state machines: you feed them messages and
drain their outgoing queues. No async, no runtime, no sockets in the core. This
is what makes the deterministic simulation tests possible — a three-week network
partition is a few `step()` calls with no sleeps — and what will keep the FFI
bindings thin. The WebSocket transport lives behind the `ws` feature, is about a
page of code, and is meant to be replaceable without touching anything else.

## The server materialises state too

The server applies every mutation to its own database before appending it, which
is what makes `Reject` meaningful: a rejection is a deterministic verdict about
the mutation, reached identically by every replica. `MutationError::Rejected` is
that verdict; `MutationError::Sqlite` is an infrastructure failure and says
nothing about the mutation, so the two are never conflated.

## A conservative fan-out rule

After handling any message, the server sends every connected client every entry
above the cursor it has been sent. One rule covers initial sync, resume and live
broadcast, and it means a client sometimes receives its own entry twice — once in
an `Ack`, once in a `Batch`. That is deliberate: delivery is idempotent by
construction, so the common path exercises the same code as the recovery path.

## The transport is a thread per connection, and blocking

`exo::transport::ws` uses `tungstenite` synchronously: each socket is owned by
one thread that polls it with a short read timeout and writes whatever the state
machine has queued in between. That costs a thread per peer and up to one tick of
latency, and it is the right trade here — it keeps `async` and a runtime out of
the crate entirely, which is what the sans-io core exists to allow. Anything that
needs more can replace the module without touching the engine.

## Nix pins the toolchain, `flake.lock` pins Nix

`flake.nix` takes its Rust from `rust-overlay`, reading the version out of
`rust-toolchain.toml` rather than naming it again, so `rustc`, `clippy`,
`rustfmt` and `rust-analyzer` are identical on every machine — and identical
whether or not you are inside the devshell, since rustup reads the same file.
That indirection exists because the version was once written down in `flake.nix`
alone, nothing checked it, and it silently fell behind what the dependency tree
required until someone ran `just` in the shell and got a wall of MSRV errors.
One number, one file, used by both toolchains.

`rust-version` in the workspace manifest is a different number and means a
different thing: the oldest toolchain that can build the tree at all, which
`diesel_derives` (via `darling`) and `ratatui` (via `instability`) put at 1.88.
It is verified by building and testing on exactly that version rather than
asserted.
The flake inputs name a release branch rather than a revision because the
revision belongs in `flake.lock`; that file is generated by the first
`nix develop` and is what actually makes the shell reproducible. The shell
already carries `ffmpeg` and the iced runtime libraries, wired through
`LD_LIBRARY_PATH`, so later phases do not need to touch it.

## The examples are TUIs, and the terminal code is shared

Both examples are ratatui/crossterm interfaces rather than REPLs. The rebase is
a thing you watch rather than a thing you read about: press `o` in two peers,
add something in each, press `o` again, and an item you added while alone slides
down the list as the other peer's confirmed entries land underneath it. All the
terminal code lives in `examples/shared/tui.rs` so each example file stays about
Exo — which is what a reader opened it for — and the terminal is restored from a
panic hook as well as on drop, because raw mode outliving the process is a
miserable way to end a demo.

They are driven under tmux for verification rather than trusted to work: a real
pty, real keypresses, and the screen captured and checked, including the
partition-and-heal flow that the whole crate exists to make correct.

## The iced example, and why `view` cannot query the database

The iced app is an example alongside the terminal ones, sharing their to-do
domain and their server: `just serve`, then `just peer alice` for a TUI peer and
`just iced bob` for a window, and an item added in one appears in the other. The
engine is untouched by it — an ordinary `exo::Client` and an ordinary iced
program.

Being an example rather than its own crate costs one thing worth knowing:
`cargo build --example` compiles every dev-dependency, so a browser build would
drag in the test suite's and the terminal examples' dependencies, and `proptest`
reaches `wait-timeout`, which has no wasm. They are declared under
`[target.'cfg(not(target_arch = "wasm32"))'.dev-dependencies]` for that reason.

One shape is worth noting. iced's `view` takes `&self` and Diesel needs `&mut`
for every query, reads included, so a query cannot happen during rendering. The
app therefore keeps the materialised rows and the pending count in its own state
and refreshes them after each mutation. That is the right shape for iced anyway —
`view` should be cheap and pure — and it is precisely the seam a reactive query
layer would slot into later.


## SQLite in the browser, without forking anything

The browser build works. `wasm32-unknown-unknown` has no libc, so SQLite's C
cannot be compiled by `libsqlite3-sys`'s `bundled` feature — its build script
handles `wasm32-wasi*` only, and WASI is not what a browser runs.
`sqlite-wasm-rs` already solves that problem: it ships a musl
header-and-source shim and compiles SQLite for the browser, delegating malloc,
math and localtime to Rust.

What makes it fit together is that `libsqlite3-sys` is only a set of
`extern "C"` declarations. If something else in the crate graph defines those
symbols, the link succeeds. So on wasm, `exo` depends on `sqlite-wasm-rs` for
the SQLite and takes `libsqlite3-sys` *without* `bundled`, so there are not two
of them; Diesel never knows the difference. Cargo unifies features per target,
so `[target.'cfg(not(target_arch = "wasm32"))'.dependencies]` keeps `bundled`
on for the desktop and the server while leaving it off for the browser. That is
what makes the split expressible without a `[patch]` — and a `[patch]` could not
have worked anyway: it is not target-conditional, and `sqlite-wasm-rs` fails
`libsqlite3-sys`'s version requirement, so cargo silently ignores it. Silently
is the dangerous part: an ignored patch still builds, and an rlib never links,
so it looks like it worked.

One wart remains. Unbundled `libsqlite3-sys` still emits `-lsqlite3` even though
it is not the one providing SQLite, so the browser build points it at an empty
archive — `printf '!<arch>\n'`, a valid empty `ar` file, generated by the recipe
rather than committed — and the real symbols come from `sqlite-wasm-rs` in the
same module. The resulting wasm has no unresolved SQLite imports.

## The clock does not exist on wasm

`std::time::SystemTime::now()` panics on `wasm32-unknown-unknown`: the target
has no clock, only its host does. `AutoCtx::system()` reads it lazily, so this
did not surface until a mutation actually ran in a browser — which is exactly
why a build that compiles is not evidence that anything works. `web-time` is the
same API backed by the browser's clock, and `AutoCtx` uses it there.

## The browser is verified by running it

`clients/iced/index.html` takes a `?selftest` query, which drives Exo directly —
two adds, a toggle, and an empty to-do that must be refused — and prints the
result to the console. It exists because a rendering engine is a poor place to
find out whether a database works: the self-test answers that question with iced
entirely out of the picture, and it is what caught the clock panic above.

Two caveats stand, both in the UI layer and neither in the engine. Under
headless SwiftShader no text glyphs paint, though widgets, layout and typed
input all work and the same code draws text correctly on the desktop — a real
GPU is the place to confirm that. And `winit`'s web event loop can panic with
"RefCell already borrowed" under synthetic input.

## `CC` in the environment hijacks the wasm build

`cc-rs` picks a compiler by looking for `CC_wasm32_unknown_unknown`, then
`TARGET_CC`, then plain `CC`. A shell that exports `CC` — the nix devshell does,
and so does most of CI — therefore hands the *host* compiler a
`wasm32-unknown-unknown` compile, and gcc cannot target wasm at all. The failure
is loud but misleading: pages of errors from inside glibc's headers, because the
host toolchain went looking for a libc that has no business being there.

So the browser recipe sets `CC_wasm32_unknown_unknown` explicitly. In the
devshell that points at an *unwrapped* clang, because nix's wrapped clang
injects the system's glibc include paths and fails in the same way for the same
reason. This did not show up on the machine the browser build was first proven
on, for the least satisfying possible reason: `CC` happened to be unset there,
so `cc-rs` defaulted to clang and everything worked by luck.

Unwrapping clang then costs you the thing the wrapper was also doing: telling
clang where its own builtin headers live. An unwrapped nix clang cannot find
`stddef.h`, because nixpkgs puts the resource directory in a separate output
from the binary. So the devshell passes `-resource-dir` too, located with
`lib.getLib` rather than a literal path so it survives that output split. Outside
nix the recipe asks `clang -print-resource-dir` itself, and a normally-installed
clang answers correctly.

## The wasm-bindgen CLI has to match the crate exactly, so the recipe fetches it

The JS glue and the wasm module carry a bindgen schema version, and the two must
be identical — a CLI one release out refuses to run, which is the right call and
a clear message. That makes "install wasm-bindgen-cli" an unusually sharp
dependency: the version is whatever `Cargo.lock` resolved, not whatever a distro
or nixpkgs happens to ship. nixpkgs 25.05 ships 0.2.100, and this tree cannot
even go that low, because `sqlite-wasm-rs` requires `wasm-bindgen ^0.2.104`.

So `just web` does not trust the one on `PATH`. It reads the version out of
`Cargo.lock`, uses the `PATH` binary only if it matches exactly, and otherwise
fetches that version into `target/` and uses it from there. The devshell still
carries nixpkgs' copy for the case where it does match, and nothing breaks when
it does not.

## Nothing renders text without a font, and a browser has none to give

iced does not embed a font by default: `fira-sans` is a feature, not part of
`default`. On a desktop it asks the system and fontconfig answers, so the gap is
invisible. In a browser there is no system to ask, and every glyph silently
fails to draw — widgets, layout and input all work, and the text is simply not
there. Enabling `fira-sans` embeds one and both targets draw.

This was misdiagnosed once as an artifact of headless software rendering, which
is a good reminder that "it only fails in my weird test setup" is a hypothesis,
not an explanation.

## The browser needs its own WebSocket

`transport::ws` is blocking `tungstenite` on a thread, which a browser has
neither of. `transport::web` is the same `Link` shape — `send`, `try_recv`,
`is_alive` — over the DOM's `WebSocket`, with frames arriving in callbacks and
landing in a queue. That queue is exactly what the sans-io client wants anyway,
which is the point of sans-io: the engine did not change to gain a second
transport, and the example picks one with a `cfg`.

Two wrinkles worth keeping: a `WebSocket` refuses sends until it is open, and
the first thing a client says is its `Hello`, so early frames are held and
flushed on open. And a sans-io client has to be pumped by someone — in iced that
is a 50ms subscription, which is also what makes incoming entries appear without
the user touching anything.
