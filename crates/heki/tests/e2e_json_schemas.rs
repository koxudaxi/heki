//! The JSON Schemas of `docs/schemas/`, against what the real binary writes: every format's
//! output on several projects must meet its schema, and the objects together must use every
//! property the schema declares, so that a field the serializer has and the schema lacks fails
//! on `additionalProperties: false`, and a field the schema has and the serializer lacks fails
//! on the property no output uses.

mod common;

use common::schema::{declared_properties, errors, seen_properties};
use common::{Project, repo_file};
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// The schema `heki check --output-format-json-schema <name>` prints, which is also the file
/// the documentation publishes.
fn schema(name: &str) -> Value {
    let p = Project::new("schema_print");
    let (code, out, err) = p.heki(&["check", "--output-format-json-schema", name]);
    assert_eq!((code, err.as_str()), (0, ""), "{name}");
    assert_eq!(
        out,
        repo_file(&format!("docs/schemas/{name}.json")),
        "docs/schemas/{name}.json is out of date (python scripts/build_docs.py)"
    );
    let schema: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(
        schema["$id"],
        json!(format!("https://heki.koxudaxi.dev/schemas/{name}.json"))
    );
    assert_eq!(
        schema["$schema"],
        json!("https://json-schema.org/draft/2020-12/schema")
    );
    schema
}

/// Every object of `schema` that has properties says no other is allowed, and every property
/// has a description.
fn assert_closed_and_described(name: &str, schema: &Value) {
    fn walk(name: &str, at: &str, node: &Value) {
        match node {
            Value::Object(map) => {
                if let Some(Value::Object(properties)) = map.get("properties") {
                    assert_eq!(
                        map.get("additionalProperties"),
                        Some(&json!(false)),
                        "{name}{at}: an object that is open to other properties"
                    );
                    for (key, property) in properties {
                        assert!(
                            property.get("description").is_some()
                                || property.get("$ref").is_some()
                                || property.get("const").is_some(),
                            "{name}{at}/{key}: no description"
                        );
                    }
                }
                for (key, inner) in map {
                    let skip_names = key == "properties";
                    if skip_names {
                        for (property, inner) in inner.as_object().unwrap() {
                            walk(name, &format!("{at}/{property}"), inner);
                        }
                    } else {
                        walk(name, &format!("{at}/{key}"), inner);
                    }
                }
            }
            Value::Array(items) => {
                for (i, inner) in items.iter().enumerate() {
                    walk(name, &format!("{at}/{i}"), inner);
                }
            }
            _ => {}
        }
    }
    walk(name, "", schema);
}

/// `value` meets the schema.
fn assert_valid(schema: &Value, value: &Value) {
    let found = errors(schema, value);
    assert!(found.is_empty(), "{found:#?}\n{value:#}");
}

/// The objects `values` describe by `$defs` entry `name` use every property it declares,
/// except those `unused` names (each a field these fixtures cannot produce, which the test says).
fn assert_all_used(schema: &Value, values: &[Value], name: &str, unused: &[&str]) {
    let seen = seen_properties(schema, values);
    let used = seen.get(name).cloned().unwrap_or_default();
    let declared = declared_properties(schema, name);
    let missing: BTreeSet<_> = declared
        .difference(&used)
        .filter(|p| !unused.contains(&p.as_str()))
        .collect();
    assert!(
        missing.is_empty(),
        "`{name}` has no output with {missing:?}"
    );
}

/// A project of `files` with `config` as its `pyproject.toml` tool table.
fn project(name: &str, config: &str, files: &[(&str, &str)]) -> Project {
    let p = Project::new(name);
    if !config.is_empty() {
        p.write(
            "pyproject.toml",
            &format!("[project]\nname = \"t\"\n\n{config}"),
        );
    }
    for (path, text) in files {
        p.write(path, text);
    }
    p
}

// ------------------------------------------------------------------------------ check

const CHAIN: &str = "\
import time


def helper():
    time.sleep(1)


async def handler():
    helper()


def grow(xs):
    out = []
    for x in xs:
        out = out + [x]
    return out
";

const MODEL: &str = "\
class Registry:
    def __init__(self) -> None:
        self.entries: list[int] = []
        self.name = \"\"
";

const STORE: &str = "\
from pkg.model import Registry


class Store:
    def add(self, r: Registry, x: int) -> None:
        r.entries.append(x)
";

const APP: &str = "\
from pkg.model import Registry


def sneak(r: Registry) -> None:
    r.entries.append(2)
