//! Property reads end to end through the real binary. Reading `obj.attr` on an instance of a
//! project class whose `attr` is a `@property` / `functools.cached_property` getter runs the
//! getter, so its effects and attribute reads reach the reader: `EFF101` (directly, one helper
//! deeper, `cached_property`, a base-class property read on a subclass instance, overriding
//! properties up to the CHA limit, `super()`), `EFF102` / `EFF103` in loops, `EFF106` under a
//! lock, `MUT101` / `MUT107`, and the `EFF201` / `MUT201` / `MUT203` contracts. No getter runs
//! for untyped receivers, methods and plain attributes sharing a property's name, un-awaited
//! `async` getters, classes outside the project, or properties with a setter (the setter shares
//! the getter's summary). A read runs every getter it can when every class the object may be
//! finds one first, which a union keeps and a class finding no getter, a base heki does not
//! know, `__getattribute__`, or a shadowing class attribute does not. The LSP server re-checks a
//! reader in another file when a getter starts blocking.

mod common;

use common::Project;
use common::lsp::{Lsp, codes, uri};
use serde_json::{Value, json};

fn diagnostics(p: &Project, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["--output-format", "json"];
    all.extend_from_slice(args);
    let (code, stdout, stderr) = p.check(&all);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

/// `(path, line, rule, qualname, target)` of every diagnostic.
fn found(diags: &[Value]) -> Vec<(String, u64, String, String, String)> {
    diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
                d["qualname"].as_str().unwrap().to_owned(),
                d["target"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn d(
    path: &str,
    line: u64,
    rule: &str,
    qualname: &str,
    target: &str,
) -> (String, u64, String, String, String) {
    (
        path.into(),
        line,
        rule.into(),
        qualname.into(),
        target.into(),
    )
}

/// `(path, line, message)` of every related location of the diagnostic of `qualname`.
fn related_of(diags: &[Value], qualname: &str) -> Vec<(String, u64, String)> {
    let diag = diags
        .iter()
        .find(|x| x["qualname"] == qualname)
        .unwrap_or_else(|| panic!("no diagnostic for `{qualname}`: {diags:#?}"));
    diag["related"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["path"].as_str().unwrap().to_owned(),
                r["line"].as_u64().unwrap(),
                r["message"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn hop(path: &str, line: u64, message: &str) -> (String, u64, String) {
    (path.into(), line, message.into())
}

const API: &str = r#"import functools
import sqlite3
import time

import requests


def _fetch(url):
    return requests.get(url)


class Client:
    def __init__(self, conn: sqlite3.Connection):
        self.conn = conn

    @property
    def text(self):
        return requests.get("https://example.com").text

    @property
    def nested(self):
        return _fetch("https://example.com")

    @functools.cached_property
    def settings(self):
        time.sleep(1)
        return {}

    @property
    def rows(self):
        return self.conn.execute("SELECT 1").fetchall()

    async def refresh(self):
        return self.text


class Base:
    @property
    def data(self):
        return 0

    @property
    def remote(self):
        return requests.get("https://example.com")


class Sub(Base):
    pass


class Slow(Base):
    @property
    def data(self):
        time.sleep(1)
        return 1


class Wrapped(Base):
    @property
    def remote(self):
        return super().remote


class Other:
    nested = None

    def __init__(self):
        self.settings = {}

    def text(self):
        return requests.get("https://example.com")


class Lazy:
    @property
    async def value(self):
        return requests.get("https://example.com")


class Page(requests.Response):
    async def body(self):
        return self.text


class Pool:
    @property
    def client(self):
        import httpx

        return httpx.Client()


class Shape:
    pass


class Square(Shape):
    @property
    def area(self):
        return requests.get("https://example.com")


class Remote:
    @property
    def body(self):
        return requests.get("https://example.com")


from vendor import TracingMixin


class Traced(TracingMixin, Remote):
    pass


class Audited:
    def __getattribute__(self, name):
        return object.__getattribute__(self, name)

    @property
    def remote(self):
        return requests.get("https://example.com")


class Feed:
    @property
    def entries(self):
        return requests.get("https://example.com")


class StaticFeed(Feed):
    entries = []


class Middle(Base):
    remote = "local"


class Child(Middle):
    async def via_super(self):
        return super().remote


class Rebound(Base):
    pass


Rebound.remote = "local"
"#;

const HANDLERS: &str = r#"import requests

from pkg.api import Audited, Base, Client, Feed, Lazy, Other, Pool, Rebound, Shape, Slow, Sub, Traced, Wrapped


async def direct(c: Client):
    return c.text


async def one_deeper(c: Client):
    return c.nested


async def cached(c: Client):
    return c.settings


async def inherited(s: Sub):
    return s.remote


async def overridden(b: Base):
    return b.data


async def through_super(w: Wrapped):
    return w.remote


async def untyped(x):
    return x.text


async def same_names(o: Other):
    return o.text, o.nested, o.settings


async def not_awaited(lazy: Lazy):
    return lazy.value


async def awaited(lazy: Lazy):
    return await lazy.value


async def external(r: requests.Response):
    return r.text


def sync_reader(c: Client):
    return c.text


def report(c: Client, ids):
    for i in ids:
        print(c.rows, i)


def clients(p: Pool, urls):
    for u in urls:
        p.client.get(u)


async def either(x: Sub | Wrapped):
    return x.remote


async def no_getter(s: Shape):
    return s.area


async def traced(t: Traced):
    return t.body


async def audited(a: Audited):
    return a.remote


async def feed_entries(f: Feed):
    return f.entries


async def rebound(r: Rebound):
    return r.remote
"#;

const LOCKS: &str = r#"import threading

from pkg.api import Client

_lock = threading.Lock()


def locked(c: Client):
    with _lock:
        return c.text
"#;

#[test]
fn execution_effects_run_property_getters() {
    let p = Project::new("property_reads_effects");
    p.write("pkg/__init__.py", "")
        .write("pkg/api.py", API)
        .write("pkg/handlers.py", HANDLERS)
        .write("pkg/locks.py", LOCKS);
    let diags = diagnostics(&p, &[]);
    let (a, h) = ("pkg/api.py", "pkg/handlers.py");
    assert_eq!(
        found(&diags),
        vec![
            // `self.text` in an `async def` method: every class the instance may be runs the
            // one getter, which waits on every path.
            d(a, 34, "EFF101", "Client.refresh", "requests.get"),
            // An `async` getter is an `async def`: it reports its own boundary, and reading it
            // (`not_awaited`, `awaited`) reports nothing.
            d(a, 77, "EFF101", "Lazy.value", "requests.get"),
            // `super().remote` in `Child` finds `Middle`'s class attribute after `Child`, not
            // `Base`'s getter.
            d(a, 141, "EFF111", "Child.via_super", "requests.get"),
            d(h, 7, "EFF101", "direct", "requests.get"),
            // One helper deeper.
            d(h, 11, "EFF101", "one_deeper", "requests.get"),
            // A `functools.cached_property` getter: a read may hand out the value an earlier
            // read computed.
            d(h, 15, "EFF111", "cached", "time.sleep"),
            // A base-class property read on a subclass instance.
            d(h, 19, "EFF101", "inherited", "requests.get"),
            // An overriding property of a subclass (CHA over `Base`): `Base.data` does not wait.
            d(h, 23, "EFF111", "overridden", "time.sleep"),
            // An overriding property reading `super().remote`.
            d(h, 27, "EFF101", "through_super", "requests.get"),
            // Property reads in unbounded loops: a query and a fresh client per iteration.
            d(h, 56, "EFF103", "report", "sqlite3.Connection.execute"),
            d(h, 61, "EFF102", "clients", "httpx.Client"),
            // Every member of a union finds a getter that waits.
            d(h, 65, "EFF101", "either", "requests.get"),
            // Not every class the object may be runs a getter: `Shape` finds no definition
            // (only its subclass `Square` defines the property), a base heki does not know
            // comes before the getter (`Traced`), `__getattribute__` runs on every read
            // (`Audited`), and a subclass's class attribute shadows the property (`StaticFeed`).
            d(h, 69, "EFF111", "no_getter", "requests.get"),
            d(h, 73, "EFF111", "traced", "requests.get"),
            d(h, 77, "EFF111", "audited", "requests.get"),
            d(h, 81, "EFF111", "feed_entries", "requests.get"),
            // A value stored on the class object outside its body (`Rebound.remote = ...`) is
            // what the read finds.
            d(h, 85, "EFF111", "rebound", "requests.get"),
        ],
        "{diags:#?}"
    );
    // Not reported: an untyped receiver (`untyped`), a method / class attribute / instance
    // attribute sharing a property's name (`same_names`), an un-awaited or awaited `async`
    // getter (`not_awaited`, `awaited`), a property of a class outside the project (`external`,
    // `Page.body`), and a synchronous reader (`sync_reader`).
    let direct = diags.iter().find(|x| x["qualname"] == "direct").unwrap();
    assert_eq!(direct["column"], 12, "at the read itself");
    assert_eq!(
        direct["message"],
        "`pkg.api.Client.text` always runs the waiting call `requests.get` on the event loop (1 call down, on every path)"
    );
    assert_eq!(
        related_of(&diags, "direct"),
        vec![
            hop(h, 7, "`direct` calls `pkg.api.Client.text`"),
            hop(a, 18, "`Client.text` calls `requests.get`"),
        ]
    );
    assert_eq!(
        related_of(&diags, "one_deeper"),
        vec![
            hop(h, 11, "`one_deeper` calls `pkg.api.Client.nested`"),
            hop(a, 22, "`Client.nested` calls `pkg.api._fetch`"),
            hop(a, 9, "`_fetch` calls `requests.get`"),
        ]
    );
    assert_eq!(
        related_of(&diags, "through_super"),
        vec![
            hop(h, 27, "`through_super` calls `pkg.api.Wrapped.remote`"),
            hop(a, 61, "`Wrapped.remote` calls `pkg.api.Base.remote`"),
            hop(a, 44, "`Base.remote` calls `requests.get`"),
        ]
    );
    assert_eq!(
        related_of(&diags, "report"),
        vec![
            hop(h, 56, "`report` calls `pkg.api.Client.rows`"),
            hop(a, 31, "`Client.rows` calls `sqlite3.Connection.execute`"),
        ]
    );
    // `EFF106` (opt-in): a getter doing network I/O read while a lock is held.
    let diags = diagnostics(&p, &["--select", "EFF106"]);
    assert_eq!(
        found(&diags),
        vec![d(
            "pkg/locks.py",
            10,
            "EFF106",
            "locked",
            "threading.Lock->requests.get"
        )],
        "{diags:#?}"
    );
    assert_eq!(
        related_of(&diags, "locked"),
        vec![
            hop("pkg/locks.py", 10, "`locked` calls `pkg.api.Client.text`"),
            hop(a, 18, "`Client.text` calls `requests.get`"),
        ]
    );
}

/// A base class whose property `kind` is overridden by `subclasses` properties (the first one
/// blocks), and an `async def` reading `kind` on the base type.
fn overriding_properties(subclasses: usize) -> String {
    let mut src = String::from(
        "import time\n\n\nclass Base:\n    @property\n    def kind(self):\n        return 0\n",
    );
    for i in 0..subclasses {
        let body = if i == 0 {
            "time.sleep(1)".to_owned()
        } else {
            format!("return {i}")
        };
        src.push_str(&format!(
            "\n\nclass Sub{i}(Base):\n    @property\n    def kind(self):\n        {body}\n"
        ));
    }
    src.push_str("\n\nasync def fan_out(b: Base):\n    return b.kind\n");
    src
}

#[test]
fn property_reads_follow_overrides_up_to_the_cha_limit() {
    // `Base.kind` and 8 overriding getters are more candidates than CHA follows
    // (`CHA_CANDIDATE_LIMIT` = 8), so `b.kind` stays unresolved. With 7 overrides it is not.
    let p = Project::new("property_reads_cha");
    p.write("pkg/__init__.py", "")
        .write("pkg/kinds.py", &overriding_properties(8));
    assert_eq!(found(&diagnostics(&p, &[])), vec![]);
    p.write("pkg/kinds.py", &overriding_properties(7));
    assert_eq!(
        found(&diagnostics(&p, &[])),
        vec![d("pkg/kinds.py", 53, "EFF111", "fan_out", "time.sleep")]
    );
}

const STATE: &str = r#"from functools import cached_property


class Cart:
    def __init__(self, items):
        self.items = list(items)
        self._owner = "a"

    @property
    def listing(self):
        return list(self.items)

    @cached_property
    def total(self):
        return len(self.listing)

    @property
    def key(self):
        return self._owner

    def __eq__(self, other):
        return self is other

    def __hash__(self):
        return hash(self.key)

    def add(self, item):
        self.items.append(item)

    def transfer(self, owner):
        self._owner = owner


class Tag:
    def __init__(self, name):
        self._name = name

    @property
    def name(self):
        return self._name

    @name.setter
    def name(self, value):
        self._name = value

    def __eq__(self, other):
        return self is other

    def __hash__(self):
        return hash(self.name)

    def rename(self, value):
        self._name = value
"#;

#[test]
fn cached_properties_and_hashes_read_through_properties() {
    let p = Project::new("property_reads_state");
    p.write("pkg/__init__.py", "").write("pkg/state.py", STATE);
    let diags = diagnostics(&p, &["--extend-select", "MUT101,MUT107"]);
    let s = "pkg/state.py";
    assert_eq!(
        found(&diags),
        vec![
            // `total` reads `self.items` through the property `listing`.
            d(s, 28, "MUT101", "Cart.add", "Cart.items->total"),
            // `__hash__` reads `self._owner` through the property `key`.
            d(s, 31, "MUT107", "Cart.transfer", "Cart._owner->__hash__"),
        ],
        "{diags:#?}"
    );
    // Known limitation: `Tag.name` has a setter, so reading it is not followed and
    // `Tag.rename` (writing `self._name`) is not reported.
    assert!(
        !diags.iter().any(|x| x["qualname"] == "Tag.rename"),
        "{diags:#?}"
    );
}

const CONTRACTS: &str = r#"[project]
name = "t"

[[tool.heki.write-owners]]
name = "store"
targets = ["Model.fields"]
allowed-writers = ["*Store*"]

[[tool.heki.read-owners]]
name = "kids"
targets = ["Model.children"]
allowed-readers = ["*Store*"]

[[tool.heki.effect-boundaries]]
name = "pure"
scopes = ["*.render", "*.render_*"]
forbid = ["blocking", "writes-targets"]
write-targets = ["Model.fields"]
"#;

const MODEL: &str = r#"import requests


class Model:
    def __init__(self):
        self.fields = []
        self.children = []
        self._fields = []

    @property
    def payload(self):
        return requests.get("https://example.com").json()

    @property
    def resolved(self):
        if not self.fields:
            self.fields = list(self._fields)
        return self.fields

    @property
    def _kids(self):
        return self.children

    @property
    def _view(self):
        return self._fields

    @_view.setter
    def _view(self, value):
        self.fields = value + self.children

    def render(self):
        return self.payload, self.resolved

    def render_view(self):
        return self._view


def summary(m: Model):
    return m._kids


def view_of(m: Model):
    return m._view
"#;

#[test]
fn contracts_follow_getters_but_not_setters() {
    let p = Project::new("property_reads_contracts");
    p.write("pyproject.toml", CONTRACTS)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", MODEL);
    let diags = diagnostics(&p, &[]);
    let m = "pkg/model.py";
    assert_eq!(
        found(&diags),
        vec![
            // A public getter's own write is reported where it is.
            d(m, 17, "MUT201", "Model.resolved", "Model.fields"),
            // The setter of `_view` writes and reads guarded fields; reading `_view` runs only
            // the getter, so the reports stay in the setter (no `MUT201` / `MUT203` / `EFF201`
            // at `render_view` or `view_of`).
            d(m, 30, "MUT201", "Model._view", "Model.fields"),
            d(m, 30, "MUT203", "Model._view", "Model.children"),
            // A scope function reading properties whose getters block and write.
            d(m, 33, "EFF201", "Model.render", "pure:blocking_net"),
            d(m, 33, "EFF201", "Model.render", "pure:Model.fields"),
            // A private getter with project readers hands its read to them.
            d(m, 40, "MUT203", "summary", "Model.children"),
        ],
        "{diags:#?}"
    );
    let eff201: Vec<&str> = diags
        .iter()
        .filter(|x| x["rule_code"] == "EFF201")
        .map(|x| x["message"].as_str().unwrap())
        .collect();
    assert_eq!(
        eff201,
        vec![
            "`Model.render` reaches blocking_net (`requests.get` via `pkg.model.Model.payload`), which effect boundary `pure` forbids",
            "`Model.render` writes `Model.fields` (by attribute via `pkg.model.Model.resolved`), which effect boundary `pure` forbids",
        ]
    );
    let summary = diags.iter().find(|x| x["qualname"] == "summary").unwrap();
    assert_eq!(
        summary["message"],
        "`summary` reads `Model.children` (by attribute) via `pkg.model.Model._kids` outside the read owners of `kids`"
    );
    assert_eq!(
        related_of(&diags, "summary"),
        vec![hop(m, 40, "`summary` calls `pkg.model.Model._kids`")]
    );
}

const LSP_API: &str = "class Client:\n    @property\n    def remote(self):\n        return 1\n";
const LSP_API_BLOCKING: &str = "import requests\n\n\nclass Client:\n    @property\n    def remote(self):\n        return requests.get(\"https://example.com\")\n";
const LSP_HANDLER: &str =
    "from pkg.api import Client\n\n\nasync def handler(c: Client):\n    return c.remote\n";

/// Edit, write, and save `rel` (the way an editor saves a changed document).
fn save(lsp: &mut Lsp, p: &Project, rel: &str, version: i32, text: &str) {
    let u = uri(&p.dir.join(rel));
    lsp.change(&u, version, text);
    p.write(rel, text);
    lsp.notify(
        "textDocument/didSave",
        json!({"textDocument": {"uri": u}, "text": text}),
    );
}

#[test]
fn lsp_reader_follows_a_getter_edit_in_another_file() {
    let p = Project::new("property_reads_lsp");
    p.write("pkg/__init__.py", "")
        .write("pkg/api.py", LSP_API)
        .write("pkg/handler.py", LSP_HANDLER);
    let (api, handler) = (
        uri(&p.dir.join("pkg/api.py")),
        uri(&p.dir.join("pkg/handler.py")),
    );
    let mut lsp = Lsp::start(&p.dir, &["server", "--stdio"]);
    lsp.request(
        "initialize",
        json!({"processId": null, "rootUri": uri(&p.dir), "capabilities": {}}),
    );
    lsp.notify("initialized", json!({}));
    lsp.ready();
    lsp.open(&handler, LSP_HANDLER);
    lsp.diagnostics(&handler, <[Value]>::is_empty);
    lsp.open(&api, LSP_API);
    lsp.diagnostics(&api, <[Value]>::is_empty);

    // The getter starts blocking: saving it re-checks the reader's file.
    save(&mut lsp, &p, "pkg/api.py", 2, LSP_API_BLOCKING);
    let diags = lsp.diagnostics(&handler, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["EFF101"]);
    assert_eq!(
        diags[0]["range"]["start"],
        json!({"line": 4, "character": 11})
    );
    let related = diags[0]["relatedInformation"].as_array().unwrap();
    assert_eq!(related[0]["location"]["uri"], handler);
    assert_eq!(related[1]["location"]["uri"], api);
    assert_eq!(
        related[1]["location"]["range"]["start"],
        json!({"line": 6, "character": 15})
    );

    // The getter stops blocking: the reader's diagnostic goes away.
    save(&mut lsp, &p, "pkg/api.py", 3, LSP_API);
    lsp.diagnostics(&handler, <[Value]>::is_empty);
    lsp.stop(true);
}
