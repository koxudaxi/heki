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
    let (_, v, _) = inspect(&p, &["mod.Loader._remembers"]);
    assert_eq!(v["error"]["suggestions"], serde_json::json!([]));
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
        serde_json::json!([{"kind": "effect-boundaries", "name": "no-net", "roles": ["boundary-scope"]}])
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
  other objects  not computed (an ownership contract, MUT108, or MUT112 propagates them)
retains   stores url; what it keeps is not computed (needs an ownership contract)
returns   value: state that outlives the call
callers   1
  handler  users.py:39
reports
  pkg/users.py:39  EFF101 `pkg.users.Loader.load` always runs the waiting call `requests.get` on the event loop (2 calls down, on every path)
  pkg/users.py:39  EFF101 `pkg.users.Loader.load` always runs the waiting call `time.sleep` on the event loop (2 calls down, on every path)
contracts none
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
    has(&run, "through subclass facts.Worker:");
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
    has(&out, "contracts\n  write-owners store: allowed-writer");
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
    has(&out, "unknown: unresolved_calls (first at pkg.calls.vague)");
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
        "class Model:\n    def __init__(self):\n        self.name = \"\"\n        self.items = [[]]\n\n\ndef delete(m: Model):\n    del m.name\n\n\ndef set_by_name(m: Model):\n    setattr(m, \"name\", \"x\")\n\n\ndef set_in_dict(m: Model):\n    m.__dict__[\"name\"] = \"x\"\n\n\ndef append(m: Model):\n    m.items.append(1)\n\n\ndef append_to_alias(m: Model):\n    xs = m.items\n    xs.append(1)\n\n\ndef append_to_item(m: Model):\n    m.items[0].append(1)\n\n\ndef helper(x):\n    x.items.append(1)\n\n\ndef through_helper(m: Model):\n    helper(m)\n",
    );
    for (symbol, kind) in [
        ("delete", "attr_delete"),
        ("set_by_name", "set_attr"),
        ("set_in_dict", "dict_direct"),
        ("append", "container_mut"),
        ("append_to_alias", "alias_container_mut"),
        ("append_to_item", "item_mut"),
        ("through_helper", "indirect_helper"),
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
