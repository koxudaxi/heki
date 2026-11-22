//! `MUT115` (`shallow-copy-nested-mutation`) end to end: its exact claim (the function changes in
//! place an object below a shallow copy it made of state that outlives the call, which the
//! copied value held there), one counterexample per premise of that claim, the shapes that are
//! no report at all, and the changes made through helpers.

mod common;

use common::Project;
use serde_json::Value;

fn diagnostics(p: &Project) -> Vec<Value> {
    let (code, stdout, stderr) = p.check(&["--select", "MUT115", "--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

/// `(line, exact)` of every `MUT115` report of `path`: whether its message states the exact
/// claim ("shares") or the heuristic one ("may share").
fn claims(diags: &[Value], path: &str) -> Vec<(u64, bool)> {
    diags
        .iter()
        .filter(|d| d["rule_code"] == "MUT115" && d["path"] == path)
        .map(|d| {
            let message = d["message"].as_str().unwrap();
            assert!(message.contains("shallow copy"), "{message}");
            (d["line"].as_u64().unwrap(), !message.contains("may share"))
        })
        .collect()
}

/// The shapes of datamodel-code-generator's issue (a `model_copy()` sharing the field's
/// aliases and data type), and its correct code, each a function of its own.
const COPIES: &str = r#"from __future__ import annotations

import copy

from pydantic import BaseModel


class DataType(BaseModel):
    type: str
    data_types: list[DataType] = []


class Field(BaseModel):
    name: str
    data_type: DataType
    validation_aliases: list[str] = []


class Node:
    def __init__(self) -> None:
        self.children: list[Node] = []


def with_alias(field: Field, alias: str) -> Field:
    copied = field.model_copy()
    copied.validation_aliases.append(alias)
    return copied


def retyped(field: Field) -> Field:
    copied = field.model_copy()
    copied.data_type.type = "int"
    return copied


def with_base(options: dict[str, list[str]], base: str) -> dict[str, list[str]]:
    result = options.copy()
    result["bases"].append(base)
    return result


def deeper(layers: dict[str, dict[str, int]]) -> dict[str, dict[str, int]]:
    result = dict(layers)
    result["top"]["depth"] += 1
    return result


def adopted(node: Node, child: Node) -> Node:
    clone = copy.copy(node)
    clone.children.append(child)
    return clone


def rebound(field: Field, alias: str) -> Field:
    copied = field.model_copy()
    copied.validation_aliases = [*field.validation_aliases, alias]
    copied.validation_aliases.append("again")
    return copied


def updated(field: Field, alias: str) -> Field:
    copied = field.model_copy(update={"validation_aliases": [*field.validation_aliases, alias]})
    copied.validation_aliases.append("again")
    return copied


def deep(field: Field, alias: str) -> Field:
    copied = field.model_copy(deep=True)
    copied.validation_aliases.append(alias)
    return copied


def marked(field: Field) -> Field:
    copied = field.model_copy()
    copied.__dict__["_marker"] = True
    return copied


def top_level(options: dict[str, list[str]]) -> dict[str, list[str]]:
    result = options.copy()
    result["bases"] = [*result["bases"], "Extra"]
    result["other"] = []
    return result


def replaced_entries(properties: dict[str, Field], alias: str) -> dict[str, Field]:
    copied = properties.copy()
    for name, value in properties.items():
        copied[name] = value.model_copy(update={"validation_aliases": [alias]})
    return copied


def counted(totals: dict[str, int]) -> dict[str, int]:
    counts = dict(totals)
    counts["seen"] += 1
    return counts
"#;

#[test]
fn mut115_states_its_claim_on_the_issue_shapes_and_leaves_the_correct_code_alone() {
    let p = Project::new("mut115_issue_shapes");
    p.write("pkg/__init__.py", "")
        .write("pkg/copies.py", COPIES);
    let diags = diagnostics(&p);
    assert_eq!(
        claims(&diags, "pkg/copies.py"),
        vec![(26, true), (32, true), (38, true), (44, true), (50, true)],
        "{diags:#?}"
    );
    let message = diags[0]["message"].as_str().unwrap();
    assert_eq!(
        message,
        "`with_alias` changes in place an object that its shallow copy of `field` (`model_copy()`, line 25) shares with `field`"
    );
    // The copy call is a related location.
    assert_eq!(diags[0]["related"][0]["line"], 25, "{:#}", diags[0]);
}

/// More copies, all exact: the constructors, the unpacking displays, a slice, `get`, items
/// below items, a copy of an attribute of `self`, of a global, and of an item of a parameter.
const MORE_COPIES: &str = r#"from typing import Any

DEFAULTS: dict[str, list[str]] = {"tags": []}


class Holder:
    def __init__(self) -> None:
        self.options: dict[str, list[str]] = {}

    def extended(self, tag: str) -> dict[str, list[str]]:
        result = dict(self.options)
        result["tags"].append(tag)
        return result


def listed(rows: list[list[int]]) -> list[list[int]]:
    copied = list(rows)
    copied[0].append(1)
    return copied


def unpacked(options: dict[str, list[str]]) -> dict[str, list[str]]:
    merged = {**options}
    merged["tags"].clear()
    return merged


def starred(rows: list[list[int]]) -> list[list[int]]:
    copied = [*rows]
    copied[0].extend([1])
    return copied


def sliced(rows: list[list[int]]) -> list[list[int]]:
    copied = rows[:]
    copied[-1].pop()
    return copied


def got(options: dict[str, list[str]]) -> None:
    copied = options.copy()
    copied.get("tags").append("x")


def nested(tree: dict[str, dict[str, list[int]]]) -> None:
    copied = tree.copy()
    copied["a"]["b"].append(1)


def from_global() -> dict[str, list[str]]:
    copied = DEFAULTS.copy()
    copied["tags"].append("x")
    return copied


def of_an_item(groups: list[dict[str, list[str]]]) -> dict[str, list[str]]:
    copied = groups[0].copy()
    copied["tags"].append("x")
    return copied


def in_place_operator(options: dict[str, list[str]]) -> None:
    copied = dict(options)
    copied["tags"] += ["x"]
"#;

#[test]
fn mut115_copies_the_builtin_containers_and_reads_their_items_exactly() {
    let p = Project::new("mut115_more_copies");
    p.write("pkg/__init__.py", "")
        .write("pkg/more.py", MORE_COPIES);
    let diags = diagnostics(&p);
    assert_eq!(
        claims(&diags, "pkg/more.py"),
        vec![
            (12, true),
            (18, true),
            (24, true),
            (30, true),
            (36, true),
            (42, true),
            (47, true),
            (52, true),
            (58, true),
            (64, true),
        ],
        "{diags:#?}"
    );
}

/// One counterexample per premise of the exact claim: each function keeps every premise but
/// one, and its report is heuristic.
const STRIPPED: &str = r#"import copy
import dataclasses
from dataclasses import dataclass


class Copied:
    def __init__(self) -> None:
        self.children: list[int] = []

    def __copy__(self):
        new = Copied()
        new.children = list(self.children)
        return new


class Guarded:
    def __init__(self) -> None:
        self.children: list[int] = []
        self._name = ""

    @property
    def name(self) -> str:
        return self._name

    @name.setter
    def name(self, value: str) -> None:
        self._name = value


class Configured:
    @property
    def options(self) -> dict[str, list[str]]:
        return {"tags": []}


@dataclass
class Point:
    tags: list[str]


def add(rows: list[int]) -> None:
    rows.append(1)


def rebuild(options):
    options["tags"] = []


# Shallow copy: the class copies itself.
def own_copy_hook(node: Copied) -> None:
    clone = copy.copy(node)
    clone.children.append(1)


# Shallow copy: a copy heki does not state level by level.
def replaced(point: Point) -> None:
    moved = dataclasses.replace(point)
    moved.tags.append("x")


# Borrowed original: the copied value is the function's own on one path.
def own_on_one_path(options: dict[str, list[str]], flag: bool) -> None:
    source = options if flag else {"tags": []}
    copied = source.copy()
    copied["tags"].append("x")


# Borrowed original: a property's value may be a new object.
def property_value(holder: Configured) -> None:
    copied = holder.options.copy()
    copied["tags"].append("x")


# Still shared: rebound on one branch only.
def rebound_on_one_branch(options: dict[str, list[str]], flag: bool) -> None:
    copied = options.copy()
    if flag:
        copied["tags"] = []
    copied["tags"].append("x")


# Still shared: the copy reaches a helper first.
def handed_first(options: dict[str, list[str]]) -> None:
    copied = options.copy()
    print(copied)
    rebuild(copied)
    copied["tags"].append("x")


# Changed below the copy: an attribute store may run a property's setter.
def property_store(guards: dict[str, Guarded]) -> None:
    copied = guards.copy()
    copied["a"].name = "x"


# Changed below the copy: a helper the object is handed to makes the change.
def through_a_helper(options: dict[str, list[int]]) -> None:
    copied = options.copy()
    add(copied["a"])
"#;

/// The counterexamples a project subclass of a builtin container makes, each in a project of
/// its own: one replacing the copying method (Shallow copy), one replacing the changing method
/// (Changed below the copy), as a value annotated with the builtin may be one (R2).
const COPYING_SUBCLASS: &str = r#"class Counting(dict):
    def copy(self):
        return {k: list(v) for k, v in self.items()}


def subclass_copy(options: dict[str, list[str]]) -> None:
    copied = options.copy()
    copied["tags"].append("x")
"#;

const CHANGING_SUBCLASS: &str = r#"class Quiet(list):
    def append(self, x):
        pass


def subclass_change(groups: dict[str, list[int]]) -> None:
    copied = groups.copy()
    copied["a"].append(1)
"#;

#[test]
fn mut115_leaves_open_each_premise_heki_cannot_establish() {
    let p = Project::new("mut115_stripped");
    p.write("pkg/__init__.py", "")
        .write("pkg/stripped.py", STRIPPED);
    let diags = diagnostics(&p);
    assert_eq!(
        claims(&diags, "pkg/stripped.py"),
        vec![
            (52, false),
            (58, false),
            (65, false),
            (71, false),
            (79, false),
            (87, false),
            (93, false),
            (99, false),
        ],
        "{diags:#?}"
    );
    for (name, source, line) in [
        ("mut115_copying_subclass", COPYING_SUBCLASS, 8),
        ("mut115_changing_subclass", CHANGING_SUBCLASS, 8),
    ] {
        let p = Project::new(name);
        p.write("pkg/__init__.py", "").write("pkg/sub.py", source);
        let diags = diagnostics(&p);
        assert_eq!(
            claims(&diags, "pkg/sub.py"),
            vec![(line, false)],
            "{diags:#?}"
        );
    }
}

/// What is no report at all: a change of the copy itself, a copy of a value the function made,
/// a deep copy, the elements of an iteration over the copy, a level rebound before the change
/// on every path, and a memo filled on the shared object.
const NO_REPORT: &str = r#"import copy


def own_copy(options: dict[str, list[str]]) -> None:
    copied = options.copy()
    copied["tags"] = []
    copied.clear()


def made_here() -> None:
    options = {"tags": []}
    copied = options.copy()
    copied["tags"].append("x")


def deep_copy(options: dict[str, list[str]]) -> None:
    copied = copy.deepcopy(options)
    copied["tags"].append("x")


def iterated(rows: list[list[int]]) -> None:
    for row in list(rows):
        row.append(1)


def rebound_first(options: dict[str, list[str]]) -> None:
    copied = options.copy()
    copied["tags"] = list(copied["tags"])
    copied["tags"].append("x")


class Box:
    def __init__(self) -> None:
        self._cache: dict[str, int] = {}


def memo(boxes: dict[str, Box]) -> None:
    copied = boxes.copy()
    copied["a"]._cache["k"] = 1
"#;

#[test]
fn mut115_does_not_report_the_copys_own_changes_deep_copies_or_iterated_copies() {
    let p = Project::new("mut115_no_report");
    p.write("pkg/__init__.py", "")
        .write("pkg/none.py", NO_REPORT);
    let diags = diagnostics(&p);
    assert_eq!(claims(&diags, "pkg/none.py"), vec![], "{diags:#?}");
}

/// A helper changing what its parameter holds, handed the copy itself, and one changing its
/// parameter, handed an object below the copy: heuristic reports at the call, with the helper
/// as related location.
const HELPERS: &str = r#"def tag(options):
    options["tags"].append("x")


def grow(rows):
    rows.append(1)


def rename(options):
    options["name"] = "x"


def handed_the_copy(options: dict[str, list[str]]) -> None:
    copied = dict(options)
    tag(copied)


def handed_a_part(options: dict[str, list[int]]) -> None:
    copied = dict(options)
    grow(copied["rows"])


def handed_the_copy_to_rename(options: dict[str, str]) -> None:
    copied = dict(options)
    rename(copied)
"#;

#[test]
fn mut115_follows_helpers_handed_the_copy_or_a_part_of_it() {
    let p = Project::new("mut115_helpers");
    p.write("pkg/__init__.py", "")
        .write("pkg/helpers.py", HELPERS);
    let diags = diagnostics(&p);
    assert_eq!(
        claims(&diags, "pkg/helpers.py"),
        vec![(15, false), (20, false)],
        "{diags:#?}"
    );
    let hop = &diags[0]["related"][1];
    assert_eq!(hop["line"], 15, "{:#}", diags[0]);
}

#[test]
fn mut115_runs_only_when_selected() {
    let p = Project::new("mut115_selection");
    p.write("pkg/__init__.py", "")
        .write("pkg/copies.py", COPIES);
    let (_, stdout, _) = p.heki(&["check", "--heuristic", "--output-format", "json", "."]);
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    assert!(
        diags.iter().all(|d| d["rule_code"] != "MUT115"),
        "{diags:#?}"
    );
    let (_, stdout, _) = p.heki(&[
        "check",
        "--extend-select",
        "MUT115",
        "--output-format",
        "json",
        ".",
    ]);
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        diags.iter().filter(|d| d["rule_code"] == "MUT115").count(),
        5
    );
}

/// Every form of copy, of load from a copy, of use of a copy, and of change the rule reads, each
/// in a function of its own, with the tier its report states (or no report).
const FORMS: &str = r#"import copy
import dataclasses
from collections import deque
from dataclasses import dataclass

from pydantic import BaseModel


class Node:
    def __init__(self) -> None:
        self.children: list[int] = []
        self.name = ""
        self.label = ""


class Box:
    def __init__(self) -> None:
        self.items: list[int] = []

    @property
    def computed(self) -> list[int]:
        return list(self.items)


class Bag:
    def append(self, item: int) -> None:
        pass


class Holder:
    def __init__(self) -> None:
        self.options: dict[str, list[str]] = {}


class Field(BaseModel):
    validation_aliases: list[str] = []


@dataclass
class Point:
    tags: list[str]


def untyped_model(m) -> None:
    copied = m.model_copy()
    copied.items.append(1)


def dunder(node: Node) -> None:
    clone = node.__copy__()
    clone.children.append(1)


def queued(rows: list[list[int]]) -> None:
    copied = deque(rows)
    copied[0].append(1)


def splatted_replace(point: Point, changes) -> None:
    moved = dataclasses.replace(point, **changes)
    moved.tags.append("x")


def computed_update(field: Field, changes) -> None:
    copied = field.model_copy(update=changes)
    copied.validation_aliases.append("x")


def splat_update(field: Field, changes) -> None:
    copied = field.model_copy(update={**changes})
    copied.validation_aliases.append("x")


def named_replace(point: Point) -> None:
    moved = dataclasses.replace(point, tags=[])
    moved.tags.append("x")


def got_holder(groups: dict[str, dict[str, list[int]]]) -> None:
    copied = groups.get("a").copy()
    copied["b"].append(1)


def below_get(tree: dict[str, dict[str, list[int]]]) -> None:
    copied = tree.copy()
    copied["a"].get("b").append(1)


def below_attr(boxes: dict[str, Box]) -> None:
    copied = boxes.copy()
    copied["a"].items.append(1)


def below_property(boxes: dict[str, Box]) -> None:
    copied = boxes.copy()
    copied["a"].computed.append(1)


def popped(options: dict[str, list[str]]) -> None:
    copied = options.copy()
    copied.pop("tags").append("x")


def either(tree: dict[str, dict[str, list[int]]], flag: bool) -> None:
    copied = tree.copy()
    inner = copied["a"] if flag else copied["a"]
    inner["b"].append(1)


def kept_in_loop(options: dict[str, list[str]], xs: list[str]) -> None:
    copied = options.copy()
    tags = copied["tags"]
    for x in xs:
        tags.append(x)
        tags = tags


def phi_copy(options: dict[str, list[str]], flag: bool) -> None:
    copied = options.copy()
    if flag:
        copied = {"tags": []}
    copied["tags"].append("x")


def unknown_key(options: dict[str, list[str]], k: str) -> None:
    copied = options.copy()
    copied[k] = []
    copied["tags"].append("x")


def renamed(node: Node) -> None:
    clone = copy.copy(node)
    clone.name = "x"
    del clone.label
    clone.children.append(1)


def printed(node: Node) -> None:
    clone = copy.copy(node)
    print(clone)
    clone.children.append(1)


def tested(node: Node) -> None:
    clone = copy.copy(node)
    if clone:
        clone.children.append(1)


def uses(options: dict[str, list[str]], other: dict[str, list[str]]) -> None:
    copied = options.copy()
    del copied["other"]
    merged = copied | other
    same = copied == other
    empty = not copied
    copied["tags"].append("x")
    print(merged, same, empty)


def holder_list(options: dict[str, list[str]]) -> None:
    copied = options.copy()
    holder = [copied]
    copied["tags"].append("x")
    print(holder)


def untyped_items(options: dict) -> None:
    copied = options.copy()
    copied["tags"].append("x")


def deleted(tree: dict[str, dict[str, int]]) -> None:
    copied = tree.copy()
    del copied["a"]["b"]


def bagged(bags: dict[str, Bag]) -> None:
    copied = bags.copy()
    copied["a"].append(1)


def undone(boxes: dict[str, Box]) -> None:
    copied = boxes.copy()
    del copied["a"].items


def nested_holder(holders: list[Holder]) -> None:
    copied = holders[0].options.copy()
    copied["tags"].append("x")


def augmented_attr(node: Node) -> None:
    clone = copy.copy(node)
    clone.children += [1]


def tupled(rows: list[list[int]]) -> None:
    copied = (*rows,)
    copied[0].append(1)


def branched(options: dict[str, list[str]]) -> None:
    copied = options.copy()
    if copied:
        copied["tags"].append("x")


def rebound_on_one_path(options: dict[str, list[str]], flag: bool) -> None:
    copied = options.copy()
    if flag:
        copied["tags"] = []
        tags = copied["tags"]
    else:
        tags = copied["tags"]
    tags.append("x")


def copied_dict(groups: dict[str, list[int]]) -> None:
    clone = copy.copy(groups)
    clone["a"].append(1)


def annotated(node: Node) -> None:
    clone: Node = copy.copy(node)
    clone.children.append(1)


def property_of_copy(box: Box) -> None:
    clone = copy.copy(box)
    clone.computed.append(1)


def model_level(field: Field) -> None:
    copied = field.model_copy()
    copied.validation_aliases.append("x")


class Table:
    def __init__(self) -> None:
        self.rows: dict[str, list[int]] = {}

    def __getitem__(self, key: str) -> list[int]:
        return self.rows[key]


def table_item(table: Table) -> None:
    clone = copy.copy(table)
    clone["a"].append(1)


def copy_in_phi(options: dict[str, list[str]], other: dict[str, list[str]], flag: bool) -> None:
    copied = options.copy()
    tags = copied["tags"]
    if flag:
        copied = other
    tags.append("x")
    print(copied)
"#;

#[test]
fn mut115_reads_every_form_of_copy_load_use_and_change() {
    let p = Project::new("mut115_forms");
    p.write("pkg/__init__.py", "").write("pkg/forms.py", FORMS);
    let diags = diagnostics(&p);
    let source: Vec<&str> = FORMS.lines().collect();
    let function_at = |line: u64| {
        source[..line as usize]
            .iter()
            .rev()
            .find_map(|l| l.strip_prefix("def "))
            .and_then(|l| l.split('(').next())
            .unwrap()
            .to_owned()
    };
    let found: Vec<(String, bool)> = claims(&diags, "pkg/forms.py")
        .into_iter()
        .map(|(line, exact)| (function_at(line), exact))
        .collect();
    let expected: Vec<(String, bool)> = [
        // Copies heki does not state level by level: a library's `model_copy`, a copy hook
        // called by name, a catalog copy, `**` keywords, an `update` heki cannot read.
        ("untyped_model", false),
        ("dunder", false),
        ("queued", false),
        ("splatted_replace", false),
        ("computed_update", false),
        ("splat_update", false),
        // `got_holder`: the copied value read with `get`; then `get` and attributes below.
        ("got_holder", true),
        ("below_get", true),
        ("below_attr", true),
        ("below_property", false),
        ("popped", false),
        ("either", false),
        ("kept_in_loop", true),
        ("phi_copy", false),
        ("unknown_key", false),
        ("renamed", true),
        ("printed", false),
        ("tested", false),
        ("uses", true),
        ("holder_list", false),
        ("untyped_items", false),
        ("deleted", true),
        ("bagged", false),
        ("undone", true),
        ("nested_holder", true),
        ("augmented_attr", true),
        ("tupled", true),
        ("branched", true),
        // One path stores a list of the copy's own under the key before reading it.
        ("rebound_on_one_path", false),
        // heki states what `copy.copy` keeps attribute by attribute only, so an item stays
        // heuristic.
        ("copied_dict", false),
        ("annotated", true),
        // A property of the copy may compute a new object.
        ("property_of_copy", false),
        ("model_level", true),
        // An item of an instance is what its `__getitem__` returns.
        ("table_item", false),
        // A copy that flows on with another value is not followed past the merge.
        ("copy_in_phi", false),
    ]
    .into_iter()
    .map(|(f, exact)| (f.to_owned(), exact))
    .collect();
    assert_eq!(found, expected, "{diags:#?}");
    let nested = diags
        .iter()
        .find(|d| {
            d["message"]
                .as_str()
                .unwrap()
                .starts_with("`nested_holder`")
        })
        .unwrap();
    assert!(
        nested["message"]
            .as_str()
            .unwrap()
            .contains("shallow copy of the copied value"),
        "{nested:#}"
    );
}
