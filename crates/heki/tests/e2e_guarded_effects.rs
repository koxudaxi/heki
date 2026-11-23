//! Guarded effects end to end (`heki_effect::guards`): an effect is not counted where a guard
//! on its path makes it not happen.
//!
//! - Lazy initialization (`EFF102` / `EFF103`): a client or query created only while an
//!   attribute is unset, which the code then sets, runs once per object, not on every loop
//!   iteration; so does a `functools.cached_property` getter. One only an unset local variable
//!   or a first-iteration flag lets run runs once per run of the loop, and one behind a module
//!   global once per process.
//! - Calls every way on from which leaves the loop (`EFF102` / `EFF103`): `return`, `break`,
//!   `raise` right after them, they run once per run of the loop.
//! - Futures known to be done (`EFF101`): `f.result()` under `if f.done():` or after
//!   `await asyncio.wrap_future(f)` returns at once.
//! - Arguments that decide a guard (`EFF102`, `EFF101`): `load(name, cache=False)` does not
//!   reach the `if cache:` branch of the callee; neither does an omitted parameter whose literal
//!   default is falsy.
//! - Tests the caller has made (`EFF105`): a call made only when `isinstance(self._input,
//!   PathLike)` is false does not reach the callee's `isinstance(self._input, PathLike)` branch.
//! - Paths or buffers (`EFF105`, `EFF101`, `EFF102`): an `io.StringIO` handed to a helper that
//!   opens its argument only when `hasattr(buf, "write")` is false does not reach the `open`;
//!   neither does `None` where the helper rebinds it to a buffer before `isinstance(buf, str)`.
//!
//! Every fixture marks the lines that must be reported with the rule code in a comment
//! (`# EFF102`); every other line must not be.

mod common;

use common::Project;
use serde_json::Value;

/// `(path, line, rule)` of every diagnostic of `rules`.
fn reported(p: &Project, args: &[&str], rules: &[&str]) -> Vec<(String, u64, String)> {
    let mut all = vec!["--output-format", "json"];
    all.extend_from_slice(args);
    let (code, stdout, stderr) = p.check(&all);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<(String, u64, String)> = diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
            )
        })
        .filter(|(_, _, r)| rules.contains(&r.as_str()))
        .collect();
    out.sort();
    out
}

/// The `(path, line, rule)` marks of `files`: a line ending in `# <RULE>` for each of `rules`.
fn marked(files: &[(&str, &str)], rules: &[&str]) -> Vec<(String, u64, String)> {
    let mut out = Vec::new();
    for (path, text) in files {
        for (i, line) in text.lines().enumerate() {
            for rule in rules {
                if line.trim_end().ends_with(&format!("# {rule}")) {
                    out.push(((*path).to_owned(), i as u64 + 1, (*rule).to_owned()));
                }
            }
        }
    }
    out.sort();
    out
}

fn check_marks(name: &str, files: &[(&str, &str)], args: &[&str], rules: &[&str]) {
    let rules = common::with_siblings(rules);
    let rules = rules.as_slice();
    let p = Project::new(name);
    for (path, text) in files {
        p.write(path, text);
    }
    assert_eq!(reported(&p, args, rules), marked(files, rules));
}

// ====================================================================== lazy initialization

/// A connection opened only while it is unset, reached directly, through overriding subclasses,
/// through an attribute of the receiver, through a pooled object, and through an early return
/// whose handler re-raises in a `finally`; and the shapes that run per call.
const LAZY: &str = r#"import sqlite3

import requests


class Conn:
    def __init__(self) -> None:
        self._sock = None

    def _connect(self) -> object:
        requests.get("https://ocsp.example.com/")
        return object()

    def send(self, cmd: str) -> None:
        if not self._sock:
            self._sock = self._connect()

    def send_none(self, cmd: str) -> None:
        if self._sock is None:
            self._sock = self._connect()

    def send_none_left(self, cmd: str) -> None:
        if None is self._sock:
            self._sock = self._connect()

    def send_eq(self, cmd: str) -> None:
        if self._sock == None:
            self._sock = self._connect()

    def connect(self) -> None:
        if self._sock:
            return
        try:
            sock = self._connect()
        except OSError as e:
            err = ConnectionError("cannot connect")
            try:
                raise err from e
            finally:
                err = None
        self._sock = sock

    def connect_not_none(self) -> None:
        if self._sock is not None:
            return
        self._sock = self._connect()

    def connect_ne(self) -> object:
        if self._sock != None:
            return self._sock
        self._sock = self._connect()
        return self._sock

    def send_packed(self, data: bytes) -> None:
        if not self._sock:
            self.connect()

    def always(self, cmd: str) -> None:
        self._sock = self._connect()

    def sometimes(self, cmd: str, ok: bool) -> None:
        if not self._sock:
            if ok:
                self._sock = self._connect()
            else:
                self._connect()

    def returns_unset(self) -> None:
        if not self._sock:
            self._connect()
            return
        self._sock = None

    def poll(self, urls: list[str]) -> None:
        for u in urls:
            if self._sock is None:
                requests.get(u)  # EFF102
        self._sock = object()


