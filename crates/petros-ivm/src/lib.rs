//! Incrementally maintained views.
//!
//! A [`View`] is a query that stays right without being run again. It is
//! hydrated once, and after that every mutation hands it the rows that changed
//! and it works out what that does to its own answer. The whole point is that
//! the work is proportional to the *change* rather than to the table: adding
//! one song to a library of ten thousand should not cost ten thousand rows.
//!
//! # Operators are pushed to and pulled from
//!
//! This is the load-bearing idea, taken from Zero's `zql`. An operator is not
//! only a function from a change to a change — it can also ask its input for
//! more rows, seeking from a cursor. That is what makes `ORDER BY … LIMIT`
//! tractable: when a delete opens a gap in the top twenty, [`Take`] does not
//! need the other nine thousand nine hundred and eighty rows in memory, it asks
//! for the one that comes next.
//!
//! # A change travels by being returned, not by being sent
//!
//! Zero wires operators to each other with output pointers: an operator is
//! handed the next one and calls `push` on it. That shape needs every node to
//! be reachable from both directions, which in Rust means reference counting
//! and interior mutability for a graph that is, in practice, a chain.
//!
//! So `push` here returns what the change *became* instead of forwarding it,
//! and each operator owns its input:
//!
//! ```text
//!     View  ->  Take  ->  Join  ->  Filter  ->  Source
//!     ^^^^ owns  ^^^^ owns  ^^^^ owns  ^^^^^^ owns ^^^^^^
//! ```
//!
//! A change enters at the root and is passed down to the source, which decides
//! whether it is even about the right table; the answer travels back up,
//! reshaped by each operator on the way. Same semantics, ordinary ownership,
//! and the pipeline is a value that can be moved and dropped.
//!
//! It also turns out to be what [`Join`] needs. A change to the *child* table
//! is nothing to the parent's source and everything to the join — and because
//! the raw change is in hand at every level on the way down, the join can look
//! at it directly rather than needing a second wire into the graph.

use petros_schema::{Change, Dir, Node, Op, Plan, Relation, Store, Table, TableDef, Value, With};

/// The tree a pipeline carries, and how to decode one.
///
/// They live in `petros-schema` because an app names them — `tables!` generates
/// a [`FromNode`] impl per table — and an app should not have to depend on the
/// operators in order to decode a row.
pub use petros_schema::{FromNode, Tree};

/// A row, as it travels through a pipeline: positional, in table order.
///
/// Untyped on purpose. Decoding is the [`View`]'s job and happens once, at the
/// top, rather than at every operator.
pub type Row = Vec<Value>;

/// A change, as it travels up a pipeline.
///
/// The first three are what a write reports. `Child` is Zero's fourth, and a
/// tree cannot do without it: hearting a song does not add, remove or edit any
/// song, but the library view has to move. Without it the only way to say
/// "something beneath this row changed" is to remove the row and add it back,
/// which is a screen flicker and a lost scroll position.
///
/// `Child` is recursive, and carries the relationship's name, because trees
/// nest: a note edited under a song under an album is a `Child` of the album
/// whose inner change is a `Child` of the song. Depth is a property of the
/// query rather than of this type.
#[derive(Debug, Clone, PartialEq)]
pub enum Delta {
    Add(Tree),
    /// The row that identifies the node. Its children go with it.
    Remove(Row),
    Edit {
        old: Row,
        new: Tree,
    },
    Child {
        parent: Row,
        /// Which relationship of the parent moved.
        name: &'static str,
        change: Box<Delta>,
    },
}

impl Delta {
    /// The row this change is about, for the operators that only care about
    /// where a node sorts or whether it passes a filter.
    fn row(&self) -> &Row {
        match self {
            Delta::Add(tree) | Delta::Edit { new: tree, .. } => &tree.row,
            Delta::Remove(row) | Delta::Child { parent: row, .. } => row,
        }
    }
}

/// What a pull asks for.
///
/// `constraint` is what makes nesting work. A join fetches the children of a
/// whole page of parents at once — `song_id IN (…)` — and that has to travel
/// *through* the child's own pipeline, filter and all, or every level of
/// nesting costs a statement per parent. Zero calls the same thing a
/// multi-constraint, and it is why its `fetch` takes one.
#[derive(Debug, Default, Clone, Copy)]
pub struct Fetch<'a> {
    /// Seek past this row, in the pipeline's order. A whole row, in table
    /// order, the same thing [`Plan::start`] carries.
    pub start: Option<&'a [Value]>,
    /// An extra condition, on top of whatever the query already said.
    pub constraint: Option<&'a Node>,
    /// At most this many rows, when the caller knows it cannot use more.
    ///
    /// A `Take` hydrating its window wants twenty rows and not the table; a
    /// refill wants exactly one. Without this the limit is applied after the
    /// rows arrive, and a view of the top twenty of a hundred thousand reads a
    /// hundred thousand — which a row counter in the tests caught and an
    /// assertion about the answer never would.
    pub limit: Option<u32>,
}

