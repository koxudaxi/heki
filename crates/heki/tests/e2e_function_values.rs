//! Calls of a function a value holds, end to end through the real binary. A field of a
//! `NamedTuple` or of a frozen dataclass holds what the call constructing the instance passed,
//! or its default: in the function constructing it, in a scope nested in that function, in an
//! attribute or a global every store of which binds such an instance, and over a module-level
//! table of instances bound once (one of the values its items hold). A function every `return`
//! of which gives a function it defines, or a function or class a name binds, returns that
//! object. A class attribute bound to a `functools.partial` object of a function, read through
//! the class or wrapped in `staticmethod`, calls the function with the call's arguments and
//! gives what it returns; a class held either way is constructed, and the object it gives is
//! new.
//!
//! Each premise changed leaves the call unresolved: a dataclass that is not frozen, one with a
//! `__post_init__`, a field a method stores through `object.__setattr__`, a copy given other
//! values (`dataclasses.replace`), a table something adds values of unknown type to, `**`
//! unpacking, a subclass of a named tuple, a class built by a `dataclass_transform` decorator, a
//! factory that may return `None` or is annotated with a class, a plain `partial` read on an
//! instance (Python 3.14 binds the instance to it, older versions do not), and a `partial`
//! holding a positional argument.
//!
//! Every fixture marks the lines `EFF201` reports in `pkg/passes.py`: `# writes` for a write of
//! `Field.required`, `# unresolved` for a call heki cannot resolve.

mod common;

use common::Project;
use serde_json::Value;

const CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "no-required-writes"
scopes = ["pkg.passes:*"]
write-universe = ["Field.required"]
allow-writes = []
unresolved = "report"
"#;

const MODEL: &str = r#"from __future__ import annotations


class Field:
    def __init__(self) -> None:
        self.required = False


def require(field: Field) -> bool:
    field.required = True
    return True


def noop(field: Field) -> bool:
    return False
"#;

/// `(line, kind)` of every `EFF201` report in `pkg/passes.py`: `writes` or `unresolved`.
fn reported(p: &Project) -> Vec<(u64, &'static str)> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "exit {code}: {stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<(u64, &'static str)> = diags
        .iter()
        .filter(|d| d["rule_code"] == "EFF201" && d["path"] == "pkg/passes.py")
        .map(|d| {
            let message = d["message"].as_str().unwrap();
            let kind = if message.contains("cannot resolve") {
                "unresolved"
            } else if message.contains("writes `Field.required`") {
                "writes"
            } else {
                panic!("unexpected report: {message}")
            };
            (d["line"].as_u64().unwrap(), kind)
        })
        .collect();
    out.sort();
    out
}

/// `(line, kind)` of every line of `text` ending in `# writes` or `# unresolved`.
fn marked(text: &str) -> Vec<(u64, &'static str)> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim_end();
        for kind in ["writes", "unresolved"] {
            if line.ends_with(&format!("# {kind}")) {
                out.push((i as u64 + 1, kind));
            }
        }
    }
    out
}

fn check_marks(name: &str, defs: &str, passes: &str) {
    let p = Project::new(name);
    p.write("pyproject.toml", CONFIG);
    p.write("pkg/__init__.py", "");
    p.write("pkg/model.py", MODEL);
    p.write("pkg/defs.py", defs);
    p.write("pkg/passes.py", passes);
    assert_eq!(reported(&p), marked(passes));
}

const FIELD_DEFS: &str = r#"from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass, field
from typing import NamedTuple

from pkg.model import Field, noop, require


class View(NamedTuple):
    is_opaque: Callable[[Field], bool]
    other: Callable[[Field], bool] = noop


@dataclass(frozen=True)
class Spec:
    name: str
    when: Callable[[Field], bool] | None = None
    run: Callable[[Field], bool] = require
    check: Callable[[Field], bool] = field(default=noop, kw_only=True)


@dataclass(frozen=True)
class Child(Spec):
    extra: Callable[[Field], bool] = noop


