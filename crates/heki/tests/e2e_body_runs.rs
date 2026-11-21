//! Where a project function's body runs, end to end through the real binary. Calling a
//! generator function only creates the generator: its body runs where the generator is
//! iterated (a `for` loop, a comprehension, unpacking, `yield from`, a consuming call, a lazy
//! wrapper that is iterated, a project function iterating its parameter), and a generator
//! returned by a plain function runs where the caller iterates it. A generator that escapes
//! (stored, returned out of an `async def`, handed to an external call such as
//! `StreamingResponse(...)`, or only stepped with `next()`) runs nothing there. A function an
//! offloading decorator wraps (`@sync_to_async`, `@database_sync_to_async`, `@asyncify`) runs on
//! another thread, and a function handed to a builtin that calls it once per item (`map`,
//! `filter`, `sorted(key=...)`, ...) runs there once per item. Every fact of a body moves with
//! it: execution effects (`EFF101`, `EFF102`, `EFF103`, `EFF201`), writes (`MUT201`), and reads
//! (`MUT203`).

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

/// `(path, line, column, rule, qualname)` of every diagnostic.
fn found(diags: &[Value]) -> Vec<(String, u64, u64, String, String)> {
    diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["column"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
                d["qualname"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn d(
    path: &str,
    line: u64,
    column: u64,
    rule: &str,
    qualname: &str,
) -> (String, u64, u64, String, String) {
    (path.into(), line, column, rule.into(), qualname.into())
}

fn message<'a>(diags: &'a [Value], path: &str, line: u64) -> &'a str {
    diags
        .iter()
        .find(|d| d["path"] == path && d["line"] == line)
        .unwrap_or_else(|| panic!("no diagnostic at {path}:{line}"))["message"]
        .as_str()
        .unwrap()
}

/// The message of the first hop of the call chain of the diagnostic at `path:line`.
fn first_hop<'a>(diags: &'a [Value], path: &str, line: u64) -> &'a str {
    diags
        .iter()
        .find(|d| d["path"] == path && d["line"] == line)
        .unwrap_or_else(|| panic!("no diagnostic at {path}:{line}"))["related"][0]["message"]
        .as_str()
        .unwrap()
}

/// `path:line` of each related location (the call chain) of the diagnostic at `path:line`.
fn chain(diags: &[Value], path: &str, line: u64) -> Vec<String> {
    diags
        .iter()
        .find(|d| d["path"] == path && d["line"] == line)
        .unwrap()["related"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| format!("{}:{}", r["path"].as_str().unwrap(), r["line"]))
        .collect()
}

// ==================================================================== the reported shapes

const STREAMING: &str = r#"import requests
from starlette.concurrency import iterate_in_threadpool
from starlette.responses import StreamingResponse

def rows():
    yield requests.get("https://api.example.com/rows", timeout=5).text

async def stream_response():
    return StreamingResponse(rows())  # Starlette iterates it in its threadpool

async def stream_offloaded():
    async for row in iterate_in_threadpool(rows()):  # iterated in a thread
        print(row)

async def iterate_on_loop():
    for row in rows():  # the body runs here, on the event loop
        print(row)

async def materialize_on_loop():
    return list(rows())
"#;

const DJANGO_VIEW: &str = r#"import requests
from asgiref.sync import sync_to_async

@sync_to_async
def fetch_profile() -> str:
    return requests.get("https://api.example.com/me", timeout=5).text

def fetch_plain() -> str:
    return requests.get("https://api.example.com/me", timeout=5).text

async def view():
    return await fetch_profile()  # runs in a worker thread

async def view_wrapped():
    return await sync_to_async(fetch_plain)()

async def view_blocking():
    return fetch_plain()
"#;

#[test]
fn generators_run_where_iterated_and_offloaded_functions_never_on_the_loop() {
    let p = Project::new("body_runs_reported");
    p.write("app/__init__.py", "")
        .write("app/streaming.py", STREAMING)
        .write("app/django_view.py", DJANGO_VIEW);
    let diags = diagnostics(&p, &[]);
    let (s, v) = ("app/streaming.py", "app/django_view.py");
    assert_eq!(
        found(&diags),
        vec![
            d(v, 18, 12, "EFF101", "view_blocking"),
            // The `for` header and the consuming call, where the body runs.
            d(s, 16, 16, "EFF101", "iterate_on_loop"),
            d(s, 20, 12, "EFF101", "materialize_on_loop"),
        ],
        "{diags:#?}"
    );
    assert_eq!(
        message(&diags, s, 16),
        "`iterate_on_loop` reaches the blocking call `requests.get` on the event loop by iterating `app.streaming.rows`"
    );
    assert_eq!(
        first_hop(&diags, v, 18),
        "`view_blocking` calls `app.django_view.fetch_plain`",
        "{diags:#?}"
    );
    assert_eq!(
        chain(&diags, s, 20),
        vec!["app/streaming.py:20", "app/streaming.py:6"]
    );
}

const GENS: &str = r#"import itertools

import requests


def rows():
    yield requests.get("https://api.example.com/rows", timeout=5).text


def consume(g):
    for row in g:
        print(row)


def first(g):
    for row in g:
        return row
    return None


def chunked(it, n):
    batch = []
    for x in it:
        batch.append(x)
        if len(batch) == n:
            yield batch
            batch = []


def relay():
    yield from rows()


def materialize():
    return sorted(rows())


