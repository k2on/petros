//! One definition per function.
//!
//! A mutation used to be written three times: once as a verb declaration with a
//! body, once as an authoring helper that built its payload, and once as a
//! method on the client a foreign caller sees. A query was written twice. None
//! of those repetitions carried information — they carried the same
//! information, in three places that could disagree.
//!
//! Here a function is written once, as an ordinary Rust function, and the
//! attribute generates the rest:
//!
//! ```ignore
//! /// Put a song in the library.
//! #[petros::mutation]
//! pub fn add_song(db: &mut Db, id: NewId, added_ms: Now, actor: Actor,
//!                 title: String, artist: String) -> Result<()> {
//!     …
//! }
//! ```
//!
//! # What the parameters mean
//!
//! The engine supplies the leading ones and a caller supplies the rest, and
//! which is which is decided by type rather than by position or by a list:
//!
//! - `&mut Db` — the store. Every function takes one.
//! - `NewId` — a fresh id, chosen once at the originating client by `fill_auto`
//!   and frozen in the log. `apply` may not invent one, because all it can reach
//!   is the store.
//! - `Now` — the clock, frozen the same way.
//! - `Actor` — who authored the entry.
//!
//! Everything after those is an argument, and is what appears in the authoring
//! function, in the schema a module carries, and in the generated TypeScript.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{FnArg, Ident, ItemFn, Pat, PatType, ReturnType, Type};

/// A parameter the engine supplies rather than a caller.
#[derive(Clone, Copy, PartialEq)]
enum Ctx {
    Db,
    NewId,
    Now,
    Actor,
}