SPECS: tuple[Spec, ...] = (Spec("a", when=require), Spec("b", when=noop), Spec("c"))
QUIET: tuple[Spec, ...] = (Spec("b", when=noop), Spec("c"))
VIEW = View(require)


class Holder:
    def __init__(self) -> None:
        self.view = View(require)

    def opaque(self, field: Field) -> bool:
        return self.view.is_opaque(field)
"#;

/// The fields a constructor binds: by position, by keyword, by a default the class body binds
/// to a function (also a base dataclass's), a keyword-only field, one a generator expression
/// reads through its closure cell, one of the items of a tuple of instances bound once (each
/// value a construction passed, `None` adding nothing: no item of `QUIET` writes), and an
/// instance held in a module global and in an attribute.
const FIELD_CALLS: &str = r#"from __future__ import annotations

from pkg.defs import QUIET, SPECS, VIEW, Child, Holder, Spec, View
from pkg.model import Field, noop, require


def by_position(field: Field) -> bool:
    return View(require).is_opaque(field)  # writes


def by_keyword(field: Field) -> bool:
    view = View(is_opaque=require)
    return view.is_opaque(field)  # writes


def a_default_that_writes_nothing(field: Field) -> bool:
    return View(require).other(field)


def a_dataclass_field(field: Field) -> bool:
    spec = Spec("a", when=require)
    return spec.when is not None and spec.when(field)  # writes


def a_default_function(field: Field) -> bool:
    return Spec("a").run(field)  # writes


def a_keyword_only_field(field: Field) -> bool:
    return Spec("a", check=require).check(field)  # writes


def a_base_field(field: Field) -> bool:
    return Child("a", when=noop).run(field)  # writes


def in_a_nested_scope(field: Field) -> bool:
    view = View(require)
    return any(view.is_opaque(f) for f in [field])  # writes


def over_a_table(field: Field) -> bool:
    return any(spec.when is not None and spec.when(field) for spec in SPECS)  # writes


def over_a_table_that_writes_nothing(field: Field) -> bool:
    return any(spec.when is not None and spec.when(field) for spec in QUIET)


def through_a_global(field: Field) -> bool:
    return VIEW.is_opaque(field)  # writes


def through_an_attribute(field: Field) -> bool:
    return Holder().opaque(field)  # writes
"#;

#[test]
fn a_field_holds_what_its_construction_passed() {
    check_marks("function_values_fields", FIELD_DEFS, FIELD_CALLS);
}

const OPEN_FIELD_DEFS: &str = r#"from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from typing import Any, NamedTuple, dataclass_transform

from pkg.model import Field, noop


class View(NamedTuple):
    is_opaque: Callable[[Field], bool]


class SubView(View):
    pass


@dataclass(frozen=True)
class Spec:
    name: str
    when: Callable[[Field], bool] | None = None


@dataclass
class Loose:
    when: Callable[[Field], bool]


@dataclass(frozen=True)
class Posted:
    when: Callable[[Field], bool]

    def __post_init__(self) -> None:
        object.__setattr__(self, "when", noop)


@dataclass(frozen=True)
class Swapped:
    when: Callable[[Field], bool]

    def swap(self, when: Callable[[Field], bool]) -> None:
        object.__setattr__(self, "when", when)


@dataclass_transform(frozen_default=True)
def record(cls: Any) -> Any:
    return cls


@record
class Recorded:
    when: Callable[[Field], bool]


GROWN = [Spec("a", when=noop)]


def grow(when: Any) -> None:
    GROWN.append(Spec("x", when=when))
"#;

/// Where a field may hold another value than the construction passed, or the construction is
/// not the one a generated constructor makes, the call stays unresolved.
const OPEN_FIELD_CALLS: &str = r#"from __future__ import annotations

import dataclasses
from typing import Any

from pkg.defs import GROWN, Loose, Posted, Recorded, Spec, SubView, Swapped
from pkg.model import Field, noop, require


def not_frozen(field: Field) -> bool:
    return Loose(noop).when(field)  # unresolved


def with_a_post_init(field: Field) -> bool:
    return Posted(require).when(field)  # unresolved


