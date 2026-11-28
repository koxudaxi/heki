//! What a contract cannot attribute, end to end through the real binary: with
//! `unresolved = "report"`, an effect boundary reports the calls heki cannot resolve, the bodies
//! it does not model, and the writes of an attribute it names on an object of unknown class in
//! its scopes' reach, each with its chain, and the JSON output lists every site of such a report
//! (`unresolved_sites`), which its baseline entry counts but does not key; an owner contract
//! judges a function some code hands on as a value (a display, a class attribute, a return, an
//! argument) in that function, as if it had no callers. Without the key, neither is reported.
//!
//! Every fixture marks the lines that must be reported with the rule code in a comment
//! (`# EFF201`); every other line must not be.

mod common;

use common::Project;
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// `(line, rule, message)` of every report of `rules` in `path`.
fn reported(p: &Project, path: &str, rules: &[&str]) -> Vec<(u64, String, String)> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<(u64, String, String)> = diags
        .iter()
        .filter(|d| d["path"] == path && rules.contains(&d["rule_code"].as_str().unwrap()))
        .map(|d| {
            (
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
                d["message"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

/// The `(line, rule)` of every line of `text` ending in `# <rule>` for one of `rules`.
fn marked(text: &str, rules: &[&str]) -> Vec<(u64, String)> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        for rule in rules {
            if line.trim_end().ends_with(&format!("# {rule}")) {
                out.push((i as u64 + 1, (*rule).to_owned()));
            }
        }
    }
    out
}

const BOUNDARIES: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "checks"
scopes = ["pkg.checks.*"]
write-universe = ["Field.required"]
allow-writes = []
allowed-callees = ["pkg.helpers.allowed"]
unresolved = "report"

[[tool.heki.effect-boundaries]]
name = "lenient"
scopes = ["pkg.lenient.*"]
write-universe = ["Field.required"]
allow-writes = []
"#;

const MODEL: &str = r#"class Field:
    def __init__(self) -> None:
        self.required = False
"#;

const HELPERS: &str = r#"import os


def call(run, field):
    run(field)


def allowed(run, field):
    run(field)


def write(thing):
    thing.required = True


def evaluated(text):
    return eval(text)


class Maker:
    def __init__(self, field):
        self.field = field


def make(field):
    return field


if os.environ.get("PLAIN"):
    make = Maker
"#;

const STUBBED: &str = r#"from pkg.model import Field

def go(field: Field) -> None: ...
"#;

/// The shapes a boundary cannot judge, and the ones it can: a call of a name bound to a
/// function on one path and to a class on another is unresolved; a call of a library function
/// the catalog has no entry for is not.
const SHAPES: &str = r#"import vendor

from pkg import helpers, stubbed
from pkg.helpers import make
from pkg.model import Field


def through_unknown(field: Field, kinds: dict) -> None:
    kinds["x"](field)  # EFF201


def through_helper(field: Field, run) -> None:
    helpers.call(run, field)  # EFF201


def through_allowed(field: Field, run) -> None:
    helpers.allowed(run, field)


def writes_unknown(thing) -> None:
    thing.required = True  # EFF201


def writes_through_helper(thing) -> None:
    helpers.write(thing)  # EFF201


def writes_other(thing) -> None:
    thing.other = True


def writes_named(thing, name: str) -> None:
    setattr(thing, name, True)  # EFF201


def evaluates(field: Field) -> None:
    helpers.evaluated("1")  # EFF201


def through_stub(field: Field) -> None:
    stubbed.go(field)  # EFF201


def writes_field(field: Field) -> None:
    field.required = True  # EFF201


def through_either(field: Field) -> None:
    make(field)  # EFF201


def through_library(field: Field) -> None:
    vendor.process(field)
"#;

fn boundary_project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("pyproject.toml", BOUNDARIES);
    p.write("pkg/__init__.py", "");
    p.write("pkg/model.py", MODEL);
    p.write("pkg/helpers.py", HELPERS);
    p.write("pkg/stubbed.pyi", STUBBED);
    p.write("pkg/checks.py", SHAPES);
    p.write("pkg/lenient.py", &SHAPES.replace("  # EFF201", ""));
    p
}

#[test]
fn a_boundary_reporting_what_it_cannot_judge_reports_each_shape_with_its_chain() {
    let p = boundary_project("unattributed_boundaries");
    let found = reported(&p, "pkg/checks.py", &["EFF201"]);
    let lines: Vec<(u64, String)> = found.iter().map(|(l, r, _)| (*l, r.clone())).collect();
    assert_eq!(lines, marked(SHAPES, &["EFF201"]), "{found:#?}");
    let message = |line: u64| &found.iter().find(|(l, _, _)| *l == line).unwrap().2;
    let at = |needle: &str| SHAPES.lines().position(|l| l.contains(needle)).unwrap() as u64 + 1;
    assert_eq!(
        message(at("kinds[\"x\"]")),
        "`through_unknown` reaches a call heki cannot resolve (`a value computed in place`), so effect boundary `checks` cannot tell what it does there"
    );
    assert_eq!(
        message(at("helpers.call(run")),
        "`through_helper` reaches a call heki cannot resolve (`run` via `pkg.helpers.call`), so effect boundary `checks` cannot tell what it does there"
    );
    assert_eq!(
        message(at("thing.required = True")),
        "`writes_unknown` writes `required` on an object of unknown class (by attribute), so effect boundary `checks` cannot tell what it does there"
    );
    assert_eq!(
        message(at("setattr(thing")),
        "`writes_named` writes an attribute named at run time on an object of unknown class (with `setattr`), so effect boundary `checks` cannot tell what it does there"
    );
    assert_eq!(
        message(at("helpers.evaluated")),
        "`evaluates` reaches a body heki does not model (via `pkg.helpers.evaluated`), so effect boundary `checks` cannot tell what it does there"
    );
    assert_eq!(
        message(at("stubbed.go")),
        "`through_stub` reaches a body only a stub declares (via `pkg.stubbed.go`), so effect boundary `checks` cannot tell what it does there"
    );
    assert_eq!(
        message(at("make(field)")),
        "`through_either` reaches a call heki cannot resolve (`make`), so effect boundary `checks` cannot tell what it does there"
    );
    assert_eq!(reported(&p, "pkg/lenient.py", &["EFF201"]).len(), 1);
}

#[test]
fn an_unknown_unresolved_value_is_a_configuration_error() {
    let p = boundary_project("unattributed_unknown_value");
    p.write(
        "pyproject.toml",
        &BOUNDARIES.replace("unresolved = \"report\"", "unresolved = \"maybe\""),
    );
    let (code, stdout, stderr) = p.check(&[]);
    assert_eq!(code, 2, "{stdout}{stderr}");
    assert!(
        stderr.contains(
            "unknown `unresolved` value `maybe` in effect-boundaries `checks` (expected `ignore` or `report`)"
        ),
        "{stderr}"
    );
}

const CONSTRUCTION: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "builds"
scopes = ["pkg.builds.*"]
write-universe = ["Record.value", "Filled.value", "Assigned.value"]
allow-writes = []
unresolved = "report"
"#;

const RECORDS: &str = r#"class Record:
    def __init__(self, name: str) -> None:
        setattr(self, name, 0)


def fill(obj, name: str) -> None:
    setattr(obj, name, 0)


def assign(obj) -> None:
    obj.value = 0


class Filled:
    def __init__(self, name: str) -> None:
        fill(self, name)


class Assigned:
    def __init__(self) -> None:
        assign(self)


class Copier:
    def __init__(self, source: Record, name: str) -> None:
        setattr(source, name, 0)
"#;

/// Constructing objects of the target classes, writes naming an attribute at run time on an
/// object the function constructs (`fresh-objects = "allow"`, the default), and writes that stay
/// unknown.
const BUILDS: &str = r#"from pkg.records import Assigned, Copier, Filled, Record, assign, fill


def builds(name: str) -> Record:
    return Record(name)


def builds_filled(name: str) -> Filled:
    return Filled(name)


def builds_assigned() -> Assigned:
    return Assigned()


def copies(record: Record, name: str) -> Copier:
    return Copier(record, name)  # EFF201


def renames(record: Record, name: str) -> None:
    setattr(record, name, 1)  # EFF201


def refills(record: Filled, name: str) -> None:
    fill(record, name)  # EFF201


def reassigns(thing) -> None:
    assign(thing)  # EFF201


def names_on_a_new_record(name: str) -> Record:
    record = Record(name)
    setattr(record, name, 1)
    return record
"#;

#[test]
fn a_class_initializing_its_own_instance_is_no_write_the_boundary_cannot_judge() {
    let p = Project::new("unattributed_construction");
    p.write("pyproject.toml", CONSTRUCTION);
    p.write("pkg/__init__.py", "");
    p.write("pkg/records.py", RECORDS);
    p.write("pkg/builds.py", BUILDS);
    let found = reported(&p, "pkg/builds.py", &["EFF201"]);
    let lines: Vec<(u64, String)> = found.iter().map(|(l, r, _)| (*l, r.clone())).collect();
    assert_eq!(lines, marked(BUILDS, &["EFF201"]), "{found:#?}");
}

const OWNERS: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT201", "MUT204"]

[[tool.heki.call-owners]]
name = "settles"
targets = ["Store.require"]
allowed-callers = ["pkg.passes.settle*"]
unresolved = "report"

[[tool.heki.call-owners]]
name = "lenient"
targets = ["Store.release"]
allowed-callers = ["pkg.passes.settle*"]

[[tool.heki.write-owners]]
name = "marks"
targets = ["Field.marked"]
allowed-writers = ["pkg.passes.settle*"]
unresolved = "report"
"#;

const STORE: &str = r#"from pkg.model import Field


class Store:
    def require(self, field: Field) -> None:
        field.required = True

    def release(self, field: Field) -> None:
        field.required = False
"#;

/// Private helpers the allowed `settle_*` functions call: one only called, the others also handed
/// on as values (a module-level display, a class attribute, a return, an argument). A contract
/// without the key keeps allowing them.
const PASSES: &str = r#"from typing import ClassVar

from pkg.model import Field
from pkg.store import Store


def _called(store: Store, field: Field) -> None:
    store.require(field)


def _in_table(store: Store, field: Field) -> None:
    store.require(field)  # MUT204
    store.release(field)


def _in_class(store: Store, field: Field) -> None:
    store.require(field)  # MUT204


def _returned(store: Store, field: Field) -> None:
    store.require(field)  # MUT204


def _handed(store: Store, field: Field) -> None:
    store.require(field)  # MUT204


def _marks(field: Field) -> None:
    field.marked = True  # MUT201


TABLE = {"require": _in_table}
MARKS = [_marks]


class Holder:
    RUN: ClassVar = staticmethod(_in_class)


def make():
    return _returned


def register(run) -> None:
    pass


def settle_all(store: Store, field: Field) -> None:
    _called(store, field)
    _in_table(store, field)
    _in_class(store, field)
    _returned(store, field)
    _handed(store, field)
    _marks(field)
    register(_handed)
"#;

#[test]
fn a_function_handed_on_as_a_value_judges_what_it_does_itself() {
    let p = Project::new("unattributed_owners");
    p.write("pyproject.toml", OWNERS);
    p.write("pkg/__init__.py", "");
    p.write(
        "pkg/model.py",
        &MODEL.replace(
            "self.required = False",
            "self.required = False\n        self.marked = False",
        ),
    );
    p.write("pkg/store.py", STORE);
    p.write("pkg/passes.py", PASSES);
    let found = reported(&p, "pkg/passes.py", &["MUT201", "MUT204"]);
    let lines: Vec<(u64, String)> = found.iter().map(|(l, r, _)| (*l, r.clone())).collect();
    let mut expected = marked(PASSES, &["MUT201", "MUT204"]);
    expected.sort();
    assert_eq!(lines, expected, "{found:#?}");
}

/// The reproduction of the request: a helper the allowed `settle` calls, also held by a table
/// and a `ClassVar`, which the forbidden checks call. The checks' writes are reported (the
/// calls resolve through the table and the class attribute), and the helper, handed on as a
/// value, judges its call of `Store.require` itself.
const REQUEST_CONFIG: &str = r#"[project]
name = "h14"
version = "0"

[tool.heki.lint]
select = ["MUT204", "MUT207", "EFF201"]

[[tool.heki.call-owners]]
name = "only-settle-requires"
targets = ["Store.require"]
allowed-callers = ["pkg.passes.settle"]
references = "calls"
unresolved = "report"

[[tool.heki.effect-boundaries]]
name = "checks-write-nothing"
scopes = ["pkg.passes.check_*"]
write-universe = ["Field.required"]
allow-writes = []
unresolved = "report"
"#;

const REQUEST_MODEL: &str = r#"from __future__ import annotations


class Field:
    def __init__(self) -> None:
        self.required = False
"#;

const REQUEST_STORE: &str = r#"from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from pkg.model import Field


class Store:
    def require(self, field: Field) -> None:
        field.required = True
"#;

const REQUEST_PASSES: &str = r#""""One pass may make a field required (``settle``); the others may not, whatever call shape they use."""

from __future__ import annotations

from typing import TYPE_CHECKING, ClassVar

if TYPE_CHECKING:
    from collections.abc import Callable

    from pkg.model import Field
    from pkg.store import Store


def _require(store: Store, field: Field) -> None:
    store.require(field)  # MUT204


TABLE: dict[str, Callable[[Store, Field], None]] = {"require": _require}


class Holder:
    REQUIRE: ClassVar[Callable[[Store, Field], None]] = staticmethod(_require)


def settle(store: Store, field: Field) -> None:
    _require(store, field)


def check_direct(store: Store, field: Field) -> None:
    store.require(field)  # EFF201 MUT204


def check_through_table(store: Store, field: Field) -> None:
    TABLE["require"](store, field)  # EFF201


def check_through_classvar(store: Store, field: Field) -> None:
    Holder.REQUIRE(store, field)  # EFF201
"#;

#[test]
fn every_check_of_the_request_is_reported() {
    let p = Project::new("unattributed_request");
    p.write("pyproject.toml", REQUEST_CONFIG);
    p.write("pkg/__init__.py", "");
    p.write("pkg/model.py", REQUEST_MODEL);
    p.write("pkg/store.py", REQUEST_STORE);
    p.write("pkg/passes.py", REQUEST_PASSES);
    let rules = ["EFF201", "MUT204"];
    let found = reported(&p, "pkg/passes.py", &rules);
    let lines: Vec<(u64, String)> = found.iter().map(|(l, r, _)| (*l, r.clone())).collect();
    let mut expected: Vec<(u64, String)> = REQUEST_PASSES
        .lines()
        .enumerate()
        .flat_map(|(i, l)| {
            let marks = l.split_once("  # ").map_or("", |(_, m)| m);
            marks
                .split(' ')
                .filter(|m| rules.contains(m))
                .map(move |m| (i as u64 + 1, m.to_owned()))
        })
        .collect();
    expected.sort();
    assert_eq!(lines, expected, "{found:#?}");
}

const LISTED_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "no-required"
scopes = ["pkg.m.run", "pkg.m.single", "pkg.m.writes", "pkg.m.sets", "pkg.m.evaluates"]
forbid = ["writes-targets"]
write-targets = ["Field.required"]
unresolved = "report"
"#;

const LISTED_M: &str = r#"from pkg import helpers, stubbed


class Field:
    def __init__(self) -> None:
        self.required = False


def run(text, table, fn):
    a = text.strip()
    b = table.get("k")
    fn(a)
    helpers.call(fn, b)
    helpers.evaluated("1")
    stubbed.go()
    return b


def single(handler):
    handler()


def writes(thing, other, name: str) -> None:
    thing.required = True
    other.required = False
    setattr(thing, name, True)


def sets(field: Field) -> None:
    field.required = True


def evaluates(text):
    return eval(text)
"#;

const LISTED_HELPERS: &str = r#"def call(run, value):
    run(value)
    value["x"](value)


def evaluated(text):
    return eval(text)
"#;

fn listed_project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("pyproject.toml", LISTED_CONFIG);
    p.write("pkg/__init__.py", "");
    p.write("pkg/m.py", LISTED_M);
    p.write("pkg/helpers.py", LISTED_HELPERS);
    p.write("pkg/stubbed.pyi", "def go() -> None: ...\n");
    p
}

