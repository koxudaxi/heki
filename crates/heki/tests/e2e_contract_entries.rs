//! `write-owners` and the entries of a target's container, end to end through the real binary:
//! a helper changing an entry of the container it is handed (`box["k"].add(v)`, through a local
//! alias of the entry) writes the target at the call, the entries a loop takes from `values()`
//! or `items()` of the container are its entries, and an alias of an object's `__dict__` that a
//! nested function captures keeps its object.

mod common;

use common::Project;
use serde_json::Value;

fn diagnostics(p: &Project) -> Vec<Value> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

/// `(path, line, code, qualname)` of every diagnostic.
fn found(diags: &[Value]) -> Vec<(String, u64, String, String)> {
    diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
                d["qualname"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn d(path: &str, line: u64, code: &str, qualname: &str) -> (String, u64, String, String) {
    (path.into(), line, code.into(), qualname.into())
}

fn message_at<'d>(diags: &'d [Value], path: &str, line: u64) -> &'d str {
    diags
        .iter()
        .find(|x| x["path"] == path && x["line"] == line)
        .unwrap_or_else(|| panic!("no diagnostic at {path}:{line}: {diags:#?}"))["message"]
        .as_str()
        .unwrap()
}

const BOX_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT201", "MUT108"]

[[tool.heki.write-owners]]
name = "box"
targets = ["pkg.model.Holder.box"]
allowed-writers = ["pkg.model:*"]
"#;

const BOX_MODEL: &str = r#"from dataclasses import dataclass, field


class Box(dict[str, set[str]]):
    def add_name(self, key: str, name: str) -> None:
        self[key].add(name)


class Holder:
    def __init__(self) -> None:
        self.box = Box()


def fill(box: Box) -> None:
    box["k"].add("v")


@dataclass(frozen=True)
class Frozen:
    tags: dict[str, list[str]] = field(default_factory=dict)
"#;

// ================================================================== a helper's entry change

const HELPERS: &str = r#"from typing import Any

from pkg.model import Box, Frozen, Holder, fill


def put(box: Box) -> None:
    box["k"] = {"v"}


def add(box: Box) -> None:
    box["k"].add("v")


def add_alias(box: Box) -> None:
    names = box["k"]
    names.add("v")


def add_untyped(box: Any) -> None:
    box["k"].discard("v")


def add_deeper(box: Box) -> None:
    add(box)


def reads(box: Box) -> int:
    return len(box["k"])


def copies(box: Box) -> None:
    names = set(box["k"])
    names.add("v")


def add_names(names: set[str]) -> None:
    names.add("v")


def hand_entry(box: Box) -> None:
    add_names(set())
    add_names(box["k"])


def hand_copy(box: Box) -> None:
    add(dict(box))


def use(holder: Holder) -> None:
    put(holder.box)
    add(holder.box)
    add_alias(holder.box)
    add_untyped(holder.box)
    add_deeper(holder.box)
    reads(holder.box)
    copies(holder.box)
    fill(holder.box)
    holder.box["k"].add("v")
    holder.box.add_name("k", "v")
    hand_entry(holder.box)
    hand_copy(holder.box)


def add_tag(tags: dict[str, list[str]]) -> None:
    tags["k"].append("v")


def frozen(config: Frozen) -> None:
    add_tag(config.tags)
    config.tags["k"].append("v")
"#;

