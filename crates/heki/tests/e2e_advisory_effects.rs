//! Execution effects end to end (`EFF101` / `EFF102` / `DATA101`): receiver and callee
//! resolution (imports, re-exports, module globals, annotations, MRO / CHA, `super()`,
//! class methods), local effect extraction (conditional definitions, comprehensions,
//! generator expressions), `sync_driven_only` coroutine drivers, and the help of each effect.

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

/// `(path, line, rule, first callee of the chain)` of every diagnostic.
fn found(diags: &[Value]) -> Vec<(String, u64, String, String)> {
    diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
                d["related"]
                    .get(0)
                    .and_then(|r| r["message"].as_str())
                    .unwrap_or_default()
                    .rsplit(" calls ")
                    .next()
                    .unwrap_or_default()
                    .trim_matches('`')
                    .to_owned(),
            )
        })
        .collect()
}

fn d(path: &str, line: u64, rule: &str, callee: &str) -> (String, u64, String, String) {
    (path.into(), line, rule.into(), callee.into())
}

const MODELS: &str = r#"import time


class Base:
    def save(self):
        time.sleep(1)

    @property
    def name(self):
        return "base"


class Sub(Base):
    pass


class Child(Base):
    @property
    def name(self):
        return super().name + "!"


class A:
    def run(self):
        time.sleep(1)


class B(A):
    pass


class C(A):
    pass


class D(B, C):
    pass


class K:
    @classmethod
    def make(cls, n):
        return cls.helper(n)

    @staticmethod
    def helper(n):
        time.sleep(n)


class Fetcher:
    def __call__(self, url):
        time.sleep(1)


class Plugin:
    # A class-level registration on another class is not a field of `Plugin`.
    Base.registry: dict = {}


fetch = Fetcher()


class Holder:
    def plain(self):
        s = self
        s.x = 1

    def branchy(self, flag):
        if flag:
            o = self
        else:
            o = self
        o.y = 2
"#;

const HANDLERS: &str = r#"from typing import Union

import httpx
import requests

import app
from app import clients, plugins
from app.models import A, Base, D, K, fetch


async def via_alias(c: app.transport.Client):
    c.fetch()


async def via_namespace():
    plugins.x.run()


async def via_global():
    clients.session.get("u")


async def via_global_call():
    clients.hook()


async def via_const():
    return clients.TIMEOUT


async def via_union(s: Union[requests.Session, None]):
    s.get("u")


async def via_union_two(s: Union[requests.Session, httpx.Client]):
    s.get("u")


async def via_callable():
    fetch("u")


async def via_unbound(x):
    Base.save(x)
    Base.missing(x)


async def via_diamond(d: D):
    d.run()


async def via_cha_diamond(a: A):
    a.run()


async def via_cha(b: Base):
    b.save()


async def local_import():
    import time

    time.sleep(1)


async def via_classmethod():
    K.make(1)


async def via_genexpr(urls):
    return list(requests.get(u) for u in urls)


def dict_comp(xs):
    return {k: k for k in xs}
"#;