/// The JSON diagnostics of `p`, by function and target.
fn by_report(p: &Project) -> BTreeMap<(String, String), Value> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let key = |d: &Value| {
        let text = |field: &str| d[field].as_str().unwrap().to_owned();
        (text("qualname"), text("target"))
    };
    diags.into_iter().map(|d| (key(&d), d)).collect()
}

/// A call site of `unresolved_sites`.
fn call(path: &str, line: u32, column: u32, function: &str, callee: Value, reason: &str) -> Value {
    json!({
        "path": path,
        "line": line,
        "column": column,
        "function": function,
        "callee": callee,
        "reason": reason,
        "parameter": null,
    })
}

#[test]
fn a_report_of_what_a_boundary_cannot_judge_lists_every_site_in_the_json_output() {
    let p = listed_project("unattributed_listed_sites");
    let found = by_report(&p);
    let report = |function: &str, target: &str| &found[&(function.to_owned(), target.to_owned())];
    let with_parameter = |mut site: Value, parameter: &str| {
        site["parameter"] = json!(parameter);
        site
    };
    // Ordered by path, line, and column: the calls of the helper the scope function reaches
    // (its own calls, not the code behind them), then the scope function's, with the bodies
    // heki does not model or only a stub declares at the calls reaching them.
    let calls = report("run", "no-required:unresolved-calls");
    assert_eq!(calls["line"], 10);
    assert_eq!(calls["sites"], 7);
    let helper = |line, column, callee: Value, reason| {
        call(
            "pkg/helpers.py",
            line,
            column,
            "pkg.helpers.call",
            callee,
            reason,
        )
    };
    let own = |line, column, callee: &str, reason| {
        call("pkg/m.py", line, column, "pkg.m.run", json!(callee), reason)
    };
    let parameter = "unannotated-parameter";
    assert_eq!(
        calls["unresolved_sites"],
        json!([
            with_parameter(helper(2, 5, json!("run"), parameter), "run"),
            helper(3, 5, Value::Null, "untyped-value"),
            with_parameter(own(10, 9, "strip", parameter), "text"),
            with_parameter(own(11, 9, "get", parameter), "table"),
            with_parameter(own(12, 5, "fn", parameter), "fn"),
            own(14, 5, "pkg.helpers.evaluated", "unmodeled-body"),
            own(15, 5, "pkg.stubbed.go", "stub-body"),
        ])
    );
    // A writes report lists the attribute and the kind of each write; a name computed at run
    // time has no attribute.
    let write = |line, attribute: Value, kind| {
        json!({
            "path": "pkg/m.py",
            "line": line,
            "column": 5,
            "function": "pkg.m.writes",
            "attribute": attribute,
            "kind": kind,
        })
    };
    let required = report("writes", "no-required:unresolved-writes:required");
    assert_eq!(required["sites"], 2);
    assert_eq!(
        required["unresolved_sites"],
        json!([
            write(24, json!("required"), "direct_attr"),
            write(25, json!("required"), "direct_attr"),
        ])
    );
    let named = report("writes", "no-required:unresolved-writes:*");
    assert_eq!(
        named["unresolved_sites"],
        json!([write(26, Value::Null, "set_attr")])
    );
    // A report of one site lists it too, where `sites` is not written.
    let single = report("single", "no-required:unresolved-calls");
    assert!(single.get("sites").is_none(), "{single:#}");
    assert_eq!(
        single["unresolved_sites"],
        json!([with_parameter(
            call(
                "pkg/m.py",
                20,
                5,
                "pkg.m.single",
                json!("handler"),
                parameter
            ),
            "handler"
        )])
    );
    // A scope function whose own body heki does not model is listed at its definition.
    let own_body = report("evaluates", "no-required:unresolved-calls");
    assert_eq!(
        own_body["unresolved_sites"],
        json!([call(
            "pkg/m.py",
            33,
            1,
            "pkg.m.evaluates",
            json!("pkg.m.evaluates"),
            "unmodeled-body"
        )])
    );
    // The other reports list nothing.
    let other = report("sets", "no-required:Field.required");
    assert!(other.get("unresolved_sites").is_none(), "{other:#}");
    assert_eq!(found.len(), 6, "{found:#?}");
}

