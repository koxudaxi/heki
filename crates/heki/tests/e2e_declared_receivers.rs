//! Receivers whose class comes from what a declaration says, end to end through the real
//! binary: a library class method returning `Self` called on a project class, an attribute a
//! class's type parameter types read on an instance whose annotation gives the argument, and a
//! base class another module binds only under `if TYPE_CHECKING:`. A write of a `__dict__` key
//! no target names is `MUT205` only on an object of unknown class: each shape either names the
//! class (no report) or leaves it open (reported). A name bound only under `if TYPE_CHECKING:`
//! types what it names but never decides what a call runs: the tier of `EFF101` tells it. A
//! name a module serves from its `__getattr__` is what the hook returns for it where heki
//! follows the hook, which decides what a call runs.

mod common;

use common::Project;
use std::collections::BTreeSet;

const CONFIG: &str = r#"[lint]
select = ["MUT201", "MUT205"]

[[write-owners]]
name = "cache"
targets = ["pkg.model.Node._cache"]
allowed-writers = ["pkg.store:*"]
unlisted-dict-keys = "report"
unresolved = "report"
"#;

const STORE: &str = r#"from pkg.model import Node


def fill(node: Node) -> None:
    node.__dict__["_cache"] = 1
"#;

/// A project with the write owner of `pkg.model.Node._cache` and `files` under `pkg/`.
fn project(name: &str, files: &[(&str, &str)]) -> Project {
    let p = Project::new(name);
    p.write("heki.toml", CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", "class Node:\n    pass\n")
        .write("pkg/store.py", STORE);
    for (rel, text) in files {
        p.write(&format!("pkg/{rel}"), text);
    }
    p
}

/// The functions `MUT205` reports a write on an object of unknown class in.
fn unknown_receivers(p: &Project) -> BTreeSet<String> {
    let (_, out, err) = p.check(&["--config", "heki.toml", "--output-format", "concise", "pkg"]);
    assert!(!err.contains("error"), "{err}");
    out.lines()
        .filter(|l| l.contains("MUT205") && l.contains("on an object of unknown class"))
        .map(|l| {
            let name = l.split('`').nth(1).unwrap_or_default();
            name.to_owned()
        })
        .collect()
}

fn names(list: &[&str]) -> BTreeSet<String> {
    list.iter().map(|s| (*s).to_owned()).collect()
}

/// `Model.model_construct(...)` and `Model.model_validate(...)` are `-> Self` class methods of
/// pydantic's: called on a project model they give an instance of that model, as
/// `model_copy()` of an instance does. A class method a project class annotates `-> Self` gives
/// the class it is called on, a subclass included.
#[test]
fn class_methods_returning_self_give_the_class_they_are_called_on() {
    let p = project(
        "classmethod_self",
        &[(
            "use.py",
            r#"from typing import Self

from pydantic import BaseModel


class Schema(BaseModel):
    name: str = ""


class Built:
    @classmethod
    def build(cls) -> Self:
        return cls()


class Derived(Built):
    pass


def copied(schema: Schema) -> None:
    copy = schema.model_copy()
    copy.__dict__["_mark"] = True


def constructed() -> None:
    made = Schema.model_construct(None)
    made.__dict__["_mark"] = True


def validated() -> None:
    made = Schema.model_validate({"name": "x"})
    made.__dict__["_mark"] = True


def built() -> None:
    made = Derived.build()
    made.__dict__["_mark"] = True


def on_unknown(kind) -> None:
    made = kind.model_construct(None)
    made.__dict__["_mark"] = True


def on_either(flag: bool, other) -> None:
    kind = Schema if flag else other
    made = kind.model_construct(None)
    made.__dict__["_mark"] = True
"#,
        )],
    );
    assert_eq!(unknown_receivers(&p), names(&["on_either", "on_unknown"]));
}

const SITE: &str = r#"from typing import Generic, Optional, TypeAlias, TypeVar

T = TypeVar("T")
U = TypeVar("U")


class Schema:
    pass


class Site(Generic[T]):
    obj: T
    maybe: Optional[T]
    quoted: "T"

    def __init__(self, obj: T) -> None:
        self.held: T = obj


class Pair(Generic[T, U]):
    first: T
    second: U
    either: T

    def __init__(self) -> None:
        self.either: U


class Other(Generic[U]):
    obj: T


SchemaSite: TypeAlias = "Site[Schema]"
"#;

/// An attribute annotated with a type parameter of its class (`obj: T` in `Site(Generic[T])`)
/// holds an instance of the argument an annotation of the instance gives (`site: Site[Schema]`),
/// also through a string `TypeAlias`, by position, and in a method's `self.held: T`; joined
/// instances hold either argument. An attribute annotated with two parameters (in the body and
/// in a method) holds neither.
#[test]
fn an_attribute_typed_by_a_type_parameter_takes_the_argument() {
    let p = project(
        "type_parameter",
        &[
            ("site.py", SITE),
            (
                "use.py",
                r#"from pkg.site import Other, Pair, Schema, SchemaSite, Site


def direct(schema: Schema) -> None:
    schema.__dict__["_mark"] = True


def through_site(site: Site[Schema]) -> None:
    site.obj.__dict__["_mark"] = True


def through_alias(site: SchemaSite) -> None:
    site.obj.__dict__["_mark"] = True


def quoted(site: "Site[Schema]") -> None:
    site.quoted.__dict__["_mark"] = True


def held(site: Site[Schema]) -> None:
    site.held.__dict__["_mark"] = True


def second(pair: Pair[int, Schema]) -> None:
    pair.second.__dict__["_mark"] = True


def first_of_pair(pair: Pair[Schema, int]) -> None:
    pair.first.__dict__["_mark"] = True


def joined(flag: bool, site: Site[Schema], pair_site: Site[Pair]) -> None:
    held = site if flag else pair_site
    held.obj.__dict__["_mark"] = True


def optional_first(pair: Pair[Schema | None, Schema]) -> None:
    pair.first.__dict__["_mark"] = True


def two_parameters(pair: Pair[Schema, Schema]) -> None:
    pair.either.__dict__["_mark"] = True


def unsubscripted(site: Site) -> None:
    site.obj.__dict__["_mark"] = True


def optional_attribute(site: Site[Schema]) -> None:
    site.maybe.__dict__["_mark"] = True


def optional_argument(site: Site[Schema | None]) -> None:
    site.obj.__dict__["_mark"] = True


def union_argument(site: Site[Schema | Pair]) -> None:
    site.obj.__dict__["_mark"] = True


def unbound_parameter(other: Other[Schema]) -> None:
    other.obj.__dict__["_mark"] = True


def rebound(site: Site[Schema], made) -> None:
    site = made
    site.obj.__dict__["_mark"] = True
"#,
            ),
        ],
    );
    assert_eq!(
        unknown_receivers(&p),
        names(&[
            "optional_argument",
            "optional_first",
            "two_parameters",
            "optional_attribute",
            "rebound",
            "unbound_parameter",
            "unsubscripted",
        ])
    );
}

/// The type parameters of a PEP 695 class statement (`class Site[K, T]:`), by position: an
/// argument left out gives nothing, and so does a parameter after a `TypeVarTuple`, whose
/// arguments take any number of positions.
#[test]
fn a_type_parameter_list_names_the_class_parameters() {
    let p = project(
        "type_parameter_list",
        &[(
            "use.py",
            r#"from typing import Generic, TypeVar, TypeVarTuple


class Schema:
    pass


class Site[K, T]:
    key: K
    obj: T


def through_site(site: Site[int, Schema]) -> None:
    site.obj.__dict__["_mark"] = True


def key(site: Site[Schema, int]) -> None:
    site.key.__dict__["_mark"] = True


def too_few(site: Site[int]) -> None:
    site.obj.__dict__["_mark"] = True


class Packed[*Ts, T]:
    obj: T


def after_a_tuple_parameter(packed: Packed[int, Schema]) -> None:
    packed.obj.__dict__["_mark"] = True


Us = TypeVarTuple("Us")
U = TypeVar("U")


class Many(Generic[U, *Us]):
    obj: U


def with_a_starred_parameter(many: Many[Schema, int]) -> None:
    many.obj.__dict__["_mark"] = True
"#,
        )],
    );
    assert_eq!(
        unknown_receivers(&p),
        names(&[
            "after_a_tuple_parameter",
            "too_few",
            "with_a_starred_parameter"
        ])
    );
}

const REEXPORT_TC: &str = r#"from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from pydantic import BaseModel as BaseModel


def __getattr__(name: str) -> Any:
    if name == "BaseModel":
        from pydantic import BaseModel  # noqa: PLC0415

        return BaseModel
    raise AttributeError(name)
"#;

const REEXPORT_ELSE: &str = r#"from typing import TYPE_CHECKING

if TYPE_CHECKING:
    from pydantic import BaseModel as BaseModel
else:
    BaseModel = object
"#;

const TYPE_CHECKING_USE: &str = r#"from pkg.reexport import BaseModel
from pkg.reexport_else import BaseModel as ElseBase
from pkg.reexport_tc import BaseModel as CheckedBase


class Plain(BaseModel):
    name: str = ""


class Checked(CheckedBase):
    name: str = ""


class Otherwise(ElseBase):
    name: str = ""


def plain(obj: Plain) -> None:
    copy = obj.model_copy()
    copy.__dict__["_mark"] = True


def checked(obj: Checked) -> None:
    copy = obj.model_copy()
    copy.__dict__["_mark"] = True


def otherwise(obj: Otherwise) -> None:
    copy = obj.model_copy()
    copy.__dict__["_mark"] = True
"#;

/// A name another module binds only under `if TYPE_CHECKING:` (and serves at run time from its
/// `__getattr__`) is the class that import names: a project class based on it has that class's
/// methods. A module that also binds the name at run time keeps that binding.
#[test]
fn a_type_checking_binding_of_another_module_names_its_class() {
    let p = project(
        "type_checking_base",
        &[
            (
                "reexport.py",
                "from pydantic import BaseModel as BaseModel\n",
            ),
            ("reexport_tc.py", REEXPORT_TC),
            ("reexport_else.py", REEXPORT_ELSE),
            ("use.py", TYPE_CHECKING_USE),
        ],
    );
    assert_eq!(unknown_receivers(&p), names(&["otherwise"]));
}

const LAZY_HTTP: &str = r#"import importlib
from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from requests import Session as Session
    from time import sleep as pause

_SOURCES = {"pause": ("asyncio", "sleep"), "Session": ("httpx", "AsyncClient")}


def __getattr__(name: str) -> Any:
    module, attr = _SOURCES[name]
    return getattr(importlib.import_module(module), attr)
"#;

const LAZY_HTTP_USE: &str = r#"import time

from pkg.lazy_http import Session, pause


class Client(Session):
    def own(self) -> None:
        time.sleep(1)

    async def through_super(self) -> None:
        super().get("https://example.com")


async def paused() -> None:
    pause(1)


async def on_instance(client: Client) -> None:
    client.get("https://example.com")


async def on_class(client: Client) -> None:
    Client.get(client, "https://example.com")


async def own_method(client: Client) -> None:
    client.own()
"#;

/// The `EFF101` reports of `p`, each as its line and its tier ([`common::tiered_code`]).
fn eff101_reports(p: &Project) -> BTreeSet<(u64, String)> {
    let (_, out, err) = p.check(&["--select", "EFF101", "--output-format", "json", "pkg"]);
    assert!(!err.contains("error"), "{err}");
    let reports: Vec<serde_json::Value> =
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    reports
        .iter()
        .map(|d| {
            let line = d["line"].as_u64().expect("a report has a line");
            (line, common::tiered_code(d).to_owned())
        })
        .collect()
}

/// A name a module binds only under `if TYPE_CHECKING:` types what it names, but does not say
/// which object runs: a module `__getattr__` heki cannot follow may serve another one. Calling
/// it is no call of the function the import names, and a method a class finds only through a
/// base named so is at most a heuristic target, on an instance, on the class, and through
/// `super()`. A method of the class itself comes before that base and stays exact.
#[test]
fn a_type_checking_binding_never_decides_what_a_call_runs() {
    let p = project(
        "type_checking_calls",
        &[("lazy_http.py", LAZY_HTTP), ("use.py", LAZY_HTTP_USE)],
    );
    let heuristic = |line: u64| (line, "EFF101 [heuristic]".to_owned());
    // `super().get(...)`, `client.get(...)`, `Client.get(client, ...)`, and `client.own()`;
    // `pause(1)` (line 15) is no report.
    assert_eq!(
        eff101_reports(&p),
        BTreeSet::from([
            heuristic(11),
            heuristic(19),
            heuristic(23),
            (27, "EFF101".to_owned()),
        ])
    );
}

const BASE: &str = r#"from typing import Self


class Base:
    def copy(self) -> Self:
        return type(self)()
"#;

/// The shape of the report: an `lru_cache` factory returning a class it defines, read from a
/// table by the name the hook is given.
const LAZY: &str = r#"from functools import lru_cache
from typing import Any


@lru_cache(maxsize=1)
def _get_base_class() -> type:
    from pkg.base import Base  # noqa: PLC0415

    class _StrictBase(Base):
        pass

    return _StrictBase


_LAZY_EXPORTS = {"Base": _get_base_class}


def __getattr__(name: str) -> Any:
    if (factory := _LAZY_EXPORTS.get(name)) is None:
        raise AttributeError(name)
    return factory()
"#;

const LAZY_BRANCH: &str = r#"import sys
from typing import Any

from pkg.base import Base as _Base


def _other():
    class _Other(_Base):
        pass

    return _Other


_LOADERS = {"Loaded": _other, "Built": _Base}


def __getattr__(name: str) -> Any:
    if name == "Base":
        if sys.version_info >= (3, 11):
            from pkg.base import Base  # noqa: PLC0415

            return Base
        from pkg.base import Base  # noqa: PLC0415

        return Base
    if name in ("Other", "Again"):
        return _other()
    return _LOADERS[name]()
"#;

const LAZY_FALLBACK: &str = r#"import importlib
from typing import Any


def __getattr__(name: str) -> Any:
    if name == "Base":
        from pkg.base import Base  # noqa: PLC0415

        return Base
    return importlib.import_module(f"pkg.{name.lower()}")
"#;

const LAZY_DECLARED: &str = r#"from pkg.base import Base


def _model() -> type[Base]:
    return Base


_EXPORTS = {"Model": _model}


def __getattr__(name: str) -> type[Base]:
    return _EXPORTS[name]()
"#;

const LAZY_OPEN: &str = r#"import importlib
from typing import Any


def __getattr__(name: str) -> Any:
    return getattr(importlib.import_module("pkg.base"), name)
"#;

const LAZY_MAYBE_NONE: &str = r#"from typing import Any


def _base() -> type:
    from pkg.base import Base  # noqa: PLC0415

    return Base


_EXPORTS = {"Base": _base}


def __getattr__(name: str) -> Any:
    try:
        return _EXPORTS[name]()
    except ImportError:
        return None
"#;

const LAZY_PATCHED: &str = r#"from typing import Any


def _base() -> type:
    from pkg.base import Base  # noqa: PLC0415

    class _Patched(Base):
        pass

    return _Patched


_EXPORTS = {"Base": _base}


def __getattr__(name: str) -> Any:
    return _EXPORTS[name]()
"#;

const PATCH: &str = r#"import pkg.lazy_patched


def patch() -> None:
    pkg.lazy_patched._EXPORTS = {}
"#;

const LAZY_CACHED: &str = r#"from typing import Any


def _base() -> type:
    from pkg.base import Base  # noqa: PLC0415

    return Base


_EXPORTS = {"Base": _base}


def __getattr__(name: str) -> Any:
    value = _EXPORTS[name]()
    globals()[name] = value
    return value
"#;

const LAZY_BOUND: &str = r#"from typing import Any

Base: Any = object


def __getattr__(name: str) -> Any:
    if name == "Base":
        from pkg.base import Base  # noqa: PLC0415

        return Base
    raise AttributeError(name)
"#;

const GETATTR_USE: &str = r#"import pkg.lazy as lazy_module
from pkg.base import Base as DirectBase
from pkg.lazy import Base as LazyBase
from pkg.lazy_bound import Base as BoundBase
from pkg.lazy_branch import Again, Built, Loaded, Missing
from pkg.lazy_branch import Base as BranchBase
from pkg.lazy_cached import Base as CachedBase
from pkg.lazy_declared import Model
from pkg.lazy_fallback import Base as FallbackBase
from pkg.lazy_fallback import Other as FallbackOther
from pkg.lazy_maybe_none import Base as MaybeNoneBase
from pkg.lazy_open import Base as OpenBase
from pkg.lazy_patched import Base as PatchedBase


class Direct(DirectBase):
    pass


class Lazy(LazyBase):
    pass


class ThroughModule(lazy_module.Base):
    pass


class Branch(BranchBase):
    pass


class Listed(Again):
    pass


class Unlisted(Missing):
    pass


class OnLoaded(Loaded):
    pass


class OnBuilt(Built):
    pass


class Declared(Model):
    pass


class Fallback(FallbackBase):
    pass


class FallbackOnOther(FallbackOther):
    pass


class Opened(OpenBase):
    pass


class MaybeNone(MaybeNoneBase):
    pass


class Patched(PatchedBase):
    pass


class Cached(CachedBase):
    pass


class Bound(BoundBase):
    pass


def direct(obj: Direct) -> None:
    obj.copy().__dict__["_mark"] = True


def lazy(obj: Lazy) -> None:
    obj.copy().__dict__["_mark"] = True


def constructed() -> None:
    LazyBase().copy().__dict__["_mark"] = True


def through_module(obj: ThroughModule) -> None:
    obj.copy().__dict__["_mark"] = True


def branch(obj: Branch) -> None:
    obj.copy().__dict__["_mark"] = True


def listed(obj: Listed) -> None:
    obj.copy().__dict__["_mark"] = True


def unlisted(obj: Unlisted) -> None:
    obj.copy().__dict__["_mark"] = True


def loaded(obj: OnLoaded) -> None:
    obj.copy().__dict__["_mark"] = True


def built(obj: OnBuilt) -> None:
    obj.copy().__dict__["_mark"] = True


def declared(obj: Declared) -> None:
    obj.copy().__dict__["_mark"] = True


def fallback(obj: Fallback) -> None:
    obj.copy().__dict__["_mark"] = True


def fallback_other(obj: FallbackOnOther) -> None:
    obj.copy().__dict__["_mark"] = True


def opened(obj: Opened) -> None:
    obj.copy().__dict__["_mark"] = True


def maybe_none(obj: MaybeNone) -> None:
    obj.copy().__dict__["_mark"] = True


def patched(obj: Patched) -> None:
    obj.copy().__dict__["_mark"] = True


def cached(obj: Cached) -> None:
    obj.copy().__dict__["_mark"] = True


def bound(obj: Bound) -> None:
    obj.copy().__dict__["_mark"] = True
"#;

/// A name a module binds no other way but serves from its `__getattr__` is what the hook
/// returns for that literal name: the class an `lru_cache` factory read from a table by the
/// name defines (the report's shape, also read as `module.Base` and called), what a branch
/// testing the name against the literal returns (also on two ways, the same class; a later
/// `return` of a table's value the name is no key of raises, and the name takes no `return`
/// past the test's other way), and the class object a factory annotated `-> type[Base]`
/// returns. A class based on it has its methods. A name no test or table of the hook names, a
/// table value heki cannot call (a class another module defines), a hook heki cannot follow, one
/// that may also return `None`, one whose table another module rebinds, one caching what it
/// serves in `globals()`, and a module binding the name itself leave the class unknown.
#[test]
fn a_name_a_module_getattr_serves_is_what_the_hook_returns() {
    let p = project(
        "getattr_served",
        &[
            ("base.py", BASE),
            ("lazy.py", LAZY),
            ("lazy_branch.py", LAZY_BRANCH),
            ("lazy_declared.py", LAZY_DECLARED),
            ("lazy_fallback.py", LAZY_FALLBACK),
            ("lazy_open.py", LAZY_OPEN),
            ("lazy_maybe_none.py", LAZY_MAYBE_NONE),
            ("lazy_patched.py", LAZY_PATCHED),
            ("patch.py", PATCH),
            ("lazy_cached.py", LAZY_CACHED),
            ("lazy_bound.py", LAZY_BOUND),
            ("use.py", GETATTR_USE),
        ],
    );
    assert_eq!(
        unknown_receivers(&p),
        names(&[
            "unlisted",
            "built",
            "fallback_other",
            "opened",
            "maybe_none",
            "patched",
            "cached",
            "bound"
        ])
    );
}

/// Hook factories: `module_hooks` makes a hook serving what `_load` gives for the table's value
/// under the name (a function's call past the tests of strings and pairs), storing it in the
/// module only when `cache` is true; `version_hook` returns a function of its module deciding
/// one literal name; `checked_hooks` raises for one literal name before serving a key's call;
/// `plain_hooks` serves a key's value; `popping_hooks` takes the value out of the table,
/// `noting_hooks` stores the name in a global, and `wrapped_hooks` serves a list.
const HOOKS: &str = r#"import sys
from collections.abc import Callable, Mapping
from importlib import import_module
from typing import Any


def _load(name: str, entry: Any) -> object:
    if isinstance(entry, str):
        return getattr(import_module(entry), name)
    if not not isinstance(entry, tuple):
        return getattr(import_module(entry[0]), entry[1])
    return entry()


def module_hooks(
    module_name: str, table: Mapping[str, Any], *, cache: bool = False
) -> tuple[Callable[[str], Any], Callable[[], list[str]]]:
    made_for = sys.modules.get(module_name)
    if not len(table):
        raise ValueError(module_name)

    def module_getattr(name: str) -> Any:
        if name not in table:
            raise AttributeError(name)
        value = _load(name, table[name])
        if cache:
            setattr(made_for or sys.modules[module_name], name, value)
        return value

    def module_dir() -> list[str]:
        return sorted({*vars(made_for or sys.modules[module_name]), *table})

    return module_getattr, module_dir


def _version_getattr(name: str) -> Any:
    if name == "Version":
        from pkg.base import Base  # noqa: PLC0415

        return Base
    raise AttributeError(name)


def version_hook() -> Callable[[str], Any]:
    return _version_getattr


def checked_hooks(table: Mapping[str, Any]) -> Callable[[str], Any]:
    def module_getattr(name: str) -> Any:
        if name in ("Hidden",):
            raise AttributeError(name)
        return table[name]()

    return module_getattr


def plain_hooks(table: Mapping[str, Any]) -> Callable[[str], Any]:
    def module_getattr(name: str) -> Any:
        if sys.flags.optimize:
            return table.get(name)
        return table.get(name)

    return module_getattr


def popping_hooks(table: dict[str, Any]) -> Callable[[str], Any]:
    def module_getattr(name: str) -> Any:
        if name in table:
            return table.pop(name)()
        raise AttributeError(name)

    return module_getattr


_last_name = ""


def noting_hooks(table: Mapping[str, Any]) -> Callable[[str], Any]:
    def module_getattr(name: str) -> Any:
        global _last_name
        _last_name = name
        return table[name]()

    return module_getattr


def wrapped_hooks(table: Mapping[str, Any]) -> Callable[[str], Any]:
    def module_getattr(name: str) -> Any:
        if name in table:
            return [table[name]]
        raise AttributeError(name)

    return module_getattr
"#;

/// A module binding its `__getattr__` to what `call` makes, with a table of `entries` and the
/// `lru_cache` factory of a class based on `Base`, the report's shape.
fn made(call: &str, entries: &str, after: &str) -> String {
    format!(
        r#"from functools import lru_cache

from pkg.base import Base as _Base
from pkg.hooks import (
    checked_hooks,
    module_hooks,
    noting_hooks,
    plain_hooks,
    popping_hooks,
    wrapped_hooks,
)


@lru_cache(maxsize=1)
def _get_base_class() -> type:
    class _StrictBase(_Base):
        pass

    return _StrictBase


_EXPORTS = {{{entries}}}
{call}
{after}"#
    )
}

const MADE_DISPATCH: &str = r#"from typing import Any


def _made() -> type:
    from pkg.base import Base  # noqa: PLC0415

    class _Made(Base):
        pass

    return _Made


_EXPORTS = {"Base": _made}


def _load(name: str, entry: Any) -> object:
    if name.startswith("_"):
        raise AttributeError(name)
    if not isinstance(entry, (str, bytes)):
        return entry()
    raise ImportError(name)


def _wrap(name: str, entry: Any) -> object:
    return [entry]


_WRAPPED = {"Wrapped": _made}


def __getattr__(name: str) -> Any:
    if name in _WRAPPED:
        return _wrap(name, _WRAPPED[name])
    if name not in _EXPORTS:
        raise AttributeError(name)
    return _load(name, _EXPORTS[name])
"#;

const MADE_LOCAL: &str = r#"from typing import Any

from pkg.base import Base as _Base


def _base() -> type:
    class _Local(_Base):
        pass

    return _Local


def _hook(table):
    def module_getattr(name: str) -> Any:
        return table[name]()

    return module_getattr


def _impl(name: str) -> Any:
    if name == "Aliased":
        return _Base
    raise AttributeError(name)


_EXPORTS = {"Local": _base}
__getattr__ = _hook(_EXPORTS)
"#;

const MADE_ALIAS: &str = r#"from typing import Any

from pkg.base import Base as _Base


def _impl(name: str) -> Any:
    if name == "Aliased":
        return _Base
    raise AttributeError(name)


_HOOKS = (_impl, None)
__getattr__ = _HOOKS[0]
"#;

const MADE_USE: &str = r#"from pkg.made import Base as MadeBase
from pkg.made_alias import Aliased
from pkg.made_flagged import Base as FlaggedBase
from pkg.made_inline import Base as InlineBase
from pkg.made_local import Local
from pkg.made_noted import Base as NotedBase
from pkg.made_wrapped import Base as WrappedBase
from pkg.made_cached import Base as CachedBase
from pkg.made_changed import Base as ChangedBase
from pkg.made_checked import Hidden, Shown
from pkg.made_dispatch import Base as DispatchBase
from pkg.made_dispatch import Wrapped as DispatchWrapped
from pkg.made_index import Base as IndexBase
from pkg.made_outside import Base as OutsideBase
from pkg.made_pair import Base as PairBase
from pkg.made_plain import Plain
from pkg.made_popped import Base as PoppedBase
from pkg.made_string import Base as StringBase
from pkg.made_version import Version


class Made(MadeBase):
    pass


class OnAliased(Aliased):
    pass


class Flagged(FlaggedBase):
    pass


class Inline(InlineBase):
    pass


class OnLocal(Local):
    pass


class Noted(NotedBase):
    pass


class Wrapped(WrappedBase):
    pass


class Cached(CachedBase):
    pass


class Changed(ChangedBase):
    pass


class OnHidden(Hidden):
    pass


class OnShown(Shown):
    pass


class Dispatched(DispatchBase):
    pass


class OnDispatchWrapped(DispatchWrapped):
    pass


class Indexed(IndexBase):
    pass


class Outside(OutsideBase):
    pass


class Paired(PairBase):
    pass


class OnPlain(Plain):
    pass


class Popped(PoppedBase):
    pass


class Stringed(StringBase):
    pass


class Versioned(Version):
    pass


def made(obj: Made) -> None:
    obj.copy().__dict__["_mark"] = True


def aliased(obj: OnAliased) -> None:
    obj.copy().__dict__["_mark"] = True


def flagged(obj: Flagged) -> None:
    obj.copy().__dict__["_mark"] = True


def inline(obj: Inline) -> None:
    obj.copy().__dict__["_mark"] = True


def local(obj: OnLocal) -> None:
    obj.copy().__dict__["_mark"] = True


def noted(obj: Noted) -> None:
    obj.copy().__dict__["_mark"] = True


def wrapped(obj: Wrapped) -> None:
    obj.copy().__dict__["_mark"] = True


def cached(obj: Cached) -> None:
    obj.copy().__dict__["_mark"] = True


def changed(obj: Changed) -> None:
    obj.copy().__dict__["_mark"] = True


def hidden(obj: OnHidden) -> None:
    obj.copy().__dict__["_mark"] = True


def shown(obj: OnShown) -> None:
    obj.copy().__dict__["_mark"] = True


def dispatched(obj: Dispatched) -> None:
    obj.copy().__dict__["_mark"] = True


def dispatch_wrapped(obj: OnDispatchWrapped) -> None:
    obj.copy().__dict__["_mark"] = True


def indexed(obj: Indexed) -> None:
    obj.copy().__dict__["_mark"] = True


def outside(obj: Outside) -> None:
    obj.copy().__dict__["_mark"] = True


def paired(obj: Paired) -> None:
    obj.copy().__dict__["_mark"] = True


def plain(obj: OnPlain) -> None:
    obj.copy().__dict__["_mark"] = True


def popped(obj: Popped) -> None:
    obj.copy().__dict__["_mark"] = True


def stringed(obj: Stringed) -> None:
    obj.copy().__dict__["_mark"] = True


def versioned(obj: Versioned) -> None:
    obj.copy().__dict__["_mark"] = True
"#;

/// A module `__getattr__` bound to what a project function in another module returns is
/// followed as a `def` hook is, with the function's parameters bound to the call's arguments:
/// the report's shape (a closure over the table, unpacked from the pair the factory returns, or
/// taken by index) serves what `_load` gives for the table's function, which the tests of
/// strings and pairs let through to its call; the function of its own module a factory returns
/// serves the literal name it decides; a key the hook tests as a literal gets nothing, the others
/// what calling their value gives; a key's value is served as it is; a factory of the module
/// itself serves alike. A `def` hook handing the value to a function of its module calling it
/// serves the same, and one handing it to a function returning anything else serves nothing. The hook storing into the module (`cache=True`, also a flag heki does not
/// read as a literal, `bool(_CACHE)`), a table changed after the binding or passed as a display, a factory of a
/// module outside the project, a factory taking the value out of the table, a hook storing into
/// a global or serving a list of the value, a function bound under the name without a call
/// (here an item of a tuple), and
/// a string or a pair entry (which `_load` imports by name, as no hook heki follows does) leave
/// the class unknown.
#[test]
fn a_module_getattr_a_project_factory_makes_serves_what_its_hook_returns() {
    let hook = "__getattr__, __dir__ = module_hooks(__name__, _EXPORTS)";
    let entry = r#""Base": _get_base_class"#;
    let files = [
        ("base.py", BASE.to_owned()),
        ("hooks.py", HOOKS.to_owned()),
        ("made.py", made(hook, entry, "")),
        (
            "made_index.py",
            made(
                "__getattr__ = module_hooks(__name__, _EXPORTS)[0]",
                entry,
                "",
            ),
        ),
        (
            "made_cached.py",
            made(
                "__getattr__, __dir__ = module_hooks(__name__, _EXPORTS, cache=True)",
                entry,
                "",
            ),
        ),
        (
            "made_changed.py",
            made(hook, entry, "_EXPORTS[\"Other\"] = _get_base_class\n"),
        ),
        (
            "made_checked.py",
            made(
                "__getattr__ = checked_hooks(_EXPORTS)",
                r#""Hidden": _get_base_class, "Shown": _get_base_class"#,
                "",
            ),
        ),
        (
            "made_plain.py",
            made(
                "__getattr__ = plain_hooks(_EXPORTS)",
                r#""Plain": _Base"#,
                "",
            ),
        ),
        (
            "made_popped.py",
            made("__getattr__ = popping_hooks(_EXPORTS)", entry, ""),
        ),
        (
            "made_outside.py",
            made(hook, entry, "").replace("pkg.hooks import", "lazyhooks import"),
        ),
        (
            "made_string.py",
            made(hook, r#""Base": "pkg.base", "Other": _get_base_class"#, ""),
        ),
        (
            "made_pair.py",
            made(
                hook,
                r#""Base": ("pkg.base", "Base"), "Other": _get_base_class"#,
                "",
            ),
        ),
        (
            "made_version.py",
            "from pkg.hooks import version_hook\n\n__getattr__ = version_hook()\n".to_owned(),
        ),
        ("made_dispatch.py", MADE_DISPATCH.to_owned()),
        ("made_local.py", MADE_LOCAL.to_owned()),
        ("made_alias.py", MADE_ALIAS.to_owned()),
        (
            "made_flagged.py",
            made(
                "__getattr__, __dir__ = module_hooks(__name__, _EXPORTS, cache=bool(_CACHE))",
                entry,
                "",
            )
            .replace("_EXPORTS = {", "_CACHE = False\n_EXPORTS = {"),
        ),
        (
            "made_inline.py",
            made(
                "__getattr__, __dir__ = module_hooks(__name__, {\"Base\": _get_base_class})",
                entry,
                "",
            ),
        ),
        (
            "made_noted.py",
            made("__getattr__ = noting_hooks(_EXPORTS)", entry, ""),
        ),
        (
            "made_wrapped.py",
            made("__getattr__ = wrapped_hooks(_EXPORTS)", entry, ""),
        ),
        ("use.py", MADE_USE.to_owned()),
    ];
    let files: Vec<(&str, &str)> = files.iter().map(|(f, t)| (*f, t.as_str())).collect();
    let p = project("getattr_made", &files);
    assert_eq!(
        unknown_receivers(&p),
        names(&[
            "dispatch_wrapped",
            "aliased",
            "flagged",
            "inline",
            "noted",
            "wrapped",
            "cached",
            "changed",
            "hidden",
            "outside",
            "paired",
            "popped",
            "stringed",
            // The hook's `setattr` when `cache` is true.
            "module_hooks.<locals>.module_getattr",
        ])
    );
}

const LAZY_SERVED: &str = r#"from typing import TYPE_CHECKING, Any

import requests

if TYPE_CHECKING:
    from asyncio import sleep as pause
    from httpx import AsyncClient as Session


def _declared() -> type[requests.Session]:
    return requests.Session


def __getattr__(name: str) -> Any:
    if name == "pause":
        from time import sleep  # noqa: PLC0415

        return sleep
    if name == "Session":
        return requests.Session
    if name == "Declared":
        return _declared()
    raise AttributeError(name)
"#;

const LAZY_SERVED_USE: &str = r#"from pkg.lazy_served import Declared, Session, pause


class Client(Session):
    async def through_super(self) -> None:
        super().get("https://example.com")


class Typed(Declared):
    pass


async def paused() -> None:
    pause(1)


async def on_instance(client: Client) -> None:
    client.get("https://example.com")


async def on_class(client: Client) -> None:
    Client.get(client, "https://example.com")


async def on_declared(typed: Typed) -> None:
    typed.get("https://example.com")


async def annotated(session: Session) -> None:
    session.get("https://example.com")
"#;

/// What a module `__getattr__` heki follows serves decides what a call runs, also where the
/// module's `if TYPE_CHECKING:` import names another object: calling the function it serves,
/// and a method a class based on the class it serves finds there, are exact targets. A class
/// object of a class or a subclass (`-> type[requests.Session]`) gives heuristic targets only,
/// and an annotation naming the name reads it as a type checker does, as the `TYPE_CHECKING`
/// import names it.
#[test]
fn a_followed_module_getattr_decides_what_a_call_runs() {
    let p = project(
        "getattr_calls",
        &[("lazy_served.py", LAZY_SERVED), ("use.py", LAZY_SERVED_USE)],
    );
    assert_eq!(
        eff101_reports(&p),
        BTreeSet::from([
            (6, "EFF101".to_owned()),
            (14, "EFF101".to_owned()),
            (18, "EFF101".to_owned()),
            (22, "EFF101".to_owned()),
            (26, "EFF101 [heuristic]".to_owned()),
        ])
    );
}

const REPLACE_HOOK_CODE: &str = r#"import pkg.lazy_served
from asyncio import sleep


def replacement(name):
    return sleep


pkg.lazy_served.__getattr__.__code__ = replacement.__code__
"#;

const REPLACE_FACTORY_CODE: &str = r#"import pkg.lazy_served


def replacement():
    return None


pkg.lazy_served._declared.__code__ = replacement.__code__
"#;

/// Another module replacing the code of the hook, or of a factory the hook calls, leaves what
/// the module serves open: no exact report through the served name.
#[test]
fn replacing_the_code_of_a_getattr_hook_or_its_factory_leaves_its_names_open() {
    let hook = project(
        "getattr_code_hook",
        &[
            ("lazy_served.py", LAZY_SERVED),
            ("use.py", LAZY_SERVED_USE),
            ("replace.py", REPLACE_HOOK_CODE),
        ],
    );
    assert!(
        eff101_reports(&hook)
            .iter()
            .all(|(_, kind)| kind.contains("[heuristic]"))
    );
    let factory = project(
        "getattr_code_factory",
        &[
            ("lazy_served.py", LAZY_SERVED),
            ("use.py", LAZY_SERVED_USE),
            ("replace.py", REPLACE_FACTORY_CODE),
        ],
    );
    assert!(!eff101_reports(&factory).contains(&(22, "EFF101".to_owned())));
}

const LAZY_TESTS: &str = r#"import os
import sys
from typing import Any

from pkg.base import Base

_ALIASES = {"Alias": Base}
_seen = []


def _made() -> type:
    class _Made(Base):
        pass

    return _Made


_FACTORIES = {"Made": _made, "Again": _made}


def __getattr__(name: str) -> Any:
    if sys.version_info < (3, 8):
        raise AttributeError(name)
    if sys.platform == "emscripten":
        raise AttributeError(name)
    if os.environ.get("NO_LAZY") is not None:
        raise AttributeError(name)
    if os.environ.get("LAZY_DEBUG"):
        _seen.append(name)
    if name.startswith("__"):
        raise AttributeError(name)
    if name == "Old":
        _seen.append(name)
    if name != "Alias":
        _seen.append(name)
    if "Old" == name:
        return Base
    if name not in ("Alias", "Made", "Again", "Missing", "Quiet", "Namespace", "Fallback"):
        raise AttributeError(name)
    if name == "Made":
        _seen.append(name)
    if name == "Fallback":
        return _FACTORIES.get(name, _made)()
    if name == "Quiet":
        return
    if name == "Namespace":
        return globals()[name]
    if not name in _FACTORIES:  # noqa: E713
        return _ALIASES[name]
    if (factory := _FACTORIES.get(name)) is not None:
        return factory()
    return _FACTORIES.get(name, _made)()
"#;

const LAZY_TWICE: &str = r#"import sys
from typing import Any

if sys.version_info >= (3, 7):

    def __getattr__(name: str) -> Any:
        if name == "Twice":
            from pkg.base import Base  # noqa: PLC0415

            return Base
        raise AttributeError(name)

else:

    def __getattr__(name: str) -> Any:
        raise AttributeError(name)
"#;

const LAZY_REBOUND: &str = r#"from typing import Any


def __getattr__(name: str) -> Any:
    if name == "Rebound":
        from pkg.base import Base  # noqa: PLC0415

        return Base
    raise AttributeError(name)


def _quiet(name: str) -> Any:
    raise AttributeError(name)


__getattr__ = _quiet
"#;

const TESTS_USE: &str = r#"from pkg.lazy_rebound import Rebound
from pkg.lazy_tests import Again, Alias, Fallback, Made, Missing, Namespace, Old, Quiet
from pkg.lazy_twice import Twice


class OnOld(Old):
    pass


class OnAlias(Alias):
    pass


class OnMade(Made):
    pass


class OnAgain(Again):
    pass


class OnMissing(Missing):
    pass


class OnQuiet(Quiet):
    pass


class OnNamespace(Namespace):
    pass


class OnFallback(Fallback):
    pass


class OnTwice(Twice):
    pass


class OnRebound(Rebound):
    pass


def old(obj: OnOld) -> None:
    obj.copy().__dict__["_mark"] = True


def alias(obj: OnAlias) -> None:
    obj.copy().__dict__["_mark"] = True


def made(obj: OnMade) -> None:
    obj.copy().__dict__["_mark"] = True


def again(obj: OnAgain) -> None:
    obj.copy().__dict__["_mark"] = True


def missing(obj: OnMissing) -> None:
    obj.copy().__dict__["_mark"] = True


def quiet(obj: OnQuiet) -> None:
    obj.copy().__dict__["_mark"] = True


def namespace(obj: OnNamespace) -> None:
    obj.copy().__dict__["_mark"] = True


def fallback(obj: OnFallback) -> None:
    obj.copy().__dict__["_mark"] = True


def twice(obj: OnTwice) -> None:
    obj.copy().__dict__["_mark"] = True


def rebound(obj: OnRebound) -> None:
    obj.copy().__dict__["_mark"] = True
"#;

/// The tests a hook makes of its name decide which `return` each literal name reaches: a test of
/// something else (the platform, the environment, a method of the name) lets every name take
/// both ways, `==` written either way round and `in` a display or a table let the names they
/// list take the way they hold and every other name the other, also negated (`!=`, `not in`,
/// `not`), and `get(name) is not None` only the table's keys; ways that join again keep the
/// names of both. A key a subscript
/// lacks raises, and `get` with a default calls what it gives for a key. A bare `return` serves
/// `None`, and the default of a key `get` lacks and a value of the namespace are what heki does
/// not follow. A module defining two hooks, or binding the hook's name again, serves no name
/// heki follows.
#[test]
fn the_tests_a_module_getattr_makes_decide_what_each_name_gets() {
    let p = project(
        "getattr_tests",
        &[
            ("base.py", BASE),
            ("lazy_tests.py", LAZY_TESTS),
            ("lazy_twice.py", LAZY_TWICE),
            ("lazy_rebound.py", LAZY_REBOUND),
            ("use.py", TESTS_USE),
        ],
    );
    assert_eq!(
        unknown_receivers(&p),
        names(&[
            "missing",
            "quiet",
            "namespace",
            "fallback",
            "twice",
            "rebound"
        ])
    );
}
