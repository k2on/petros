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

use petros::backend::SqliteStore;
use petros::{AutoCtx, Connection};
// `as _` because wasmi also has a `Store`, and this one is only wanted for
// its methods.
use petros_schema::{Request, Store as _};
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
    /// Kept so `swap` can build the next worker from the same engine.
    #[allow(dead_code)]
    engine: Engine,
    #[allow(dead_code)]
    module: Arc<Module>,
    /// The thread and the instance every call runs on, kept alive.
    worker: std::sync::Mutex<Worker>,
    /// Bumped on every swap, so a caller can tell which module produced a row.
    pub generation: u64,
}

/// What a host function can reach while the guest is running: the very
/// connection the engine has a transaction open on.
#[derive(Default)]
struct HostState {
    /// Valid only for the duration of one call, and absent for `fill_auto`,
    /// which touches no database. The `Store` never outlives the borrow that
    /// produced this, which is what makes it sound.
    conn: Option<*mut Connection>,
    memory: Option<Memory>,
    alloc: Option<TypedFunc<u32, u32>>,
    failure: Option<String>,
    /// The answer to the guest's last question, waiting to be copied out.
    ///
    /// A host function cannot allocate in the guest without re-entering an
    /// instance that is already running, so the answer waits here and the guest
    /// asks for it once it has made room.
    answer: Vec<u8>,
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

/// Build the instance a worker will keep, and remember the two exports every
/// call needs.
fn instantiate(
    engine: &Engine,
    module: &Module,
    store: &mut Store<HostState>,
) -> Result<wasmi::Instance, String> {
    let mut linker = Linker::new(engine);
    linker
        .func_wrap("petros", "store", host_store)
        .and_then(|l| l.func_wrap("petros", "take", host_take))
        .map_err(|e| format!("could not define the host imports: {e}"))?;

    let instance = linker
        .instantiate_and_start(&mut *store, module)
        .map_err(|e| format!("could not instantiate: {e}"))?;

    let memory = instance
        .get_memory(&*store, "memory")
        .ok_or("the module exports no memory")?;
    let alloc = instance
        .get_typed_func::<u32, u32>(&*store, "petros_alloc")
        .map_err(|e| format!("petros_alloc has the wrong shape: {e}"))?;
    store.data_mut().memory = Some(memory);
    store.data_mut().alloc = Some(alloc);
    Ok(instance)
}

/// A raw `&mut Connection` on its way to the worker.
///
/// The caller blocks on the reply for the whole call, so the borrow it came
/// from is live and exclusive throughout — the same argument the scoped thread
/// used to make structurally, now made by hand because the thread outlives any
/// one call.
struct Borrowed(Option<*mut Connection>);

// SAFETY: only ever moved to the worker while the caller that produced the
// borrow is blocked, and cleared before the reply is sent.
unsafe impl Send for Borrowed {}

/// One thread, one instance, both alive for the life of the module.
///
/// The thread is not optional. A host import runs *inside* wasmi's execution
/// loop, so its frames sit on top of the interpreter's, and this one then calls
/// Diesel — whose query machinery is many layers of deeply nested generics.
/// Together they overflowed a default 2MB thread stack in the tests, and iOS
/// gives React Native's JS thread about half of that. So a mutation gets a stack
/// sized for the job rather than borrowing whichever one happened to call in.
struct Worker {
    jobs: std::sync::mpsc::Sender<Job>,
}

type Job = Box<dyn FnOnce(&mut Store<HostState>, &wasmi::Instance) + Send>;
/// The same job before it is promised to live as long as the channel wants.
type BorrowedJob<'a> = Box<dyn FnOnce(&mut Store<HostState>, &wasmi::Instance) + Send + 'a>;

impl Worker {
    fn start(engine: Engine, module: Arc<Module>) -> Result<Worker, String> {
        let (jobs, rx) = std::sync::mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
        std::thread::Builder::new()
            .name("petros-mutator".into())
            .stack_size(MUTATOR_STACK)
            .spawn(move || {
                let mut store = Store::new(&engine, HostState::default());
                match instantiate(&engine, &module, &mut store) {
                    Ok(instance) => {
                        let _ = ready_tx.send(Ok(()));
                        // Ends when the sender drops, which is when the module
                        // is swapped or the process goes away.
                        while let Ok(job) = rx.recv() {
                            job(&mut store, &instance);
                        }
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                    }
                }
            })
            .map_err(|e| format!("could not start the mutator thread: {e}"))?;
        ready_rx
            .recv()
            .map_err(|_| "the mutator thread died before it was ready".to_string())??;
        Ok(Worker { jobs })
    }

