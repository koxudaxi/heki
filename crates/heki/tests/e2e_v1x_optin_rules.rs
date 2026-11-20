//! v1.x opt-in rules end to end: `MUT106` (single-pass iterator consumed twice: sources,
//! consumers, wrappers, cross-module generators and consuming helpers, CFG reachability, the
//! loop case, and every filter), `EFF106` (`await` / blocking network I/O while a type-certain
//! lock or DB transaction is held), and `EFF107` (locally bound `asyncio` task never
//! retained), plus selection, inline suppressions, a Ratchet baseline round trip, and docs.

mod common;

use common::Project;
use serde_json::Value;

fn diagnostics(p: &Project, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["--output-format", "json"];
    all.extend_from_slice(args);
    let (code, stdout, stderr) = p.check(&all);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

/// `(path, line, rule, target)` of every diagnostic.
fn found(diags: &[Value]) -> Vec<(String, u64, String, String)> {
    diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
                d["target"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn d(path: &str, line: u64, rule: &str, target: &str) -> (String, u64, String, String) {
    (path.into(), line, rule.into(), target.into())
}

fn message<'a>(diags: &'a [Value], path: &str, line: u64) -> &'a str {
    field(diags, path, line, "message")
}

fn help<'a>(diags: &'a [Value], path: &str, line: u64) -> &'a str {
    field(diags, path, line, "help")
}

fn field<'a>(diags: &'a [Value], path: &str, line: u64, name: &str) -> &'a str {
    diags
        .iter()
        .find(|d| d["path"] == path && d["line"] == line)
        .unwrap_or_else(|| panic!("no diagnostic at {path}:{line}"))[name]
        .as_str()
        .unwrap()
}

/// Related-location messages (the call chain) of the diagnostic at `path:line`.
fn chain(diags: &[Value], path: &str, line: u64) -> Vec<String> {
    diags
        .iter()
        .find(|d| d["path"] == path && d["line"] == line)
        .unwrap()["related"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["message"].as_str().unwrap().to_owned())
        .collect()
}

// ======================================================================== MUT106 fixtures

/// Generators, consuming helpers, and CHA hierarchies the other modules call.
const SOURCES: &str = r##"from contextlib import contextmanager


def read_rows(path):
    yield path


async def numbers():
    yield 1


@contextmanager
def managed():
    yield 1


def drain(items):
    for _ in items:
        pass


def total(items):
    return sum(items)


def forward(items):
    drain(items)


def maybe(items, flag):
    if flag:
        drain(items)


def passthrough(items):
    for item in items:
        yield item


async def drain_async(items):
    for _ in items:
        pass


def early(items):
    for item in items:
        if item:
            return item
    return None


class Sink:
    def write_all(self, rows):
        for r in rows:
            print(r)

    @staticmethod
    def count(rows):
        return sum(1 for _ in rows)


class Feeder:
    def feed(self, rows):
        return list(rows)


class AllFeed(Feeder):
    def feed(self, rows):
        return sorted(rows)


class Base:
    def feed(self, rows):
        return list(rows)


class Lazy(Base):
    def feed(self, rows):
        return rows
"##;

/// True positives, one function per pattern.
const USE: &str = r##"import itertools
from collections.abc import Generator
from itertools import islice, product, takewhile
from typing import AsyncIterator, Iterable, Iterator, Sequence

from pkg.sources import (
    AllFeed,
    Base,
    Feeder,
    Sink,
    drain,
    drain_async,
    early,
    forward,
    managed,
    maybe,
    numbers,
    passthrough,
    read_rows,
    total,
)


def genexpr_then_loop(lines):
    rows = (line.strip() for line in lines)
    count = sum(1 for _ in rows)
    for row in rows:
        print(row)
    return count


def through_alias(lines):
    it = iter(lines)
    other = it
    first = list(it)
    return first, tuple(other)


def generator_from_another_module(path):
    rows = read_rows(path)
    first = list(rows)
    return first, set(rows)


def helpers_consume_their_parameter(lines):
    a = iter(lines)
    drain(a)
    b = map(str, lines)
    total(items=b)
    c = filter(None, lines)
    forward(c)
    sink = Sink()
    d = zip(lines, lines)
    sink.write_all(d)
    e = enumerate(lines)
    Sink.count(e)
    return list(a), list(b), list(c), list(d), list(e)


def wrappers_consume_what_they_wrap(lines):
    rows = iter(lines)
    for i, row in enumerate(rows):
        print(i, row)
    for row in rows:
        print(row)
    e = enumerate(rows)
    list(e)
    list(e)
    z = zip(rows, rows)
    return list(z)


def annotated(rows: Iterator[int], gen: Generator[int, None, None], items: Iterable[int]):
    n = sum(rows)
    m = max(rows)
    loaded: Iterator[int] = load()
    k = sum(loaded)
    j = all(loaded)
    s = min(gen)
    t = sorted(gen)
    list(items)
    list(items)
    return n, m, k, j, s, t