impl<'a> Fetch<'a> {
    pub fn after(start: &'a [Value]) -> Self {
        Fetch {
            start: Some(start),
            ..Fetch::default()
        }
    }

    pub fn where_(constraint: &'a Node) -> Self {
        Fetch {
            constraint: Some(constraint),
            ..Fetch::default()
        }
    }

    pub fn at_most(self, limit: usize) -> Self {
        Fetch {
            limit: Some(limit as u32),
            ..self
        }
    }
}

/// One stage of a pipeline.
///
/// Both directions: `fetch` pulls nodes, `push` reacts to a change. An operator
/// that only pushed could not maintain a limit.
pub trait Operator {
    /// Pull nodes, in the pipeline's order.
    ///
    /// The cursor in [`Fetch`] is why a limit can be maintained at all: a seek
    /// past a known row rather than a scan and a skip.
    fn fetch(&mut self, store: &mut dyn Store, req: Fetch<'_>) -> Vec<Tree>;

    /// React to a change to the database.
    ///
    /// The *raw* change, not a reshaped one, so that every stage can decide for
    /// itself whether the change is about a table it cares about. Returns what
    /// it becomes for whatever is above: nothing if this operator swallows it,
    /// or several if it causes a knock-on — which is what a limit does when a
    /// delete makes room.
    fn push(&mut self, store: &mut dyn Store, change: &Change) -> Vec<Delta>;
}

/// A boxed operator is an operator, so a pipeline can be assembled at runtime
/// from a plan that only says at build time which stages it has.
///
/// `Send`, because a view lives inside whatever holds the client — on the phone
/// that is a UniFFI object reachable from any thread. Nothing in a pipeline is
/// thread-bound; the bound is only here so the boxes say so.
impl Operator for Box<dyn Operator + Send> {
    fn fetch(&mut self, store: &mut dyn Store, req: Fetch<'_>) -> Vec<Tree> {
        (**self).fetch(store, req)
    }

    fn push(&mut self, store: &mut dyn Store, change: &Change) -> Vec<Delta> {
        (**self).push(store, change)
    }
}

// ----------------------------------------------------------------- the source

/// The bottom of every pipeline: the table itself.
///
/// It holds the whole plan except the limit, so a pull is one statement with
/// the filter and the order already in it. A push it only has to recognise —
/// deciding whether a change is even about this table.
pub struct Source {
    plan: Plan,
}

impl Source {
    pub fn new(plan: Plan) -> Self {
        Source { plan }
    }
}

impl Operator for Source {
    fn fetch(&mut self, store: &mut dyn Store, req: Fetch<'_>) -> Vec<Tree> {
        let mut plan = self.plan.clone();
        plan.start = req.start.map(|s| s.to_vec());
        if let Some(constraint) = req.constraint {
            // Into the statement, not applied to the rows it returns. This is
            // the whole point: one query for a page of parents' children.
            plan.filter = Some(and(plan.filter.take(), constraint.clone()));
        }
        // A constrained pull is many parents' children at once, so the caller's
        // limit is per parent and cannot be applied to the whole statement.
        plan.limit = if req.constraint.is_some() {
            None
        } else {
            req.limit
        };
        store.fetch(&plan).into_iter().map(Tree::leaf).collect()
    }

    fn push(&mut self, _store: &mut dyn Store, change: &Change) -> Vec<Delta> {
        if table_of(change) != self.plan.table {
            return Vec::new();
        }
        vec![match change.clone() {
            Change::Add { row, .. } => Delta::Add(Tree::leaf(row)),
            Change::Remove { row, .. } => Delta::Remove(row),
            Change::Edit { old, new, .. } => Delta::Edit {
                old,
                new: Tree::leaf(new),
            },
        }]
    }
}

// ----------------------------------------------------------------- the filter

/// `WHERE`, for changes.
///
/// The predicate is compiled into SQL for a pull — it is in the source's plan —
/// and interpreted here for a push, because a change arrives as a row and there
/// is no statement to put it through. One `Node`, read two ways, which is the
/// reason a query is data rather than a string.
///
/// So `fetch` delegates: its input has already applied this.
pub struct Filter<I> {
    input: I,
    node: Node,
    columns: &'static [&'static str],
}

impl<I: Operator> Filter<I> {
    pub fn new(input: I, node: Node, columns: &'static [&'static str]) -> Self {
        Filter {
            input,
            node,
            columns,
        }
    }

    fn admits(&self, row: &Row) -> bool {
        matches(&self.node, row, self.columns)
    }
}

impl<I: Operator> Operator for Filter<I> {
    fn fetch(&mut self, store: &mut dyn Store, req: Fetch<'_>) -> Vec<Tree> {
        self.input.fetch(store, req)
    }