async def helper_consumes():
    consume(rows())


async def helper_may_stop_early():
    return first(rows())


async def wrappers(names):
    for i, row in enumerate(rows()):
        print(i, row)
    for name, row in zip(names, rows()):
        print(name, row)
    for row in itertools.islice(rows(), 3):
        print(row)


async def project_generator_over_it():
    for batch in chunked(rows(), 10):
        print(batch)


async def delegated():
    for row in relay():
        print(row)


async def alias_and_unpacking():
    g = rows()
    h = g
    (only,) = h
    return only


async def sync_helper_materializes():
    return materialize()


async def stepped_only():
    return next(rows())


class Holder:
    def __init__(self):
        self.pending = None

    async def stored(self):
        self.pending = rows()


async def returned():
    return rows()


async def created_and_dropped():
    rows()


def load_names():
    return ["a", "b"]


async def iterate_plain_result():
    for name in load_names():
        print(name)
"#;

#[test]
fn consumers_wrappers_and_escapes() {
    let p = Project::new("body_runs_consumers");
    p.write("app/__init__.py", "").write("app/gens.py", GENS);
    let diags = diagnostics(&p, &[]);
    let g = "app/gens.py";
    assert_eq!(
        found(&diags),
        vec![
            // A sync helper iterating its parameter: at the call that hands it over.
            d(g, 39, 5, "EFF101", "helper_consumes"),
            // One that may stop at the first item still runs the body.
            d(g, 43, 12, "EFF101", "helper_may_stop_early"),
            // Lazy wrappers that are iterated: `enumerate`, `zip` with another iterable,
            // `islice`.
            d(g, 47, 19, "EFF101", "wrappers"),
            d(g, 49, 22, "EFF101", "wrappers"),
            d(g, 51, 16, "EFF101", "wrappers"),
            // A project generator iterating the generator it is handed, iterated here.
            d(g, 56, 18, "EFF101", "project_generator_over_it"),
            // `yield from` in a generator iterated here.
            d(g, 61, 16, "EFF101", "delegated"),
            // Through two aliases, by unpacking.
            d(g, 68, 6, "EFF101", "alias_and_unpacking"),
            // A sync helper materializing a generator runs its body.
            d(g, 73, 12, "EFF101", "sync_helper_materializes"),
        ],
        "{diags:#?}"
    );
    assert_eq!(
        first_hop(&diags, g, 39),
        "`helper_consumes` calls `app.gens.consume`"
    );
    // The helper is on the chain: the generator runs inside it.
    assert_eq!(
        chain(&diags, g, 39),
        vec!["app/gens.py:39", "app/gens.py:39", "app/gens.py:7"]
    );
    assert!(message(&diags, g, 56).ends_with("on the event loop by iterating `app.gens.chunked`"));
}

const COMPREHENSION: &str = r#"import requests


def rows():
    yield requests.get("https://api.example.com/rows", timeout=5).text


async def collect():
    return [row.upper() for row in rows()]
"#;

#[test]
fn comprehensions_follow_the_target_version() {
    let p = Project::new("body_runs_comprehension");
    p.write("app.py", COMPREHENSION);
    // Python 3.11 runs the comprehension in a frame of its own, which receives the generator:
    // the body runs at the call of that frame (the comprehension).
    let diags = diagnostics(&p, &["--target-version", "py311"]);
    assert_eq!(found(&diags), vec![d("app.py", 9, 12, "EFF101", "collect")]);
    // Python 3.12 inlines it (PEP 709): the body runs at its `for` clause.
    let diags = diagnostics(&p, &["--target-version", "py312"]);
    assert_eq!(found(&diags), vec![d("app.py", 9, 36, "EFF101", "collect")]);
}

const FACTORY: &str = r#"import requests


def rows():
    yield requests.get("https://api.example.com/rows", timeout=5).text


def make_rows():
    return rows()


def make_numbered():
    return enumerate(rows())


async def iterate_factory():
    for row in make_rows():
        print(row)


async def iterate_later():
    pending = make_rows()
    print("ready")
    for row in pending:
        print(row)


async def iterate_wrapped_factory():
    for i, row in make_numbered():
        print(i, row)


async def factory_escapes():
    return make_rows()
"#;

#[test]
fn a_returned_generator_runs_where_the_caller_iterates_it() {
    let p = Project::new("body_runs_factory");
    p.write("app/__init__.py", "")
        .write("app/factory.py", FACTORY);
    let diags = diagnostics(&p, &[]);
    let f = "app/factory.py";
    assert_eq!(
        found(&diags),
        vec![
            d(f, 17, 16, "EFF101", "iterate_factory"),
            d(f, 24, 16, "EFF101", "iterate_later"),
            d(f, 29, 19, "EFF101", "iterate_wrapped_factory"),
        ],
        "{diags:#?}"
    );
    // The generator is called from the iterating loop, not from the factory that returned it.
    assert!(message(&diags, f, 17).ends_with("on the event loop by iterating `app.factory.rows`"));
    assert_eq!(
        chain(&diags, f, 24),
        vec!["app/factory.py:24", "app/factory.py:5"]
    );
}

// ==================================================================== contracts and loops

const CONTRACT_CONFIG: &str = r#"[project]
name = "t"

[[tool.heki.write-owners]]
name = "model-store"
targets = ["app.models.Model.fields"]
allowed-writers = ["app.models.Store.*", "app.models.Factories.*"]