def load():
    return []


def consumed_in_a_loop(lines, batches):
    it = iter(lines)
    for b in batches:
        print(list(it) if b else b)
    pending = map(str, lines)
    while batches:
        print(sum(1 for _ in pending))


def full_then_partial(lines):
    it = iter(lines)
    data = list(it)
    for line in it:
        if line:
            break
    return data


def yield_twice(lines):
    it = iter(lines)
    yield from it
    yield from it


def joined(lines, sep: str):
    names = map(str, lines)
    a = ", ".join(names)
    glue = "-"
    b = glue.join(names)
    c = sep.join(names)
    d = f"{sep}".join(names)
    return a, b, c, d


def eager_product(lines):
    it = iter(lines)
    pairs = product(it, repeat=2)
    return list(pairs), list(it)


def cha_all_consume(lines, feeder: Feeder):
    it = iter(lines)
    feeder.feed(it)
    return list(it)


async def awaited_consumer(lines):
    it = iter(lines)
    await drain_async(it)
    return list(it)


async def async_sources(agen: AsyncIterator[int]):
    async for x in agen:
        print(x)
    async for x in agen:
        print(x)
    nums = numbers()
    async for n in nums:
        print(n)
    return [n async for n in nums]


def same_source_per_iteration(paths):
    for p in paths:
        rows = read_rows(p)
        first = sum(1 for _ in rows)
        second = sum(1 for _ in rows)
        print(first, second)
"##;

/// Every filter: nothing here may be reported.
const NEGATIVES: &str = r##"import itertools
from itertools import count, cycle, islice, repeat, takewhile, tee
from typing import Iterable, Sequence

from pkg.sources import Base, drain, drain_async, early, managed, maybe, passthrough


def exclusive_branches(lines, flag):
    it = iter(lines)
    if flag:
        return list(it)
    else:
        return sorted(it)


def partial_first_pass(lines):
    it = iter(lines)
    for line in it:
        if line.startswith("#"):
            break
    for line in it:
        if not line:
            return None
    for line in it:
        if line == "end":
            raise ValueError(line)
    return list(it)


def stepping(lines):
    it = iter(lines)
    head = next(it)
    second = next(it)
    return head, second, list(it)


def deciding_item(flags):
    it = iter(flags)
    found = any(it)
    rest = all(it)
    return found, rest, list(it)


def drain_second(first, second):
    return first, list(second)


def unknown_parameter(lines, pair):
    it = iter(lines)
    drain_second(*pair, it)
    return list(it)


def rebinding(lines):
    it = iter(lines)
    a = list(it)
    it = iter(lines)
    b = list(it)
    return a, b


def not_single_pass(lines):
    a, b = tee(lines)
    x, y = list(a), list(b)
    for gen in (repeat(1, 3), cycle(lines), count()):
        list(islice(gen, 2))
    r = repeat(1, 3)
    list(r)
    c = itertools.count()
    return x, y, list(r), list(islice(c, 2))


def reiterable_annotations(items: Iterable[int], seq: Sequence[int]):
    return list(items), list(items), sum(seq), max(seq)


def bounded_loops(lines, batches):
    it = iter(lines)
    for _ in range(1):
        once = list(it)
    other = iter(lines)
    for _ in (1,):
        print(sum(other))
    third = iter(lines)
    for b in batches:
        if b:
            print(list(third))
            break
    fourth = iter(lines)
    for b in batches:
        return list(fourth)
    return once


def rest_inside_its_own_loop(lines):
    it = iter(lines)
    for line in it:
        rest = list(it)
        print(line, rest)


def helpers_that_may_not_consume(lines, obj: Base):
    a = iter(lines)
    maybe(a, False)
    b = iter(lines)
    passthrough(b)
    c = iter(lines)
    drain_async(c)
    d = iter(lines)
    obj.feed(d)
    e = iter(lines)
    early(e)
    return list(a), list(b), list(c), list(d), list(e)


def decorated_generator():
    ctx = managed()
    return list(ctx), list(ctx)


def shadowed(lines, iter, list):
    it = iter(lines)
    return list(it), list(it)


def two_values(lines, other):
    it = iter(lines)
    biggest = max(it, other)
    least = min(it, other)
    return biggest, least, sorted(it)


def only_on_the_exception_path(lines):
    it = iter(lines)
    try:
        return list(it)
    except ValueError:
        return list(it)


def partial_itertools(lines):
    it = iter(lines)
    head = list(islice(it, 3))
    comments = list(takewhile(lambda l: l.startswith("#"), it))
    callable_iter = iter(lines.pop, None)
    return head, comments, list(it), list(callable_iter)