    fn push(&mut self, store: &mut dyn Store, change: &Change) -> Vec<Delta> {
        let mut out = Vec::new();
        for delta in self.input.push(store, change) {
            match delta {
                // An edit can cross the predicate in either direction, and then
                // it is not an edit any more. Getting this wrong is how a row
                // that no longer qualifies stays on screen.
                Delta::Edit { old, new } => {
                    let (was, is) = (self.admits(&old), self.admits(&new.row));
                    match (was, is) {
                        (true, true) => out.push(Delta::Edit { old, new }),
                        (false, true) => out.push(Delta::Add(new)),
                        (true, false) => out.push(Delta::Remove(old)),
                        (false, false) => {}
                    }
                }
                other => {
                    if self.admits(other.row()) {
                        out.push(other);
                    }
                }
            }
        }
        out
    }
}

// ------------------------------------------------------------------- the join

/// A relationship, maintained: a row with its children hanging off it.
///
/// The child side is a *pipeline*, not a plan, which is what makes nesting
/// work: a song's notes can themselves carry their author, and each level is
/// an operator like any other. It is also why [`Fetch`] carries a constraint —
/// the `IN` covering a whole page of parents has to travel through the child's
/// own filter and limit rather than be applied outside them.
///
/// The parent side comes from `input`. The child side does not come from below
/// — a change to the child table is nothing to the parent's source — so the raw
/// change goes to the child pipeline separately, and whatever it makes of it,
/// including a deeper `Child`, is attributed to the parent it belongs under.
pub struct Join<I, C> {
    input: I,
    child: C,
    /// The relationship's name, which is what a `Child` change carries so a
    /// row with two of them can tell which one moved.
    name: &'static str,
    /// The parent's column the child points at, as a position.
    from: usize,
    /// The child's column that points back, by name and by position.
    to: (String, usize),
    /// The parent's own plan, for finding the parent a changed child points at.
    parent: Plan,
    from_column: String,
}

impl<I: Operator, C: Operator> Join<I, C> {
    pub fn new<P: Table, K: Table>(
        input: I,
        parent: Plan,
        name: &'static str,
        rel: Relation<P, K>,
        child: C,
    ) -> Self {
        Join {
            from: position(P::DEF.columns, rel.from),
            to: (rel.to.to_string(), position(K::DEF.columns, rel.to)),
            input,
            child,
            name,
            parent,
            from_column: rel.from.to_string(),
        }
    }

    /// Every child of these parents, in one pull through the child pipeline,
    /// grouped by the parent each belongs to.
    fn children_of(&mut self, store: &mut dyn Store, parents: &[Row]) -> Vec<Vec<Tree>> {
        let keys: Vec<Value> = parents.iter().map(|p| p[self.from].clone()).collect();
        if keys.is_empty() {
            return Vec::new();
        }
        let constraint = Node::In {
            column: self.to.0.clone(),
            values: keys,
        };
        let kids = self.child.fetch(store, Fetch::where_(&constraint));
        parents
            .iter()
            .map(|parent| {
                kids.iter()
                    .filter(|kid| kid.row[self.to.1] == parent[self.from])
                    .cloned()
                    .collect()
            })
            .collect()
    }

    fn hydrate(&mut self, store: &mut dyn Store, rows: Vec<Row>) -> Vec<Tree> {
        let related = self.children_of(store, &rows);
        rows.into_iter()
            .zip(related)
            .map(|(row, kids)| Tree {
                row,
                related: vec![(self.name, kids)],
            })
            .collect()
    }

    /// The parent a child row points at, if there is one this view would hold.
    ///
    /// The parent plan's own filter is kept, so hearting a song the view
    /// excludes reports nothing rather than a change to a row that is not
    /// there.
    fn parent_of(&mut self, store: &mut dyn Store, child: &Row) -> Option<Row> {
        let mut plan = self.parent.clone();
        plan.filter = Some(and(
            plan.filter.take(),
            Node::Cmp {
                column: self.from_column.clone(),
                op: Op::Eq,
                value: child[self.to.1].clone(),
            },
        ));
        plan.limit = Some(1);
        store.fetch(&plan).into_iter().next()
    }

    fn under(&self, parent: Row, change: Delta) -> Delta {
        Delta::Child {
            parent,
            name: self.name,
            change: Box::new(change),
        }
    }
}

impl<I: Operator, C: Operator> Operator for Join<I, C> {
    fn fetch(&mut self, store: &mut dyn Store, req: Fetch<'_>) -> Vec<Tree> {
        let rows: Vec<Row> = self
            .input
            .fetch(store, req)
            .into_iter()
            .map(|tree| tree.row)
            .collect();
        self.hydrate(store, rows)
    }

