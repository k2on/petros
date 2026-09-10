//! The client a foreign caller sees, generated.
//!
//! Almost none of it is an app's business. Opening a database, draining an
//! outbox, handing a frame to the engine, swapping a module — every Petros app
//! does those identically, and each one that wrote them out was writing the
//! same two hundred lines again.
//!
//! What an app *does* supply is the module its `apply` arrives in, and the
//! verbs and queries — and those come from the functions themselves, because
//! `#[mutation]` and `#[query]` each emit their own method.
//!
//! It has to be a macro rather than a type in this crate: UniFFI exports what
//! it can see in the crate being compiled, so the object has to be defined
//! there.

/// Generate the peer a foreign caller talks to.
///
/// ```ignore
/// petros::foreign_peer!(Peer {
///     schema: crate::schema::SCHEMA,
///     module: include_bytes!("../../../target/…/harken.wasm"),
/// });
/// ```
///
/// `module` is the build this binary shipped with. A peer with no Metro
/// attached installs it at startup; one with Metro replaces it whenever a
/// mutation changes, which is the loop the whole design exists for.
///
/// `views:` is optional and names a type implementing [`crate::Views`]. The
/// peer then keeps them beside the client, under the same lock, and brings them
/// up to date after every mutation and every message from the server. Under the
/// same lock because a view and the database it describes must not be readable
/// apart — and after *every* path because a view the app has to remember to
/// update is one it will eventually forget to.
#[macro_export]
macro_rules! foreign_peer {
    ($peer:ident {
        schema: $schema:expr,
        module: $module:expr $(,)?
    }) => {
        $crate::foreign_peer!($peer {
            schema: $schema,
            module: $module,
            views: (),
        });
    };
    ($peer:ident {
        schema: $schema:expr,
        module: $module:expr,
        views: $views:ty $(,)?
    }) => {
        /// The module this build was compiled against.
        ///
        /// Baked in rather than found at runtime: a peer with no Metro attached
        /// wants exactly this and nothing else.
        pub const BUNDLED: &[u8] = $module;

        /// One mutation, as the bytes the log stores.
        ///
        /// Wire-identical to the linked build's payload — both are
        /// `#[serde(transparent)]` over the same CBOR value — and separate only
        /// because a crate may not implement a foreign trait for a foreign
        /// type.
        #[derive(Debug, Clone, PartialEq, ::serde::Serialize, ::serde::Deserialize)]
        #[serde(transparent)]
        pub struct ForeignPayload(pub $crate::petros_schema::cbor::Value);

        impl ::core::convert::From<Payload> for ForeignPayload {
            fn from(p: Payload) -> Self {
                ForeignPayload(p.0)
            }
        }

        impl ::core::convert::From<$crate::petros_schema::cbor::Value> for ForeignPayload {
            fn from(v: $crate::petros_schema::cbor::Value) -> Self {
                ForeignPayload(v)
            }
        }

        impl $crate::Mutation for ForeignPayload {
            fn fill_auto(&mut self, ctx: &mut $crate::AutoCtx) {
                // Through the module, not the linked build: the phone's `apply`
                // is the module's, and so is its idea of what needs filling.
                let mut bytes = ::std::vec::Vec::new();
                if ::ciborium::into_writer(&self.0, &mut bytes).is_err() {
                    return;
                }
                let filled = ::petros_wasm_host::MUTATORS
                    .read()
                    .ok()
                    .and_then(|slot| {
                        slot.as_ref()
                            .and_then(|m| m.fill_auto(&bytes, ctx).ok())
                    });
                if let ::core::option::Option::Some(bytes) = filled {
                    if let ::core::result::Result::Ok(v) =
                        ::ciborium::from_reader(&bytes[..])
                    {
                        self.0 = v;
                    }
                }
            }

            fn apply(
                &self,
                tx: &mut $crate::Transaction,
                actor: &$crate::ActorId,
            ) -> ::core::result::Result<(), $crate::MutationError> {
                let mut bytes = ::std::vec::Vec::new();
                ::ciborium::into_writer(&self.0, &mut bytes)
                    .map_err(|e| $crate::MutationError::rejected(e.to_string()))?;
                let slot = ::petros_wasm_host::MUTATORS
                    .read()
                    .map_err(|_| $crate::MutationError::rejected(
                        "the mutator lock was poisoned by an earlier panic",
                    ))?;
                let module = slot.as_ref().ok_or_else(|| {
                    $crate::MutationError::rejected(
                        "no mutator module is loaded; the app must install one before mutating",
                    )
                })?;
                match module.apply(tx.conn(), &bytes, actor.as_str()) {
                    // The module's writes went through the host's store, which
                    // is created per request and dropped — so the changes come
                    // back from the apply and are reported here, exactly as a
                    // linked `apply` reports its own. A phone maintains a view
                    // the same way a desktop does.
                    ::core::result::Result::Ok(::core::result::Result::Ok(changes)) => {
                        tx.record(changes);
                        ::core::result::Result::Ok(())
                    }
                    ::core::result::Result::Ok(::core::result::Result::Err(reason)) => {
                        ::core::result::Result::Err($crate::MutationError::rejected(reason))
                    }
                    ::core::result::Result::Err(e) => {
                        ::core::result::Result::Err($crate::MutationError::rejected(e))
                    }
                }
            }
        }

        /// The app whose `apply` is a wasm module — the phone's, and only the
        /// phone's. Every other peer links it.
        #[derive(Debug)]
        pub struct ForeignApp;

        impl $crate::App for ForeignApp {
            type Mutation = ForeignPayload;
            // One schema. Migrations are the one thing that should not arrive
            // over the air, so they stay where every peer can see them.
            const SCHEMA: &'static str = $schema;
        }

        /// A mutation the server refused. Not a failure: a deterministic
        /// verdict every replica would have reached identically.
        #[derive(Debug, Clone, uniffi::Record)]
        pub struct Rejection {
            pub id: ::std::string::String,
            pub reason: ::std::string::String,
        }

        /// What can go wrong, as a foreign caller sees it.
        #[derive(Debug, ::thiserror::Error, uniffi::Error)]
        #[uniffi(flat_error)]
        pub enum PeerError {
            /// The app itself said no. Show it to a person; retrying it
            /// unchanged will fail the same way.
            #[error("{reason}")]
            Refused { reason: ::std::string::String },
            /// Anything else: a broken database, a frame that will not decode.
            /// A bug or a broken environment, not a judgement.
            #[error("{message}")]
            Engine { message: ::std::string::String },
        }

        impl ::core::convert::From<$crate::Error> for PeerError {
            fn from(e: $crate::Error) -> Self {
                match e {
                    $crate::Error::Mutation($crate::MutationError::Rejected(reason)) => {
                        PeerError::Refused { reason }
                    }
                    other => PeerError::Engine {
                        message: other.to_string(),
                    },
                }
            }
        }

        /// A query's refusal, and anything else that arrives as a sentence.
        impl ::core::convert::From<::std::string::String> for PeerError {
            fn from(message: ::std::string::String) -> Self {
                PeerError::Engine { message }
            }
        }

        /// A peer, for a foreign caller.
        ///
        /// The client owns a SQLite connection, which is `Send` but not `Sync`,
        /// and a read needs `&mut` like a write does — so every method takes the
        /// lock. That is not a concession to the boundary: a linked client
        /// serialises access the same way, because the optimistic savepoint
        /// means there is only ever one coherent view to read.
        #[derive(uniffi::Object)]
        pub struct $peer {
            inner: ::std::sync::Mutex<PeerInner>,
        }

        /// The client and whatever this app maintains beside it, together
        /// because they describe the same database and must not be readable
        /// apart.
        #[doc(hidden)]
        pub struct PeerInner {
            client: $crate::Client<ForeignApp>,
            views: $views,
        }

        impl $peer {
            #[doc(hidden)]
            pub fn with<T>(
                &self,
                f: impl FnOnce(&mut $crate::Client<ForeignApp>) -> ::core::result::Result<T, PeerError>,
            ) -> ::core::result::Result<T, PeerError> {
                let mut guard = self.inner.lock().map_err(|_| PeerError::Engine {
                    message: "the client lock was poisoned by an earlier panic".into(),
                })?;
                f(&mut guard.client)
            }

            /// Read what this peer maintains, after settling it.
            #[doc(hidden)]
            pub fn views<T>(
                &self,
                f: impl FnOnce(&mut $views) -> ::core::result::Result<T, PeerError>,
            ) -> ::core::result::Result<T, PeerError> {
                let mut guard = self.inner.lock().map_err(|_| PeerError::Engine {
                    message: "the client lock was poisoned by an earlier panic".into(),
                })?;
                let inner = &mut *guard;
                $crate::settle(&mut inner.client, &mut inner.views);
                f(&mut inner.views)
            }

            /// Bring the views up to date. Called after anything that can move
            /// the database, so an app cannot forget one.
            fn settle(&self) {
                if let ::core::result::Result::Ok(mut guard) = self.inner.lock() {
                    let inner = &mut *guard;
                    $crate::settle(&mut inner.client, &mut inner.views);
                }
            }

            /// Run a mutation authored by one of this app's functions.
            #[doc(hidden)]
            pub fn run(
                &self,
                m: $crate::petros_schema::cbor::Value,
            ) -> ::core::result::Result<(), PeerError> {
                let outcome = self.with(|c| {
                    c.mutate(ForeignPayload(m))?;
                    ::core::result::Result::Ok(())
                });
                // Even on a refusal: a rejected mutation is rolled back, and
                // nothing to settle is what `settle` does with an empty list.
                // Settling only on success would leave the one path that
                // matters — a refusal after other work — to be noticed later.
                self.settle();
                outcome
            }

            /// Run a query against this peer's database.
            #[doc(hidden)]
            pub fn read<T>(
                &self,
                f: impl FnOnce(
                    &mut $crate::backend::SqliteStore<'_>,
                ) -> ::core::result::Result<T, ::std::string::String>,
            ) -> ::core::result::Result<T, PeerError> {
                self.with(|c| ::core::result::Result::Ok(f(&mut c.store())?))
            }
        }

        #[uniffi::export]
        impl $peer {
            /// Open the peer's database, running Petros's migrations and the
            /// app's.
            #[uniffi::constructor]
            pub fn open(
                db_path: ::std::string::String,
                actor: ::std::string::String,
            ) -> ::core::result::Result<Self, PeerError> {
                let conn = $crate::open_path(&db_path)?;
                let mut client =
                    $crate::Client::<ForeignApp>::open(conn, actor, $crate::AutoCtx::system())?;
                // One full read, here and nowhere else. Everything after this
                // is maintained.
                let mut views = <$views as $crate::Views>::build();
                $crate::Views::hydrate(&mut views, &mut client.store());
                let _ = client.take_changes();
                ::core::result::Result::Ok($peer {
                    inner: ::std::sync::Mutex::new(PeerInner { client, views }),
                })
            }

            /// Author any mutation the loaded module understands.
            ///
            /// The one entry point that does not grow when the domain does.
            /// `kind` is a verb and `args` a JSON object of its fields; the
            /// module decides what both mean. Adding a verb is a module rebuild
            /// and a call site, neither of which needs a native build — which is
            /// the whole reason the domain is a module rather than a symbol.
            ///
            /// The named methods are conveniences over exactly this, generated
            /// from the functions themselves.
            pub fn mutate(
                &self,
                kind: ::std::string::String,
                args: ::std::string::String,
            ) -> ::core::result::Result<(), PeerError> {
                let payload = from_json(&kind, &args)
                    .map_err(|reason| PeerError::Refused { reason })?;
                self.run(payload.0)
            }

            /// How much of the server's log this peer has applied.
            pub fn cursor(&self) -> u64 {
                self.with(|c| ::core::result::Result::Ok(c.cursor())).unwrap_or(0)
            }

            /// What this peer has done that no server has confirmed.
            pub fn pending_len(&self) -> u32 {
                self.with(|c| ::core::result::Result::Ok(c.pending_len() as u32))
                    .unwrap_or(0)
            }

            /// Install a module, replacing whatever was running. Returns the
            /// generation, which moves on every successful swap.
            pub fn load_mutators(&self, wasm: ::std::vec::Vec<u8>)
                -> ::core::result::Result<u64, PeerError>
            {
                ::petros_wasm_host::load(&wasm).map_err(|message| PeerError::Engine { message })
            }

            /// Which module is running, or zero if none has been installed.
            pub fn mutators_generation(&self) -> u64 {
                ::petros_wasm_host::generation()
            }

            /// Ask for everything since the cursor, and re-offer everything
            /// pending.
            pub fn connected(&self) -> ::core::result::Result<(), PeerError> {
                self.with(|c| ::core::result::Result::Ok(c.connected()?))
            }

            /// Drain the outbox as encoded frames, ready for a socket.
            pub fn take_outgoing(&self)
                -> ::core::result::Result<::std::vec::Vec<::std::vec::Vec<u8>>, PeerError>
            {
                self.with(|c| {
                    ::core::result::Result::Ok(
                        c.take_outgoing()
                            .iter()
                            .map($crate::encode)
                            .collect::<::core::result::Result<_, _>>()?,
                    )
                })
            }

            /// Hand one frame from the server to the engine.
            pub fn recv(&self, frame: ::std::vec::Vec<u8>)
                -> ::core::result::Result<(), PeerError>
            {
                let msg: $crate::ServerMsg<ForeignPayload> = $crate::decode(&frame)?;
                let outcome = self.with(|c| ::core::result::Result::Ok(c.recv(msg)?));
                // The server's messages are the other thing that moves the
                // database, and the one that produces a rebase.
                self.settle();
                outcome
            }

            /// Mutations the server refused since the last call.
            pub fn take_rejections(&self) -> ::std::vec::Vec<Rejection> {
                self.with(|c| {
                    ::core::result::Result::Ok(
                        c.take_rejections()
                            .into_iter()
                            .map(|r| Rejection {
                                id: r.id.to_string(),
                                reason: r.reason,
                            })
                            .collect(),
                    )
                })
                .unwrap_or_default()
            }
        }
    };
}
