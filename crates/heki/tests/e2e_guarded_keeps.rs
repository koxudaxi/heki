//! Stores a function makes only when tests of its parameters pass (`fresh-objects = "allow"`),
//! end to end through the real binary: a call whose arguments fail the tests keeps nothing, so
//! the object it returns, or the object it is handed, stays new; a test on the caller's own
//! parameter is the caller's callers' to decide.

mod common;

use common::Project;
use serde_json::Value;

/// A tree copy that registers each copy into an older object unless told not to, and the
/// functions using it.
const TREE: &str = r#"from __future__ import annotations

from copy import deepcopy


class Ref:
    def __init__(self) -> None:
        self.children: list[DT] = []


class DT:
    def __init__(self, ref: Ref | None = None) -> None:
        self.ref = ref
        self.is_list = False
        self.members: list[DT] = []

    def register(self) -> None:
        if self.ref:
            self.ref.children.append(self)

    def model_copy(self) -> DT:
        return deepcopy(self)


def copy_node(dt: DT) -> DT:
    copied = dt.model_copy()
    copied.members = []
    return copied


def copy_tree(dt: DT, *, register: bool = True) -> DT:
    copied = copy_node(dt)
    copied.members = [copy_tree(member, register=register) for member in dt.members]
    if register:
        copied.register()
    return copied


def copy_tree_plain(dt: DT) -> DT:
    copied = copy_node(dt)
    copied.members = [copy_tree_plain(member) for member in dt.members]
    return copied


def register_tree(dt: DT) -> None:
    for member in dt.members:
        register_tree(member)
    dt.register()


def merge(new: DT, flag: bool) -> None:
    new.is_list = flag


def a_flag_false_then_register(dt: DT) -> DT:
    copied = copy_tree(dt, register=False)
    merge(copied, flag=True)
    register_tree(copied)
    return copied


def b_plain_then_register(dt: DT) -> DT:
    copied = copy_tree_plain(dt)
    merge(copied, flag=True)
    register_tree(copied)
    return copied


def c_flag_true(dt: DT) -> DT:
    copied = copy_tree(dt)
    merge(copied, flag=True)
    return copied


def e_plain_direct(dt: DT) -> DT:
    copied = copy_tree_plain(dt)
    copied.is_list = True
    register_tree(copied)
    return copied
"#;

const TREE_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "tree-passes"
scopes = ["pkg.core:a_*", "pkg.core:b_*", "pkg.core:c_*", "pkg.core:e_*"]
write-universe = ["DT.is_list"]
allow-writes = []
"#;

/// Helpers that keep what they make or what they are handed only under tests of their
/// parameters.
const MODEL: &str = r#"from __future__ import annotations

import copy


class Node:
    def __init__(self) -> None:
        self.flag = False
        self.parent: Node | None = None
        self.children: list[Node] = []

    def adopt(self) -> None:
        if self.parent is not None:
            self.parent.children.append(self)


KEPT: list[Node] = []


def copy_node(node: Node) -> Node:
    return copy.deepcopy(node)


def copy_registered(node: Node, *, register: bool = True) -> Node:
    copied = copy_node(node)
    if register:
        copied.adopt()
    return copied


def copy_quietly(node: Node, *, register: bool = False) -> Node:
    copied = copy_node(node)
    if register:
        KEPT.append(copied)
    return copied


def copy_into(node: Node, registry: list[Node] | None = None) -> Node:
    copied = copy_node(node)
    if registry is not None:
        registry.append(copied)
    return copied


def copy_kept_twice(node: Node, *, first: bool = False, second: bool = False) -> Node:
    copied = copy_node(node)
    if first:
        KEPT.append(copied)
    if second:
        copied.adopt()
    return copied


def copy_relayed(node: Node, *, register: bool = True) -> Node:
    return copy_registered(node, register=register)


def attach(node: Node, *, keep: bool = True) -> None:
    if keep:
        KEPT.append(node)


def attach_relayed(node: Node, keep: bool) -> None:
    attach(node, keep=keep)


def set_flag(node: Node) -> None:
    node.flag = True
"#;

/// The scope functions: those whose names end in `_kept` are reported at their `set_flag` call.
const PASSES: &str = r#"from __future__ import annotations

from pkg.model import (
    Node,
    attach,
    attach_relayed,
    copy_into,
    copy_kept_twice,
    copy_node,
    copy_quietly,
    copy_registered,
    copy_relayed,
    set_flag,
)


def flag_false(node: Node) -> Node:
    copied = copy_registered(node, register=False)
    set_flag(copied)
    return copied


def flag_default_kept(node: Node) -> Node:
    copied = copy_registered(node)
    set_flag(copied)
    return copied


def flag_true_kept(node: Node) -> Node:
    copied = copy_registered(node, register=True)
    set_flag(copied)
    return copied