class SSLConn(Conn):
    def __init__(self, validate: bool) -> None:
        super().__init__()
        self.validate = validate

    def _connect(self) -> object:
        sock = super()._connect()
        if self.validate:
            requests.get("https://ocsp.example.com/")
        return sock


class Pool:
    def __init__(self) -> None:
        self.free: list[Conn] = []

    def get(self) -> Conn:
        return self.free.pop() if self.free else Conn()


class Client:
    def __init__(self, pool: Pool) -> None:
        self.pool = pool
        self.conn: Conn = Conn()

    def execute(self, cmd: str) -> None:
        self.pool.get().send(cmd)

    def reset(self) -> None:
        self.conn.send("UNWATCH")

    def ensure(self) -> None:
        if self.conn._sock is None:
            self.conn._sock = self.conn._connect()


class Cache:
    def __init__(self) -> None:
        self._session = None

    def fetch_all(self, urls: list[str]) -> None:
        for u in urls:
            if self._session is None:
                self._session = requests.Session()
            self._session.get(u)

    def fetch_inside(self, urls: list[str]) -> None:
        if self._session is None:
            for u in urls:
                requests.get(u)  # EFF102
            self._session = object()

    def fetch_fresh(self, urls: list[str]) -> None:
        for u in urls:
            c = Cache()
            if c._session is None:
                c._session = requests.Session()  # EFF102

    def fetch_either(self, urls: list[str], flag: bool) -> None:
        for u in urls:
            c = Cache() if flag else Cache()
            if c._session is None:
                c._session = requests.Session()  # EFF102


class Store:
    def __init__(self, cur: sqlite3.Cursor) -> None:
        self.cur = cur
        self._columns = None

    def columns(self) -> object:
        if self._columns is None:
            self._columns = self.cur.execute("SELECT * FROM meta")
        return self._columns

    def rows(self) -> object:
        return self.cur.execute("SELECT * FROM rows")


def send_to(conn: Conn, cmd: str) -> None:
    conn.send(cmd)


def run(conn: Conn, store: Store, client: Client, cmds: list[str], conns: list[Conn]) -> None:
    for cmd in cmds:
        conn.send(cmd)
        conn.send_none(cmd)
        conn.send_none_left(cmd)
        conn.send_eq(cmd)
        conn.connect()
        conn.connect_not_none()
        conn.connect_ne()
        conn.send_packed(b"x")
        conn.always(cmd)  # EFF102
        conn.sometimes(cmd, True)  # EFF102
        conn.returns_unset()  # EFF102
        Conn().send(cmd)  # EFF102
        send_to(conn, cmd)
        send_to(*conns)  # EFF102
        client.execute(cmd)
        client.reset()
        client.ensure()
        Client(Pool()).execute(cmd)  # EFF102
        store.columns()
        store.rows()  # EFF103
    c = Conn()
    for cmd in cmds:
        c.send(cmd)
    for k in conns:
        k.send("x")


def run_any(conn: Conn, cmds: list[str]) -> None:
    for cmd in cmds:
        conn.send(cmd)


async def handler(conn: Conn) -> None:
    conn.send("x")  # EFF111
"#;

#[test]
fn lazy_initialization_runs_once_per_object() {
    check_marks(
        "guards_lazy",
        &[("pkg/__init__.py", ""), ("pkg/lazy.py", LAZY)],
        &[],
        &["EFF101", "EFF102", "EFF103"],
    );
}

/// A lazy initialization written with `hasattr`, `getattr(..., None)`, `try` / `except
/// AttributeError` (also after a clause that takes another error), or a store through
/// `object.__setattr__` runs once per object as one under a `None` test does; one under `except
/// KeyError` runs on every call.
#[test]
fn lazy_initialization_spelled_otherwise_runs_once_per_object() {
    let source = r#"import requests


class Lazy:
    def __init__(self) -> None:
        self._conn = None

    def by_hasattr(self) -> None:
        if not hasattr(self, "_session"):
            self._session = requests.Session()

    def by_getattr(self) -> None:
        if getattr(self, "_client", None) is None:
            self._client = requests.Session()

    def by_try(self) -> object:
        try:
            return self._pool
        except AttributeError:
            self._pool = requests.Session()
            return self._pool

    def by_setattr(self) -> None:
        if self._conn is None:
            object.__setattr__(self, "_conn", requests.Session())

    def by_other_error(self) -> object:
        try:
            return self._other
        except KeyError:
            self._other = requests.Session()
            return self._other

    def by_second_clause(self) -> object:
        try:
            return self._second
        except KeyError:
            raise
        except AttributeError:
            self._second = requests.Session()
            return self._second


def run(lazy: Lazy, urls: list[str]) -> None:
    for u in urls:
        lazy.by_hasattr()
        lazy.by_getattr()
        lazy.by_try()
        lazy.by_setattr()
        lazy.by_other_error()  # EFF102
        lazy.by_second_clause()
"#;
    check_marks(
        "guards_lazy_spelled",
        &[("pkg/__init__.py", ""), ("pkg/lazy.py", source)],
        &[],
        &["EFF101", "EFF102", "EFF103"],
    );
}