#[test]
fn eff101_resolves_receivers_through_imports_globals_annotations_and_classes() {
    let p = Project::new("effects_resolution");
    p.write(
        "app/__init__.py",
        "from app import net as transport\nfrom . import plugins\n",
    )
    .write(
        "app/net.py",
        "import time\n\n\nclass Client:\n    def fetch(self):\n        time.sleep(1)\n",
    )
    // A namespace subpackage (no `__init__.py`) imported by the package.
    .write(
        "app/plugins/x.py",
        "import time\n\n\ndef run():\n    time.sleep(1)\n",
    )
    .write(
        "app/clients.py",
        "import time\n\nimport requests\n\nsession = requests.Session()\nTIMEOUT = 5\n\n\nclass Hook:\n    def __call__(self):\n        time.sleep(1)\n\n\nhook = Hook()\n",
    )
    .write("app/models.py", MODELS)
    .write("app/handlers.py", HANDLERS);
    let diags = diagnostics(&p, &[]);
    let h = "app/handlers.py";
    assert_eq!(
        found(&diags),
        vec![
            // `app.transport` re-exports the module `app.net`.
            d(h, 12, "EFF101", "app.net.Client.fetch"),
            // `from . import plugins` names a namespace subpackage.
            d(h, 16, "EFF101", "app.plugins.x.run"),
            // A module global holding an instance.
            d(h, 20, "EFF101", "requests.Session.get"),
            // `Union[T, None]` is `T`; a union of two types stays unresolved (line 36).
            d(h, 32, "EFF101", "requests.Session.get"),
            // A callable instance imported from another module (`__call__`).
            d(h, 40, "EFF101", "app.models.Fetcher.__call__"),
            // An unbound call through the class (`Base.missing` does not exist).
            d(h, 44, "EFF101", "app.models.Base.save"),
            // MRO through a diamond, and CHA over a diamond of subclasses.
            d(h, 49, "EFF101", "app.models.A.run"),
            d(h, 53, "EFF101", "app.models.A.run"),
            // CHA over a subclass that does not override the method.
            d(h, 57, "EFF101", "app.models.Base.save"),
            // A function-local import.
            d(h, 63, "EFF101", "time.sleep"),
            // A class method calling a static method through `cls`.
            d(h, 67, "EFF111", "app.models.K.make"),
            // A generator expression runs where it is consumed.
            d(h, 71, "EFF102", "requests.get"),
            d(
                h,
                71,
                "EFF111",
                "app.handlers.via_genexpr.<locals>.<genexpr>"
            ),
        ],
        "{diags:#?}"
    );
    // Not resolved: calling a module-level instance through its module (`clients.hook()`).
    assert!(!diags.iter().any(|x| x["line"] == 24), "{diags:#?}");
}

const RESOLUTION: &str = r#"from typing import Optional

import boto3
import httpx
import requests

from .util import helper


class Factory:
    def client(self) -> httpx.Client:
        return httpx.Client()


class Loader:
    def load(self):
        helper()


class Remote(Loader):
    async def refresh(self):
        super().load()


class Job:
    def run(self):
        return 1


class SlowJob(Job):
    def run(self):
        helper()


async def relative():
    helper()


async def pipe_none(c: httpx.Client | None):
    c.get("u")


async def pipe_two(c: httpx.Client | requests.Session):
    c.get("u")


async def optional(c: Optional[httpx.Client]):
    c.get("u")


async def returned(f: Factory):
    f.client().get("u")


async def overridden(j: Job):
    j.run()


async def storage():
    bucket = boto3.client("s3")
    bucket.get_object(Bucket="b", Key="k")
"#;

/// A base class whose `go` is overridden by `subclasses` subclasses (the first one blocks),
/// and an `async def` calling `go` on the base type.
fn overrides(subclasses: usize) -> String {
    let mut src = String::from(
        "from pkg.util import helper\n\n\nclass Base:\n    def go(self):\n        return 0\n",
    );
    for i in 0..subclasses {
        let body = if i == 0 {
            "helper()".to_owned()
        } else {
            format!("return {i}")
        };
        src.push_str(&format!(
            "\n\nclass Sub{i}(Base):\n    def go(self):\n        {body}\n"
        ));
    }
    src.push_str("\n\nasync def fan_out(b: Base):\n    b.go()\n");
    src
}

