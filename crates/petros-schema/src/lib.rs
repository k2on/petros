//! What an app is, as data.
//!
//! An app tells Petros what its mutations are called and what arguments they
//! take. That description is not documentation — it is read at least four
//! times, by four different things, and the whole point is that they read the
//! *same* one:
//!
//!   - the domain, so an unknown verb can say what it does know;
//!   - the module compiled from it, which will one day carry this with it;
//!   - the host that runs the module;
//!   - the code generator, which turns it into TypeScript types so a call site
//!     naming a verb that does not exist is a compile error rather than a
//!     message on somebody's phone.
//!
//! The engine's own entry point is deliberately generic — `mutate(kind, args)`
//! takes a string, because the engine genuinely does not know what verbs exist,
//! and that is what lets a new verb ship without a native build. This is the
//! other half of that bargain: unknown at runtime, known at compile time, from
//! one declaration.
//!
//! `AddFive` shipped as `AddRandom` once, to a phone, for exactly the reason
//! this crate exists.

#![forbid(unsafe_code)]

pub mod store;
pub use store::{Backend, Cell, Column, ColumnTy, Store, Table, TableDef, Value, Write};

/// Every mutation an app understands.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct AppSchema {
    pub verbs: Vec<Verb>,
}

impl AppSchema {
    pub fn new(verbs: impl IntoIterator<Item = Verb>) -> Self {
        AppSchema {
            verbs: verbs.into_iter().collect(),
        }
    }

    /// The verb by that name, if the app has one.
    pub fn verb(&self, name: &str) -> Option<&Verb> {
        self.verbs.iter().find(|v| v.name == name)
    }

    /// Every verb name, in declaration order. What an "unknown verb" error
    /// should list.
    pub fn names(&self) -> Vec<&str> {
        self.verbs.iter().map(|v| v.name.as_str()).collect()
    }
}

/// One mutation: a name the log will carry forever, and the arguments a caller
/// supplies.
///
/// Arguments are what a *caller* passes. Anything `fill_auto` supplies — an id,
/// a timestamp — is deliberately absent: the caller does not get to choose it,
/// and a type that offered the choice would be lying.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Verb {
    pub name: String,
    pub args: Vec<Arg>,
}

impl Verb {
    /// A verb with no arguments.
    pub fn new(name: impl Into<String>) -> Self {
        Verb {
            name: name.into(),
            args: Vec::new(),
        }
    }

    /// Add an argument. Chained, so a declaration reads in the order the
    /// arguments appear.
    pub fn arg(mut self, name: impl Into<String>, ty: Ty) -> Self {
        self.args.push(Arg {
            name: name.into(),
            ty,
        });
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Arg {
    pub name: String,
    pub ty: Ty,
}

/// The types an argument can have.
///
/// Short on purpose. Every one of these has to survive the round trip through
/// JSON at the foreign boundary, through CBOR in the log, and through SQLite —
/// and it has to mean the same thing in all three. Floats are absent because
/// `apply` must not branch on one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Ty {
    /// A canonical uuid string in memory; sixteen bytes on the wire. The one
    /// conversion the JSON entry point performs, and the only non-JSON type
    /// here.
    Id,
    Text,
    Integer,
    Bool,
}

impl Ty {
    /// The name this type is declared under, and carried as.
    pub const fn name(self) -> &'static str {
        match self {
            Ty::Id => "Id",
            Ty::Text => "Text",
            Ty::Integer => "Integer",
            Ty::Bool => "Bool",
        }
    }

    /// The inverse of [`name`](Self::name).
    pub fn parse(s: &str) -> Result<Ty, String> {
        match s {
            "Id" => Ok(Ty::Id),
            "Text" => Ok(Ty::Text),
            "Integer" => Ok(Ty::Integer),
            "Bool" => Ok(Ty::Bool),
            other => Err(format!("`{other}` is not a type Petros knows")),
        }
    }

    /// How this type is spelled in TypeScript.
    pub const fn typescript(self) -> &'static str {
        match self {
            Ty::Id => "Id",
            Ty::Text => "string",
            Ty::Integer => "number",
            Ty::Bool => "boolean",
        }
    }
}

// ------------------------------------------------------- what a mutation sees