    fn run<R: Send>(
        &self,
        conn: Option<&mut Connection>,
        call: impl FnOnce(&mut Store<HostState>, &wasmi::Instance) -> Result<R, String> + Send,
    ) -> Result<R, String> {
        let borrowed = Borrowed(conn.map(|c| c as *mut Connection));
        let (tx, rx) = std::sync::mpsc::channel();

        // Boxed with a borrowed lifetime first, because that is what the
        // closure honestly is.
        let job: BorrowedJob<'_> = Box::new(move |store, instance| {
            let borrowed = borrowed;
            store.data_mut().conn = borrowed.0;
            store.data_mut().failure = None;
            let out = call(store, instance);
            store.data_mut().conn = None;
            // A host function that failed reports through the store rather
            // than through the guest, which only knows it got no answer.
            let out = match store.data_mut().failure.take() {
                Some(e) => Err(e),
                None => out,
            };
            let _ = tx.send(out);
        });
        // SAFETY: the job runs to completion before `rx.recv()` below returns,
        // so nothing it borrows outlives this call. `'static` is what a channel
        // demands and what the scoped thread used to provide structurally.
        let job: Job = unsafe { std::mem::transmute::<BorrowedJob<'_>, Job>(job) };

        self.jobs
            .send(job)
            .map_err(|_| "the mutator thread is gone".to_string())?;
        rx.recv()
            // A guest that traps is caught by wasmi and reported; a panic here
            // is ours, and losing the thread is better than losing the process
            // with a transaction open.
            .map_err(|_| "the mutator thread panicked".to_string())?
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
        for required in [
            "petros_alloc",
            "petros_free",
            "petros_apply",
            "petros_fill_auto",
            "memory",
        ] {
            if !names.contains(&required) {
                return Err(format!("the module does not export `{required}`"));
            }
        }
        let module = Arc::new(module);
        let worker = Worker::start(engine.clone(), module.clone())?;
        Ok(Mutators {
            engine,
            module,
            worker: std::sync::Mutex::new(worker),
            generation: 1,
        })
    }

    /// Replace the module in place, keeping the generation counter moving.
    ///
    /// The old worker's thread ends when its sender drops with the old
    /// `Mutators`, so a swap is also how the instance is rebuilt — which is
    /// where the guest's memory is reclaimed wholesale if it ever needed to be.
    pub fn swap(&mut self, bytes: &[u8]) -> Result<(), String> {
        let next = Mutators::load(bytes)?;
        let generation = self.generation + 1;
        *self = next;
        self.generation = generation;
        Ok(())
    }

    /// Call one exported function on the worker's stack and instance.
    ///
    /// Both are kept alive. It used to build a thread and an instance per call,
    /// which was defensible when the guest leaked every buffer — a fresh
    /// instance was how the leak was collected. The guest frees now, so the
    /// instance can live, and on a phone that mattered: replaying forty pending
    /// entries meant forty 64MB thread spawns and forty instantiations, and was
    /// most of a 390ms tap.
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
        let worker = self
            .worker
            .lock()
            .map_err(|_| "the mutator worker was poisoned by an earlier panic".to_string())?;
        worker.run(conn, call)
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
                .get_typed_func::<(u32, u32, u32, i64), u64>(&*store, "petros_fill_auto")
                .map_err(|e| format!("petros_fill_auto has the wrong shape: {e}"))?;
            let packed = f
                .call(&mut *store, (p.0, p.1, u.0, now))
                .map_err(|e| format!("petros_fill_auto trapped: {e}"))?;
            let out = read_packed(store, packed);
            free_bytes(store, instance, p.0, p.1)?;
            free_bytes(store, instance, u.0, u.1)?;
            free_bytes(store, instance, (packed >> 32) as u32, packed as u32)?;
            out
        })
    }

    /// How many pages of linear memory the guest is holding.
    ///
    /// For the test that keeps instance reuse honest: the guest frees what it
    /// is given, so this must not climb with the number of calls.
    pub fn memory_pages(&self) -> u32 {
        self.run(None, |store, _| {
            let memory = store.data().memory.ok_or("no memory yet")?;
            Ok(memory.size(&*store) as u32)
        })
        .unwrap_or(0)
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
                .get_typed_func::<(u32, u32, u32, u32), u64>(&*store, "petros_apply")
                .map_err(|e| format!("petros_apply has the wrong shape: {e}"))?;
            let packed = f
                .call(&mut *store, (p.0, p.1, a.0, a.1))
                .map_err(|e| format!("petros_apply trapped: {e}"))?;
            free_bytes(store, instance, p.0, p.1)?;
            free_bytes(store, instance, a.0, a.1)?;
            if packed == 0 {
                return Ok(Ok(()));
            }
            let reason = read_packed(store, packed)?;
            free_bytes(store, instance, (packed >> 32) as u32, packed as u32)?;
            Ok(Err(String::from_utf8_lossy(&reason).into_owned()))
        })
    }
}