#[test]
fn eff101_resolves_pipe_and_optional_annotations_super_returns_and_overrides() {
    let p = Project::new("effects_more_resolution");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.ecosystem]]\nsymbol = \"boto3.client.get_object\"\neffects = [\"blocking_net\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write(
        "pkg/util.py",
        "import time\n\n\ndef helper():\n    time.sleep(1)\n",
    )
    .write("pkg/api.py", RESOLUTION)
    .write("pkg/many.py", &overrides(8));
    let diags = diagnostics(&p, &[]);
    let a = "pkg/api.py";
    assert_eq!(
        found(&diags),
        vec![
            // `super().load()` binds the base class method.
            d(a, 22, "EFF101", "pkg.api.Loader.load"),
            // `from .util import helper` inside a package.
            d(a, 36, "EFF101", "pkg.util.helper"),
            // `T | None` and `Optional[T]` are `T`; a union of two types stays unresolved
            // (line 44).
            d(a, 40, "EFF101", "httpx.Client.get"),
            d(a, 48, "EFF101", "httpx.Client.get"),
            // A method's return annotation types its result.
            d(a, 52, "EFF101", "httpx.Client.get"),
            // CHA over a subclass overriding the method.
            d(a, 56, "EFF111", "pkg.api.SlowJob.run"),
            // A Layer 2 constructor with a lowercase name (`boto3.client`) returns a client
            // whose methods the configuration catalogues.
            d(a, 61, "EFF101", "boto3.client.get_object"),
        ],
        "{diags:#?}"
    );
    // `pkg/many.py`: `Base.go` and 8 overrides are more candidates than CHA follows
    // (`CHA_CANDIDATE_LIMIT` = 8), so `b.go()` stays unresolved. With 7 overrides it is not.
    p.write("pkg/many.py", &overrides(7));
    assert_eq!(
        found(&diagnostics(&p, &["pkg/many.py", "pkg/util.py"])),
        vec![d("pkg/many.py", 45, "EFF111", "pkg.many.Sub0.go")]
    );
}

#[test]
fn eff101_merges_conditional_definitions_and_follows_python_311_comprehensions() {
    let p = Project::new("effects_local");
    p.write(
        "pkg/dup.py",
        "import sys\nimport time\n\nif sys.version_info >= (3, 12):\n    def pause(items):\n        time.sleep(1)\n        items.append(1)\nelse:\n    def pause(items):\n        time.sleep(2)\n        items.append(2)\n\n\nasync def caller():\n    pause([])\n\n\nif sys.version_info >= (3, 12):\n    def push(items, extra):\n        extra.append(items)\nelse:\n    def push(items):\n        return items\n",
    )
    .write(
        "pkg/comps.py",
        "import requests\n\n\nasync def fetch_all(urls):\n    return [requests.get(u) for u in urls]\n",
    );
    // Both definitions of `pause` count (their facts are merged).
    let diags = diagnostics(&p, &["pkg/dup.py"]);
    assert_eq!(
        found(&diags),
        vec![d("pkg/dup.py", 15, "EFF111", "pkg.dup.pause")]
    );
    let (_, stdout, _) = p.heki(&["snapshot", "pkg/dup.py"]);
    let snap: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        snap["functions"]["pkg.dup:pause"]["mutates_params"],
        serde_json::json!(["items"])
    );
    // The entry describes the last definition: a parameter only the other one has is named
    // by its position.
    assert_eq!(
        snap["functions"]["pkg.dup:push"],
        serde_json::json!({"mutates_params": ["#1"], "escapes_params": ["items"]})
    );
    // `MUT103` compares through the same definition: unchanged code reports nothing (the first
    // definition's `extra` is the snapshot's `#1`), and a new effect is reported once, at the
    // last definition.
    p.heki(&["snapshot", "pkg/dup.py", "--write"]);
    let mut103 = |p: &Project| -> Vec<(String, u64)> {
        diagnostics(p, &["pkg/dup.py", "--select", "MUT103"])
            .iter()
            .map(|d| {
                (
                    d["qualname"].as_str().unwrap().to_owned(),
                    d["line"].as_u64().unwrap(),
                )
            })
            .collect()
    };
    assert_eq!(mut103(&p), vec![]);
    let dup = p.read("pkg/dup.py").replace(
        "        extra.append(items)\n",
        "        extra.append(items)\n        pause([])\n",
    );
    p.write("pkg/dup.py", &dup);
    assert_eq!(mut103(&p), vec![("push".to_owned(), 23)]);
    // Python 3.12 inlines the comprehension (PEP 709); Python 3.11 calls its own frame.
    let diags = diagnostics(&p, &["pkg/comps.py"]);
    assert_eq!(
        found(&diags),
        vec![
            d("pkg/comps.py", 5, "EFF101", "requests.get"),
            d("pkg/comps.py", 5, "EFF102", "requests.get"),
        ]
    );
    let diags = diagnostics(&p, &["pkg/comps.py", "--target-version", "py311"]);
    // Through the comprehension's frame the call runs once per item: not on every path.
    let eff101 = &diags[0];
    assert_eq!(eff101["rule_code"], "EFF111");
    assert_eq!(eff101["column"], 12, "at the comprehension call");
    assert_eq!(
        eff101["related"][0]["message"],
        "`fetch_all` calls `pkg.comps.fetch_all.<locals>.<listcomp>`",
        "{diags:#?}"
    );
}