/// A `functools.cached_property` getter runs once per instance, as a lazy initialization of
/// `self` does, through any number of calls: `cached_property` from `functools`, from a module
/// of the project that imports it, or from `kombu.utils`, which re-exports it. Reading one in
/// a loop on an instance the loop does not construct, also one of the instances a loop
/// iterates over, does not run it per iteration; on one it constructs in each iteration it
/// does. A plain `@property` runs on every read, and the event loop blocks the one time.
const CACHED: &str = r#"import functools
import sqlite3

import requests
from kombu.utils import cached_property as kombu_cached

from pkg.compat import cached_property


class Downloader:
    @functools.cached_property
    def db(self):
        return sqlite3.connect("cookies.db")

    @cached_property
    def session(self):
        return requests.Session()

    @kombu_cached
    def client(self):
        return requests.Session()

    @property
    def fresh(self):
        return requests.Session()

    @functools.cached_property
    def pooled(self):
        if self._pool is None:
            self._pool = requests.Session()
        return self._pool

    def available(self, target):
        return self.db is not None

    def first_comp(self, targets):
        return [t for t in targets if self.available(t)]

    def each(self, urls):
        for u in urls:
            self.session.get(u)
            self.client.get(u)
            self.pooled.get(u)
            self.fresh.get(u)  # EFF102


def loops(d: Downloader, ds: list[Downloader], targets):
    for t in targets:
        d.available(t)
        Downloader().available(t)  # EFF102
    for e in ds:
        e.available(1)


async def handler(d: Downloader):
    return d.db  # EFF111
"#;

#[test]
fn cached_properties_run_once_per_instance() {
    check_marks(
        "guards_cached",
        &[
            ("pkg/__init__.py", ""),
            ("pkg/compat.py", "from functools import cached_property\n"),
            ("pkg/cached.py", CACHED),
        ],
        &[],
        &["EFF101", "EFF102", "EFF103"],
    );
}

/// A local variable a loop carries, made only while it is unset (`if conn is None: conn =
/// connect()`, `conn = conn or connect()`, also a parameter's default), and a call only a
/// first-iteration flag lets run (`if first: ...; first = False`, `if not seen:`), run once
/// per run of the loop, as does a loop around both loops that keeps the variable. A module
/// global set the same way (`global _session`) is set once per process, for every caller.
/// Reported: a variable reset in the loop (also by a handler), or set there to a value the
/// branch does not compute (`prev = row`, which may be `None`), a branch that does not always
/// set it, a loop that starts it unset in each iteration, a flag set again in each iteration,
/// a global the branch does not set, and a global the function also sets back to `None` before
/// the test or after the initialization (one set back before a `raise` undoes a failed
/// initialization only).
const LOCAL_LAZY: &str = r#"import sqlite3

import requests

_session = None
_warned = False


def lazy_none(urls):
    s = None
    for u in urls:
        if s is None:
            s = requests.Session()
        s.get(u)


def lazy_not(urls, s=None):
    for u in urls:
        if not s:
            s = requests.Session()
        s.get(u)


def lazy_or(urls):
    s = None
    for u in urls:
        s = s or requests.Session()
        s.get(u)


def lazy_query(cur: sqlite3.Cursor, ids):
    cols = None
    for i in ids:
        if cols is None:
            cols = cur.execute("SELECT * FROM meta").fetchall()
        print(cols, i)


def lazy_outer_kept(groups):
    s = None
    for g in groups:
        for u in g:
            if s is None:
                s = requests.Session()
            s.get(u)


def lazy_marker(cur: sqlite3.Cursor, ids):
    marker = None
    for i in ids:
        if marker is None:
            cur.execute("SELECT 1")
            marker = 1


def lazy_last(urls):
    last = object()
    for u in urls:
        if last is not None:
            requests.get(u)
            last = None


def lazy_break(urls, c):
    if c:
        s = requests.Session()
    else:
        s = None
    for u in urls:
        if s is None:
            s = requests.Session()
            break


def lazy_reset(urls):
    s = None
    for u in urls:
        if s is None:
            s = requests.Session()  # EFF102
        s.get(u)
        s = None


def lazy_param_reset(urls, default):
    s = None
    for u in urls:
        if s is None:
            s = requests.Session()  # EFF102
        if not u:
            s = default


def lazy_sometimes(urls, ok):
    s = None
    for u in urls:
        if s is None:
            if ok:
                s = requests.Session()  # EFF102
            else:
                requests.Session()  # EFF102


def lazy_outer_reset(groups):
    for g in groups:
        s = None
        for u in g:
            if s is None:
                s = requests.Session()  # EFF102
            s.get(u)


def lazy_reset_on_error(urls):
    s = None
    for u in urls:
        try:
            if s is None:
                s = requests.Session()  # EFF102
            s.get(u)
        except OSError:
            s = None


def first_flag(cur: sqlite3.Cursor, ids):
    first = True
    for i in ids:
        if first:
            cur.execute("SELECT 1")
            first = False


def seen_flag(urls):
    seen = False
    for u in urls:
        if not seen:
            requests.get(u)
            seen = True


def prev_none(cur: sqlite3.Cursor, rows):
    prev = None
    for r in rows:
        if prev is None:
            cur.execute("SELECT 1")  # EFF113
        prev = r


