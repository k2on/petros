//! The verbs this domain understands, declared once.
//!
//! Read four times, which is the point: by [`domain::apply`](crate::domain) so
//! an unknown verb can say what it *does* know; by `crates/todo-wasm`, which
//! carries it into the module so a tool holding only the `.wasm` can recover
//! it; by `emit-mutators`, which turns it into TypeScript types; and by the
//! tests, which check that everything declared here is handled, so the
//! declaration cannot quietly lie.
//!
//! Arguments are what a *caller* passes. Anything `fill_auto` supplies — an id,
//! a timestamp — is deliberately absent: the caller does not choose it, and a
//! type that offered the choice would be lying. `Add` takes only `text`; its
//! `id` and `created_ms` are frozen into the log at the originating client.
//!
//! This was a dependency-free file `include!`d by whoever needed it — textual
//! sharing, which broke once on a `//!` comment and would have broken again.

petros_schema::declare! {
    Add { text: Text }
    SetDone { id: Id, done: Bool }
    Remove { id: Id }
    MarkAllDone {}
    AddFive {}
}
