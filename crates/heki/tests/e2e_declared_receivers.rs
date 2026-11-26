//! Receivers whose class comes from what a declaration says, end to end through the real
//! binary: a library class method returning `Self` called on a project class, an attribute a
//! class's type parameter types read on an instance whose annotation gives the argument, and a
//! base class another module binds only under `if TYPE_CHECKING:`. A write of a `__dict__` key
//! no target names is `MUT205` only on an object of unknown class: each shape either names the
//! class (no report) or leaves it open (reported). A name bound only under `if TYPE_CHECKING:`
//! types what it names but never decides what a call runs: the tier of `EFF101` tells it.

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

const LAZY_HTTP: &str = r#"from typing import TYPE_CHECKING, Any

if TYPE_CHECKING:
    from requests import Session as Session
    from time import sleep as pause


def __getattr__(name: str) -> Any:
    if name == "pause":
        from asyncio import sleep  # noqa: PLC0415

        return sleep
    if name == "Session":
        from httpx import AsyncClient  # noqa: PLC0415

        return AsyncClient
    raise AttributeError(name)
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
/// which object runs: a module `__getattr__` may serve another one. Calling it is no call of
/// the function the import names, and a method a class finds only through a base named so is
/// at most a heuristic target, on an instance, on the class, and through `super()`. A method of
/// the class itself comes before that base and stays exact.
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
