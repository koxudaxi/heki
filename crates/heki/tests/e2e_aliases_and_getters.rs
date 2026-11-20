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

/// The diagnostics of `pkg/use.py` before (`before = true`) or from line 39 on, where the
/// allocations start.
fn in_part(diags: &[Value], before: bool) -> Vec<(String, u64, String, String)> {
    found(diags)
        .into_iter()
        .filter(|x| x.0 != "pkg/use.py" || (x.1 < 39) == before)
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
            d(m, 63, "MUT201", "Node.kind"),
            // `_set` is `object.__setattr__`, imported from the module binding it once; the
            // name bound twice, and the one another module rebinds, are not.
            d(u, 9, "MUT201", "Node.kind"),
            // `getattr` with a constant name loads the attribute (behind a walrus, with a
            // default); with a computed name it names none.
            d(u, 16, "MUT201", "Node.extras[\"k\"]"),
            d(u, 17, "MUT201", "Node.items"),
            // A method, a property, a function, and a static method called on an instance
            // returning `node.items` hand out the list. A copy, an attribute returned on one
            // path only or one of two, a property with a setter, a decorated method, and a
            // call unpacking its arguments do not.
            d(u, 22, "MUT201", "Node.items"),
            d(u, 23, "MUT201", "Node.items"),
            d(u, 24, "MUT201", "Node.items"),
            d(u, 25, "MUT201", "Node.items"),
            // The alias imported, in an annotation.
            d(u, 35, "MUT201", "Node.kind"),
        ]
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
    assert_eq!(lines(&diagnostics(&p, &[])), vec![75]);
    p.write(
        "pyproject.toml",
        &CONFIG.replace(
            "allowed-writers",
            "fresh-objects = \"report\"\nallowed-writers",
        ),
    );
    // With `fresh-objects = "report"`, each write is one of the node's class.
    assert_eq!(lines(&diagnostics(&p, &[])), vec![44, 50, 56, 64, 75]);
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
"#;

#[test]
fn a_module_global_is_its_own_object_and_an_alias_the_one_it_names() {
    let p = Project::new("module_globals");
    p.write("pkg/__init__.py", "")
        .write("pkg/io.py", IO)
        .write("pkg/aliased.py", ALIASED);
    assert_eq!(
        found(&diagnostics(&p, &["--extend-select", "EFF105"])),
        vec![
            // An alias of the builtin `open`, and of `time.sleep` (a default rule).
            d("pkg/aliased.py", 8, "EFF105", "builtins.open"),
            d("pkg/aliased.py", 12, "EFF101", "time.sleep"),
            // `open` bound by the module to something else is no builtin, nor is a global
            // bound to a choice between objects.
        ]
    );
}