def never_reiterated(lines):
    rows = (line for line in lines)
    for a, b in zip(rows, rows):
        print(a, b)
    return [x for x in map(str, lines)]


def zip_stops_at_the_shortest(lines, names, values, flags):
    # `heapq.nlargest`: take `n` items, then go on with the rest.
    it = iter(lines)
    head = [x for _, x in zip(range(3), it)]
    for line in it:
        print(line)
    # `inspect`: one iterator shared by two `zip`s over lists.
    defaults = itertools.chain(repeat(None, 2), values)
    for name, default in zip(names, defaults):
        print(name, default)
    for name, default in zip(names, defaults):
        print(name, default)
    picked = itertools.compress(defaults, flags)
    labels = map(str.join, names, defaults)
    return head, list(picked), list(labels), list(defaults)
"##;

fn mut106_project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("pkg/__init__.py", "")
        .write("pkg/sources.py", SOURCES)
        .write("pkg/use.py", USE)
        .write("pkg/negatives.py", NEGATIVES);
    p
}

#[test]
fn mut106_reports_second_passes_over_single_pass_iterators() {
    let p = mut106_project("v1x_mut106");
    let diags = diagnostics(&p, &["--select", "MUT106"]);
    let u = "pkg/use.py";
    assert_eq!(
        found(&diags),
        vec![
            // A generator expression consumed by `sum`, then iterated.
            d(u, 27, "MUT106", "rows"),
            // Through a local alias.
            d(u, 36, "MUT106", "it"),
            // A generator function of another module.
            d(u, 42, "MUT106", "rows"),
            // Consumed by project helpers: a loop, `sum` (keyword argument), a helper calling
            // a consuming helper, a bound method, and a static method.
            d(u, 57, "MUT106", "a"),
            d(u, 57, "MUT106", "b"),
            d(u, 57, "MUT106", "c"),
            d(u, 57, "MUT106", "d"),
            d(u, 57, "MUT106", "e"),
            // `enumerate(rows)` consumed `rows`; a wrapper over the exhausted `rows`; `e`
            // itself twice (only `e` is named, not also `rows`); `zip(rows, rows)` once.
            d(u, 64, "MUT106", "rows"),
            d(u, 67, "MUT106", "rows"),
            d(u, 68, "MUT106", "e"),
            d(u, 70, "MUT106", "rows"),
            // `Iterator` / `Generator` parameters and an annotated local.
            d(u, 75, "MUT106", "rows"),
            d(u, 78, "MUT106", "loaded"),
            d(u, 80, "MUT106", "gen"),
            // Created outside a `for` / `while` loop, consumed inside it.
            d(u, 93, "MUT106", "it"),
            d(u, 96, "MUT106", "pending"),
            // A full pass, then a partial one.
            d(u, 102, "MUT106", "it"),
            // `yield from` twice.
            d(u, 111, "MUT106", "it"),
            // `str.join` on a literal, a copied literal, a `str` parameter, an f-string.
            d(u, 118, "MUT106", "names"),
            d(u, 119, "MUT106", "names"),
            d(u, 120, "MUT106", "names"),
            // `itertools.product` materializes its input when it is built.
            d(u, 127, "MUT106", "it"),
            // Every CHA candidate consumes; an awaited coroutine consumes.
            d(u, 133, "MUT106", "it"),
            d(u, 139, "MUT106", "it"),
            // `async for` over an `AsyncIterator` and over a project async generator.
            d(u, 145, "MUT106", "agen"),
            d(u, 150, "MUT106", "nums"),
            // Within one iteration (each iteration binds a fresh generator).
            d(u, 157, "MUT106", "rows"),
        ]
    );
    assert_eq!(
        message(&diags, u, 27),
        "`rows` is a single-pass iterator already consumed at line 26, so this pass sees no items"
    );
    assert_eq!(
        help(&diags, u, 27),
        "build a `list` once to iterate it more than once, or create a new iterator for each pass"
    );
    assert_eq!(
        message(&diags, u, 93),
        "`it` is a single-pass iterator consumed by the previous iteration of the loop at line 92, so this pass sees no items"
    );
    // `while batches:` evaluates nothing on the loop's own line.
    assert_eq!(
        message(&diags, u, 96),
        "`pending` is a single-pass iterator consumed by the previous iteration of its loop, so this pass sees no items"
    );
    for d in &diags {
        assert!(d["fix"].is_null(), "{d}");
        assert_eq!(d["rule_slug"], "iterator-double-consumption");
    }
}

