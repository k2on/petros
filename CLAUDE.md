# harken

A self-hosted, local-first music system. **Phase 1 plus its clients exist**:
`crates/exo`, a general-purpose offline-first sync engine that knows nothing
about music; `crates/todo`, the demo domain it is exercised with; `crates/ffi`,
that domain exported over UniFFI; and four peers — two TUIs, an iced window
(desktop and browser) and an Expo app. `crates/api` and `crates/server` are
later phases and are not built. Do not start one without being asked.

## What Exo is

A server owns an append-only, totally ordered log of mutations. Mutations are
*intents*, not facts — `AddToList { list, items }`, never
`ItemInserted { pos: "a5" }` — and `apply` may read the database to decide what
to write. A client's view is always:

```
replay(confirmed) then replay(pending)
```

Confirmed state only moves forward, because the log is never reordered. The only
thing ever undone is the client's own pending mutations, replayed on top after
confirmed entries land. That is the rebase, and it is the whole concurrency
story: no CRDTs, no vector clocks, no merge functions.

## Invariants. Breaking one makes replicas diverge silently

- `Mutation::apply` must be a pure function of `(transaction state, arguments)`.
  No clock, no RNG, no network, no filesystem. Explicit `ORDER BY` on every
  query — SQLite's natural order is not a contract. No floats in control flow.
- All non-determinism goes through `AutoCtx`, in `fill_auto`, exactly once, at
  the originating client. It is then frozen in the log forever.
- The log is permanent. Never rename or remove a mutation variant, never change
  a field's type; add fields with `#[serde(default)]`. `tests/wire.rs` pins this
  against a checked-in byte fixture — if it fails, the change would have broken
  every existing installation.
- **Exo owns every transaction boundary.** Never call Diesel's
  `Connection::transaction` on a client's connection: the optimistic savepoint
  outlives any single call, so boundaries are raw SQL through `batch_execute`.
- Exo owns tables prefixed `exo_`. The app owns everything else.
- `exo::client` and `exo::server` are sans-io: no sockets, no async, no runtime.
  This is what makes the deterministic simulation tests possible. The transport
  module is the only place networking lives.

## Layout

```
crates/exo/src/          the engine
  client.rs              the savepoint rebase — the least obvious code here
  server.rs              assigns sequence numbers, dedupes, fans out
  store.rs, schema.rs    the three exo_ tables, as Diesel models
  transport/{ws,web}.rs  thin, replaceable; ws = native, web = browser
crates/exo/tests/        19 tests; common/sim.rs is a seeded in-process network
crates/exo/examples/     offline (TUI), multiplayer (TUI), iced (GUI, native+web)
crates/todo/             the demo domain — the ONLY apply, shared by every peer
crates/ffi/              crates/todo over UniFFI, for the Expo client
clients/expo/            the Expo app; src/ is UI and a socket, nothing else
  modules/exo-todo/      the turbo module — generated, gitignored, not authored
docs/decisions.md        why everything is the way it is — read this first
```

## Running it

```
just              # fmt, lint, test
just test         # 19 tests + doctests, must stay under 30s
just offline      # a client with no server in existence
just serve        # one server…
just peer alice   # …a TUI peer…
just iced bob     # …a desktop GUI peer…
just web          # …a browser peer, at localhost:8080
just ffi-bindings # regenerate the Expo client's TS from crates/ffi, and typecheck
just expo-android # …and a phone. Needs `nix develop .#android`; see below.
```

All five peers share one server. Take any of them offline, mutate on both sides,
come back — that is the rebase, visible.

## Traps that have each already cost a debugging round

- **`CC` in the environment hijacks the wasm build.** `cc-rs` falls back to
  plain `CC`, and gcc cannot target wasm; the failure appears as pages of errors
  inside glibc headers. The web recipe sets `CC_wasm32_unknown_unknown`.
- **nix's wrapped clang** injects the system's glibc includes and fails the same
  way; the devshell passes an unwrapped one plus `-resource-dir`, because an
  unwrapped clang cannot find its own `stddef.h`.
- **The wasm-bindgen CLI must match `Cargo.lock` exactly.** The recipe fetches
  the right version into `target/` rather than trusting `PATH`.
- **iced embeds no font by default.** Without the `fira-sans` feature a browser
  draws no glyphs at all, while widgets and input work — so it looks like a
  renderer bug and is not one.
- A build that compiles is not evidence anything works. `SystemTime::now()`
  panics on wasm and only surfaced when a mutation actually ran in a browser.

## The Expo client, in one paragraph

Never write domain logic in TypeScript. `crates/todo` holds the mutations and
the queries; `crates/ffi` exports them with `#[uniffi::export]` (there is no UDL
file — the Rust is the interface definition); `uniffi-bindgen-react-native`
generates `clients/expo/modules/exo-todo/`, which is gitignored precisely so it
cannot be hand-edited. `just ffi-bindings` regenerates and runs `tsc`, which
turns a stale call site into a compile error. It reads the metadata out of a
*host* build, so it needs no NDK and no Xcode and takes seconds.

## Traps in the client toolchain

- **The NDK is x86_64-only.** Google publishes no aarch64-linux host toolchain,
  so on an ARM Linux box the NDK's `clang` cannot execute — binfmt hands it to
  qemu, which has no x86-64 loader. Build Android on x86_64 or in CI. iOS needs
  Xcode, so it is a macOS runner. `.github/workflows/expo.yml` does both.
- **nix does not supply the Android SDK**, on purpose: gradle installs missing
  SDK components into the SDK directory, and the store is read-only. Bring your
  own (Android Studio, `sdkmanager`, or a runner image) and export
  `ANDROID_HOME`; `nix develop .#android` adds `cargo-ndk` and a JDK to it.
- **Expo Go cannot load this app.** It calls into Rust, so it needs a
  development build. `ios/` and `android/` are generated by `expo prebuild` and
  are not in the tree.
- **`uniffi` is pinned to `=0.31`** because `uniffi-bindgen-react-native` pins
  it. The generator and the runtime must agree on the metadata format.
- **`nix develop` sets `TMPDIR`.** The demo databases go to
  `std::env::temp_dir()`, so a server started inside `nix develop --command`
  does not share a database with one started under direnv. Fine in normal use;
  confusing for five minutes if you hit it.

## Not verified

Android and iOS have never been built end to end from this repository — no
machine here can run either toolchain (see above), so `.github/workflows/expo.yml`
is written but has not had a green run. What *is* verified is everything up to
that line: the Rust builds, the bindings generate, and the app typechecks
against them.