[[tool.heki.read-owners]]
name = "model-secret"
targets = ["app.models.Model.secret"]
allowed-readers = ["app.models.Store.*"]
"#;

const MODELS: &str = r#"import asyncio


class Model:
    def __init__(self) -> None:
        self.fields: list[str] = []
        self.secret: str = ""


def _fill(model: Model):
    model.fields.append("x")
    yield model


def _peek(model: Model):
    yield model.secret


class Store:
    def make(self, model: Model):
        return _fill(model)

    def drain(self, g):
        for _ in g:
            pass

    def run(self, model: Model):
        self.drain(_fill(model))

    def peek(self, model: Model):
        return _peek(model)


def consume_outside(store: Store, model: Model):
    for _ in store.make(model):
        pass


def read_outside(store: Store, model: Model):
    for _ in store.peek(model):
        pass


def created_only(model: Model):
    _fill(model)


async def _write_later(model: Model):
    model.fields.append("later")


def schedule(model: Model):
    return asyncio.ensure_future(_write_later(model))


async def _write_now(model: Model):
    model.fields.append("now")


async def apply_now(model: Model):
    await _write_now(model)


def show(g):
    for item in g:
        print(item)


def read_through(model: Model):
    show(_peek(model))


def fill_public(model: Model):
    model.fields.append("public")
    yield model


class Factories(Store):
    def make_public(self, model: Model):
        return fill_public(model)


def iterate_public(factories: Factories, model: Model):
    for _ in factories.make_public(model):
        pass


def fill_drained(model: Model):
    model.fields.append("drained")
    yield model


def drain_outside(store: Store, model: Model):
    store.drain(fill_drained(model))
"#;

#[test]
fn contract_reports_follow_where_the_body_runs() {
    let p = Project::new("body_runs_contracts");
    p.write("pyproject.toml", CONTRACT_CONFIG)
        .write("app/__init__.py", "")
        .write("app/models.py", MODELS);
    let diags = diagnostics(&p, &[]);
    let m = "app/models.py";
    assert_eq!(
        found(&diags),
        vec![
            // Created inside the store, iterated outside it: the write runs here. Iterated
            // inside the store (`Store.run` hands it to `Store.drain`): allowed. Created and
            // dropped: nothing runs.
            d(m, 35, 14, "MUT201", "consume_outside"),
            d(m, 40, 14, "MUT203", "read_outside"),
            // A coroutine handed to `asyncio` never runs at a project call: it reports its own
            // write. An awaited one hands its report to the caller.
            d(m, 49, 5, "MUT201", "_write_later"),
            d(m, 61, 11, "MUT201", "apply_now"),
            // Read inside the helper the generator is handed to.
            d(m, 70, 5, "MUT203", "read_through"),
            // A public generator reports its own write: its result is returned by an allowed
            // writer but iterated outside the store. One handed to the store, which iterates it
            // (`drain_outside`), writes inside the store.
            d(m, 74, 5, "MUT201", "fill_public"),
        ],
        "{diags:#?}"
    );
    assert_eq!(
        chain(&diags, m, 70),
        vec!["app/models.py:70", "app/models.py:70"]
    );
    assert_eq!(
        message(&diags, m, 35),
        "`consume_outside` writes `app.models.Model.fields` (container_mut) via `app.models._fill` outside the write owners of `model-store`"
    );
    // Called from the iterating loop: the store's `make` is not on the chain.
    assert_eq!(chain(&diags, m, 35), vec!["app/models.py:35"]);
}

const LOOPS: &str = r#"import sqlite3


def user_rows(conn: sqlite3.Connection, user_id):
    for row in conn.execute("SELECT * FROM t WHERE u = ?", (user_id,)):
        yield row


def report(conn: sqlite3.Connection, users):
    for u in users:
        pending = user_rows(conn, u)
        for row in pending:
            print(row)


def report_materialized(conn: sqlite3.Connection, users):
    for u in users:
        print(list(user_rows(conn, u)))


def report_once(conn: sqlite3.Connection, users):
    pending = user_rows(conn, 0)
    for u in users:
        print(u)
    return list(pending)
"#;

#[test]
fn a_generator_iterated_in_a_loop_queries_on_every_iteration() {
    let p = Project::new("body_runs_loops");
    p.write("app.py", LOOPS);
    let diags = diagnostics(&p, &["--select", "EFF103"]);
    assert_eq!(
        found(&diags),
        vec![
            // At the inner loop's iterable and at the consuming call, inside the outer loop.
            d("app.py", 12, 20, "EFF103", "report"),
            d("app.py", 18, 15, "EFF103", "report_materialized"),
        ],
        "{diags:#?}"
    );
}

const BOUNDARY_CONFIG: &str = r#"[project]
name = "t"

[[tool.heki.effect-boundaries]]
name = "pure-render"
scopes = ["app.render.render_*"]
forbid = ["blocking"]
"#;

const RENDER: &str = r#"import requests


def rows():
    yield requests.get("https://api.example.com/rows", timeout=5).text


def make_rows():
    return rows()


def render_joined():
    return "".join(rows())


def render_handed_out():
    return make_rows()


def render_factory():
    return list(make_rows())


def drain(g):
    for _ in g:
        pass


def render_drained():
    drain(rows())
"#;