#[test]
fn mut106_follows_python311_comprehension_frames() {
    let p = Project::new("v1x_mut106_py311");
    p.write(
        "app.py",
        "def comp(lines):\n    it = iter(lines)\n    a = [x for x in it]\n    b = {x for x in it}\n    return a, b\n\n\ndef helper(xs):\n    return [x for x in xs]\n\n\ndef cross(lines, flag):\n    it = iter(lines)\n    helper(it)\n    other = iter(lines)\n    if flag:\n        helper(other)\n    return list(it), list(other)\n",
    );
    // On the `flag` path `helper(other)` exhausts `other` too: the pairing is per path.
    let expected = vec![
        d("app.py", 4, "MUT106", "it"),
        d("app.py", 18, "MUT106", "it"),
        d("app.py", 18, "MUT106", "other"),
    ];
    // Python 3.11 runs a comprehension in its own frame (`__comp_<id>(iterable)`).
    let diags = diagnostics(&p, &["--select", "MUT106", "--target-version", "py311"]);
    assert_eq!(found(&diags), expected);
    assert_eq!(diags[0]["column"], 9);
    // Python 3.12+ inlines it: the same reports, at the iterable.
    let diags = diagnostics(&p, &["--select", "MUT106"]);
    assert_eq!(found(&diags), expected);
    assert_eq!(diags[0]["column"], 21);
}

// ======================================================================== EFF106 fixtures

const LOCKS: &str = r##"import asyncio
import threading
import time
from contextlib import asynccontextmanager

import anyio
import click
import psycopg
import requests
from django.db import transaction
from sqlalchemy import create_engine
from sqlalchemy.orm import Session

_lock = threading.Lock()
_cond = threading.Condition()


def fetch(url):
    return requests.get(url)


async def fetch_async(url):
    return fetch(url)


class Syncer:
    def __init__(self):
        self._lock = threading.Lock()
        self._sem = threading.BoundedSemaphore(2)
        self._alock = asyncio.Lock()
        self.conn = None

    async def sync_all(self, items):
        with self._lock:
            for item in items:
                await asyncio.sleep(0)
            fetch("https://example.com")
        await asyncio.sleep(1)
        async with self._alock:
            await asyncio.sleep(1)
        async with self._lock:
            await asyncio.sleep(1)

    def nested(self):
        with _lock:
            with self._sem:
                requests.get("https://example.com")

    async def awaited_helper(self):
        with self._lock:
            await fetch_async("https://example.com")

    async def untyped(self, manager):
        with self.conn:
            await asyncio.sleep(0)
        with manager:
            await asyncio.sleep(0)
        with open("f") as fh:
            await asyncio.sleep(0)


def module_lock():
    with _lock:
        requests.get("https://example.com")
    requests.get("https://example.com")


def condition():
    with _cond:
        requests.post("https://example.com")


def annotated(lock: threading.RLock):
    with lock:
        fetch("https://example.com")


def orm(session: Session):
    with session.begin():
        session.execute("SELECT 1")
        requests.post("https://example.com")


async def orm_await(session: Session):
    tx = session.begin_nested()
    with tx:
        await asyncio.sleep(0)


def atomic_view():
    with transaction.atomic():
        requests.get("https://example.com")


def engine_begin():
    engine = create_engine("sqlite://")
    with engine.begin() as conn:
        conn.execute("SELECT 1")
        fetch("https://example.com")


def psycopg_tx():
    conn = psycopg.connect("")
    with conn.transaction():
        fetch("https://example.com")


def released_before_io(items):
    for item in items:
        with _lock:
            if item:
                break
    requests.get("https://example.com")
    with _lock:
        time.sleep(1)
        pending = asyncio.to_thread(fetch, "https://example.com")
        loop = asyncio.get_event_loop()
        loop.run_in_executor(None, fetch, "https://example.com")
    return pending


async def handler_after_release():
    try:
        with _lock:
            value = 1
    except ValueError:
        await asyncio.sleep(0)
        requests.get("https://example.com")
    return value


async def async_locks():
    lock = asyncio.Lock()
    async with lock:
        await asyncio.sleep(0)
    async with anyio.Lock():
        await asyncio.sleep(0)


@click.command()
def cli():
    with _lock:
        requests.get("https://example.com")


@asynccontextmanager
async def lifespan(app):
    with _lock:
        await asyncio.sleep(0)
    yield


def test_inline():
    with _lock:
        requests.get("https://example.com")
"##;

const TEST_LOCKS: &str = r##"import threading

import requests

_lock = threading.Lock()


def helper():
    with _lock:
        requests.get("https://example.com")
"##;

