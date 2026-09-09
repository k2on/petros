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
//!     View  ->  Take  ->  Filter  ->  Source
//!     ^^^^ owns  ^^^^ owns  ^^^^^^ owns ^^^^^^
//! ```
//!
//! A change enters at the root and is passed down to the source, which decides
//! whether it is even about the right table; the answer travels back up,
//! reshaped by each operator on the way. Same semantics, ordinary ownership,
//! and the pipeline is a value that can be moved and dropped.

use petros_schema::{Change, Dir, Node, Op, Plan, Store, Table, TableDef, Value};

/// A row, as it travels through a pipeline: positional, in table order.
///
/// Untyped on purpose. Decoding is the [`View`]'s job and happens once, at the
/// top, rather than at every operator.
pub type Row = Vec<Value>;

/// One stage of a pipeline.
///
/// Both directions: `fetch` pulls rows, `push` reacts to a change. An operator
/// that only pushed could not maintain a limit.
pub trait Operator {
    /// Pull rows, in the pipeline's order, starting after `start`.
    ///
    /// `start` is a cursor, not an offset: it is the last row already seen — a
    /// whole row, in table order, the same thing [`Plan::start`] carries — and
    /// it exists so that a seek is a seek rather than a scan and a skip. The
    /// ordering columns are picked out of it by whoever does the comparing, so
    /// there is one convention here rather than one per layer.
    fn fetch(&mut self, store: &mut dyn Store, start: Option<&[Value]>) -> Vec<Row>;

    /// React to a change to the database.
    ///
    /// Returns what the change becomes for whatever is above: nothing if this
    /// operator swallows it, one change if it passes through, or several if it
    /// causes a knock-on — which is what a limit does when a delete makes room.
    fn push(&mut self, store: &mut dyn Store, change: &Change) -> Vec<Change>;
}

/// A boxed operator is an operator, so a pipeline can be assembled at runtime
/// from a plan that only says at build time which stages it has.
impl Operator for Box<dyn Operator> {
    fn fetch(&mut self, store: &mut dyn Store, start: Option<&[Value]>) -> Vec<Row> {
        (**self).fetch(store, start)
    }