#[test]
fn effect_boundaries_follow_iterations() {
    let p = Project::new("body_runs_boundaries");
    p.write("pyproject.toml", BOUNDARY_CONFIG)
        .write("app/__init__.py", "")
        .write("app/render.py", RENDER);
    let diags = diagnostics(&p, &["--select", "EFF201"]);
    let r = "app/render.py";
    assert_eq!(
        found(&diags),
        vec![
            d(r, 13, 12, "EFF201", "render_joined"),
            // Handing the generator out runs nothing; iterating what a factory returns runs
            // the generator, through the factory.
            d(r, 21, 12, "EFF201", "render_factory"),
            // Inside the helper that iterates it.
            d(r, 30, 5, "EFF201", "render_drained"),
        ],
        "{diags:#?}"
    );
    assert_eq!(
        chain(&diags, r, 30),
        vec!["app/render.py:30", "app/render.py:30", "app/render.py:5"]
    );
    assert_eq!(
        chain(&diags, r, 21),
        vec!["app/render.py:21", "app/render.py:9", "app/render.py:5"]
    );
}

const CALLS_CONFIG: &str = r#"[project]
name = "t"

[[tool.heki.call-owners]]
name = "ledger-posts"
targets = ["app.ledger.Ledger.post"]
allowed-callers = ["app.ledger.Store.*"]

[[tool.heki.effect-boundaries]]
name = "pure-render"
scopes = ["app.render.render_*"]
forbid = ["calls", "reads-targets"]
call-targets = ["app.ledger.Ledger.post"]
read-targets = ["app.ledger.Vault.secret"]
"#;

const LEDGER: &str = r#"class Ledger:
    def post(self, amount):
        return amount


class Vault:
    def __init__(self) -> None:
        self.secret: str = ""


def _posting(ledger: Ledger):
    ledger.post(1)
    yield ledger


def postings(ledger: Ledger):
    ledger.post(2)
    yield ledger


def peek(vault: Vault):
    yield vault.secret


class Store:
    def make_posts(self, ledger: Ledger):
        return _posting(ledger)

    def run_posts(self, ledger: Ledger):
        for _ in _posting(ledger):
            pass


def post_outside(store: Store, ledger: Ledger):
    for _ in store.make_posts(ledger):
        pass
"#;

const RENDER_CONTRACTS: &str = r#"from app.ledger import Ledger, Vault, peek, postings


def render_posts(ledger: Ledger):
    for _ in postings(ledger):
        pass


def render_hands_out(ledger: Ledger):
    return postings(ledger)


def render_reads(vault: Vault):
    return list(peek(vault))


def render_hands_out_reads(vault: Vault):
    return peek(vault)
"#;

#[test]
fn calls_and_reads_of_a_generator_count_where_it_is_iterated() {
    let p = Project::new("body_runs_calls");
    p.write("pyproject.toml", CALLS_CONFIG)
        .write("app/__init__.py", "")
        .write("app/ledger.py", LEDGER)
        .write("app/render.py", RENDER_CONTRACTS);
    let diags = diagnostics(&p, &["--select", "MUT204,EFF201"]);
    let (l, r) = ("app/ledger.py", "app/render.py");
    assert_eq!(
        found(&diags),
        vec![
            // A public generator reports its own call (no allowed caller runs it).
            d(l, 17, 5, "MUT204", "postings"),
            // The private generator's call runs where `post_outside` iterates what the store
            // returns: outside the store. Inside it (`Store.run_posts`), it is allowed.
            d(l, 35, 14, "MUT204", "post_outside"),
            // The calls and reads of a generator iterated in a scope function count there;
            // handing it out runs none of them.
            d(r, 5, 14, "EFF201", "render_posts"),
            d(r, 14, 12, "EFF201", "render_reads"),
        ],
        "{diags:#?}"
    );
}

const LAYERED_CONFIG: &str = r#"[project]
name = "t"

[[tool.heki.read-owners]]
name = "views-read-nothing"
targets = ["app.vault.Vault.secret"]
forbidden-readers = ["app.views.*"]
"#;

const VAULT: &str = r#"class Vault:
    def __init__(self) -> None:
        self.secret: str = ""


def _peek(vault: Vault):
    yield vault.secret
"#;

const VIEWS_LAYER: &str = r#"def show(g):
    for item in g:
        print(item)
"#;

const SERVICE: &str = r#"from app.vault import Vault, _peek
from app.views import show


def present(vault: Vault):
    show(_peek(vault))


def collect(vault: Vault):
    return list(_peek(vault))
"#;

#[test]
fn a_forbidden_layer_iterating_a_generator_reads_inside_it() {
    let p = Project::new("body_runs_forbidden");
    p.write("pyproject.toml", LAYERED_CONFIG)
        .write("app/__init__.py", "")
        .write("app/vault.py", VAULT)
        .write("app/views.py", VIEWS_LAYER)
        .write("app/service.py", SERVICE);
    let diags = diagnostics(&p, &["--select", "MUT203"]);
    // The read runs inside `app.views.show`, which iterates the generator `present` hands it;
    // `collect` reads outside the forbidden layer.
    assert_eq!(
        found(&diags),
        vec![d("app/service.py", 6, 5, "MUT203", "present")],
        "{diags:#?}"
    );
    assert_eq!(
        chain(&diags, "app/service.py", 6),
        vec!["app/service.py:6", "app/service.py:6"]
    );
}

// ==================================================================== offloading decorators

const VIEWS_CONFIG: &str = r#"[project]
name = "t"

[[tool.heki.ecosystem]]
symbol = "app.workers.in_thread"
offloads-blocking-arg = 0
"#;

