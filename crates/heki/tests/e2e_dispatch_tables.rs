//! Calls through the values a project binds by name, end to end through the real binary: a
//! module-level dict of functions bound once (by a key the code fixes, or any key, which may be
//! any of its values), a class attribute bound once to a function of the project or of a
//! library, a `staticmethod` or a class (through the class, `cls`, or an instance), a function
//! or a method returning a module it imports (a local import breaking a cycle, with a
//! `functools` cache), a name bound to one of two modules, and a value typed by a `TypeVar` with
//! a bound or constraints. A table of `functools.partial` objects holding no positions calls
//! their callables, and what such a call returns is new when every callable returns a new
//! object. A dict or list something changes in place keeps the items of its own
//! and of what the changes add, or none when a change adds what heki cannot type. A
//! module-level function or a method its body binds again on some paths may be either object.
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

from typing import TYPE_CHECKING, TypeAlias, TypeVar

if TYPE_CHECKING:
    from collections.abc import Callable, Mapping

    from pkg.model import Field

KEY = "require"
Handler: TypeAlias = "Callable[[Field], None]"


def _noop(field: Field) -> None:
    pass


def _require(field: Field) -> None:
    field.required = True


TABLE: dict[str, Callable[[Field], None]] = {"require": _require, "noop": _noop}
LAMBDAS = {"other": lambda field: None, "require": _require}
LAST = {"require": _noop, "require": _require}
EXTRA = {"noop": _noop}
ALIASED: dict[str, Handler] = {"require": _require}
Opaque = TypeVar("Opaque")
OPAQUE: dict[str, Opaque] = {"require": _require}
MAPPED: Mapping[str, Callable[[Field], None]] = {"require": _require}
UNPACKING = {"require": _require, **EXTRA}
"#;

/// A constant key gives its own value; any other key, iterating the values, or a key a constant
/// names one of them; an entry bound again by a later one of the same key is the later one. An
/// annotation keeps the stored functions when its item type is a callable type, an alias of one,
/// or a `TypeVar` with no bound, and so does one of another mapping class; a dict with a `**`
/// entry may give any of its values.
const TABLE_CALLS: &str = r#"from __future__ import annotations

from typing import TYPE_CHECKING

from pkg import tables
from pkg.tables import ALIASED, KEY, LAMBDAS, LAST, MAPPED, OPAQUE, TABLE, UNPACKING

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


def through_an_aliased_item_type(field: Field) -> None:
    ALIASED["require"](field)  # EFF201


def through_an_opaque_item_type(field: Field) -> None:
    OPAQUE["require"](field)  # EFF201


def through_a_mapping(field: Field) -> None:
    MAPPED["require"](field)  # EFF201


def beside_an_unpacking(field: Field) -> None:
    UNPACKING["require"](field)  # EFF201

"#;

#[test]
fn a_table_of_functions_bound_once_gives_the_value_of_each_key() {
    check_marks(
        "dispatch_tables",
        &[("pkg/tables.py", TABLES), ("pkg/passes.py", TABLE_CALLS)],
    );
}

/// Something changing a table in place with values of unknown type (an item store, `update`,
/// through an import of it or in another module, through the module's own object) leaves its
/// items unknown: no key gives a value, and the function calling one is not pure. An annotated global keeps the items it
/// declares, also a tuple annotated as one of any length.
const CHANGED: &str = r#"from __future__ import annotations

from typing import TYPE_CHECKING

import pkg.changed as this_module

if TYPE_CHECKING:
    from pkg.model import Field


def _noop(field: Field) -> None:
    pass


HANDLERS = {"noop": _noop}
UPDATED = {"noop": _noop}
ELSEWHERE = {"noop": _noop}
THROUGH_MODULE = {"noop": _noop}
THROUGH_IMPORT = {"noop": _noop}
ITEMS = [_noop]
KEPT = [_noop]


class Slot:
    def end(self, field: Field) -> None:
        field.required = True


SLOTS: dict[str, Slot] = {}
SHARED_SLOTS: dict[str, Slot] = {}
ORDERED_SLOTS: tuple[Slot, ...] = (Slot(), Slot())


def open_slot(key: str) -> None:
    SLOTS[key] = Slot()


def register(name: str, run) -> None:
    HANDLERS[name] = run


def update(more: dict) -> None:
    UPDATED.update(more)


def register_through_module(name: str, run) -> None:
    this_module.THROUGH_MODULE[name] = run


def register_through_import(name: str, run) -> None:
    from pkg import changed

    changed.THROUGH_IMPORT[name] = run


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
from pkg.changed import (
    ELSEWHERE,
    HANDLERS,
    ITEMS,
    KEPT,
    ORDERED_SLOTS,
    SHARED_SLOTS,
    SLOTS,
    THROUGH_IMPORT,
    THROUGH_MODULE,
    UPDATED,
    Slot,
)

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


def by_through_module(field: Field) -> None:
    THROUGH_MODULE["noop"](field)