    fn push(&mut self, store: &mut dyn Store, change: &Change) -> Vec<Change> {
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
    fn fetch(&mut self, store: &mut dyn Store, start: Option<&[Value]>) -> Vec<Row> {
        let mut plan = self.plan.clone();
        plan.start = start.map(|s| s.to_vec());
        store.fetch(&plan)
    }

    fn push(&mut self, _store: &mut dyn Store, change: &Change) -> Vec<Change> {
        if table_of(change) == self.plan.table {
            vec![change.clone()]
        } else {
            vec![]
        }
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
    fn fetch(&mut self, store: &mut dyn Store, start: Option<&[Value]>) -> Vec<Row> {
        self.input.fetch(store, start)
    }

    fn push(&mut self, store: &mut dyn Store, change: &Change) -> Vec<Change> {
        let mut out = Vec::new();
        for change in self.input.push(store, change) {
            match &change {
                Change::Add { row, .. } | Change::Remove { row, .. } => {
                    if self.admits(row) {
                        out.push(change);
                    }
                }
                // An edit can cross the predicate in either direction, and then
                // it is not an edit any more. Getting this wrong is how a row
                // that no longer qualifies stays on screen.
                Change::Edit { table, old, new } => {
                    let (was, is) = (self.admits(old), self.admits(new));
                    out.push(match (was, is) {
                        (true, true) => change.clone(),
                        (false, true) => Change::Add {
                            table: table.clone(),
                            row: new.clone(),
                        },
                        (true, false) => Change::Remove {
                            table: table.clone(),
                            row: old.clone(),
                        },
                        (false, false) => continue,
                    });
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
/// most `limit` rows — and nothing else, so its memory is the size of what is
/// on screen rather than the size of the table.
///
/// The hard case is a delete inside the window: the answer is now one row short
/// and the row that should replace it was never in memory. Because an operator
/// can pull, that is a seek from the last row it still holds, and costs one
/// row rather than a re-run.
pub struct Take<I> {
    input: I,
    limit: usize,
    order: Vec<(usize, Dir)>,
    /// The window, in order. The last of these is the bound.
    window: Vec<Row>,
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

    /// Where a row sorts against the window, by the plan's order.
    fn before(&self, a: &Row, b: &Row) -> std::cmp::Ordering {
        compare(a, b, &self.order)
    }

    /// The cursor for a seek: the last row held.
    fn bound(&self) -> Option<Row> {
        self.window.last().cloned()
    }

    /// Pull the row that comes after the window, to replace one that left it.
    fn refill(&mut self, store: &mut dyn Store) -> Option<Row> {
        let bound = self.bound();
        self.input.fetch(store, bound.as_deref()).into_iter().next()
    }

    fn insert(&mut self, row: Row) {
        let at = self
            .window
            .partition_point(|held| self.before(held, &row).is_lt());
        self.window.insert(at, row);
    }
}

impl<I: Operator> Operator for Take<I> {
    fn fetch(&mut self, store: &mut dyn Store, start: Option<&[Value]>) -> Vec<Row> {
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
                .filter(|row| compare(row, &start.to_vec(), &self.order).is_gt())
                .cloned()
                .collect(),
        }
    }

    fn push(&mut self, store: &mut dyn Store, change: &Change) -> Vec<Change> {
        let mut out = Vec::new();
        for change in self.input.push(store, change) {
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
            let steps: Vec<Change> = match &change {
                Change::Edit { table, old, new } => vec![
                    Change::Add {
                        table: table.clone(),
                        row: new.clone(),
                    },
                    Change::Remove {
                        table: table.clone(),
                        row: old.clone(),
                    },
                ],
                other => vec![other.clone()],
            };

            for step in steps {
                match step {
                    Change::Add { table, row } => {
                        // Past the bound and the window is already full: it
                        // does not belong on screen, and nothing above needs
                        // to hear about it. This is the case that makes an
                        // append to a big table cost nothing.
                        if self.window.len() >= self.limit
                            && self
                                .window
                                .last()
                                .is_some_and(|last| self.before(&row, last).is_gt())
                        {
                            continue;
                        }
                        self.insert(row.clone());
                        out.push(Change::Add {
                            table: table.clone(),
                            row,
                        });
                        if self.window.len() > self.limit {
                            let evicted = self.window.pop().expect("longer than the limit");
                            out.push(Change::Remove {
                                table,
                                row: evicted,
                            });
                        }
                    }
                    Change::Remove { table, row } => {
                        let Some(at) = self.window.iter().position(|held| *held == row) else {
                            continue;
                        };
                        self.window.remove(at);
                        out.push(Change::Remove {
                            table: table.clone(),
                            row,
                        });
                        // The window is a row short. Pull the next one rather
                        // than re-running the query.
                        if let Some(next) = self.refill(store) {
                            self.window.push(next.clone());
                            out.push(Change::Add { table, row: next });
                        }
                    }
                    Change::Edit { .. } => unreachable!("split above"),
                }
            }
        }
        out
    }
}

// ------------------------------------------------------------------- the view

/// A query, materialised and kept right.
///
/// Hydrate it once and then hand it what each mutation changed. `rows` is the
/// answer at any moment, decoded.
pub struct View<T: Table> {
    top: Box<dyn Operator>,
    rows: Vec<Row>,
    order: Vec<(usize, Dir)>,
    hydrated: bool,
    marker: std::marker::PhantomData<fn() -> T>,
}

impl<T: Table + 'static> View<T> {
    /// Compile a query into a pipeline.
    ///
    /// The filter goes in twice on purpose: into the source, where it becomes
    /// SQL and makes the hydrate one statement, and into a [`Filter`], where it
    /// judges rows that arrive as changes. The limit becomes a [`Take`].
    pub fn new(query: petros_schema::Query<T>) -> Self {
        let plan = query.into_plan();
        let def = T::DEF;
        let order = order_of(&plan, &def);

        let mut source = plan.clone();
        source.limit = None;
        source.start = None;
        let mut top: Box<dyn Operator> = Box::new(Source::new(source));
        if let Some(node) = plan.filter.clone() {
            top = Box::new(Filter::new(top, node, def.columns));
        }
        if let Some(limit) = plan.limit {
            top = Box::new(Take::new(top, limit as usize, order.clone()));
        }

        View {
            top,
            rows: Vec::new(),
            order,
            hydrated: false,
            marker: std::marker::PhantomData,
        }
    }

    /// Run the query once, to have something to maintain.
    pub fn hydrate(&mut self, store: &mut dyn Store) {
        self.rows = self.top.fetch(store, None);
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
        for change in self.top.push(store, change) {
            moved += 1;
            match change {
                Change::Add { row, .. } => {
                    let at = self
                        .rows
                        .partition_point(|held| compare(held, &row, &self.order).is_lt());
                    self.rows.insert(at, row);
                }
                Change::Remove { row, .. } => {
                    if let Some(at) = self.rows.iter().position(|held| *held == row) {
                        self.rows.remove(at);
                    }
                }
                Change::Edit { old, new, .. } => {
                    if let Some(at) = self.rows.iter().position(|held| *held == old) {
                        self.rows.remove(at);
                    }
                    let at = self
                        .rows
                        .partition_point(|held| compare(held, &new, &self.order).is_lt());
                    self.rows.insert(at, new);
                }
            }
        }
        moved
    }

    /// Everything a mutation changed, in one call. What a client does after
    /// applying an entry. Returns how much of it reached the view.
    pub fn apply(&mut self, store: &mut dyn Store, changes: &[Change]) -> usize {
        changes.iter().map(|change| self.push(store, change)).sum()
    }

    /// The answer, decoded.
    pub fn rows(&self) -> Vec<T> {
        self.rows.iter().filter_map(|r| T::from_row(r)).collect()
    }

    /// How many rows the view holds, without decoding them.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
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