def prev_reset(cur: sqlite3.Cursor, rows):
    prev = None
    for r in rows:
        if prev is None:
            cur.execute("SELECT 1")  # EFF113
        prev = r if r else None


def empty_reset(cur: sqlite3.Cursor, rows):
    cache = []
    for r in rows:
        if not cache:
            cache = cur.execute("SELECT 1").fetchall()  # EFF113
        if r:
            cache = []


def flag_set_each(urls):
    pending = None
    for u in urls:
        if pending is not None:
            requests.get(pending)  # EFF102
            pending = None
        pending = u


def session():
    global _session
    if _session is None:
        _session = requests.Session()
    return _session


def warn_once():
    global _warned
    if not _warned:
        _warned = True
        requests.get("https://example.com/warn")


def unset_global():
    global _session
    if _session is None:
        requests.Session()


def inline_global(urls):
    global _session
    for u in urls:
        if _session is None:
            _session = requests.Session()
        _session.get(u)


def reset_before():
    global _session
    _session = None
    if _session is None:
        _session = requests.Session()
    return _session


def reset_after():
    global _session
    if _session is None:
        _session = requests.Session()
    s = _session
    _session = None
    return s


def reset_sometimes(flag):
    global _session
    if _session is None:
        _session = requests.Session()
    s = _session
    if flag:
        _session = None
    return s


_conn = None


def connect_or_fail():
    global _conn
    if _conn is None:
        try:
            _conn = requests.Session()
        except OSError:
            _conn = None
            raise
    return _conn


def callers(urls):
    for u in urls:
        session().get(u)
        warn_once()
        connect_or_fail()
        unset_global()  # EFF102
        reset_before()  # EFF102
        reset_after()  # EFF102
        reset_sometimes(u)  # EFF102
"#;

#[test]
fn local_and_global_lazy_initialization() {
    check_marks(
        "guards_local_lazy",
        &[("pkg/__init__.py", ""), ("pkg/local_lazy.py", LOCAL_LAZY)],
        &[],
        &["EFF102", "EFF103"],
    );
}

/// A call every way on from which leaves the loop (`return`, `break`, `raise`, also after
/// other calls) runs at most once per run of the loop; a `break` out of an inner loop still
/// runs it once per iteration of the outer one, and a handler that goes on with the loop runs
/// it again.
const EXITS: &str = r#"import sqlite3

import requests


def first_match(urls):
    for u in urls:
        if u.startswith("https"):
            s = requests.Session()
            return s.get(u)
    return None


def first_break(urls):
    for u in urls:
        if u.startswith("https"):
            requests.get(u)
            print(u)
            break


def query_then_return(cur: sqlite3.Cursor, ids):
    while ids:
        cur.execute("SELECT 1")
        return ids.pop()
    return None


def raise_after(urls):
    for u in urls:
        if not u:
            requests.get("https://example.com/report")
            raise ValueError(u)


def inner_break(groups):
    for g in groups:
        for u in g:
            requests.get(u)  # EFF102
            break


def retry_on_error(urls):
    for u in urls:
        try:
            return requests.get(u)  # EFF102
        except OSError:
            continue
    return None


def per_item(urls):
    for u in urls:
        if u:
            requests.get(u)  # EFF102
"#;

#[test]
fn calls_before_the_loop_ends_run_once() {
    check_marks(
        "guards_exits",
        &[("pkg/__init__.py", ""), ("pkg/exits.py", EXITS)],
        &[],
        &["EFF102", "EFF103"],
    );
}

/// Waiting on a `concurrent.futures.Future` known to be done returns at once: `result()` or
/// `exception()` under `if f.done():` (after an early return, a `while not f.done():` poll, an
/// `and`), once `await asyncio.wrap_future(f)` has returned (also imported, or through a
/// copy), on an attribute the method does not assign, and in a helper. Reported: a future no
/// test or `await` on every path finished, a test of another future, and an event a done
/// callback sets, which is not followed.
const FUTURES: &str = r#"import asyncio
from asyncio import wrap_future
from concurrent.futures import Future, ThreadPoolExecutor


async def done_test(f: Future):
    if f.done():
        return f.result()
    return None


async def early_return(f: Future):
    if not f.done():
        return None
    return f.exception()


async def poll_until(f: Future):
    while not f.done():
        await asyncio.sleep(0.1)
    return f.result()


async def and_test(f: Future, ok: bool):
    if ok and f.done():
        return f.result()
    return None


async def wrapped(ex: ThreadPoolExecutor):
    f = ex.submit(print)
    await asyncio.wrap_future(f)
    return f.result()


async def imported(f: Future):
    await wrap_future(f)
    return f.result()


async def copied(f: Future):
    w = asyncio.wrap_future(f)
    await w
    return f.result()


class Holder:
    def __init__(self, f: Future):
        self._future = f

    async def get(self):
        if self._future.done():
            return self._future.result()
        return None

    async def other(self, g: Future):
        if g.done():
            return self._future.result()  # EFF111
        return None


def helper(f: Future):
    if f.done():
        return f.result()
    return None


async def through_helper(f: Future):
    return helper(f)


async def not_done(f: Future):
    return f.result()  # EFF111


async def or_test(f: Future, ok: bool):
    if ok or f.done():
        return f.result()  # EFF111
    return None


