//! Running the domain from a `.wasm` module instead of from a linked symbol.
//!
//! The point is that `apply` becomes a file the peer loads rather than a
//! function it was compiled with. That is what makes a saved Rust file reach a
//! running phone in about a second — Metro pushes the new module the way it
//! pushes any other changed asset — and it is what would let an old client
//! replay a mutation written after it was built.
//!
//! Two things fall out of the sandbox for free. `apply` cannot read a clock or
//! a random number generator, because the host imports `query` and `exec` and
//! nothing else; `CLAUDE.md` currently has to ask for that in prose. And a
//! module that traps takes its mutation down with it, not the process.

use std::sync::Arc;

use diesel::connection::SimpleConnection;
use diesel::deserialize::QueryableByName;
use diesel::sql_types::BigInt;
use diesel::{sql_query, RunQueryDsl};
use exo::{AutoCtx, Connection};
use wasmi::{Caller, Engine, Linker, Memory, Module, Store, TypedFunc};

/// How much stack one mutation gets.
///
/// This is not a guess. A host import runs *inside* wasmi's execution loop, and
/// measurement shows those frames accumulate for as long as the guest function
/// runs rather than unwinding between calls — so the stack a mutation needs
/// scales with how many times it calls `query_int`, `query_exists` or `exec`,
/// not with how deep any one of them goes.
///
/// The constant that matters is the frame size, and it is dominated by Diesel's
/// monomorphised query machinery: roughly a megabyte per call unoptimised, much
/// less in release. `Add` makes three calls and fit in 8MB either way;
/// `AddFive` makes eleven and overflowed 8MB in a test build while passing in
/// release. That gap is the whole reason this is 64MB now: a limit only a debug
/// build trips is a limit that will be tripped by whoever writes the next
/// mutation, on the machine where it is hardest to diagnose.
///
/// It costs nothing to be generous. This is reserved address space; only pages
/// actually touched become resident.
const MUTATOR_STACK: usize = 64 * 1024 * 1024;

/// The compiled module, shared by every call. Compilation is the expensive part
/// of running wasm; instantiation is not, so this is what gets cached and what
/// [`Mutators::swap`] replaces.
pub struct Mutators {
    engine: Engine,
    module: Arc<Module>,
    /// Bumped on every swap, so a caller can tell which module produced a row.
    pub generation: u64,
}

/// What a host function can reach while the guest is running: the very
/// connection the engine has a transaction open on.
struct HostState {
    /// Valid only for the duration of one call, and absent for `fill_auto`,
    /// which touches no database. The `Store` never outlives the borrow that
    /// produced this, which is what makes it sound.
    conn: Option<*mut Connection>,
    memory: Option<Memory>,
    alloc: Option<TypedFunc<u32, u32>>,
    failure: Option<String>,
}

impl HostState {
    fn conn(&mut self) -> Result<&mut Connection, String> {
        // SAFETY: set from a live `&mut Connection` in `run`, and the Store is
        // dropped before that borrow ends.
        match self.conn {
            Some(conn) => Ok(unsafe { &mut *conn }),
            None => Err("this entry point has no database".into()),
        }
    }
}

impl std::fmt::Debug for Mutators {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mutators")
            .field("generation", &self.generation)
            .finish_non_exhaustive()
    }
}

impl Mutators {
    /// Compile a module. Rejects one that does not export the ABI, so a bad
    /// push fails here rather than at the first mutation.
    pub fn load(bytes: &[u8]) -> Result<Self, String> {
        let engine = Engine::default();
        let module = Module::new(&engine, bytes).map_err(|e| format!("not a wasm module: {e}"))?;
        let names: Vec<&str> = module.exports().map(|e| e.name()).collect();
        for required in ["exo_alloc", "exo_apply", "exo_fill_auto", "memory"] {
            if !names.contains(&required) {
                return Err(format!("the module does not export `{required}`"));
            }
        }
        Ok(Mutators {
            engine,
            module: Arc::new(module),
            generation: 1,
        })
    }

    /// Replace the module in place, keeping the generation counter moving.
    pub fn swap(&mut self, bytes: &[u8]) -> Result<(), String> {
        let next = Mutators::load(bytes)?;
        let generation = self.generation + 1;
        *self = next;
        self.generation = generation;
        Ok(())
    }

