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

Steps 1, 2 and most of 3 are done, on this branch.

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

**Relationships**, generated from the foreign keys — `PRAGMA foreign_key_list`
reports what `REFERENCES` said — so a result is a tree and neither `LEFT JOIN`
nor `INNER JOIN` appears anywhere. Which end you read from is the join.

**The operators**: `Source`, `Filter`, `Join`, `Take`, and a `View` that holds
the answer. `petros-ivm`. A view is hydrated once and then handed what each
mutation changed.

**`Child`**, Zero's fourth kind of change, without which a tree cannot be
maintained: hearting a song does not add, remove or edit any *song*, and yet a
library view has to move. The alternative — remove the row and add it back — is
a flicker and a lost scroll position.

**Nesting**, to any depth. `Child` carries the relationship's name and another
`Delta`, so a change two levels down arrives as a child of a child. A
`Pipeline` composes:

```rust
Pipeline::of(songs)
    .related(Song::note, Pipeline::of(notes)
        .related(Note::author, Pipeline::of(authors)))
```

The child side is an operator rather than a plan, which is what makes that
work — and it is why `Fetch` carries a constraint. The `IN` covering a page of
parents has to travel *through* the child's own filter and limit, or every
level costs a statement per parent.

**A client that uses it.** `Client::take_changes()`, and the desktop client
holds a `View` instead of re-reading the list on every tap.

## One thing I got wrong about the shape

Zero wires operators with output pointers: an operator holds the next one and
calls `push` on it. Copying that into Rust means `Rc<RefCell<…>>` on every node,
because the graph is then reachable from both ends — for something that is, in
practice, a chain.

So `push` here **returns** what a change became instead of forwarding it, and
each operator owns its input. A change enters at the top, is passed down to the
source, which decides whether it is even about the right table; the answer comes
back up, reshaped on the way. Same semantics, ordinary ownership, and a pipeline
is a value that can be moved and dropped.

## The bug that only a random session found

By the time a change reaches an operator, the store is **already at the new
state**. So a pull can see rows the push has not reported yet.

An edit is split into a remove and an add, because a limit cares about position
above all. Remove-first looks obviously right and is wrong: the refill after the
remove seeks past the bound, finds the edited row sitting there in its new
place, pulls it in — and then the add puts it in a second time. Add-first makes
the new row part of the window before anything seeks past it.

Six hand-written tests all passed with this bug present. A two-thousand-step
random session against a re-run of the same query found it at step 26.

## Two things a test that checks the answer cannot see

Twice now, deliberately breaking the code failed nothing:

- an append past a full window was admitted and then evicted — same answer,
  wasted work. Fixed by having `push` return how many changes reached the view.
- the constraint never reaching the child pipeline, and the limit never reaching
  the statement — same answer, whole tables read. Fixed by a `Store` wrapper in
  the tests that counts pulls and rows.

The second one found a real bug rather than only proving a claim: `Take` was
fetching its input entire and truncating, so hydrating a view of the top twenty
of a hundred thousand rows read a hundred thousand. It now asks for twenty, and
a refill asks for one.

The lesson is the same both times. An incremental view is a *cost* argument, and
a test that only compares rows passes against a pipeline doing all the work it
was built to avoid.

## What the return-value shape bought a second time

The join needs to see changes to the *child* table, which the parent's source
knows nothing about. With output pointers that means a second wire into the
graph — the child source connected to the join as well. Here the raw change is
in hand at every level on the way down, so the join simply looks at it. No
second edge, and the pipeline is still a chain.

`Child` was not recursive at first, on the argument that a type which can only
ever be one deep would be a lie about what the code does. That was right about
the code and wrong about where it was going: a level later it *is* recursive,
carries the relationship's name, and the child side of a join is an operator
rather than a plan — so a note under a song under an album is a `Child` whose
inner change is a `Child`.

The argument still holds; it was the code that moved. Depth is now a property of
the query rather than of the type, which is the version worth keeping.

## What I got wrong about foreign keys

The first version of "a new parent arrives with its children" wrote the child
first and then the parent. It failed, and the reason was not the join: the
foreign key refused the orphan, so the child was never written and the view was
right to show nothing. `put_row` swallowed the error — it does correctly report
*no change* when a write fails, so the view stayed consistent with the database,
but a mutation that violates a constraint gets no signal at all.

That is worth fixing separately and is not the join's business: `Store::put_row`
returns `()`, so reporting would change the trait and the guest ABI with it.

The test now brings the parent into view by editing it across the filter, which
is a real case and does not need an orphan to exist.

## What I would build, in order

1. ~~Change capture~~ and ~~a source over SQLite~~ — done. Triggers turned out
   to be unnecessary: typed writes report changes directly, which is simpler
   than reading them back out of a table, and the cost is one point lookup per
   write rather than an insert.
