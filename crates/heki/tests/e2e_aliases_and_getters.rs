//! What a call or a name stands for, end to end through the real binary: module-level names
//! bound once to another object (`_set = object.__setattr__`, `sleep = time.sleep`), the
//! module's own globals (never a builtin of the same name), `getattr` with a constant name,
//! functions and properties returning an attribute of an argument, and `object.__new__`.

mod common;

use common::Project;
use serde_json::Value;

fn diagnostics(p: &Project, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["--output-format", "json"];
    all.extend_from_slice(args);
    let (code, stdout, stderr) = p.check(&all);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

/// `(path, line, code, target)` of every diagnostic.
fn found(diags: &[Value]) -> Vec<(String, u64, String, String)> {
    diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
                d["target"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

fn d(path: &str, line: u64, code: &str, target: &str) -> (String, u64, String, String) {
    (path.into(), line, code.into(), target.into())
}

const CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT201"]

[[tool.heki.write-owners]]
name = "store"
targets = ["Node.kind", "Node.items", 'Node.extras["k"]']
allowed-writers = ["pkg.store.*"]
"#;

const MODEL: &str = r#"import functools
from typing import Any

_set = object.__setattr__
_new = object.__new__
_twice = object.__setattr__
_rebound_elsewhere = object.__setattr__


def _reset() -> None:
    global _twice
    _twice = print


class Node:
    def __init__(self) -> None:
        self.kind = ""
        self.items: list[int] = []
        self.others: list[int] = []
        self.extras: dict[str, Any] = {}

    def members(self) -> list[int]:
        return self.items

    @property
    def view(self) -> list[int]:
        return self.items

    def copied(self) -> list[int]:
        return list(self.items)

    def either(self, flag: bool) -> list[int]:
        if flag:
            return self.items
        return []

    def one_of_two(self, flag: bool) -> list[int]:
        if flag:
            return self.items
        return self.others

    @property
    def settable(self) -> list[int]:
        return self.items

    @settable.setter
    def settable(self, value: list[int]) -> None:
        pass

    @functools.cache
    def cached(self) -> list[int]:
        return self.items

    @staticmethod
    def items_of(node: "Node") -> list[int]:
        return node.items

    def chosen(self) -> list[int]:
        return self.items


class Branch(Node):
    def chosen(self) -> list[int]:
        return self.others


NodeAlias = Node


def mark(node: NodeAlias) -> None:
    node.kind = "marked"
"#;

/// Rebinds `model._rebound_elsewhere`, which then names no fixed object.
const OTHER: &str = "from pkg import model\n\nmodel._rebound_elsewhere = print\n";

const USE: &str = r#"from pkg.model import Node, NodeAlias, _new, _rebound_elsewhere, _set, _twice


def items_of(node: Node) -> list[int]:
    return node.items


def aliased(node: Node) -> None:
    _set(node, "kind", "a")
    _twice(node, "kind", "b")
    _rebound_elsewhere(node, "kind", "c")


def got(node: Node, name: str) -> None:
    if (extras := getattr(node, "extras", None)) is not None:
        extras["k"] = 1
    getattr(node, "items").append(1)
    getattr(node, name).append(1)


def returned(node: Node, flag: bool, pair: tuple[Node, Node]) -> None:
    node.members().append(1)
    node.view.append(1)
    items_of(node).append(1)
    node.items_of(node).append(1)
    node.copied().append(1)
    node.either(flag).append(1)
    node.one_of_two(flag).append(1)
    node.settable.append(1)
    node.cached().append(1)
    items_of(*pair).append(1)
    node.chosen().append(1)


def mark_imported(node: NodeAlias) -> None:
    node.kind = "imported"


def _link(node: Node) -> None:
    node.kind = "linked"


def built() -> Node:
    node = object.__new__(Node)
    _link(node)
    return node


def built_by_alias() -> Node:
    node = _new(Node)
    _link(node)
    return node


def built_through_the_class() -> Node:
    node = Node.__new__(Node)
    _link(node)
    return node


class Leaf(Node):
    @classmethod
    def make(cls) -> "Leaf":
        made = cls.__new__(cls)
        _link(made)
        return made


class Fresh(Node):
    def __new__(cls) -> "Fresh":
        return super().__new__(cls)


def of_unknown_class(kind: type) -> None:
    made = object.__new__(kind)
    _link(made)
"#;

fn project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("pyproject.toml", CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", MODEL)
        .write("pkg/other.py", OTHER)
        .write("pkg/use.py", USE);
    p
}

/// The diagnostics of `pkg/use.py` before (`before = true`) or from line 40 on, where the
/// allocations start.
fn in_part(diags: &[Value], before: bool) -> Vec<(String, u64, String, String)> {
    found(diags)
        .into_iter()
        .filter(|x| x.0 != "pkg/use.py" || (x.1 < 40) == before)
        .filter(|x| before || x.0 == "pkg/use.py")
        .collect()
}

#[test]
fn aliases_getattr_and_getters_reach_the_guarded_attribute() {
    let p = project("aliases_getters");
    let (m, u) = ("pkg/model.py", "pkg/use.py");
    assert_eq!(
        in_part(&diagnostics(&p, &[]), true),
        vec![
            // An annotation naming a module alias of the class.
            d(m, 71, "MUT201", "Node.kind"),
            // `_set` is `object.__setattr__`, imported from the module binding it once; the
            // name bound twice, and the one another module rebinds, are not.
            d(u, 9, "MUT201", "Node.kind"),
            // `getattr` with a constant name loads the attribute (behind a walrus, with a
            // default); with a computed name it names none.
            d(u, 16, "MUT201", "Node.extras[\"k\"]"),
            d(u, 17, "MUT201", "Node.items"),
            // A method, a property, a function, and a static method called on an instance
            // returning `node.items` hand out the list. A copy, an attribute returned on one
            // path only or one of two, a property with a setter, a decorated method, a call
            // unpacking its arguments, and overrides returning different attributes do not.
            d(u, 22, "MUT201", "Node.items"),
            d(u, 23, "MUT201", "Node.items"),
            d(u, 24, "MUT201", "Node.items"),
            d(u, 25, "MUT201", "Node.items"),
            // The alias imported, in an annotation.
            d(u, 36, "MUT201", "Node.kind"),
        ]
    );
}

const CALLABLE: &str = r#"class Node:
    def __init__(self) -> None:
        self.items: list[int] = []


class Picker:
    def __call__(self, node: Node) -> list[int]:
        return node.items


class Copier:
    def __call__(self, node: Node) -> list[int]:
        return list(node.items)


def grow(node: Node, pick: Picker, copy: Copier) -> None:
    pick(node).append(1)
    copy(node).append(1)
"#;

/// Calling an instance whose `__call__` returns an attribute of its argument hands out that
/// object; one returning a copy does not.
#[test]
fn a_callable_instance_returning_an_attribute_hands_it_out() {
    let p = Project::new("callable_getter");
    p.write("pyproject.toml", CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", CALLABLE);
    assert_eq!(
        found(&diagnostics(&p, &[])),
        vec![d("pkg/model.py", 17, "MUT201", "Node.items")]
    );
}

#[test]
fn object_new_allocates_an_instance_nothing_holds() {
    let p = project("object_new");
    let lines =
        |diags: &[Value]| -> Vec<u64> { in_part(diags, false).iter().map(|x| x.1).collect() };
    // `object.__new__(Node)`, its alias, `Node.__new__(Node)`, and `cls.__new__(cls)` in a class
    // method construct the node the helper fills: no report with `fresh-objects = "allow"`.
    // `object.__new__` of a class heki does not know constructs nothing it can tell.
    assert_eq!(lines(&diagnostics(&p, &[])), vec![76]);
    p.write(
        "pyproject.toml",
        &CONFIG.replace(
            "allowed-writers",
            "fresh-objects = \"report\"\nallowed-writers",
        ),
    );
    // With `fresh-objects = "report"`, each write is one of the node's class.
    assert_eq!(lines(&diagnostics(&p, &[])), vec![45, 51, 57, 65, 76]);
}

const IO: &str = r#"import functools
import time


def _quiet_open(path: str) -> str:
    return path


open = functools.partial(_quiet_open)
reader = __builtins__["open"] if isinstance(__builtins__, dict) else _quiet_open


async def shadowed() -> str:
    return open("data.txt")


async def chosen() -> object:
    return reader("data.txt")
"#;

const ALIASED: &str = r#"import time

reader = open
nap = time.sleep


async def read() -> object:
    return reader("data.txt")


async def wait() -> None:
    nap(1)


def cleanup(open: object) -> None:
    del open


async def builtin() -> object:
    return open("data.txt")
"#;

const ALIAS_CALLER: &str = r#"from pkg import aliased
from pkg.aliased import nap


async def by_module() -> None:
    aliased.nap(1)


async def by_name() -> None:
    nap(1)
"#;

#[test]
fn a_module_global_is_its_own_object_and_an_alias_the_one_it_names() {
    let p = Project::new("module_globals");
    p.write("pkg/__init__.py", "")
        .write("pkg/io.py", IO)
        .write("pkg/aliased.py", ALIASED)
        .write("pkg/caller.py", ALIAS_CALLER);
    assert_eq!(
        found(&diagnostics(&p, &["--extend-select", "EFF105"])),
        vec![
            // An alias of the builtin `open`, and of `time.sleep` (a default rule).
            d("pkg/aliased.py", 8, "EFF105", "builtins.open"),
            d("pkg/aliased.py", 12, "EFF101", "time.sleep"),
            // Deleting a local of the same name binds no global: `open` is still the builtin.
            d("pkg/aliased.py", 20, "EFF105", "builtins.open"),
            // The alias called through its module, and imported from it.
            d("pkg/caller.py", 6, "EFF101", "time.sleep"),
            d("pkg/caller.py", 10, "EFF101", "time.sleep"),
            // `open` bound by the module to something else is no builtin, nor is a global
            // bound to a choice between objects.
        ]
    );
}

const CATALOGUED: &str = r#"[project]
name = "t"

[[tool.heki.ecosystem]]
symbol = "mypkg._native.read"
effects = ["blocking_net"]

[[tool.heki.ecosystem]]
symbol = "mypkg.fetch"
effects = ["blocking_net"]

[[tool.heki.ecosystem]]
symbol = "mypkg.http.get"
effects = ["blocking_net"]

[[tool.heki.ecosystem]]
symbol = "mypkg.http.session"
return-type = "requests.Session"
"#;

const HTTP: &str = r#"import functools
import time
import urllib.request

get = functools.partial(urllib.request.urlopen, timeout=5)
nap = functools.partial(time.sleep, 1)
session = functools.partial(object)


async def same_module() -> None:
    get("x")
"#;

const APP: &str = r#"import mypkg._native as native
from mypkg import fetch, http
from mypkg._native import read


async def by_name() -> None:
    read()


async def by_module() -> None:
    native.read()


async def star_imported() -> None:
    fetch()


async def declared_global() -> None:
    http.get("x")


async def undeclared_global() -> None:
    http.nap()


async def declared_return_type() -> None:
    http.session().get("x")
"#;

/// A name heki does not see the module bind (a compiled submodule, a star import of one) is
/// what the catalog says of it, and so is a module global the configuration declares; a global
/// of a value heki does not know and the configuration leaves out is an unresolved call.
#[test]
fn a_catalogued_name_heki_does_not_see_bound_keeps_its_effects() {
    let p = Project::new("catalogued_names");
    p.write("pyproject.toml", CATALOGUED)
        .write("mypkg/__init__.py", "from ._native import *\n")
        .write("mypkg/http.py", HTTP)
        .write("mypkg/app.py", APP);
    assert_eq!(
        found(&diagnostics(&p, &[])),
        vec![
            d("mypkg/app.py", 7, "EFF101", "mypkg._native.read"),
            d("mypkg/app.py", 11, "EFF101", "mypkg._native.read"),
            d("mypkg/app.py", 15, "EFF101", "mypkg.fetch"),
            d("mypkg/app.py", 19, "EFF101", "mypkg.http.get"),
            // The declared `return-type` of a module global types what calling it returns.
            d("mypkg/app.py", 27, "EFF101", "requests.Session.get"),
            d("mypkg/http.py", 11, "EFF101", "mypkg.http.get"),
        ]
    );
}

const GETTERS: &str = r#"import functools


class A:
    def __init__(self) -> None:
        self.items: list[int] = []

    @property
    def view(self) -> list[int]:
        return self.items

    def members(self) -> list[int]:
        return self.items


class B:
    def __init__(self) -> None:
        self.items: list[int] = []
        self.view: list[int] = []


class C:
    def __init__(self) -> None:
        self.items: list[int] = []

    @functools.cached_property
    def view(self) -> list[int]:
        return self.items


class D(A):
    view: list[int] = []  # noqa: RUF012


class E(A):
    pass


class F(object):
    pass


def plain(a: A, e: E) -> None:
    a.view.append(1)
    e.view.append(1)


def subclass_may_override(a: A) -> None:
    a.members().append(1)


def union(obj: A | B) -> None:
    obj.view.append(1)
    obj.members().append(1)


def cached(c: C) -> None:
    c.view = []
    c.view.append(1)


def class_attribute_first(d: D) -> None:
    d.view.append(1)


def lacking(obj: A | F) -> None:
    obj.view.append(1)
    obj.members().append(1)


class G(A):
    pass


G.view = []  # type: ignore[assignment]


class Pool:
    def __init__(self) -> None:
        self.items: list[int] = []

    def drain(self) -> list[int]:
        items = self.items
        self.items = []
        return items

    def peek(self) -> list[int]:
        return self.items


def pick(*args: object, node: Pool) -> list[int]:
    return node.items


def rebound_on_class(g: G) -> None:
    g.view.append(1)


def drained(pool: Pool, decoy: A) -> None:
    pool.drain().append(1)
    pool.peek().append(1)
    pick(decoy, node=pool).append(1)
"#;

const GETTERS_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT201"]

[[tool.heki.write-owners]]
name = "store"
targets = ["A.items", "B.items", "C.items", "D.items", "E.items", "Pool.items"]
allowed-writers = ["pkg.store.*"]
"#;

/// A getter hands out its attribute only on a class whose reads run it: not on a member of a
/// union reading a plain attribute or lacking the method, not through a `cached_property`
/// (its value can be replaced on the instance), not on a class whose own class attribute
/// comes first along its MRO or is stored on the class outside its body.
#[test]
fn a_getter_hands_out_its_attribute_only_where_every_read_runs_it() {
    let p = Project::new("getters_every_read");
    p.write("pyproject.toml", GETTERS_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", GETTERS);
    let m = "pkg/model.py";
    assert_eq!(
        found(&diagnostics(&p, &[])),
        vec![
            d(m, 44, "MUT201", "A.items"),
            // A target on a base covers the instances of its subclasses: the write matches
            // `A.items` and `E.items`, one report of the target configured first.
            d(m, 45, "MUT201", "A.items"),
            d(m, 49, "MUT201", "A.items"),
            // `drain` storing the attribute again is a write itself.
            d(m, 84, "MUT201", "Pool.items"),
            // A getter returning the attribute; not one that stores it again before returning
            // what it held, nor a keyword-only parameter bound by name.
            d(m, 101, "MUT201", "Pool.items"),
        ]
    );
}

const BASES: &str = r#"import requests
from vendor import CacheMixin


class Cached(CacheMixin, requests.Session):
    pass


class Plain(requests.Session):
    pass


async def cached(s: Cached) -> None:
    s.get("x")


async def plain(s: Plain) -> None:
    s.get("x")


class Table(dict):
    pass


def private(t: Table) -> None:
    t._helper()
"#;

/// A method no project class defines comes from the first base outside the project along the
/// MRO that heki takes to define it: a catalogued base's after a base heki knows nothing of,
/// which may define it, is unknown, and a builtin type does not define a private name.
#[test]
fn an_unknown_base_before_a_catalogued_one_leaves_the_method_unknown() {
    let p = Project::new("unknown_base_first");
    p.write("pkg/__init__.py", "").write("pkg/bases.py", BASES);
    assert_eq!(
        found(&diagnostics(&p, &[])),
        vec![d("pkg/bases.py", 18, "EFF101", "requests.Session.get")]
    );
}

const CLOSURE_MODEL: &str = r#"class Field:
    def __init__(self) -> None:
        self.extras: dict[str, object] = {}
        self.meta: dict[str, list[int]] = {}
        self.other: dict[str, object] = {}


class Other:
    def __init__(self) -> None:
        self.extras: dict[str, object] = {}
"#;

const CLOSURES: &str = r#"from collections.abc import Callable

from pkg.model import Field, Other


def closure_writes_the_alias(field: Field) -> Callable[[], None]:
    extras = field.extras

    def inner() -> None:
        extras["a"] = 1

    return inner


def lambda_writes_the_alias(field: Field) -> Callable[[], object]:
    extras = field.extras
    return lambda: extras.setdefault("b", 2)


def control_local_alias(field: Field) -> None:
    extras = field.extras
    extras["c"] = 3


def comprehension(field: Field, keys: list[str]) -> None:
    extras = field.extras
    [extras.setdefault(k, 1) for k in keys]


def generator(field: Field, keys: list[str]) -> None:
    extras = field.extras
    any(extras.setdefault(k, 1) for k in keys)


def two_levels(field: Field):
    extras = field.extras

    def mid():
        def inner() -> None:
            extras["a"] = 1

        return inner

    return mid


def entry_in_closure(field: Field):
    entry = field.meta["k"]
    return lambda: entry.append(1)


def untyped_alias(target):
    extras = target.extras

    def inner() -> None:
        extras["a"] = 1

    return inner


def rebound_by_nonlocal(field: Field, fresh: dict[str, object]):
    extras = field.extras

    def swap() -> None:
        nonlocal extras
        extras = fresh

    def inner() -> None:
        extras["a"] = 1

    return swap, inner


def another_class(other: Other):
    extras = other.extras
    return lambda: extras.setdefault("b", 2)


def two_attributes(field: Field, flag: bool):
    extras = field.extras
    if flag:
        extras = field.other
    return lambda: extras.setdefault("b", 2)


def read_only(field: Field):
    extras = field.extras
    return lambda: extras.get("b")
"#;

/// A scope nested in a function reads the function's variable through its closure cell, which
/// holds every value the function binds to it: an alias of a guarded attribute (or of an entry
/// of its container) bound in the function is that attribute in a nested function, a lambda, a
/// comprehension, or a generator expression, at any depth. A cell a nested scope rebinds
/// (`nonlocal`) may hold anything, and a cell bound to two attributes holds neither.
#[test]
fn a_nested_scope_writes_the_attribute_an_enclosing_alias_holds() {
    let c = "pkg/closures.py";
    for attribution in ["call-graph", "lexical"] {
        let p = Project::new(&format!(
            "closure_aliases_{}",
            attribution.replace('-', "_")
        ));
        p.write(
            "pyproject.toml",
            &format!(
                "[project]\nname = \"t\"\nrequires-python = \">=3.11\"\n\n[tool.heki.lint]\nselect = [\"MUT201\", \"MUT205\"]\n\n[[tool.heki.write-owners]]\nname = \"store\"\ntargets = [\"Field.extras\", \"Field.meta\"]\nallowed-writers = [\"pkg.store.*\"]\nunresolved = \"report\"\nattribution = \"{attribution}\"\n"
            ),
        )
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", CLOSURE_MODEL)
        .write("pkg/closures.py", CLOSURES);
        let diags = diagnostics(&p, &[]);
        assert!(diags.iter().all(|x| x["path"] == c), "{diags:#?}");
        let mut at: Vec<(u64, String, String)> = diags
            .iter()
            .map(|x| {
                (
                    x["line"].as_u64().unwrap(),
                    x["rule_code"].as_str().unwrap().to_owned(),
                    x["qualname"].as_str().unwrap().to_owned(),
                )
            })
            .collect();
        at.sort();
        let lexical = attribution == "lexical";
        let lambda = |f: &str| {
            if lexical {
                f.to_owned()
            } else {
                format!("{f}.<locals>.<lambda>")
            }
        };
        let e =
            |line: u64, code: &str, qualname: &str| (line, code.to_owned(), qualname.to_owned());
        assert_eq!(
            at,
            vec![
                e(10, "MUT201", "closure_writes_the_alias.<locals>.inner"),
                e(17, "MUT201", &lambda("lambda_writes_the_alias")),
                e(22, "MUT201", "control_local_alias"),
                // A comprehension or generator expression with a scope of its own (3.11) hands
                // its writes to the function running it.
                e(27, "MUT201", "comprehension"),
                e(32, "MUT201", "generator"),
                e(40, "MUT201", "two_levels.<locals>.mid.<locals>.inner"),
                e(49, "MUT201", &lambda("entry_in_closure")),
                // An attribute of an object of unknown class.
                e(56, "MUT205", "untyped_alias.<locals>.inner"),
            ],
            "{attribution}: {diags:#?}"
        );
        assert_eq!(
            found(&diags)
                .into_iter()
                .filter(|x| x.1 == 49)
                .map(|x| x.3)
                .collect::<Vec<_>>(),
            ["Field.meta"]
        );
    }
}