async def one_branch(f: Future, ok: bool):
    if ok:
        await asyncio.wrap_future(f)
    return f.result()  # EFF111


async def handler_path(f: Future):
    try:
        await asyncio.wrap_future(f)
    except OSError:
        pass
    return f.result()  # EFF111


class Waiter:
    async def __call__(self):
        return None


async def call_instance(f: Future, w: Waiter):
    await w()
    return f.result()  # EFF111


async def unpacked(fs: list[Future]):
    await asyncio.wrap_future(*fs)
    return fs[0].result()  # EFF111


async def event_set(f: Future):
    done = asyncio.Event()
    loop = asyncio.get_running_loop()
    f.add_done_callback(lambda _: loop.call_soon_threadsafe(done.set))
    await done.wait()
    return f.result()  # EFF111
"#;

#[test]
fn futures_known_to_be_done_do_not_block() {
    check_marks(
        "guards_futures",
        &[("pkg/__init__.py", ""), ("pkg/futures.py", FUTURES)],
        &[],
        &["EFF101"],
    );
}

// ======================================================================= constant arguments

/// A helper whose request runs only under `if cache:`, called with every kind of constant,
/// through defaults, unpacking, wrappers, and conditional definitions.
const CONSTANTS: &str = r#"import sys
from urllib.request import urlopen


def names() -> list[str]:
    with urlopen("https://example.com/names.txt") as resp:
        return resp.read().decode().split()


def load(name: str, cache: bool = True) -> str:
    if cache:
        if name not in names():
            raise ValueError(name)
    return name


def load_off(name: str, cache=False, *, fast=None) -> str:
    if not cache:
        return name
    names()
    return name


def maybe(name: str, session=None) -> str:
    if session is not None:
        names()
    return name


def fallback(name: str, session=None) -> str:
    if session is None:
        names()
    return name


def both(name: str, a=None, b=None) -> str:
    if a and b:
        names()
    return name


def either(name: str, a=None, b=None) -> str:
    if a or b:
        names()
    return name


def equal(name: str, a=None, b=None) -> str:
    if a == b:
        names()
    return name


def never(name: str, a=None) -> str:
    if a and not a:
        names()
    return name


def odd(name: str, a=None, b=None) -> None:
    if a:
        if (not a) or b:
            names()


def spin(items: list[str], ok=None) -> None:
    while not ok:
        ok = ok or items.pop()
    if ok:
        names()


def turn(items: list[str], ok=None) -> None:
    while True:
        ok = ok or items.pop()
        if len(items) < 2:
            break
    if ok:
        names()


def when_set(x=None) -> None:
    if x:
        names()


def when_none(x=None) -> None:
    if x is None:
        names()


def twice(cache: bool) -> None:
    if cache:
        names()
    names()


def guarded_twice(cache: bool) -> None:
    if cache:
        names()
    if cache:
        names()


def wrapper(name: str, cache: bool) -> str:
    return load(name, cache)


def rest(name: str, *more, flag=False) -> None:
    if flag:
        names()


def by_float(name: str, d=0.0) -> None:
    if d:
        names()


def by_str(name: str, e="") -> None:
    if e:
        names()


def by_bytes(name: str, f=b"") -> None:
    if f:
        names()


def by_negative(name: str, g=-1) -> None:
    if not g:
        names()


def by_copy(name: str, cache=None) -> None:
    wanted = cache is not None
    if wanted:
        names()


if sys.version_info >= (3, 12):

    def pick(cache=True) -> None:
        if cache:
            names()

    def pick_both(cache=True) -> None:
        if cache:
            names()
else:

    def pick(cache=True) -> None:
        names()

    def pick_both(cache=True) -> None:
        if cache:
            names()


def run(xs: list[str], flag: bool, opts: dict, more: list, v: object) -> None:
    for x in xs:
        load(x, cache=False)
        load(x, False)
        load(x, None)
        load(x, 0)
        load(x, 0.0)
        load(x, "")
        load(x, [])
        load(x, ())
        load(x, {})
        load(x, cache=True)  # EFF102
        load(x)  # EFF102
        load(x, flag)  # EFF102
        load(x, b"")  # EFF102
        load(x, ...)  # EFF102
        load(x, 1j)  # EFF102
        load(x, 1.5)  # EFF102
        load(x, "y")  # EFF102
        load(x, [1])  # EFF102
        load(x, (1,))  # EFF102
        load(x, {"k": 1})  # EFF102
        load(x, {**opts})  # EFF102
        load(x, f"{x}")  # EFF102
        load(*more)  # EFF102
        load(x, *more)  # EFF102
        load(x, **opts)  # EFF102
        off = False
        alias = off
        load(x, alias)
        items = []
        items.append(x)
        load(x, items)  # EFF102
        load_off(x)
        load_off(x, fast=1)
        load_off(x, True)  # EFF102
        maybe(x)
        maybe(x, None)
        maybe(x, session=x)  # EFF102
        fallback(x)  # EFF102
        fallback(x, "s")
        both(x, a=0, b=1)
        both(x, 1, 1)  # EFF102
        either(x, 0, 1)  # EFF102
        never(x, 1)  # EFF102
        equal(x, 1, 2)  # EFF102
        odd(x, 1, 0)
        odd(x, 1, 1)  # EFF102
        spin(more)  # EFF102
        turn(more)  # EFF102
        twice(False)  # EFF102
        guarded_twice(False)
        wrapper(x, False)
        wrapper(x, True)  # EFF102
        rest(x, 1)
        rest(x, 1, flag=True)  # EFF102
        by_float(x)
        by_str(x)
        by_bytes(x)  # EFF102
        by_negative(x)
        by_copy(x)
        by_copy(x, 1)  # EFF102
        pick(False)  # EFF102
        pick_both(False)
        if v is None:
            when_set(v)
        if v:
            when_none(v)
        if not v:
            when_set(v)
        if flag:
            when_set(v)  # EFF102


