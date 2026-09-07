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

`AddTracks { playlist, tracks }`, never `ItemInserted { pos: "a5" }`. `apply` may
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