    fn push(&mut self, store: &mut dyn Store, change: &Change) -> Vec<Delta> {
        let mut out = Vec::new();

        // The parent side: whatever the input made of the change, with the
        // children filled in. An added parent arrives with its playlist
        // position already on it rather than blank for a frame.
        for delta in self.input.push(store, change) {
            match delta {
                Delta::Add(tree) => {
                    out.extend(
                        self.hydrate(store, vec![tree.row])
                            .into_iter()
                            .map(Delta::Add),
                    );
                }
                Delta::Edit { old, new } => {
                    let mut hydrated = self.hydrate(store, vec![new.row]);
                    out.push(Delta::Edit {
                        old,
                        new: hydrated.pop().expect("one in, one out"),
                    });
                }
                other => out.push(other),
            }
        }

        // The child side, which nothing below this can see. Note that the
        // change goes to the child *pipeline*, so what comes back may itself be
        // a `Child` from a level deeper — and it is attributed the same way.
        for delta in self.child.push(store, change) {
            match delta {
                Delta::Edit { old, new } => {
                    // A child can be edited onto a different parent. That is a
                    // remove from one and an add to the other; passing it
                    // through whole would leave the old parent holding it.
                    let from = self.parent_of(store, &old);
                    let to = self.parent_of(store, &new.row);
                    match (from, to) {
                        (Some(a), Some(b)) if a == b => {
                            out.push(self.under(a, Delta::Edit { old, new }))
                        }
                        (from, to) => {
                            if let Some(parent) = from {
                                out.push(self.under(parent, Delta::Remove(old)));
                            }
                            if let Some(parent) = to {
                                out.push(self.under(parent, Delta::Add(new)));
                            }
                        }
                    }
                }
                other => {
                    if let Some(parent) = self.parent_of(store, &other.row().clone()) {
                        out.push(self.under(parent, other));
                    }
                }
            }
        }
        out
    }
}

// ------------------------------------------------------------------- the take

/// `ORDER BY … LIMIT`, maintained.
///
/// This is the operator the whole design exists for. It keeps the window — at
/// most `limit` nodes — and nothing else, so its memory is the size of what is
/// on screen rather than the size of the table.
///
/// The hard case is a delete inside the window: the answer is now one node
/// short and the one that should replace it was never in memory. Because an
/// operator can pull, that is a seek from the last node it still holds, and
/// costs one row rather than a re-run.
///
/// # Partitions
///
/// Under a join, a limit means "this many *per parent*" — three notes for each
/// song, not three notes altogether. So the state is one window per parent
/// rather than one window, keyed by the column the relationship joins on, and
/// that is what `partition` carries. Zero keys its take state the same way and
/// for the same reason. Without it a limited relationship is not merely slower
/// but wrong, and quietly: the first parent takes the whole limit and every
/// other parent shows nothing.
pub struct Take<I> {
    input: I,
    limit: usize,
    order: Vec<(usize, Dir)>,
    /// The column a limit is counted within, and its position in a row.
    partition: Option<(String, usize)>,
    /// One window per partition, in order. `Value::Null` is the key when there
    /// is no partition, so there is one code path rather than two.
    windows: Vec<(Value, Vec<Tree>)>,
}

impl<I: Operator> Take<I> {
    pub fn new(input: I, limit: usize, order: Vec<(usize, Dir)>) -> Self {
        Take {
            input,
            limit,
            order,
            partition: None,
            windows: Vec::new(),
        }
    }

    /// Count the limit within this column. Set by the join above.
    pub fn per(mut self, column: &str, at: usize) -> Self {
        self.partition = Some((column.to_string(), at));
        self
    }

    /// Which window a row belongs in.
    fn key_of(&self, row: &Row) -> Value {
        match &self.partition {
            Some((_, at)) => row[*at].clone(),
            None => Value::Null,
        }
    }

    fn window(&mut self, key: &Value) -> &mut Vec<Tree> {
        if let Some(at) = self.windows.iter().position(|(k, _)| k == key) {
            return &mut self.windows[at].1;
        }
        self.windows.push((key.clone(), Vec::new()));
        &mut self.windows.last_mut().expect("just pushed").1
    }

    fn held(&self, key: &Value) -> Option<&Vec<Tree>> {
        self.windows.iter().find(|(k, _)| k == key).map(|(_, w)| w)
    }

    fn before(&self, a: &Row, b: &Row) -> std::cmp::Ordering {
        compare(a, b, &self.order)
    }

    /// Pull the node after this partition's window, to replace one that left.
    ///
    /// Constrained to the partition, so a song short of a note asks for that
    /// song's next note rather than the next note in the table.
    fn refill(&mut self, store: &mut dyn Store, key: &Value) -> Option<Tree> {
        let bound = self.held(key).and_then(|w| w.last()).map(|t| t.row.clone());
        let constraint = self.partition.as_ref().map(|(column, _)| Node::Cmp {
            column: column.clone(),
            op: Op::Eq,
            value: key.clone(),
        });
        let req = Fetch {
            start: bound.as_deref(),
            constraint: constraint.as_ref(),
            limit: Some(1),
        };
        self.input.fetch(store, req).into_iter().next()
    }

    fn insert(&mut self, tree: Tree) {
        let key = self.key_of(&tree.row);
        let order = self.order.clone();
        let window = self.window(&key);
        let at = window.partition_point(|held| compare(&held.row, &tree.row, &order).is_lt());
        window.insert(at, tree);
    }
}