/// The database, as much of it as a mutation is allowed to see.
///
/// Three methods, and deliberately no more. On wasm this is what the sandbox
/// enforces — the module imports these and nothing else, so `apply` *cannot*
/// read a clock, draw a random number, open a socket or touch a file. Natively
/// the same trait is the only argument `apply` gets, so the rule is the same
/// one, kept by construction rather than by review.
pub trait Host {
    /// First column of the first row, as an integer. Zero for no rows or NULL —
    /// which is what `MAX(pos)` over an empty table should mean.
    fn query_int(&mut self, sql: &str) -> i64;
    /// Whether the query matched anything at all.
    fn query_exists(&mut self, sql: &str) -> bool;
    /// Run a statement.
    fn exec(&mut self, sql: &str);
}

/// A SQLite literal, escaped the way SQLite defines them.
///
/// Values are inlined rather than bound because the wasm side has no way to
/// bind: it holds a channel to the host's SQLite, not a connection. Both builds
/// go through here so the SQL is identical either way, which is the property a
/// conformance test can then check.
pub enum Lit<'a> {
    Int(i64),
    Text(&'a str),
    Blob(&'a [u8]),
}

pub fn lit(v: Lit<'_>) -> String {
    match v {
        Lit::Int(i) => i.to_string(),
        // A single quote is escaped by doubling it. That is the whole rule.
        Lit::Text(s) => format!("'{}'", s.replace('\'', "''")),
        Lit::Blob(b) => {
            let mut out = String::with_capacity(b.len() * 2 + 3);
            out.push_str("X'");
            for byte in b {
                out.push_str(&format!("{byte:02x}"));
            }
            out.push('\'');
            out
        }
    }
}

// ------------------------------------------------- declaring one, carrying one

/// The custom section a module carries its schema in.
pub const SECTION: &str = "petros_schema";

/// Declare an app's verbs once, and get both halves.
///
/// Expands to a `schema()` function for anything that wants the typed form, and
/// a `SCHEMA_TEXT` constant for the module to carry. The two cannot drift,
/// because they are the same tokens.
///
/// ```
/// petros_schema::declare! {
///     Add { text: Text }
///     SetDone { id: Id, done: Bool }
///     MarkAllDone {}
/// }
/// assert_eq!(schema().names(), ["Add", "SetDone", "MarkAllDone"]);
/// assert_eq!(SCHEMA_TEXT, "Add text:Text\nSetDone id:Id done:Bool\nMarkAllDone\n");
/// ```
#[macro_export]
macro_rules! declare {
    ($($verb:ident { $($arg:ident : $ty:ident),* $(,)? })*) => {
        /// Every mutation a caller may author.
        pub fn schema() -> $crate::AppSchema {
            $crate::AppSchema::new([
                $( $crate::Verb::new(stringify!($verb))
                    $( .arg(stringify!($arg), $crate::Ty::$ty) )* ,)*
            ])
        }

        /// The same declaration as the bytes a module carries, so a tool with
        /// only the `.wasm` can recover it. See [`petros_schema::embed`].
        pub const SCHEMA_TEXT: &str = concat!(
            $( stringify!($verb), $(" ", stringify!($arg), ":", stringify!($ty),)* "\n",)*
        );
    };
}

/// Put a `SCHEMA_TEXT` into this module's `petros_schema` custom section.
///
/// For the wasm build only, and the reason the module is self-describing: a
/// generator that has the artifact does not also need to compile the domain to
/// know what is in it. That is not a nicety — linking the domain into the code
/// generator put 0.31s on a 0.45s edit-to-device loop, because the generator
/// then relinks on every change to `apply`.
#[macro_export]
macro_rules! embed {
    ($text:path) => {
        #[link_section = "petros_schema"]
        #[used]
        static PETROS_SCHEMA_SECTION: [u8; $text.len()] = $crate::section_bytes($text);
    };
}

/// `&str` to a fixed-size array, in const, so it can sit in a link section.
/// `include_bytes!` cannot help here: the text is computed, not a file.
pub const fn section_bytes<const N: usize>(s: &str) -> [u8; N] {
    let src = s.as_bytes();
    let mut out = [0u8; N];
    let mut i = 0;
    while i < N {
        out[i] = src[i];
        i += 1;
    }
    out
}

// ------------------------------------------------------------------ recovering

/// Read a schema back out of a compiled module.
///
/// Walks the section table rather than searching for the text, so a module that
/// merely *mentions* the marker is not mistaken for one that declares it. No
/// dependencies and no interpreter: the caller may be a code generator on the
/// hot path.
pub fn from_wasm(wasm: &[u8]) -> Result<AppSchema, String> {
    if wasm.len() < 8 || &wasm[..4] != b"\0asm" {
        return Err("not a wasm module".into());
    }
    let mut i = 8;
    while i < wasm.len() {
        let id = wasm[i];
        i += 1;
        let (size, next) = leb128(wasm, i)?;
        let end = next
            .checked_add(size)
            .filter(|e| *e <= wasm.len())
            .ok_or("a section runs past the end of the module")?;
        if id == 0 {
            let (name_len, after_len) = leb128(wasm, next)?;
            let name_end = after_len
                .checked_add(name_len)
                .filter(|e| *e <= end)
                .ok_or("a section name runs past its section")?;
            if &wasm[after_len..name_end] == SECTION.as_bytes() {
                let text = std::str::from_utf8(&wasm[name_end..end])
                    .map_err(|e| format!("the schema section is not utf-8: {e}"))?;
                return parse(text);
            }
        }
        i = end;
    }
    Err(format!(
        "the module carries no `{SECTION}` section — is `petros_schema::embed!` missing?"
    ))
}

/// One unsigned LEB128, and where it ended.
fn leb128(bytes: &[u8], mut i: usize) -> Result<(usize, usize), String> {
    let (mut value, mut shift) = (0usize, 0u32);
    loop {
        let byte = *bytes
            .get(i)
            .ok_or("a length runs past the end of the module")?;
        i += 1;
        value |= ((byte & 0x7f) as usize) << shift;
        if byte & 0x80 == 0 {
            return Ok((value, i));
        }
        shift += 7;
        if shift > 63 {
            return Err("a length is not a valid LEB128".into());
        }
    }
}

/// The text form: one verb per line, `Name arg:Ty arg:Ty`.
pub fn parse(text: &str) -> Result<AppSchema, String> {
    let mut verbs = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        let mut parts = line.split_whitespace();
        let name = parts.next().ok_or("a schema line has no verb")?;
        let mut verb = Verb::new(name);
        for part in parts {
            let (arg, ty) = part
                .split_once(':')
                .ok_or_else(|| format!("`{part}` is not `name:Type`"))?;
            verb = verb.arg(arg, Ty::parse(ty)?);
        }
        verbs.push(verb);
    }
    Ok(AppSchema { verbs })
}

