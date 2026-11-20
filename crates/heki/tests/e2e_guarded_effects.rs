//! Guarded effects end to end (`heki_effect::guards`): an effect is not counted where a guard
//! on its path makes it not happen.
//!
//! - Lazy initialization (`EFF102` / `EFF103`): a client or query created only while an
//!   attribute is unset, which the code then sets, runs once per object, not on every loop
//!   iteration.
//! - Arguments that decide a guard (`EFF102`, `EFF101`): `load(name, cache=False)` does not
//!   reach the `if cache:` branch of the callee; neither does an omitted parameter whose literal
//!   default is falsy.
//! - Tests the caller has made (`EFF105`): a call made only when `isinstance(self._input,
//!   PathLike)` is false does not reach the callee's `isinstance(self._input, PathLike)` branch.
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
    conn.send("x")  # EFF101
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
    load(x, cache=True)  # EFF101
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
    list(rows(x))  # EFF101


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