    /// Instantiate and call one exported function, on a stack of our own.
    ///
    /// A fresh `Store` per call, because a `Store` owns the guest's linear
    /// memory and one mutation must not see the leftovers of the last. It costs
    /// a memory allocation, which is cheap next to the SQL underneath it.
    ///
    /// The thread is not optional. A host import runs *inside* wasmi's
    /// execution loop, so its frames sit on top of the interpreter's, and this
    /// one then calls Diesel — whose query machinery is many layers of deeply
    /// nested generics. Together they overflowed a default 2MB thread stack in
    /// the tests, and iOS gives React Native's JS thread about half of that. So
    /// the mutation gets a stack sized for the job rather than borrowing
    /// whichever one happened to call in.
    fn run<R: Send>(
        &self,
        conn: Option<&mut Connection>,
        call: impl FnOnce(&mut Store<HostState>, &wasmi::Instance) -> Result<R, String> + Send,
    ) -> Result<R, String> {
        std::thread::scope(|scope| {
            std::thread::Builder::new()
                .name("exo-mutator".into())
                .stack_size(MUTATOR_STACK)
                .spawn_scoped(scope, || self.run_here(conn, call))
                .map_err(|e| format!("could not start the mutator thread: {e}"))?
                .join()
                // A guest that traps is caught by wasmi and reported; a panic
                // here is ours, and losing the thread is better than losing the
                // process with a transaction open.
                .map_err(|_| "the mutator thread panicked".to_string())?
        })
    }

    fn run_here<R>(
        &self,
        conn: Option<&mut Connection>,
        call: impl FnOnce(&mut Store<HostState>, &wasmi::Instance) -> Result<R, String>,
    ) -> Result<R, String> {
        let mut store = Store::new(
            &self.engine,
            HostState {
                conn: conn.map(|c| c as *mut Connection),
                memory: None,
                alloc: None,
                failure: None,
            },
        );
        let mut linker = Linker::new(&self.engine);
        linker
            .func_wrap("exo", "query_int", host_query_int)
            .and_then(|l| l.func_wrap("exo", "query_exists", host_query_exists))
            .and_then(|l| l.func_wrap("exo", "exec", host_exec))
            .map_err(|e| format!("could not define the host imports: {e}"))?;

        let instance = linker
            .instantiate_and_start(&mut store, &self.module)
            .map_err(|e| format!("could not instantiate: {e}"))?;

        let memory = instance
            .get_memory(&store, "memory")
            .ok_or("the module exports no memory")?;
        let alloc = instance
            .get_typed_func::<u32, u32>(&store, "exo_alloc")
            .map_err(|e| format!("exo_alloc has the wrong shape: {e}"))?;
        store.data_mut().memory = Some(memory);
        store.data_mut().alloc = Some(alloc);

        let out = call(&mut store, &instance)?;
        match store.data_mut().failure.take() {
            Some(e) => Err(e),
            None => Ok(out),
        }
    }

    /// Hoist the non-deterministic arguments in, exactly once, at the
    /// originating client. The host supplies the values; the module decides
    /// where they belong, so that knowledge stays in one place.
    pub fn fill_auto(&self, payload: &[u8], auto: &mut AutoCtx) -> Result<Vec<u8>, String> {
        let uuid = auto.uuid().as_uuid().as_bytes().to_vec();
        let now = auto.now_ms();
        self.fill_auto_with(payload, &uuid, now)
    }

    /// As [`fill_auto`](Self::fill_auto), with the two non-deterministic values
    /// supplied rather than drawn. What a conformance test needs: the seed is
    /// the input, not the thing under comparison.
    pub fn fill_auto_with(&self, payload: &[u8], uuid: &[u8], now: i64) -> Result<Vec<u8>, String> {
        let uuid = uuid.to_vec();
        self.run(None, |store, instance| {
            let p = write_bytes(store, payload)?;
            let u = write_bytes(store, &uuid)?;
            let f = instance
                .get_typed_func::<(u32, u32, u32, i64), u64>(&*store, "exo_fill_auto")
                .map_err(|e| format!("exo_fill_auto has the wrong shape: {e}"))?;
            let packed = f
                .call(&mut *store, (p.0, p.1, u.0, now))
                .map_err(|e| format!("exo_fill_auto trapped: {e}"))?;
            read_packed(store, packed)
        })
    }

    /// Apply one mutation, as the payload the log stores.
    pub fn apply(
        &self,
        conn: &mut Connection,
        payload: &[u8],
        actor: &str,
    ) -> Result<Result<(), String>, String> {
        self.run(Some(conn), |store, instance| {
            let p = write_bytes(store, payload)?;
            let a = write_bytes(store, actor.as_bytes())?;
            let f = instance
                .get_typed_func::<(u32, u32, u32, u32), u64>(&*store, "exo_apply")
                .map_err(|e| format!("exo_apply has the wrong shape: {e}"))?;
            let packed = f
                .call(&mut *store, (p.0, p.1, a.0, a.1))
                .map_err(|e| format!("exo_apply trapped: {e}"))?;
            if packed == 0 {
                return Ok(Ok(()));
            }
            let reason = read_packed(store, packed)?;
            Ok(Err(String::from_utf8_lossy(&reason).into_owned()))
        })
    }
}