def stored_by_a_method(field: Field) -> bool:
    return Swapped(noop).when(field)  # unresolved


def replaced(field: Field) -> bool:
    spec = dataclasses.replace(Spec("a", when=noop), when=require)
    return spec.when is not None and spec.when(field)  # unresolved


def copied(field: Field) -> bool:
    spec = dataclasses.replace(Spec("a", when=require))
    return spec.when is not None and spec.when(field)  # writes


def grown(field: Field) -> bool:
    return any(spec.when is not None and spec.when(field) for spec in GROWN)  # unresolved


def unpacked(field: Field, kwargs: dict[str, Any]) -> bool:
    spec = Spec("a", **kwargs)
    return spec.when is not None and spec.when(field)  # unresolved


def a_subclass(field: Field) -> bool:
    return SubView(noop).is_opaque(field)  # unresolved


def a_transform(field: Field) -> bool:
    return Recorded(noop).when(field)  # unresolved


def through_cls(cls: type[Spec], field: Field) -> bool:
    spec = cls("a", when=noop)
    return spec.when is not None and spec.when(field)  # unresolved
"#;

#[test]
fn a_field_stays_unknown_where_its_value_may_change() {
    check_marks(
        "function_values_open_fields",
        OPEN_FIELD_DEFS,
        OPEN_FIELD_CALLS,
    );
}

const FACTORY_DEFS: &str = r#"from __future__ import annotations

from collections.abc import Callable
from typing import Protocol

from pkg.model import Field, noop, require


class Step(Protocol):
    def __call__(self, field: Field) -> bool: ...


def make_requirer() -> Callable[[Field], None]:
    def requirer(field: Field) -> None:
        field.required = True

    return requirer


def make_plain():
    def requirer(field: Field) -> None:
        field.required = True

    return requirer


def make_quiet() -> Callable[[Field], None]:
    def quiet(field: Field) -> None:
        pass

    return quiet


def pick(flag: bool) -> Callable[[Field], bool]:
    if flag:
        return require
    return noop


def maybe(flag: bool) -> Callable[[Field], bool] | None:
    if flag:
        return require
    return None


def as_step() -> Step:
    return require


def replaced_later() -> Callable[[Field], bool]:
    return target


def target(field: Field) -> bool:
    return False


def wrapped(function: Callable[[], Callable[[Field], bool]]) -> Callable[[], Callable[[Field], bool]]:
    return function


@wrapped
def decorated() -> Callable[[Field], bool]:
    return require
"#;

/// A factory returning a function it defines (with or without an annotation), one of two
/// functions, a function that writes nothing; one that may return `None`, is annotated with a
/// class, or is replaced by a decorator stays unresolved, and one returning a module global that
/// another module binds again returns whatever that module binds.
const FACTORY_CALLS: &str = r#"from __future__ import annotations

import pkg.defs
from pkg.defs import as_step, decorated, make_plain, make_quiet, make_requirer, maybe, pick, replaced_later
from pkg.model import Field, require

pkg.defs.target = require


def a_closure(field: Field) -> None:
    requirer = make_requirer()
    requirer(field)  # writes


def without_an_annotation(field: Field) -> None:
    make_plain()(field)  # writes


def a_closure_that_writes_nothing(field: Field) -> None:
    make_quiet()(field)


def one_of_two(field: Field, flag: bool) -> bool:
    return pick(flag)(field)  # writes


def maybe_none(field: Field, flag: bool) -> bool:
    run = maybe(flag)
    return run is not None and run(field)  # unresolved


def a_protocol(field: Field) -> bool:
    return as_step()(field)  # unresolved


def a_global_bound_again(field: Field) -> bool:
    return replaced_later()(field)  # unresolved


def through_a_decorator(field: Field) -> bool:
    return decorated()(field)  # unresolved
"#;

#[test]
fn a_function_returning_a_function_returns_it() {
    check_marks("function_values_factories", FACTORY_DEFS, FACTORY_CALLS);
}

const PARTIAL_DEFS: &str = r#"from __future__ import annotations