const WORKERS: &str = r#"import asyncio


def in_thread(fn):
    async def run(*args):
        return await asyncio.to_thread(fn, *args)

    return run
"#;

const VIEWS: &str = r#"import functools

import requests
from asgiref.sync import sync_to_async
from asyncer import asyncify
from channels.db import database_sync_to_async

from app.workers import in_thread

FAST = True


@sync_to_async(thread_sensitive=False)
def fetch_settings() -> str:
    return requests.get("https://api.example.com/settings", timeout=5).text


@asyncify
def fetch_report() -> str:
    return requests.get("https://api.example.com/report", timeout=5).text


@in_thread
def fetch_job() -> str:
    return requests.get("https://api.example.com/job", timeout=5).text


def logged(fn):
    @functools.wraps(fn)
    def wrapper(*args, **kwargs):
        return fn(*args, **kwargs)

    return wrapper


@logged
def fetch_logged() -> str:
    return requests.get("https://api.example.com/logged", timeout=5).text


class ProfileConsumer:
    @database_sync_to_async
    def load_user(self) -> str:
        return requests.get("https://api.example.com/user", timeout=5).text

    async def connect(self) -> str:
        return await self.load_user()


async def view_factory():
    return await fetch_settings()


async def view_asyncified():
    return await fetch_report()


async def view_project_decorator():
    return await fetch_job()


async def view_logged():
    return fetch_logged()


if FAST:

    @sync_to_async
    def fetch_either() -> str:
        return requests.get("https://api.example.com/either", timeout=5).text

else:

    def fetch_either() -> str:
        return requests.get("https://api.example.com/either", timeout=5).text


async def view_either():
    return await fetch_either()
"#;

#[test]
fn offloading_decorators_and_plain_ones() {
    let p = Project::new("body_runs_decorators");
    p.write("pyproject.toml", VIEWS_CONFIG)
        .write("app/__init__.py", "")
        .write("app/workers.py", WORKERS)
        .write("app/views.py", VIEWS);
    let diags = diagnostics(&p, &[]);
    // The decorator factory form, `asyncify`, `database_sync_to_async` on a method, and a
    // project decorator declared in `[[tool.heki.ecosystem]]` offload; a plain
    // `functools.wraps` decorator is followed, and so is a function only one of whose
    // definitions is offloaded.
    assert_eq!(
        found(&diags),
        vec![
            d("app/views.py", 63, 12, "EFF101", "view_logged"),
            d("app/views.py", 79, 18, "EFF101", "view_either"),
        ],
        "{diags:#?}"
    );
}

// ==================================================================== incremental re-checks

fn wait_for(rx: &std::sync::mpsc::Receiver<String>, needle: &str) {
    let mut seen = Vec::new();
    loop {
        let line = rx
            .recv_timeout(std::time::Duration::from_secs(20))
            .unwrap_or_else(|_| panic!("no `{needle}` in {seen:#?}"));
        let hit = line.contains(needle);
        seen.push(line);
        if hit {
            return;
        }
    }
}

#[test]
fn watch_follows_a_generator_through_the_factory_returning_it() {
    use std::io::{BufRead, BufReader};
    use std::process::{Command, Stdio};

    let p = Project::new("body_runs_watch");
    p.write("pkg/__init__.py", "")
        .write(
            "pkg/rows.py",
            "def rows():\n    yield 1\n\n\ndef make_rows():\n    return rows()\n",
        )
        .write(
            "pkg/api.py",
            "from pkg.rows import make_rows\n\n\nasync def handler():\n    for row in make_rows():\n        print(row)\n",
        );
    let mut child = Command::new(env!("CARGO_BIN_EXE_heki"))
        .args(["check", "--watch", "--output-format", "concise"])
        .current_dir(&p.dir)
        // One re-check per edit below.
        .env("HEKI_WATCH_MAX_RUNS", "3")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    wait_for(&rx, "[heki watch] 0 diagnostic(s) in 3 file(s)");
    // The generator starts blocking: the caller iterating what the factory returns is
    // re-checked in its own file.
    p.write(
        "pkg/rows.py",
        "import time\n\n\ndef rows():\n    time.sleep(1)\n    yield 1\n\n\ndef make_rows():\n    return rows()\n",
    );
    wait_for(&rx, "[heki watch] 1 diagnostic(s) in 3 file(s)");
    // Moving code only changes no facts.
    p.write(
        "pkg/rows.py",
        "import time\n\n\n\ndef rows():\n    time.sleep(1)\n    yield 1\n\n\ndef make_rows():\n    return rows()\n",
    );
    wait_for(&rx, "[heki watch] 1 diagnostic(s) in 3 file(s)");
    // The factory hands out a list instead.
    p.write(
        "pkg/rows.py",
        "def rows():\n    yield 1\n\n\ndef make_rows():\n    return []\n",
    );
    wait_for(&rx, "[heki watch] 0 diagnostic(s) in 3 file(s)");
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains(
            "pkg/api.py:5:16: EFF101 `handler` reaches the blocking call `time.sleep` on the event loop by iterating `pkg.rows.rows`"
        ),
        "{stdout}"
    );
}

// ==================================================================== MUT106 shares the table

const SHARED_TABLE: &str = r#"def extend_then_list(lines):
    it = iter(lines)
    out = []
    out.extend(it)
    return out, list(it)


