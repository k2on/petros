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

## A tap is one commit, because it used to be two

`mutate` probes the intent against the optimistic view and then records it
durably. Those were two transactions, and it was not obvious: `RELEASE` on the
outermost savepoint *is* a commit, so the probe committed on its own when no
optimistic savepoint was held, `discard_optimistic` committed when one was, and
`put_pending` then committed again as its own implicit transaction.

At `synchronous = FULL` — SQLite's default, which Exo never overrode — each of
those fsyncs the WAL, and an fsync is the largest number in the system. Traced
with `strace`: two `fsync` calls per tap, against 4.0ms for a single fsync of a
4KiB overwrite on this machine, which is the whole 8.0ms a tap measured.

So the probe and the insert now share one transaction. `probe` nests inside the
pending savepoint when one is held and opens its own when not; rolling back to
`pending` empties the transaction of everything except the insert about to
happen, and one `COMMIT` closes it. One fsync, verified by the same trace, and
a tap goes from 8.0ms to 4.1ms with no change in what is durable — the probe's
commit was carrying nothing.

What remains is genuinely one fsync per durable local write, which is the floor
for surviving power loss. Going below it means changing the *policy* rather
than the mechanism: `synchronous = NORMAL` risks losing commits since the last
checkpoint to a power cut or kernel panic, but never to an application crash
and never to corruption — and while a peer is connected and being acked, the
server's log is already the durable copy, so a lost local intent is refetched
by the next `Hello`.

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

## The demo domain is a crate, because it now has more than one caller

`examples/shared/todo.rs` was a file three examples included with `#[path]`.
That worked while every caller was an example in the same crate. It stopped
working the moment the Expo client needed the same mutations, because an
example cannot be depended on. So the to-do domain is `crates/todo`: the same
code, in a place `crates/ffi` can reach. `crates/exo` takes it as a
dev-dependency, which is a cycle — `todo` depends on `exo` — and one cargo
resolves without complaint.

The move is the whole point rather than tidying. There is exactly one `apply`
in this repository, and the terminal peers, the iced window, the server and the
phone all run it.

## The Expo client calls Rust; it does not reimplement it

The obvious way to put a to-do list on a phone is to write one in TypeScript and
teach it the wire format. It was tried here first, and it was wrong: two
`apply`s in two languages is two definitions of what a mutation *means*, and the
first time they disagree — about `MAX(pos) + 1`, about whether a trimmed empty
string is refused, about what happens to an edit whose row a confirmed entry
removed — the replicas diverge silently and neither side is obviously at fault.
Every invariant at the top of `CLAUDE.md` is a property of one implementation,
not of two that intend to match.

So `crates/ffi` exports the client over UniFFI and
`uniffi-bindgen-react-native` generates the TypeScript. The generated files are
gitignored rather than committed, so nobody can hand-edit them and wonder why
the next build reverts it, and `just ffi-bindings` regenerates and then runs
`tsc` — which turns "the app still calls the API the Rust used to have" into a
compile error instead of a crash on a device.

There is no UDL file. `#[uniffi::export]` on the Rust *is* the interface
definition; the generator reads the metadata back out of the compiled library.
One place to change, and it is the place that already had to be right.

## The socket stayed in JavaScript

`crates/ffi` exposes the sans-io client and nothing else: `take_outgoing()`
hands back encoded frames, `recv()` takes them, and the caller owns the
transport. React Native then does what a browser does in `transport/web.rs`,
for the same reason it did there — the platform already has a WebSocket.

The alternative was to run `exo::transport::ws` on a thread inside the FFI and
hand JavaScript nothing but a `connect(url)`. It would have been thinner at the
call site and worse everywhere else: a `tungstenite` and a TLS stack in the
mobile binary, a uniffi callback interface to push changes back up, a thread to
manage across backgrounding, and `wss://` reimplemented next to the platform
trust store that already does it. Frames here are tens of bytes a few times a
second; React Native base64s binary frames across its bridge, and at this volume
that is not where the time goes. If it ever is — a media sync, say — the
transport is a page of code and moving it is a local change, which is what
sans-io bought in the first place.

## Native projects are generated, not committed

The Expo app uses Continuous Native Generation: there is no `ios/` or
`android/` in the tree, `expo prebuild` makes them, and the turbo module is a
workspace package React Native autolinks. This is why the client cannot run in
Expo Go — Expo Go ships a fixed set of native modules and ours is not one of
them — and a development build is not a limitation to work around but the
consequence of calling into Rust at all.

## Where each platform can be built, and where it cannot

Google publishes the NDK as prebuilt binaries for `linux-x86_64` and nothing
else. On an ARM Linux machine `nix develop .#android` resolves perfectly and
then its `clang` will not execute — binfmt routes it to qemu, which has no
x86-64 loader to give it. Apple's linker only exists inside Xcode. So neither
mobile build runs on an ARM Linux workstation, and both run in CI instead:
`.github/workflows/expo.yml` builds Android on an x86_64 runner and iOS on a
macOS one, with nix supplying the identical toolchain in both. Nothing in the
flake changes; the machine does.

