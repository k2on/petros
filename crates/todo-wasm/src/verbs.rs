// The verbs this module understands, declared once as data.
//
// Read three times, which is the point:
//
//   - by the module, so an unknown verb can say what it *does* know;
//   - by `emit-mutators`, which turns it into TypeScript types, so a call site
//     naming a verb that does not exist is a compile error rather than a
//     message on somebody's phone;
//   - by the tests, which check that everything declared here is actually
//     handled by `apply`, so the declaration cannot quietly lie.
//
// It exists because the FFI entry point is deliberately generic —
// `mutate(kind, args)` — which is what lets a new verb ship without a native
// build, and which gives up the compiler's opinion on verb names to get it.
// `AddFive` shipped as `AddRandom` once for exactly that reason. This puts the
// check back where it costs nothing.
//
// `args` are what a *caller* passes. Anything `fill_auto` supplies — an id, a
// timestamp — is deliberately absent: the caller does not get to choose it,
// and a type that offered the choice would be lying.
//
// This file has no dependencies and no `use` statements on purpose: it is
// `include!`d by a crate that must not grow a dependency graph.

pub struct Verb {
    pub name: &'static str,
    pub args: &'static [Arg],
}

pub struct Arg {
    pub name: &'static str,
    pub ty: Ty,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    /// A canonical uuid string in memory; sixteen bytes on the wire. The one
    /// conversion `from_json` performs, and the only non-JSON type here.
    Id,
    Text,
    Integer,
    Bool,
}

impl Ty {
    pub const fn typescript(self) -> &'static str {
        match self {
            Ty::Id => "Id",
            Ty::Text => "string",
            Ty::Integer => "number",
            Ty::Bool => "boolean",
        }
    }
}

pub const VERBS: &[Verb] = &[
    Verb {
        name: "Add",
        args: &[Arg {
            name: "text",
            ty: Ty::Text,
        }],
    },
    Verb {
        name: "SetDone",
        args: &[
            Arg {
                name: "id",
                ty: Ty::Id,
            },
            Arg {
                name: "done",
                ty: Ty::Bool,
            },
        ],
    },
    Verb {
        name: "Remove",
        args: &[Arg {
            name: "id",
            ty: Ty::Id,
        }],
    },
    Verb {
        name: "MarkAllDone",
        args: &[],
    },
    Verb {
        name: "AddFive",
        args: &[],
    },
];