def unpack_then_loop(pairs):
    it = iter(pairs)
    first, *rest = it
    for pair in it:
        print(pair)
    return first, rest


def subscript_is_no_pass(order):
    import itertools

    levels = itertools.product(*order)
    count = len(list(levels))
    return count, levels["row"]
"#;

#[test]
fn iterator_double_consumption_shares_the_consuming_calls() {
    let p = Project::new("body_runs_mut106");
    p.write("app.py", SHARED_TABLE);
    let diags = diagnostics(&p, &["--select", "MUT106"]);
    // `out.extend(it)` exhausts `it`, and so does unpacking it; subscripting an iterator
    // (`levels["row"]`, a `TypeError`) is no pass over it.
    assert_eq!(
        found(&diags),
        vec![
            d("app.py", 5, 17, "MUT106", "extend_then_list"),
            d("app.py", 11, 17, "MUT106", "unpack_then_loop"),
        ],
        "{diags:#?}"
    );
    assert_eq!(
        message(&diags, "app.py", 5),
        "`it` is a single-pass iterator already consumed at line 4, so this pass sees no items"
    );
    assert_eq!(
        message(&diags, "app.py", 11),
        "`it` is a single-pass iterator already consumed at line 10, so this pass sees no items"
    );
}

/// `(path, line, rule)` of diagnostics or marks.
type Marks = Vec<(String, u64, String)>;

/// `(path, line, rule)` of the diagnostics of `rules` for `files`, and the marks of `files`: a
/// line ending in `# <RULE>`.
fn reported_and_marked(name: &str, files: &[(&str, &str)], rules: &[&str]) -> (Marks, Marks) {
    let p = Project::new(name);
    p.write("pyproject.toml", DEFERRED_CONFIG);
    p.write("pkg/__init__.py", "");
    for (path, text) in files {
        p.write(path, text);
    }
    let mut reported: Marks = found(&diagnostics(&p, &[]))
        .into_iter()
        .filter(|(_, _, _, r, _)| rules.contains(&r.as_str()))
        .map(|(path, line, _, rule, _)| (path, line, rule))
        .collect();
    reported.sort();
    let mut marked = Vec::new();
    for (path, text) in files {
        for (i, line) in text.lines().enumerate() {
            for rule in rules {
                if line.trim_end().ends_with(&format!("# {rule}")) {
                    marked.push(((*path).to_owned(), i as u64 + 1, (*rule).to_owned()));
                }
            }
        }
    }
    marked.sort();
    (reported, marked)
}

const DEFERRED_CONFIG: &str = r#"[project]
name = "t"

[[tool.heki.effect-boundaries]]
name = "render"
scopes = ["pkg.deferred.render_*"]
forbid = ["sleep"]
"#;

/// What runs where a function is called, a generator is made and iterated, and a class is
/// defined: the implementation behind `typing.overload` signatures, stepping by `any` / `all`,
/// a generator a property returns, one of several generators a branch chooses, the body of a
/// class a function defines (its methods run only when called), a coroutine awaited after it is
/// made, and a generator handed to an adapter a function returns.
const DEFERRED: &str = r#"import time
import typing


@typing.overload
def rows(x: int) -> typing.Iterator[int]: ...


@typing.overload
def rows(x: str) -> typing.Iterator[str]: ...


def rows(x):
    time.sleep(1)
    yield x


async def made():
    rows(1)


async def iterated():
    list(rows(1))  # EFF101


async def stepped():
    any(rows(1))  # EFF101


class Table:
    @property
    def rows(self):
        return rows(1)

    @property
    def size(self):
        return 1


async def through_property(t: Table):
    list(t.rows)  # EFF101
    t.rows
    t.size


async def chosen(flag):
    it = rows(1) if flag else rows(2)
    list(it)  # EFF101


async def chosen_or_empty(flag):
    it = rows(1) if flag else []
    list(it)  # EFF101


async def chosen_not_iterated(flag):
    it = rows(1) if flag else rows(2)
    return it


async def local_class():
    class Local:
        value = time.sleep(1)  # EFF101

        class Inner:
            other = time.sleep(2)  # EFF101

        def later(self):
            time.sleep(3)

    return Local


async def _work():
    time.sleep(4)  # EFF101


async def render_awaited_later():
    pending = _work()
    await pending  # EFF201


async def render_never_awaited():
    pending = _work()
    return pending


def relay(it):
    yield from it


def adapter():
    return relay(rows(1))


async def through_adapter():
    list(adapter())  # EFF101


async def adapter_not_iterated():
    adapter()
"#;

#[test]
fn bodies_run_where_python_runs_them() {
    let (reported, marked) = reported_and_marked(
        "deferred_bodies",
        &[("pkg/deferred.py", DEFERRED)],
        &["EFF101", "EFF201"],
    );
    assert_eq!(reported, marked);
}

/// The queries a project function makes, called once per item by builtins.
const PER_ITEM_QUERIES: &str = r#"import sqlite3

CONN = sqlite3.connect("app.db")


def name(uid):
    return CONN.execute("SELECT name FROM users WHERE id = ?", (uid,)).fetchone()


def name_of_pair(uid, other):
    return name(uid)


def add_name(total, uid):
    return total + name(uid)


def score(conn: sqlite3.Connection, uid):
    return conn.execute("SELECT score FROM users WHERE id = ?", (uid,)).fetchone()