#[test]
fn eff106_reports_awaits_and_network_io_while_a_lock_or_transaction_is_held() {
    let p = Project::new("v1x_eff106");
    p.write("pkg/__init__.py", "")
        .write("pkg/locks.py", LOCKS)
        .write("tests/test_locks.py", TEST_LOCKS);
    let diags = diagnostics(&p, &["--select", "EFF106"]);
    let l = "pkg/locks.py";
    assert_eq!(
        found(&diags),
        vec![
            // `self._lock` from `__init__`: an `await`, then a helper reaching `requests.get`;
            // nothing after the block, and never `async with`.
            d(l, 36, "EFF106", "threading.Lock->await"),
            d(l, 37, "EFF106", "threading.Lock->requests.get"),
            // Nested blocks name the outermost lock.
            d(l, 47, "EFF106", "threading.Lock->requests.get"),
            // An awaited `async def` reaching the network is reported once, as the `await`.
            d(l, 51, "EFF106", "threading.Lock->await"),
            // Module-level lock / condition, annotated `RLock` parameter.
            d(l, 64, "EFF106", "threading.Lock->requests.get"),
            d(l, 70, "EFF106", "threading.Condition->requests.post"),
            d(l, 75, "EFF106", "threading.RLock->requests.get"),
            // Transactions: `Session.begin()` (its query is not reported), `begin_nested()`
            // through a local, `transaction.atomic()`, `Engine.begin()`, psycopg.
            d(
                l,
                81,
                "EFF106",
                "sqlalchemy.orm.Session.begin->requests.post"
            ),
            d(
                l,
                87,
                "EFF106",
                "sqlalchemy.orm.Session.begin_nested->await"
            ),
            d(
                l,
                92,
                "EFF106",
                "django.db.transaction.atomic->requests.get"
            ),
            d(
                l,
                99,
                "EFF106",
                "sqlalchemy.engine.Engine.begin->requests.get"
            ),
            d(
                l,
                105,
                "EFF106",
                "psycopg.Connection.transaction->requests.get"
            ),
        ]
    );
    assert_eq!(
        message(&diags, l, 36),
        "`async def Syncer.sync_all` awaits while holding the synchronous `threading.Lock` taken at line 34"
    );
    assert_eq!(
        help(&diags, l, 36),
        "move the `await` out of the `with` block and hold the lock only for the update"
    );
    assert_eq!(
        message(&diags, l, 37),
        "`Syncer.sync_all` calls `pkg.locks.fetch`, which reaches blocking network I/O `requests.get`, while holding the synchronous `threading.Lock` taken at line 34"
    );
    assert_eq!(
        help(&diags, l, 37),
        "make the request before taking the lock and hold it only for the update"
    );
    assert_eq!(
        message(&diags, l, 81),
        "`orm` calls `requests.post` (blocking network I/O) while the database transaction from `sqlalchemy.orm.Session.begin` (line 79) is open"
    );
    assert_eq!(
        help(&diags, l, 81),
        "make the request before or after the transaction"
    );
    assert_eq!(
        message(&diags, l, 87),
        "`async def orm_await` awaits while the database transaction from `sqlalchemy.orm.Session.begin_nested` (line 86) is open"
    );
    assert_eq!(
        help(&diags, l, 87),
        "move the `await` before or after the transaction"
    );
    // Indirect I/O carries the shortest call chain; a direct call its own hop.
    assert_eq!(
        chain(&diags, l, 37),
        [
            "`Syncer.sync_all` calls `pkg.locks.fetch`",
            "`fetch` calls `requests.get`"
        ]
    );
    assert_eq!(chain(&diags, l, 64), ["`module_lock` calls `requests.get`"]);
    assert!(chain(&diags, l, 36).is_empty());
    for d in &diags {
        assert!(d["fix"].is_null(), "{d}");
    }

    // Test code (a `tests/` file, a `test_*` function) is relaxed unless configured; CLI
    // commands and ASGI lifespans stay relaxed.
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nrelax-async-in-tests = false\n",
    );
    let strict = found(&diagnostics(&p, &["--select", "EFF106"]));
    assert_eq!(strict.len(), 14, "{strict:?}");
    assert!(strict.contains(&d(l, 155, "EFF106", "threading.Lock->requests.get")));
    assert!(strict.contains(&d(
        "tests/test_locks.py",
        10,
        "EFF106",
        "threading.Lock->requests.get"
    )));
}

// ======================================================================== EFF107 fixtures

const TASKS: &str = r##"import asyncio
from asyncio import create_task, ensure_future

background = set()
registry = {}
LAST = None


async def work():
    await asyncio.sleep(1)


def register(task):
    background.add(task)


async def dropped(flag):
    task = asyncio.create_task(work())
    alias = task
    alias.add_done_callback(print)
    alias.set_name("worker")
    maybe = create_task(work()) if flag else None
    helper = lambda: 1
    numbers = [1, 2]
    mapping = {"k": 1, **registry}
    squares = (n * n for n in numbers)
    await asyncio.sleep(0)
    return helper, mapping, list(squares)


async def via_loop():
    loop = asyncio.get_running_loop()
    task = loop.create_task(work())
    await asyncio.sleep(0)


def ensure():
    fut = ensure_future(work())


async def bare_statements(loop: asyncio.AbstractEventLoop):
    asyncio.create_task(work())
    asyncio.ensure_future(work())
    loop.create_task(work())


