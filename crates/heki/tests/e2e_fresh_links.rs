//! New objects stored in one another (`fresh-objects = "allow"`), end to end through the real
//! binary: an attribute store of a new object into another new object links the two, and the
//! group stays new until one of its objects, or a value read out of one, is kept. A store into
//! an object the function did not make still ends the freshness of what it stores, and so does
//! keeping a value read out of an object that may hold the one written, in the function or out
//! of a project function's result that holds itself.

mod common;

use common::Project;
use serde_json::Value;

const MODEL: &str = r#"class Node:
    def __init__(self) -> None:
        self.flag = False
        self.parent: "Node | None" = None
        self.children: list["Node"] = []


OLD: list[object] = []
SHARED = Node()


def old_node() -> Node:
    return SHARED


def make_linked() -> Node:
    copied, part = Node(), Node()
    copied.parent = part
    part.parent = copied
    return copied


def make_plain() -> Node:
    copied, part = Node(), Node()
    copied.parent = part
    return copied


def make_relayed() -> Node:
    made = make_linked()
    return made


def remember(node: Node) -> None:
    OLD.append(node)


def publish_parent(part: Node) -> None:
    OLD.append(part.parent)


def parent_of(part: Node) -> "Node | None":
    return part.parent
"#;

/// The scope functions; the expected reports are in the test.
const PASSES: &str = r#"from pkg.model import (
    OLD,
    Node,
    make_linked,
    make_plain,
    make_relayed,
    old_node,
    parent_of,
    publish_parent,
    remember,
)


def links_two_new_objects(node: Node) -> Node:
    copied, part = Node(), Node()
    part.parent = copied
    copied.flag = True
    return copied


def links_on_separate_lines(node: Node) -> Node:
    copied = Node()
    part = Node()
    part.parent = copied
    copied.parent = part
    copied.flag = True
    part.flag = True
    return copied


def unpacks_new_objects(node: Node) -> Node:
    copied, part = Node(), Node()
    copied.flag = True
    return copied


def reads_a_factory_result_that_holds_only_others(node: Node) -> Node:
    copied = make_plain()
    OLD.append(copied.parent)
    copied.flag = True
    return copied


def keeps_an_older_object_before_writing(node: Node) -> Node:
    copied = make_linked()
    OLD.append(node.parent)
    copied.flag = True
    return copied


def links_into_a_new_factory_result(node: Node) -> Node:
    copied = Node()
    part = make_plain()
    part.parent = copied
    copied.flag = True
    return copied


def keeps_a_test_of_a_value_read_back(node: Node) -> Node:
    copied, part = Node(), Node()
    part.parent = copied
    OLD.append(part.parent is None)
    copied.flag = True
    return copied


def links_the_input_into_a_new_object(node: Node) -> Node:
    copied = Node()
    copied.parent = node
    copied.flag = True
    return copied


def unpacks_beside_another_display(node: Node) -> Node:
    copied, part = Node(), Node()
    part.children = [part]
    copied.flag = True
    return copied


def links_and_loops(node: Node) -> None:
    for _ in range(3):
        a = Node()
        b = Node()
        a.parent = b
        b.flag = True


def links_into_an_older_object(node: Node) -> Node:
    copied = Node()
    node.parent = copied
    copied.flag = True
    return copied


def links_into_an_older_object_a_function_returns(node: Node) -> Node:
    copied = Node()
    part = old_node()
    part.parent = copied
    copied.flag = True
    return copied


def keeps_the_unpacked_display(node: Node) -> Node:
    copied, part = pair = Node(), Node()
    OLD.append(pair)
    copied.flag = True
    return copied


def links_into_an_object_a_helper_keeps(node: Node) -> None:
    a = Node()
    remember(a)
    b = Node()
    a.parent = b
    b.flag = True


def links_into_a_result_of_its_own_recursion(n: int) -> Node:
    copied = Node()
    if n:
        part = links_into_a_result_of_its_own_recursion(n - 1)
    else:
        part = Node()
    part.parent = copied
    copied.flag = True
    return part


def keeps_the_holder(node: Node) -> Node:
    copied, part = Node(), Node()
    part.parent = copied
    OLD.append(part)
    copied.flag = True
    return copied


def keeps_a_value_read_back(node: Node) -> Node:
    copied, part = Node(), Node()
    part.parent = copied
    OLD.append(part.parent)
    copied.flag = True
    return copied


def keeps_a_value_read_back_twice(node: Node) -> Node:
    copied, part = Node(), Node()
    copied.parent = part
    part.parent = copied
    OLD.append(copied.parent.parent)
    copied.flag = True
    return copied


def keeps_a_value_read_far_down(node: Node) -> Node:
    copied, part = Node(), Node()
    part.parent = copied
    copied.parent = part
    OLD.append(part.parent.parent.parent.parent.parent.parent.parent.parent.parent.parent.parent.parent.parent.parent.parent.parent.parent)
    copied.flag = True
    return copied


def keeps_a_value_read_out_of_a_linked_result(node: Node) -> Node:
    copied = make_linked()
    OLD.append(copied.parent.parent)
    copied.flag = True
    return copied