class Users:
    def name(self, uid):
        return name(uid)

    def name_of_pair(self, uid, other):
        return name(uid)

    def add_name(self, total, uid):
        return total + name(uid)
"#;

/// Each builtin calling a callable once per item, with a lambda, a project function from
/// another module, a bound method, and a `functools.partial`: a loop as long as the items. A
/// lambda next to another one of its function is not followed: they share one summary.
const PER_ITEM: &str = r#"import functools
import itertools
import sqlite3
from functools import partial

from pkg.queries import Users, add_name, name, name_of_pair, score


def lambda_map(ids):
    return list(map(lambda u: name(u), ids))  # EFF103


def lambda_filter(ids):
    return list(filter(lambda u: name(u), ids))  # EFF103


def lambda_filterfalse(ids):
    return list(itertools.filterfalse(lambda u: name(u), ids))  # EFF103


def lambda_starmap(pairs):
    return list(itertools.starmap(lambda u, v: name(u), pairs))  # EFF103


def lambda_reduce(ids):
    return functools.reduce(lambda t, u: t + name(u), ids, 0)  # EFF103


def lambda_sorted(ids):
    return sorted(ids, key=lambda u: name(u))  # EFF103


def lambda_min(ids):
    return min(ids, key=lambda u: name(u))  # EFF103


def lambda_max(ids):
    return max(ids, key=lambda u: name(u))  # EFF103


def lambda_sort(rows: list[int]):
    rows.sort(key=lambda u: name(u))  # EFF103


def lambda_groupby(ids):
    key = lambda u: name(u)
    return itertools.groupby(ids, key)  # EFF103


def lambdas_of_one_name(ids, on_done):
    # Both lambdas are `lambdas_of_one_name.<locals>.<lambda>`: one summary holds the query of
    # the second.
    ordered = sorted(ids, key=lambda u: u)
    on_done(lambda: name(0))
    return ordered


def functions(ids, pairs, rows: list[int]):
    list(map(name, ids))  # EFF103
    list(filter(name, ids))  # EFF103
    list(itertools.filterfalse(name, ids))  # EFF103
    list(itertools.starmap(name_of_pair, pairs))  # EFF103
    functools.reduce(add_name, ids, 0)  # EFF103
    sorted(ids, key=name)  # EFF103
    min(ids, key=name)  # EFF103
    max(ids, key=name)  # EFF103
    rows.sort(key=name)  # EFF103
    itertools.groupby(ids, key=name)  # EFF103


def bound_methods(users: Users, ids, pairs, rows: list[int]):
    list(map(users.name, ids))  # EFF103
    list(filter(users.name, ids))  # EFF103
    list(itertools.filterfalse(users.name, ids))  # EFF103
    list(itertools.starmap(users.name_of_pair, pairs))  # EFF103
    functools.reduce(users.add_name, ids, 0)  # EFF103
    sorted(ids, key=users.name)  # EFF103
    min(ids, key=users.name)  # EFF103
    max(ids, key=users.name)  # EFF103
    rows.sort(key=users.name)  # EFF103
    itertools.groupby(ids, users.name)  # EFF103


def partials(conn: sqlite3.Connection, ids, extra):
    list(map(functools.partial(score, conn), ids))  # EFF103
    list(map(partial(score, conn), ids))  # EFF103
    sorted(ids, key=functools.partial(functools.partial(score), conn))  # EFF103
    sorted(ids, key=functools.partial(score, *extra))  # EFF103
    key = functools.partial(score, conn)
    sorted(ids, key=key)  # EFF103


def unpacked_iterables(columns):
    list(map(name_of_pair, *columns))  # EFF103


def fixed_items(ids):
    sorted((1, 2, 3), key=name)
    list(map(name, range(3)))
    max(1, 2, key=name)
    min("ab", key=name)
    list(map(name_of_pair, ids, [1, 2]))


def query_outside_the_callable(ids):
    first = name(0)
    return sorted(ids, key=lambda u: u == first)


def callables_that_run_nothing_here(ids, fn):
    sorted(ids, key=len)
    list(filter(None, ids))
    list(map(aname, ids))
    list(map(fn, ids))
    list(map(Users, ids))


async def aname(uid):
    return name(uid)  # EFF101


def every_outer_iteration(batches):
    for batch in batches:
        max(1, 2, key=lambda u: name(batch))  # EFF103


def items_of_the_first_batch(batches):
    # The loop runs the call once (every way on breaks out), the builtin once per item.
    for batch in batches:
        sorted(batch, key=lambda u: name(u))  # EFF103
        break


def fixed_items_of_the_first_batch(batches):
    for batch in batches:
        max(1, 2, key=lambda u: name(batch))
        break
"#;

/// Clients made once per item, and clients a pool or the receiver holds, or that the omitted
/// arguments' defaults switch off.
const PER_ITEM_CLIENTS: &str = r#"import functools
import itertools
import time

import requests


def fetch(url):
    return requests.get(url)


def pause(delay):
    time.sleep(delay)


class Api:
    def __init__(self):
        self.session = requests.Session()
        self._lazy = None

    def get(self, url):
        return self.session.get(url)

    def lazy(self):
        if self._lazy is None:
            self._lazy = requests.Session()
        return self._lazy

    def lazy_get(self, url):
        return self.lazy().get(url)

    def all_lazy(self, urls):
        return list(map(self.lazy_get, urls))

    def all_lazy_through_a_lambda(self, urls):
        return list(map(lambda u: self.lazy_get(u), urls))