impl<I: Operator> Operator for Take<I> {
    fn fetch(&mut self, store: &mut dyn Store, req: Fetch<'_>) -> Vec<Tree> {
        if req.constraint.is_some() {
            // A page of parents at once. One statement for all of them — that
            // is what the constraint is for — and then `limit` kept from each,
            // because the limit is per parent and SQL cannot say that without a
            // window function the plan has no room for.
            //
            // The cost is that a parent with many children has them all read
            // and most thrown away. Fine for the handful a screen shows under a
            // row; the fix if it is ever not, is a seek per partition, which is
            // a statement per parent and only better when the children are
            // many.
            let rows = self.input.fetch(store, req);
            if self.partition.is_none() {
                let mut rows = rows;
                rows.truncate(self.limit);
                return rows;
            }
            let mut kept: Vec<(Value, usize)> = Vec::new();
            let mut out = Vec::new();
            for tree in rows {
                let key = self.key_of(&tree.row);
                let seen = match kept.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, n)) => n,
                    None => {
                        kept.push((key, 0));
                        &mut kept.last_mut().expect("just pushed").1
                    }
                };
                if *seen < self.limit {
                    *seen += 1;
                    out.push(tree);
                }
            }
            // Remember them, so a later push maintains the same windows a pull
            // produced rather than a different set.
            self.windows.clear();
            for tree in &out {
                let key = self.key_of(&tree.row);
                self.window(&key).push(tree.clone());
            }
            return out;
        }

        let key = Value::Null;
        if self.held(&key).is_none() {
            let mut rows = self.input.fetch(store, req.at_most(self.limit));
            rows.truncate(self.limit);
            *self.window(&key) = rows;
        }
        let window = self.held(&key).cloned().unwrap_or_default();
        match req.start {
            None => window,
            Some(start) => window
                .into_iter()
                .filter(|tree| compare(&tree.row, &start.to_vec(), &self.order).is_gt())
                .collect(),
        }
    }

    fn push(&mut self, store: &mut dyn Store, change: &Change) -> Vec<Delta> {
        let mut out = Vec::new();
        for delta in self.input.push(store, change) {
            // An edit is a move when it touches the order, and a limit cares
            // about position above all, so it is split. Keeping it whole would
            // be an optimisation that has to prove the ordering columns did not
            // change.
            //
            // The add goes *first*, and that is not arbitrary. By the time a
            // change arrives here the store is already at the new state, so a
            // pull can see rows this push has not reported yet. Remove-then-add
            // means the refill after the remove can fetch the edited row
            // itself, and then the add puts it in a second time. Add-first
            // makes the new row part of the window before anything seeks past
            // it, so it cannot be pulled in twice.
            let steps: Vec<Delta> = match delta {
                Delta::Edit { old, new } => vec![Delta::Add(new), Delta::Remove(old)],
                other => vec![other],
            };

            for step in steps {
                match step {
                    Delta::Add(tree) => {
                        let key = self.key_of(&tree.row);
                        // Past the bound and this partition's window is already
                        // full: it does not belong on screen, and nothing above
                        // needs to hear about it. The case that makes an append
                        // to a big table cost nothing.
                        let full = self.held(&key).is_some_and(|w| w.len() >= self.limit)
                            && self
                                .held(&key)
                                .and_then(|w| w.last())
                                .is_some_and(|last| self.before(&tree.row, &last.row).is_gt());
                        if full {
                            continue;
                        }
                        self.insert(tree.clone());
                        out.push(Delta::Add(tree));
                        if self.held(&key).is_some_and(|w| w.len() > self.limit) {
                            let evicted = self.window(&key).pop().expect("longer than the limit");
                            out.push(Delta::Remove(evicted.row));
                        }
                    }
                    Delta::Remove(row) => {
                        let key = self.key_of(&row);
                        let Some(at) = self
                            .held(&key)
                            .and_then(|w| w.iter().position(|held| held.row == row))
                        else {
                            continue;
                        };
                        self.window(&key).remove(at);
                        out.push(Delta::Remove(row));
                        // This partition is a node short. Pull the next one
                        // rather than re-running the query.
                        if let Some(next) = self.refill(store, &key) {
                            self.window(&key).push(next.clone());
                            out.push(Delta::Add(next));
                        }
                    }
                    // A child moved under a row. The row itself has not, so the
                    // window's shape is unchanged — but the copy it holds has
                    // to be updated too, or a later pull hands back a node with
                    // stale children.
                    Delta::Child {
                        parent,
                        name,
                        change,
                    } => {
                        let key = self.key_of(&parent);
                        let Some(held) =
                            self.window(&key).iter_mut().find(|held| held.row == parent)
                        else {
                            continue;
                        };
                        apply_child(held, name, &change);
                        out.push(Delta::Child {
                            parent,
                            name,
                            change,
                        });
                    }
                    Delta::Edit { .. } => unreachable!("split above"),
                }
            }
        }
        out
    }
}

// --------------------------------------------------------------- the pipeline

