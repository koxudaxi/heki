//! Calls through the values a project binds by name, end to end through the real binary: a
//! module-level dict of functions bound once (by a key the code fixes, or any key, which may be
//! any of its values), a class attribute bound once to a function, a `staticmethod` or a class
//! (through the class, `cls`, or an instance), a function returning a module it imports (a
//! local import breaking a cycle, with a `functools` cache), and a value typed by a `TypeVar`
//! with a bound or constraints. A dict or list something changes in place keeps no items.
//!
//! Every fixture marks the lines that must be reported with the rule code in a comment
//! (`# EFF201`); every other line must not be.

mod common;

use common::Project;
use serde_json::Value;

const CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "passes-write-nothing"
scopes = ["pkg.passes.*", "pkg.passes.*.*"]
write-universe = ["Field.required"]
allow-writes = []
"#;

const MODEL: &str = r#"class Field:
    def __init__(self) -> None:
        self.required = False
"#;

/// `(path, line)` of every `EFF201` report.
fn reported(p: &Project) -> Vec<(String, u64)> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<(String, u64)> = diags
        .iter()
        .filter(|d| d["rule_code"] == "EFF201")
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
            )
        })
        .collect();
    out.sort();
    out
}

/// The `(path, line)` of every line of `files` ending in `# EFF201`.
fn marked(files: &[(&str, &str)]) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    for (path, text) in files {
        for (i, line) in text.lines().enumerate() {
            if line.trim_end().ends_with("# EFF201") {
                out.push(((*path).to_owned(), i as u64 + 1));
            }
        }
    }
    out.sort();
    out
}

fn project(name: &str, files: &[(&str, &str)]) -> Project {
    let p = Project::new(name);
    p.write("pyproject.toml", CONFIG);
    p.write("pkg/__init__.py", "");
    p.write("pkg/model.py", MODEL);
    for (path, text) in files {
        p.write(path, text);
    }
    p
}

fn check_marks(name: &str, files: &[(&str, &str)]) {
    let p = project(name, files);
    assert_eq!(reported(&p), marked(files));
}

