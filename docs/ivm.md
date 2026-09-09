# Incremental views, and a query builder

Notes from reading `rocicorp/mono/packages/zql` — Zero's query layer — and
working out what of it applies here. Nothing is built yet; this is the shape of
the thing and the order to do it in.

## What Zero actually does, and what surprised me

**It is not a Z-set system.** I expected DBSP: deltas with integer
multiplicities, retraction as multiplicity −1, operators as linear functions
over an abelian group. It is not that. A change is

```ts
type SourceChange =
  | [ADD,    row]
  | [REMOVE, row]
  | [EDIT,   row, oldRow]
```

and downstream, on nodes rather than rows, there is a fourth: `CHILD`, meaning
"this row is unchanged but something beneath it moved". No multiplicities, no
algebra. Simpler than the literature, and it fits a system whose sources are
databases rather than streams.

**Operators are both push *and* pull.** This is the load-bearing idea and I had
not appreciated it:

```ts
interface Input {
  fetch(req: FetchRequest): Stream<Node>;   // pull, with constraint + start
}
interface Output {
  push(change: Change, from: InputBase): Stream;  // push
}
```

An operator receives changes *and* can ask upstream for more data, seeking from
a cursor. That is what makes the hard operator tractable.

**`ORDER BY … LIMIT` — which I called "genuinely hard" when we last discussed
IVM — is solved by pulling.** `Take` keeps two things per partition:

```ts
type TakeState = { size: number; bound: Row | undefined };
```

The bound is the last row it accepted. When a delete opens a gap in the top N,
it does not need the whole relation in memory — it calls `fetch` upstream with
`start: bound` and takes the next row. The state is O(1) per partition and the
work is a seek.

I was wrong about the difficulty, and wrong about why: the problem is only hard
if operators can *only* be pushed to.

**Results are trees, not relations.** A node is a row plus named relationships,
and `related()` nests a subquery under a parent. `CHILD` changes propagate
through the nesting. That is what a UI actually wants — an issue with its
comments, arriving as one thing that updates in place — and it is why the
builder has `related` at all.

**Operator state goes through a `Storage` interface**, keyed, rather than living
in operator fields. So state can be bounded, inspected, or persisted.

## Why this suits Petros better than it suits Zero

`Take`'s trick needs a source that is **sorted, seekable and can push down a
constraint**. Zero has to build that: `MemorySource` maintains its own indexes,
and `TableSource` translates fetches into SQL.

We already have one. `Store::query` is checked SQL over SQLite, and a seek is

```sql
WHERE … AND (pos, id) > (?, ?) ORDER BY pos, id LIMIT ?
```

against an index that already exists. The source layer that is a real piece of
work for Zero is close to free here.

## The blocker: we do not know what changed

Zero's sources are *pushed typed row changes*. Ours are not. A mutation writes
through `petros_sql::exec!` — arbitrary checked SQL — so after
`UPDATE song SET pos = pos + 1 WHERE pos >= 3` we know a statement ran and
nothing about which rows moved.

That is the whole problem. Three ways out:

**Typed writes.** Go back to `put`/`delete` over typed rows, which is what the
store looked like before checked SQL. Deltas fall out for free. Costs set
operations — `FavoriteAll` becomes a scan and a write per row — which is exactly
why we left.

**SQLite's hooks.** `sqlite3_update_hook` and `sqlite3_preupdate_hook` report
row-level changes with old and new values, which is precisely `SourceChange`.
Diesel exposes neither, and we have no raw handle — this is the same wall we hit
looking for reactivity before.

**Triggers.** Generate, from `schema.sql`, three triggers per table writing into
a `petros_changes` table:

```sql
CREATE TRIGGER song_ins AFTER INSERT ON song BEGIN
  INSERT INTO petros_changes(tbl, kind, new) VALUES('song', 'add', …);
END;
```

Pure SQL, no C API, no Diesel gap, and it survives `exec!` writing whatever it
likes. It costs an insert per changed row, and the triggers have to be generated
from the schema rather than written by hand — which is a thing we now do.

**Triggers are the one to try.** They keep checked raw SQL, which was a
deliberate decision made twice, and they are the only option that does not
require a C API we cannot reach.

## Where it has got to

Steps 1 and 2 are done, on this branch.

**Typed writes**, so a write says what changed: `Add`, `Remove`, or `Edit`
carrying both versions. `petros_sql::tables!()` generates the row types from
`schema.sql` by asking SQLite what is in it, so the tables are described once,
in DDL.

**A query builder**, and reads go through it rather than SQL. A query is *data*
— a `Plan` with a filter, an order, a limit and a cursor — which is what lets
the same query be one statement today and a maintained pipeline later. The
cursor is there from the start because `Take` seeks with it and retrofitting it
would mean rewriting every source.

`exec!` and `query!` are gone. The domain no longer contains SQL at all: reads
and writes are the same shape, which was the point of doing both at once rather
than leaving a seam between them.

## What I would build, in order

1. ~~Change capture~~ and ~~a source over SQLite~~ — done. Triggers turned out
   to be unnecessary: typed writes report changes directly, which is simpler
   than reading them back out of a table, and the cost is one point lookup per
   write rather than an insert.
2. **`push`.** A source that fans a change to connected operators, and the
   `Input`/`Output` pair. Nothing is connected to anything yet.
3. **The operators that earn their place:** filter, join, take. In that order —
   filter is trivial, join is where the value is, take is where the design is
   tested, and take is the one whose seek the `Plan` already supports.
4. **Relationships.** Zero's results are trees: a row with named children, and
   `related()` nests a subquery under a parent. That is what a UI wants, and it
   is also what replaces the joins a flat query builder cannot express — the
   read model here has two of them.

## What to measure before any of it

The honest case against doing this at all: `library()` is **3.7ms per 10k rows**,
and the cost was Diesel materialising rows, not SQLite executing the query. A
phone shows a screenful. IVM solves re-execution cost, and re-execution may not
be the problem.

So, first:

- **A real query at a real size**, re-run versus maintained. If re-running a
  `LIMIT 50` against an index is under a millisecond at 100k rows, this whole
  document is premature and the reactivity problem — *knowing when to re-run* —
  is the one worth solving instead.
- **The extra read on a write.** `put` looks the old row up to report an edit.
  Measure it at one row and at a thousand; if it is material, an add-only path
  for rows known to be new is the obvious relief.

Both are an afternoon, and they decide whether the rest is worth a month.