// ----------------------------------------------------- declaring the whole app

/// The CBOR a mutation is, and the few accessors every domain needs.
#[cfg(feature = "cbor")]
pub mod cbor {
    pub use ciborium::value::Value;

    pub fn field<'a>(v: &'a Value, name: &str) -> Option<&'a Value> {
        v.as_map()?
            .iter()
            .find(|(k, _)| k.as_text() == Some(name))
            .map(|(_, v)| v)
    }

    /// Set a field, replacing it if it is already there. What `fill_auto` uses
    /// to hoist a value in exactly once.
    pub fn set(v: &mut Value, name: &str, to: Value) {
        if let Value::Map(entries) = v {
            for (k, existing) in entries.iter_mut() {
                if k.as_text() == Some(name) {
                    *existing = to;
                    return;
                }
            }
            entries.push((Value::Text(name.to_string()), to));
        }
    }

    pub fn as_text(v: &Value) -> Option<String> {
        v.as_text().map(str::to_string)
    }

    pub fn as_bytes(v: &Value) -> Option<Vec<u8>> {
        v.as_bytes().cloned()
    }

    pub fn as_int(v: &Value) -> Option<i64> {
        v.as_integer().and_then(|i| i128::from(i).try_into().ok())
    }

    // Each of these is what one declared type decodes to. Required arguments
    // say which mutation and which field is missing, because that error reaches
    // a person; the rest take the empty value, because a field a caller left off
    // means the same thing on every replica and refusing it is the domain's
    // decision to make, not the decoder's.
    #[doc(hidden)]
    pub fn need_id(m: &Value, verb: &str, name: &str) -> Result<Vec<u8>, String> {
        field(m, name)
            .and_then(as_bytes)
            .ok_or_else(|| format!("{verb} has no {name}"))
    }

    #[doc(hidden)]
    pub fn need_array<'a>(m: &'a Value, verb: &str, name: &str) -> Result<&'a [Value], String> {
        match field(m, name) {
            Some(Value::Array(items)) => Ok(items),
            _ => Err(format!("{verb} has no {name}")),
        }
    }

    #[doc(hidden)]
    pub fn opt_text(m: &Value, name: &str) -> String {
        field(m, name).and_then(as_text).unwrap_or_default()
    }

    #[doc(hidden)]
    pub fn opt_int(m: &Value, name: &str) -> i64 {
        field(m, name).and_then(as_int).unwrap_or(0)
    }

    #[doc(hidden)]
    pub fn opt_bool(m: &Value, name: &str) -> bool {
        matches!(field(m, name), Some(Value::Bool(true)))
    }
}