async def retained_by_await():
    task = asyncio.create_task(work())
    await asyncio.sleep(0)
    await task


async def retained_by_return():
    task = asyncio.create_task(work())
    return task


def retained_by_yield():
    task = asyncio.ensure_future(work())
    yield task


def retained_by_yield_from():
    task = asyncio.ensure_future(work())
    yield from task


async def retained_by_containers():
    a = asyncio.create_task(work())
    b = asyncio.create_task(work())
    c = asyncio.create_task(work())
    d = asyncio.create_task(work())
    tasks = [a]
    registry["b"] = b
    registry[c] = None
    found = {"d": d}
    return tasks, found


async def retained_by_calls():
    a = asyncio.create_task(work())
    b = asyncio.create_task(work())
    c = asyncio.create_task(work())
    background.add(a)
    register(task=b)
    await asyncio.gather(c)


class Owner:
    async def start(self):
        self.task = asyncio.create_task(work())
        t = asyncio.create_task(work())
        self.other = t


def retained_by_global():
    global LAST
    task = asyncio.ensure_future(work())
    LAST = task


def retained_by_closure():
    holder = None
    task = asyncio.ensure_future(work())

    def inner():
        nonlocal holder
        t = asyncio.ensure_future(work())
        holder = t
        return task

    gen = (x for x in range(1) if task)
    return inner, gen


async def lifetime_managed():
    hb = asyncio.create_task(work())
    done = asyncio.create_task(work())
    failed = asyncio.create_task(work())
    try:
        await asyncio.sleep(1)
    finally:
        hb.cancel()
    print(done.result(), failed.exception())


async def task_group_and_unknown(scheduler):
    async with asyncio.TaskGroup() as tg:
        t = tg.create_task(work())
    u = scheduler.create_task(work())
    v = scheduler.ensure_future(work())


task_at_module_level = asyncio.ensure_future(work())


class Body:
    kept = asyncio.ensure_future(work())
"##;

#[test]
fn eff107_reports_bound_tasks_that_nothing_retains() {
    let p = Project::new("v1x_eff107");
    p.write("pkg/__init__.py", "").write("pkg/tasks.py", TASKS);
    let diags = diagnostics(&p, &["--select", "EFF107"]);
    let t = "pkg/tasks.py";
    assert_eq!(
        found(&diags),
        vec![
            // Only the receiver of `add_done_callback` / `set_name` (through an alias), and a
            // conditional binding (`create_task` imported directly).
            d(t, 18, "EFF107", "asyncio.create_task->task"),
            d(t, 22, "EFF107", "asyncio.create_task->maybe"),
            // `loop.create_task` on `asyncio.get_running_loop()`.
            d(
                t,
                33,
                "EFF107",
                "asyncio.AbstractEventLoop.create_task->task"
            ),
            // `ensure_future` in a synchronous function.
            d(t, 38, "EFF107", "asyncio.ensure_future->fut"),
        ]
    );
    assert_eq!(
        message(&diags, t, 18),
        "the task from `asyncio.create_task` is bound to `task` but never awaited, returned, or stored, so it can be garbage-collected before it finishes"
    );
    assert_eq!(
        help(&diags, t, 18),
        "keep it in a set and discard it in a done callback, or await it"
    );
    for d in &diags {
        assert!(d["fix"].is_null(), "{d}");
    }
}

const BOUND_METHODS: &str = r##"import asyncio
import atexit
import unittest


async def work():
    await asyncio.sleep(1)


class Case(unittest.IsolatedAsyncioTestCase):
    async def test_cleanup(self):
        t = asyncio.create_task(work())
        self.addCleanup(t.cancel)

    async def test_stored(self):
        t = asyncio.create_task(work())
        self.stop = t.cancel


async def passed_on(loop: asyncio.AbstractEventLoop, stack):
    a = asyncio.create_task(work())
    b = asyncio.create_task(work())
    c = asyncio.create_task(work())
    d = asyncio.create_task(work())
    loop.call_later(1, a.cancel)
    stack.callback(callback=b.cancel)
    atexit.register(c.cancel)
    cancel = d.cancel
    handlers = [cancel]
    return handlers


def returned():
    task = asyncio.ensure_future(work())
    return task.result


async def method_results_only():
    task = asyncio.create_task(work())
    print(task.get_name(), task.done())
    if task.cancelled:
        pass
"##;

#[test]
fn eff107_bound_methods_of_the_task_keep_it_where_they_are_kept() {
    let p = Project::new("v1x_eff107_bound_methods");
    p.write("pkg/__init__.py", "")
        .write("pkg/bound.py", BOUND_METHODS);
    let diags = diagnostics(&p, &["--select", "EFF107"]);
    // A bound method passed to a call (positionally or by keyword), stored in an attribute or a
    // container through a copy, or returned holds the task. The results of its methods, and a
    // method read only to be tested, hold nothing.
    assert_eq!(
        found(&diags),
        vec![d("pkg/bound.py", 39, "EFF107", "asyncio.create_task->task")]
    );
}

