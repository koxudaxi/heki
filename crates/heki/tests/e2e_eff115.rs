//! `EFF115` (`async-def-as-sync-callback`) end to end: an `async def` handed to every kind of
//! library call that never awaits what it returns (threads, timers, processes, executors, event
//! loop callbacks, offloading APIs, `atexit`, signal handlers, exit stacks, finalizers, and the
//! predicates `filter` and three `itertools` functions test for truth), directly, as a bound
//! method, through `functools.partial`, from another module, and in a module body; the wording of
//! a decorated function and of a method with a plain implementation; and the shapes left silent:
//! a plain function, `map` and the sort keys, a project runner or `Thread` subclass, an async
//! generator, a lambda, a class, and unpacked arguments; the opt-in selection, and `heki rule`.
//! Every line marked `# EFF115` in the sources is reported, and no other.

mod common;

use common::Project;
use serde_json::Value;

const JOBS: &str = r#"import functools


async def flush(n: int = 0) -> int:
    return n


async def keep(n: int) -> bool:
    return n > 0


def plain(n: int = 0) -> int:
    return n


def decorate(fn):
    @functools.wraps(fn)
    async def wrapper(*args, **kwargs):
        return await fn(*args, **kwargs)

    return wrapper


@decorate
async def decorated() -> None:
    pass


async def ticks():
    yield 1
"#;

const RUNNERS: &str = r#"import asyncio
import atexit
import concurrent.futures
import contextlib
import functools
import multiprocessing
import signal
import threading
import weakref
from concurrent.futures import Executor, ThreadPoolExecutor
from threading import Thread

import anyio.to_thread

from pkg.jobs import flush, plain


class Exporter:
    async def close(self) -> None:
        pass

    def start(self) -> None:
        threading.Thread(target=self.close).start()  # EFF115
        atexit.register(functools.partial(self.close))  # EFF115


def threads(items: list[int]) -> None:
    threading.Thread(target=flush, args=(1,)).start()  # EFF115
    Thread(None, flush).start()  # EFF115
    threading.Timer(1.0, flush).start()  # EFF115
    multiprocessing.Process(target=flush).start()  # EFF115
    threading.Thread(target=plain).start()
    threading.Timer(1.0, plain).start()


def executors(ex: Executor, items: list[int]) -> None:
    ex.submit(flush, 1)  # EFF115
    ex.map(flush, items)  # EFF115
    with ThreadPoolExecutor() as pool:
        pool.submit(functools.partial(flush, 1))  # EFF115
        pool.submit(plain, 1)
    with concurrent.futures.ProcessPoolExecutor() as procs:
        procs.submit(flush)  # EFF115


def handlers(items: list[int]) -> None:
    atexit.register(flush)  # EFF115
    signal.signal(signal.SIGTERM, flush)  # EFF115
    with contextlib.ExitStack() as stack:
        stack.callback(flush, 1)  # EFF115
        stack.callback(plain, 1)
    weakref.finalize(items, flush)  # EFF115
    atexit.register(plain)
    signal.signal(signal.SIGTERM, signal.SIG_DFL)


async def loops() -> None:
    loop = asyncio.get_running_loop()
    await loop.run_in_executor(None, flush, 1)  # EFF115
    await asyncio.to_thread(flush)  # EFF115
    await anyio.to_thread.run_sync(flush)  # EFF115
    loop.call_soon(flush)  # EFF115
    loop.call_soon_threadsafe(flush)  # EFF115
    loop.call_later(1.0, flush)  # EFF115
    loop.call_at(loop.time() + 1.0, flush)  # EFF115
    async with contextlib.AsyncExitStack() as stack:
        stack.callback(flush)  # EFF115
    await loop.run_in_executor(None, plain, 1)
    await asyncio.to_thread(plain)


atexit.register(flush)  # EFF115
"#;

const CONSUMERS: &str = r#"import asyncio
import itertools

from pkg.jobs import flush, keep