/// Declare a domain: its verbs, their arguments, and what each one does.
///
/// This exists because the alternative had a hole in it. The schema said
/// `Add { text: Text }` and `apply` reached for a field it named itself, and
/// nothing checked the two agreed — so renaming an argument on one side
/// generated TypeScript describing a module nobody was running, and the call
/// site type-checked all the way to a device. Here the name is written once and
/// both the schema entry and the binding come from it.
///
/// `auto` arguments are decoded like any other but never enter the schema:
/// `fill_auto` supplies them at the originating client, so a caller neither
/// chooses them nor should see a type offering the choice.
///
/// ```
/// use petros_schema::Host;
/// petros_schema::mutations! {
///     /// Add one.
///     Add { text: Text } auto { id: Id } => |host, actor| {
///         host.exec(&format!("INSERT INTO t VALUES ({}, {}, {})",
///             petros_schema::lit(petros_schema::Lit::Blob(&id)),
///             petros_schema::lit(petros_schema::Lit::Text(&text)),
///             petros_schema::lit(petros_schema::Lit::Text(actor))));
///         Ok(())
///     }
///     Clear {} => |host, actor| {
///         let _ = actor;
///         host.exec("DELETE FROM t");
///         Ok(())
///     }
/// }
/// // The schema carries what a caller passes, and not what `fill_auto` does.
/// assert_eq!(schema().names(), ["Add", "Clear"]);
/// assert_eq!(schema().verb("Add").unwrap().args.len(), 1);
/// ```
#[cfg(feature = "cbor")]
#[macro_export]
macro_rules! mutations {
    (
        $(
            $(#[$meta:meta])*
            $verb:ident { $($arg:ident : $ty:ident),* $(,)? }
            $(auto { $($aarg:ident : $aty:ident),* $(,)? })?
            => |$host:ident, $actor:ident| $body:block
        )*
    ) => {
        $crate::declare! { $( $verb { $($arg : $ty),* } )* }

        /// Applying a mutation: `Ok`, or a deterministic refusal every replica
        /// reaches identically.
        pub fn apply<H: $crate::Host>(
            host: &mut H,
            mutation: &$crate::cbor::Value,
            actor: &str,
        ) -> ::core::result::Result<(), ::std::string::String> {
            let tag = $crate::cbor::field(mutation, "t")
                .and_then($crate::cbor::as_text)
                .unwrap_or_default();
            match tag.as_str() {
                $(
                    stringify!($verb) => {
                        // The names the body asked for. Bound here rather than
                        // in the signature because a signature is written once
                        // and these are per-arm.
                        let $host = &mut *host;
                        let $actor: &str = actor;
                        $( let $arg = $crate::mutations!(@get $ty, mutation, stringify!($verb), stringify!($arg)); )*
                        $($( let $aarg = $crate::mutations!(@get $aty, mutation, stringify!($verb), stringify!($aarg)); )*)?
                        $body
                    }
                )*
                // A variant this build has never heard of. The log is permanent
                // and variants are only ever added, so this is a peer newer
                // than us. Saying what this one *does* know turns "why did
                // nothing happen" into an answer.
                other => ::core::result::Result::Err(::std::format!(
                    "unknown mutation \"{}\"; this build knows {}",
                    other,
                    schema().names().join(", ")
                )),
            }
        }
    };

    // Required, because a missing one means the payload is malformed.
    (@get Id, $m:expr, $verb:expr, $name:expr) => { $crate::cbor::need_id($m, $verb, $name)? };
    (@get Array, $m:expr, $verb:expr, $name:expr) => { $crate::cbor::need_array($m, $verb, $name)? };
    // Optional, because an absent one is a value every replica agrees on.
    (@get Text, $m:expr, $verb:expr, $name:expr) => { $crate::cbor::opt_text($m, $name) };
    (@get Integer, $m:expr, $verb:expr, $name:expr) => { $crate::cbor::opt_int($m, $name) };
    (@get Bool, $m:expr, $verb:expr, $name:expr) => { $crate::cbor::opt_bool($m, $name) };
}
