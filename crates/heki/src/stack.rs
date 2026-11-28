//! The threads that parse and analyze source. The syntax tree of an expression is as deep as its
//! source is long (a chain of `+` operands, subscripts, or attributes, brackets in brackets), and
//! the parser, the lowering, the walks over the tree, and the annotation readers recurse once per
//! level, so they run on threads of [`ANALYSIS_STACK`], not on the main thread (1 MiB on
//! Windows, `ulimit -s` elsewhere) or on threads of the default 2 MiB.

/// The stack of every thread that parses or walks source. A level of an operator chain takes
/// under 1 KiB of it in a release build (a level of nested brackets under 2 KiB, about 10 KiB in
/// a debug build), so it holds several times the chains CPython compiles on an 8 MiB stack, about
/// 58,000 operands; CPython nests brackets 200 deep at most. The platforms heki ships for map a
/// thread's stack as the thread reaches it (`mmap` on Linux and macOS, a reservation that Windows
/// commits page by page), so a thread costs the stack it uses, and Linux's default overcommit
/// check refuses a reservation only when it exceeds the machine's memory and swap.
pub(crate) const ANALYSIS_STACK: usize = 256 << 20;

/// Runs `command` on a thread of [`ANALYSIS_STACK`], after building there the thread pool of
/// the parallel work.
pub(crate) fn on_analysis_stack<R: Send>(command: impl FnOnce() -> R + Send) -> R {
    std::thread::scope(|scope| {
        std::thread::Builder::new()
            .name("heki".to_owned())
            .stack_size(ANALYSIS_STACK)
            .spawn_scoped(scope, || {
                build_thread_pool();
                command()
            })
            .expect("the analysis thread starts")
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
}

/// The global pool `par_iter` runs on, of threads of [`ANALYSIS_STACK`]. A counting build
/// (`count-alloc`) runs the parallel work on the calling thread alone: a second thread's timing
/// would move the peak between runs.
fn build_thread_pool() {
    let pool = rayon::ThreadPoolBuilder::new().stack_size(ANALYSIS_STACK);
    #[cfg(feature = "count-alloc")]
    let pool = pool.num_threads(1).use_current_thread();
    pool.build_global()
        .expect("the global thread pool is built before any parallel work");
}
