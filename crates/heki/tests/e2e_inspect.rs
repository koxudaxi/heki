//! `heki inspect` end to end: resolving a symbol, the facts of the report against what `heki
//! check` reports for the same project, purity, callers, contracts, errors, and the text form.

mod common;

use common::Project;
use serde_json::Value;

/// `heki inspect --heuristic <args> --output-format json`: exit code, the JSON object, stderr.
/// The heuristic rules run, as in `Project::check`, so the reports are the same.
fn inspect(p: &Project, args: &[&str]) -> (i32, Value, String) {
    let mut all = vec!["inspect", "--heuristic"];
    all.extend_from_slice(args);
    all.extend(["--output-format", "json"]);
    let (code, out, err) = p.heki(&all);
    let v = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}\n{err}"));
    (code, v, err)
}

/// The report on `symbol`, which must inspect with exit code 0.
fn report(p: &Project, symbol: &str) -> Value {
    report_with(p, &[symbol])
}

fn report_with(p: &Project, args: &[&str]) -> Value {
    let (code, v, err) = inspect(p, args);
    assert_eq!(code, 0, "{v:#}\n{err}");
    v
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap().to_owned())
        .collect()
}

/// `(line, calls)` of each hop of a chain.
fn hops(chain: &Value) -> Vec<(u64, String)> {
    chain
        .as_array()
        .unwrap()
        .iter()
        .map(|h| {
            (
                h["line"].as_u64().unwrap(),
                h["calls"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

const USERS: &str = r#"import requests
import time

CACHE = {}


class Loader:
    def __init__(self):
        self.items = []

    def load(self, url, cache=True):
        if cache:
            CACHE.clear()
        self._remember(url)
        return fetch(url)

    def _remember(self, url):
        self.items.append(url)

    @property
    def size(self):
        return len(self.items)

    @classmethod
    def make(cls):
        return cls()

    @staticmethod
    def parse(text):
        return text.split()


def fetch(url):
    time.sleep(1)
    return requests.get(url)


async def handler(url):
    return Loader().load(url)


def pure(xs):
    return [x * 2 for x in xs]


def outer():
    def inner():
        return 1

    return inner()
"#;

fn users_project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("pkg/__init__.py", "")
        .write("pkg/users.py", USERS)
        .write("pkg/other.py", "def load():\n    return None\n");
    p
}

#[test]
fn symbols_resolve_by_name_suffix_glob_line_and_file() {
    let p = users_project("inspect_resolve");
    let exact = report(&p, "pkg.users.fetch");
    assert_eq!(exact["resolved_from"], Value::Null);
    assert_eq!(exact["function"]["fqn"], "pkg.users.fetch");
    assert_eq!(exact["function"]["path"], "pkg/users.py");
    assert_eq!(exact["function"]["kind"], "function");
    assert_eq!(exact["function"]["params"], serde_json::json!(["url"]));
    for (symbol, from, fqn) in [
        ("users.fetch", "suffix", "pkg.users.fetch"),
        ("*.fet?h", "glob", "pkg.users.fetch"),
        ("pkg/users.py:35", "line", "pkg.users.fetch"),
        ("pkg/users.py:20", "line", "pkg.users.Loader.size"),
        ("pkg/users.py:3", "line", "pkg.users.<module>"),
        ("pkg/users.py:7", "line", "pkg.users.<module>"),
        ("pkg/users.py:9", "line", "pkg.users.Loader.__init__"),
        ("pkg/users.py:48", "line", "pkg.users.outer.<locals>.inner"),
        ("pkg/users.py:49", "line", "pkg.users.outer"),
        ("pkg/users.py::Loader.load", "name", "pkg.users.Loader.load"),
    ] {
        let r = report(&p, symbol);
        assert_eq!(r["resolved_from"], from, "{symbol}");
        assert_eq!(r["function"]["fqn"], fqn, "{symbol}");
    }
    for (symbol, kind) in [
        ("Loader.load", "method"),
        ("Loader.size", "property"),
        ("Loader.make", "classmethod"),
        ("Loader.parse", "staticmethod"),
        ("pkg.users.<module>", "module_body"),
        ("outer.<locals>.inner", "nested"),
    ] {
        assert_eq!(report(&p, symbol)["function"]["kind"], kind, "{symbol}");
    }
    let load = report(&p, "Loader.load");
    assert_eq!(load["function"]["class"], "pkg.users.Loader");
    assert_eq!(load["function"]["line"], 11);
    assert_eq!(load["function"]["end_line"], 15);
    assert_eq!(report(&p, "handler")["function"]["async"], true);
}

#[test]
fn a_stub_and_conditional_definitions() {
    let p = Project::new("inspect_definitions");
    p.write(
        "m.py",
        "import sys\n\nif sys.version_info >= (3, 12):\n    def f(x):\n        return x\nelse:\n    def f(x):\n        return [x]\n",
    )
    .write("m.pyi", "def f(x: int) -> int: ...\n")
    .write("only.pyi", "def g() -> None: ...\n")
    .write(
        "wrap.py",
        "from only import g\nfrom m import f\n\n\ndef stubbed():\n    g()\n\n\ndef known():\n    return f(1)\n",
    );
    let f = report(&p, "m.f");
    assert_eq!(f["function"]["path"], "m.py");
    assert_eq!(f["function"]["definitions"], serde_json::json!([4, 7]));
    assert_eq!(f["function"]["stub"], false);
    let g = report(&p, "only.g");
    assert_eq!(g["function"]["stub"], true);
    assert_eq!(g["purity"]["verdict"], "unknown");
    assert_eq!(strings(&g["purity"]["because"]), ["no_body"]);
    // A call into a body only a stub declares: unknown, not pure.
    let stubbed = report(&p, "wrap.stubbed");
    assert_eq!(stubbed["purity"]["verdict"], "unknown");
    assert_eq!(strings(&stubbed["purity"]["because"]), ["no_body"]);
    assert_eq!(stubbed["purity"]["unknown_at"]["fqn"], "only.g");
    // The `.py` implementation next to a stub is a known body.
    assert_eq!(report(&p, "wrap.known")["purity"]["verdict"], "pure");
}

#[test]
fn a_symbol_naming_no_single_function_exits_2_with_an_error_object() {
    let p = users_project("inspect_errors");
    let (code, v, err) = inspect(&p, &["load"]);
    assert_eq!(code, 2);
    assert_eq!(v["error"]["kind"], "ambiguous");
    let fqns: Vec<&str> = v["error"]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["fqn"].as_str().unwrap())
        .collect();
    assert_eq!(fqns, ["pkg.other.load", "pkg.users.Loader.load"]);
    assert_eq!(v["error"]["candidates"][1]["line"], 11);
    assert_eq!(v["error"]["candidates"][1]["path"], "pkg/users.py");
    assert!(err.contains("ambiguous symbol `load`"), "{err}");
    let (code, v, _) = inspect(&p, &["pkg.users.fech"]);
    assert_eq!((code, v["error"]["kind"].as_str()), (2, Some("unknown")));
    // No function has the name: the ones whose names look like it.
    let (_, v, err) = inspect(&p, &["mod.Loader._remembers"]);
    assert_eq!(
        v["error"]["suggestions"][0]["fqn"],
        "pkg.users.Loader._remember"
    );
    assert!(
        err.contains("= help: did you mean `pkg.users.Loader._remember`?"),
        "{err}"
    );
    assert_eq!(
        v["error"]["docs"],
        "https://heki.koxudaxi.dev/coding-agents/#what-a-function-does-heki-inspect-experimental"
    );
    let (_, v, _) = inspect(&p, &["nothing.load"]);
    assert_eq!(v["error"]["suggestions"].as_array().unwrap().len(), 2);
    let (code, v, _) = inspect(&p, &["pkg.users.Loader"]);
    assert_eq!((code, v["error"]["kind"].as_str()), (2, Some("class")));
    assert_eq!(v["error"]["methods"].as_array().unwrap().len(), 6);
    let (code, v, _) = inspect(&p, &["pkg.users"]);
    assert_eq!((code, v["error"]["kind"].as_str()), (2, Some("module")));
    // The top-level functions and the module body.
    assert_eq!(v["error"]["functions"].as_array().unwrap().len(), 5);
    let (code, v, _) = inspect(&p, &["elsewhere.py:3"]);
    assert_eq!(
        (code, v["error"]["kind"].as_str()),
        (2, Some("outside_project"))
    );
    let (code, v, _) = inspect(&p, &["elsewhere.py::f"]);
    assert_eq!(
        (code, v["error"]["kind"].as_str()),
        (2, Some("outside_project"))
    );
    let (code, v, err) = inspect(&p, &["pkg/users.py"]);
    assert_eq!((code, v["error"]["kind"].as_str()), (2, Some("invalid")));
    assert!(err.contains("give a line"), "{err}");
    let (code, v, _) = inspect(&p, &["  "]);
    assert_eq!((code, v["error"]["kind"].as_str()), (2, Some("invalid")));
    // `--depth` without `--callers`.
    let (code, _, err) = p.heki(&["inspect", "--heuristic", "fetch", "--depth", "2"]);
    assert_eq!(code, 2);
    assert!(err.contains("--callers"), "{err}");
    // In text form the error goes to stderr.
    let (code, out, err) = p.heki(&["inspect", "--heuristic", "load"]);
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(
        err.contains("pkg.users.Loader.load  pkg/users.py:11"),
        "{err}"
    );
}

#[test]
fn a_file_that_does_not_parse_leaves_the_report_incomplete() {
    let p = users_project("inspect_incomplete");
    p.write("pkg/broken.py", "def f(:\n");
    let (code, v, err) = inspect(&p, &["fetch"]);
    assert_eq!(code, 2);
    assert_eq!(v["complete"], false);
    assert_eq!(v["errors"].as_array().unwrap().len(), 1);
    assert!(err.contains("pkg/broken.py"), "{err}");
    let (code, v, _) = inspect(&p, &["nothing"]);
    assert_eq!(code, 2);
    assert_eq!(v["error"]["errors"].as_array().unwrap().len(), 1);
}

#[test]
fn a_configuration_error_is_an_error_of_the_command() {
    let p = users_project("inspect_configuration_error");
    let (code, out, err) = p.heki(&["inspect", "fetch", "--select", "NOPE1"]);
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.contains("unknown rule selector `NOPE1`"), "{err}");
    let (code, out, err) = p.heki(&["inspect", "--imports", "fetch", "--select", "NOPE1"]);
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.contains("unknown rule selector `NOPE1`"), "{err}");
}

/// `heki check --output-format json` of the project.
fn check_json(p: &Project, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["--output-format", "json"];
    all.extend_from_slice(args);
    let (_, out, err) = p.check(&all);
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}\n{err}"))
}

#[test]
fn the_effect_chain_is_the_one_an_effect_boundary_reports() {
    let p = users_project("inspect_effect_chain");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.effect-boundaries]]\nname = \"no-net\"\nscopes = [\"pkg.users.Loader.load\"]\nforbid = [\"blocking-net\"]\n",
    );
    let diags = check_json(&p, &[]);
    let eff201 = diags
        .iter()
        .find(|d| d["rule_code"] == "EFF201")
        .expect("EFF201");
    let related: Vec<u64> = eff201["related"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["line"].as_u64().unwrap())
        .collect();
    let r = report(&p, "Loader.load");
    let net = r["effects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["effect"] == "blocking_net")
        .unwrap();
    let lines: Vec<u64> = hops(&net["chain"]).into_iter().map(|(l, _)| l).collect();
    assert_eq!(lines, related);
    assert_eq!(
        hops(&net["chain"]).last().unwrap().1,
        "requests.get",
        "{net:#}"
    );
    assert_eq!(net["direct"], false);
    assert_eq!(net["blocking"], true);
    assert_eq!(net["runs"], "called");
    assert_eq!(
        r["contracts"],
        serde_json::json!([{"kind": "effect-boundaries", "name": "no-net", "roles": ["boundary-scope", "reported"]}])
    );
    assert_eq!(r["reports"].as_array().unwrap().len(), 1);
    assert_eq!(r["reports"][0]["where"], "in");
    assert_eq!(r["reports"][0]["rule_code"], "EFF201");
}

const READONLY: &str = "from collections.abc import Sequence\n\n\ndef entry(xs: Sequence[int]):\n    helper(xs)\n\n\ndef helper(x):\n    relay(x)\n\n\ndef relay(y):\n    y.append(1)\n";