async def handler(x: str) -> None:
    load(x, cache=False)
    load(x, cache=True)  # EFF111
"#;

/// The decided call one call below the loop: `check_load_dataset(name)` calls
/// `load_dataset(name, cache=False)`.
const DATASETS: &str = r#"from urllib.request import urlopen


def get_dataset_names() -> list[str]:
    with urlopen("https://example.com/names.txt") as resp:
        return resp.read().decode().split()


def load_dataset(name: str, cache: bool = True, data_home=None, **kws) -> str:
    if isinstance(name, dict):
        raise TypeError(name)
    if cache:
        if name not in get_dataset_names():
            raise ValueError(name)
    return name


def check_load_dataset(name: str) -> None:
    load_dataset(name, cache=False)


def check_load_cached_dataset(name: str) -> None:
    load_dataset(name, cache=True, data_home="/tmp")


def test_load_datasets() -> None:
    for name in get_dataset_names():
        check_load_dataset(name)


def test_load_cached_datasets() -> None:
    for name in get_dataset_names():
        check_load_cached_dataset(name)  # EFF102
"#;

#[test]
fn constant_arguments_switch_guarded_effects_off() {
    check_marks(
        "guards_constants",
        &[
            ("pkg/__init__.py", ""),
            ("pkg/consts.py", CONSTANTS),
            ("pkg/datasets.py", DATASETS),
        ],
        &[],
        &["EFF101", "EFF102"],
    );
}

// =================================================================== the caller's own tests

/// A file opened only under `isinstance(self._input, PathLike)`, called where the caller has
/// decided that test (an early return, a conditional expression), and the shapes that keep it.
const PATHS: &str = r#"from os import PathLike
from pathlib import Path


class Source:
    def __init__(self, src: object) -> None:
        self._input = src
        self._file = None

    def open(self):
        if isinstance(self._input, PathLike) and self._file is None:
            self._file = Path(self._input).open("rb")
        return self._file

    async def aopen(self):
        if isinstance(self._input, PathLike):
            return None
        return self.open()

    async def aopen_expr(self):
        return None if isinstance(self._input, PathLike) else self.open()

    async def aopen_plain(self):
        return self.open()  # EFF105

    async def aopen_path(self):
        if not isinstance(self._input, PathLike):
            return None
        return self.open()  # EFF105

    async def aread_input(self):
        if isinstance(self._input, PathLike):
            return None
        return read(self._input)

    async def aopen_assigned(self, other: object):
        if isinstance(self._input, PathLike):
            return None
        self._input = other
        return self.open()  # EFF105

    def reopen(self, other: object):
        self._input = other
        return self.open()


async def areopen(s: Source, other: object):
    if isinstance(s._input, PathLike):
        return None
    return s.reopen(other)  # EFF105


class Thing:
    pass


def read(src: object):
    if isinstance(src, PathLike):
        return Path(src).read_text()
    return None


def read_text(src: object):
    if isinstance(src, (str, bytes)):
        return None
    return Path(src).read_text()


def read_str(src: object):
    if isinstance(src, str):
        return Path(src).read_text()
    return None


TEXT_KINDS = (str, bytes)


def read_kinds(src: object):
    if isinstance(src, TEXT_KINDS):
        return None
    return Path(src).read_text()


def kind(x: object, kinds: object):
    if isinstance(x, PathLike):
        Path("k").read_text()
    if isinstance(x, kinds):
        return None
    return None


def thing(x: object):
    if isinstance(x, Thing):
        Path("t").read_text()


async def readers(p: object, q: object):
    read("literal")
    read(p)  # EFF105
    read_text("s")
    read_text(1)  # EFF105
    read_kinds(b"s")
    read_kinds(1)  # EFF105
    if isinstance(q, str):
        read_text(q)
        read_str(q)  # EFF105
    if not isinstance(q, (str, bytes)):
        read_str(q)
    if isinstance(q, str) and p:
        read_str(q)  # EFF105
    if not q:
        read_str(q)  # EFF105
    kind(None, q)
    kind(True, q)
    kind(1, q)
    kind(1.5, q)
    kind("s", q)
    kind(b"s", q)
    kind([1], q)
    kind((1,), q)
    kind({1}, q)
    kind({"a": 1}, q)
    kind(..., q)  # EFF105
    thing("s")  # EFF105
"#;

#[test]
fn correlated_isinstance_guards() {
    check_marks(
        "guards_isinstance",
        &[("pkg/__init__.py", ""), ("pkg/paths.py", PATHS)],
        &["--extend-select", "EFF105"],
        &["EFF105"],
    );
}