// ------------------------------------------------------------ memory plumbing

/// Ask the guest for a buffer and fill it. Everything crossing the boundary
/// goes through here, in both directions.
fn write_bytes(store: &mut Store<HostState>, bytes: &[u8]) -> Result<(u32, u32), String> {
    let alloc = store.data().alloc.ok_or("no petros_alloc yet")?;
    let memory = store.data().memory.ok_or("no memory yet")?;
    let ptr = alloc
        .call(&mut *store, bytes.len() as u32)
        .map_err(|e| format!("petros_alloc trapped: {e}"))?;
    memory
        .write(&mut *store, ptr as usize, bytes)
        .map_err(|e| format!("could not write guest memory: {e}"))?;
    Ok((ptr, bytes.len() as u32))
}

/// Read a `(ptr << 32) | len` pair out of guest memory.
/// Give a buffer back to the guest.
///
/// Every `write_bytes` and every packed return has to be matched by one of
/// these, or a reused instance grows without bound — which is what a fresh
/// instance per call used to hide.
fn free_bytes(
    store: &mut Store<HostState>,
    instance: &wasmi::Instance,
    ptr: u32,
    len: u32,
) -> Result<(), String> {
    if len == 0 {
        return Ok(());
    }
    let f = instance
        .get_typed_func::<(u32, u32), ()>(&*store, "petros_free")
        .map_err(|e| format!("petros_free has the wrong shape: {e}"))?;
    f.call(&mut *store, (ptr, len))
        .map_err(|e| format!("petros_free trapped: {e}"))
}

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

/// Answer one question from the guest, and say how long the answer is.
///
/// The SQL inside was checked at build time by `petros-sql`, so there is
/// nothing to validate here — only to bind and run. A request that will not
/// decode is a broken module, and it fails the mutation rather than the
/// process.
fn host_store(mut caller: Caller<'_, HostState>, request: u32, len: u32) -> u32 {
    let outcome = read_bytes(&caller, request, len).and_then(|bytes| {
        let request: Request = ciborium::from_reader(&bytes[..])
            .map_err(|e| format!("the module sent a request we cannot read: {e}"))?;
        let conn = caller.data_mut().conn()?;
        let mut store = SqliteStore::new(conn);
        let encode = |rows: Vec<Vec<petros_schema::Value>>| -> Result<Vec<u8>, String> {
            let mut out = Vec::new();
            ciborium::into_writer(&rows, &mut out)
                .map_err(|e| format!("could not encode the rows: {e}"))?;
            Ok(out)
        };
        Ok(match request {
            Request::Query { sql, params, types } => encode(store.query(&sql, &params, &types))?,
            Request::Get { table, key } => {
                encode(store.get_row(&table, &key).into_iter().collect())?
            }
            Request::Put { table, row } => {
                store.put_row(&table, &row);
                Vec::new()
            }
            Request::Delete { table, key } => {
                store.delete_row(&table, &key);
                Vec::new()
            }
        })
    });
    match outcome {
        Ok(answer) => {
            let len = answer.len() as u32;
            caller.data_mut().answer = answer;
            len
        }
        Err(e) => {
            caller.data_mut().failure.get_or_insert(e);
            0
        }
    }
}

/// Copy the waiting answer into a buffer the guest has just made.
fn host_take(mut caller: Caller<'_, HostState>, into: u32, len: u32) {
    let answer = std::mem::take(&mut caller.data_mut().answer);
    if answer.len() != len as usize {
        caller
            .data_mut()
            .failure
            .get_or_insert_with(|| "the module asked for the wrong number of bytes".into());
        return;
    }
    let Some(memory) = caller.data().memory else {
        return;
    };
    if let Err(e) = memory.write(&mut caller, into as usize, &answer) {
        caller
            .data_mut()
            .failure
            .get_or_insert(format!("could not write guest memory: {e}"));
    }
}

fn read_bytes(caller: &Caller<'_, HostState>, ptr: u32, len: u32) -> Result<Vec<u8>, String> {
    let memory = caller.data().memory.ok_or("no memory yet")?;
    let mut out = vec![0u8; len as usize];
    memory
        .read(caller, ptr as usize, &mut out)
        .map_err(|e| format!("could not read guest memory: {e}"))?;
    Ok(out)
}
