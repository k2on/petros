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

/// A row, as it travels through a pipeline: positional, in table order.
///
/// Untyped on purpose. Decoding is the [`View`]'s job and happens once, at the
/// top, rather than at every operator.
pub type Row = Vec<Value>;

/// What a pipeline carries: a row, and whatever hangs off it.
///
/// Every stage deals in these, even the ones with no relationship in them,
/// where `related` is simply empty. One item type rather than two means an
/// operator does not have to know whether there is a join below it.
#[derive(Debug, Clone, PartialEq)]
pub struct Tree {
    pub row: Row,
    pub related: Vec<Row>,
}

impl Tree {
    /// A row with nothing under it.
    pub fn leaf(row: Row) -> Self {
        Tree {
            row,
            related: Vec::new(),
        }
    }
}

/// A change, as it travels up a pipeline.
///
/// The first three are what a write reports. `Child` is Zero's fourth, and a
/// tree cannot do without it: hearting a song does not add, remove or edit any
/// song, but the library view has to move. Without it the only way to say
/// "something beneath this row changed" is to remove the row and add it back,
/// which is a screen flicker and a lost scroll position.
///
/// Zero's is recursive because its trees nest arbitrarily. This one is not,
/// because [`Join`] nests one level — the same level `select_with` reads — and
/// a recursive type that can only ever be one deep is a lie about the design.
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
        change: ChildChange,
    },
}