`just ffi-bindings` deliberately needs neither. It reads the UniFFI metadata out
of a *host* build of the crate, so the loop that actually matters day to day —
change the Rust, regenerate, see whether the app still compiles — is a couple of
seconds on any machine.

## The Android SDK comes from the machine, not from nix

`nix develop .#android` was composed with `androidenv` at first, so that the SDK
and NDK were pinned like everything else. It failed in CI, and the error was the
interesting part:

```
Failed to install the following SDK components:
    ndk;27.1.12297006 NDK (Side by side) 27.1.12297006
The SDK directory is not writable (/nix/store/…-androidsdk/libexec/android-sdk)
```

The Android Gradle Plugin does not merely *read* the SDK directory; it resolves
the versions a project asks for against it and installs whatever is missing. A
nix store path is read-only by construction, so any version the flake did not
happen to pin is a hard failure rather than a download — and the flake had
pinned build-tools 35 and NDK 28 against an Expo that wanted 36 and 27, in a
layout (`ndk-bundle` rather than `ndk/<version>`) the plugin does not recognise
as installed.

Matching those numbers exactly would have postponed the fight rather than won
it: Expo moves its `compileSdk` and `ndkVersion` on its own schedule and nixpkgs
moves on another, and the next bump breaks the build again in the same way.

So the SDK now comes from where it comes from for every other React Native
project — Android Studio locally, the runner image in CI — and nix pins the part
that is actually ours: the Rust toolchain, its Android targets, `cargo-ndk` and
bun. That is the half that has to match `Cargo.lock`; the Android SDK never did.
The shell also stopped being several gigabytes, and `flake.nix` stopped needing
an unfree opt-in to exist.

## The Rust targets stay in `rust-toolchain.toml`

Not in the Android shell. That file is the one place a target is named, read by
`rust-overlay` inside the devshell and by rustup outside it, and splitting it
would put `wasm32-unknown-unknown` and `aarch64-linux-android` in different
places for no reason. The Android shell adds `cargo-ndk`, not a second
toolchain.

## `apply` is a linked symbol natively and a wasm module on the phone

The Expo client's feedback loop was measured before it was redesigned, and the
measurement moved the problem. Editing a mutation and having TypeScript know
about it took 3.2s — cargo, `ubrn generate`, `tsc`. Editing a mutation and
having the *phone* run it took about four minutes, because the domain was
compiled into the native binary and a native binary has to be rebuilt for two
ABIs, relinked and reinstalled. UniFFI collapsed both loops into the slower one.

A wasm module fixes that, and for a while every peer ran one — server included,
which is what makes a rejection a verdict rather than one machine's opinion.
Measuring the result showed what it cost the peers that never had the problem:

```
one `Add`, release build (ms)
    natively linked                          0.027
    via wasmi, thread + fresh instance       0.39
```

The slope is the finding, not the intercept. A tap replays every pending
mutation, so the whole wasm overhead is paid once per pending entry: at a depth
of 50, which an offline peer reaches easily, that is ~20ms of interpreter
against ~1.3ms of SQLite. A server rebuild was never the thing that hurt.

Two warnings about these numbers, both learned the hard way. They were first
taken at the test profile, where our own crates build at `opt-level = 1`, and
that inflates the wasm path by about 4.7x (0.62ms vs 2.89ms for the same
`apply`) while barely moving the native path, which is dominated by SQLite
rather than by our Rust. Comparing the two at that profile therefore overstates
the gap several-fold. `just latency` now passes `--release` for that reason.

So the module is scoped to the caller that needs it. `crates/todo` holds the
domain generic over a three-method `Host` trait; `crates/todo/storage.rs`
implements that trait over Diesel for everything that links Rust, and
`crates/todo-wasm` implements it over three imported functions for the phone.
One `apply`, two hosts, and the boundary is a trait rather than a copy.

The phone's loop is unchanged:

```
save a .rs   ->  0.48s   cargo, wasm32, `mutators` profile
             ->  ~0.0s   rewrite mutators.gen.ts (a base64 string)
             ->  ~0.2s   Metro fast refresh pushes 150KB
             ->  1.4ms   wasmi compiles and verifies it
```

Metro carries the module because Metro's fast refresh moves *modules*, not
assets: the wasm becomes a base64 string in a generated `.ts`, and the channel
already pushing component edits pushes `apply`. No asset pipeline, no fetch, no
dev server of our own.

## The profile was chosen by measuring, not by reflex

`codegen-units = 1` is the usual reflex for a small wasm artifact, and here it
is the wrong call: it buys 17KB nobody notices over a LAN and costs 0.38s on a
loop measured in single seconds. `opt-level = "z"` with `codegen-units = 16`
was both the smallest and the fastest of the combinations tried; the table is
in `Cargo.toml` next to the profile.

## The module cannot break determinism; the linked build only promises not to

The invariant at the top of `CLAUDE.md` asks that `apply` never read a clock,
never call a random number generator, never touch the network or the disk. In
the module it *cannot*: the host imports `query_int`, `query_exists` and `exec`,
and nothing else exists to misuse. `fill_auto` runs with no database at all, so
it cannot even read state.