#[test]
fn a_helpers_change_of_an_entry_of_its_parameter_writes_the_target_at_the_call() {
    let p = Project::new("entries_helper");
    p.write("pyproject.toml", BOX_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", BOX_MODEL)
        .write("pkg/use.py", HELPERS);
    let diags = diagnostics(&p);
    let u = "pkg/use.py";
    assert_eq!(
        found(&diags),
        vec![
            // An item store on the parameter, as before.
            d(u, 50, "MUT201", "use"),
            // An entry changed in place, also through a local alias of it, by a helper that does
            // not type the container, and two calls deep.
            d(u, 51, "MUT201", "use"),
            d(u, 52, "MUT201", "use"),
            d(u, 53, "MUT201", "use"),
            d(u, 54, "MUT201", "use"),
            // A read, a copy of an entry, and an allowed writer change nothing outside the
            // owners; the caller's own change of an entry, and a method of the container
            // changing an entry of its own `self`.
            d(u, 58, "MUT201", "use"),
            d(u, 59, "MUT201", "use"),
            // An entry, or a shallow copy holding the entries, handed on to a helper changing it.
            d(u, 60, "MUT201", "use"),
            d(u, 61, "MUT201", "use"),
        ]
    );
    assert!(
        message_at(&diags, u, 51).contains(
            "writes `pkg.model.Holder.box` (inside an item, through a helper) via `pkg.use.add`"
        ),
        "{diags:#?}"
    );
    assert!(
        message_at(&diags, u, 52).contains("via `pkg.use.add_alias`"),
        "{diags:#?}"
    );
    assert!(
        message_at(&diags, u, 59)
            .contains("writes `pkg.model.Holder.box` (in place inside an item)"),
        "{diags:#?}"
    );
    // A frozen field's entries are not the field: neither the direct change nor the helper's is
    // `MUT108`.
    assert!(
        diags.iter().all(|x| x["rule_code"] != "MUT108"),
        "{diags:#?}"
    );
}

// ================================================================== entries a loop takes

const LOOPS: &str = r#"from pkg.model import Holder


def by_key(holder: Holder) -> None:
    holder.box["k"].clear()


def by_values(holder: Holder) -> None:
    for names in holder.box.values():
        names.clear()


def by_items(holder: Holder) -> None:
    for _key, names in holder.box.items():
        names.clear()


def by_pair(holder: Holder) -> None:
    for pair in holder.box.items():
        pair[1].clear()


def by_view(holder: Holder) -> None:
    values = holder.box.values()
    for names in values:
        names.discard("v")


def by_get(holder: Holder) -> None:
    holder.box.get("k", set()).clear()


def keys_only(holder: Holder) -> None:
    for key in holder.box:
        print(key.upper())
    for key, _names in holder.box.items():
        print(key.upper())


def other_container(holder: Holder, other: dict[str, set[str]]) -> None:
    for names in other.values():
        names.clear()
    for names in holder.box.values():
        print(len(names))
"#;

#[test]
fn the_entries_a_loop_takes_from_values_or_items_are_entries_of_the_target() {
    let p = Project::new("entries_loops");
    p.write("pyproject.toml", BOX_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", BOX_MODEL)
        .write("pkg/use.py", LOOPS);
    let diags = diagnostics(&p);
    let u = "pkg/use.py";
    assert_eq!(
        found(&diags),
        vec![
            d(u, 5, "MUT201", "by_key"),
            // `values()`, `items()` unpacked or indexed, a view bound first, and `get`.
            d(u, 10, "MUT201", "by_values"),
            d(u, 15, "MUT201", "by_items"),
            d(u, 20, "MUT201", "by_pair"),
            d(u, 26, "MUT201", "by_view"),
            d(u, 30, "MUT201", "by_get"),
            // Keys, another container's entries, and reads of the entries are no writes.
        ]
    );
    assert!(
        message_at(&diags, u, 15)
            .contains("writes `pkg.model.Holder.box` (in place inside an item)"),
        "{diags:#?}"
    );
}

// ================================================================== a captured `__dict__` alias

const DICT_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT201", "MUT205"]

[[tool.heki.write-owners]]
name = "cache"
targets = ["pkg.model.Node._cache"]
allowed-writers = ["pkg.store:*"]
unlisted-dict-keys = "report"
unresolved = "report"
"#;

const CLOSURES: &str = r#"from pkg.model import Node


def direct(node: Node) -> None:
    namespace = node.__dict__
    namespace["_mark"] = 1


def closure(node: Node) -> None:
    namespace = node.__dict__

    def inner() -> None:
        namespace["_mark"] = 1

    inner()


def closure_annotated(node: Node) -> None:
    namespace: dict[str, object] = node.__dict__

    def inner() -> None:
        namespace["_mark"] = 1

    inner()


def closure_target(node: Node) -> None:
    namespace = vars(node)
    set_cache = lambda: namespace.setdefault("_cache", 1)
    set_cache()


def two_bindings(node: Node, other: Node, flag: bool) -> None:
    namespace = node.__dict__
    if flag:
        namespace = other.__dict__

    def inner() -> None:
        namespace["_mark"] = 1

    inner()


def mixed_bindings(node: Node, other: object, flag: bool) -> None:
    namespace = node.__dict__
    if flag:
        namespace = other.__dict__

    def inner() -> None:
        namespace["_mark"] = 1

    inner()


def rebound(node: Node) -> None:
    namespace = node.__dict__

    def reset() -> None:
        nonlocal namespace
        namespace = {}

    def inner() -> None:
        namespace["_mark"] = 1

    reset()
    inner()


def other_dict(node: Node) -> None:
    namespace = {"a": node}

    def inner() -> None:
        namespace["_mark"] = 1

    inner()


def unknown(obj) -> None:
    namespace = obj.__dict__

    def inner() -> None:
        namespace["_cache"] = 1

    inner()
"#;

#[test]
fn a_dict_alias_a_nested_function_captures_keeps_its_object() {
    let p = Project::new("entries_closure_dict");
    p.write("pyproject.toml", DICT_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", "class Node:\n    pass\n")
        .write(
            "pkg/store.py",
            "from pkg.model import Node\n\n\ndef fill(node: Node) -> None:\n    node.__dict__[\"_cache\"] = 1\n",
        )
        .write("pkg/use.py", CLOSURES);
    let diags = diagnostics(&p);
    let u = "pkg/use.py";
    assert_eq!(
        found(&diags),
        vec![
            d(u, 6, "MUT201", "direct"),
            // Through the captured alias, with or without an annotation, and from a lambda,
            // whose write its caller reports at the call.
            d(u, 13, "MUT201", "closure.<locals>.inner"),
            d(u, 22, "MUT201", "closure_annotated.<locals>.inner"),
            d(u, 30, "MUT201", "closure_target"),
            // Two bindings of the cell, both a namespace of a `Node`.
            d(u, 39, "MUT201", "two_bindings.<locals>.inner"),
            // Bindings of different objects, a nested scope rebinding it (`nonlocal`), and another
            // dict keep nothing; the namespace of an object of unknown class is an unresolved
            // write, as it is directly.
            d(u, 82, "MUT205", "unknown.<locals>.inner"),
        ]
    );
    assert!(
        message_at(&diags, u, 13).contains(
            "writes `pkg.model.Node._mark` (as a `__dict__` item), a `__dict__` key no target names"
        ),
        "{diags:#?}"
    );
    assert!(
        message_at(&diags, u, 30).contains("writes `pkg.model.Node._cache` (as a `__dict__` item)"),
        "{diags:#?}"
    );
}