/// What happened underneath a row.
#[derive(Debug, Clone, PartialEq)]
pub enum ChildChange {
    Add(Row),
    Remove(Row),
    Edit { old: Row, new: Row },
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

/// One stage of a pipeline.
///
/// Both directions: `fetch` pulls nodes, `push` reacts to a change. An operator
/// that only pushed could not maintain a limit.
pub trait Operator {
    /// Pull nodes, in the pipeline's order, starting after `start`.
    ///
    /// `start` is a cursor, not an offset: it is the last row already seen — a
    /// whole row, in table order, the same thing [`Plan::start`] carries — and
    /// it exists so that a seek is a seek rather than a scan and a skip.
    fn fetch(&mut self, store: &mut dyn Store, start: Option<&[Value]>) -> Vec<Tree>;

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
impl Operator for Box<dyn Operator> {
    fn fetch(&mut self, store: &mut dyn Store, start: Option<&[Value]>) -> Vec<Tree> {
        (**self).fetch(store, start)
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
    fn fetch(&mut self, store: &mut dyn Store, start: Option<&[Value]>) -> Vec<Tree> {
        let mut plan = self.plan.clone();
        plan.start = start.map(|s| s.to_vec());
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
    fn fetch(&mut self, store: &mut dyn Store, start: Option<&[Value]>) -> Vec<Tree> {
        self.input.fetch(store, start)
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
/// The parent side comes from `input`. The child side does not come from
/// anywhere below — a change to the child table is nothing to the parent's
/// source — so this operator reads the raw change on its way down and decides
/// for itself. That is the second thing the return-value shape buys.
///
/// Children are fetched for a whole page at once, with `IN`, rather than one
/// statement per parent. The same batched fetch `select_with` does.
pub struct Join<I> {
    input: I,
    /// The parent's column the child points at, as a position.
    from: usize,
    /// The child's column that points back, by name and by position.
    to: (String, usize),
    child: Plan,
    child_def: TableDef,
    /// The parent's own plan, for finding the parent a changed child points at.
    parent: Plan,
    from_column: String,
}

impl<I: Operator> Join<I> {
    pub fn new<P: Table, C: Table>(
        input: I,
        parent: Plan,
        rel: Relation<P, C>,
        children: petros_schema::Query<C>,
    ) -> Self {
        let child = children.into_plan();
        Join {
            from: position(P::DEF.columns, rel.from),
            to: (rel.to.to_string(), position(C::DEF.columns, rel.to)),
            input,
            child,
            child_def: C::DEF,
            parent,
            from_column: rel.from.to_string(),
        }
    }

    /// Every child of these parents, in one statement, grouped.
    fn children_of(&mut self, store: &mut dyn Store, parents: &[Row]) -> Vec<Vec<Row>> {
        let keys: Vec<Value> = parents.iter().map(|p| p[self.from].clone()).collect();
        if keys.is_empty() {
            return Vec::new();
        }
        let mut plan = self.child.clone();
        plan.filter = Some(and(
            plan.filter.take(),
            Node::In {
                column: self.to.0.clone(),
                values: keys,
            },
        ));
        // A limit here would mean "this many children per parent", which one
        // statement cannot say. `select_with` refuses it for the same reason.
        plan.limit = None;

        let rows = store.fetch(&plan);
        parents
            .iter()
            .map(|parent| {
                rows.iter()
                    .filter(|child| child[self.to.1] == parent[self.from])
                    .cloned()
                    .collect()
            })
            .collect()
    }

    fn hydrate(&mut self, store: &mut dyn Store, rows: Vec<Row>) -> Vec<Tree> {
        let related = self.children_of(store, &rows);
        rows.into_iter()
            .zip(related)
            .map(|(row, related)| Tree { row, related })
            .collect()
    }

    /// Does this child belong in the relationship at all?
    fn admits(&self, child: &Row) -> bool {
        self.child
            .filter
            .as_ref()
            .is_none_or(|node| matches(node, child, self.child_def.columns))
    }

    /// The parent a child points at, if there is one the view would hold.
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
}

impl<I: Operator> Operator for Join<I> {
    fn fetch(&mut self, store: &mut dyn Store, start: Option<&[Value]>) -> Vec<Tree> {
        let rows: Vec<Row> = self
            .input
            .fetch(store, start)
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
                    let hydrated = self.hydrate(store, vec![tree.row]);
                    out.extend(hydrated.into_iter().map(Delta::Add));
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

        // The child side, which nothing below this can see: a change to the
        // child table is not a change to any parent row, and yet the view has
        // to move. This is the case `Child` exists for.
        if table_of(change) != self.child.table {
            return out;
        }
        match change.clone() {
            Change::Add { row, .. } => {
                if self.admits(&row) {
                    if let Some(parent) = self.parent_of(store, &row) {
                        out.push(Delta::Child {
                            parent,
                            change: ChildChange::Add(row),
                        });
                    }
                }
            }
            Change::Remove { row, .. } => {
                if self.admits(&row) {
                    if let Some(parent) = self.parent_of(store, &row) {
                        out.push(Delta::Child {
                            parent,
                            change: ChildChange::Remove(row),
                        });
                    }
                }
            }
            Change::Edit { old, new, .. } => {
                // A child can be edited across the relationship's own filter,
                // or onto a different parent. Both are a remove from one place
                // and an add to another, and treating them as an edit would
                // leave the old parent holding a child it no longer has.
                let (was, is) = (self.admits(&old), self.admits(&new));
                let from = if was {
                    self.parent_of(store, &old)
                } else {
                    None
                };
                let to = if is {
                    self.parent_of(store, &new)
                } else {
                    None
                };
                match (from, to) {
                    (Some(a), Some(b)) if a == b => out.push(Delta::Child {
                        parent: a,
                        change: ChildChange::Edit { old, new },
                    }),
                    (from, to) => {
                        if let Some(parent) = from {
                            out.push(Delta::Child {
                                parent,
                                change: ChildChange::Remove(old),
                            });
                        }
                        if let Some(parent) = to {
                            out.push(Delta::Child {
                                parent,
                                change: ChildChange::Add(new),
                            });
                        }
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
pub struct Take<I> {
    input: I,
    limit: usize,
    order: Vec<(usize, Dir)>,
    /// The window, in order. The last of these is the bound.
    window: Vec<Tree>,
    hydrated: bool,
}

impl<I: Operator> Take<I> {
    pub fn new(input: I, limit: usize, order: Vec<(usize, Dir)>) -> Self {
        Take {
            input,
            limit,
            order,
            window: Vec::new(),
            hydrated: false,
        }
    }

    fn before(&self, a: &Row, b: &Row) -> std::cmp::Ordering {
        compare(a, b, &self.order)
    }

    /// The cursor for a seek: the last row held.
    fn bound(&self) -> Option<Row> {
        self.window.last().map(|tree| tree.row.clone())
    }

    /// Pull the node that comes after the window, to replace one that left it.
    fn refill(&mut self, store: &mut dyn Store) -> Option<Tree> {
        let bound = self.bound();
        self.input.fetch(store, bound.as_deref()).into_iter().next()
    }

    fn insert(&mut self, tree: Tree) {
        let at = self
            .window
            .partition_point(|held| self.before(&held.row, &tree.row).is_lt());
        self.window.insert(at, tree);
    }
}

impl<I: Operator> Operator for Take<I> {
    fn fetch(&mut self, store: &mut dyn Store, start: Option<&[Value]>) -> Vec<Tree> {
        if !self.hydrated {
            self.window = self.input.fetch(store, None);
            self.window.truncate(self.limit);
            self.hydrated = true;
        }
        match start {
            None => self.window.clone(),
            Some(start) => self
                .window
                .iter()
                .filter(|tree| compare(&tree.row, &start.to_vec(), &self.order).is_gt())
                .cloned()
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
                        // Past the bound and the window is already full: it
                        // does not belong on screen, and nothing above needs
                        // to hear about it. This is the case that makes an
                        // append to a big table cost nothing.
                        if self.window.len() >= self.limit
                            && self
                                .window
                                .last()
                                .is_some_and(|last| self.before(&tree.row, &last.row).is_gt())
                        {
                            continue;
                        }
                        self.insert(tree.clone());
                        out.push(Delta::Add(tree));
                        if self.window.len() > self.limit {
                            let evicted = self.window.pop().expect("longer than the limit");
                            out.push(Delta::Remove(evicted.row));
                        }
                    }
                    Delta::Remove(row) => {
                        let Some(at) = self.window.iter().position(|held| held.row == row) else {
                            continue;
                        };
                        self.window.remove(at);
                        out.push(Delta::Remove(row));
                        // The window is a node short. Pull the next one rather
                        // than re-running the query.
                        if let Some(next) = self.refill(store) {
                            self.window.push(next.clone());
                            out.push(Delta::Add(next));
                        }
                    }
                    // A child moved under a row. The row itself has not, so the
                    // window's shape is unchanged — but the copy it holds has
                    // to be updated too, or the next refill hands the view a
                    // node with stale children.
                    Delta::Child { parent, change } => {
                        let Some(held) = self.window.iter_mut().find(|held| held.row == parent)
                        else {
                            continue;
                        };
                        apply_child(&mut held.related, &change, &self.order);
                        out.push(Delta::Child { parent, change });
                    }
                    Delta::Edit { .. } => unreachable!("split above"),
                }
            }
        }
        out
    }
}

// ------------------------------------------------------------------- the view

/// A query, materialised and kept right.
///
/// Hydrate it once and then hand it what each mutation changed.
pub struct View<P: Table> {
    top: Box<dyn Operator>,
    nodes: Vec<Tree>,
    order: Vec<(usize, Dir)>,
    child_order: Vec<(usize, Dir)>,
    hydrated: bool,
    marker: std::marker::PhantomData<fn() -> P>,
}

impl<P: Table + 'static> View<P> {
    /// Compile a query into a pipeline.
    ///
    /// The filter goes in twice on purpose: into the source, where it becomes
    /// SQL and makes the hydrate one statement, and into a [`Filter`], where it
    /// judges rows that arrive as changes. The limit becomes a [`Take`].
    pub fn new(query: petros_schema::Query<P>) -> Self {
        Self::build(query, |top| top, Vec::new())
    }

    /// The same, with a relationship hanging off each row — the maintained
    /// counterpart of `select_with`, and the same arguments in the same order.
    pub fn related<C: Table + 'static>(
        query: petros_schema::Query<P>,
        rel: Relation<P, C>,
        children: petros_schema::Query<C>,
    ) -> Self {
        let child_order = order_of(children.plan(), &C::DEF);
        let parent = {
            let mut plan = query.plan().clone();
            plan.limit = None;
            plan.start = None;
            plan
        };
        Self::build(
            query,
            move |top| Box::new(Join::new(top, parent, rel, children)) as Box<dyn Operator>,
            child_order,
        )
    }

    fn build(
        query: petros_schema::Query<P>,
        join: impl FnOnce(Box<dyn Operator>) -> Box<dyn Operator>,
        child_order: Vec<(usize, Dir)>,
    ) -> Self {
        let plan = query.into_plan();
        let def = P::DEF;
        let order = order_of(&plan, &def);

        let mut source = plan.clone();
        source.limit = None;
        source.start = None;
        let mut top: Box<dyn Operator> = Box::new(Source::new(source));
        if let Some(node) = plan.filter.clone() {
            top = Box::new(Filter::new(top, node, def.columns));
        }
        // The join sits above the filter and below the take: a parent the
        // filter refused has no children worth fetching, and the take must
        // count parents rather than rows of a product.
        top = join(top);
        if let Some(limit) = plan.limit {
            top = Box::new(Take::new(top, limit as usize, order.clone()));
        }

        View {
            top,
            nodes: Vec::new(),
            order,
            child_order,
            hydrated: false,
            marker: std::marker::PhantomData,
        }
    }

    /// Run the query once, to have something to maintain.
    pub fn hydrate(&mut self, store: &mut dyn Store) {
        self.nodes = self.top.fetch(store, None);
        self.hydrated = true;
    }

    /// Take account of one change to the database.
    ///
    /// Returns how many changes actually reached the view, which is zero when
    /// the change misses it — a different table, a row the filter refuses, an
    /// append past a full window. That is the common case in a long list and
    /// the reason any of this is worth doing, so it is a number a caller can
    /// see rather than a claim: a UI can skip a render on nothing, and a test
    /// can hold the cost to zero instead of only checking the answer.
    pub fn push(&mut self, store: &mut dyn Store, change: &Change) -> usize {
        debug_assert!(self.hydrated, "hydrate the view before pushing to it");
        let mut moved = 0;
        for delta in self.top.push(store, change) {
            moved += 1;
            match delta {
                Delta::Add(tree) => {
                    let at = self
                        .nodes
                        .partition_point(|held| compare(&held.row, &tree.row, &self.order).is_lt());
                    self.nodes.insert(at, tree);
                }
                Delta::Remove(row) => {
                    if let Some(at) = self.nodes.iter().position(|held| held.row == row) {
                        self.nodes.remove(at);
                    }
                }
                Delta::Edit { old, new } => {
                    if let Some(at) = self.nodes.iter().position(|held| held.row == old) {
                        self.nodes.remove(at);
                    }
                    let at = self
                        .nodes
                        .partition_point(|held| compare(&held.row, &new.row, &self.order).is_lt());
                    self.nodes.insert(at, new);
                }
                Delta::Child { parent, change } => {
                    if let Some(held) = self.nodes.iter_mut().find(|held| held.row == parent) {
                        apply_child(&mut held.related, &change, &self.child_order);
                    }
                }
            }
        }
        moved
    }

    /// Everything a mutation changed, in one call. What a client does after
    /// applying an entry. Returns how much of it reached the view.
    pub fn apply(&mut self, store: &mut dyn Store, changes: &[Change]) -> usize {
        changes.iter().map(|c| self.push(store, c)).sum()
    }

    /// The answer, decoded.
    pub fn rows(&self) -> Vec<P> {
        self.nodes
            .iter()
            .filter_map(|node| P::from_row(&node.row))
            .collect()
    }

    /// The answer with its relationship, for a view built by [`View::related`].
    /// The same shape `select_with` returns, so a screen reads one or the other
    /// without knowing which.
    pub fn with<C: Table>(&self) -> Vec<With<P, C>> {
        self.nodes
            .iter()
            .filter_map(|node| {
                Some(With {
                    row: P::from_row(&node.row)?,
                    related: node.related.iter().filter_map(|r| C::from_row(r)).collect(),
                })
            })
            .collect()
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
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

/// One child change, against the list a node holds.
fn apply_child(related: &mut Vec<Row>, change: &ChildChange, order: &[(usize, Dir)]) {
    match change {
        ChildChange::Add(row) => {
            let at = related.partition_point(|held| compare(held, row, order).is_lt());
            related.insert(at, row.clone());
        }
        ChildChange::Remove(row) => {
            if let Some(at) = related.iter().position(|held| held == row) {
                related.remove(at);
            }
        }
        ChildChange::Edit { old, new } => {
            if let Some(at) = related.iter().position(|held| held == old) {
                related.remove(at);
            }
            let at = related.partition_point(|held| compare(held, new, order).is_lt());
            related.insert(at, new.clone());
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