def by_through_import(field: Field) -> None:
    THROUGH_IMPORT["noop"](field)


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


def by_annotated_tuple(field: Field) -> None:
    for slot in ORDERED_SLOTS:
        slot.end(field)  # EFF201
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
    for f in [
        "by_handler",
        "by_updated",
        "by_elsewhere",
        "by_through_module",
        "by_through_import",
        "by_item",
    ] {
        assert_eq!(verdict(&p, &format!("pkg.passes.{f}")), "unknown", "{f}");
    }
    assert_eq!(verdict(&p, "pkg.changed.Registry.run"), "unknown");
    assert_eq!(verdict(&p, "pkg.passes.by_kept"), "pure");
}

/// A class attribute bound once to a function, a `staticmethod` of one, or bound in a base:
/// through the class, `cls`, and an instance (a plain function binds the instance first). A
/// subclass binding the name to a method of its own, an instance store of the name (also under a
/// name computed at run time), a field annotation, a hook on attribute reads, another module
/// binding the name, or a class whose MRO binds no such name (a mixin calling what a subclass
/// binds) leave the call to what the value may be. Read on an instance, the attribute is what any
/// subclass binds it to.
const HELPERS: &str = r#"from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from pkg.model import Field


def _require(field: Field) -> None:
    field.required = True


def _set(holder: object, field: Field) -> None:
    field.required = True


def _skip(field: Field) -> None:
    pass
"#;

const CLASS_ALIASES: &str = r#"from __future__ import annotations

from dataclasses import dataclass
from typing import TYPE_CHECKING, ClassVar

from pkg.helpers import _require, _set, _skip

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


class Quiet:
    RUN = staticmethod(_skip)


class Loud(Quiet):
    RUN = staticmethod(_require)


def read_on_a_base(q: Quiet, field: Field) -> None:
    run = q.RUN
    run(field)  # EFF201


def read_overridden(o: Overriding, field: Field) -> None:
    run = o.REQUIRE
    run(field)


def shadowed(s: Shadowed, field: Field) -> None:
    s.REQUIRE(field)


class Mixin:
    def finish(self, field: Field) -> None:
        self.FINISH(field)


class Finished(Mixin):
    FINISH = staticmethod(_require)


@dataclass
class Settings:
    REQUIRE: Callable[[Field], None] = _require


class Loaded:
    REQUIRE = staticmethod(_require)

    def load(self, name: str, value: object) -> None:
        setattr(self, name, value)


class Hooked:
    REQUIRE = staticmethod(_require)

    def __getattribute__(self, name: str) -> object:
        return object.__getattribute__(self, name)


class Patched:
    REQUIRE = staticmethod(_require)


def annotated(s: Settings, field: Field) -> None:
    s.REQUIRE(field)


def loaded(o: Loaded, field: Field) -> None:
    o.REQUIRE(field)


def hooked(h: Hooked, field: Field) -> None:
    h.REQUIRE(field)


def patched(p: Patched, field: Field) -> None:
    p.REQUIRE(field)
"#;

const PATCH: &str = r#"from pkg.helpers import _skip
from pkg.passes import Patched

Patched.REQUIRE = staticmethod(_skip)
"#;

#[test]
fn a_class_attribute_bound_to_a_function_is_that_function() {
    check_marks(
        "dispatch_class_aliases",
        &[
            ("pkg/helpers.py", HELPERS),
            ("pkg/passes.py", CLASS_ALIASES),
            ("pkg/patch.py", PATCH),
        ],
    );
}

/// A function or a method every return of which gives a module one of its imports binds (a
/// local import breaking an import cycle, a `functools` cache around it) returns that module; a
/// function returning one of two modules, typed `ModuleType`, says nothing of the module's
/// functions. A name the function binds to one of two modules it imports calls the function of
/// either.
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


class Lazy:
    def ops(self) -> ModuleType:
        from pkg import ops

        return ops


def through_method(lazy: Lazy, field: Field) -> None:
    lazy.ops().require(field)  # EFF201


def through_chosen(field: Field, flag: bool) -> None:
    from pkg import checks, ops

    chosen = ops if flag else checks
    chosen.require(field)  # EFF201


def through_chosen_reader(field: Field, flag: bool) -> None:
    from pkg import checks, lint

    chosen = checks if flag else lint
    chosen.require(field)
"#;

const CHECKS: &str = r#"from __future__ import annotations

from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from pkg.model import Field


def require(field: Field) -> bool:
    return field.required
"#;

