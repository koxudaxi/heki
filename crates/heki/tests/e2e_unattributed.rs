//! What a contract cannot attribute, end to end through the real binary: with
//! `unresolved = "report"`, an effect boundary reports the calls heki cannot resolve, the bodies
//! it does not model, the writes of an attribute it names on an object of unknown class, and the
//! reads of one on such an object or under a name heki cannot read in its scopes' reach, each
//! with its chain; an owner contract judges a function some code hands
//! on as a value (a display, a class attribute, a return, an argument) in that function, as if
//! it had no callers. Without the key, neither is reported.
//!
//! Every fixture marks the lines that must be reported with the rule code in a comment
//! (`# EFF201`); every other line must not be.

mod common;

use common::Project;
use serde_json::Value;

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

const READS: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "reads"
scopes = ["pkg.reads.*"]
read-universe = ["Config.*"]
unresolved = "report"

[[tool.heki.effect-boundaries]]
name = "targets"
scopes = ["pkg.targets.*"]
forbid = ["reads-targets"]
read-targets = ["Config.debug"]
unresolved = "report"

[[tool.heki.effect-boundaries]]
name = "lenient"
scopes = ["pkg.lenient.*"]
read-universe = ["Config.*"]
"#;

const CONFIG_MODEL: &str = r#"class Config:
    def __init__(self, **values) -> None:
        self.debug = False
        for key in values:
            self._load(key)

    def _load(self, key: str) -> None:
        print(getattr(self, key))


class Point:
    def __init__(self) -> None:
        self.debug = 1


def read(thing, key: str) -> object:
    return getattr(thing, key)
"#;

/// The reads a boundary judging reads cannot judge, and the ones it can: a read of a name the
/// universe's class has on an object of unknown class, and a read under a name heki cannot read
/// on an object that may be a universe class or of unknown class, are reported; a name no
/// universe class has, an object of another class, an `in` test or iterating the keys of
/// `__dict__`, and a class's own initialization reading its instance are not.
const READ_SHAPES: &str = r#"from pkg.model import Config, Point, read


def unknown(other) -> object:
    return other.debug  # EFF201


def not_a_member(other) -> object:
    return other.colour


def computed(config: Config, key: str) -> object:
    return getattr(config, key)  # EFF201


def computed_unknown(other, key: str) -> bool:
    return hasattr(other, key)  # EFF201


def computed_other(point: Point, key: str) -> object:
    return getattr(point, key), vars(point), point.__dict__[key]


def item(config: Config, key: str) -> object:
    return config.__dict__[key]  # EFF201


def item_or_none(config: Config, key: str) -> object:
    return config.__dict__.get(key)  # EFF201


def whole(config: Config) -> object:
    return list(config.__dict__.items())  # EFF201


def keys_only(config: Config) -> object:
    return "debug" in vars(config), [k for k in config.__dict__]


def returned(config: Config) -> dict:
    return vars(config)  # EFF201


def through_helper(config: Config, key: str) -> object:
    return read(config, key)  # EFF201


def constructs() -> Config:
    return Config(debug=True)


def twice(config: Config, key: str) -> object:
    return None


def twice(config: Config, key: str) -> object:
    return getattr(config, key)  # EFF201
"#;

const TARGET_SHAPES: &str = r#"from pkg.model import Config


def unknown(other) -> object:
    return other.debug  # EFF201


def other_name(other) -> object:
    return other.name


def computed(config: Config, key: str) -> object:
    return getattr(config, key)  # EFF201
"#;

#[test]
fn a_boundary_judging_reads_reports_the_reads_it_cannot_judge() {
    let p = Project::new("unattributed_reads");
    p.write("pyproject.toml", READS);
    p.write("pkg/__init__.py", "");
    p.write("pkg/model.py", CONFIG_MODEL);
    p.write("pkg/reads.py", READ_SHAPES);
    p.write("pkg/targets.py", TARGET_SHAPES);
    p.write("pkg/lenient.py", &READ_SHAPES.replace("  # EFF201", ""));
    let found = reported(&p, "pkg/reads.py", &["EFF201"]);
    let lines: Vec<(u64, String)> = found.iter().map(|(l, r, _)| (*l, r.clone())).collect();
    assert_eq!(lines, marked(READ_SHAPES, &["EFF201"]), "{found:#?}");
    let message = |line: u64| &found.iter().find(|(l, _, _)| *l == line).unwrap().2;
    let at = |needle: &str| {
        READ_SHAPES
            .lines()
            .position(|l| l.contains(needle))
            .unwrap() as u64
            + 1
    };
    assert_eq!(
        message(at("other.debug")),
        "`unknown` reads `debug` on an object of unknown class (by attribute), so effect boundary `reads` cannot tell what it does there"
    );
    assert_eq!(
        message(at("getattr(config, key)")),
        "`computed` reads an attribute under a name heki cannot read (with `getattr`), so effect boundary `reads` cannot tell what it does there"
    );
    assert_eq!(
        message(at("read(config, key)")),
        "`through_helper` reads an attribute under a name heki cannot read (with `getattr` via `pkg.model.read`), so effect boundary `reads` cannot tell what it does there"
    );
    let found = reported(&p, "pkg/targets.py", &["EFF201"]);
    let lines: Vec<(u64, String)> = found.iter().map(|(l, r, _)| (*l, r.clone())).collect();
    assert_eq!(lines, marked(TARGET_SHAPES, &["EFF201"]), "{found:#?}");
    assert!(reported(&p, "pkg/lenient.py", &["EFF201"]).is_empty());
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