import dataclasses
import functools
from functools import partial

from pkg.model import Field


def copy_field(field: Field, *, deep: bool = True) -> Field:
    return Field()


def copy_into(field: Field, target: Field) -> Field:
    return target


class Copier:
    _copy = staticmethod(partial(copy_field, deep=False))
    _bare = staticmethod(functools.partial(copy_field))
    _plain = partial(copy_field)
    _held = staticmethod(partial(copy_into, Field()))
    _library = staticmethod(partial(dataclasses.replace, name="x"))

    def copy_required(self, field: Field) -> Field:
        copied = self._copy(field)
        copied.required = True
        return copied

    def bare(self, field: Field) -> Field:
        copied = self._bare(field)
        copied.required = True
        return copied

    def plain_on_instance(self, field: Field) -> Field:
        copied = self._plain(field)
        copied.required = True
        return copied

    def holding_a_position(self, field: Field) -> Field:
        copied = self._held(field)
        copied.required = True
        return copied


def through_the_class(field: Field) -> Field:
    copied = Copier._plain(field)
    copied.required = True
    return copied
"#;

/// The copy a partial class attribute makes is a new object: writing it is a write of a fresh
/// object, which the boundary allows. A plain partial read on an instance and one holding a
/// positional argument stay unresolved, and the write on the latter's result is a write of an
/// object of unknown class.
const PARTIAL_CALLS: &str = r#"from __future__ import annotations

from pkg.defs import Copier, through_the_class
from pkg.model import Field


def through_staticmethod(field: Field) -> Field:
    return Copier().copy_required(field)


def without_keywords(field: Field) -> Field:
    return Copier().bare(field)


def through_class(field: Field) -> Field:
    return through_the_class(field)


def plain_on_an_instance(field: Field) -> Field:
    return Copier().plain_on_instance(field)  # unresolved


def holding_a_position(field: Field) -> Field:
    return Copier().holding_a_position(field)  # unresolved
"#;

#[test]
fn a_partial_class_attribute_calls_its_callable() {
    let p = Project::new("function_values_partials");
    p.write("pyproject.toml", CONFIG);
    p.write("pkg/__init__.py", "");
    p.write("pkg/model.py", MODEL);
    p.write("pkg/defs.py", PARTIAL_DEFS);
    p.write("pkg/passes.py", PARTIAL_CALLS);
    let (code, stdout, stderr) = p.check(&["--output-format", "concise"]);
    assert!(code == 0 || code == 1, "exit {code}: {stdout}{stderr}");
    let lines: Vec<&str> = stdout.lines().filter(|l| l.contains("EFF201")).collect();
    for name in ["through_staticmethod", "without_keywords", "through_class"] {
        assert!(
            !lines.iter().any(|l| l.contains(&format!("`{name}`"))),
            "{name} is reported: {stdout}"
        );
    }
    let plain: Vec<&&str> = lines
        .iter()
        .filter(|l| l.contains("`plain_on_an_instance`"))
        .collect();
    assert!(
        plain.iter().any(
            |l| l.contains("cannot resolve (`_plain` via `pkg.defs.Copier.plain_on_instance`)")
        ),
        "{stdout}"
    );
    let held: Vec<&&str> = lines
        .iter()
        .filter(|l| l.contains("`holding_a_position`"))
        .collect();
    assert!(
        held.iter().any(|l| l.contains("cannot resolve (`_held`")),
        "{stdout}"
    );
    assert!(
        held.iter()
            .any(|l| l.contains("writes `required` on an object of unknown class")),
        "{stdout}"
    );
}