#[test]
fn a_function_returning_an_imported_module_returns_that_module() {
    let files = [
        ("pkg/ops.py", OPS),
        ("pkg/checks.py", CHECKS),
        ("pkg/lint.py", CHECKS),
        ("pkg/passes.py", MODULES),
    ];
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

/// A list attribute something changes in place keeps its class: its items are unknown, and a
/// slice of it is a list too (`OPT112` reads the class).
const BUFFER: &str = r#"class Writer:
    def __init__(self, flush_amount):
        self._items = []
        self._flush_amount = flush_amount

    def add(self, request):
        self._remove_duplicates(request)
        self._items.append(request)

    def _remove_duplicates(self, request):
        for item in self._items:
            if item == request:
                self._items.remove(item)  # OPT112

    def flush(self):
        sent = self._items[: self._flush_amount]
        self._items = self._items[self._flush_amount :]
        return sent
"#;

#[test]
fn a_list_something_changes_in_place_stays_a_list() {
    let p = Project::new("dispatch_changed_list");
    p.write("pkg/__init__.py", "");
    p.write("pkg/writer.py", BUFFER);
    let (code, stdout, stderr) = p.check(&["--select", "OPT112", "--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let lines: Vec<u64> = diags.iter().map(|d| d["line"].as_u64().unwrap()).collect();
    let marked: Vec<u64> = BUFFER
        .lines()
        .enumerate()
        .filter(|(_, l)| l.ends_with("# OPT112"))
        .map(|(i, _)| i as u64 + 1)
        .collect();
    assert_eq!(lines, marked);
}

/// Changes in place adding items of known types, in another module or in the module itself,
/// keep the items a list, a set, or a dict holds, joined with what they add: `append` of a
/// typed value, `extend` and `+=` of a list of known items, an item store of a typed value
/// (also of a slice), `add` to a set, and a function added to a table of functions (a call
/// through it runs either). An addition heki cannot type (an unannotated value, a `Callable`, a
/// list a `setdefault` gives that the code then appends to) leaves the items unknown.
const TYPED_MODEL: &str = r#"from __future__ import annotations

from typing import Callable

from pkg.model import Field


class Model:
    def __init__(self, fields: list[Field]) -> None:
        self.fields = self._validate(fields) if fields else []
        self.extended = self._validate(fields)
        self.grown = self._validate(fields)
        self.sliced = self._validate(fields)
        self.untyped = self._validate(fields)
        self.by_name = {"first": fields[0]}
        self.groups = {"all": self._validate(fields)}
        self.unique = set(self._validate(fields))
        self.own = self._validate(fields)

    def _validate(self, fields: list[Field]) -> list[Field]:
        return fields

    def add_own(self, field: Field) -> None:
        self.own.append(field)


def _noop(field: Field) -> None:
    pass


def _require(field: Field) -> None:
    field.required = True


HANDLERS = {"noop": _noop}
CALLABLES = {"noop": _noop}
RUNS = [_noop]


def register_require() -> None:
    HANDLERS["require"] = _require


def register_callable(name: str, run: Callable[[Field], None]) -> None:
    CALLABLES[name] = run


def add_require() -> None:
    global RUNS
    RUNS += [_require]
"#;

const TYPED_STORE: &str = r#"from __future__ import annotations

from pkg.model import Field
from pkg.typed import Model


def append_field(model: Model, field: Field) -> None:
    model.fields.append(field)


def extend_fields(model: Model, more: list[Field]) -> None:
    model.extended.extend(more)


def grow(model: Model, field: Field) -> None:
    model.grown += [field]


def splice(model: Model, more: list[Field]) -> None:
    model.sliced[1:2] = more


def append_any(model: Model, thing) -> None:
    model.untyped.append(thing)


def name_field(model: Model, name: str, field: Field) -> None:
    model.by_name[name] = field


def group(model: Model, key: str, thing) -> None:
    model.groups.setdefault(key, []).append(thing)


def add_unique(model: Model, field: Field) -> None:
    model.unique.add(field)
"#;

const TYPED_CALLS: &str = r#"from __future__ import annotations

from pkg.model import Field
from pkg.typed import CALLABLES, HANDLERS, RUNS, Model


def through_fields(models: list[Model]) -> None:
    for model in models:
        for field in model.fields:
            field.required = True  # EFF201


def through_one_model(model: Model) -> None:
    model.fields[0].required = True  # EFF201


def through_extended(model: Model) -> None:
    for field in model.extended:
        field.required = True  # EFF201


def through_grown(model: Model) -> None:
    for field in model.grown:
        field.required = True  # EFF201


def through_sliced(model: Model) -> None:
    for field in model.sliced:
        field.required = True  # EFF201


def through_untyped(model: Model) -> None:
    for field in model.untyped:
        field.required = True


def through_by_name(model: Model) -> None:
    model.by_name["a"].required = True  # EFF201


def through_groups(model: Model) -> None:
    for field in model.groups["all"]:
        field.required = True


def through_unique(model: Model) -> None:
    for field in model.unique:
        field.required = True  # EFF201


def through_own(model: Model) -> None:
    for field in model.own:
        field.required = True  # EFF201


def by_handler(field: Field, kind: str) -> None:
    HANDLERS[kind](field)  # EFF201


def by_callable(field: Field, kind: str) -> None:
    CALLABLES[kind](field)


def by_run(field: Field) -> None:
    for run in RUNS:
        run(field)  # EFF201
"#;

#[test]
fn typed_additions_keep_the_items_and_join_what_they_add() {
    let files = [
        ("pkg/typed.py", TYPED_MODEL),
        ("pkg/store.py", TYPED_STORE),
        ("pkg/passes.py", TYPED_CALLS),
    ];
    let p = project("dispatch_typed_additions", &files);
    assert_eq!(reported(&p), marked(&files));
    assert_eq!(verdict(&p, "pkg.passes.by_callable"), "unknown");
    assert_eq!(verdict(&p, "pkg.passes.by_handler"), "impure");
    assert_eq!(verdict(&p, "pkg.passes.by_run"), "impure");
}

/// A list a global or an attribute holds that the code extends with `+=` and then changes
/// through what the operator gave (`xs += [a]`, then `xs.append(b)`), or extends with `+=` from
/// a local alias, gets what both add: `heki inspect` does not call a loop over it pure.
const IN_PLACE_OPERATORS: &str = r#"COUNT = 0


class Quiet:
    def run(self) -> None:
        pass


class Loud:
    def run(self) -> None:
        global COUNT
        COUNT += 1


ITEMS = [Quiet()]
ALIASED = [Quiet()]


def grow() -> None:
    global ITEMS
    ITEMS += [Loud()]


def grow_through_alias(extra) -> None:
    xs = ALIASED
    xs += [Quiet()]
    xs.append(extra)


def run_items() -> None:
    for item in ITEMS:
        item.run()


def run_aliased() -> None:
    for item in ALIASED:
        item.run()


class Holder:
    def __init__(self) -> None:
        self.items = [Quiet()]

    def grow(self) -> None:
        self.items += [Loud()]

    def run_all(self) -> None:
        for item in self.items:
            item.run()
"#;

#[test]
fn in_place_operators_add_their_items() {
    let p = Project::new("dispatch_in_place_operators");
    p.write("m.py", IN_PLACE_OPERATORS);
    assert_eq!(verdict(&p, "m.run_items"), "impure");
    assert_eq!(verdict(&p, "m.run_aliased"), "unknown");
    assert_eq!(verdict(&p, "m.Holder.run_all"), "impure");
}

/// Changes that add no item keep the items: those after a `return`, whose operands no path
/// binds, and `+=` on a tuple, which makes a new tuple (an alias keeps the items of the one it
/// holds). `setdefault` with a key only adds the key.
const ADDING_NOTHING: &str = r#"COUNT = 0


class Quiet:
    def run(self) -> None:
        pass


class Loud:
    def run(self) -> None:
        global COUNT
        COUNT += 1


ITEMS = [Quiet()]
TABLE = {"quiet": Quiet()}


def after_return() -> None:
    return
    more = [Loud()]
    ITEMS.extend(more)
    extra = {"loud": Loud()}
    TABLE.update(extra)


def run_items() -> None:
    for item in ITEMS:
        item.run()


def run_table() -> None:
    TABLE["quiet"].run()


def run_tuple_alias() -> None:
    items = (Quiet(),)
    kept = items
    items += (Loud(),)
    for item in kept:
        item.run()


def run_set_default() -> None:
    seen = {Quiet(): 1}
    seen.setdefault(Loud())
    for item in seen:
        item.run()
"#;

#[test]
fn changes_adding_no_item_keep_the_items() {
    let p = Project::new("dispatch_adding_nothing");
    p.write("m.py", ADDING_NOTHING);
    assert_eq!(verdict(&p, "m.run_items"), "pure");
    assert_eq!(verdict(&p, "m.run_table"), "pure");
    assert_eq!(verdict(&p, "m.run_tuple_alias"), "pure");
    assert_eq!(verdict(&p, "m.run_set_default"), "impure");
}

/// A module body or a class body binding a function's name again on some paths
/// (`if flag: fetch = _quiet`, also to a function another module defines, or a `def` on one
/// branch and another function on the other): a call through the name may run either function,
/// so a report through it is not exact, whichever of them waits. Bound again on every path, the
/// name holds only what binds it last; a definition after that binding holds it again. An alias
/// of a definition bound before the definition's name is bound again keeps the definition. A
/// class body binding a name on some paths only, or to an import on one, says nothing of it
/// (`Optional.extra`, `Client.parse`).
const REBOUND_FETCHERS: &str = r#"import os

import requests


def _quiet(url):
    return url


def _blocking(url):
    return requests.get(url)


def fetch(url):
    return requests.get(url)


if os.environ.get("QUIET"):
    fetch = _quiet


def load(url):
    return url


if os.environ.get("SLOW"):
    load = _blocking


def get(url):
    return requests.get(url)


_original_get = get
get = _quiet


def read(url):
    return url


read = _blocking


def late(url):
    return url


late = _blocking


def late(url):
    return url


if os.environ.get("FAST"):

    def pick(url):
        return requests.get(url)

else:
    pick = _quiet


def first(url):
    return requests.get(url)


def second(url):
    return url


first = second
second = _blocking


async def in_module():
    fetch("u")  # EFF101 [heuristic]
    read("u")  # EFF101
    get("u")
"#;

const REBOUND_CLIENT: &str = r#"import os

import requests


def _quiet(self, url):
    return url


def _blocking(self, url):
    return requests.get(url)


def _blocking_url(url):
    return requests.get(url)


class Client:
    def fetch(self, url):
        return requests.get(url)

    if os.environ.get("QUIET"):
        fetch = _quiet

    def load(self, url):
        return url

    if os.environ.get("SLOW"):
        load = _blocking

    def get(self, url):
        return requests.get(url)

    get = _quiet

    late = _blocking

    def late(self, url):
        return url

    def check(self, url):
        return url

    if os.environ.get("SLOW"):
        check = staticmethod(_blocking_url)

    def parse(self, url):
        return url

    if os.environ.get("FAST"):
        from json import loads as parse


class Base:
    def extra(self, url):
        return requests.get(url)


class Optional(Base):
    if os.environ.get("QUIET"):
        extra = _quiet
"#;

const REBOUND_CALLS: &str = r#"from pkg import fetchers
from pkg.client import Client
from pkg.fetchers import fetch


async def module_on_a_path():
    fetchers.fetch("u")  # EFF101 [heuristic]


async def imported_on_a_path():
    fetch("u")  # EFF101 [heuristic]


async def module_blocking_on_a_path():
    fetchers.load("u")  # EFF101 [heuristic]


async def module_on_every_path():
    fetchers.get("u")


async def module_alias_before():
    fetchers._original_get("u")  # EFF101


async def module_blocking_on_every_path():
    fetchers.read("u")  # EFF101


async def module_defined_last():
    fetchers.late("u")


async def module_on_either_branch():
    fetchers.pick("u")  # EFF101 [heuristic]


async def module_alias_of_a_definition_bound_again():
    fetchers.first("u")


async def module_definition_bound_again():
    fetchers.second("u")  # EFF101


async def class_on_a_path(c: Client):
    c.fetch("u")  # EFF101 [heuristic]


async def class_object_on_a_path():
    Client.fetch(Client(), "u")  # EFF101 [heuristic]


async def class_object_static_on_a_path():
    Client.check(Client(), "u")  # EFF101 [heuristic]


async def class_attribute_held(c: Client):
    run = Client.fetch
    run(c, "u")  # EFF101 [heuristic]


async def class_blocking_on_a_path(c: Client):
    c.load("u")  # EFF101 [heuristic]


async def class_on_every_path(c: Client):
    c.get("u")


async def class_defined_last(c: Client):
    c.late("u")
"#;

/// A fallback another module defines, bound in place of a function on some paths.
const FALLBACK: &str = r#"import os

from pkg.slow import lookup_slowly


def lookup(url):
    return url


if os.environ.get("SLOW"):
    lookup = lookup_slowly
"#;

const SLOW: &str = r#"import requests


def lookup_slowly(url):
    return requests.get(url)
"#;

const FALLBACK_CALLS: &str = r#"from pkg import fallback


async def module_imported_on_a_path():
    fallback.lookup("u")  # EFF101 [heuristic]
"#;

/// `(path, line, code)` of every report of `codes`.
fn reported_codes(p: &Project, codes: &[&str]) -> Vec<(String, u64, String)> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<(String, u64, String)> = diags
        .iter()
        .filter_map(|d| {
            let rule = common::tiered_code(d);
            codes.contains(&rule).then(|| {
                (
                    d["path"].as_str().unwrap().to_owned(),
                    d["line"].as_u64().unwrap(),
                    rule.to_owned(),
                )
            })
        })
        .collect();
    out.sort();
    out
}

/// The `(path, line, code)` of every line of `files` ending in `# CODE` for one of `codes`.
fn marked_codes(files: &[(&str, &str)], codes: &[&str]) -> Vec<(String, u64, String)> {
    let mut out = Vec::new();
    for (path, text) in files {
        for (i, line) in text.lines().enumerate() {
            for code in codes {
                if line.trim_end().ends_with(&format!("# {code}")) {
                    out.push(((*path).to_owned(), i as u64 + 1, (*code).to_owned()));
                }
            }
        }
    }
    out.sort();
    out
}

#[test]
fn a_name_bound_again_on_some_paths_is_each_binding() {
    const CODES: &[&str] = &["EFF101", "EFF101 [heuristic]"];
    let files = [
        ("pkg/fetchers.py", REBOUND_FETCHERS),
        ("pkg/client.py", REBOUND_CLIENT),
        ("pkg/serve.py", REBOUND_CALLS),
    ];
    let p = Project::new("dispatch_rebound_names");
    p.write("pyproject.toml", BLOCKING_CONFIG);
    p.write("pkg/__init__.py", "");
    for (path, text) in files {
        p.write(path, text);
    }
    assert_eq!(reported_codes(&p, CODES), marked_codes(&files, CODES));
    // A project of its own: past ten reports of one blocking call, one report names the others.
    let files = [
        ("pkg/fallback.py", FALLBACK),
        ("pkg/slow.py", SLOW),
        ("pkg/serve.py", FALLBACK_CALLS),
    ];
    let p = Project::new("dispatch_rebound_fallback");
    p.write("pyproject.toml", BLOCKING_CONFIG);
    p.write("pkg/__init__.py", "");
    for (path, text) in files {
        p.write(path, text);
    }
    assert_eq!(reported_codes(&p, CODES), marked_codes(&files, CODES));
}

const BLOCKING_CONFIG: &str =
    "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"EFF101\"]\n";

/// A class attribute bound to a library function runs it through the class, and as a
/// `staticmethod` also on an instance, where every class the instance may be binds it to the
/// same function. Read on an instance outside `staticmethod`, the function may bind the instance
/// (a Python function) or not (a builtin): the call is left unresolved.
const LIBRARY_ALIASES: &str = r#"import requests


class Api:
    get = requests.get
    fetch = staticmethod(requests.get)


class Mirror(Api):
    fetch = staticmethod(requests.get)


async def on_an_instance(api: Api) -> None:
    api.get("u")


async def on_the_class() -> None:
    Api.get("u")  # EFF101


async def static_on_an_instance(api: Api) -> None:
    api.fetch("u")  # EFF101
"#;

#[test]
fn a_class_attribute_bound_to_a_library_function_is_that_function() {
    const CODES: &[&str] = &["EFF101", "EFF101 [heuristic]"];
    let files = [("pkg/api.py", LIBRARY_ALIASES)];
    let p = Project::new("dispatch_library_aliases");
    p.write("pyproject.toml", BLOCKING_CONFIG);
    p.write("pkg/__init__.py", "");
    for (path, text) in files {
        p.write(path, text);
    }
    assert_eq!(reported_codes(&p, CODES), marked_codes(&files, CODES));
}

/// A base binding `FETCH` to a blocking function, `n - 1` subclasses binding it to others of
/// their own, and an `async def` calling it on an instance of the base.
fn aliased_functions(n: usize) -> String {
    let mut out = vec!["import requests\n".to_owned()];
    for k in 0..n {
        out.push(format!(
            "\ndef fetch_{k}(url):\n    return requests.get(url)\n"
        ));
    }
    out.push("\nclass Source:\n    FETCH = staticmethod(fetch_0)\n".to_owned());
    for k in 1..n {
        out.push(format!(
            "\nclass Source{k}(Source):\n    FETCH = staticmethod(fetch_{k})\n"
        ));
    }
    out.push("\nasync def handle(source: Source) -> None:\n    source.FETCH(\"u\")\n".to_owned());
    out.join("\n")
}

/// A call through a name the classes a receiver may be bind to more than eight functions
/// (`resolver::CHA_CANDIDATE_LIMIT`) runs too many to follow, as the implementations of a method
/// do.
#[test]
fn class_aliases_of_more_than_eight_functions_are_too_many() {
    for (n, reports) in [(8, 1), (9, 0)] {
        let p = Project::new(&format!("dispatch_aliased_functions_{n}"));
        p.write("pyproject.toml", BLOCKING_CONFIG);
        p.write("pkg/__init__.py", "");
        p.write("pkg/sources.py", &aliased_functions(n));
        let found = reported_codes(&p, &["EFF101", "EFF101 [heuristic]"]);
        assert_eq!(found.len(), reports, "{n}: {found:?}");
    }
}

/// A table whose values are `functools.partial` objects of project functions holding keywords
/// or nothing calls those functions, with the call's positions as written, beside named
/// functions, in a list, or bound to an attribute; a partial object holding positions keeps the
/// call unresolved.
const PARTIAL_TABLES: &str = r#"from __future__ import annotations

from functools import partial
from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from pkg.model import Field


def _noop(field: Field) -> None:
    pass


def _require(field: Field, *, deep: bool = False) -> None:
    field.required = True


def _set(value: bool, field: Field) -> None:
    field.required = value


PARTIALS = {"require": partial(_require), "noop": partial(_noop)}
KEYWORDS = {"require": partial(_require, deep=True)}
MIXED = {"noop": _noop, "require": partial(_require)}
LISTED = [partial(_noop), partial(_require)]
NOOPS = {"noop": partial(_noop), "other": partial(_noop)}
POSITIONS = {"require": partial(_set, True)}


class Pass:
    def __init__(self) -> None:
        self._policy = {"require": partial(_require)}

    def run(self, kind: str, field: Field) -> None:
        self._policy[kind](field)
"#;

const PARTIAL_TABLE_CALLS: &str = r#"from __future__ import annotations

from typing import TYPE_CHECKING

from pkg.tables import KEYWORDS, LISTED, MIXED, NOOPS, PARTIALS, POSITIONS, Pass

if TYPE_CHECKING:
    from pkg.model import Field


def by_key(field: Field) -> None:
    PARTIALS["require"](field)  # EFF201


def by_other_key(field: Field) -> None:
    PARTIALS["noop"](field)


def by_any_key(field: Field, kind: str) -> None:
    PARTIALS[kind](field)  # EFF201


def holding_keywords(field: Field, kind: str) -> None:
    KEYWORDS[kind](field)  # EFF201


def beside_a_function(field: Field, kind: str) -> None:
    MIXED[kind](field)  # EFF201


def by_position(field: Field, i: int) -> None:
    LISTED[i](field)  # EFF201


def every_value_writes_nothing(field: Field, kind: str) -> None:
    NOOPS[kind](field)


def holding_positions(field: Field, kind: str) -> None:
    POSITIONS[kind](field)


def through_an_attribute(p: Pass, field: Field, kind: str) -> None:
    p.run(kind, field)  # EFF201
"#;

#[test]
fn a_table_of_partial_objects_calls_their_callables() {
    check_marks(
        "dispatch_partial_tables",
        &[
            ("pkg/tables.py", PARTIAL_TABLES),
            ("pkg/passes.py", PARTIAL_TABLE_CALLS),
        ],
    );
}

const FRESH_TABLE_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "pass"
scopes = ["pkg.passes:*"]
write-universe = ["Field.flag"]
allow-writes = []
fresh-objects = "allow"
"#;

/// The tables of the passes below, in another module: what a call of one of their values
/// returns is new when every value it may be makes a new object.
const FRESH_TABLES: &str = r#"from __future__ import annotations

from functools import partial
from typing import Any

KEPT: list[Any] = []


class Field:
    def __init__(self, data_type: Any = None) -> None:
        self.data_type = data_type
        self.flag = False


def make_field(data_type: Any) -> Field:
    return Field(data_type)


def other_field(data_type: Any) -> Field:
    return Field(data_type)


def kept_field(data_type: Any) -> Field:
    field = Field(data_type)
    KEPT.append(field)
    return field


def unknown_factory() -> Any:
    return KEPT[0]


PARTIALS = {"made": partial(make_field), "other": partial(other_field)}
KEYWORDS = {"row": partial(Field, data_type=None)}
MIXED = {"made": make_field, "other": partial(other_field)}
LISTED = [partial(make_field), partial(other_field)]
POSITIONS = {"row": partial(Field, None)}
ONE_KEPT = {"made": partial(make_field), "kept": partial(kept_field)}
ONE_UNKNOWN = {"row": partial(Field), "other": unknown_factory()}
CHANGED = {"row": partial(Field)}


def register(name: str, factory: Any) -> None:
    CHANGED[name] = factory


class Parser:
    def __init__(self) -> None:
        self._policy = {"row": partial(Field)}

    def build(self, row: str, data_type: Any) -> Field:
        return self._policy[row](data_type=data_type)


class OpenParser:
    def __init__(self) -> None:
        self._policy = {"row": partial(Field)}

    def add(self, name: str, factory: Any) -> None:
        self._policy[name] = factory

    def build(self, row: str, data_type: Any) -> Field:
        return self._policy[row](data_type=data_type)


class ReboundParser:
    def __init__(self) -> None:
        self._policy = {"row": partial(Field)}

    def rebind(self, policy: Any) -> None:
        self._policy = policy

    def build(self, row: str, data_type: Any) -> Field:
        return self._policy[row](data_type=data_type)
"#;

/// A table changed in place, rebound, or holding a value heki does not know, and a partial
/// object holding positions, keep the call unresolved: its result may be an older object. The
/// annotations type the results, so that every write is one of `Field.flag`.
const FRESH_TABLE_PASSES: &str = r#"from __future__ import annotations

from typing import Any

from pkg.tables import (
    CHANGED,
    Field,
    KEYWORDS,
    LISTED,
    MIXED,
    ONE_KEPT,
    ONE_UNKNOWN,
    PARTIALS,
    POSITIONS,
    OpenParser,
    Parser,
    ReboundParser,
)


def through_partials(row: str, data_type: Any) -> None:
    built: Field = PARTIALS[row](data_type)
    built.flag = True


def holding_keywords(row: str) -> None:
    built: Field = KEYWORDS[row]()
    built.flag = True


def beside_a_function(row: str, data_type: Any) -> None:
    built: Field = MIXED[row](data_type)
    built.flag = True


def by_position(i: int, data_type: Any) -> None:
    built: Field = LISTED[i](data_type)
    built.flag = True


def through_an_attribute(parser: Parser, data_type: Any) -> None:
    built: Field = parser.build("row", data_type)
    built.flag = True


def holding_positions(row: str) -> None:
    built: Field = POSITIONS[row]()
    built.flag = True  # EFF201


def one_value_keeps_it(row: str, data_type: Any) -> None:
    built: Field = ONE_KEPT[row](data_type)
    built.flag = True  # EFF201


def one_value_unknown(row: str, data_type: Any) -> None:
    built: Field = ONE_UNKNOWN[row](data_type)
    built.flag = True  # EFF201


def changed_in_place(row: str, data_type: Any) -> None:
    built: Field = CHANGED[row](data_type)
    built.flag = True  # EFF201


def an_open_attribute(parser: OpenParser, data_type: Any) -> None:
    built: Field = parser.build("row", data_type)
    built.flag = True  # EFF201


def a_rebound_attribute(parser: ReboundParser, data_type: Any) -> None:
    built: Field = parser.build("row", data_type)
    built.flag = True  # EFF201
"#;

#[test]
fn a_call_of_a_table_value_returns_new_objects_when_every_value_makes_one() {
    let files = [
        ("pkg/tables.py", FRESH_TABLES),
        ("pkg/passes.py", FRESH_TABLE_PASSES),
    ];
    let p = Project::new("dispatch_fresh_tables");
    p.write("pyproject.toml", FRESH_TABLE_CONFIG);
    p.write("pkg/__init__.py", "");
    for (path, text) in files {
        p.write(path, text);
    }
    assert_eq!(reported(&p), marked(&files));
}

const REQUEST_FIELD_CONFIG: &str = r#"[project]
name = "t"
version = "0"
requires-python = ">=3.11"
dependencies = ["pydantic>=2"]

[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "pass"
scopes = ["pkg.passes:*"]
write-universe = ["DataType.flag", "DataType.data_types", "Field._nullable_without_default", "Field._alias_scope"]
allow-writes = []
fresh-objects = "allow"
"#;

const REQUEST_FIELD_MODEL: &str = r#"from __future__ import annotations

from functools import partial
from typing import Any, Self

from pydantic import BaseModel


class Reference(BaseModel):
    children: list[Any] = []


class DataType(BaseModel):
    flag: bool = False
    data_types: list[DataType] = []
    parent: Any = None
    reference: Reference | None = None

    def register_reference(self) -> None:
        if self.reference:
            self.reference.children.append(self)

    @property
    def all_data_types(self):
        for data_type in self.data_types:
            yield from data_type.all_data_types
        yield self


def copy_tree(data_type: DataType, *, link_parents: bool) -> DataType:
    """Copy a tree; nothing is registered, so the copy is new."""
    copied = data_type.model_copy()
    copied.parent = None
    copied.data_types = [copy_tree(child, link_parents=link_parents) for child in data_type.data_types]
    if link_parents:
        for child in copied.data_types:
            child.parent = copied
    return copied


class Field(BaseModel):
    data_type: DataType
    parent: Any = None

    def copy_owning(self, *, data_type: DataType) -> Self:
        copied = self.model_copy(update={"data_type": data_type, "parent": None})
        data_type.parent = copied
        return copied


class Model(BaseModel):
    fields: list[Field] = []


class Builder:
    def record_nullable_without_default(self, field: Field, value: bool | None) -> None:
        field.__dict__["_nullable_without_default"] = value

    def record_alias_scope(self, field: Field, scope: str) -> None:
        field.__dict__["_alias_scope"] = scope

    def append_field(self, model: Model, field: Field) -> None:
        field.parent = model
        model.fields.append(field)


class Parser:
    def __init__(self) -> None:
        self._field_policy = {"row": partial(Field)}

    def build_field(self, row: str, data_type: DataType) -> Field:
        return self._field_policy[row](data_type=data_type)

    def build_field_directly(self, data_type: DataType) -> Field:
        return Field(data_type=data_type)
"#;

/// A field built directly and one built through a table of `functools.partial` objects are
/// both new: the writes through the builder change objects the pass made.
const REQUEST_FIELD_PASSES: &str = r#"from __future__ import annotations

from pkg.model import Builder, DataType, Model, Parser


def d_control_field_built_directly(parser: Parser, builder: Builder, model: Model, data_type: DataType) -> None:
    built = parser.build_field_directly(data_type.model_copy())
    builder.record_alias_scope(built, "Model")
    builder.append_field(model, built)


def e_field_built_through_a_table_value(parser: Parser, builder: Builder, model: Model, data_type: DataType) -> None:
    built = parser.build_field("row", data_type.model_copy())
    builder.record_alias_scope(built, "Model")
    builder.append_field(model, built)
"#;

#[test]
fn a_field_the_request_builds_through_a_table_value_is_new() {
    let p = Project::new("dispatch_request_field_table");
    p.write("pyproject.toml", REQUEST_FIELD_CONFIG);
    p.write("pkg/__init__.py", "");
    p.write("pkg/model.py", REQUEST_FIELD_MODEL);
    p.write("pkg/passes.py", REQUEST_FIELD_PASSES);
    assert_eq!(reported(&p), Vec::<(String, u64)>::new());
}