const ARGUMENTS: &str = r##"import asyncio
import concurrent.futures
from collections.abc import Awaitable, Callable, Coroutine
from typing import Any


def retry(fn):
    return fn


class Event:
    def __init__(self):
        self.waiters = set()

    def wait(self):
        fut = asyncio.get_running_loop().create_future()
        self.waiters.add(fut)
        return fut

    @staticmethod
    def make():
        return None


class Job:
    def __init__(self):
        self.done = False

    def __await__(self):
        return asyncio.sleep(0).__await__()


async def work():
    await asyncio.sleep(0)


def plain():
    pass


def gen():
    yield 1


async def agen():
    yield 1


@retry
def wrapped():
    return work()


async def no_new_task(e: Event, fut: asyncio.Future[None], task: asyncio.Task[None]):
    loop = asyncio.get_running_loop()
    pool = concurrent.futures.ThreadPoolExecutor()
    a = asyncio.ensure_future(e.wait())
    pending = e.wait()
    b = asyncio.ensure_future(pending)
    c = asyncio.create_task(plain())
    d = asyncio.create_task(coro=plain())
    g = asyncio.ensure_future(coro_or_future=e.wait())
    h = loop.create_task(Event.make())
    i = asyncio.create_task(gen())
    j = asyncio.ensure_future(agen())
    k = asyncio.ensure_future(loop.run_in_executor(None, plain))
    m = asyncio.ensure_future(loop.create_future())
    n = asyncio.ensure_future(asyncio.gather(work()))
    o = asyncio.ensure_future(asyncio.wrap_future(pool.submit(plain)))
    p = asyncio.ensure_future(fut)
    q = asyncio.ensure_future(task)
    r = asyncio.ensure_future(pool.submit(plain))

    def remove_victim():
        pass

    s = asyncio.create_task(remove_victim())
    await asyncio.sleep(0)


async def may_make_a_task(
    coro,
    callback: Callable[[], Awaitable[None] | None],
    factory: Callable[..., Coroutine[Any, Any, Any]],
    coros,
):
    a = asyncio.ensure_future(work())
    b = asyncio.create_task(coro=work())
    c = asyncio.create_task(coro)
    result = callback()
    d = asyncio.ensure_future(result)
    e = asyncio.ensure_future(factory())
    f = asyncio.ensure_future(wrapped())
    g = asyncio.ensure_future(Job())
    h = asyncio.create_task(*coros)
    await asyncio.sleep(0)
"##;

#[test]
fn eff107_reports_only_arguments_that_may_be_coroutines() {
    let p = Project::new("v1x_eff107_arguments");
    p.write("pkg/__init__.py", "")
        .write("pkg/args.py", ARGUMENTS);
    let diags = diagnostics(&p, &["--select", "EFF107"]);
    let t = "pkg/args.py";
    // `no_new_task`: no call makes a task from the result of a plain `def`, a static method, a
    // generator or an async generator function (through a copy, by keyword, or in a nested
    // function), nor from a value typed as a future (catalog return types, annotations,
    // `Executor.submit`). `may_make_a_task`: each one may, from a coroutine, a parameter, an
    // awaitable, an annotated callable's result, a decorated function's result, a constructed
    // object, or a `*` argument.
    assert_eq!(
        found(&diags),
        vec![
            d(t, 87, "EFF107", "asyncio.ensure_future->a"),
            d(t, 88, "EFF107", "asyncio.create_task->b"),
            d(t, 89, "EFF107", "asyncio.create_task->c"),
            d(t, 91, "EFF107", "asyncio.ensure_future->d"),
            d(t, 92, "EFF107", "asyncio.ensure_future->e"),
            d(t, 93, "EFF107", "asyncio.ensure_future->f"),
            d(t, 94, "EFF107", "asyncio.ensure_future->g"),
            d(t, 95, "EFF107", "asyncio.create_task->h"),
        ]
    );
}

// ======================================================================== selection & CLI

const ONE_OF_EACH: &str = r##"import asyncio
import threading

import requests

LOCK = threading.Lock()


async def work():
    await asyncio.sleep(0)


def twice(lines):
    rows = (line for line in lines)
    first = list(rows)
    return first, list(rows)


def held():
    with LOCK:
        requests.get("https://example.com")


async def spawn():
    task = asyncio.create_task(work())
    await asyncio.sleep(0)
"##;