/// A pipeline under construction.
///
/// It exists so that relationships can nest. `View::related` is the one-level
/// convenience; two levels are two `related` calls, and there is no depth this
/// type knows about.
///
/// ```text
/// Pipeline::of(albums)
///     .related(Album::song, Pipeline::of(songs)
///         .related(Song::note, Pipeline::of(notes)))
/// ```
///
/// The limit is held back rather than applied as it goes, so the `Take` ends up
/// *above* the joins. Not for correctness — this join nests rather than
/// flattening, so there is no product and either position counts parents — but
/// because the window then holds whole nodes. A child that moves under a row in
/// the window updates the copy held there, and a later pull is answered from
/// memory instead of re-reading the children.
pub struct Pipeline {
    top: Box<dyn Operator + Send>,
    order: Vec<(usize, Dir)>,
    limit: Option<u32>,
    /// This level's plan, which the join above needs to find the parent a
    /// changed child points at.
    plan: Plan,
}

impl Pipeline {
    /// Start one from a query.
    ///
    /// The filter goes in twice on purpose: into the source, where it becomes
    /// SQL and makes a pull one statement, and into a [`Filter`], which judges
    /// rows that arrive as changes. One `Node`, read two ways.
    pub fn of<T: Table + 'static>(query: petros_schema::Query<T>) -> Self {
        let plan = query.into_plan();
        let def = T::DEF;
        let order = order_of(&plan, &def);

        let mut source = plan.clone();
        source.limit = None;
        source.start = None;
        let mut top: Box<dyn Operator + Send> = Box::new(Source::new(source));
        if let Some(node) = plan.filter.clone() {
            top = Box::new(Filter::new(top, node, def.columns));
        }

        let mut bare = plan.clone();
        bare.limit = None;
        bare.start = None;
        Pipeline {
            top,
            order,
            limit: plan.limit,
            plan: bare,
        }
    }

    /// Hang a child pipeline off each row.
    ///
    /// The relationship is named after the child's table, which is the name
    /// `tables!` gives the constant it generates — so `Song::favorite` and
    /// `with::<Favorite>()` agree without anyone writing the string.
    pub fn related<P: Table, C: Table + 'static>(
        mut self,
        rel: Relation<P, C>,
        child: Pipeline,
    ) -> Self {
        let parent = self.plan.clone();
        let within = (rel.to, position(C::DEF.columns, rel.to));
        self.top = Box::new(Join::new(
            self.top,
            parent,
            C::DEF.name,
            rel,
            child.finish(Some(within)),
        ));
        self
    }

    /// Put the take on top.
    ///
    /// `within` is the column a limit is counted inside, which the join above
    /// supplies: under a relationship a limit means "this many per parent", and
    /// a take that does not know that gives the whole limit to the first parent
    /// and nothing to the rest.
    fn finish(self, within: Option<(&str, usize)>) -> Box<dyn Operator + Send> {
        match self.limit {
            Some(limit) => {
                let take = Take::new(self.top, limit as usize, self.order);
                Box::new(match within {
                    Some((column, at)) => take.per(column, at),
                    None => take,
                })
            }
            None => self.top,
        }
    }
}

/// What a view did to its own list, in the order it did it.
///
/// The point is a caller who keeps a *rendered* list beside the view — decoded
/// rows, widgets, whatever a screen holds. Maintaining the query and then
/// decoding every row again is still O(n), and past a few hundred rows that
/// decode is most of what is left; these say which entries moved so the rest
/// are not touched.
///
/// Positions are valid in sequence: apply them in order to a list that started
/// equal to the view's and it ends equal again. The node travels with the patch
/// rather than being looked up afterwards, because by then the view has already
/// applied the rest of them.
#[derive(Debug, Clone, PartialEq)]
pub enum Patch {
    Insert { at: usize, node: Tree },
    Remove { at: usize },
    Update { at: usize, node: Tree },
}

// ------------------------------------------------------------------- the view

/// A query, materialised and kept right.
///
/// Hydrate it once and then hand it what each mutation changed.
pub struct View<P: Table> {
    top: Box<dyn Operator + Send>,
    nodes: Vec<Tree>,
    order: Vec<(usize, Dir)>,
    hydrated: bool,
    marker: std::marker::PhantomData<fn() -> P>,
}

