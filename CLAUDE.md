# petros

A general-purpose, offline-first sync engine that knows nothing about your
domain. `crates/todo` is the worked example it is exercised with — a to-do list,
compiled both natively and to wasm, which is what lets `conformance.rs` prove
those two builds agree.

## What Petros is

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

- `apply` must be a pure function of `(transaction state, arguments)`. No clock,
  no RNG, no network, no filesystem. Explicit `ORDER BY` on every query —
  SQLite's natural order is not a contract. No floats in control flow.
- All non-determinism goes through `AutoCtx`, in `fill_auto`, exactly once, at
  the originating client. It is then frozen in the log forever.
- The log is permanent. Never rename or remove a mutation variant, never change
  a field's type; add fields with `#[serde(default)]`. `tests/wire.rs` pins this
  against a checked-in byte fixture — if it fails, the change would have broken
  every existing installation.
- **Petros owns every transaction boundary.** Never call Diesel's
  `Connection::transaction` on a client's connection: the optimistic savepoint
  outlives any single call, so boundaries are raw SQL through `batch_execute`.
- Petros owns tables prefixed `petros_`. The app owns everything else.
- `petros::client` and `petros::server` are sans-io: no sockets, no async, no
  runtime. That is what makes the deterministic simulation tests possible. The
  transport module is the only place networking lives.

## Layout

```
crates/petros/               the engine
  client.rs                  the savepoint rebase — the least obvious code here
  server.rs                  assigns sequence numbers, dedupes, fans out
  store.rs, schema.rs        the three petros_ tables, as Diesel models
  transport/{ws,web}.rs      thin, replaceable; ws = native, web = browser
crates/petros-schema/        the app contract: `mutations!`, the schema it
                             declares, and the `Host` a mutation sees. No deps
crates/petros-wasm-guest/    `export!` — an app's wasm crate is one line
crates/petros-wasm-host/     wasmi, for a peer that replaces `apply` at runtime.
                             Knows nothing about any domain
crates/petros-codegen/       reads a module's schema section, writes TypeScript
crates/petros-axum/          one handler; the app keeps its routes and its auth
crates/petros-testkit/       the seeded in-process network, generic over an app
crates/todo/                 the worked example: domain.rs, storage.rs, examples
crates/todo-wasm/            the same domain as wasm — one `export!`
docs/decisions.md            why everything is the way it is — read this first
```

## Running it

```
just              # fmt, lint, test
just test         # must stay under 30s
just mutators     # rebuild the wasm module and its TypeScript types
just offline      # a client with no server in existence
just serve        # one server…
just peer alice   # …a TUI peer…
just iced bob     # …a desktop GUI peer…
just web          # …a browser peer, at localhost:8080
```

Take any of them offline, mutate on both sides, come back — that is the rebase,
visible.

## Using it from an app

`../harken` is one. An app declares its domain with `petros_schema::mutations!`,
gets a wasm build from `petros_wasm_guest::export!`, serves it with
`petros-axum`, and tests it against a simulated fleet with `petros-testkit`.
Depend on these by path while they are unpublished:

```toml
petros = { path = "../petros/crates/petros" }
```

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
- **`default-features = false` has to be said in the workspace entry too.** A
  member cannot subtract what `[workspace.dependencies]` asked for, and nothing
  warns. It put Diesel and SQLite in the wasm graph for weeks.
- A build that compiles is not evidence anything works. `SystemTime::now()`
  panics on wasm and only surfaced when a mutation actually ran in a browser.
- **A test can pass for the wrong reason, and three here did.** Each time the
  assertion was satisfied by a path other than the one under test. Falsify every
  new test by breaking the thing it claims to check; it costs a minute and has
  not once been wasted.