def flag_of_the_caller_kept(node: Node, register: bool) -> Node:
    copied = copy_registered(node, register=register)
    set_flag(copied)
    return copied


def flag_of_the_caller_tested_first(node: Node, register: bool) -> Node | None:
    if not register:
        copied = copy_registered(node, register=register)
        set_flag(copied)
        return copied
    return None


def flag_of_a_variable_kept(node: Node) -> Node:
    register = bool(node.children)
    copied = copy_registered(node, register=register)
    set_flag(copied)
    return copied


def default_false(node: Node) -> Node:
    copied = copy_quietly(node)
    set_flag(copied)
    return copied


def default_false_passed_true_kept(node: Node) -> Node:
    copied = copy_quietly(node, register=True)
    set_flag(copied)
    return copied


def registry_left_out(node: Node) -> Node:
    copied = copy_into(node)
    set_flag(copied)
    return copied


def registry_none(node: Node) -> Node:
    copied = copy_into(node, None)
    set_flag(copied)
    return copied


def registry_passed_kept(node: Node, items: list[Node]) -> Node:
    copied = copy_into(node, items)
    set_flag(copied)
    return copied


def neither_store(node: Node) -> Node:
    copied = copy_kept_twice(node)
    set_flag(copied)
    return copied


def first_store_kept(node: Node) -> Node:
    copied = copy_kept_twice(node, first=True)
    set_flag(copied)
    return copied


def second_store_kept(node: Node) -> Node:
    copied = copy_kept_twice(node, second=True)
    set_flag(copied)
    return copied


def relayed_false(node: Node) -> Node:
    copied = copy_relayed(node, register=False)
    set_flag(copied)
    return copied


def relayed_default_kept(node: Node) -> Node:
    copied = copy_relayed(node)
    set_flag(copied)
    return copied


def attach_without_keeping(node: Node) -> Node:
    copied = copy_node(node)
    attach(copied, keep=False)
    set_flag(copied)
    return copied


def attach_keeping_kept(node: Node) -> Node:
    copied = copy_node(node)
    attach(copied)
    set_flag(copied)
    return copied


def attach_relayed_without_keeping(node: Node) -> Node:
    copied = copy_node(node)
    attach_relayed(copied, False)
    set_flag(copied)
    return copied


def attach_relayed_keeping_kept(node: Node, keep: bool) -> Node:
    copied = copy_node(node)
    attach_relayed(copied, keep)
    set_flag(copied)
    return copied
"#;

const CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "no-node-writes"
scopes = ["pkg.passes:*"]
write-universe = ["Node.flag"]
"#;

fn diagnostics(p: &Project) -> Vec<Value> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

/// `(qualname, line)` of every report in `path`.
fn reports(diags: &[Value], path: &str) -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = diags
        .iter()
        .filter(|d| d["path"] == path)
        .map(|d| {
            (
                d["qualname"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
            )
        })
        .collect();
    out.sort();
    out
}

/// The line of the first line of `text` holding `needle` after the definition of `func`.
fn line_of(source: &str, func: &str, needle: &str) -> u64 {
    let lines: Vec<&str> = source.lines().collect();
    let def = lines
        .iter()
        .position(|l| l.starts_with(&format!("def {func}(")))
        .unwrap_or_else(|| panic!("no {func}"));
    let at = (lines[def..].iter())
        .position(|l| l.contains(needle))
        .unwrap_or_else(|| panic!("no {needle} in {func}"));
    u64::try_from(def + at + 1).unwrap()
}

#[test]
fn a_copy_made_with_its_registration_turned_off_stays_new() {
    let p = Project::new("guarded_keeps_tree");
    p.write("pyproject.toml", TREE_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/core.py", TREE);
    let found = reports(&diagnostics(&p), "pkg/core.py");
    let at = |func: &str, text: &str| (func.to_owned(), line_of(TREE, func, text));
    // The copy is registered before the write only where the call leaves `register` true.
    assert_eq!(found, vec![at("c_flag_true", "merge(copied")]);
}

#[test]
fn a_call_failing_the_tests_of_a_store_keeps_nothing() {
    let p = Project::new("guarded_keeps");
    p.write("pyproject.toml", CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", MODEL)
        .write("pkg/passes.py", PASSES);
    let found = reports(&diagnostics(&p), "pkg/passes.py");
    let mut expected: Vec<(String, u64)> = PASSES
        .lines()
        .filter_map(|l| l.strip_prefix("def ")?.split_once('('))
        .map(|(func, _)| func)
        .filter(|func| func.ends_with("_kept"))
        .map(|func| (func.to_owned(), line_of(PASSES, func, "set_flag(copied)")))
        .collect();
    expected.sort();
    assert_eq!(found, expected);
}