The linked build has the whole standard library in reach and keeps the rule by
convention. That asymmetry is not a reason to link everything through wasm — it
is a reason the `Host` trait is the only way `domain.rs` touches the world, so
the surface where a clock could sneak in is three methods wide either way, and
the conformance test below runs the constrained build against the unconstrained
one.

What the module costs is Diesel. It has no SQLite, only a channel to the host's,
so its SQL is written out and its values are inlined as literals rather than
bound — which gives up `check_for_backend`'s compile-time check that a model
still matches its table. Since `domain.rs` is shared, the linked build gives
that up too; the tests carry it instead.


## Two hosts is two chances to diverge, so a test runs both

The whole argument against a TypeScript reimplementation is that two `apply`s
are two definitions of what a mutation means. Compiling one `apply` twice is a
much weaker version of the same risk — the source is shared, but the toolchains,
the integer widths and the SQL round-trip are not — and a drift would be frozen
into the log by whichever build authored the entry.

`crates/mutators/tests/conformance.rs` runs every declared verb through both
builds against real SQLite and compares the rows and the refusals.

The first version of it passed while being vacuous. It filled each payload once,
natively, then applied that filled payload both ways — so it covered `apply` and
never touched `fill_auto`, which is the half where the non-determinism actually
lives. Changing the wasm build's generated text and watching the test stay green
is what surfaced it. `fill_auto_agrees_between_the_two_builds` uses
`Mutators::fill_auto_with`, which takes the seed as input, so the two builds are
given the same entropy and compared on what they do with it. Three deliberate
one-sided changes — a different `pos`, a different generated text, a refusal
that stops refusing — are each caught.

## A wasm mutation gets its own stack, and the stack bounds the mutation

A host import runs *inside* wasmi's execution loop, so its frames sit on top of
the interpreter's, and this one then calls Diesel — many layers of deeply nested
generics. Together they overflowed a default 2MB thread stack, which is how this
was found; iOS gives React Native's JS thread about half of that. So each
mutation runs on a scoped thread of its own rather than borrowing whichever
stack called in.

Writing a mutation that inserts five rows turned up the part that matters:
those host frames *accumulate* for as long as the guest function runs, instead
of unwinding between calls. So the stack a mutation needs scales with how many
times it calls into the host. `Add` makes three calls; `AddFive` makes eleven
and overflowed 8MB in a test build while passing in release, because the frame
size is dominated by Diesel's generics and release inlines most of them away.

A limit only a debug build trips is one the next person will trip on the machine
where it is hardest to diagnose, so the stack is 64MB — reserved address space,
of which only touched pages are resident. `exec` also stopped going through
`sql_query(..).execute(..)` in favour of `batch_execute`, which is a fraction of
the frame and wanted no row count anyway.

None of this applies to a linked peer any more, which is most of why the linked
path is faster. The thread itself is *not* where the time goes, despite an
earlier note here saying so: a scoped spawn-and-join with a 64MB stack and an
empty body measures 0.06ms, at either profile. Of the 0.39ms a release-build
wasm `Add` costs, roughly 0.06ms is the thread, 0.13ms is `Store::new` plus
instantiation, and the remaining ~0.24ms is interpreted execution and the SQL
under it — against 0.027ms for the same mutation linked.

So a persistent instance is worth about 0.15ms of 0.39ms, not the bulk of it,
and it needs an ABI change first: `exo_alloc` deliberately leaks
(`mem::forget`) and so does `packed` (`Vec::leak`), which is harmless only
because each call gets a fresh `Store`. Measured, sixty calls into one reused
instance grow guest memory to 1.1MiB and it never comes back. Reuse needs
either `exo_free` or a bump arena the entry points reset, gated on
`exo_abi_version`.

## `default-features = false` needs saying twice

`crates/todo-wasm` declared `todo = { default-features = false }` and got the
default features anyway, silently, because the *workspace* dependency did not
say it too: cargo unions the workspace entry's features with the member's, and
the member's `default-features = false` cannot subtract what the workspace
entry asked for. Diesel and `libsqlite3-sys` were in a `wasm32-unknown-unknown`
graph that has no C compiler for them, held up only by the `storage` feature
never being reachable from the module's own code.

It is worth knowing because nothing reports it. The build succeeded, the module
worked, and the only visible symptom was a dependency graph with nine entries
in it that could not possibly be linked. The workspace entry now carries the
flag and each consumer opts in with `features = ["storage"]`.

## The mutation type is the payload, not a Rust enum

`Payload` is the CBOR the log stores, held as a `ciborium::value::Value` and
handed to the module verbatim. No Rust enum mirrors it, and that is the point:
a peer that has never heard of a variant still carries it through the log
intact, and applies it as soon as it has a module that knows what it means. The
old arrangement could not — serde failed to decode the unknown variant and the
client could not apply that batch at all, so an old peer was stuck until someone
shipped it a new binary through an app store.

## Android is built by EAS

`.eas/build/rust.yml` installs Rust from `rust-toolchain.toml` — still the one
place a version or a target is named — then builds the module, the engine and
the turbo module before Expo's own prebuild and gradle steps. This should run
rarely by design: changing a mutation does not need a build, only changing the
engine does.