def keeps_a_value_read_out_of_a_relayed_result(node: Node) -> Node:
    copied = make_relayed()
    OLD.append(copied.parent)
    copied.flag = True
    return copied


def hands_the_holder_to_a_publishing_helper(node: Node) -> Node:
    copied, part = Node(), Node()
    part.parent = copied
    publish_parent(part)
    copied.flag = True
    return copied


def keeps_what_a_helper_reads_back(node: Node) -> Node:
    copied, part = Node(), Node()
    part.parent = copied
    OLD.append(parent_of(part))
    copied.flag = True
    return copied


def links_after_the_holder_is_kept(node: Node) -> None:
    a = Node()
    node.parent = a
    b = Node()
    a.parent = b
    b.flag = True


def links_after_the_holder_is_kept_in_a_loop(node: Node) -> None:
    for _ in range(3):
        a = Node()
        node.parent = a
        b = Node()
        a.parent = b
        b.flag = True
"#;

const CONFIG: &str = r#"[project]
name = "t"
requires-python = ">=3.12"

[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "no-flag"
scopes = ["pkg.passes:*"]
forbid = ["writes-targets"]
write-targets = ["Node.flag"]
fresh-objects = "allow"
"#;

fn diagnostics(p: &Project) -> Vec<Value> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

/// `(qualname, line)` of every report in `pkg/passes.py`.
fn reports(diags: &[Value], text: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out: Vec<(String, String)> = diags
        .iter()
        .filter(|d| d["path"] == "pkg/passes.py")
        .map(|d| {
            let line = usize::try_from(d["line"].as_u64().unwrap()).unwrap();
            (
                d["qualname"].as_str().unwrap().to_owned(),
                lines[line - 1].trim().to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

#[test]
fn objects_linked_to_one_another_stay_new_until_one_is_kept() {
    let p = Project::new("fresh_links");
    p.write("pyproject.toml", CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", MODEL)
        .write("pkg/passes.py", PASSES);
    let found = reports(&diagnostics(&p), PASSES);
    let at = |func: &str, line: &str| (func.to_owned(), line.to_owned());
    let mut expected = vec![
        // The object stored is held by the input.
        at("links_into_an_older_object", "copied.flag = True"),
        at(
            "links_into_an_older_object_a_function_returns",
            "copied.flag = True",
        ),
        // The object holding it, or a value read out of it, is kept.
        at("keeps_the_holder", "copied.flag = True"),
        // The display holding the objects is kept itself.
        at("keeps_the_unpacked_display", "copied.flag = True"),
        // The holder is kept by a helper before the link.
        at("links_into_an_object_a_helper_keeps", "b.flag = True"),
        // The holder may be what the function itself returns, which is not known to be new
        // while the cycle is computed.
        at(
            "links_into_a_result_of_its_own_recursion",
            "copied.flag = True",
        ),
        at("keeps_a_value_read_back", "copied.flag = True"),
        at("keeps_a_value_read_back_twice", "copied.flag = True"),
        at("keeps_a_value_read_far_down", "copied.flag = True"),
        // Read out of a result that holds itself, directly or through a function returning it.
        at(
            "keeps_a_value_read_out_of_a_linked_result",
            "copied.flag = True",
        ),
        at(
            "keeps_a_value_read_out_of_a_relayed_result",
            "copied.flag = True",
        ),
        // A helper may keep what it reads out of the object it is handed, or hand it back.
        at(
            "hands_the_holder_to_a_publishing_helper",
            "copied.flag = True",
        ),
        at("keeps_what_a_helper_reads_back", "copied.flag = True"),
        // The holder is kept before the link, so it keeps the new object linked into it, also
        // when the loop makes a new object of the same allocation.
        at("links_after_the_holder_is_kept", "b.flag = True"),
        at("links_after_the_holder_is_kept_in_a_loop", "b.flag = True"),
    ];
    expected.sort();
    assert_eq!(found, expected);
}

/// A tree of pydantic models copied with the children's parent links set on request: the copy
/// with links is reported, since the copy function links the children it makes through a
/// comprehension calling itself, whose results are not known to be new; the field copied with
/// its new type is reported, since the copy helper stores the new type into its parameter.
const TREE_MODEL: &str = r#"from __future__ import annotations

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


class Builder:
    def record_nullable_without_default(self, field: Field, value: bool | None) -> None:
        field.__dict__["_nullable_without_default"] = value
"#;

const TREE_PASSES: &str = r#"from __future__ import annotations

from pkg.model import Builder, DataType, Field, copy_tree


def copy_without_parent_links(field: Field) -> DataType:
    copied = copy_tree(field.data_type, link_parents=False)
    copied.flag = True
    for node in copied.all_data_types:
        node.register_reference()
    return copied


def copy_with_parent_links(field: Field) -> DataType:
    copied = copy_tree(field.data_type, link_parents=True)
    copied.flag = True
    for node in copied.all_data_types:
        node.register_reference()
    return copied


def field_copy_linked_to_its_new_type(builder: Builder, field: Field) -> Field:
    copied_type = copy_tree(field.data_type, link_parents=False)
    copied_field = field.copy_owning(data_type=copied_type)
    builder.record_nullable_without_default(copied_field, None)
    return copied_field
"#;

const TREE_CONFIG: &str = r#"[project]
name = "t"
requires-python = ">=3.11"
dependencies = ["pydantic>=2"]

[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "pass"
scopes = ["pkg.passes:*"]
write-universe = ["DataType.flag", "DataType.data_types", "Field._nullable_without_default"]
allow-writes = []
fresh-objects = "allow"
"#;

#[test]
fn copies_of_a_pydantic_tree_with_parent_links() {
    let p = Project::new("fresh_links_tree");
    p.write("pyproject.toml", TREE_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", TREE_MODEL)
        .write("pkg/passes.py", TREE_PASSES);
    let found = reports(&diagnostics(&p), TREE_PASSES);
    let at = |func: &str, line: &str| (func.to_owned(), line.to_owned());
    assert_eq!(
        found,
        [
            at("copy_with_parent_links", "copied.flag = True"),
            at(
                "field_copy_linked_to_its_new_type",
                "builder.record_nullable_without_default(copied_field, None)"
            ),
        ]
    );
}

/// Pydantic models linked to one another: a write of a configured model may validate the value
/// (`validate_assignment`) and run the project's validators, which may keep it, so only a model
/// that no class it may be configures links. The configuration is in the class itself, a project
/// base, or a project subclass.
const PYDANTIC_MODEL: &str = r#"from typing import Any

from pydantic import BaseModel, ConfigDict

OPTIONS = {"validate_assignment": True}


class Plain(BaseModel):
    flag: bool = False
    parent: Any = None


class Configured(BaseModel):
    model_config = ConfigDict(validate_assignment=True)
    flag: bool = False
    parent: Any = None


class OldConfig(BaseModel):
    class Config:
        validate_assignment = True

    flag: bool = False
    parent: Any = None


class Keyworded(BaseModel, validate_assignment=True):
    flag: bool = False
    parent: Any = None


class Unpacked(BaseModel, **OPTIONS):
    flag: bool = False
    parent: Any = None


class ConfiguredBase(BaseModel):
    model_config = ConfigDict(validate_assignment=True)


class OldConfigBase(BaseModel):
    class Config:
        validate_assignment = True


class KeywordedBase(BaseModel, validate_assignment=True):
    pass


class FromConfiguredBase(ConfiguredBase):
    flag: bool = False
    parent: Any = None


class FromOldConfigBase(OldConfigBase):
    flag: bool = False
    parent: Any = None


class FromKeywordedBase(KeywordedBase):
    flag: bool = False
    parent: Any = None


class ConfiguredBelow(BaseModel):
    flag: bool = False
    parent: Any = None


class ConfiguredSubclass(ConfiguredBelow):
    model_config = ConfigDict(validate_assignment=True)


class OldConfigBelow(BaseModel):
    flag: bool = False
    parent: Any = None


class OldConfigSubclass(OldConfigBelow):
    class Config:
        validate_assignment = True


class KeywordedBelow(BaseModel):
    flag: bool = False
    parent: Any = None


class KeywordedSubclass(KeywordedBelow, validate_assignment=True):
    pass
"#;

/// One scope function per model class: `{class}` is replaced by its name.
const PYDANTIC_PASS: &str = r#"

def links_{class}() -> {class}:
    copied, part = {class}(), {class}()
    part.parent = copied
    copied.flag = True
    return copied
"#;

const PYDANTIC_CLASSES: &[&str] = &[
    "Plain",
    "Configured",
    "OldConfig",
    "Keyworded",
    "Unpacked",
    "FromConfiguredBase",
    "FromOldConfigBase",
    "FromKeywordedBase",
    "ConfiguredBelow",
    "OldConfigBelow",
    "KeywordedBelow",
];

#[test]
fn configured_pydantic_models_do_not_link() {
    let mut passes = format!("from pkg.model import {}\n", PYDANTIC_CLASSES.join(", "));
    for class in PYDANTIC_CLASSES {
        passes.push_str(&PYDANTIC_PASS.replace("{class}", class));
    }
    let universe = (PYDANTIC_CLASSES.iter())
        .map(|c| format!("\"{c}.flag\""))
        .collect::<Vec<_>>()
        .join(", ");
    let config = format!(
        r#"[project]
name = "t"
requires-python = ">=3.12"
dependencies = ["pydantic>=2"]

[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "pass"
scopes = ["pkg.passes:*"]
write-universe = [{universe}]
allow-writes = []
fresh-objects = "allow"
"#
    );
    let p = Project::new("fresh_links_pydantic");
    p.write("pyproject.toml", &config)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", PYDANTIC_MODEL)
        .write("pkg/passes.py", &passes);
    let found = reports(&diagnostics(&p), &passes);
    // Only the model no class configures links.
    let mut expected: Vec<(String, String)> = (PYDANTIC_CLASSES.iter())
        .filter(|c| **c != "Plain")
        .map(|c| (format!("links_{c}"), "copied.flag = True".to_owned()))
        .collect();
    expected.sort();
    assert_eq!(found, expected);
}