def predicates(items: list[int]) -> list[int]:
    kept = list(filter(keep, items))  # EFF115
    dropped = list(itertools.filterfalse(keep, items))  # EFF115
    head = list(itertools.takewhile(keep, items))  # EFF115
    tail = list(itertools.dropwhile(keep, items))  # EFF115
    plain = list(filter(None, items)) + list(filter(lambda n: n > 0, items))
    return kept + dropped + head + tail + plain


async def results(items: list[int]) -> list[int]:
    return await asyncio.gather(*map(flush, items))


def keys(items: list[int]) -> int:
    return sorted(items, key=flush)[0] + min(items, key=flush) + max(items, key=flush)
"#;

/// Calls that hand an `async def` on to code that may run it, or values that make no coroutine.
const SILENT: &str = r#"import asyncio
import inspect
import threading

from pkg.jobs import flush, ticks


class AsyncThread(threading.Thread):
    def run(self) -> None:
        asyncio.run(self._target())


def submit(fn) -> None:
    if inspect.iscoroutinefunction(fn):
        asyncio.run(fn())
    else:
        fn()


class Registry:
    def register(self, fn) -> None:
        self.fn = fn


class Job:
    def __init__(self) -> None:
        self.done = False


def run(registry: Registry, args: tuple) -> None:
    AsyncThread(target=flush).start()
    submit(flush)
    registry.register(flush)
    threading.Thread(target=ticks).start()
    threading.Thread(target=lambda: flush()).start()
    threading.Thread(target=Job).start()
    threading.Thread(*args).start()
"#;

/// The reports worded as possible: a decorated `async def`, and a method whose call may run a
/// plain implementation.
const POSSIBLE: &str = r#"import threading

from pkg.jobs import decorated


class Handler:
    def handle(self) -> None:
        pass


class AsyncHandler(Handler):
    async def handle(self) -> None:
        pass


def start(handler: Handler) -> None:
    threading.Thread(target=decorated).start()  # EFF115
    threading.Thread(target=handler.handle).start()  # EFF115
"#;

const SOURCES: &[(&str, &str)] = &[
    ("pkg/jobs.py", JOBS),
    ("pkg/runners.py", RUNNERS),
    ("pkg/consumers.py", CONSUMERS),
    ("pkg/silent.py", SILENT),
    ("pkg/possible.py", POSSIBLE),
];

fn project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("pkg/__init__.py", "");
    for (rel, text) in SOURCES {
        p.write(rel, text);
    }
    p
}

fn diagnostics(p: &Project, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["--output-format", "json"];
    all.extend_from_slice(args);
    let (code, stdout, stderr) = p.check(&all);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

fn eff115(diags: &[Value]) -> Vec<&Value> {
    diags
        .iter()
        .filter(|d| d["rule_code"] == "EFF115")
        .collect()
}

/// `(path, line)` of every `EFF115` diagnostic.
fn found(diags: &[Value]) -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = eff115(diags)
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
            )
        })
        .collect();
    out.sort();
    out
}

/// `(path, line)` of every line marked `# EFF115` in the sources.
fn marked() -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = SOURCES
        .iter()
        .flat_map(|(rel, text)| {
            text.lines()
                .zip(1u64..)
                .filter(|(line, _)| line.ends_with("# EFF115"))
                .map(|(_, n)| ((*rel).to_owned(), n))
        })
        .collect();
    out.sort();
    out
}

fn message_at<'d>(diags: &'d [Value], path: &str, line: u64) -> &'d str {
    eff115(diags)
        .into_iter()
        .find(|d| d["path"] == path && d["line"] == line)
        .and_then(|d| d["message"].as_str())
        .unwrap_or_else(|| panic!("no EFF115 at {path}:{line}"))
}

#[test]
fn an_async_def_handed_to_a_call_that_never_awaits_it_is_reported() {
    let p = project("eff115_shapes");
    let diags = diagnostics(&p, &["--select", "EFF115"]);
    assert_eq!(found(&diags), marked());
}