// =============================================================== paths or buffers

/// Helpers taking a path or a buffer: the file is opened (or the URL fetched) only on the path
/// branch, chosen by `hasattr`, `isinstance`, or `is None`, also after the helper rebinds its
/// parameter to a buffer on one branch.
const BUFFERS: &str = r#"import io
import os
from io import StringIO
from urllib.request import urlopen


def save(text, buf=None):
    if buf is None:
        return text
    elif hasattr(buf, "write"):
        buf.write(text)
    else:
        with open(buf, "w") as f:
            f.write(text)
    return None


def save_named(text, buf):
    if hasattr(buf, "name"):
        buf.write(text)
    else:
        with open(buf, "w") as f:
            f.write(text)


WRITE = "write"


def save_as(text, buf):
    if hasattr(buf, WRITE):
        buf.write(text)
    else:
        with open(buf, "w") as f:
            f.write(text)


def emit(text, out=None):
    if out:
        out.write(text)
        return
    with open("log.txt", "a") as f:
        f.write(text)


def write_path(text, dest):
    if isinstance(dest, (str, os.PathLike)):
        with open(dest, "w") as f:
            f.write(text)
    else:
        dest.write(text)


def write_io(text, dest):
    if isinstance(dest, io.IOBase):
        dest.write(text)
    else:
        with open(dest, "w") as f:
            f.write(text)


def write_typed(text, dest):
    if issubclass(type(dest), io.IOBase):
        dest.write(text)
    else:
        with open(dest, "w") as f:
            f.write(text)


def rebound(text, buf=None):
    if buf is not None:
        buf = os.fspath(buf)
    else:
        buf = StringIO()
    if hasattr(buf, "write"):
        buf.write(text)
    elif isinstance(buf, str):
        with open(buf, "w") as f:
            f.write(text)


def rebound_default(text, buf=None):
    if buf is None:
        buf = StringIO()
    if isinstance(buf, str):
        with open(buf, "w") as f:
            f.write(text)


def rebound_named(text, buf=None):
    if buf is None:
        buf = StringIO()
    target = buf
    if isinstance(target, str):
        with open(target, "w") as f:
            f.write(text)


def carried(text, buf=None, more=()):
    if buf is None:
        buf = StringIO()
    for p in more:
        if isinstance(buf, str):
            with open(buf, "w") as f:
                f.write(text)
        buf = p


def read_any(src):
    if hasattr(src, "read"):
        return src.read()
    return urlopen(src).read()


def lines(items):
    if isinstance(items, str):
        with open(items) as f:
            return f.read().split()
    return items


class Chain:
    def __init__(self, certs):
        self.certs = lines(certs)


class Buffer(StringIO):
    pass
"#;

/// Callers handing a buffer or a path: a buffer or `None` decides the test; a path, an
/// argument of unknown class, a subclass of a buffer, a value that takes the path branch, or a
/// test of an attribute a buffer may lack, or of one not named by a string literal, does not.
/// (More than ten callers reaching `open` would be reported together, so the callers are in two
/// projects.)
const BUFFER_USES: &str = r#"import io
from io import BytesIO, StringIO

from pkg.buffers import Buffer, save, save_as, save_named, write_io, write_path


async def saving(t, p):
    save(t)
    save(t, None)
    save(t, StringIO())
    save(t, io.StringIO())
    save(t, BytesIO())
    buf = StringIO()
    save(t, buf)
    save(t, "out.txt")  # EFF105
    save(t, p)  # EFF105
    save(t, [])  # EFF105
    save(t, Buffer())  # EFF105
    save_named(t, StringIO())  # EFF105
    save_as(t, StringIO())  # EFF105


async def paths(t, p):
    write_path(t, StringIO())
    write_path(t, BytesIO())
    write_path(t, p)  # EFF105
    write_path(t, "out.txt")  # EFF105
    write_io(t, StringIO())
    write_io(t, "out.txt")  # EFF105
    write_io(t, None)  # EFF105
"#;

/// Callers of the helpers rebinding their parameter, testing a buffer's truth or its class
/// through `issubclass` (no guard), of a reader of a path, a URL, or a buffer (in an `async def`
/// and in loops), and of a helper given a display, a comprehension, or a phi of both.
const REBINDING_USES: &str = r#"from io import BytesIO, StringIO

from pkg.buffers import (
    Chain,
    carried,
    emit,
    read_any,
    rebound,
    rebound_default,
    rebound_named,
    write_typed,
)


async def rebinding(t, p):
    rebound(t)
    rebound(t, None)
    rebound(t, "out.txt")  # EFF105
    rebound_default(t)
    rebound_default(t, StringIO())
    rebound_default(t, "out.txt")  # EFF105
    rebound_default(t, p)  # EFF105
    carried(t, None, ["out.txt"])  # EFF105
    rebound_named(t)
    rebound_named(t, "out.txt")  # EFF105


async def flags(t):
    emit(t, StringIO())
    emit(t)  # EFF105
    write_typed(t, StringIO())  # EFF105