#[test]
fn sync_driven_coroutines_and_route_decorators() {
    let p = Project::new("effects_drivers");
    p.write("pkg/__init__.py", "").write(
        "pkg/drivers.py",
        r#"import asyncio
import functools
import time
from asyncio import run

from fastapi import FastAPI

app = FastAPI()


async def _by_function():
    time.sleep(1)


async def _by_method():
    time.sleep(2)


async def _closed():
    time.sleep(3)


async def _main():
    time.sleep(4)


async def _entry():
    time.sleep(7)


@app.get("/")
async def _route():
    time.sleep(5)


@functools.lru_cache
async def _wrapped():
    time.sleep(6)


def run_sync(coro):
    return coro


def drive(portal):
    run_sync(_by_function())
    portal.run_sync(_by_method())
    c = _closed()
    c.close()
    asyncio.run(_main())
    run(_entry())
    _route().send(None)
    _wrapped().send(None)
"#,
    );
    let diags = diagnostics(&p, &[]);
    let f = "pkg/drivers.py";
    // Driven by `run_sync(...)`, `portal.run_sync(...)`, or `.send(None)`: relaxed. A
    // coroutine that is only closed, or run by `asyncio.run` / `run`, runs on an event loop;
    // a web route is never relaxed.
    assert_eq!(
        found(&diags),
        vec![
            d(f, 20, "EFF101", "time.sleep"),
            d(f, 24, "EFF101", "time.sleep"),
            d(f, 28, "EFF101", "time.sleep"),
            d(f, 33, "EFF101", "time.sleep"),
        ]
    );
}

