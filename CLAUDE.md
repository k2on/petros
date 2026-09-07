# harken

A self-hosted, local-first music system. **Only Phase 1 exists**: `crates/exo`,
a general-purpose offline-first sync engine that knows nothing about music.
`crates/api`, `crates/server`, `crates/ffi` and the Expo client are later phases
and are not built. Do not start one without being asked.

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
just web          # …and a browser peer, at localhost:8080
```

All four peers share one server. Take any of them offline, mutate on both sides,
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

## Not verified

`flake.nix` has never been evaluated — it was written in an environment with no
nix. `nix develop` and the `WASM_CC` / `WASM_CFLAGS` wiring are the parts most
likely to need adjusting.