2. ~~`push`~~ — done, as a return value rather than an output pointer.
3. ~~The operators:~~ filter, take and join, all done. Filter was trivial and
   take was where the design was tested, exactly as expected.
4. ~~Relationships~~ — `select_with` reads one, `View::related` maintains one.
5. ~~Wire a view into a client~~ — `Client::take_changes()` and the desktop
   client. The rebase turned out to be the interesting part: it rolls the
   optimistic view back, and a rollback reports nothing, so no list of changes
   describes it. `Changes::Rebuilt` says so instead of lying, and costs one
   query in the case where the server speaks while something of ours is
   pending.
6. ~~A patch stream~~ — `View::push` returns what it did to its own list, as
   positions: `Insert { at, node }`, `Remove { at }`, `Update { at, node }`.
   Maintaining the query and then decoding every row again is still O(n), and
   that decode turned out to be most of what was left on the client path. The
   node travels with the patch rather than being looked up afterwards, because
   by then the view has applied the rest of them.
7. ~~A typed accessor for a nested view~~ — and it turned out not to be the
   type-level work this list expected. `With` already nests structurally:
   `With<Song, With<Note, Author>>` *is* the shape. What was missing was a way
   to decode at that depth, which is one recursive trait, and the relationship
   each level reads is named by the table at that level — so `Song::note` and
   `With<Note, _>` agree without anyone writing a string.

   The type is a projection: ask for less than the view holds and the rest is
   not decoded. There is no blanket impl over `Table`, because it would overlap
   the nesting one; `tables!` emits the leaf impl per table instead.
8. ~~Per-partition take state~~ — done, and it was not an optimisation. A limit
   on a child relationship was silently *wrong*: the first parent took the whole
   limit and every other parent showed nothing. `Take` keys its window by the
   column the relationship joins on now, as Zero does, and a refill seeks within
   its own partition. `select_with` had the same bug from the other side — it
   dropped a child limit entirely — and now applies it per parent, so the run
   query and the maintained one agree.
9. ~~Aggregates~~ — `Tally`, a maintained `COUNT`, holding a number rather than
   the rows. A `View` can already count by holding every matching row, and
   `MAX` is already a maintained `ORDER BY … DESC LIMIT 1`; what was missing was
   the O(1)-memory version, which is the difference between a count being free
   and a count costing the answer it counts.

   The push side is arithmetic, because `Filter` has already turned an edit that
   crosses the predicate into an add or a remove. That is worth noticing: the
   operator is three lines instead of a case per kind of change, and it is the
   pipeline that earned that, not the aggregate.

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
## What it is actually worth — measured

`cargo test -p petros-ivm --release --test cost -- --ignored --nocapture`.
One write, then the top 20 rows, on an in-memory database:

```
    ordering column indexed:
        rows        re-run    maintained    ratio
         100     0.0113 ms     0.0018 ms     6.3x
        1000     0.0117 ms     0.0019 ms     6.1x
       10000     0.0079 ms     0.0013 ms     6.3x
      100000     0.0079 ms     0.0013 ms     6.3x

    ordering column not indexed:
         100     0.0140 ms     0.0013 ms    11.2x
        1000     0.0447 ms     0.0013 ms    34.6x
       10000     0.3521 ms     0.0013 ms   264.2x
      100000     3.4419 ms     0.0013 ms  2664.0x
```

The first table is the one that should temper the enthusiasm. **Given the right
index, SQLite already answers this in O(limit) and a re-run is flat too** — the
win is a constant factor of six, and six times almost nothing is almost nothing.
Anyone arguing for incremental maintenance on asymptotics alone has not checked
whether the planner was already doing it.

The second table is the case for it. Without an index the re-run is O(n log n) —
3.4ms at a hundred thousand rows, which is a dropped frame — and the maintained
answer does not move. That is the real claim, and it is not "faster": it is that
the cost stops depending on the size of the table *and* on whether the planner
found a way. A view that is right by construction rather than by luck of
indexing is a different kind of thing to reason about.

Maintained is 0.0013 ms in every row of both tables. That flatness is the
property, not the ratio.

The tree is the case that grows, because a re-run of it is two statements and
the second one's `IN` gets longer as the relationship fills:

```
  one heart, then the library of N songs with their favourites:
       songs        re-run    maintained    ratio
         100     0.0333 ms     0.0054 ms     6.1x
        1000     0.0726 ms     0.0053 ms    13.6x
       10000     0.3575 ms     0.0039 ms    91.3x
```

**The extra read on a write**, which is what a write pays to be able to report
what it changed:

```
  one write into a table of N rows:
        rows        insert        update
         100     0.0082 ms     0.0103 ms
       10000     0.0035 ms     0.0043 ms
      100000     0.0035 ms     0.0045 ms
```

About a microsecond, flat, and it does not grow with the table — an indexed
point lookup on the primary key. Nothing to reclaim here; the add-only path this
document speculated about would not be worth the second code path.

Both are an afternoon, and they decide whether the rest is worth a month.