#[test]
fn opt_in_rules_run_only_when_selected_by_code_or_slug() {
    let p = Project::new("v1x_selection");
    p.write("app.py", ONE_OF_EACH);
    let rules = |args: &[&str]| -> Vec<String> {
        found(&diagnostics(&p, args))
            .into_iter()
            .map(|(_, _, rule, _)| rule)
            .collect()
    };
    let ours = |all: Vec<String>| -> Vec<String> {
        all.into_iter()
            .filter(|r| ["MUT106", "EFF106", "EFF107"].contains(&r.as_str()))
            .collect()
    };
    // Off by default, and a prefix does not turn an opt-in rule on.
    assert!(ours(rules(&[])).is_empty());
    assert!(ours(rules(&["--select", "MUT,EFF"])).is_empty());
    assert!(ours(rules(&["--select", "ALL"])).is_empty());
    assert_eq!(
        ours(rules(&["--extend-select", "MUT106,EFF106,EFF107"])),
        ["MUT106", "EFF106", "EFF107"]
    );
    assert_eq!(
        rules(&[
            "--select",
            "iterator-double-consumption,lock-or-tx-held-across-io,unretained-async-task"
        ]),
        ["MUT106", "EFF106", "EFF107"]
    );
    // From the configuration too.
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"MUT106\", \"EFF107\"]\n",
    );
    assert_eq!(ours(rules(&[])), ["MUT106", "EFF107"]);
}

#[test]
fn inline_suppressions_and_ratchet_baseline() {
    let p = Project::new("v1x_suppress");
    p.write(
        "app.py",
        &ONE_OF_EACH
            .replace(
                "return first, list(rows)",
                "return first, list(rows)  # heki: ignore[MUT106]",
            )
            .replace(
                "requests.get(\"https://example.com\")",
                "requests.get(\"https://example.com\")  # noqa: lock-or-tx-held-across-io",
            )
            .replace(
                "task = asyncio.create_task(work())",
                "task = asyncio.create_task(work())  # heki: ignore[EFF107]",
            ),
    );
    let select = ["--select", "MUT106,EFF106,EFF107"];
    assert!(diagnostics(&p, &select).is_empty());

    // Ratchet baseline: the existing EFF107 report is accepted, a new one is a regression.
    p.write("app.py", ONE_OF_EACH);
    let (code, _, stderr) = p.check(&[
        "--select",
        "EFF107",
        "--baseline",
        "heki-baseline.toml",
        "--update-baseline",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let text = p.read("heki-baseline.toml");
    assert!(
        text.contains("rule_code = \"EFF107\"")
            && text.contains("target = \"asyncio.create_task->task\""),
        "{text}"
    );
    let with_baseline = ["--select", "EFF107", "--baseline", "heki-baseline.toml"];
    assert_eq!(p.check(&with_baseline).0, 0);
    // Moving code does not matter (no line numbers in the key).
    p.write("app.py", &format!("# moved\n\n{ONE_OF_EACH}"));
    assert_eq!(p.check(&with_baseline).0, 0);
    p.write(
        "app.py",
        &format!(
            "{ONE_OF_EACH}\n\nasync def spawn_more():\n    extra = asyncio.ensure_future(work())\n"
        ),
    );
    let (code, stdout, _) = p.check(&[
        "--select",
        "EFF107",
        "--baseline",
        "heki-baseline.toml",
        "--output-format",
        "concise",
    ]);
    assert_eq!(code, 1);
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert!(stdout.starts_with("app.py:30:13: EFF107 "), "{stdout}");
}

#[test]
fn rule_command_prints_the_documentation() {
    let p = Project::new("v1x_docs");
    for (code, slug) in [
        ("MUT106", "iterator-double-consumption"),
        ("EFF106", "lock-or-tx-held-across-io"),
        ("EFF107", "unretained-async-task"),
    ] {
        let (status, doc, _) = p.heki(&["rule", code]);
        assert_eq!(status, 0);
        assert!(doc.starts_with(&format!("# {slug} ({code})\n\n")), "{doc}");
        let at = |h: &str| doc.find(h).unwrap_or_else(|| panic!("{code}: no {h}"));
        assert!(
            at("## What it does\n") < at("## Why is this bad?\n")
                && at("## Why is this bad?\n") < at("## Example\n")
                && at("## Example\n") < at("Use instead:\n")
                && at("Use instead:\n") < at("## Fix safety\nNo automatic fix.\n")
                && at("## Fix safety\n") < at("## Options\n"),
            "{doc}"
        );
        assert!(doc.contains(&format!("--extend-select {code}")), "{doc}");
        assert_eq!(p.heki(&["explain", slug]).1, doc);
    }
    let (_, all, _) = p.heki(&["rule", "--all"]);
    for code in ["MUT106", "EFF106", "EFF107"] {
        assert!(
            all.lines()
                .any(|l| l.starts_with(code) && l.contains("[default=false fix=false]")),
            "{all}"
        );
    }
}