impl Ctx {
    fn of(ty: &Type) -> Option<Ctx> {
        let text = quote!(#ty).to_string().replace(' ', "");
        match text.trim_start_matches('&').trim_start_matches("mut") {
            "Db" => Some(Ctx::Db),
            "NewId" => Some(Ctx::NewId),
            "Now" => Some(Ctx::Now),
            "Actor" => Some(Ctx::Actor),
            _ => None,
        }
    }
}

/// One argument a caller passes.
struct Arg {
    name: Ident,
    ty: Type,
}

/// The schema's name for a type. The list is deliberately short: these are the
/// types that survive a log, a CBOR round trip and a foreign boundary without
/// anyone having to decide anything.
fn schema_ty(ty: &Type) -> Result<&'static str, String> {
    let text = quote!(#ty).to_string().replace(' ', "");
    // Matched on the last path segment, so `Id`, `petros::Id` and
    // `petros_schema::Id` are all the same type to a reader and to this.
    let last = text.rsplit("::").next().unwrap_or(&text);
    Ok(match last {
        "String" | "&str" | "&'staticstr" => "Text",
        "i64" | "u64" | "i32" | "u32" => "Integer",
        "bool" => "Bool",
        "Id" | "NewId" | "Vec<u8>" => "Id",
        other => {
            return Err(format!(
                "`{other}` is not a type a mutation argument can be. \
                 The log is permanent and a foreign caller has to be able to write one, \
                 so arguments are String, i64, bool or Id."
            ))
        }
    })
}

struct Parsed {
    ctx: Vec<(Ident, Ctx)>,
    args: Vec<Arg>,
}

fn parse(f: &ItemFn) -> Result<Parsed, syn::Error> {
    let mut ctx = Vec::new();
    let mut args = Vec::new();
    for input in &f.sig.inputs {
        let FnArg::Typed(PatType { pat, ty, .. }) = input else {
            return Err(syn::Error::new_spanned(
                input,
                "a mutation is a free function; it has no `self`",
            ));
        };
        let Pat::Ident(name) = &**pat else {
            return Err(syn::Error::new_spanned(
                pat,
                "each parameter needs a plain name: it becomes an argument name in the log",
            ));
        };
        match Ctx::of(ty) {
            Some(kind) => {
                if !args.is_empty() {
                    return Err(syn::Error::new_spanned(
                        ty,
                        "the engine's parameters come before a caller's",
                    ));
                }
                ctx.push((name.ident.clone(), kind));
            }
            None => args.push(Arg {
                name: name.ident.clone(),
                ty: (**ty).clone(),
            }),
        }
    }
    if !ctx.iter().any(|(_, k)| *k == Ctx::Db) {
        return Err(syn::Error::new_spanned(
            &f.sig,
            "a function needs `db: &mut Db` — it is how it reaches the database",
        ));
    }
    Ok(Parsed { ctx, args })
}

/// Declare a mutation: an intent, applied by every replica, recorded forever.
///
/// See the crate docs for what the parameters mean. What this generates:
///
/// - the body, as `apply` for this verb, taking the store and the payload;
/// - an authoring function of the same name, taking only a caller's arguments
///   and returning the payload to hand to `Client::mutate`;
/// - a line in the module's schema section, so a generator with only the
///   `.wasm` can recover the declaration;
/// - a method on `Peer`, for a foreign caller.
#[proc_macro_attribute]
pub fn mutation(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let f = syn::parse_macro_input!(item as ItemFn);
    match expand_mutation(f) {
        Ok(t) => t.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_mutation(f: ItemFn) -> Result<proc_macro2::TokenStream, syn::Error> {
    let parsed = parse(&f)?;
    let name = f.sig.ident.clone();
    let verb = camel(&name.to_string());
    let verb_lit = verb.clone();
    let docs = docs_of(&f);
    let body = &f.block;
    let vis = &f.vis;

    let apply_fn = format_ident!("__petros_apply_{}", name);
    let db = parsed
        .ctx
        .iter()
        .find(|(_, k)| *k == Ctx::Db)
        .map(|(n, _)| n.clone())
        .expect("checked in parse");

    // The engine's parameters, bound from what `fill_auto` froze into the entry
    // and from the actor the log records.
    let ctx_binds = parsed
        .ctx
        .iter()
        .filter(|(_, k)| *k != Ctx::Db)
        .map(|(n, k)| {
            let field = n.to_string();
            match k {
                Ctx::NewId => quote! {
                    let #n: ::petros_schema::Id = ::petros_schema::cbor::field(mutation, #field)
                        .and_then(::petros_schema::cbor::as_bytes)
                        .ok_or_else(|| ::std::format!(
                            "{} has no {}; fill_auto did not run", #verb_lit, #field))?;
                },
                Ctx::Now => quote! {
                    let #n: i64 = ::petros_schema::cbor::field(mutation, #field)
                        .and_then(::petros_schema::cbor::as_int)
                        .ok_or_else(|| ::std::format!(
                            "{} has no {}; fill_auto did not run", #verb_lit, #field))?;
                },
                Ctx::Actor => quote! { let #n: &str = actor; },
                Ctx::Db => unreachable!(),
            }
        });

    let arg_binds = parsed.args.iter().map(|a| {
        let n = &a.name;
        let field = n.to_string();
        let ty = &a.ty;
        // A bad type is reported below, with a span and a message. Falling
        // back here only keeps this expansion well-formed until it is.
        let kind = schema_ty(ty).unwrap_or("Text");
        let getter = match kind {
            "Text" => quote! { ::petros_schema::cbor::opt_text(mutation, #field) },
            "Integer" => quote! {
                ::petros_schema::cbor::field(mutation, #field)
                    .and_then(::petros_schema::cbor::as_int)
                    .unwrap_or_default()
            },
            "Bool" => quote! {
                ::petros_schema::cbor::field(mutation, #field)
                    .and_then(::petros_schema::cbor::as_bool)
                    .unwrap_or_default()
            },
            _ => quote! {
                ::petros_schema::cbor::field(mutation, #field)
                    .and_then(::petros_schema::cbor::as_bytes)
                    .unwrap_or_default()
            },
        };
        quote! { let #n: #ty = #getter; }
    });

    // Type errors on arguments are worth a good message, so they are checked
    // rather than left to fail somewhere inside the expansion.
    for a in &parsed.args {
        schema_ty(&a.ty).map_err(|m| syn::Error::new_spanned(&a.ty, m))?;
    }

    let arg_names: Vec<_> = parsed.args.iter().map(|a| a.name.clone()).collect();
    let arg_tys: Vec<_> = parsed.args.iter().map(|a| a.ty.clone()).collect();
    let arg_strs: Vec<String> = arg_names.iter().map(|n| n.to_string()).collect();
    let arg_kinds: Vec<Ident> = parsed
        .args
        .iter()
        .map(|a| format_ident!("{}", schema_ty(&a.ty).unwrap()))
        .collect();

    // What `fill_auto` has to put in, and what a caller must not.
    let autos: Vec<(String, Ctx)> = parsed
        .ctx
        .iter()
        .filter(|(_, k)| matches!(k, Ctx::NewId | Ctx::Now))
        .map(|(n, k)| (n.to_string(), *k))
        .collect();
    let auto_names: Vec<String> = autos.iter().map(|(n, _)| n.clone()).collect();
    let auto_is_id: Vec<bool> = autos.iter().map(|(_, k)| *k == Ctx::NewId).collect();

    // The declaration, as the bytes the module carries. Emitted per function
    // rather than assembled centrally: the linker concatenates a section, so
    // nothing has to hold the list, and `concat!` cannot see another item's
    // const anyway.
    let mut line = verb.clone();
    for (name, kind) in arg_strs.iter().zip(arg_kinds.iter()) {
        line.push_str(&format!(" {name}:{kind}"));
    }
    line.push('\n');
    let line_len = line.len();
    let line_lit = line.clone();
    let section_ident = format_ident!("__PETROS_SCHEMA_{}", name);

    // How each argument crosses to a foreign caller. An id goes as its
    // canonical string: sixteen bytes is not a thing JavaScript holds, and
    // `from_json` parses one back.
    let ffi_tys: Vec<proc_macro2::TokenStream> = arg_kinds
        .iter()
        .map(|k| match k.to_string().as_str() {
            "Integer" => quote!(i64),
            "Bool" => quote!(bool),
            _ => quote!(::std::string::String),
        })
        .collect();

    Ok(quote! {
        #(#docs)*
        ///
        /// Authoring. Hand the result to `Client::mutate`; the body above runs
        /// on every replica, from the log, not here.
        #vis fn #name(#(#arg_names: #arg_tys),*) -> ::petros_schema::cbor::Value {
            let mut fields = ::std::vec![(
                ::petros_schema::cbor::Value::Text("t".into()),
                ::petros_schema::cbor::Value::Text(#verb_lit.into()),
            )];
            #(
                fields.push((
                    ::petros_schema::cbor::Value::Text(#arg_strs.into()),
                    ::petros_schema::IntoCbor::into_cbor(#arg_names),
                ));
            )*
            ::petros_schema::cbor::Value::Map(fields)
        }

        #[doc(hidden)]
        #[allow(non_snake_case, clippy::needless_question_mark)]
        pub fn #apply_fn<S: ::petros_schema::Store>(
            #db: &mut S,
            mutation: &::petros_schema::cbor::Value,
            actor: &str,
        ) -> ::core::result::Result<(), ::std::string::String> {
            #(#ctx_binds)*
            #(#arg_binds)*
            #body
        }

        #[doc(hidden)]
        pub mod #name {
            /// The verb as it appears in the log and in the schema.
            pub const VERB: &str = #verb_lit;
            /// This verb's line of the declaration a module carries.
            pub const LINE: &str = #line_lit;
            /// A caller's arguments, in declaration order.
            pub const ARGS: &[(&str, ::petros_schema::Ty)] =
                &[#((#arg_strs, ::petros_schema::Ty::#arg_kinds)),*];
            /// What `fill_auto` fills, and whether it is an id or a timestamp.
            pub const AUTO: &[(&str, bool)] = &[#((#auto_names, #auto_is_id)),*];
        }

        // The method a foreign caller sees. Emitted here rather than from a
        // central list because an attribute cannot see its siblings — and it
        // does not need to: uniffi accepts several exported impl blocks for one
        // object, so each function contributes its own.
        #[cfg(feature = "foreign")]
        #[uniffi::export]
        impl Peer {
            #(#docs)*
            ///
            /// Authored here and applied by every replica from the log.
            pub fn #name(
                &self,
                #(#arg_names: #ffi_tys),*
            ) -> ::core::result::Result<(), crate::PeerError> {
                self.mutate(
                    #verb_lit.to_string(),
                    ::serde_json::json!({ #(#arg_strs: #arg_names),* }).to_string(),
                )
            }
        }

        /// Wasm only: the module is self-describing so a generator that has the
        /// artifact does not also have to compile the domain to know what is in
        /// it. Linking the domain into the code generator put 0.31s on a 0.45s
        /// edit-to-device loop.
        #[cfg(target_arch = "wasm32")]
        #[link_section = "petros_schema"]
        #[used]
        // Named after the function so two of them cannot collide, which means
        // it cannot also be SHOUTING_CASE.
        #[allow(non_upper_case_globals)]
        static #section_ident: [u8; #line_len] =
            ::petros_schema::section_bytes(#name::LINE);
    })
}

/// `add_song` -> `AddSong`. The log records the verb, and the verb is what a
/// foreign caller writes, so it is spelled the way a type is.
fn camel(snake: &str) -> String {
    let mut out = String::new();
    let mut up = true;
    for c in snake.chars() {
        if c == '_' {
            up = true;
        } else if up {
            out.extend(c.to_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn docs_of(f: &ItemFn) -> Vec<proc_macro2::TokenStream> {
    f.attrs
        .iter()
        .filter(|a| a.path().is_ident("doc"))
        .map(|a| quote!(#a))
        .collect()
}

/// Declare a query: a read, run wherever it is asked, recorded nowhere.
///
/// The same shape as a mutation and for the same reason — one definition. The
/// body is kept as written, generic over the store so any peer can run it, and
/// a method on `Peer` is generated for a foreign caller.
///
/// A query takes `&mut Db` like a mutation does: SQLite advances a statement to
/// produce rows, so reading needs `&mut` as much as writing.
#[proc_macro_attribute]
pub fn query(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let f = syn::parse_macro_input!(item as ItemFn);
    match expand_query(f) {
        Ok(t) => t.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand_query(f: ItemFn) -> Result<proc_macro2::TokenStream, syn::Error> {
    let parsed = parse(&f)?;
    if parsed.ctx.iter().any(|(_, k)| *k != Ctx::Db) {
        return Err(syn::Error::new_spanned(
            &f.sig,
            "a query takes only `db: &mut Db` from the engine. `NewId` and `Now` are \
             frozen into a log entry, and a query does not write one; `Actor` is who \
             authored an entry, and a query reads every peer's.",
        ));
    }
    let name = f.sig.ident.clone();
    let docs = docs_of(&f);
    let body = &f.block;
    let vis = &f.vis;
    let ret = match &f.sig.output {
        ReturnType::Type(_, ty) => quote!(#ty),
        ReturnType::Default => {
            return Err(syn::Error::new_spanned(
                &f.sig,
                "a query returns something; that is what makes it a query",
            ))
        }
    };
    let db = parsed
        .ctx
        .iter()
        .find(|(_, k)| *k == Ctx::Db)
        .map(|(n, _)| n.clone())
        .expect("checked in parse");
    let arg_names: Vec<_> = parsed.args.iter().map(|a| a.name.clone()).collect();
    let arg_tys: Vec<_> = parsed.args.iter().map(|a| a.ty.clone()).collect();

    // A query's rows cross as the record `row!` generated for them: `Song`
    // here, `foreign::Song` there. Recognised by shape rather than by a list,
    // because there is only one shape a query returns.
    let ffi = ffi_return(&f.sig.output)?;
    let (ffi_ret, ffi_body) = match ffi {
        Rows(elem) => (
            quote!(::std::vec::Vec<crate::foreign::#elem>),
            quote!(rows.into_iter().map(::core::convert::Into::into).collect()),
        ),
        One(elem) => (quote!(crate::foreign::#elem), quote!(rows.into())),
    };

    Ok(quote! {
        // The method a foreign caller sees.
        #[cfg(feature = "foreign")]
        #[uniffi::export]
        impl Peer {
            #(#docs)*
            pub fn #name(
                &self,
                #(#arg_names: #arg_tys),*
            ) -> ::core::result::Result<#ffi_ret, crate::PeerError> {
                let rows = self.read(|db| #name(db #(, #arg_names)*))?;
                ::core::result::Result::Ok(#ffi_body)
            }
        }

        // A query never runs inside the sandbox — the module applies mutations
        // and reads nothing back — so it is built only where there is a real
        // database. Gated here rather than at the call site, because it is a
        // fact about queries and not about any one of them.
        #[cfg(feature = "storage")]
        #(#docs)*
        #vis fn #name<S: ::petros_schema::Store>(
            #db: &mut S,
            #(#arg_names: #arg_tys),*
        ) -> #ret {
            #body
        }
    })
}

/// Wire a crate's mutations together: dispatch, `fill_auto`, and the schema.
///
/// One line naming the mutations, and only the mutations — a query needs no
/// listing, because nothing dispatches to it by name.
///
/// ```ignore
/// petros::peer!(add_song, favorite, unfavorite, favorite_all, remove_song);
/// ```
///
/// Why a list at all: dispatch has to turn a verb read out of the log into a
/// call, and an attribute macro cannot see its sibling items. The alternative
/// is a registry crate resolved at link time, which is more machinery than one
/// line of names.
#[proc_macro]
pub fn peer(item: TokenStream) -> TokenStream {
    let names = syn::parse_macro_input!(item with
        syn::punctuated::Punctuated::<Ident, syn::Token![,]>::parse_terminated);
    let names: Vec<Ident> = names.into_iter().collect();
    let applies: Vec<Ident> = names
        .iter()
        .map(|n| format_ident!("__petros_apply_{}", n))
        .collect();

    quote! {
        /// Applying a mutation: `Ok`, or a deterministic refusal every replica
        /// reaches identically.
        pub fn apply<S: ::petros_schema::Store>(
            db: &mut S,
            mutation: &::petros_schema::cbor::Value,
            actor: &str,
        ) -> ::core::result::Result<(), ::std::string::String> {
            let tag = ::petros_schema::cbor::field(mutation, "t")
                .and_then(::petros_schema::cbor::as_text)
                .unwrap_or_default();
            match tag.as_str() {
                #( #names::VERB => #applies(db, mutation, actor), )*
                // A verb this build has never heard of. The log is permanent and
                // verbs are only ever added, so this is a peer newer than us.
                // Saying what this one *does* know turns "why did nothing
                // happen" into an answer.
                other => ::core::result::Result::Err(::std::format!(
                    "unknown mutation \"{}\"; this build knows {}",
                    other,
                    ::std::vec![#(#names::VERB),*].join(", ")
                )),
            }
        }

        /// Hoist the non-deterministic arguments in. Runs exactly once, at the
        /// originating client; from here the values are frozen in the log
        /// forever.
        ///
        /// Which fields a verb wants is not written here — it is what the
        /// function asked for by taking a `NewId` or a `Now`.
        pub fn fill_auto(
            mutation: &mut ::petros_schema::cbor::Value,
            uuid: ::std::vec::Vec<u8>,
            now_ms: i64,
        ) {
            let tag = ::petros_schema::cbor::field(mutation, "t")
                .and_then(::petros_schema::cbor::as_text)
                .unwrap_or_default();
            let auto: &[(&str, bool)] = match tag.as_str() {
                #( #names::VERB => #names::AUTO, )*
                _ => &[],
            };
            for (field, is_id) in auto {
                let value = if *is_id {
                    ::petros_schema::cbor::Value::Bytes(uuid.clone())
                } else {
                    ::petros_schema::cbor::Value::Integer(now_ms.into())
                };
                ::petros_schema::cbor::set(mutation, field, value);
            }
        }

        /// Every mutation a caller may author.
        pub fn schema() -> ::petros_schema::AppSchema {
            ::petros_schema::AppSchema::new([
                #(
                    #names::ARGS.iter().fold(
                        ::petros_schema::Verb::new(#names::VERB),
                        |v, (name, ty)| v.arg(*name, *ty),
                    ),
                )*
            ])
        }
    }
    .into()
}

/// What a query gives back, as far as the boundary cares.
enum Returned {
    /// `Result<Vec<Song>>` — many rows.
    Rows(Ident),
    /// `Result<Song>` — one.
    One(Ident),
}
use Returned::{One, Rows};

fn ffi_return(out: &ReturnType) -> Result<Returned, syn::Error> {
    let ReturnType::Type(_, ty) = out else {
        return Err(syn::Error::new_spanned(
            out,
            "a query returns something; that is what makes it a query",
        ));
    };
    let text = quote!(#ty).to_string().replace(' ', "");
    // Written against the text rather than the syntax tree because the only
    // shapes that mean anything here are these two, and saying so plainly beats
    // walking generics to discover it.
    let inner = text
        .strip_prefix("Result<")
        .and_then(|t| t.strip_suffix('>'))
        .ok_or_else(|| {
            syn::Error::new_spanned(ty, "a query returns `Result<…>`, so a refusal can cross")
        })?;
    let (name, many) = match inner.strip_prefix("Vec<").and_then(|t| t.strip_suffix('>')) {
        Some(elem) => (elem, true),
        None => (inner, false),
    };
    let name = name.rsplit("::").next().unwrap_or(name);
    if !name.chars().all(|c| c.is_alphanumeric() || c == '_') || name.is_empty() {
        return Err(syn::Error::new_spanned(
            ty,
            "a query returns a row type or a `Vec` of one — the record `row!` generated \
             for it is what crosses to a foreign caller",
        ));
    }
    let ident = format_ident!("{}", name);
    Ok(if many { Rows(ident) } else { One(ident) })
}
