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