#[test]
fn a_baseline_keys_a_report_listing_its_sites_by_its_target_and_counts_the_sites() {
    let p = listed_project("unattributed_listed_baseline");
    let (code, stdout, stderr) = p.check(&["--baseline", "baseline.toml", "--update-baseline"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    let baseline: toml::Table = toml::from_str(&p.read("baseline.toml")).unwrap();
    // The key holds no site, and the count is the number of sites the report lists.
    let entries: Vec<(String, String, i64, Vec<String>)> = baseline["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            let e = e.as_table().unwrap();
            let text = |field: &str| e[field].as_str().unwrap().to_owned();
            let fields = e.keys().cloned().collect();
            (
                text("qualname"),
                text("target"),
                e["count"].as_integer().unwrap(),
                fields,
            )
        })
        .collect();
    let fields: Vec<String> = [
        "count",
        "heuristic",
        "path",
        "qualname",
        "rule_code",
        "target",
    ]
    .map(str::to_owned)
    .to_vec();
    let entry = |qualname: &str, target: &str, count| {
        (
            qualname.to_owned(),
            format!("no-required:{target}"),
            count,
            fields.clone(),
        )
    };
    assert_eq!(
        entries,
        vec![
            entry("evaluates", "unresolved-calls", 1),
            entry("run", "unresolved-calls", 7),
            entry("sets", "Field.required", 1),
            entry("single", "unresolved-calls", 1),
            entry("writes", "unresolved-writes:*", 1),
            entry("writes", "unresolved-writes:required", 2),
        ]
    );
    let (code, stdout, stderr) = p.check(&["--baseline", "baseline.toml"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(!stdout.contains("EFF201"), "{stdout}");
}