";

const OWNERS: &str = "\
[[tool.heki.write-owners]]
name = \"registry\"
targets = [\"Registry.entries\"]
allowed-writers = [\"pkg.store.Store.*\"]
message = \"add entries through `Store`\"
";

const HELPER_READS: &str = "\
class Model:
    def __init__(self) -> None:
        self.value = 0


def _peek(m: Model) -> int:
    return m.value + m.value


def run(m: Model) -> int:
    return _peek(m)
";

const READERS: &str = "\
[[tool.heki.read-owners]]
name = \"readers\"
targets = [\"Model.value\"]
allowed-readers = [\"app.Store.*\"]
";

fn check_json(p: &Project, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["check", "--output-format", "json"];
    all.extend_from_slice(args);
    let (code, out, err) = p.heki(&all);
    assert!(code <= 1, "{out}{err}");
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"))
}

#[test]
fn the_diagnostics_array_meets_its_schema() {
    let schema = schema("check-1");
    assert_closed_and_described("check-1", &schema);
    let empty = project("schema_check_empty", "", &[("a.py", "x = 1\n")]);
    let none = check_json(&empty, &[]);
    assert_eq!(none, Vec::<Value>::new());
    assert_valid(&schema, &json!(none));

    let chain = project("schema_check_chain", "", &[("a.py", CHAIN)]);
    let owners = project(
        "schema_check_owners",
        OWNERS,
        &[
            ("pkg/__init__.py", ""),
            ("pkg/model.py", MODEL),
            ("pkg/store.py", STORE),
            ("pkg/app.py", APP),
        ],
    );
    let counted = project("schema_check_sites", READERS, &[("app.py", HELPER_READS)]);
    let mut all = Vec::new();
    for (p, args) in [
        (&chain, vec!["--heuristic"]),
        (&owners, vec!["--select", "MUT201"]),
        (&counted, vec!["--select", "MUT203"]),
    ] {
        let found = check_json(p, &args);
        assert!(!found.is_empty());
        assert_valid(&schema, &json!(found));
        all.extend(found);
    }
    // Every report is a diagnostic; the fixtures use each of its properties.
    let all = json!(all);
    for name in [
        "Diagnostic",
        "RelatedLocation",
        "TextEdit",
        "AllowedExample",
        "Span",
    ] {
        assert_all_used(&schema, std::slice::from_ref(&all), name, &[]);
    }
    // A property the schema does not know is refused.
    let mut extra = all[0].clone();
    extra["extra"] = json!(1);
    assert!(!errors(&schema, &json!([extra])).is_empty());
}

// ------------------------------------------------------------------------------ files

const SUPPRESSED: &str = "\
import time


async def handler():
    time.sleep(1)  # heki: ignore[EFF101]
    time.sleep(2)  # noqa: EFF101
    time.sleep(3)  # noqa
    time.sleep(4)  # heki: ignore
";

#[test]
fn the_suppressed_report_meets_its_schema() {
    let schema = schema("report-suppressed-1");
    assert_closed_and_described("report-suppressed-1", &schema);
    let empty = project("schema_suppressed_empty", "", &[("a.py", "x = 1\n")]);
    assert_eq!(
        empty.heki(&["check", "--report-suppressed", "out.json"]).0,
        0
    );
    let none: Value = serde_json::from_str(&empty.read("out.json")).unwrap();
    assert_eq!(none, json!({"schema_version": 1, "suppressed": []}));
    assert_valid(&schema, &none);

    let p = project("schema_suppressed", "", &[("a.py", SUPPRESSED)]);
    assert!(p.heki(&["check", "--report-suppressed", "out.json"]).0 <= 1);
    let doc: Value = serde_json::from_str(&p.read("out.json")).unwrap();
    assert_valid(&schema, &doc);
    let kinds: BTreeSet<&str> = doc["suppressed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["directive"]["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        BTreeSet::from(["heki-ignore", "heki-ignore-bare", "noqa-code", "noqa-bare"])
    );
    for name in ["Entry", "Directive"] {
        assert_all_used(&schema, std::slice::from_ref(&doc), name, &[]);
    }
    assert_eq!(
        declared_top(&schema),
        BTreeSet::from(["schema_version".to_owned(), "suppressed".to_owned()])
    );
}

/// The property names of the schema's own object.
fn declared_top(schema: &Value) -> BTreeSet<String> {
    schema["properties"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect()
}

const MUTATING: &str = "\
import time


class Cache:
    def __init__(self) -> None:
        self.n = 0

    def bump(self, items):
        items.append(1)
        self.n += 1
        time.sleep(1)
        return items

    def store(self, items):
        self.kept = items
";

#[test]
fn the_summary_snapshot_meets_its_schema() {
    let schema = schema("summary-snapshot-1");
    assert_closed_and_described("summary-snapshot-1", &schema);
    let empty = project("schema_snapshot_empty", "", &[("a.py", "x = 1\n")]);
    assert_eq!(empty.heki(&["snapshot", "--write"]).0, 0);
    let none: Value = serde_json::from_str(&empty.read(".heki-summary-snapshot.json")).unwrap();
    assert_eq!(none["functions"], json!({}));
    assert_valid(&schema, &none);
    let (code, printed, _) = empty.heki(&["snapshot"]);
    assert_eq!(code, 0);
    assert_valid(&schema, &serde_json::from_str(&printed).unwrap());

    let p = project("schema_snapshot", "", &[("m.py", MUTATING)]);
    assert_eq!(p.heki(&["snapshot", "--write"]).0, 0);
    let snapshot: Value = serde_json::from_str(&p.read(".heki-summary-snapshot.json")).unwrap();
    assert_valid(&schema, &snapshot);
    assert_all_used(
        &schema,
        std::slice::from_ref(&snapshot),
        "SnapshotEntry",
        &[],
    );
    assert_eq!(declared_top(&schema).len(), 2);
}

// A baseline file is TOML; the schema describes its values as JSON.
fn baseline_json(text: &str) -> Value {
    serde_json::to_value(text.parse::<toml::Table>().unwrap()).unwrap()
}

const V1: &str = "\
[[entries]]
path = \"a.py\"
qualname = \"handler\"
rule_code = \"EFF101\"
target = \"time.sleep\"
count = 1
";

const V2: &str = "\
version = 2

[[entries]]
path = \"a.py\"
qualname = \"handler\"
rule_code = \"EFF101\"
target = \"time.sleep\"
count = 2
";

const V3: &str = "\
version = 3

[[entries]]
path = \"a.py\"
qualname = \"handler\"
rule_code = \"EFF101\"
target = \"time.sleep\"
count = 2
heuristic = 1

[[entries]]
rule_code = \"IMP101\"
target = \"a, b\"
count = 1
";

#[test]
fn every_baseline_version_heki_reads_meets_its_schema() {
    let p = project(
        "schema_baseline",
        "",
        &[(
            "a.py",
            "import time\n\n\nasync def handler():\n    time.sleep(1)\n",
        )],
    );
    let schemas: Vec<Value> = (1..=3).map(|v| schema(&format!("baseline-{v}"))).collect();
    for (v, schema) in schemas.iter().enumerate() {
        assert_closed_and_described(&format!("baseline-{}", v + 1), schema);
    }
    // The file heki writes.
    assert_eq!(
        p.heki(&["check", "--baseline", "b.toml", "--update-baseline"])
            .0,
        0
    );
    let written = baseline_json(&p.read("b.toml"));
    assert_eq!(written["version"], json!(3));
    assert_valid(&schemas[2], &written);
    // An empty baseline.
    p.write("empty.toml", "version = 3\nentries = []\n");
    assert_valid(&schemas[2], &baseline_json(&p.read("empty.toml")));

    for (version, text) in [(1, V1), (2, V2), (3, V3)] {
        p.write("old.toml", text);
        // The file is one this heki reads.
        let (code, out, err) = p.heki(&["check", "--baseline", "old.toml"]);
        assert!(code <= 1, "version {version}: {out}{err}");
        let file = baseline_json(text);
        assert_valid(&schemas[version - 1], &file);
        // And the other versions do not take it.
        for (other, other_schema) in schemas.iter().enumerate() {
            if other + 1 != version {
                assert!(
                    !errors(other_schema, &file).is_empty(),
                    "version {version} file meets the schema of version {}",
                    other + 1
                );
            }
        }
    }
    let v3 = baseline_json(V3);
    assert_all_used(&schemas[2], std::slice::from_ref(&v3), "BaselineEntry", &[]);
    // Version 1 files may leave out `version`, and say it is 1.
    let mut one = baseline_json(V1);
    one["version"] = json!(1);
    assert_valid(&schemas[0], &one);
}

// ------------------------------------------------------------------------------ inspect

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

const OWNER_CONTRACTS: &str = r#"[project]
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

const OWNER_CODE: &str = r#"class Model:
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

const FACT_CONTRACTS: &str = "\
[[tool.heki.write-owners]]
name = \"log\"
targets = [\"Log.lines\"]
allowed-writers = [\"m.Log.*\"]

[[tool.heki.call-owners]]
name = \"sleepers\"
targets = [\"m.work\"]
allowed-callers = [\"m.Mixin.*\"]
";

/// What the facts project adds to `FACTS`: a blocking call in a coroutine, a module global and
/// a closure variable that functions change.
const MORE_FACTS: &str = "

COUNTER = {}


def tally(k):
    COUNTER[k] = 1


def fill(target):
    out = target

    def add(x):
        out.append(x)

    add(1)


async def early():
    time.sleep(1)
";

const LATER: &str = "

async def later():
    requests.get(\"u\")
";

const KEYED_OWNERS: &str = "\
[[tool.heki.write-owners]]
name = \"owned\"
targets = [\"Model.extras\"]
allowed-writers = [\"nobody\"]
";

const KEYED: &str = "\
class Model:
    def __init__(self):
        self.extras = {}


def put(m: Model, key):
    m.extras[\"init\"] = 1
    m.extras[key] = 2
";

fn inspect_json(p: &Project, args: &[&str]) -> (i32, Value) {
    let mut all = vec!["inspect", "--heuristic"];
    all.extend_from_slice(args);
    all.extend(["--output-format", "json"]);
    let (code, out, err) = p.heki(&all);
    let value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}\n{err}"));
    (code, value)
}

/// The reports `heki inspect` writes on `symbols` of `p`, each without and with `--callers`.
fn reports_on(schema: &Value, p: &Project, symbols: &[&str], extra: &[&str]) -> Vec<Value> {
    let mut all = Vec::new();
    for symbol in symbols {
        for callers in [&[][..], &["--callers"][..]] {
            let mut args = vec![*symbol];
            args.extend_from_slice(extra);
            args.extend_from_slice(callers);
            let (code, report) = inspect_json(p, &args);
            assert_eq!(code, 0, "{symbol}: {report:#}");
            assert_valid(schema, &report);
            all.push(report);
        }
    }
    all
}

#[test]
fn the_inspect_output_meets_its_schema() {
    let schema = schema("inspect-1");
    assert_closed_and_described("inspect-1", &schema);
    let p = project(
        "schema_inspect",
        FACT_CONTRACTS,
        &[("m.py", &format!("{FACTS}{MORE_FACTS}"))],
    );
    // A baseline that allows `early` and not `later`.
    assert_eq!(
        p.heki(&[
            "check",
            "--heuristic",
            "--baseline",
            "b.toml",
            "--update-baseline"
        ])
        .0,
        0
    );
    p.write("m.py", &format!("{FACTS}{MORE_FACTS}{LATER}"));
    let mut all = reports_on(
        &schema,
        &p,
        &[
            "m.maybe",
            "m.Store.conn",
            "m.Store.note",
            "m.Store.view",
            "m.Store.forget",
            "m.Log.add",
            "m.rows",
            "m.lazy",
            "m.work",
            "m.Mixin.run",
            "m.Worker.execute",
            "m.ident",
            "m.first",
            "m.fresh",
            "m.tally",
            "m.fill",
            "m.early",
            "m.later",
        ],
        &["--baseline", "b.toml"],
    );
    // Every way a symbol names no single function.
    for (symbol, kind) in [
        ("m", "module"),
        ("m.Store", "class"),
        ("m.nothing", "unknown"),
        ("nothing.at.all", "unknown"),
        ("m.py", "invalid"),
        ("pkg.py:1", "outside_project"),
    ] {
        let (code, error) = inspect_json(&p, &[symbol]);
        assert_eq!(code, 2, "{symbol}: {error:#}");
        assert_eq!(error["error"]["kind"], json!(kind), "{symbol}: {error:#}");
        assert_valid(&schema, &error);
        all.push(error);
    }
    // An ambiguous name lists the candidates.
    p.write("n.py", "def ident(x):\n    return x\n");
    let (code, ambiguous) = inspect_json(&p, &["*ident"]);
    assert_eq!(code, 2);
    assert_eq!(ambiguous["error"]["kind"], "ambiguous", "{ambiguous:#}");
    assert_valid(&schema, &ambiguous);
    all.push(ambiguous);
    // Standard input: an array of reports and errors.
    let each = {
        use std::io::Write;
        use std::process::Stdio;
        let mut child = common::heki_command()
            .args(["inspect", "-", "--output-format", "json"])
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
            .write_all(b"m.first\nm.nothing\nm.*ow\n")
            .unwrap();
        let out = child.wait_with_output().unwrap();
        serde_json::from_slice::<Value>(&out.stdout).unwrap()
    };
    assert!(each.as_array().is_some_and(|a| a.len() >= 3), "{each:#}");
    assert_valid(&schema, &each);

    let keyed = project("schema_inspect_keys", KEYED_OWNERS, &[("m.py", KEYED)]);
    all.extend(reports_on(
        &schema,
        &keyed,
        &["put"],
        &["--extend-select", "MUT112"],
    ));

    let owners = Project::new("schema_inspect_contracts");
    owners
        .write("pyproject.toml", OWNER_CONTRACTS)
        .write("m.py", OWNER_CODE)
        .write(
            "wrap.py",
            "from m import Model, Store\n\n\ndef secret_of(m: Model):\n    return Store().read(m)\n",
        );
    all.extend(reports_on(
        &schema,
        &owners,
        &[
            "wrap.secret_of",
            "m.Store.read",
            "m.render",
            "m.Model.reset",
            "m.Model.save",
        ],
        &[],
    ));

    let panels = Project::new("schema_inspect_panels");
    for (file, text) in common::panels::files(&[]) {
        panels.write(&format!("panels/{file}"), &text);
    }
    panels.write("panels/__init__.py", "");
    let (code, registered) = inspect_json(&panels, &["panels.views.Page.store"]);
    assert_eq!(code, 0, "{registered:#}");
    assert_valid(&schema, &registered);
    all.push(registered);

    let all = json!(all);
    for name in [
        "InspectReport",
        "FunctionInfo",
        "Purity",
        "UnknownReach",
        "UnknownAt",
        "Hop",
        "EffectInfo",
        "GuardInfo",
        "SubclassChain",
        "Changes",
        "ParamChangeInfo",
        "SelfAttrChange",
        "ClassAttrWrite",
        "GlobalChange",
        "Retains",
        "OnlyReads",
        "Returns",
        "OriginsInfo",
        "SharedWith",
        "CallersInfo",
        "CallerEntry",
        "Report",
        "ContractRole",
        "RegisteredContext",
        "Site",
        "Condition",
        "Location",
        "ErrorBody",
        "Candidate",
        "InspectError",
    ] {
        // `Report` flattens a diagnostic: its two properties written only for some reports are
        // used by the `check` test.
        let unused: &[&str] = if name == "Report" {
            &["allowed_examples", "sites"]
        } else {
            &[]
        };
        assert_all_used(&schema, std::slice::from_ref(&all), name, unused);
    }
}

const IMPORTS_A: &str = "\
import importlib
import os
import pkg.b
import requests
from . import b
from .b import thing
from .. import above
from pkg import *
from typing import TYPE_CHECKING

try:
    import vanished
except ImportError:
    vanished = None

if TYPE_CHECKING:
    import decimal


def f(name):
    import json
    importlib.import_module(\"pkg.plugins.\" + name)
    importlib.import_module(name)
    return json
";

#[test]
fn the_inspect_imports_output_meets_its_schema() {
    let schema = schema("inspect-imports-1");
    assert_closed_and_described("inspect-imports-1", &schema);
    let p = project(
        "schema_imports",
        "",
        &[
            ("pkg/__init__.py", ""),
            ("pkg/a.py", IMPORTS_A),
            ("pkg/b.py", "thing = 1\n"),
            ("pkg/c.pyi", "import os\n"),
            ("pkg/plugins/__init__.py", ""),
            ("pkg/plugins/one.py", "x = 1\n"),
            ("tests/test_a.py", "import pkg.a\n"),
        ],
    );
    let mut all = Vec::new();
    for query in ["pkg.*", "tests.*", "pkg.a"] {
        let (code, out, err) = p.heki(&["inspect", "--imports", query, "--output-format", "json"]);
        assert_eq!(code, 0, "{out}{err}");
        let report: Value = serde_json::from_str(&out).unwrap();
        assert_valid(&schema, &report);
        all.push(report);
    }
    for (query, kind) in [("nothing.*", "unknown"), ("pkg.[", "invalid")] {
        let (code, out, _) = p.heki(&["inspect", "--imports", query, "--output-format", "json"]);
        assert_eq!(code, 2, "{query}: {out}");
        let error: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(error["error"]["kind"], json!(kind), "{query}: {error:#}");
        assert_valid(&schema, &error);
    }
    for name in ["ImportsReport", "ModuleReport", "SiteReport", "Loads"] {
        assert_all_used(&schema, &all, name, &[]);
    }
}

// ------------------------------------------------------------------------------ rule, assumptions

#[test]
fn the_rule_output_meets_its_schema() {
    let schema = schema("rule-1");
    assert_closed_and_described("rule-1", &schema);
    let p = Project::new("schema_rule");
    let (code, out, _) = p.heki(&["rule", "--all", "--output-format", "json"]);
    assert_eq!(code, 0);
    let all: Value = serde_json::from_str(&out).unwrap();
    assert!(all.as_array().unwrap().len() > 50);
    assert_valid(&schema, &all);
    let mut one = Vec::new();
    for rule in ["EFF101", "MUT101", "OPT101", "IMP101"] {
        let (code, out, _) = p.heki(&["rule", rule, "--output-format", "json"]);
        assert_eq!(code, 0, "{rule}");
        let record: Value = serde_json::from_str(&out).unwrap();
        assert!(record.is_object());
        assert_valid(&schema, &record);
        one.push(record);
    }
    let mut values = vec![all];
    values.extend(one);
    for name in [
        "RuleRecord",
        "Certification",
        "Checked",
        "Harm",
        "PublishedMeasurement",
        "ExperimentalFeature",
        "Premise",
    ] {
        let seen = seen_properties(&schema, &values);
        let used = seen.get(name).cloned().unwrap_or_default();
        // No rule is certified on the rare-failure path yet, so no record carries its real bugs.
        let unused_today: &[&str] = match name {
            "Certification" => &["real_bugs"],
            _ => &[],
        };
        let missing: BTreeSet<_> = declared_properties(&schema, name)
            .difference(&used)
            .filter(|p| !unused_today.contains(&p.as_str()))
            .cloned()
            .collect();
        assert!(
            missing.is_empty(),
            "`{name}` has no output with {missing:?}"
        );
    }
}

#[test]
fn the_assumptions_output_meets_its_schema() {
    let schema = schema("assumptions-1");
    assert_closed_and_described("assumptions-1", &schema);
    let empty = project("schema_assumptions_empty", "", &[("a.py", "x = 1\n")]);
    let (code, out, _) = empty.heki(&["assumptions", "--json"]);
    assert_eq!(code, 0);
    let none: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(none, json!([]));
    assert_valid(&schema, &none);

    let p = project(
        "schema_assumptions",
        "",
        &[(
            "a.py",
            "import sys\n\n\ndef f(code):\n    eval(code)\n    return locals()\n\n\ndef g():\n    return sys._getframe()\n",
        )],
    );
    let (code, out, _) = p.heki(&["assumptions", "--json"]);
    assert_eq!(code, 0);
    let found: Value = serde_json::from_str(&out).unwrap();
    assert!(found.as_array().unwrap().len() >= 2, "{found:#}");
    assert_valid(&schema, &found);
    assert_all_used(
        &schema,
        std::slice::from_ref(&found),
        "AssumptionRecord",
        &[],
    );
}

// ------------------------------------------------------------------------------ error

#[test]
fn what_a_failed_command_writes_meets_its_schema() {
    let schema = schema("error-1");
    assert_closed_and_described("error-1", &schema);
    let mut all = Vec::new();
    let broken_config = project(
        "schema_error_config",
        "[tool.heki.lint]\nselect = [\"NOPE1\"]\n",
        &[("a.py", "x = 1\n")],
    );
    let unknown_key = project(
        "schema_error_key",
        "[tool.heki]\nselct = []\n",
        &[("a.py", "x = 1\n")],
    );
    let plain = project("schema_error_plain", "", &[("a.py", "x = 1\n")]);
    for (p, args) in [
        (&plain, vec!["check", "missing.py"]),
        (&plain, vec!["check", "--select", "NOPE1"]),
        (&broken_config, vec!["check"]),
        (&unknown_key, vec!["check"]),
        (&plain, vec!["check", "--baseline", "absent.toml"]),
    ] {
        let mut all_args = args.clone();
        all_args.extend(["--output-format", "json"]);
        let (code, out, _) = p.heki(&all_args);
        assert_eq!(code, 2, "{args:?}: {out}");
        let error: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
        assert_valid(&schema, &error);
        all.push(error);
    }
    assert_all_used(&schema, &all, "Problem", &[]);
}