async def reading(src, data):
    read_any(BytesIO(data))
    read_any(StringIO("x"))
    read_any(src)  # EFF111
    read_any("https://example.com/")  # EFF111
    read_any(None)  # EFF111
    read_any(...)  # EFF111


def read_all(srcs, blobs):
    for b in blobs:
        read_any(BytesIO(b))
    for s in srcs:
        read_any(s)  # EFF102


async def chains(xs):
    certs = []
    if xs:
        certs = [x for x in xs]
    Chain(certs)
    Chain([x for x in xs])
    Chain({x for x in xs})
    Chain({x: x for x in xs})
    Chain("certs.pem")  # EFF105
"#;

#[test]
fn buffer_arguments_switch_path_branches_off() {
    let rules = ["EFF101", "EFF102", "EFF105"];
    for (name, uses) in [("buffers", BUFFER_USES), ("rebinding", REBINDING_USES)] {
        let files = [
            ("pkg/__init__.py", ""),
            ("pkg/buffers.py", BUFFERS),
            ("pkg/uses.py", uses),
        ];
        // Python 3.11 builds a comprehension in a frame of its own.
        for version in ["py311", "py312"] {
            check_marks(
                &format!("guards_{name}_{version}"),
                &files,
                &["--extend-select", "EFF105", "--target-version", version],
                &rules,
            );
        }
    }
}

/// The guards of a callee in another module reach its callers through the index.
#[test]
fn guards_cross_modules() {
    check_marks(
        "guards_modules",
        &[
            ("pkg/__init__.py", ""),
            (
                "pkg/net.py",
                "from urllib.request import urlopen\n\n\ndef fetch(url, cache=True):\n    if cache:\n        return urlopen(url)\n    return None\n",
            ),
            (
                "pkg/use.py",
                "from pkg.net import fetch\n\n\ndef run(urls):\n    for u in urls:\n        fetch(u, cache=False)\n        fetch(u)  # EFF102\n",
            ),
        ],
        &[],
        &["EFF102"],
    );
}

// ============================================================ generators and nested loops

/// A generator's guards are decided at the call that creates it, and its body runs where it is
/// iterated; a lazy initialization counts per loop: a data-sized loop that constructs the object
/// in each iteration is reported, a loop that keeps it is not.
const ITERATED: &str = r#"import requests
from urllib.request import urlopen


def names():
    with urlopen("https://example.com/names.txt") as resp:
        return resp.read().decode().split()


def rows(name, cache=True):
    if cache:
        yield from names()
    yield name


def run(xs):
    for x in xs:
        for r in rows(x, cache=False):
            print(r)
        for r in rows(x):  # EFF102
            print(r)
        list(rows(x, False))
        list(rows(x))  # EFF102


def gen_wrap(x, cache):
    return rows(x, cache)


def run_wrap(xs):
    for x in xs:
        list(gen_wrap(x, False))
        list(gen_wrap(x, True))  # EFF102


async def handler(x):
    list(rows(x, cache=False))
    list(rows(x))  # EFF111


class Conn:
    def __init__(self):
        self._sock = None

    def send(self, cmd):
        if self._sock is None:
            self._sock = requests.Session()


class Poller:
    def __init__(self):
        self._session = None

    def poll_nested(self, hosts, urls):
        for h in hosts:
            for u in urls:
                if self._session is None:
                    self._session = requests.Session()

    def poll_outer(self, hosts, urls):
        for h in hosts:
            if self._session is None:
                for u in urls:
                    requests.get(u)  # EFF102
                self._session = object()


class Feed:
    def __init__(self):
        self._session = None

    def items(self):
        if self._session is None:
            self._session = requests.Session()
        yield 1


def consume(feed: Feed, xs):
    for x in xs:
        # Where a generator's facts run elsewhere than at its call, no loop is relaxed.
        for i in feed.items():  # EFF102
            print(i)


def nested(hosts, cmds):
    for h in hosts:
        c = Conn()
        for cmd in ("a", "b"):
            c.send(cmd)  # EFF102
    d = Conn()
    for h in hosts:
        for cmd in cmds:
            d.send(cmd)
    for h in hosts:
        e = Conn()
        for cmd in cmds:
            e.send(cmd)  # EFF102
"#;

#[test]
fn guards_with_generators_and_nested_loops() {
    check_marks(
        "guards_iterated",
        &[("pkg/__init__.py", ""), ("pkg/iterated.py", ITERATED)],
        &[],
        &["EFF101", "EFF102"],
    );
}

/// A list display decides a truthiness test of the helper only when an element is written as
/// itself: `[*parts]` is empty when `parts` is.
#[test]
fn an_unpacked_display_decides_no_truthiness_test() {
    check_marks(
        "unpacked_display_truthiness",
        &[
            ("pkg/__init__.py", ""),
            (
                "pkg/fetch.py",
                "from urllib.request import urlopen\n\n\ndef fetch(xs):\n    if not xs:\n        return urlopen(\"http://example.com\").read()\n    return b\"\"\n",
            ),
            (
                "pkg/use.py",
                "from pkg.fetch import fetch\n\n\nasync def known(parts):\n    return fetch([1, *parts])\n\n\nasync def unpacked(parts):\n    return fetch([*parts])  # EFF111\n",
            ),
        ],
        &[],
        &["EFF101"],
    );
}
