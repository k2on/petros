//! Reading, without writing SQL.
//!
//! A query is a value: a table, some conditions, an order, a limit, and
//! optionally a cursor to start from. It is built with typed columns, so a
//! comparison against the wrong type does not compile, and it is *data* — which
//! is the part that matters beyond taste.
//!
//! Being data is what lets the same query be compiled to one SQL statement
//! today and to a maintained pipeline later. Zero's `Take` operator, which is
//! how `ORDER BY … LIMIT` is maintained incrementally, works by seeking its
//! source from the last row it accepted — `start` below is that seek, and it is
//! here from the beginning because retrofitting it would mean rewriting every
//! source.

use crate::{ColumnTy, Table, Value};
use core::marker::PhantomData;

/// One column of one table, carrying both in its type.
///
/// `Song::pos.eq(…)` will not compile against a `Favorite` query, nor against a
/// string.
#[derive(Debug)]
pub struct Column<T, V> {
    pub name: &'static str,
    pub ty: ColumnTy,
    marker: PhantomData<fn() -> (T, V)>,
}

impl<T, V> Clone for Column<T, V> {
    fn clone(&self) -> Self {
        *self
    }
}
impl<T, V> Copy for Column<T, V> {}

impl<T, V> Column<T, V> {
    pub const fn new(name: &'static str, ty: ColumnTy) -> Self {
        Column {
            name,
            ty,
            marker: PhantomData,
        }
    }

    /// Ascending by this column. An order is a list of these.
    pub fn asc(self) -> Order<T> {
        Order {
            column: self.name,
            dir: Dir::Asc,
            marker: PhantomData,
        }
    }

    pub fn desc(self) -> Order<T> {
        Order {
            column: self.name,
            dir: Dir::Desc,
            marker: PhantomData,
        }
    }
}

/// The comparisons a column supports. Typed: the value has to be the column's.
impl<T, V: crate::Bind> Column<T, V> {
    pub fn eq(self, v: V) -> Cond<T> {
        self.cmp(Op::Eq, v)
    }
    pub fn ne(self, v: V) -> Cond<T> {
        self.cmp(Op::Ne, v)
    }
    pub fn lt(self, v: V) -> Cond<T> {
        self.cmp(Op::Lt, v)
    }
    pub fn le(self, v: V) -> Cond<T> {
        self.cmp(Op::Le, v)
    }
    pub fn gt(self, v: V) -> Cond<T> {
        self.cmp(Op::Gt, v)
    }
    pub fn ge(self, v: V) -> Cond<T> {
        self.cmp(Op::Ge, v)
    }

    fn cmp(self, op: Op, v: V) -> Cond<T> {
        Cond {
            node: Node::Cmp {
                column: self.name.to_string(),
                op,
                value: v.to_value(),
            },
            marker: PhantomData,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Dir {
    Asc,
    Desc,
}

/// A condition, as data.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Node {
    Cmp {
        /// Owned, because a plan crosses a wasm boundary and a borrow does not.
        column: String,
        op: Op,
        value: Value,
    },
    All(Vec<Node>),
    Any(Vec<Node>),
    Not(Box<Node>),
}

/// A condition on one table, with the table in its type.
#[derive(Debug)]
pub struct Cond<T> {
    pub node: Node,
    marker: PhantomData<fn() -> T>,
}

impl<T> Cond<T> {
    pub fn and(self, other: Cond<T>) -> Cond<T> {
        Cond {
            node: Node::All(vec![self.node, other.node]),
            marker: PhantomData,
        }
    }
    pub fn or(self, other: Cond<T>) -> Cond<T> {
        Cond {
            node: Node::Any(vec![self.node, other.node]),
            marker: PhantomData,
        }
    }
    /// `negate`, not `not`: a bare `not` reads as `std::ops::Not` at every call
    /// site, and this is not that.
    pub fn negate(self) -> Cond<T> {
        Cond {
            node: Node::Not(Box::new(self.node)),
            marker: PhantomData,
        }
    }
}

/// One term of an order.
#[derive(Debug)]
pub struct Order<T> {
    pub column: &'static str,
    pub dir: Dir,
    marker: PhantomData<fn() -> T>,
}

/// A read, as data.
///
/// The dynamic form: what crosses a wasm boundary and what a source compiles.
#[derive(Debug, Clone, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Plan {
    pub table: String,
    pub filter: Option<Node>,
    pub order: Vec<(String, Dir)>,
    pub limit: Option<u32>,
    /// Seek past this row in the order. What an incrementally maintained
    /// `limit` uses to find the row that replaces a deleted one.
    pub start: Option<Vec<Value>>,
}

/// A read being built, with the table in its type.
#[derive(Debug)]
pub struct Query<T> {
    plan: Plan,
    marker: PhantomData<fn() -> T>,
}

impl<T: Table> Default for Query<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Table> Query<T> {
    pub fn new() -> Self {
        Query {
            plan: Plan {
                table: T::DEF.name.to_string(),
                ..Plan::default()
            },
            marker: PhantomData,
        }
    }

    /// Narrow the result. Repeating this ands the conditions together, which is
    /// what reading them one after another suggests.
    pub fn filter(mut self, cond: Cond<T>) -> Self {
        self.plan.filter = Some(match self.plan.filter.take() {
            Some(existing) => Node::All(vec![existing, cond.node]),
            None => cond.node,
        });
        self
    }

    /// Order the result. Always say one: SQLite's natural order is not a
    /// contract, and two peers showing the same rows differently is a bug that
    /// only appears on someone else's machine.
    pub fn order_by(mut self, order: Order<T>) -> Self {
        self.plan.order.push((order.column.to_string(), order.dir));
        self
    }

    pub fn limit(mut self, n: u32) -> Self {
        self.plan.limit = Some(n);
        self
    }

    /// Start after this row, in the query's order.
    pub fn start(mut self, row: &T) -> Self {
        self.plan.start = Some(row.to_row());
        self
    }

    pub fn plan(&self) -> &Plan {
        &self.plan
    }

    pub fn into_plan(self) -> Plan {
        self.plan
    }
}

/// Every row of a table, in some order. The start of every read.
pub fn all<T: Table>() -> Query<T> {
    Query::new()
}