#[test]
fn a_report_names_the_call_and_the_function() {
    let p = project("eff115_messages");
    let diags = diagnostics(&p, &["--select", "EFF115"]);
    assert_eq!(
        message_at(&diags, "pkg/runners.py", 28),
        "`threading.Thread` never awaits what `flush` returns, and `flush` is an `async def`: \
         calling it only creates a coroutine, so the body does not run"
    );
    assert_eq!(
        message_at(&diags, "pkg/runners.py", 23),
        "`threading.Thread` never awaits what `Exporter.close` returns, and `Exporter.close` is \
         an `async def`: calling it only creates a coroutine, so the body does not run"
    );
    assert_eq!(
        message_at(&diags, "pkg/consumers.py", 8),
        "`filter` tests what `keep` returns for truth, and `keep` is an `async def`: the result \
         is a coroutine, which is always true, and the body does not run"
    );
    assert_eq!(
        message_at(&diags, "pkg/possible.py", 17),
        "`threading.Thread` never awaits what `decorated` returns, and `decorated` is an \
         `async def` under a decorator, which may make it a plain function: calling it only \
         creates a coroutine, so the body does not run"
    );
    assert_eq!(
        message_at(&diags, "pkg/possible.py", 18),
        "`threading.Thread` never awaits what `AsyncHandler.handle` returns, and \
         `AsyncHandler.handle`, one implementation the call may run, is an `async def`: calling \
         it only creates a coroutine, so the body does not run"
    );
    let first = eff115(&diags)
        .into_iter()
        .find(|d| d["path"] == "pkg/runners.py" && d["line"] == 28)
        .unwrap();
    assert_eq!(first["qualname"], "threads");
    assert_eq!(first["target"], "threading.Thread->flush");
    assert_eq!(first["certainty"], "heuristic");
}

#[test]
fn a_suppression_comment_silences_a_report() {
    let p = Project::new("eff115_noqa");
    p.write("pkg/__init__.py", "").write(
        "pkg/main.py",
        "import atexit\n\n\nasync def flush() -> None:\n    pass\n\n\natexit.register(flush)  # heki: ignore[EFF115]\natexit.register(flush)\n",
    );
    let diags = diagnostics(&p, &["--select", "EFF115"]);
    assert_eq!(found(&diags), vec![("pkg/main.py".to_owned(), 9)]);
}

#[test]
fn the_rule_is_opt_in() {
    let p = project("eff115_opt_in");
    assert!(found(&diagnostics(&p, &[])).is_empty());
    assert!(found(&diagnostics(&p, &["--select", "EFF"])).is_empty());
    assert_eq!(
        found(&diagnostics(
            &p,
            &["--select", "async-def-as-sync-callback"]
        )),
        marked()
    );
}

#[test]
fn heki_rule_explains_it() {
    let p = Project::new("eff115_rule");
    let (code, stdout, _) = p.heki(&["rule", "EFF115"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("async-def-as-sync-callback"));
    assert!(stdout.contains("Coroutine function"));
    assert!(stdout.contains("experimental"));
}

#[test]
fn an_async_cleanup_is_reported_in_a_test_case_and_not_in_an_async_one() {
    let p = Project::new("eff115_unittest_cleanups");
    p.write(
        "tests/test_close.py",
        r#"import unittest


class Plain(unittest.TestCase):
    async def async_close(self) -> None:
        pass

    def setUp(self) -> None:
        self.addCleanup(self.async_close)


class Isolated(unittest.IsolatedAsyncioTestCase):
    async def async_close(self) -> None:
        pass

    async def asyncSetUp(self) -> None:
        self.addCleanup(self.async_close)
"#,
    );
    let diags = diagnostics(&p, &["--select", "EFF115"]);
    assert_eq!(found(&diags), [("tests/test_close.py".to_owned(), 9)]);
}
