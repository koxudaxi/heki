//! The threads that parse and analyze source. The syntax tree of an expression is as deep as its
//! source is long (a chain of `+` operands, subscripts, or attributes, brackets in brackets), and
//! the parser, the lowering, the walks over the tree, and the annotation readers recurse once per
//! level, so they run on threads of [`ANALYSIS_STACK`], not on the main thread (1 MiB on
//! Windows, `ulimit -s` elsewhere) or on threads of the default 2 MiB.

use crate::Commands;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

/// The stack of every thread that parses or walks source. A level of an operator chain takes
/// under 1 KiB of it in a release build (a level of nested brackets under 2 KiB, about 10 KiB in
/// a debug build), so it holds several times the chains CPython compiles on an 8 MiB stack, about
/// 58,000 operands; CPython nests brackets 200 deep at most. The platforms heki ships for map a
/// thread's stack as the thread reaches it (`mmap` on Linux and macOS, a reservation that Windows
/// commits page by page), so a thread costs the stack it uses, and Linux's default overcommit
/// check refuses a reservation only when it exceeds the machine's memory and swap. A limit on
/// virtual memory (`ulimit -v`) counts the whole reservation of every thread, though: where the
/// host refuses it, the threads get the platform's default stack, after one warning.
pub(crate) const ANALYSIS_STACK: usize = 256 << 20;

/// Test hook: the stack of the analysis threads in bytes, in place of [`ANALYSIS_STACK`], for a
/// test of a host that refuses the reservation. Not a setting.
const STACK_ENV: &str = "HEKI_ANALYSIS_STACK";

/// Whether a reservation of the analysis stack was refused: every thread spawned after it gets
/// the platform's default stack.
static REFUSED: AtomicBool = AtomicBool::new(false);

/// Whether `command` parses or analyzes source, and so runs on the analysis threads.
pub(crate) fn analyzes_source(command: &Commands) -> bool {
    match command {
        Commands::Check(_)
        | Commands::Server(_)
        | Commands::Snapshot(_)
        | Commands::Assumptions(_)
        | Commands::Inspect(_)
        | Commands::Recommend(_)
        | Commands::DumpSemantic(_)
        | Commands::DumpIr(_)
        | Commands::DumpFacts(_)
        | Commands::DumpRecommendFacts(_)
        | Commands::DumpOpenPremises(_) => true,
        Commands::Rule(_)
        | Commands::Skill(_)
        | Commands::Version(_)
        | Commands::DumpExperimental
        | Commands::DumpPrecision => false,
    }
}

/// Runs `command`: one that analyzes source on a thread of [`thread_stack`], after building
/// there the global pool of the parallel work, whose threads get the same stack; any other on
/// the calling thread, with no pool.
pub(crate) fn run<R: Send + 'static>(
    analyzes_source: bool,
    command: impl FnOnce() -> R + Send + 'static,
) -> R {
    if !analyzes_source {
        return command();
    }
    let command = move || {
        build_thread_pool();
        command()
    };
    match spawn_sized(Some("heki".to_owned()), command) {
        Ok(thread) => thread
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic)),
        Err(command) => command(),
    }
}

/// The stack of the threads that parse or walk source: [`ANALYSIS_STACK`], or the platform's
/// default (`None`) once the host refused it.
pub(crate) fn thread_stack() -> Option<usize> {
    let size = std::env::var(STACK_ENV)
        .ok()
        .and_then(|size| size.parse().ok())
        .unwrap_or(ANALYSIS_STACK);
    (!REFUSED.load(Ordering::Relaxed)).then_some(size)
}

/// Spawns `work` on a thread of [`thread_stack`], or gives it back where the host refuses that
/// stack (or refused it before).
fn spawn_sized<R, F>(name: Option<String>, work: F) -> Result<JoinHandle<R>, F>
where
    R: Send + 'static,
    F: FnOnce() -> R + Send + 'static,
{
    let Some(size) = thread_stack() else {
        return Err(work);
    };
    // A refused spawn drops the closure it was given: `work` waits in a slot the thread takes it
    // from, so that it can be taken back.
    let slot = Arc::new(Mutex::new(Some(work)));
    let job = {
        let slot = Arc::clone(&slot);
        move || take(&slot)()
    };
    let mut thread = std::thread::Builder::new().stack_size(size);
    if let Some(name) = name {
        thread = thread.name(name);
    }
    thread.spawn(job).map_err(|_| {
        if !REFUSED.swap(true, Ordering::Relaxed) {
            eprintln!(
                "warning: this host refuses threads with a stack of {} MiB (a limit on virtual \
                 memory); heki runs on the default stack, which an expression nested thousands \
                 deep may overflow",
                ANALYSIS_STACK >> 20
            );
        }
        take(&slot)
    })
}

/// The work a slot of [`spawn_sized`] holds, taken once.
fn take<F>(slot: &Mutex<Option<F>>) -> F {
    slot.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .take()
        .expect("the work of a thread is taken once")
}

/// The global pool `par_iter` runs on, of threads of [`thread_stack`]. A counting build
/// (`count-alloc`) runs the parallel work on the calling thread alone: a second thread's timing
/// would move the peak between runs.
fn build_thread_pool() {
    let pool = rayon::ThreadPoolBuilder::new().spawn_handler(|worker| {
        match spawn_sized(worker.name().map(str::to_owned), move || worker.run()) {
            Ok(_) => Ok(()),
            Err(work) => std::thread::Builder::new().spawn(work).map(drop),
        }
    });
    #[cfg(feature = "count-alloc")]
    let pool = pool.num_threads(1).use_current_thread();
    pool.build_global()
        .expect("the global thread pool is built before any parallel work");
}