#[test]
fn a_parameter_change_chain_is_the_one_mut110_reports() {
    let p = Project::new("inspect_param_chain");
    p.write("m.py", READONLY);
    let diags = check_json(&p, &[]);
    let mut110 = diags.iter().find(|d| d["rule_code"] == "MUT110").unwrap();
    let related: Vec<u64> = mut110["related"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["line"].as_u64().unwrap())
        .collect();
    let helper = report(&p, "helper");
    let change = &helper["changes"]["params"][0];
    assert_eq!(change["param"], "x");
    assert_eq!(change["what"], serde_json::json!([""]));
    assert_eq!(change["direct"], false);
    let lines: Vec<u64> = hops(&change["chain"]).into_iter().map(|(l, _)| l).collect();
    // MUT110's chain starts with the call that hands the read-only parameter over.
    assert_eq!(lines, related[1..]);
    assert_eq!(change["at"]["line"], 13);
    let relay = report(&p, "relay");
    assert_eq!(relay["changes"]["params"][0]["direct"], true);
    assert_eq!(relay["changes"]["params"][0]["at"]["line"], 13);
    assert_eq!(
        relay["purity"]["because"],
        serde_json::json!(["changes_params"])
    );
    // With `--callers`, the report of the caller whose chain runs through the function.
    let r = report_with(&p, &["helper", "--callers"]);
    assert_eq!(r["reports"].as_array().unwrap().len(), 1);
    assert_eq!(r["reports"][0]["where"], "through");
    assert_eq!(r["reports"][0]["rule_code"], "MUT110");
    assert!(
        report(&p, "helper")["reports"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn reports_are_heki_checks_with_their_baseline_status() {
    let p = users_project("inspect_reports");
    let diags = check_json(&p, &[]);
    let handler: Vec<&Value> = diags
        .iter()
        .filter(|d| d["qualname"] == "handler")
        .collect();
    assert!(!handler.is_empty());
    let r = report(&p, "handler");
    let reports = r["reports"].as_array().unwrap();
    assert_eq!(reports.len(), handler.len());
    for (a, b) in reports.iter().zip(&handler) {
        assert_eq!(
            (&a["rule_code"], &a["line"], &a["message"]),
            (&b["rule_code"], &b["line"], &b["message"])
        );
        assert_eq!(a["baseline"], Value::Null);
    }
    let (code, _, err) = p.check(&["--baseline", "b.toml", "--update-baseline"]);
    assert_eq!(code, 0, "{err}");
    p.write(
        "pkg/users.py",
        &USERS.replace(
            "async def handler(url):\n",
            "async def handler(url):\n    time.sleep(2)\n",
        ),
    );
    let r = report_with(&p, &["handler", "--baseline", "b.toml"]);
    let statuses: Vec<&str> = r["reports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["baseline"].as_str().unwrap())
        .collect();
    assert!(statuses.contains(&"allowed"), "{statuses:?}");
    assert!(statuses.contains(&"regression"), "{statuses:?}");
    // A suppressed report is not shown.
    p.write(
        "pkg/users.py",
        &USERS.replace(
            "    return Loader().load(url)\n",
            "    return Loader().load(url)  # heki: ignore\n",
        ),
    );
    assert!(
        report(&p, "handler")["reports"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}

const FACTS: &str = r#"import asyncio
import sqlite3
import time
from functools import cached_property

import requests
from asgiref.sync import sync_to_async


def maybe(url, live):
    if live:
        requests.get(url)


class Store:
    def __init__(self):
        self._conn = None
        self.items = []
        self.log = Log()

    def conn(self):
        if self._conn is None:
            self._conn = sqlite3.connect("db")
        return self._conn

    def note(self, x):
        self.log.add(x)

    @cached_property
    def view(self):
        return self.items

    def forget(self):
        del self.view


class Log:
    def __init__(self):
        self.lines = []

    def add(self, x):
        self.lines.append(x)


def rows(url):
    yield requests.get(url)


def lazy(url):
    return rows(url)


@sync_to_async
def work():
    time.sleep(1)


class Mixin:
    def run(self):
        return self.execute()

    def execute(self):
        return None


class Worker(Mixin):
    def execute(self):
        time.sleep(1)


def ident(x):
    return x


def first(xs):
    return xs[0]


def fresh():
    return []
"#;

fn facts_project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("m.py", FACTS);
    p
}

/// The effect `name` of a report.
fn effect<'v>(r: &'v Value, name: &str) -> &'v Value {
    r["effects"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["effect"] == name)
        .unwrap_or_else(|| panic!("no {name}: {r:#}"))
}

#[test]
fn effects_say_when_they_run_and_what_they_need() {
    let p = facts_project("inspect_effects");
    let maybe = report(&p, "maybe");
    let net = effect(&maybe, "blocking_net");
    assert_eq!(net["direct"], true);
    assert_eq!(
        net["guards"],
        serde_json::json!([{"param": "live", "attr": null, "test": "truthy", "classes": [], "has": null, "holds": true}])
    );
    let conn = report(&p, "Store.conn");
    assert_eq!(
        effect(&conn, "blocking_db")["once_per"],
        serde_json::json!(["self"])
    );
    let rows = report(&p, "rows");
    assert_eq!(effect(&rows, "blocking_net")["runs"], "iterated");
    assert_eq!(rows["returns"]["kind"], "generator");
    let lazy = report(&p, "lazy");
    assert_eq!(effect(&lazy, "blocking_net")["runs"], "result_iterated");
    assert_eq!(
        lazy["returns"]["iterated_effects"],
        serde_json::json!(["blocking_net", "unpooled_client_alloc"])
    );
    let work = report(&p, "work");
    assert_eq!(
        work["function"]["offloaded_by"],
        "asgiref.sync.sync_to_async"
    );
    let run = report(&p, "Mixin.run");
    let sleep = effect(&run, "sleep");
    assert_eq!(sleep["via_subclasses"][0]["class"], "m.Worker");
    assert_eq!(sleep["via_subclasses"][0]["param"], "self");
    assert_eq!(
        hops(&sleep["via_subclasses"][0]["chain"]).last().unwrap().1,
        "time.sleep"
    );
    let users = users_project("inspect_effects_async");
    let handler = report(&users, "handler");
    assert_eq!(effect(&handler, "sleep")["runs"], "awaited");
}

#[test]
fn changes_of_self_through_calls_and_handed_attributes() {
    let p = facts_project("inspect_changes");
    let note = report(&p, "Store.note");
    let log = &note["changes"]["self_attrs"][0];
    assert_eq!(log["attr"], "log");
    assert_eq!(log["direct"], false);
    assert_eq!(hops(&log["chain"]), [(27, "m.Log.add".to_owned())]);
    assert_eq!(log["at"]["line"], 42);
    let users = users_project("inspect_changes_self");
    let load = report(&users, "Loader.load");
    let items = &load["changes"]["self_attrs"][0];
    assert_eq!(items["attr"], "items");
    assert_eq!(
        hops(&items["chain"]),
        [(14, "pkg.users.Loader._remember".to_owned())]
    );
    assert_eq!(items["at"]["line"], 18);
    let remember = report(&users, "Loader._remember");
    assert_eq!(remember["changes"]["self_attrs"][0]["direct"], true);
    assert_eq!(
        remember["purity"]["because"],
        serde_json::json!(["changes_self"])
    );
    let forget = report(&p, "Store.forget");
    assert_eq!(
        forget["changes"]["invalidates"],
        serde_json::json!(["view"])
    );
    assert_eq!(
        forget["purity"]["because"],
        serde_json::json!(["changes_self"])
    );
    let view = report(&p, "Store.view");
    assert_eq!(view["function"]["kind"], "cached_property");
    assert_eq!(view["returns"]["self_attrs"], serde_json::json!(["items"]));
}

#[test]
fn shared_state_changed_directly_through_calls_and_in_closures() {
    let p = users_project("inspect_globals");
    let load = report(&p, "Loader.load");
    let cache = &load["changes"]["globals"][0];
    assert_eq!(
        (&cache["name"], &cache["cell_of"], &cache["direct"]),
        (&Value::from("CACHE"), &Value::Null, &Value::from(true))
    );
    assert_eq!(cache["at"]["line"], 13);
    let handler = report(&p, "handler");
    let cache = &handler["changes"]["globals"][0];
    assert_eq!(cache["direct"], false);
    assert_eq!(
        hops(&cache["chain"]),
        [(39, "pkg.users.Loader.load".to_owned())]
    );
    let c = Project::new("inspect_cells");
    c.write(
        "m.py",
        "def fill(target):\n    out = target\n\n    def add(x):\n        out.append(x)\n\n    add(1)\n\n\ndef accumulate():\n    out = []\n\n    def add(x):\n        out.append(x)\n\n    add(1)\n    return out\n",
    );
    let fill = report(&c, "fill");
    let out = &fill["changes"]["globals"][0];
    assert_eq!(out["name"], "out");
    assert_eq!(out["cell_of"], "m.fill");
    assert_eq!(hops(&out["chain"]), [(7, "m.fill.<locals>.add".to_owned())]);
    let accumulate = report(&c, "accumulate");
    assert!(
        accumulate["changes"]["globals"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // `out` in `add` is the list `accumulate` binds: `out.append` changes a new object.
    assert_eq!(accumulate["purity"]["verdict"], "pure");
    let add = report(&c, "fill.<locals>.add");
    assert_eq!(add["changes"]["globals"][0]["direct"], true);
}

const WORKER_STATE: &str = r#"from concurrent.futures import ProcessPoolExecutor, ThreadPoolExecutor

CACHE = []


def remember(x):
    CACHE.append(x)


def on_threads(ex: ThreadPoolExecutor):
    ex.submit(remember, 1)


def in_processes(ex: ProcessPoolExecutor):
    ex.submit(remember, 1)
    list(ex.map(remember, [1, 2]))
"#;

/// A worker thread changes the module state of the caller's process; a worker process changes
/// its own.
#[test]
fn shared_state_a_worker_process_changes_is_its_own() {
    let p = Project::new("inspect_worker_state");
    p.write("m.py", WORKER_STATE);
    let names = |r: &Value| -> Vec<String> {
        r["changes"]["globals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["name"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(names(&report(&p, "m.on_threads")), ["CACHE"]);
    assert!(names(&report(&p, "m.in_processes")).is_empty());
}

const HANDED: &str = r#"import os

CACHE = []
A = []
B = []


class Registry:
    def __init__(self):
        self.items = []

    def add(self, x):
        self.items.append(x)

    def get(self, i):
        return self.items[i]


REGISTRY = Registry()
os.environ["MODE"] = "fast"


def append(xs):
    xs.append(1)


def size(xs):
    return len(xs)


def update():
    append(CACHE)


def measure():
    return size(CACHE)


def register(x):
    REGISTRY.add(x)


def lookup(i):
    return REGISTRY.get(i)


def refresh():
    update()


def either(flag):
    target = A if flag else B
    target.clear()


def clear_a():
    A.clear()


def clear_b():
    B.clear()


def both():
    A.clear()
    clear_b()
    update()
    clear_a()


def two_helpers():
    clear_a()
    clear_b()


async def rows():
    yield 1
"#;

#[test]
fn shared_state_handed_to_callees_and_merged_and_at_import() {
    let p = Project::new("inspect_handed");
    p.write("m.py", HANDED);
    let names = |r: &Value| -> Vec<String> {
        r["changes"]["globals"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| g["name"].as_str().unwrap().to_owned())
            .collect()
    };
    // Handed to a callee changing its argument: one hop, at the callee's change.
    let update = report(&p, "m.update");
    let cache = &update["changes"]["globals"][0];
    assert_eq!(
        (&cache["name"], &cache["direct"]),
        (&Value::from("CACHE"), &Value::from(false))
    );
    assert_eq!(hops(&cache["chain"]), [(32, "m.append".to_owned())]);
    assert_eq!(cache["at"]["line"], 24);
    assert_eq!(
        update["purity"]["because"],
        serde_json::json!(["changes_globals"])
    );
    // A module-level object whose method changes `self`.
    let register = report(&p, "m.register");
    assert_eq!(names(&register), ["REGISTRY"]);
    assert_eq!(
        hops(&register["changes"]["globals"][0]["chain"]),
        [(40, "m.Registry.add".to_owned())]
    );
    // Two levels up: the chain to the function that hands it on.
    let refresh = report(&p, "m.refresh");
    assert_eq!(
        hops(&refresh["changes"]["globals"][0]["chain"]),
        [(48, "m.update".to_owned()), (32, "m.append".to_owned())]
    );
    // A callee that only reads it.
    for symbol in ["m.measure", "m.lookup"] {
        let r = report(&p, symbol);
        assert!(names(&r).is_empty(), "{symbol}");
        assert_eq!(r["purity"]["verdict"], "pure", "{symbol}");
    }
    // Every global a merged value may be.
    assert_eq!(names(&report(&p, "m.either")), ["A", "B"]);
    // Every global changed, here and through each callee, once.
    assert_eq!(names(&report(&p, "m.both")), ["A", "B", "CACHE"]);
    assert_eq!(names(&report(&p, "m.two_helpers")), ["A", "B"]);
    // An async generator is iterated, not awaited.
    let rows = report(&p, "m.rows");
    assert_eq!(
        (&rows["returns"]["kind"], &rows["function"]["async"]),
        (&Value::from("generator"), &Value::from(true))
    );
    // The module body defines its names, and changes what it changes in place at import.
    assert_eq!(names(&report(&p, "m.<module>")), ["os"]);
}

#[test]
fn facts_the_configuration_does_not_compute_are_null() {
    let p = Project::new("inspect_modes");
    p.write(
        "m.py",
        "class Model:\n    def __init__(self):\n        self.name = \"\"\n\n\ndef rename(m: Model):\n    set_name(m)\n\n\ndef set_name(m: Model):\n    m.name = \"x\"\n\n\ndef keep(x, sink: list):\n    sink.append(x)\n\n\ndef fresh():\n    return Model()\n",
    );
    let local = report(&p, "rename");
    assert_eq!(local["changes"]["class_attrs"], Value::Null);
    assert!(
        local["changes"]["class_attrs_needs"]
            .as_str()
            .unwrap()
            .contains("MUT112")
    );
    assert_eq!(local["retains"]["kept"], Value::Null);
    assert_eq!(local["returns"]["new_object"], Value::Null);
    let selected = report_with(&p, &["rename", "--extend-select", "MUT112"]);
    assert_eq!(
        selected["changes"]["class_attrs"],
        serde_json::json!([{"class": "m.Model", "attr": "name", "kind": "direct_attr", "fresh": false}])
    );
    assert!(strings(&selected["purity"]["because"]).contains(&"writes_objects".to_owned()));
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.write-owners]]\nname = \"names\"\ntargets = [\"Model.name\"]\nallowed-writers = [\"set_name\"]\n",
    );
    let owned = report(&p, "keep");
    // `sink` is handed to `list.append`, code heki does not follow.
    assert_eq!(owned["retains"]["kept"], serde_json::json!(["x", "sink"]));
    assert_eq!(owned["retains"]["stored"], serde_json::json!(["x"]));
    assert_eq!(report(&p, "fresh")["returns"]["new_object"], "own");
    assert_eq!(report(&p, "rename")["returns"]["new_object"], "no");
}

const INITIALIZATION: &str = r#"from __future__ import annotations


class Node:
    def __init__(self, members: list[Node]) -> None:
        self.members = members
        _link(self)

    def relink(self) -> None:
        _link(self)


def _link(node: Node) -> None:
    node.optional = len(node.members) > 1


def link_all(nodes: list[Node]) -> None:
    for n in nodes:
        _link(n)


def constructs(members: list[Node]) -> Node:
    return Node(members)


def relinks(node: Node) -> None:
    node.relink()
"#;

/// A class's own initialization writes the object it constructs, through the helpers it passes
/// `self` to too (as `EFF201` judges it): constructing a node changes no existing object, while
/// the same helper called by a method after `__init__` returned, or on other nodes, does.
#[test]
fn an_initializer_and_its_helpers_write_the_object_it_constructs() {
    let p = Project::new("inspect_initialization");
    p.write("m.py", INITIALIZATION);
    let r = |symbol: &str| report_with(&p, &[symbol, "--extend-select", "MUT112"]);
    let attrs = |v: &Value| -> Vec<(String, bool)> {
        (v["changes"]["class_attrs"].as_array().unwrap().iter())
            .map(|w| (w["attr"].as_str().unwrap().to_owned(), w["fresh"] == true))
            .collect()
    };
    let constructs = r("constructs");
    assert_eq!(
        attrs(&constructs),
        [("members".to_owned(), true), ("optional".to_owned(), true)]
    );
    assert_eq!(constructs["purity"]["verdict"], "pure");
    let init = r("Node.__init__");
    assert_eq!(strings(&init["purity"]["because"]), ["changes_self"]);
    for writer in ["relinks", "Node.relink", "link_all"] {
        let w = r(writer);
        assert_eq!(attrs(&w), [("optional".to_owned(), false)], "{writer}");
        assert!(strings(&w["purity"]["because"]).contains(&"writes_objects".to_owned()));
    }
    let (code, out, err) = p.heki(&["inspect", "constructs", "--extend-select", "MUT112"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("changes   none"), "{out}");
}

#[test]
fn return_origins() {
    let p = facts_project("inspect_returns");
    let origins = |f: &str| report(&p, f)["returns"]["origins"].clone();
    assert_eq!(origins("ident")["params"], serde_json::json!(["x"]));
    assert_eq!(origins("first")["elements_of"], serde_json::json!(["xs"]));
    assert_eq!(origins("fresh")["new"], true);
    assert_eq!(origins("Store.conn")["held"], true);
    let users = users_project("inspect_returns_none");
    assert_eq!(
        report(&users, "outer.<locals>.inner")["returns"]["origins"],
        Value::Null
    );
}

#[test]
fn purity_rests_on_evidence_and_says_what_is_unknown() {
    let p = Project::new("inspect_purity");
    p.write(
        "m.py",
        "class Model:\n    def __init__(self):\n        self.name = \"\"\n\n\ndef make() -> Model:\n    return Model()\n\n\ndef top(x):\n    return middle(x)\n\n\ndef middle(x):\n    return bottom(x)\n\n\ndef bottom(x):\n    return x.anything()\n\n\ndef run(code):\n    exec(code)\n\n\ndef gen(x):\n    yield x.anything()\n\n\ndef creates(x):\n    gen(x)\n    return 1\n\n\ndef touch():\n    make().name = \"x\"\n\n\nclass Base:\n    def step(self):\n        raise NotImplementedError\n\n\nimport abc\n\n\nclass Shape(abc.ABC):\n    @abc.abstractmethod\n    def area(self):\n        ...\n",
    );
    let top = report(&p, "top");
    assert_eq!(top["purity"]["verdict"], "unknown");
    assert_eq!(
        top["purity"]["because"],
        serde_json::json!(["unresolved_calls"])
    );
    assert_eq!(top["purity"]["unknown_at"]["fqn"], "m.bottom");
    assert_eq!(
        hops(&top["purity"]["unknown_at"]["chain"]),
        [(11, "m.middle".to_owned()), (15, "m.bottom".to_owned())]
    );
    assert_eq!(report(&p, "run")["purity"]["verdict"], "unknown");
    // The generator is created and dropped: its body never runs.
    assert_eq!(report(&p, "creates")["purity"]["verdict"], "pure");
    let touch = report(&p, "touch");
    assert_eq!(
        touch["purity"]["because"],
        serde_json::json!(["untracked_writes"])
    );
    assert_eq!(
        report(&p, "Shape.area")["purity"]["because"],
        serde_json::json!(["no_body"])
    );
    assert_eq!(report(&p, "Shape.area")["function"]["abstract"], true);
    let assumes = strings(&top["purity"]["assumes"]);
    assert_eq!(assumes.len(), 2);
    let users = users_project("inspect_purity_impure");
    let load = report(&users, "Loader.load");
    assert_eq!(load["purity"]["verdict"], "impure");
    assert_eq!(
        load["purity"]["because"],
        serde_json::json!(["effects", "changes_self", "changes_globals"])
    );
    assert_eq!(report(&users, "pure")["purity"]["verdict"], "pure");
}

/// Values whose type says nothing of the code a call on them runs.
const OPAQUE: &str = r#"import collections.abc
import typing
from typing import Any, Callable, Optional


def on_any(x: Any):
    x.save()


def on_object(x: object):
    x.save()


def on_attribute_of_any(x: typing.Any):
    x.conn.save()


def on_optional_any(x: Optional[Any]):
    if x is not None:
        x.save()


def builds_any_class(cls: type[Any]):
    cls()


def calls_callable(cb: Callable[[], None]):
    cb()


def calls_abc_callable(cb: collections.abc.Callable[[], None]):
    cb()


def calls_bare_callable(cb: Callable):
    cb()


def calls_untyped(cb):
    cb()


def calls_unbound():
    undefined_helper()


class Holder:
    def __init__(self, client: Any):
        self.client = client

    def fetch(self):
        return self.client.get()


def through_holder(h: Holder):
    return h.fetch()


def known(x: object):
    print(x)
    return len(str(x))
"#;

#[test]
fn calls_on_values_whose_type_says_nothing_are_unresolved() {
    let p = Project::new("inspect_opaque_calls");
    p.write("m.py", OPAQUE);
    for name in [
        "on_any",
        "on_object",
        "on_attribute_of_any",
        "on_optional_any",
        "builds_any_class",
        "calls_callable",
        "calls_abc_callable",
        "calls_bare_callable",
        "calls_untyped",
        "calls_unbound",
        "Holder.fetch",
    ] {
        let purity = &report(&p, name)["purity"];
        assert_eq!(purity["verdict"], "unknown", "{name}: {purity:#}");
        assert_eq!(
            purity["because"],
            serde_json::json!(["unresolved_calls"]),
            "{name}"
        );
        assert_eq!(purity["unknown_at"]["fqn"], format!("m.{name}"), "{name}");
    }
    let through = report(&p, "through_holder");
    assert_eq!(through["purity"]["verdict"], "unknown");
    assert_eq!(through["purity"]["unknown_at"]["fqn"], "m.Holder.fetch");
    // Builtins called with such a value are known.
    assert_eq!(report(&p, "known")["purity"]["verdict"], "pure");
}

/// Methods called on constants, on locals and module names bound only to constants, and the
/// shapes that typed their receivers already (an annotation, an f-string).
const CONSTANTS: &str = r#"SEP = ", "


def joined(xs: list[str]) -> str:
    return ", ".join(xs)


def stripped() -> str:
    return "  hi  ".strip()


def through_local(xs: list[str]) -> str:
    sep = ", "
    return sep.join(xs)


def either_constant(c: bool, xs: list[str]) -> str:
    sep = ", " if c else "; "
    return sep.join(xs)


def through_module_name(xs: list[str]) -> str:
    return SEP.join(xs)


def raw(xs: list[bytes]) -> bytes:
    return b",".join(xs)


def bits() -> int:
    return (42).bit_length()


def whole() -> bool:
    return (1.5).is_integer()


def flag() -> int:
    return True.bit_length()


def annotated(s: str) -> str:
    return s.strip()


def formatted(x: int) -> str:
    return f"{x}".lower()


def constant_or_unknown(c: bool, x):
    v = "a" if c else x
    return v.strip()
"#;

#[test]
fn methods_of_constants_are_their_builtin_class_methods() {
    let p = Project::new("inspect_constant_receivers");
    p.write("m.py", CONSTANTS);
    for name in [
        "joined",
        "stripped",
        "through_local",
        "either_constant",
        "through_module_name",
        "raw",
        "bits",
        "whole",
        "flag",
        "annotated",
        "formatted",
    ] {
        let purity = &report(&p, name)["purity"];
        assert_eq!(purity["verdict"], "pure", "{name}: {purity:#}");
    }
    // A local a constant binds on one path only has no class on the other.
    let purity = &report(&p, "constant_or_unknown")["purity"];
    assert_eq!(purity["verdict"], "unknown", "{purity:#}");
    assert_eq!(purity["because"], serde_json::json!(["unresolved_calls"]));
}

/// A dispatch through a table heki does not resolve (another function adds to it), in functions
/// that also write.
const DISPATCH: &str = r#"from typing import Callable


class Field:
    def __init__(self):
        self.required = False


def _flag(field: Field) -> None:
    field.required = True


TABLE: dict[str, Callable[[Field], None]] = {"flag": _flag}


def register(name: str, run: Callable[[Field], None]) -> None:
    TABLE[name] = run


def only_dispatches(field: Field) -> None:
    TABLE["flag"](field)


def writes_and_dispatches(field: Field, other: Field) -> None:
    other.required = False
    TABLE["flag"](field)


def calls_one_that_dispatches(field: Field, other: Field) -> None:
    other.required = False
    only_dispatches(field)


def vague(x):
    return x.thing()


def through_both(field: Field, other: Field, x):
    calls_one_that_dispatches(field, other)
    vague(x)
"#;

fn unknown_fqns(purity: &Value) -> Vec<String> {
    purity["unknown_reach"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| u["fqn"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn unresolved_calls_in_reach_are_named_whatever_the_verdict() {
    let p = Project::new("inspect_unknown_reach");
    p.write("m.py", DISPATCH);
    let only = &report(&p, "only_dispatches")["purity"];
    assert_eq!(only["verdict"], "unknown");
    assert_eq!(only["unknown_at"]["fqn"], "m.only_dispatches");
    // A function that writes is impure, and still names the call heki cannot follow.
    let writes = &report(&p, "writes_and_dispatches")["purity"];
    assert_eq!(writes["verdict"], "impure");
    assert_eq!(writes["because"], serde_json::json!(["changes_params"]));
    assert_eq!(writes["unknown_at"]["fqn"], "m.writes_and_dispatches");
    assert_eq!(writes["unknown_at"]["chain"], serde_json::json!([]));
    // So does every caller, with the chain down to it.
    let calls = &report(&p, "calls_one_that_dispatches")["purity"];
    assert_eq!(calls["verdict"], "impure");
    assert_eq!(calls["unknown_at"]["fqn"], "m.only_dispatches");
    assert_eq!(
        hops(&calls["unknown_at"]["chain"]),
        [(31, "m.only_dispatches".to_owned())]
    );
    // Every one reached, nearest first; `unknown_at` is the first.
    let both = &report(&p, "through_both")["purity"];
    assert_eq!(both["verdict"], "impure");
    assert_eq!(unknown_fqns(both), ["m.vague", "m.only_dispatches"]);
    assert_eq!(both["unknown_at"], both["unknown_reach"]["entries"][0]);
    assert_eq!(
        hops(&both["unknown_reach"]["entries"][1]["chain"]),
        [
            (39, "m.calls_one_that_dispatches".to_owned()),
            (31, "m.only_dispatches".to_owned())
        ]
    );
    assert_eq!(both["unknown_reach"]["truncated"], false);
    let known = &report(&p, "_flag")["purity"];
    assert_eq!(known["verdict"], "impure");
    assert_eq!(known["unknown_at"], Value::Null);
    assert_eq!(known["unknown_reach"]["entries"], serde_json::json!([]));
    let (code, out, err) = p.heki(&["inspect", "through_both"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains(
            "  impure: changes_params\n  cannot follow\n    vague            m.py:40 m.vague\n    only_dispatches  m.py:39 m.calls_one_that_dispatches -> m.py:31 m.only_dispatches\n"
        ),
        "{out}"
    );
}

#[test]
fn the_functions_heki_cannot_follow_stop_at_their_cap() {
    let p = Project::new("inspect_unknown_cap");
    let mut src = String::new();
    for i in 0..101 {
        src.push_str(&format!("def vague{i}(x):\n    return x.thing()\n\n\n"));
    }
    let calls = |n: usize| -> String { (0..n).map(|i| format!("    vague{i}(x)\n")).collect() };
    src.push_str(&format!("def hundred(x):\n{}\n\n", calls(100)));
    src.push_str(&format!("def more(x):\n{}", calls(101)));
    p.write("m.py", &src);
    let hundred = &report(&p, "hundred")["purity"];
    assert_eq!(unknown_fqns(hundred).len(), 100);
    assert_eq!(hundred["unknown_reach"]["truncated"], false);
    let more = &report(&p, "more")["purity"];
    assert_eq!(
        unknown_fqns(more),
        (0..100).map(|i| format!("m.vague{i}")).collect::<Vec<_>>()
    );
    assert_eq!(more["unknown_reach"]["truncated"], true);
    let (code, out, err) = p.heki(&["inspect", "more"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("  cannot follow (the first 100)\n"), "{out}");
}

const CALLERS: &str = r#"class Base:
    def step(self):
        return 1


class Sub(Base):
    def step(self):
        return 2


def run(b: Base):
    return b.step()


def leaf(x):
    return x


def mid(x):
    return leaf(x)


def upper(x):
    return mid(x)


def top(x):
    return upper(x)


def batch(xs):
    for x in xs:
        leaf(x)


def rec(n):
    if n:
        return rec(n - 1)
    return leaf(n)


async def job():
    return 1


async def waits():
    await job()


def rows():
    yield 1


def walks():
    for r in rows():
        print(r)


def relay(items):
    for i in items:
        print(i)


def hands():
    relay(rows())


class Box:
    @property
    def size(self):
        return 1


def show(b: Box):
    return b.size
"#;

/// The caller entries of a report, as `(fqn, depth, role)`.
fn callers_of(r: &Value) -> Vec<(String, u64, String)> {
    r["callers"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["fqn"].as_str().unwrap().to_owned(),
                c["depth"].as_u64().unwrap(),
                c["role"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn callers_say_how_and_where_they_call() {
    let p = Project::new("inspect_callers");
    p.write("m.py", CALLERS);
    let leaf = report(&p, "leaf");
    let entries = leaf["callers"]["entries"].as_array().unwrap();
    let batch = entries.iter().find(|c| c["fqn"] == "m.batch").unwrap();
    assert_eq!(batch["in_loop"], true);
    assert_eq!(batch["role"], "call");
    let mid = entries.iter().find(|c| c["fqn"] == "m.mid").unwrap();
    assert_eq!(
        (&mid["in_loop"], &mid["role"]),
        (&Value::from(false), &Value::from("return"))
    );
    assert_eq!(leaf["callers"]["depth"], 1);
    let deep = report_with(&p, &["leaf", "--callers", "--depth", "2"]);
    let found = callers_of(&deep);
    assert!(
        found.contains(&("m.upper".to_owned(), 2, "return".to_owned())),
        "{found:?}"
    );
    assert!(!found.iter().any(|(f, _, _)| f == "m.top"), "{found:?}");
    let upper = deep["callers"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["fqn"] == "m.upper")
        .unwrap();
    assert_eq!(upper["via"], "m.mid");
    // Recursion: the function is its own caller, listed once.
    let rec = report_with(&p, &["rec", "--callers", "--depth", "5"]);
    assert_eq!(
        callers_of(&rec),
        [("m.rec".to_owned(), 1, "return".to_owned())]
    );
    let step = report(&p, "Base.step");
    assert_eq!(step["callers"]["entries"][0]["candidates"], 2);
    assert_eq!(report(&p, "job")["callers"]["entries"][0]["awaited"], true);
    let rows = callers_of(&report(&p, "rows"));
    assert!(
        rows.contains(&("m.walks".to_owned(), 1, "iterate".to_owned())),
        "{rows:?}"
    );
    assert!(
        rows.contains(&("m.hands".to_owned(), 1, "through".to_owned())),
        "{rows:?}"
    );
    assert_eq!(
        callers_of(&report(&p, "Box.size")),
        [("m.show".to_owned(), 1, "property".to_owned())]
    );
}

#[test]
fn caller_lists_stop_at_their_caps() {
    let p = Project::new("inspect_caller_caps");
    let mut src = String::from("def target():\n    return 1\n");
    for i in 0..120 {
        src.push_str(&format!("\n\ndef caller{i}():\n    return target()\n"));
    }
    p.write("m.py", &src);
    let direct = report(&p, "target");
    assert_eq!(direct["callers"]["count"], 100);
    assert_eq!(direct["callers"]["truncated"], true);
    let all = report_with(&p, &["target", "--callers"]);
    assert_eq!(all["callers"]["count"], 120);
    assert_eq!(all["callers"]["truncated"], false);
}

const CONTRACTS: &str = r#"[project]
name = "t"

[[tool.heki.write-owners]]
name = "store"
targets = ["Model.name"]
allowed-writers = ["m.Store.*"]
forbidden-writers = ["m.render*"]
exempt-methods = ["copy_deep", "m.Model.reset"]

[[tool.heki.read-owners]]
name = "secrets"
targets = ["Model.secret"]
allowed-readers = ["m.Store.read"]
forbidden-readers = ["m.render"]

[[tool.heki.call-owners]]
name = "saves"
targets = ["Model.save"]
allowed-callers = ["m.Store.*"]
forbidden-callers = ["m.render"]

[[tool.heki.effect-boundaries]]
name = "pure-render"
scopes = ["m.render"]
forbid = ["blocking"]
allowed-callees = ["m.Store.read"]

[[tool.heki.readonly-returns]]
name = "shared"
producers = ["m.Store.read"]

[[tool.heki.invalidation]]
name = "names"
targets = ["Model.name"]
invalidators = ["m.Store.refresh"]
exempt-writers = ["m.Model.reset"]

[[tool.heki.ecosystem]]
symbol = "m.Store.refresh"
effects = []
"#;

const CONTRACTS_CODE: &str = r#"class Model:
    def __init__(self):
        self.name = ""
        self.secret = ""

    def save(self):
        return None

    def reset(self):
        self.name = ""

    def copy_deep(self):
        return Model()


class Store:
    def read(self, m: Model):
        return m.secret

    def refresh(self):
        return None


def render(m: Model):
    return m.name
"#;

/// The roles of a report, as `(kind, name, roles)`.
fn roles(r: &Value) -> Vec<(String, String, Vec<String>)> {
    r["contracts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["kind"].as_str().unwrap().to_owned(),
                c["name"].as_str().unwrap().to_owned(),
                strings(&c["roles"]),
            )
        })
        .collect()
}

#[test]
fn contracts_name_the_roles_a_function_has() {
    let p = Project::new("inspect_contracts");
    p.write("pyproject.toml", CONTRACTS)
        .write("m.py", CONTRACTS_CODE);
    let s = |v: &[&str]| v.iter().map(|x| (*x).to_owned()).collect::<Vec<_>>();
    let entry = |kind: &str, name: &str, r: &[&str]| (kind.to_owned(), name.to_owned(), s(r));
    assert_eq!(
        roles(&report(&p, "Store.read")),
        [
            entry("write-owners", "store", &["allowed-writer"]),
            entry("read-owners", "secrets", &["allowed-reader"]),
            entry("call-owners", "saves", &["allowed-caller"]),
            entry("effect-boundaries", "pure-render", &["allowed-callee"]),
            entry("readonly-returns", "shared", &["read-only-producer"]),
        ]
    );
    assert_eq!(
        roles(&report(&p, "render")),
        [
            entry("write-owners", "store", &["forbidden-writer"]),
            entry("read-owners", "secrets", &["forbidden-reader"]),
            entry("call-owners", "saves", &["forbidden-caller"]),
            entry("effect-boundaries", "pure-render", &["boundary-scope"]),
        ]
    );
    assert_eq!(
        roles(&report(&p, "Model.reset")),
        [
            entry("write-owners", "store", &["exempt-method"]),
            entry("invalidation", "names", &["exempt-writer"]),
        ]
    );
    assert_eq!(
        roles(&report(&p, "Model.copy_deep")),
        [entry("write-owners", "store", &["exempt-method"])]
    );
    assert_eq!(
        roles(&report(&p, "Model.save")),
        [entry("call-owners", "saves", &["call-target"])]
    );
    let refresh = roles(&report(&p, "Store.refresh"));
    assert!(
        refresh.contains(&entry("invalidation", "names", &["invalidator"])),
        "{refresh:?}"
    );
    assert!(
        refresh.contains(&entry("ecosystem", "m.Store.refresh", &["catalog-entry"])),
        "{refresh:?}"
    );
    assert!(
        report(&p, "Model.__init__")["contracts"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    // The contracts compiled once name the roles of each function reported on in one run.
    let symbols = [
        "Store.read",
        "render",
        "Model.reset",
        "Model.save",
        "Store.refresh",
    ];
    let (code, entries) = inspect_each_json(&p, &symbols.join("\n"), &[]);
    assert_eq!(code, 0);
    for (e, symbol) in entries.iter().zip(symbols) {
        assert_eq!(e["contracts"], report(&p, symbol)["contracts"], "{symbol}");
    }
}

/// A contract that names nothing of a function is listed when a report shown with it is of
/// that contract: a write outside the owners, and an import a boundary forbids, are reported
/// where they happen. A function with no such report lists none.
#[test]
fn contracts_name_the_contracts_of_the_reports_shown() {
    let p = Project::new("inspect_reported_contracts");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.write-owners]]\nname = \"through-ledger\"\ntargets = [\"pkg.model:Holder.items\"]\nallowed-writers = [\"pkg.model:Holder.__init__\"]\n\n[[tool.heki.import-boundaries]]\nname = \"no-json\"\nimporters = [\"pkg.model\"]\nforbidden-imports = [\"json\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write(
        "pkg/model.py",
        "class Holder:\n    def __init__(self):\n        self.items = []\n\n\ndef bad(h: Holder):\n    h.items.append(1)\n\n\ndef dump(x):\n    import json\n\n    return json.dumps(x)\n\n\ndef fine(x):\n    return x\n",
    );
    let r = report(&p, "pkg.model.bad");
    assert_eq!(r["reports"][0]["rule_code"], "MUT201", "{r:#}");
    assert_eq!(
        r["contracts"],
        serde_json::json!([{"kind": "write-owners", "name": "through-ledger", "roles": ["reported"]}])
    );
    let r = report(&p, "pkg.model.dump");
    assert_eq!(r["reports"][0]["rule_code"], "IMP201", "{r:#}");
    assert_eq!(
        r["contracts"],
        serde_json::json!([{"kind": "import-boundaries", "name": "no-json", "roles": ["reported"]}])
    );
    let r = report(&p, "pkg.model.fine");
    assert_eq!(r["contracts"], serde_json::json!([]), "{r:#}");
    let (_, out, _) = p.heki(&["inspect", "pkg.model.bad"]);
    assert!(
        out.contains("contracts that name it, or that a report above is of (`reported`)\n  write-owners through-ledger: reported\n"),
        "{out}"
    );
    let (_, out, _) = p.heki(&["inspect", "pkg.model.fine"]);
    assert!(
        out.contains("contracts none name it, and no report above is of one\n"),
        "{out}"
    );
}

#[test]
fn the_text_form_has_one_line_per_fact() {
    let p = users_project("inspect_text");
    let (code, out, err) = p.heki(&["inspect", "--heuristic", "Loader.load", "--callers"]);
    assert_eq!(code, 0, "{err}");
    let expected = "\
Loader.load  (pkg.users)
  pkg/users.py:11-15  method
  impure: effects, changes_self, changes_globals
  assumes: libraries outside the ecosystem catalog have no effects; objects of unknown origin changed in place are not tracked
effects
  blocking_net (called), 2 calls down, whenever it runs
    users.py:15         users.fetch
    users.py:35         requests.get
  sleep (called), 2 calls down, whenever it runs
    users.py:15         users.fetch
    users.py:34         time.sleep
  unpooled_client_alloc (called), 2 calls down, whenever it runs
    users.py:15         users.fetch
    users.py:35         requests.get
changes
  self.items     users.py:14 Loader._remember -> users.py:18
  global CACHE   directly: users.py:13
  other objects  not computed (an ownership contract, MUT108, MUT112, or MUT120 propagates them)
retains   stores url; what it keeps is not computed (needs an ownership contract)
returns   value: state that outlives the call
callers   1
  handler  users.py:39
reports
  pkg/users.py:39  EFF101 `pkg.users.Loader.load` always runs the waiting call `requests.get` on the event loop (2 calls down, on every path)
  pkg/users.py:39  EFF101 `pkg.users.Loader.load` always runs the waiting call `time.sleep` on the event loop (2 calls down, on every path)
contracts none name it, and no report above is of one
";
    assert_eq!(out, expected);
}

#[test]
fn names_within_a_file_and_names_that_are_no_pattern() {
    let p = users_project("inspect_names");
    let r = report(&p, "pkg/users.py::load");
    assert_eq!(
        (&r["resolved_from"], &r["function"]["fqn"]),
        (&Value::from("name"), &Value::from("pkg.users.Loader.load"))
    );
    let (code, v, _) = inspect(&p, &["pkg/users.py::Loader"]);
    assert_eq!((code, v["error"]["kind"].as_str()), (2, Some("class")));
    // An unclosed `[` is no glob: nothing matches it.
    let (code, v, _) = inspect(&p, &["pkg.[load"]);
    assert_eq!((code, v["error"]["kind"].as_str()), (2, Some("unknown")));
}

/// `heki inspect --heuristic - <args>` with `symbols` on standard input: exit code, stdout,
/// stderr.
fn inspect_each(p: &Project, symbols: &str, args: &[&str]) -> (i32, String, String) {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = common::heki_command()
        .args(["inspect", "--heuristic", "-"])
        .args(args)
        .current_dir(&p.dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(symbols.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

/// [`inspect_each`] with `--output-format json`: exit code and the array.
fn inspect_each_json(p: &Project, symbols: &str, args: &[&str]) -> (i32, Vec<Value>) {
    let mut all = args.to_vec();
    all.extend(["--output-format", "json"]);
    let (code, out, err) = inspect_each(p, symbols, &all);
    let v: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}\n{err}"));
    (code, v.as_array().unwrap().clone())
}

fn fqns(entries: &[Value]) -> Vec<&str> {
    entries
        .iter()
        .map(|e| e["function"]["fqn"].as_str().unwrap_or("-"))
        .collect()
}

#[test]
fn symbols_on_standard_input_are_reported_from_one_analysis_in_their_order() {
    let p = users_project("inspect_each");
    let symbols = "pkg.users.fetch\nLoader.load\n\n  pkg/users.py:35  \n*.fet?h\nhandler\n";
    let (code, entries) = inspect_each_json(&p, symbols, &[]);
    assert_eq!(code, 0);
    let queries: Vec<&str> = entries
        .iter()
        .map(|e| e["query"].as_str().unwrap())
        .collect();
    assert_eq!(
        queries,
        [
            "pkg.users.fetch",
            "Loader.load",
            "pkg/users.py:35",
            "*.fet?h",
            "handler"
        ]
    );
    assert_eq!(
        fqns(&entries),
        [
            "pkg.users.fetch",
            "pkg.users.Loader.load",
            "pkg.users.fetch",
            "pkg.users.fetch",
            "pkg.users.handler"
        ]
    );
    // Each is the object the symbol alone prints, its reports from `heki check` included.
    assert!(!entries[4]["reports"].as_array().unwrap().is_empty());
    for e in &entries {
        assert_eq!(
            *e,
            report(&p, e["query"].as_str().unwrap()),
            "{}",
            e["query"]
        );
    }
    // With the callers and their reports, and a baseline.
    let (code, _, err) = p.check(&["--baseline", "b.toml", "--update-baseline"]);
    assert_eq!(code, 0, "{err}");
    let args = ["--callers", "--depth", "2", "--baseline", "b.toml"];
    let (code, entries) = inspect_each_json(&p, "fetch\nLoader._remember\n", &args);
    assert_eq!(code, 0);
    assert_eq!(entries[0]["reports"][0]["where"], "through");
    assert_eq!(entries[0]["reports"][0]["baseline"], "allowed");
    for e in &entries {
        let query = e["query"].as_str().unwrap();
        let mut single = vec![query];
        single.extend(args);
        assert_eq!(*e, report_with(&p, &single), "{query}");
    }
    // Nothing on standard input: nothing to report.
    let (code, entries) = inspect_each_json(&p, "\n \n", &[]);
    assert_eq!((code, entries.len()), (0, 0));
}

#[test]
fn a_glob_on_standard_input_names_each_function_it_matches() {
    let p = users_project("inspect_each_glob");
    let (code, entries) = inspect_each_json(&p, "pkg.users.Loader.*\npkg/users.py::*.make\n", &[]);
    assert_eq!(code, 0);
    assert_eq!(
        fqns(&entries),
        [
            "pkg.users.Loader.__init__",
            "pkg.users.Loader._remember",
            "pkg.users.Loader.load",
            "pkg.users.Loader.make",
            "pkg.users.Loader.parse",
            "pkg.users.Loader.size",
            "pkg.users.Loader.make"
        ]
    );
    let from: Vec<&str> = entries
        .iter()
        .map(|e| e["resolved_from"].as_str().unwrap())
        .collect();
    assert_eq!(
        from,
        ["glob", "glob", "glob", "glob", "glob", "glob", "name"]
    );
    assert!(
        entries[..6]
            .iter()
            .all(|e| e["query"] == "pkg.users.Loader.*")
    );
    // As the symbol on the command line, a glob names one function: several are ambiguous.
    let (code, v, _) = inspect(&p, &["pkg.users.Loader.*"]);
    assert_eq!((code, v["error"]["kind"].as_str()), (2, Some("ambiguous")));
    // A name that matches several functions without a glob stays ambiguous.
    let (code, entries) = inspect_each_json(&p, "load\n", &[]);
    assert_eq!(
        (code, entries[0]["error"]["kind"].as_str()),
        (2, Some("ambiguous"))
    );
}

#[test]
fn symbols_on_standard_input_that_name_no_function_are_their_error_objects() {
    let p = users_project("inspect_each_errors");
    let symbols = [
        "pkg.users.fetch",
        "nothing.here",
        "pkg.nothing.*",
        "load",
        "pkg.users.Loader",
        "pkg.users",
        "pkg/users.py",
        "elsewhere.py:3",
        "handler",
    ];
    let (code, entries) = inspect_each_json(&p, &symbols.join("\n"), &[]);
    assert_eq!(code, 2);
    let kinds: Vec<&str> = entries
        .iter()
        .map(|e| e["error"]["kind"].as_str().unwrap_or("report"))
        .collect();
    assert_eq!(
        kinds,
        [
            "report",
            "unknown",
            "unknown",
            "ambiguous",
            "class",
            "module",
            "invalid",
            "outside_project",
            "report"
        ]
    );
    // Each is the object the symbol alone prints.
    for (e, symbol) in entries.iter().zip(symbols) {
        assert_eq!(*e, inspect(&p, &[symbol]).1, "{symbol}");
    }
    // In text form the reports go to stdout, a blank line between two, and the errors to stderr.
    let (code, out, err) = inspect_each(&p, &symbols.join("\n"), &[]);
    assert_eq!(code, 2);
    let text = |symbol| p.heki(&["inspect", "--heuristic", symbol]).1;
    assert_eq!(
        out,
        format!("{}\n{}", text("pkg.users.fetch"), text("handler"))
    );
    assert!(
        err.contains("no function of the project is named `nothing.here`"),
        "{err}"
    );
    assert!(err.contains("ambiguous symbol `load`"), "{err}");
    assert!(
        err.contains("pkg.users.Loader.load  pkg/users.py:11"),
        "{err}"
    );
    // Symbols that cannot name a function leave the project unanalyzed.
    p.write("pkg/broken.py", "def f(:\n");
    let (code, out, err) = inspect_each(&p, "pkg/users.py\n", &["--output-format", "json"]);
    assert_eq!(code, 2);
    assert!(out.contains("\"kind\": \"invalid\""), "{out}");
    assert!(!err.contains("pkg/broken.py"), "{err}");
    let (code, entries) = inspect_each_json(&p, "fetch\n", &[]);
    assert_eq!((code, entries[0]["complete"].as_bool()), (2, Some(false)));
}

const MORE: &str = r#"import copy
import requests


class A:
    pass


def nothing_unless(x):
    if x is None:
        requests.get("u")


def only_for(x):
    if isinstance(x, A):
        requests.get("u")


def entry(d: dict[str, list[int]]):
    d["k"].append(1)


class Bag:
    def __init__(self):
        self.items = []

    def note(self, x):
        append_to(self.items, x)

    def poke(self):
        touch(self)


def append_to(xs, x):
    xs.append(x)


def touch(o):
    o.items.append(1)


def dup(x):
    return copy.copy(x)


G = 0


def outer(target):
    out = target

    def inner():
        global G
        G = 1
        out.append(1)

    inner()


def chain1(x):
    return chain2(x)


def chain2(x):
    return chain3(x)


def chain3(x):
    return chain4(x)


def chain4(x):
    return chain5(x)


def chain5(x):
    return requests.get(x)


def deep1():
    deep2()


def deep2():
    deep3()


def deep3():
    deep4()


def deep4():
    deep5()


def deep5():
    global G
    G = 2


class Flags:
    def __init__(self):
        self.live = True
        self.data = {}

    def run(self, dry):
        if self.live and not dry:
            requests.get("u")

    def mark(self):
        self.data["k"] = 1


def rename(o):
    o.name = 1


def only_with(x):
    if hasattr(x, "url"):
        requests.get(x.url)


def nap(seconds):
    import time

    time.sleep(seconds)
"#;

#[test]
fn more_facts_guards_slots_hand_offs_and_copies() {
    let p = Project::new("inspect_more");
    p.write("m.py", MORE).write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.write-owners]]\nname = \"bags\"\ntargets = [\"Bag.items\"]\nallowed-writers = [\"m.Bag.*\"]\n",
    );
    let guard = |f: &str| effect(&report(&p, f), "blocking_net")["guards"][0].clone();
    assert_eq!(guard("nothing_unless")["test"], "is_none");
    let isinstance = guard("only_for");
    assert_eq!(isinstance["test"], "isinstance");
    assert_eq!(isinstance["classes"], serde_json::json!(["m.A"]));
    let has = guard("only_with");
    assert_eq!(
        (&has["test"], &has["has"]),
        (&Value::from("hasattr"), &Value::from("url"))
    );
    // `time.sleep(seconds)` waits unless the catalog's zero-sleep case holds.
    assert_eq!(
        effect(&report(&p, "nap"), "sleep")["guards"][0]["test"],
        "catalog"
    );
    let entry = report(&p, "entry");
    assert_eq!(
        entry["changes"]["params"][0]["slots"],
        serde_json::json!(["[\"k\"]"])
    );
    let note = report(&p, "Bag.note");
    let items = &note["changes"]["self_attrs"][0];
    assert_eq!(hops(&items["chain"]), [(28, "m.append_to".to_owned())]);
    assert_eq!(items["at"]["line"], 35);
    // `self` handed to a function: a change of the method's parameter `self`.
    let poke = report(&p, "Bag.poke");
    let own = &poke["changes"]["params"][0];
    assert_eq!(
        (&own["param"], &own["what"]),
        (&Value::from("self"), &serde_json::json!(["items"]))
    );
    assert_eq!(hops(&own["chain"]), [(31, "m.touch".to_owned())]);
    assert_eq!(own["at"]["line"], 39);
    assert_eq!(report(&p, "dup")["returns"]["new_object"], "copy");
    // The global the nested function changes reaches its owner; the cell it changes, bound to
    // the owner's parameter, too.
    let outer = report(&p, "outer");
    let globals: Vec<(String, Value)> = outer["changes"]["globals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| (g["name"].as_str().unwrap().to_owned(), g["cell_of"].clone()))
        .collect();
    assert_eq!(
        globals,
        [
            ("G".to_owned(), Value::Null),
            ("out".to_owned(), Value::from("m.outer"))
        ]
    );
    let long = report(&p, "chain1");
    assert_eq!(
        effect(&long, "blocking_net")["chain"]
            .as_array()
            .unwrap()
            .len(),
        5
    );
    // A related location in the function that is not a call of it: the chain of a report two
    // callers up passes through it.
    let ro = Project::new("inspect_through_depth");
    ro.write("m.py", READONLY);
    let relay = report_with(&ro, &["relay", "--callers"]);
    assert_eq!(relay["reports"].as_array().unwrap().len(), 1);
    assert_eq!(relay["reports"][0]["where"], "through");
}

#[test]
fn a_read_only_producer_wrapper_shares_its_result() {
    let p = Project::new("inspect_shared");
    p.write("pyproject.toml", CONTRACTS)
        .write("m.py", CONTRACTS_CODE)
        .write("wrap.py", "from m import Model, Store\n\n\ndef secret_of(m: Model):\n    return Store().read(m)\n");
    let r = report(&p, "wrap.secret_of");
    assert_eq!(r["returns"]["shared_with"]["producer"], "m.Store.read");
}

/// A subclass's method on one parameter (`Rack.put` on `tray`) reaching the method another
/// subclass gives a call on a second parameter (`Gate.preview` on `card`) runs only when both
/// arguments are instances of them: the way names both.
const NEEDS: &str = r#"import time


class Card:
    def preview(self):
        return None


class Gate(Card):
    def preview(self):
        time.sleep(1)


class Tray:
    def put(self, card: Card):
        return None


class Rack(Tray):
    def put(self, card: Card):
        return card.preview()


def forward(tray: Tray, card: Card):
    return tray.put(card)
"#;

#[test]
fn a_way_through_two_subclasses_names_both() {
    let p = Project::new("inspect_needs");
    p.write("m.py", NEEDS);
    let forward = report(&p, "forward");
    let way = &effect(&forward, "sleep")["via_subclasses"][0];
    assert_eq!(way["param"], "tray");
    assert_eq!(way["class"], "m.Rack");
    assert_eq!(
        way["also"],
        serde_json::json!([{"param": "card", "class": "m.Gate"}])
    );
    let (code, out, err) = p.heki(&["inspect", "--heuristic", "m.forward"]);
    assert_eq!(code, 0, "{err}");
    let line = "through subclass m.Rack of `tray` and subclass m.Gate of `card`:";
    assert!(out.contains(line), "`{line}` not in:\n{out}");
}

#[test]
fn the_text_form_shows_every_kind_of_fact() {
    let p = Project::new("inspect_text_kinds");
    p.write("m.py", MORE)
        .write("facts.py", FACTS)
        .write("callers.py", CALLERS);
    let text = |symbol: &str, extra: &[&str]| {
        let mut args = vec!["inspect", "--heuristic", symbol];
        args.extend_from_slice(extra);
        let (code, out, err) = p.heki(&args);
        assert_eq!(code, 0, "{err}");
        out
    };
    let has = |out: &str, line: &str| assert!(out.contains(line), "`{line}` not in:\n{out}");
    let maybe = text("facts.maybe", &[]);
    has(&maybe, "blocking_net (called), directly, only when live");
    has(&text("m.nothing_unless", &[]), "only when x is None");
    has(&text("m.only_for", &[]), "only when isinstance(x, m.A)");
    has(&text("m.only_with", &[]), "only when hasattr(x, \"url\")");
    has(&text("facts.Store.conn", &[]), "once per self");
    let run = text("facts.Mixin.run", &[]);
    has(&run, "through subclass facts.Worker of `self`:");
    has(
        &text("m.deep1", &[]),
        "global G       m.py:81 m.deep2 -> ... -> m.py:93 m.deep5 -> m.py:98",
    );
    let run = text("m.Flags.run", &[]);
    has(&run, "only when self.live");
    has(&run, "only unless dry");
    has(&text("m.Flags.mark", &[]), "self.data[\"k\"]");
    has(&text("m.rename", &[]), "o.name");
    let long = text("m.chain1", &[]);
    has(&long, "... 1 more (--output-format json lists every hop)");
    let entry = text("m.entry", &[]);
    has(&entry, "d[\"k\"]         in place, directly");
    let forget = text("facts.Store.forget", &[]);
    has(&forget, "deletes the cached value");
    let outer = text("m.outer", &[]);
    has(&outer, "out (closure of m.outer)");
    has(&outer, "global G");
    let lazy = text("facts.lazy", &[]);
    has(
        &lazy,
        "iterating it runs: blocking_net, unpooled_client_alloc",
    );
    has(&text("facts.ident", &[]), "returns   value: `x`");
    has(&text("facts.first", &[]), "an element of `xs`");
    has(&text("facts.fresh", &[]), "a new object");
    has(
        &text("callers.top", &[]),
        "callers   none in the checked files",
    );
    let leaf = text("callers.leaf", &["--callers"]);
    has(&leaf, "(in a loop)");
    has(&leaf, "    upper  callers.py:24  (return)");
    has(&text("callers.job", &[]), "(awaited)");
    has(
        &text("callers.Base.step", &[]),
        "(return, 1 of 2 candidates)",
    );
    let unknown = text("facts.Mixin.execute", &[]);
    has(&unknown, "  pure");
    let top = text("m.chain2", &[]);
    has(&top, "impure: effects");
    let owned = Project::new("inspect_text_owned");
    owned
        .write("pyproject.toml", CONTRACTS)
        .write("m.py", CONTRACTS_CODE)
        .write("other.py", "import m\n\n\ndef keeps(x, sink: list):\n    sink.append(x)\n\n\ndef nothing(x):\n    return 1\n")
        .write("wrap.py", "from m import Model, Store\n\n\ndef secret_of(m: Model):\n    return Store().read(m)\n");
    let (_, out, _) = owned.heki(&["inspect", "--heuristic", "m.Store.read"]);
    has(
        &out,
        "contracts that name it, or that a report above is of (`reported`)\n  write-owners store: allowed-writer",
    );
    let (_, out, _) = owned.heki(&["inspect", "--heuristic", "wrap.secret_of"]);
    has(&out, "  shared with read-only producer m.Store.read");
    let (_, out, _) = owned.heki(&["inspect", "--heuristic", "other.keeps"]);
    has(&out, "retains   stores x; keeps x, sink");
    let (_, out, _) = owned.heki(&["inspect", "--heuristic", "other.nothing"]);
    has(&out, "retains   stores no parameter; keeps none");
    let (_, out, _) = owned.heki(&[
        "inspect",
        "--heuristic",
        "m.Model.reset",
        "--extend-select",
        "MUT112",
    ]);
    has(&out, "m.Model.name");
    owned.write("broken.py", "def f(:\n");
    let (code, out, _) = owned.heki(&["inspect", "--heuristic", "other.nothing"]);
    assert_eq!(code, 2);
    has(&out, "error: ");
    let users = users_project("inspect_text_unknown");
    users.write("pkg/calls.py", "from pkg.users import fetch\n\n\ndef far(u):\n    return fetch(u)\n\n\ndef vague(x):\n    return x.thing()\n");
    let (_, out, _) = users.heki(&["inspect", "--heuristic", "pkg.calls.vague"]);
    has(
        &out,
        "  unknown: unresolved_calls\n  cannot follow\n    vague  itself\n",
    );
    let (_, out, _) = users.heki(&["inspect", "--heuristic", "fetch", "--callers"]);
    has(&out, "  calls.far  calls.py:5  (return)");
}

#[test]
fn a_nested_function_names_the_enclosing_parameters_it_keeps() {
    let p = Project::new("inspect_captured_params");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.write-owners]]\nname = \"owned\"\ntargets = [\"Model.name\"]\nallowed-writers = [\"nobody\"]\n",
    );
    p.write(
        "m.py",
        "REGISTRY = []\n\n\ndef outer(items, other):\n    def inner():\n        REGISTRY.append(other)\n\n    return inner\n\n\ndef outer_with_parameter(items, other):\n    def inner(extra):\n        REGISTRY.append(other)\n\n    inner(1)\n\n\ndef outer_with_varargs(items, other):\n    def inner(*args, **kwargs):\n        REGISTRY.append(other)\n\n    inner()\n\n\ndef outer_with_lambdas(xs, other):\n    wide = lambda a, b: a\n    narrow = lambda: REGISTRY.append(other)\n    return wide, narrow\n",
    );
    for symbol in [
        "outer.<locals>.inner",
        "outer_with_parameter.<locals>.inner",
        "outer_with_parameter",
        "outer_with_varargs.<locals>.inner",
        "outer_with_varargs",
        "outer_with_lambdas.<locals>.<lambda>",
    ] {
        assert_eq!(
            report(&p, symbol)["retains"]["kept"],
            serde_json::json!(["other"]),
            "{symbol}"
        );
    }
}

#[test]
fn class_attribute_writes_name_how_they_write() {
    let p = Project::new("inspect_write_kinds");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.write-owners]]\nname = \"owned\"\ntargets = [\"Model.name\", \"Model.items\"]\nallowed-writers = [\"nobody\"]\n",
    );
    p.write(
        "m.py",
        "class Model:\n    def __init__(self):\n        self.name = \"\"\n        self.items = [[]]\n\n\ndef delete(m: Model):\n    del m.name\n\n\ndef set_by_name(m: Model):\n    setattr(m, \"name\", \"x\")\n\n\ndef set_in_dict(m: Model):\n    m.__dict__[\"name\"] = \"x\"\n\n\ndef append(m: Model):\n    m.items.append(1)\n\n\ndef append_to_alias(m: Model):\n    xs = m.items\n    xs.append(1)\n\n\ndef append_to_item(m: Model):\n    m.items[0].append(1)\n\n\ndef helper(x):\n    x.items.append(1)\n\n\ndef through_helper(m: Model):\n    helper(m)\n\n\ndef entry_helper(xs):\n    xs[0].append(1)\n\n\ndef through_entry_helper(m: Model):\n    entry_helper(m.items)\n",
    );
    for (symbol, kind) in [
        ("delete", "attr_delete"),
        ("set_by_name", "set_attr"),
        ("set_in_dict", "dict_direct"),
        ("append", "container_mut"),
        ("append_to_alias", "alias_container_mut"),
        ("append_to_item", "item_mut"),
        ("through_helper", "indirect_helper"),
        ("through_entry_helper", "indirect_item_mut"),
    ] {
        let r = report_with(&p, &[symbol, "--extend-select", "MUT112"]);
        let kinds: Vec<&Value> = r["changes"]["class_attrs"]
            .as_array()
            .unwrap()
            .iter()
            .map(|w| &w["kind"])
            .collect();
        assert_eq!(kinds, [kind], "{symbol}");
    }
}

// =========================================================== parameters a function only reads

/// Premises 1 to 3 of a parameter annotated with a mutable container: the function, and every
/// function it hands the object to, neither changes it nor keeps it, and every call it is handed
/// to resolves to such a function or to a library call Layer 1 lists as reading it.
const ONLY_READ: &str = r#"from typing import Protocol

STORE = []


class Box:
    def __init__(self, items: list[int]) -> None:
        self.items = items

    def total(self, xs: list[int]) -> int:
        return self.size(xs)

    def size(self, xs):
        return len(xs)


class Reader(Protocol):
    def read(self, xs: list[int]) -> int: ...


def appends(xs: list[int]) -> None:
    xs.append(1)


def appends_to_alias(xs: list[int]) -> None:
    ys = xs
    ys.append(1)


def appends_before_aliasing(xs: list[int]) -> int:
    xs.append(1)
    ys = xs
    return len(ys)


def appends_to_join(xs: list[int], flag: bool) -> None:
    ys = xs if flag else []
    ys.append(1)


def stores_item(xs: list[int]) -> None:
    xs[0] = 1


def deletes_item(xs: list[int]) -> None:
    del xs[0]


def deletes_key(xs: list[int], table: dict) -> None:
    del table[xs]


def extends_in_place(xs: list[int]) -> None:
    xs += [1]


def unknown_method(xs: list[int]) -> list[int]:
    return xs.__mul__(2)


def added_to_object(xs: list[int], box: Box) -> object:
    return box + xs


def compared_with_object(xs: list[int], box: Box) -> bool:
    return box == xs


def key_of_object(xs: list[int], box: Box) -> object:
    return box[xs]


def in_object(xs: list[int], box: Box) -> bool:
    return xs in box


def added_to_either(xs: list[int], box: Box | Reader) -> object:
    return box + xs


def compared_with_unknown(xs: list[int], other) -> bool:
    return other == xs


def compared_with_object_typed(xs: list[int], other: object) -> bool:
    return other == xs


def compared_with_list(xs: list[int], other: list[int]) -> bool:
    return other == xs


def compared_with_class(xs: list[int]) -> bool:
    return Box == xs


def keyed_by_object(xs: list[int], box: Box) -> object:
    return xs[box]


def stores_attribute(xs: list[int], box: Box) -> None:
    box.items = xs


def stores_global(xs: list[int]) -> None:
    global STORE
    STORE = xs


def stores_value(xs: list[int], table: dict) -> None:
    table["k"] = xs


def in_display(xs: list[int]) -> int:
    return len([xs])


def in_dict(xs: list[int]) -> int:
    return len({"k": xs})


def unpacked(xs: list[int], d: dict[str, int]) -> int:
    return len([*xs]) + len({**d}) + len(dict(**d))


def captures(xs: list[int]):
    def inner():
        return len(xs)

    return inner


def captures_in_lambda(xs: list[int], keys: list[int]) -> list[int]:
    return sorted(keys, key=lambda k: xs.index(k))


def captures_in_class(xs: list[int]):
    class Holder:
        items = xs

    return Holder


def tests_in_genexp(xs: list[int], keys: list[int]) -> bool:
    return any(k in xs for k in keys)


def genexp_of_alias(xs: list[int], keys: list[int]) -> bool:
    ys = xs
    return any(k in ys for k in keys)


def yields(xs: list[int]):
    yield xs


def yields_from(xs: list[int]):
    yield from xs


def returns(xs: list[int]) -> list[int]:
    return xs


def returns_view(d: dict[str, int]):
    return d.keys()


def returns_max(xs: list[int]) -> int:
    return max(xs)


def reads(xs: list[int], d: dict[str, int], s: set[int]) -> int:
    n = len(xs) + sum(xs) + sorted(d.values())[0] + max(*xs)
    if xs and 1 in s and "k" in d:
        n += xs[0] + d["k"] + xs.count(1) + d.get("k", 0)
    for i, x in enumerate(xs):
        n += i + x
    acc = []
    acc.extend(xs)
    return n + sorted(s)[0] + len(frozenset(s)) + any(xs) + all(d) + len(acc)


def calls_unresolved(xs: list[int], g) -> None:
    g(xs)


def prints(xs: list[int]) -> None:
    print(xs)


def calls_itself(xs: list[int]) -> None:
    xs()


def keyword_to_library(xs: list[int]) -> int:
    return len(dict(items=xs))


def after_unpacking(xs: list[int], ys: list[int]) -> int:
    return max(*ys, xs)


def start_of_sum(xs: list[int], items: list[list[int]]) -> int:
    return len(sum(items, xs))


def compared_by_min(xs: list[int], ys: list[int]) -> int:
    return len(min(xs, ys))


def removed_from(xs: list[int], other: list[list[int]]) -> None:
    other.remove(xs)


def constructs(xs: list[int]) -> Box:
    return Box(xs)


def variadic(*args):
    return len(args)


def to_variadic(xs: list[int]) -> int:
    return variadic(xs)


def unpacked_to_variadic(xs: list[int]) -> int:
    return variadic(*xs)


def after_unpacking_to_variadic(xs: list[int], ys: list[int]) -> int:
    return variadic(*ys, xs)


def reader(ys):
    return len(ys)


def keeper(ys):
    STORE.append(ys)


def to_reader(xs: list[int]) -> int:
    return reader(xs)


def to_reader_by_keyword(xs: list[int]) -> int:
    return reader(ys=xs)


def to_keeper(xs: list[int]) -> None:
    keeper(xs)


def to_changer(xs: list[int]) -> None:
    appends(xs)


def view_to_reader(xs: list[int]) -> int:
    return reader(enumerate(xs))


def through_method(xs: list[int], box: Box) -> int:
    return box.total(xs)


def through_protocol(xs: list[int], r: Reader) -> int:
    return r.read(xs)


def iterates_generator(xs: list[int]) -> int:
    n = 0
    for x in yields_from(xs):
        n += x
    return n


def returns_generator(xs: list[int]):
    return yields_from(xs)


async def counts(ys):
    return len(ys)


async def awaits(xs: list[int]) -> int:
    return await counts(xs)


async def creates_coroutine(xs: list[int]):
    return counts(xs)


def ping(xs: list[int], n: int) -> int:
    return pong(xs, n - 1) if n else len(xs)


def pong(xs: list[int], n: int) -> int:
    return ping(xs, n)


def tick(xs: list[int], n: int) -> int:
    return tock(xs, n - 1) if n else len(xs)


def tock(xs: list[int], n: int) -> int:
    if n == 3:
        xs.append(n)
    return tick(xs, n)


def evaluates(xs: list[int]) -> None:
    eval("xs")


if STORE:
    def twice(xs: list[int]) -> int:
        return len(xs)
else:
    def twice(xs: list[int]) -> int:
        xs.append(1)
        return 0
"#;

/// `(only_reads.params, read_only_uses, bound_once)` of each function of `module`, by name.
fn only_reads(p: &Project, module: &str, names: &[&str]) -> Vec<(String, [Vec<String>; 3])> {
    let symbols: String = names.iter().map(|n| format!("{module}.{n}\n")).collect();
    let (code, reports) = inspect_each_json(p, &symbols, &[]);
    assert_eq!(code, 0, "{reports:#?}");
    reports
        .iter()
        .map(|r| {
            let o = &r["only_reads"];
            let facts = [
                strings(&o["params"]),
                strings(&o["read_only_uses"]),
                strings(&o["bound_once"]),
            ];
            (
                r["function"]["qualname"].as_str().unwrap().to_owned(),
                facts,
            )
        })
        .collect()
}

#[test]
fn parameters_only_read_by_the_function_and_every_call_they_are_handed_to() {
    let p = Project::new("inspect_only_read");
    p.write("m.py", ONLY_READ)
        .write("typed.pyi", "def declared(xs: list[int]) -> int: ...\n");
    // Each function with the parameters it only reads.
    let cases: &[(&str, &[&str])] = &[
        // 1. No change: by the name, an alias, a value that may be it, an item store or
        //    deletion, an augmented assignment, a method no table lists as reading.
        ("appends", &[]),
        ("appends_to_alias", &[]),
        ("appends_before_aliasing", &[]),
        ("appends_to_join", &[]),
        ("stores_item", &[]),
        ("deletes_item", &[]),
        ("deletes_key", &["xs"]),
        ("extends_in_place", &[]),
        ("unknown_method", &[]),
        //    Nor handed to a special method of a project class's instance.
        ("added_to_object", &[]),
        ("compared_with_object", &[]),
        ("key_of_object", &[]),
        ("in_object", &[]),
        ("added_to_either", &[]),
        //    Nor compared with a value that may run code of the project: one of unknown class or
        //    of `object`; a `list` runs the builtin's comparison.
        ("compared_with_unknown", &[]),
        ("compared_with_object_typed", &[]),
        ("compared_with_list", &["xs", "other"]),
        //    A class object's comparison is its metaclass's; a key of a project class may run its
        //    code.
        ("compared_with_class", &[]),
        ("keyed_by_object", &[]),
        // 2. Not kept: an attribute, a global, an item, a display, a closure, a lambda, a class
        //    body, a generator expression reading a copy, a yield, a return, a returned view or
        //    a result that may be an argument (`max(a, b)` returns one of them).
        ("stores_attribute", &[]),
        ("stores_global", &[]),
        ("stores_value", &[]),
        ("in_display", &[]),
        ("in_dict", &[]),
        ("unpacked", &["xs", "d"]),
        ("captures", &[]),
        ("captures_in_lambda", &["keys"]),
        ("captures_in_class", &[]),
        ("tests_in_genexp", &["xs", "keys"]),
        ("genexp_of_alias", &["keys"]),
        ("yields", &[]),
        ("yields_from", &["xs"]),
        ("returns", &[]),
        ("returns_view", &[]),
        ("returns_max", &[]),
        // 3. Every call resolves and only reads: Layer 1's reading builtins, the reading methods
        //    of the builtin containers, and project functions that only read it, by position or
        //    keyword, in a call cycle (the greatest fixpoint), through a method, a generator
        //    iterated here, an awaited coroutine; not an unresolved call, a library call Layer 1
        //    does not list as reading, the value itself called, a keyword or a position past `*`
        //    of a library call, a constructor, a variadic parameter, a view handed on, a
        //    protocol, a callee keeping or changing it, a generator or coroutine returned.
        ("reads", &["xs", "d", "s"]),
        ("calls_unresolved", &[]),
        ("prints", &[]),
        ("calls_itself", &[]),
        ("keyword_to_library", &[]),
        ("after_unpacking", &["ys"]),
        //    An item's `__radd__` gets `sum`'s start, the other argument's `__lt__` an argument
        //    of `min`: escaped.
        ("start_of_sum", &["items"]),
        ("compared_by_min", &[]),
        ("removed_from", &[]),
        ("constructs", &[]),
        ("to_variadic", &[]),
        ("unpacked_to_variadic", &["xs"]),
        ("after_unpacking_to_variadic", &["ys"]),
        ("to_reader", &["xs"]),
        ("to_reader_by_keyword", &["xs"]),
        ("to_keeper", &[]),
        ("to_changer", &[]),
        ("view_to_reader", &[]),
        ("through_method", &["xs"]),
        ("through_protocol", &[]),
        ("iterates_generator", &["xs"]),
        ("returns_generator", &[]),
        ("awaits", &["xs"]),
        ("creates_coroutine", &[]),
        ("ping", &["xs"]),
        ("pong", &["xs"]),
        ("tick", &[]),
        ("tock", &[]),
        // A body heki does not model, and a name defined twice whose definitions differ.
        ("evaluates", &[]),
        ("twice", &[]),
    ];
    let names: Vec<&str> = cases.iter().map(|(n, _)| *n).collect();
    let found = only_reads(&p, "m", &names);
    for ((name, expected), (qualname, [params, ..])) in cases.iter().zip(&found) {
        assert_eq!(qualname, name);
        assert_eq!(params, expected, "{name}");
    }
    // A parameter the function only reads is never one `MUT110`'s judgment sees changed.
    let r = report(&p, "m.to_changer");
    assert_eq!(r["changes"]["params"][0]["param"], "xs");
    // The containers a function annotates, the established ones among them, in the text form.
    let containers = |symbol: &str| strings(&report(&p, symbol)["only_reads"]["containers"]);
    assert_eq!(containers("m.reads"), ["xs", "d", "s"]);
    assert_eq!(containers("m.evaluates"), ["xs"]);
    assert_eq!(containers("typed.declared"), ["xs"]);
    assert!(strings(&report(&p, "typed.declared")["only_reads"]["params"]).is_empty());
    assert!(containers("m.Box.size").is_empty());
    let text = |symbol: &str| {
        let (code, out, err) = p.heki(&["inspect", symbol]);
        assert_eq!(code, 0, "{err}");
        out.lines()
            .find(|l| l.starts_with("only reads"))
            .map(str::to_owned)
    };
    assert_eq!(text("m.reads").as_deref(), Some("only reads xs, d, s"));
    assert_eq!(
        text("m.captures_in_lambda").as_deref(),
        Some("only reads keys (not xs)")
    );
    assert_eq!(text("m.appends").as_deref(), Some("only reads none of xs"));
    assert_eq!(text("m.Box.size"), None);
}

/// The local part of premises 5 and 6: every use of the parameter is one its read-only type
/// allows, a value derived from it is only iterated, unpacked, tested, measured, or handed to a
/// call Layer 1 lists as reading it (a set: in no order), and its name is bound once.
const READ_ONLY_USES: &str = r#"from collections.abc import MutableMapping, MutableSequence, MutableSet
from typing import List, ReadOnly, Sequence


def reader(ys):
    return len(ys)


def abstract(xs: list[int], d: dict[str, int], s: set[int], t: set[int], table: dict) -> int:
    first, *rest = xs
    n = xs[0] + len(xs) + xs.count(1) + xs.index(1) + len(xs[1:]) + d.get("k", 0)
    n += len(d.keys()) + len({**d}) + len([*xs]) + max(*xs) + len(table[xs])
    if xs == [1] or xs is None or 1 in xs or not xs:
        n += 1
    if s <= t and s.isdisjoint({1}):
        n += len(s | t) + sorted(s & t)[0] + len(s - {1})
    for x in xs:
        n += x
    acc: list[int] = []
    acc += xs
    print(f"{xs}")
    del first
    n += len(dict(**d)) + any(x > 0 for x in xs)
    return n + reader(xs) + reader(ys=xs) + sum(xs)


def joined(xs: list[int] | None) -> int:
    ys = xs or []
    n = 0
    for y in ys:
        n += y
    a, b = ys
    if ys and 1 in ys and ys is not None:
        n += len(ys)
    return n + len(sorted(ys[1:]))


def copies(xs: list[int]) -> int:
    return len(xs.copy())


def adds(xs: list[int]) -> int:
    return len(xs + [1])


def ors(xs: list[int]) -> int:
    return len(xs | [1])


def orders(xs: list[int], ys: list[int]) -> bool:
    return xs < ys


def negates(xs: list[int]) -> int:
    return -xs


def calls_it(xs: list[int]) -> None:
    xs()


def returns(xs: list[int]) -> list[int]:
    return xs


def yields(xs: list[int]):
    yield xs


def captures(xs: list[int], keys: list[int]) -> bool:
    return any(k in xs for k in keys)


def in_dict(xs: list[int]) -> int:
    return len({"k": xs})


def list_as_mapping(xs: list[int]) -> int:
    return len({**xs})


def keyword_to_library(xs: list[int]) -> int:
    return len(dict(items=xs))


def to_unresolved(xs: list[int], g) -> None:
    g(xs)


def prints(xs: list[int]) -> None:
    print(xs)


def item_of_join(xs: list[int] | None) -> int:
    ys = xs or []
    return ys[0]


def join_compared(xs: list[int] | None, ys: list[int]) -> bool:
    return (xs or []) == ys


def join_formatted(xs: list[int] | None) -> str:
    return f"{xs or []}"


def join_to_reader(xs: list[int] | None) -> int:
    return reader(xs or [])


def join_method(xs: list[int] | None) -> int:
    return (xs or []).count(1)


def join_mapping(d: dict[str, int] | None) -> int:
    return len({**(d or {})})


def iterates_set(s: set[int]) -> list[int]:
    return [x for x in s]


def unpacks_set(s: set[int]) -> int:
    a, b = s
    return a


def lists_set(s: set[int]) -> list[int]:
    return list(s)


def stars_set(s: set[int]) -> int:
    return max(*s)


def extends_with_set(s: set[int]) -> list[int]:
    acc = []
    acc += s
    return acc


def yields_from_set(s: set[int]):
    yield from s


def genexp_over_set(s: set[int]) -> bool:
    return any(x > 0 for x in s)


def default_list(xs: list[int] | None = None) -> int:
    if xs is None:
        xs = []
    return xs.index(1)


def default_dict(d: dict[str, int] | None = None) -> int:
    if d is None:
        d = {}
    return len(d)


def default_set(s: set[int] | None = None) -> int:
    if None is s:
        s = set()
    return len(s)


def default_then_copies(xs: list[int] | None = None) -> int:
    if xs is None:
        xs = []
    return len(xs.copy())


MISSING: list[int] = []


def default_after_sentinel(xs: list[int] = MISSING) -> int:
    if xs is MISSING:
        xs = []
    return len(xs)


def default_after_equality(xs: list[int] | None = None) -> int:
    if xs == None:
        xs = []
    return len(xs)


def default_after_not(xs: list[int] | None = None) -> int:
    if not xs:
        xs = []
    return len(xs)


def rebinds(xs: list[int]) -> int:
    xs = [1]
    return len(xs)


def rebinds_to_other(xs: list[int], ys: list[int]) -> int:
    xs = ys
    return len(xs)


def rebinds_to_sum(xs: list[int] | None, ys: list[int]) -> int:
    if xs is None:
        xs = ys + ys
    return len(xs)


def default_of_other_kind(xs: list[int] | None = None) -> int:
    if xs is None:
        xs = set()
    return len(xs)


def loops_over_name(xs: list[int]) -> int:
    for xs in [[1]]:
        pass
    return 0


def deletes_name(xs: list[int]) -> None:
    del xs


def spellings(a: MutableSequence[int], b: MutableMapping[str, int], c: MutableSet[int], d: List[int], e: "Dict[str, int]", f: "Unknown", g: ReadOnly[list[int]], h: Sequence[int], i: "Deque[int]", *args: list[int]) -> int:
    return 0
"#;

fn strs(names: &[&str]) -> Vec<String> {
    names.iter().map(|n| (*n).to_owned()).collect()
}

#[test]
fn uses_the_read_only_type_allows_and_names_bound_once() {
    let p = Project::new("inspect_read_only_uses");
    p.write("m.py", READ_ONLY_USES);
    // Each function with the parameters whose uses the read-only type allows and those whose
    // name is bound once.
    let all = |names: &[&'static str]| names.to_vec();
    let cases: Vec<(&str, Vec<&str>, Vec<&str>)> = vec![
        (
            "abstract",
            all(&["xs", "d", "s", "t", "table"]),
            all(&["xs", "d", "s", "t", "table"]),
        ),
        ("joined", all(&["xs"]), all(&["xs"])),
        ("copies", vec![], all(&["xs"])),
        ("adds", vec![], all(&["xs"])),
        ("ors", vec![], all(&["xs"])),
        ("orders", vec![], all(&["xs", "ys"])),
        ("negates", vec![], all(&["xs"])),
        ("calls_it", vec![], all(&["xs"])),
        ("returns", vec![], all(&["xs"])),
        ("yields", vec![], all(&["xs"])),
        ("captures", vec!["keys"], all(&["xs", "keys"])),
        ("in_dict", vec![], all(&["xs"])),
        ("list_as_mapping", vec![], all(&["xs"])),
        ("keyword_to_library", vec![], all(&["xs"])),
        ("to_unresolved", vec![], all(&["xs"])),
        ("prints", vec![], all(&["xs"])),
        ("item_of_join", vec![], all(&["xs"])),
        ("join_compared", vec!["ys"], all(&["xs", "ys"])),
        ("join_formatted", vec![], all(&["xs"])),
        ("join_to_reader", vec![], all(&["xs"])),
        ("join_method", vec![], all(&["xs"])),
        ("join_mapping", vec![], all(&["d"])),
        ("iterates_set", vec![], all(&["s"])),
        ("unpacks_set", vec![], all(&["s"])),
        ("lists_set", vec![], all(&["s"])),
        ("stars_set", vec![], all(&["s"])),
        ("extends_with_set", vec![], all(&["s"])),
        ("yields_from_set", vec![], all(&["s"])),
        ("genexp_over_set", vec![], all(&["s"])),
        ("default_list", all(&["xs"]), all(&["xs"])),
        ("default_dict", all(&["d"]), all(&["d"])),
        ("default_set", all(&["s"]), all(&["s"])),
        ("default_then_copies", vec![], all(&["xs"])),
        ("default_after_sentinel", all(&["xs"]), vec![]),
        ("default_after_equality", all(&["xs"]), vec![]),
        ("default_after_not", all(&["xs"]), vec![]),
        ("rebinds", all(&["xs"]), vec![]),
        ("rebinds_to_other", all(&["xs", "ys"]), vec!["ys"]),
        ("rebinds_to_sum", vec!["xs"], vec!["ys"]),
        ("default_of_other_kind", all(&["xs"]), vec![]),
        ("loops_over_name", all(&["xs"]), vec![]),
        ("deletes_name", all(&["xs"]), vec![]),
    ];
    let names: Vec<&str> = cases.iter().map(|(n, ..)| *n).collect();
    let found = only_reads(&p, "m", &names);
    for ((name, uses, once), (qualname, [_, ro, bound])) in cases.iter().zip(&found) {
        assert_eq!(qualname, name);
        assert_eq!((ro, bound), (&strs(uses), &strs(once)), "{name}");
    }
    // `list`, `dict`, `set`, their `typing` forms and the mutable abstract containers, also
    // unresolved, are containers; `ReadOnly`, a read-only type, an unknown name, and a variadic
    // parameter are not.
    let r = report(&p, "m.spellings");
    assert_eq!(
        strings(&r["only_reads"]["containers"]),
        ["a", "b", "c", "d", "e"]
    );
}

/// A value `typing.cast` hands back is the parameter's object itself
/// (`ownership::handed_back`): its uses are the parameter's.
const READS_THROUGH_CAST: &str = r#"import typing
from typing import cast


def measures(xs: list[int]) -> int:
    ys = cast(list[int], xs)
    return len(ys) + ys[0]


def measures_by_module(xs: list[int]) -> int:
    return len(typing.cast(list[int], xs))


def appends(xs: list[int]) -> None:
    ys = cast(list[int], xs)
    ys.append(1)


def returns(xs: list[int]) -> list[int]:
    return cast(list[int], xs)
"#;

#[test]
fn a_value_a_cast_hands_back_is_read_as_the_parameter() {
    let p = Project::new("inspect_reads_through_cast");
    p.write("m.py", READS_THROUGH_CAST);
    // Each function with the parameters it only reads and those whose uses the read-only type
    // allows.
    let cases: &[(&str, &[&str])] = &[
        ("measures", &["xs"]),
        ("measures_by_module", &["xs"]),
        ("appends", &[]),
        ("returns", &[]),
    ];
    let names: Vec<&str> = cases.iter().map(|(n, _)| *n).collect();
    let found = only_reads(&p, "m", &names);
    for ((name, expected), (qualname, [params, uses, once])) in cases.iter().zip(&found) {
        assert_eq!(qualname, name);
        assert_eq!((params, uses), (&strs(expected), &strs(expected)), "{name}");
        assert_eq!(once, &strs(&["xs"]), "{name}");
    }
}