impl<P: Table + 'static> View<P> {
    /// A view over any pipeline, however deep.
    pub fn over(pipeline: Pipeline) -> Self {
        let order = pipeline.order.clone();
        View {
            top: pipeline.finish(None),
            nodes: Vec::new(),
            order,
            hydrated: false,
            marker: std::marker::PhantomData,
        }
    }

    /// A flat query, maintained.
    pub fn new(query: petros_schema::Query<P>) -> Self {
        Self::over(Pipeline::of(query))
    }

    /// One relationship hanging off each row — the maintained counterpart of
    /// `select_with`, and the same arguments in the same order. For more than
    /// one level, build a [`Pipeline`] and use [`View::over`].
    pub fn related<C: Table + 'static>(
        query: petros_schema::Query<P>,
        rel: Relation<P, C>,
        children: petros_schema::Query<C>,
    ) -> Self {
        Self::over(Pipeline::of(query).related(rel, Pipeline::of(children)))
    }

    /// Run the query once, to have something to maintain.
    pub fn hydrate(&mut self, store: &mut dyn Store) {
        self.nodes = self.top.fetch(store, Fetch::default());
        self.hydrated = true;
    }

    /// Take account of one change to the database.
    ///
    /// Returns what it did to its own list — empty when the change misses the
    /// view entirely: a different table, a row the filter refuses, an append
    /// past a full window. That is the common case in a long list and the
    /// reason any of this is worth doing, so it is something a caller can see
    /// rather than a claim: a UI can skip a render on nothing, splice a
    /// rendered list rather than rebuild it, and a test can hold the cost to
    /// zero instead of only checking the answer.
    pub fn push(&mut self, store: &mut dyn Store, change: &Change) -> Vec<Patch> {
        debug_assert!(self.hydrated, "hydrate the view before pushing to it");
        let mut patches = Vec::new();
        for delta in self.top.push(store, change) {
            match delta {
                Delta::Add(tree) => {
                    let at = self
                        .nodes
                        .partition_point(|held| compare(&held.row, &tree.row, &self.order).is_lt());
                    patches.push(Patch::Insert {
                        at,
                        node: tree.clone(),
                    });
                    self.nodes.insert(at, tree);
                }
                Delta::Remove(row) => {
                    if let Some(at) = self.nodes.iter().position(|held| held.row == row) {
                        self.nodes.remove(at);
                        patches.push(Patch::Remove { at });
                    }
                }
                Delta::Edit { old, new } => {
                    // A move, not an update: an edit can change where the row
                    // sorts, and a list that only replaced in place would show
                    // it in the old position.
                    if let Some(at) = self.nodes.iter().position(|held| held.row == old) {
                        self.nodes.remove(at);
                        patches.push(Patch::Remove { at });
                    }
                    let at = self
                        .nodes
                        .partition_point(|held| compare(&held.row, &new.row, &self.order).is_lt());
                    patches.push(Patch::Insert {
                        at,
                        node: new.clone(),
                    });
                    self.nodes.insert(at, new);
                }
                Delta::Child {
                    parent,
                    name,
                    change,
                } => {
                    if let Some(at) = self.nodes.iter().position(|held| held.row == parent) {
                        apply_child(&mut self.nodes[at], name, &change);
                        patches.push(Patch::Update {
                            at,
                            node: self.nodes[at].clone(),
                        });
                    }
                }
            }
        }
        patches
    }

    /// Everything a mutation changed, in one call. What a client does after
    /// applying an entry.
    pub fn apply(&mut self, store: &mut dyn Store, changes: &[Change]) -> Vec<Patch> {
        changes.iter().flat_map(|c| self.push(store, c)).collect()
    }

    /// The answer, decoded.
    pub fn rows(&self) -> Vec<P> {
        self.nodes
            .iter()
            .filter_map(|node| P::from_row(&node.row))
            .collect()
    }

    /// The answer, decoded to whatever depth the type asks for.
    ///
    /// ```text
    /// view.decode::<With<Song, Favorite>>()                    // one level
    /// view.decode::<With<Song, With<Note, Author>>>()          // three
    /// ```
    ///
    /// The type is the projection. Ask for less than the view holds and the
    /// rest is not decoded; ask for more and the missing levels come back
    /// empty, because a relationship the pipeline does not carry has no
    /// children under that name.
    pub fn decode<T: FromNode>(&self) -> Vec<T> {
        self.nodes.iter().filter_map(T::from_node).collect()
    }

    /// One relationship, which is what most views have. The same shape
    /// `select_with` returns, so a screen reads the run query or the maintained
    /// one without knowing which.
    pub fn with<C: Table + FromNode>(&self) -> Vec<With<P, C>> {
        self.decode()
    }

    /// The tree as the pipeline holds it, untyped. For a caller that wants the
    /// nodes rather than a projection of them — a patch applier, or a test.
    pub fn nodes(&self) -> &[Tree] {
        &self.nodes
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

// ------------------------------------------------------------------- a tally

/// How many rows a query matches, maintained.
///
/// A [`View`] already knows its own length, so a count *can* be had by holding
/// every matching row. This holds a number instead: O(1) memory whatever the
/// table does, which is the difference between a count being free and a count
/// costing the answer it is counting.
///
/// The push side is arithmetic. `Filter` has already turned an edit that
/// crosses the predicate into an add or a remove, so by the time a change
/// arrives here there is nothing left to decide — which is worth noticing,
/// because it is the reason this operator is three lines rather than a special
/// case for every kind of change.
///
/// A limit makes no sense here and is ignored: `COUNT` over the top twenty is
/// twenty. Hydrating still reads the matching rows once, because a `Plan` has
/// no `COUNT(*)` in it — the count is maintained after that, not derived again.
pub struct Tally {
    top: Box<dyn Operator + Send>,
    n: usize,
    hydrated: bool,
}

impl Tally {
    /// Count what this query matches.
    pub fn of<T: Table + 'static>(query: petros_schema::Query<T>) -> Self {
        let mut plan = query.into_plan();
        plan.limit = None;
        Tally {
            top: Pipeline::of(petros_schema::Query::<T>::from_plan(plan)).finish(None),
            n: 0,
            hydrated: false,
        }
    }

    pub fn hydrate(&mut self, store: &mut dyn Store) {
        self.n = self.top.fetch(store, Fetch::default()).len();
        self.hydrated = true;
    }

    /// Take account of what a mutation changed. Returns whether the count
    /// moved, so a caller can skip a render on nothing.
    pub fn apply(&mut self, store: &mut dyn Store, changes: &[Change]) -> bool {
        debug_assert!(self.hydrated, "hydrate the tally before pushing to it");
        let before = self.n;
        for change in changes {
            for delta in self.top.push(store, change) {
                match delta {
                    Delta::Add(_) => self.n += 1,
                    Delta::Remove(_) => self.n = self.n.saturating_sub(1),
                    // A row that stayed, and a child that moved under one. The
                    // filter has already decided both are still matches.
                    Delta::Edit { .. } | Delta::Child { .. } => {}
                }
            }
        }
        self.n != before
    }

    pub fn get(&self) -> usize {
        self.n
    }
}