/// The purity verdict `heki inspect` gives the function `symbol`.
fn verdict(p: &Project, symbol: &str) -> String {
    let (code, stdout, stderr) = p.heki(&["inspect", symbol, "--output-format", "json"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    let v: Value = serde_json::from_str(&stdout).unwrap();
    v["purity"]["verdict"].as_str().unwrap().to_owned()
}

const TABLES: &str = r#"from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from collections.abc import Callable

    from pkg.model import Field

KEY = "require"


def _noop(field: Field) -> None:
    pass


def _require(field: Field) -> None:
    field.required = True


TABLE: dict[str, Callable[[Field], None]] = {"require": _require, "noop": _noop}
LAMBDAS = {"other": lambda field: None, "require": _require}
LAST = {"require": _noop, "require": _require}
"#;

/// A constant key gives its own value; any other key, iterating the values, or a key a constant
/// names one of them; an entry bound again by a later one of the same key is the later one.
const TABLE_CALLS: &str = r#"from __future__ import annotations

from typing import TYPE_CHECKING

from pkg import tables
from pkg.tables import KEY, LAMBDAS, LAST, TABLE

if TYPE_CHECKING:
    from pkg.model import Field


def by_key(field: Field) -> None:
    TABLE["require"](field)  # EFF201


def by_other_key(field: Field) -> None:
    TABLE["noop"](field)


def by_any_key(field: Field, kind: str) -> None:
    TABLE[kind](field)  # EFF201


def by_constant(field: Field) -> None:
    TABLE[KEY](field)  # EFF201


def through_module(field: Field) -> None:
    tables.TABLE["require"](field)  # EFF201


def through_values(field: Field) -> None:
    for run in TABLE.values():
        run(field)  # EFF201


def beside_a_lambda(field: Field) -> None:
    LAMBDAS["require"](field)  # EFF201


def the_lambda(field: Field) -> None:
    LAMBDAS["other"](field)


def later_entry(field: Field) -> None:
    LAST["require"](field)  # EFF201
"#;

#[test]
fn a_table_of_functions_bound_once_gives_the_value_of_each_key() {
    check_marks(
        "dispatch_tables",
        &[("pkg/tables.py", TABLES), ("pkg/passes.py", TABLE_CALLS)],
    );
}

/// Something changing a table in place (an item store, `update`, through an import of it or in
/// another module) leaves its items unknown: no key gives a value, and the function calling
/// one is not pure. An annotated global keeps the items it declares.
const CHANGED: &str = r#"from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from pkg.model import Field


def _noop(field: Field) -> None:
    pass


HANDLERS = {"noop": _noop}
UPDATED = {"noop": _noop}
ELSEWHERE = {"noop": _noop}
ITEMS = [_noop]
KEPT = [_noop]


class Slot:
    def end(self, field: Field) -> None:
        field.required = True


SLOTS: dict[str, Slot] = {}
SHARED_SLOTS: dict[str, Slot] = {}


def open_slot(key: str) -> None:
    SLOTS[key] = Slot()


def register(name: str, run) -> None:
    HANDLERS[name] = run


def update(more: dict) -> None:
    UPDATED.update(more)


def add(run) -> None:
    ITEMS.append(run)


class Registry:
    def __init__(self) -> None:
        self.handlers = {"noop": _noop}

    def register(self, name: str, run) -> None:
        self.handlers[name] = run

    def run(self, field: Field, kind: str) -> None:
        self.handlers[kind](field)
"#;

const CHANGED_CALLS: &str = r#"from __future__ import annotations

from typing import TYPE_CHECKING

from pkg import changed
from pkg.changed import ELSEWHERE, HANDLERS, ITEMS, KEPT, SHARED_SLOTS, SLOTS, UPDATED, Slot

if TYPE_CHECKING:
    from pkg.model import Field


def changes_elsewhere(name: str, run) -> None:
    ELSEWHERE[name] = run
    SHARED_SLOTS[name] = Slot()


def by_handler(field: Field) -> None:
    HANDLERS["noop"](field)


def by_updated(field: Field) -> None:
    UPDATED["noop"](field)


def by_elsewhere(field: Field) -> None:
    changed.ELSEWHERE["noop"](field)


def by_item(field: Field) -> None:
    for run in ITEMS:
        run(field)


def by_kept(field: Field) -> None:
    for run in KEPT:
        run(field)


def by_annotated(field: Field, key: str) -> None:
    SLOTS[key].end(field)  # EFF201


def by_annotated_elsewhere(field: Field, key: str) -> None:
    SHARED_SLOTS[key].end(field)  # EFF201
"#;

#[test]
fn a_table_something_changes_in_place_keeps_no_items() {
    let p = project(
        "dispatch_tables_changed",
        &[
            ("pkg/changed.py", CHANGED),
            ("pkg/passes.py", CHANGED_CALLS),
        ],
    );
    assert_eq!(reported(&p), marked(&[("pkg/passes.py", CHANGED_CALLS)]));
    for f in ["by_handler", "by_updated", "by_elsewhere", "by_item"] {
        assert_eq!(verdict(&p, &format!("pkg.passes.{f}")), "unknown", "{f}");
    }
    assert_eq!(verdict(&p, "pkg.changed.Registry.run"), "unknown");
    assert_eq!(verdict(&p, "pkg.passes.by_kept"), "pure");
}

/// A class attribute bound once to a function, a `staticmethod` of one, or bound in a base:
/// through the class, `cls`, and an instance (a plain function binds the instance first). A
/// subclass binding the name to a method of its own, or an instance store of the name, leave
/// the call to what the value may be.
const HELPERS: &str = r#"from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from pkg.model import Field


def _require(field: Field) -> None:
    field.required = True


def _set(holder: object, field: Field) -> None:
    field.required = True
"#;

const CLASS_ALIASES: &str = r#"from __future__ import annotations

from typing import TYPE_CHECKING, ClassVar

from pkg.helpers import _require, _set

if TYPE_CHECKING:
    from collections.abc import Callable

    from pkg.model import Field


class Holder:
    REQUIRE: ClassVar[Callable[[Field], None]] = staticmethod(_require)
    SET = _set

    def through_self(self, field: Field) -> None:
        self.REQUIRE(field)  # EFF201

    def bound(self, field: Field) -> None:
        self.SET(field)  # EFF201

    @classmethod
    def through_cls(cls, field: Field) -> None:
        cls.REQUIRE(field)  # EFF201


class Child(Holder):
    pass


class Overriding:
    REQUIRE = staticmethod(_require)


class Override(Overriding):
    def REQUIRE(self, field: Field) -> None:
        pass


class Shadowed:
    REQUIRE = staticmethod(_require)

    def __init__(self, run) -> None:
        self.REQUIRE = run


def through_the_class(field: Field) -> None:
    Holder.REQUIRE(field)  # EFF201


def plain_through_the_class(holder: Holder, field: Field) -> None:
    Holder.SET(holder, field)  # EFF201


def inherited(field: Field) -> None:
    Child.REQUIRE(field)  # EFF201


def held(field: Field) -> None:
    run = Holder.REQUIRE
    run(field)  # EFF201


def overridden(o: Overriding, field: Field) -> None:
    o.REQUIRE(field)


def shadowed(s: Shadowed, field: Field) -> None:
    s.REQUIRE(field)
"#;

#[test]
fn a_class_attribute_bound_to_a_function_is_that_function() {
    check_marks(
        "dispatch_class_aliases",
        &[
            ("pkg/helpers.py", HELPERS),
            ("pkg/passes.py", CLASS_ALIASES),
        ],
    );
}

/// A function every return of which gives a module one of its imports binds (a local import
/// breaking an import cycle, a `functools` cache around it) returns that module; a function
/// returning one of two modules, typed `ModuleType`, says nothing of the module's functions.
const OPS: &str = r#"from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from pkg.model import Field


def require(field: Field) -> None:
    field.required = True
"#;

const MODULES: &str = r#"from __future__ import annotations

import functools
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from types import ModuleType

    from pkg.model import Field


def _ops() -> ModuleType:
    from pkg import ops

    return ops


@functools.cache
def _cached() -> ModuleType:
    from pkg import ops

    return ops


def _dotted():
    import pkg.ops

    return pkg.ops


def _either(flag: bool) -> ModuleType:
    if flag:
        from pkg import ops

        return ops
    from pkg import model

    return model


def through_module(field: Field) -> None:
    _ops().require(field)  # EFF201


def through_cache(field: Field) -> None:
    _cached().require(field)  # EFF201


def through_dotted(field: Field) -> None:
    _dotted().require(field)  # EFF201


def through_either(field: Field, flag: bool) -> None:
    _either(flag).require(field)
"#;

#[test]
fn a_function_returning_an_imported_module_returns_that_module() {
    let files = [("pkg/ops.py", OPS), ("pkg/passes.py", MODULES)];
    check_marks("dispatch_modules", &files);
    let p = project("dispatch_modules_unknown", &files);
    assert_eq!(verdict(&p, "pkg.passes.through_either"), "unknown");
}

/// A value typed by a `TypeVar` with a bound (a name, a string, a variable of another module)
/// or constraints is typed by them; an unbounded one by nothing.
const TYPE_VARS: &str = r#"from __future__ import annotations

from typing import TYPE_CHECKING, Generic, Protocol, TypeVar

import typing_extensions

if TYPE_CHECKING:
    from pkg.model import Field


class View(Protocol):
    @property
    def fields(self) -> list[Field]: ...


class Left:
    def touch(self, field: Field) -> None:
        field.required = True


class Right:
    def touch(self, field: Field) -> None:
        pass


V = TypeVar("V", bound=View)
Quoted = typing_extensions.TypeVar("Quoted", bound="View")
Either = TypeVar("Either", Left, Right)
Free = TypeVar("Free")


class Run(Generic[V]):
    def __init__(self, views: list[V]) -> None:
        self.views = views
"#;

const TYPE_VAR_USES: &str = r#"from __future__ import annotations

from typing import TYPE_CHECKING

from pkg.views import Either, Free, Quoted, Run, V

if TYPE_CHECKING:
    from pkg.model import Field


def bound(view: V) -> None:
    for field in view.fields:
        field.required = True  # EFF201


def quoted(view: Quoted) -> None:
    for field in view.fields:
        field.required = True  # EFF201


def items(run: Run[V]) -> None:
    for view in run.views:
        for field in view.fields:
            field.required = True  # EFF201


def constrained(value: Either, field: Field) -> None:
    value.touch(field)  # EFF201


def free(value: Free) -> None:
    for field in value.fields:
        field.required = True
"#;

#[test]
fn a_value_typed_by_a_bound_type_variable_is_typed_by_the_bound() {
    check_marks(
        "dispatch_type_vars",
        &[
            ("pkg/views.py", TYPE_VARS),
            ("pkg/passes.py", TYPE_VAR_USES),
        ],
    );
}

/// The reproduction of the request (five passes writing `Field.required`, directly and through
/// a table, a `ClassVar` `staticmethod`, a lazily imported module, and a bound `TypeVar`): every
/// pass is reported.
const REQUEST_CONFIG: &str = r#"[project]
name = "h9"
version = "0"

[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "passes-write-nothing"
scopes = ["pkg.passes.direct", "pkg.passes.through_*"]
write-universe = ["Field.required"]
allow-writes = []
"#;

const REQUEST_MODEL: &str = r#"from __future__ import annotations


class Field:
    def __init__(self) -> None:
        self.required = False
"#;

const REQUEST_OPS: &str = r#"from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from pkg.model import Field


def require(field: Field) -> None:
    field.required = True
"#;

const REQUEST_PASSES: &str = r#""""Five passes that write Field.required: directly, and through four call shapes."""

from __future__ import annotations

from typing import TYPE_CHECKING, ClassVar, Generic, Protocol, TypeVar

if TYPE_CHECKING:
    from collections.abc import Callable
    from types import ModuleType

    from pkg.model import Field


def _require(field: Field) -> None:
    field.required = True


TABLE: dict[str, Callable[[Field], None]] = {"require": _require}


class Holder:
    REQUIRE: ClassVar[Callable[[Field], None]] = staticmethod(_require)


def _ops() -> ModuleType:
    from pkg import ops

    return ops


class ContextView(Protocol):
    @property
    def fields(self) -> list[Field]: ...


C = TypeVar("C", bound=ContextView)


class Run(Generic[C]):
    def __init__(self, contexts: list[C]) -> None:
        self.contexts = contexts


def direct(field: Field) -> None:
    field.required = True  # EFF201


def through_table(field: Field) -> None:
    TABLE["require"](field)  # EFF201


def through_classvar(field: Field) -> None:
    Holder.REQUIRE(field)  # EFF201


def through_module(field: Field) -> None:
    _ops().require(field)  # EFF201


def through_generic(run: Run[C]) -> None:
    for context in run.contexts:
        for field in context.fields:
            field.required = True  # EFF201
"#;

#[test]
fn every_pass_of_the_request_is_reported() {
    let p = Project::new("dispatch_request");
    p.write("pyproject.toml", REQUEST_CONFIG);
    p.write("pkg/__init__.py", "");
    p.write("pkg/model.py", REQUEST_MODEL);
    p.write("pkg/ops.py", REQUEST_OPS);
    p.write("pkg/passes.py", REQUEST_PASSES);
    assert_eq!(reported(&p), marked(&[("pkg/passes.py", REQUEST_PASSES)]));
}

/// A class attribute bound to a class constructs it when called through the class, `cls`, or
/// an instance, as a module-level alias does (`call-owners` with `references = "reads"`).
const CONSTRUCTIONS_CONFIG: &str = r#"[project]
name = "h8"
version = "0"
requires-python = ">=3.11"

[tool.heki.lint]
select = ["MUT204"]

[[tool.heki.call-owners]]
name = "home:core.py:ClassScope"
targets = ["pkg.core.ClassScope"]
allowed-callers = ["pkg.core:*", "pkg.core:<module>"]
references = "reads"
attribution = "lexical"
"#;

const CONSTRUCTIONS_CORE: &str = r#"class ClassScope:
    def __init__(self, name: str) -> None:
        self.name = name
"#;

const CONSTRUCTIONS: &str = r#"from pkg.core import ClassScope

Alias = ClassScope


class Parser:
    factory = ClassScope

    def through_self(self) -> object:
        return self.factory("a")  # MUT204

    @classmethod
    def through_cls(cls) -> object:
        return cls.factory("b")  # MUT204


def through_the_class() -> object:
    return Parser.factory("c")  # MUT204


def through_module_alias() -> object:
    return Alias("d")  # MUT204
"#;

#[test]
fn a_class_attribute_bound_to_a_class_constructs_it() {
    let p = Project::new("dispatch_constructions");
    p.write("pyproject.toml", CONSTRUCTIONS_CONFIG);
    p.write("pkg/__init__.py", "");
    p.write("pkg/core.py", CONSTRUCTIONS_CORE);
    p.write("pkg/parser.py", CONSTRUCTIONS);
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut lines: Vec<u64> = diags
        .iter()
        .filter(|d| d["rule_code"] == "MUT204")
        .map(|d| d["line"].as_u64().unwrap())
        .collect();
    lines.sort_unstable();
    let marked: Vec<u64> = CONSTRUCTIONS
        .lines()
        .enumerate()
        .filter(|(_, l)| l.ends_with("# MUT204"))
        .map(|(i, _)| i as u64 + 1)
        .collect();
    assert_eq!(lines, marked);
}