def per_url(urls):
    return list(map(fetch, urls))  # EFF102


def per_url_lambda(urls):
    return list(map(lambda u: requests.get(u), urls))  # EFF102


def new_api_per_url(urls):
    return list(map(lambda u: Api().lazy_get(u), urls))  # EFF102


def lazy_get_one(api: Api, url):
    return api.lazy_get(url)


def each_api_once(apis: list[Api]):
    return list(map(lazy_get_one, apis, ["https://example.com"] * len(apis)))


def pooled(api: Api, urls):
    return list(map(api.get, urls))


def two_hosts():
    return list(map(fetch, ["https://a.example.com", "https://b.example.com"]))


async def on_the_event_loop(delays):
    list(map(pause, delays))  # EFF101


def fetch_if(url, enabled=False):
    if enabled:
        return requests.get(url)
    return None


def fetch_if_asked(url, *, enabled=False):
    if enabled:
        return requests.get(url)
    return None


def defaults_switch_it_off(urls):
    list(map(fetch_if, urls))
    list(map(fetch_if_asked, urls))
    sorted(urls, key=fetch_if)


def a_keyword_switches_it_on(urls):
    return list(map(functools.partial(fetch_if, enabled=True), urls))  # EFF102


def fetch_flagged(url, enabled=True, marker=None, item=None):
    if enabled:
        return requests.get(url)
    return None


def after_an_inner_unpacking(values, flags):
    # `fetch_flagged("u", *flags, False, item)`: `False` lands after what `flags` holds.
    key = functools.partial(functools.partial(fetch_flagged, "u", *flags), False)
    return sorted(values, key=key)  # EFF102


def keywords_an_outer_unpacking_may_override(urls, options):
    key = functools.partial(functools.partial(fetch_if_asked, enabled=False), **options)
    return list(map(key, urls))  # EFF102


def fetch_columns(url, enabled, *items):
    if enabled:
        return requests.get(url)
    return None


def held_arguments_next_to_unpacked_iterables(columns):
    return list(map(functools.partial(fetch_columns, "u", False), *columns))


def a_held_keyword_next_to_unpacked_items(rows):
    return list(itertools.starmap(functools.partial(fetch_if, enabled=False), rows))
"#;

#[test]
fn builtins_call_a_callable_once_per_item() {
    let files = [
        ("pkg/queries.py", PER_ITEM_QUERIES),
        ("pkg/per_item.py", PER_ITEM),
        ("pkg/clients.py", PER_ITEM_CLIENTS),
    ];
    let (reported, marked) =
        reported_and_marked("per_item", &files, &["EFF101", "EFF102", "EFF103"]);
    assert_eq!(reported, marked);

    let p = Project::new("per_item_messages");
    p.write("pkg/__init__.py", "");
    for (path, text) in files {
        p.write(path, text);
    }
    let diags = diagnostics(&p, &["--select", "EFF102,EFF103"]);
    // The line of `PER_ITEM` holding `code`.
    let at = |code: &str| {
        PER_ITEM
            .lines()
            .position(|l| l.contains(code))
            .map(|i| i as u64 + 1)
            .unwrap()
    };
    let help = |line: u64| {
        diags
            .iter()
            .find(|d| d["path"] == "pkg/per_item.py" && d["line"] == line)
            .unwrap()["help"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    let sorted = at("sorted(ids, key=lambda u: name(u))");
    assert_eq!(
        message(&diags, "pkg/per_item.py", sorted),
        "`pkg.per_item.lambda_sorted.<locals>.<lambda>` runs a database query \
         (`sqlite3.Connection.execute`) for every item `sorted` calls it with (N+1)"
    );
    assert_eq!(
        help(sorted),
        "batch the queries, or load the data once before the call"
    );
    assert_eq!(
        first_hop(&diags, "pkg/per_item.py", sorted),
        "`lambda_sorted` calls `pkg.per_item.lambda_sorted.<locals>.<lambda>`"
    );
    assert_eq!(
        message(&diags, "pkg/per_item.py", at("rows.sort(key=lambda")),
        "`pkg.per_item.lambda_sort.<locals>.<lambda>` runs a database query \
         (`sqlite3.Connection.execute`) for every item `list.sort` calls it with (N+1)"
    );
    // A fixed number of items inside a data-sized loop: the loop repeats the query.
    let outer = at("max(1, 2, key=lambda u: name(batch))");
    assert_eq!(
        message(&diags, "pkg/per_item.py", outer),
        "`pkg.per_item.every_outer_iteration.<locals>.<lambda>` runs a database query \
         (`sqlite3.Connection.execute`) on every loop iteration (N+1)"
    );
    assert_eq!(
        help(outer),
        "batch the queries, or load the data once before the loop"
    );
    assert_eq!(
        message(&diags, "pkg/per_item.py", at("sorted(batch, key=lambda")),
        "`pkg.per_item.items_of_the_first_batch.<locals>.<lambda>` runs a database query \
         (`sqlite3.Connection.execute`) for every item `sorted` calls it with (N+1)"
    );
    assert_eq!(
        message(
            &diags,
            "pkg/clients.py",
            PER_ITEM_CLIENTS
                .lines()
                .position(|l| l.contains("list(map(fetch, urls))"))
                .unwrap() as u64
                + 1
        ),
        "`pkg.clients.fetch` allocates an unpooled client (`requests.get`) for every item `map` \
         calls it with"
    );
}