// ------------------------------------------------------------------ machinery

fn table_of(change: &Change) -> &str {
    match change {
        Change::Add { table, .. } | Change::Remove { table, .. } | Change::Edit { table, .. } => {
            table
        }
    }
}

fn position(columns: &[&str], name: &str) -> usize {
    columns.iter().position(|c| *c == name).unwrap_or(0)
}

/// Add a condition to one that may already be there.
fn and(existing: Option<Node>, extra: Node) -> Node {
    match existing {
        Some(node) => Node::All(vec![node, extra]),
        None => extra,
    }
}

/// One change, against the children a node holds under `name`.
///
/// Recursive: a `Child` inside a `Child` walks down another level, which is how
/// a note edited under a song under an album reaches the note.
///
/// The children arrive from the child pipeline already in its order, and a new
/// one is appended rather than sorted in — the pipeline decides the order, and
/// re-deriving it here would be a second opinion about it. A view that needs
/// the child order maintained across inserts pulls the relationship again;
/// that is the next thing to sharpen if it matters.
fn apply_child(node: &mut Tree, name: &'static str, change: &Delta) {
    let kids = node.children_mut(name);
    match change {
        Delta::Add(tree) => kids.push(tree.clone()),
        Delta::Remove(row) => {
            if let Some(at) = kids.iter().position(|held| held.row == *row) {
                kids.remove(at);
            }
        }
        Delta::Edit { old, new } => {
            if let Some(at) = kids.iter().position(|held| held.row == *old) {
                kids[at] = new.clone();
            }
        }
        Delta::Child {
            parent,
            name: inner,
            change,
        } => {
            if let Some(kid) = kids.iter_mut().find(|held| held.row == *parent) {
                apply_child(kid, inner, change);
            }
        }
    }
}

/// The plan's order, as column positions. Resolved once, so comparing two rows
/// is not a string lookup per column.
fn order_of(plan: &Plan, def: &TableDef) -> Vec<(usize, Dir)> {
    plan.order
        .iter()
        .filter_map(|(name, dir)| {
            def.columns
                .iter()
                .position(|c| c == name)
                .map(|i| (i, *dir))
        })
        .collect()
}

fn compare(a: &Row, b: &Row, order: &[(usize, Dir)]) -> std::cmp::Ordering {
    for (i, dir) in order {
        let ord = a[*i].cmp(&b[*i]);
        let ord = match dir {
            Dir::Asc => ord,
            Dir::Desc => ord.reverse(),
        };
        if ord.is_ne() {
            return ord;
        }
    }
    std::cmp::Ordering::Equal
}

/// A filter, evaluated against one row. The same `Node` the source turns into
/// SQL — which is what keeps a pushed row and a pulled row agreeing.
fn matches(node: &Node, row: &Row, columns: &[&str]) -> bool {
    match node {
        Node::Cmp { column, op, value } => {
            let Some(at) = columns.iter().position(|c| c == column) else {
                return false;
            };
            let ord = row[at].cmp(value);
            match op {
                Op::Eq => ord.is_eq(),
                Op::Ne => ord.is_ne(),
                Op::Lt => ord.is_lt(),
                Op::Le => ord.is_le(),
                Op::Gt => ord.is_gt(),
                Op::Ge => ord.is_ge(),
            }
        }
        Node::In { column, values } => {
            let Some(at) = columns.iter().position(|c| c == column) else {
                return false;
            };
            values.contains(&row[at])
        }
        Node::All(nodes) => nodes.iter().all(|n| matches(n, row, columns)),
        Node::Any(nodes) => nodes.iter().any(|n| matches(n, row, columns)),
        Node::Not(inner) => !matches(inner, row, columns),
    }
}