// ------------------------------------------------------------ memory plumbing

/// Ask the guest for a buffer and fill it. Everything crossing the boundary
/// goes through here, in both directions.
fn write_bytes(store: &mut Store<HostState>, bytes: &[u8]) -> Result<(u32, u32), String> {
    let alloc = store.data().alloc.ok_or("no exo_alloc yet")?;
    let memory = store.data().memory.ok_or("no memory yet")?;
    let ptr = alloc
        .call(&mut *store, bytes.len() as u32)
        .map_err(|e| format!("exo_alloc trapped: {e}"))?;
    memory
        .write(&mut *store, ptr as usize, bytes)
        .map_err(|e| format!("could not write guest memory: {e}"))?;
    Ok((ptr, bytes.len() as u32))
}

/// Read a `(ptr << 32) | len` pair out of guest memory.
fn read_packed(store: &mut Store<HostState>, packed: u64) -> Result<Vec<u8>, String> {
    let ptr = (packed >> 32) as usize;
    let len = (packed & 0xffff_ffff) as usize;
    if len == 0 {
        return Ok(Vec::new());
    }
    let memory = store.data().memory.ok_or("no memory yet")?;
    let mut out = vec![0u8; len];
    memory
        .read(&*store, ptr, &mut out)
        .map_err(|e| format!("could not read guest memory: {e}"))?;
    Ok(out)
}

// ------------------------------------------------------------- host functions

/// One integer column. Diesel needs a named type even for a scalar.
#[derive(QueryableByName)]
struct IntRow {
    #[diesel(sql_type = BigInt, column_name = v)]
    v: i64,
}

fn read_sql(caller: &Caller<'_, HostState>, ptr: u32, len: u32) -> Result<String, String> {
    let memory = caller.data().memory.ok_or("no memory yet")?;
    let mut out = vec![0u8; len as usize];
    memory
        .read(caller, ptr as usize, &mut out)
        .map_err(|e| format!("could not read guest memory: {e}"))?;
    String::from_utf8(out).map_err(|e| format!("the guest sent invalid utf-8: {e}"))
}

fn host_query_int(mut caller: Caller<'_, HostState>, sql: u32, sql_len: u32) -> i64 {
    let outcome = read_sql(&caller, sql, sql_len).and_then(|sql| {
        let conn = caller.data_mut().conn()?;
        sql_query(format!("SELECT ({sql}) AS v"))
            .load::<IntRow>(conn)
            .map(|rows| rows.first().map(|r| r.v).unwrap_or(0))
            .map_err(|e| format!("{sql}: {e}"))
    });
    match outcome {
        Ok(v) => v,
        Err(e) => {
            caller.data_mut().failure.get_or_insert(e);
            0
        }
    }
}

fn host_query_exists(mut caller: Caller<'_, HostState>, sql: u32, sql_len: u32) -> i32 {
    let outcome = read_sql(&caller, sql, sql_len).and_then(|sql| {
        let conn = caller.data_mut().conn()?;
        sql_query(format!("SELECT EXISTS({sql}) AS v"))
            .load::<IntRow>(conn)
            .map(|rows| rows.first().map(|r| r.v).unwrap_or(0) as i32)
            .map_err(|e| format!("{sql}: {e}"))
    });
    match outcome {
        Ok(v) => v,
        Err(e) => {
            caller.data_mut().failure.get_or_insert(e);
            0
        }
    }
}

fn host_exec(mut caller: Caller<'_, HostState>, sql: u32, sql_len: u32) -> i64 {
    let outcome = read_sql(&caller, sql, sql_len).and_then(|sql| {
        let conn = caller.data_mut().conn()?;
        // `batch_execute` rather than `sql_query(..).execute(..)`: this frame
        // sits on top of wasmi's, and those frames accumulate for as long as
        // the guest function runs, so every kilobyte here is paid once per host
        // call a mutation makes. Diesel's typed query machinery is many layers
        // of monomorphised generics; the simple path is a fraction of it, and
        // nothing here wants the row count anyway.
        conn.batch_execute(&sql)
            .map(|()| 0i64)
            .map_err(|e| format!("{sql}: {e}"))
    });
    match outcome {
        Ok(n) => n,
        Err(e) => {
            caller.data_mut().failure.get_or_insert(e);
            -1
        }
    }
}