/// A keyword a partial class attribute holds binds its parameter with a value the call does not
/// show: the parameter's default is not what the callable gets, so the wait its default selects
/// is no exact report. A partial holding nothing passes the default, as the call written out
/// does.
#[test]
fn a_keyword_a_partial_holds_is_no_default() {
    let p = Project::new("function_values_partial_keywords");
    p.write("pyproject.toml", "[project]\nname = \"t\"\n");
    p.write("pkg/__init__.py", "");
    p.write(
        "pkg/naps.py",
        r#"import time
from functools import partial


def nap(seconds: float = 1.0) -> None:
    time.sleep(seconds)


class Napper:
    quick = staticmethod(partial(nap, seconds=0))
    plain = staticmethod(partial(nap))

    async def held_keyword(self) -> None:
        self.quick()

    async def no_keyword(self) -> None:
        self.plain()
"#,
    );
    let (code, stdout, stderr) = p.check(&["--select", "EFF101", "--output-format", "json"]);
    assert!(code == 0 || code == 1, "exit {code}: {stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let at = |line: u64| -> Vec<&str> {
        diags
            .iter()
            .filter(|d| d["line"].as_u64() == Some(line))
            .map(common::tiered_code)
            .collect()
    };
    assert_eq!(at(14), ["EFF101 [heuristic]"], "{stdout}");
    assert_eq!(at(17), ["EFF101"], "{stdout}");
}

/// The request's reproduction: a named tuple of functions, a closure a factory returns, the
/// `when` of a module-level table of frozen dataclasses, and a copy through
/// `staticmethod(partial(...))`, which writes a fresh object.
#[test]
fn the_reproduction_resolves_every_held_function() {
    let p = Project::new("function_values_reproduction");
    p.write("pyproject.toml", CONFIG);
    p.write("pkg/__init__.py", "");
    p.write("pkg/model.py", MODEL);
    p.write(
        "pkg/defs.py",
        r#"from __future__ import annotations

from collections.abc import Callable
from dataclasses import dataclass
from functools import partial
from typing import NamedTuple

from pkg.model import Field, require


class View(NamedTuple):
    is_opaque: Callable[[Field], bool]


def make_requirer() -> Callable[[Field], None]:
    def requirer(field: Field) -> None:
        field.required = True

    return requirer


@dataclass(frozen=True)
class Spec:
    name: str
    when: Callable[[Field], bool] | None = None


SPECS: tuple[Spec, ...] = (Spec("a", when=lambda field: require(field)),)


def copy_field(field: Field, *, deep: bool = True) -> Field:
    return Field()


class Copier:
    _copy = staticmethod(partial(copy_field, deep=False))

    def copy_required(self, field: Field) -> Field:
        copied = self._copy(field)
        copied.required = True
        return copied
"#,
    );
    let passes = r#"from __future__ import annotations

from pkg.defs import SPECS, Copier, View, make_requirer
from pkg.model import Field, require


def calls_a_named_tuple_field(field: Field) -> bool:
    return View(require).is_opaque(field)  # writes


def calls_a_returned_closure(field: Field) -> None:
    make_requirer()(field)  # writes


def calls_the_when_of_a_table(field: Field) -> bool:
    return any(spec.when is not None and spec.when(field) for spec in SPECS)  # writes


def copies_through_a_partial(field: Field) -> Field:
    return Copier().copy_required(field)
"#;
    p.write("pkg/passes.py", passes);
    assert_eq!(reported(&p), marked(passes));
}

/// A class a field or a partial class attribute holds is constructed by a call through it: the
/// object the call returns is new, and writing it is a write of a fresh object, as for the class
/// called directly.
#[test]
fn a_held_class_constructs_a_new_object() {
    check_marks(
        "function_values_held_classes",
        r#"from __future__ import annotations

from functools import partial
from typing import NamedTuple

from pkg.model import Field


class Fresh(Field):
    def __init__(self) -> None:
        super().__init__()
        self.required = True


class Factories(NamedTuple):
    make: type[Field]


class Kit:
    make = staticmethod(partial(Fresh))
"#,
        r#"from __future__ import annotations

from pkg.defs import Factories, Fresh, Kit
from pkg.model import Field


def through_a_field() -> Field:
    made = Factories(Fresh).make()
    made.required = True
    return made


def through_a_partial() -> Field:
    made = Kit().make()
    made.required = True
    return made


def directly() -> Field:
    made = Fresh()
    made.required = True
    return made
"#,
    );
}