#[test]
fn eff103_reads_query_constants_with_non_ascii_text() {
    // The DDL prefix test once sliced the constant by bytes and panicked inside a multi-byte
    // character, on any call in a loop whose first argument is such a string (`✓` is three
    // bytes, so bytes 4 and 5 fall inside the second one).
    let p = Project::new("effects_non_ascii_constants");
    p.write(
        "db.py",
        r#"import sqlite3


def g(t):
    return t


def label(xs):
    for x in xs:
        g("✓✓✓ multi-byte symbols ✓✓✓")


def query(cur: sqlite3.Cursor, ids):
    for i in ids:
        cur.execute("SELECT * FROM € WHERE id = ?", (i,))


def ddl(cur: sqlite3.Cursor, names):
    for n in names:
        cur.execute("  create table € (a int)")
"#,
    );
    let diags = diagnostics(&p, &["--select", "EFF103,EFF113"]);
    let got: Vec<(&str, &str)> = diags
        .iter()
        .map(|d| {
            (
                d["rule_code"].as_str().unwrap(),
                d["qualname"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(got, vec![("EFF103", "query")], "{diags:#?}");
}

#[test]
fn a_project_root_that_is_itself_a_package() {
    let p = Project::new("effects_root_package");
    p.write("__init__.py", "from .util import helper\n")
        .write(
            "util.py",
            "import time\n\n\ndef helper():\n    time.sleep(1)\n",
        )
        .write(
            "api.py",
            "from . import util\nfrom .util import helper\n\n\nasync def handler():\n    helper()\n    util.helper()\n",
        );
    let diags = diagnostics(&p, &[]);
    // The root is the package of the top-level modules: `from .util import helper` and
    // `from . import util` both resolve to `util`.
    assert_eq!(
        found(&diags),
        vec![
            d("api.py", 6, "EFF101", "util.helper"),
            d("api.py", 7, "EFF101", "util.helper")
        ]
    );
}

#[test]
fn data101_needs_a_quadratic_call_in_the_self_accumulation() {
    let p = Project::new("effects_data101");
    p.write(
        "frames.py",
        r#"import pandas as pd


def grow(df, fallback, parts):
    for part in parts:
        if part:
            df = pd.concat([df, part])
        else:
            df = fallback
    return df


def total(acc, parts):
    for part in parts:
        acc = combine(acc, part)
    return acc


def combine(a, b):
    return a + b
"#,
    );
    let diags = diagnostics(&p, &[]);
    assert_eq!(
        found(&diags),
        vec![d("frames.py", 7, "DATA101", "pandas.concat")]
    );
    assert_eq!(diags[0]["target"], "df");
}

#[test]
fn py_module_beats_sibling_pyi_stub_end_to_end() {
    let p = Project::new("effects_py_over_pyi");
    p.write("pkg/__init__.py", "")
        .write(
            "pkg/util.py",
            "import time\n\n\ndef helper():\n    time.sleep(1)\n",
        )
        .write("pkg/util.pyi", "def helper() -> None: ...\n")
        .write(
            "pkg/api.py",
            "from pkg.util import helper\n\n\nasync def handler():\n    helper()\n",
        )
        .write("pkg/api.pyi", "async def handler() -> None: ...\n");
    let diags = diagnostics(&p, &[]);
    assert_eq!(
        found(&diags),
        vec![d("pkg/api.py", 5, "EFF101", "pkg.util.helper")],
        "{diags:#?}"
    );
}

#[test]
fn eff103_tracks_db_connection_and_cursor_factories_without_annotations() {
    let p = Project::new("effects_db_factories");
    p.write(
        "queries.py",
        r#"import psycopg
import sqlite3
import sqlalchemy


def via_sqlite(ids):
    conn = sqlite3.connect("app.db")
    cur = conn.cursor()
    for i in ids:
        conn.execute("SELECT 1 WHERE id = ?", (i,))
        cur.execute("SELECT 2 WHERE id = ?", (i,))


def via_psycopg_and_sa(ids):
    pg = psycopg.connect("dbname=test")
    pg_cur = pg.cursor()
    engine = sqlalchemy.create_engine("sqlite://")
    sa_conn = engine.connect()
    for i in ids:
        pg.execute("SELECT 1")
        pg_cur.execute("SELECT 2")
        sa_conn.execute("SELECT 3")
"#,
    );
    let diags = diagnostics(&p, &["--select", "EFF103,EFF113"]);
    assert_eq!(
        found(&diags),
        vec![
            d("queries.py", 10, "EFF103", "sqlite3.Connection.execute"),
            d("queries.py", 11, "EFF103", "sqlite3.Cursor.execute"),
            d("queries.py", 20, "EFF103", "psycopg.Connection.execute"),
            d("queries.py", 21, "EFF103", "psycopg.Cursor.execute"),
            d(
                "queries.py",
                22,
                "EFF103",
                "sqlalchemy.engine.Connection.execute"
            ),
        ],
        "{diags:#?}"
    );
}

// ================================================================= one sentence, then the help

const EFFECTS: &str = r#"import sqlite3
import subprocess
import threading
import time

import requests

LOCK = threading.Lock()


async def sleeps():
    time.sleep(1)


async def fetches():
    return requests.get("https://example.com", timeout=5)


async def queries(path):
    return sqlite3.connect(path)


async def runs():
    return subprocess.run(["ls"], check=True)


async def waits():
    LOCK.acquire()


async def reads(path):
    with open(path) as f:
        return f.read()


def get(url):
    return requests.get(url, timeout=5)


def clients(urls):
    for url in urls:
        requests.get(url, timeout=5)
        get(url)


def lookup(cursor: sqlite3.Cursor, i):
    cursor.execute("SELECT name FROM users WHERE id = ?", (i,))


def names(cursor: sqlite3.Cursor, ids):
    for i in ids:
        cursor.execute("SELECT name FROM users WHERE id = ?", (i,))
        lookup(cursor, i)
"#;

/// The message says what happens and the help what to do: each blocking effect gets the
/// replacement that fits it, and a loop's report names the call with the effect only when a
/// helper reaches it.
#[test]
fn each_effect_has_its_own_help() {
    let p = Project::new("effects_help");
    p.write("app.py", EFFECTS);
    let diags = diagnostics(&p, &["--extend-select", "EFF105"]);
    let reports: Vec<(u64, &str, &str)> = diags
        .iter()
        .map(|d| {
            (
                d["line"].as_u64().unwrap(),
                d["message"].as_str().unwrap(),
                d["help"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    let offload = "offload it with `asyncio.to_thread`, or";
    let client = "create one client or session before the loop and reuse it";
    // An exact claim's report has no advice beyond the claim.
    let none = "";
    assert_eq!(
        reports,
        vec![
            (
                12,
                "`sleeps` runs the waiting call `time.sleep` on the event loop",
                "use `await asyncio.sleep(...)`, or offload it with `asyncio.to_thread`"
            ),
            (
                16,
                "`fetches` runs the waiting call `requests.get` on the event loop",
                &format!("{offload} use an async client")
            ),
            (
                20,
                "`queries` reaches the blocking call `sqlite3.connect` on the event loop",
                &format!("{offload} use an async driver")
            ),
            (
                24,
                "`runs` runs the waiting call `subprocess.run` on the event loop",
                &format!("{offload} use `asyncio.create_subprocess_exec`")
            ),
            (
                28,
                "`waits` reaches the blocking call `threading.Lock.acquire` on the event loop",
                &format!(
                    "{offload} use the `asyncio` primitive (`asyncio.Lock`, `asyncio.Event`, `asyncio.Queue`)"
                )
            ),
            (
                32,
                "`reads` reaches the blocking filesystem call `builtins.open` on the event loop",
                &format!("{offload} use an async file API")
            ),
            (
                42,
                "`requests.get` allocates an unpooled client on every loop iteration",
                client
            ),
            (
                43,
                "`app.get` allocates an unpooled client (`requests.get`) on every loop iteration",
                client
            ),
            (
                52,
                "every iteration over parameter `ids` sends a database statement: `sqlite3.Cursor.execute`",
                none
            ),
            (
                53,
                "every iteration over parameter `ids` sends a database statement: `sqlite3.Cursor.execute` (through `app.lookup`)",
                none
            ),
        ]
    );
}

/// Each premise of the exact claims, established and not: every implementation of a call (two
/// sinks, a class object, an instance's `__call__`, `super()`, a union, subclasses that alone
/// define the method, a constructor through `cls`), the `with` statements whose managers never
/// suppress (an instance of a catalogued class, the result of a catalogued function or method)
/// and one that may, and the origins of a loop's items (a part of a parameter, the rows of a
/// query, the lines of a file or of its text, directory entries, a count; a receiver's attribute
/// and the target of a `with` are not established).
const PREMISES: &str = r#"import os
import sqlite3
import threading
import time
from pathlib import Path

import httpx
import requests
from sqlalchemy.engine import Engine


class Repo:
    def get(self, key):
        raise NotImplementedError


class Remote(Repo):
    def get(self, key):
        return requests.get(key)


class Local(Repo):
    def get(self, key):
        return requests.post(key)


class Factory:
    @classmethod
    def make(cls):
        return cls.load()

    @classmethod
    def load(cls):
        return requests.get("https://example.com")


class Caller:
    def __call__(self):
        time.sleep(1)


class Child(Remote):
    def get(self, key):
        return super().get(key)


def with_lock(lock: threading.Lock):
    with lock:
        print("held")
        time.sleep(1)


def with_file(p):
    with open(p) as f:
        f.read()
        time.sleep(1)


def with_client(url):
    with httpx.Client() as client:
        print(client)
        return requests.get(url)


def with_unknown(manager):
    with manager:
        print("entered")
        time.sleep(1)


async def tp_two_sinks(r: Repo):
    r.get("k")


async def tp_class_object():
    Factory.make()


async def tp_instance_call(c: Caller):
    c()


async def tp_super(c: Child):
    c.get("k")


async def tp_managers(lock: threading.Lock, manager):
    with_lock(lock)
    with_file("x")
    with_client("u")
    with_unknown(manager)


def per_part(conn: sqlite3.Connection, req):
    for i in req.ids:
        conn.execute("SELECT ?", (i,))


def per_line(conn: sqlite3.Connection, p):
    with open(p) as f:
        for line in f:
            conn.execute("SELECT ?", (line,))


def per_entry(conn: sqlite3.Connection, root):
    for name in os.listdir(root):
        conn.execute("SELECT ?", (name,))


def per_text_line(conn: sqlite3.Connection, p: Path):
    for line in p.read_text().splitlines():
        conn.execute("SELECT ?", (line,))


def per_count(conn: sqlite3.Connection, ids):
    for i in map(str, ids):
        conn.execute("SELECT ?", (i,))


class Store:
    def save(self, conn, x):
        raise NotImplementedError


class SqlStore(Store):
    def save(self, conn: sqlite3.Connection, x):
        conn.execute("INSERT INTO a VALUES (?)", (x,))


class OtherStore(Store):
    def save(self, conn: sqlite3.Connection, x):
        conn.executemany("INSERT INTO b VALUES (?)", [(x,)])


def per_store(store: Store, conn, xs):
    for x in xs:
        store.save(conn, x)


async def tp_union(r: Remote | Local):
    r.get("k")


class Built:
    def __init__(self):
        time.sleep(1)

    @classmethod
    def create(cls):
        return cls()


async def tn_subclass_constructed():
    Built.create()


class Plain:
    pass


class PlainSub(Plain):
    def run(self):
        time.sleep(1)


async def tp_method_only_subclasses_define(p: Plain):
    p.run()


def with_connection(engine: Engine):
    with engine.connect() as conn:
        print(conn)
        time.sleep(1)


async def tp_method_manager(engine):
    with_connection(engine)


class Batch:
    def run(self, conn: sqlite3.Connection):
        for x in self.items:
            conn.execute("SELECT ?", (x,))


def per_row(conn: sqlite3.Connection):
    for (i,) in conn.execute("SELECT id FROM t").fetchall():
        conn.execute("SELECT ?", (i,))


def per_open_line(conn: sqlite3.Connection, p):
    for line in open(p):
        conn.execute("SELECT ?", (line,))


def per_character(conn: sqlite3.Connection, p: Path):
    for ch in p.read_text():
        conn.execute("SELECT ?", (ch,))


from abc import ABC, abstractmethod


class Sender(ABC):
    @abstractmethod
    def send(self, x): ...


class Mailer(Sender):
    def send(self, x):
        return requests.post(x)


async def tp_abstract(s: Sender):
    s.send("x")


def per_row_of_a_file():
    conn = sqlite3.connect("app.db")
    for (i,) in conn.execute("SELECT id FROM t"):
        conn.execute("SELECT ?", (i,))
"#;

#[test]
fn exact_claims_establish_each_premise() {
    let p = Project::new("exact_premises");
    p.write("pkg/__init__.py", "")
        .write("pkg/premises.py", PREMISES);
    let diags = diagnostics(&p, &["--select", "EFF101,EFF111,EFF103,EFF113"]);
    let got: Vec<(u64, &str, &str)> = diags
        .iter()
        .map(|d| {
            (
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap(),
                d["message"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (
                72,
                "EFF101",
                "`get` always runs a waiting call on the event loop: `requests.get` (in `Remote.get`) or `requests.post` (in `Local.get`)"
            ),
            (
                76,
                "EFF101",
                "`pkg.premises.Factory.make` always runs the waiting call `requests.get` on the event loop (2 calls down, on every path)"
            ),
            (
                80,
                "EFF101",
                "`pkg.premises.Caller.__call__` always runs the waiting call `time.sleep` on the event loop (1 call down, on every path)"
            ),
            (
                84,
                "EFF101",
                "`pkg.premises.Child.get` always runs the waiting call `requests.get` on the event loop (2 calls down, on every path)"
            ),
            (
                88,
                "EFF101",
                "`pkg.premises.with_lock` always runs the waiting call `time.sleep` on the event loop (1 call down, on every path)"
            ),
            (
                89,
                "EFF101",
                "`pkg.premises.with_file` always runs the waiting call `time.sleep` on the event loop (1 call down, on every path)"
            ),
            (
                90,
                "EFF101",
                "`pkg.premises.with_client` always runs the waiting call `requests.get` on the event loop (1 call down, on every path)"
            ),
            (
                91,
                "EFF111",
                "`tp_managers` reaches the blocking call `time.sleep` on the event loop"
            ),
            (
                96,
                "EFF103",
                "every iteration over a part of parameter `req` sends a database statement: `sqlite3.Connection.execute`"
            ),
            (
                102,
                "EFF113",
                "`sqlite3.Connection.execute` runs a database query on every loop iteration"
            ),
            (
                107,
                "EFF103",
                "every iteration over the entries of `os.listdir` sends a database statement: `sqlite3.Connection.execute`"
            ),
            (
                112,
                "EFF103",
                "every iteration over the lines of the text of `pathlib.Path.read_text` sends a database statement: `sqlite3.Connection.execute`"
            ),
            (
                117,
                "EFF103",
                "every iteration, once for each item of parameter `ids`, sends a database statement: `sqlite3.Connection.execute`"
            ),
            (
                137,
                "EFF103",
                "every iteration over parameter `xs` sends a database statement: `sqlite3.Connection.executemany` (in `OtherStore.save`) or `sqlite3.Connection.execute` (in `SqlStore.save`)"
            ),
            (
                141,
                "EFF101",
                "`get` always runs a waiting call on the event loop: `requests.post` (in `Local.get`) or `requests.get` (in `Remote.get`)"
            ),
            (
                154,
                "EFF111",
                "`tn_subclass_constructed` reaches the blocking call `time.sleep` on the event loop"
            ),
            (
                167,
                "EFF101",
                "`pkg.premises.PlainSub.run` always runs the waiting call `time.sleep` on the event loop (1 call down, on every path)"
            ),
            (
                177,
                "EFF101",
                "`pkg.premises.with_connection` always runs the waiting call `time.sleep` on the event loop (1 call down, on every path)"
            ),
            (
                177,
                "EFF111",
                "`tp_method_manager` reaches the blocking call `sqlalchemy.engine.Engine.connect` on the event loop"
            ),
            (
                183,
                "EFF113",
                "`sqlite3.Connection.execute` runs a database query on every loop iteration"
            ),
            (
                188,
                "EFF103",
                "every iteration over the rows of `sqlite3.Cursor.fetchall` sends a database statement: `sqlite3.Connection.execute`"
            ),
            (
                193,
                "EFF103",
                "every iteration over the lines of `builtins.open` sends a database statement: `sqlite3.Connection.execute`"
            ),
            (
                198,
                "EFF113",
                "`sqlite3.Connection.execute` runs a database query on every loop iteration"
            ),
            (
                215,
                "EFF101",
                "`pkg.premises.Mailer.send` always runs the waiting call `requests.post` on the event loop (1 call down, on every path)"
            ),
            (
                221,
                "EFF103",
                "every iteration over the rows of `sqlite3.Connection.execute` sends a database statement: `sqlite3.Connection.execute`"
            ),
        ]
    );
    // Each implementation's waiting call is named, and related, where it is: `Child.get` reaches
    // `requests.get` through `super().get`, in `Remote.get`.
    let witnesses = diags
        .iter()
        .find(|d| d["line"] == 72)
        .map(|d| {
            let related: Vec<(u64, &str)> = d["related"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| (r["line"].as_u64().unwrap(), r["message"].as_str().unwrap()))
                .collect();
            (d["target"].as_str().unwrap(), related)
        })
        .unwrap();
    assert_eq!(
        witnesses,
        (
            "requests.get, requests.post",
            vec![
                (72, "`tp_two_sinks` calls `pkg.premises.Local.get`"),
                (24, "`Local.get` calls `requests.post`"),
                (19, "`Remote.get` calls `requests.get`"),
            ]
        )
    );
}
