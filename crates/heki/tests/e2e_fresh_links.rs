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
        self.count: int = 0

    def __neg__(self) -> "Node | None":
        return self.parent

    def __lt__(self, other: "Node") -> "Node | None":
        return self.parent


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


class Registry:
    def __getitem__(self, key: Node) -> Node:
        return key


REGISTRY = Registry()
INDEX: dict[Node, int] = {}
LAST: object = None


def _save(node: Node) -> None:
    global LAST
    kept, _ = (node, None)
    LAST = kept


def _unpack(node: Node) -> None:
    kept, rest = (node, None)
    print(rest)


class Recording(type):
    def __new__(mcs, name, bases, ns):
        def __setattr__(self, attr, value):
            OLD.append(value)
            object.__setattr__(self, attr, value)

        ns["__setattr__"] = __setattr__
        return super().__new__(mcs, name, bases, ns)


OPTIONS = {"metaclass": Recording}


class Holder(**{"metaclass": Recording}):
    def __init__(self) -> None:
        self.child: Node | None = None


class OptionsHolder(**OPTIONS):
    def __init__(self) -> None:
        self.child: Node | None = None


class MergedHolder(**{**OPTIONS}):
    def __init__(self) -> None:
        self.child: Node | None = None


class Tagging:
    def __init_subclass__(cls, tag: str = "", **kw: object) -> None:
        super().__init_subclass__(**kw)
        cls.tag = tag


class TaggedHolder(Tagging, **{"tag": "x"}):
    def __init__(self) -> None:
        self.child: Node | None = None


class EmptyHolder(**{}):
    def __init__(self) -> None:
        self.child: Node | None = None
"#;

/// The scope functions; the expected reports are in the test.
const PASSES: &str = r#"from pkg.model import (
    INDEX,
    OLD,
    REGISTRY,
    EmptyHolder,
    Holder,
    MergedHolder,
    Node,
    OptionsHolder,
    TaggedHolder,
    _save,
    _unpack,
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


def unpacks_a_starred_display(nodes: list[Node]) -> Node:
    first, second, third = *nodes, Node()
    second.flag = True
    return third


def keeps_what_an_operator_reads_back(node: Node) -> Node:
    copied, part = Node(), Node()
    part.parent = copied
    OLD.append(-part)
    copied.flag = True
    return copied


def keeps_what_a_comparison_reads_back(node: Node) -> Node:
    copied, part = Node(), Node()
    part.parent = copied
    OLD.append(part < node)
    copied.flag = True
    return copied


def keeps_an_operator_of_a_number_read_back(node: Node) -> Node:
    copied, part = Node(), Node()
    part.parent = copied
    OLD.append(-part.count)
    OLD.append(part.count < 3)
    copied.flag = True
    return copied


def keeps_a_negation_of_a_value_read_back(node: Node) -> Node:
    copied, part = Node(), Node()
    part.parent = copied
    OLD.append(not part.flag)
    copied.flag = True
    return copied


def keeps_a_string_of_a_value_read_back(node: Node) -> Node:
    copied, part = Node(), Node()
    part.parent = copied
    OLD.append(f"{part.parent!r}")
    OLD.append(f"parent: {part.parent}")
    copied.flag = True
    return copied


def keeps_what_a_custom_subscript_reads_back(node: Node) -> Node:
    copied, part = Node(), Node()
    part.parent = copied
    OLD.append(REGISTRY[part])
    copied.flag = True
    return copied


def keeps_items_of_builtin_dicts_keyed_by_the_holder(node: Node) -> Node:
    copied, part = Node(), Node()
    part.parent = copied
    OLD.append(INDEX[part])
    counts = dict(INDEX)
    OLD.append(counts[part])
    copied.flag = True
    return copied


def hands_to_a_helper_storing_an_unpacked_item(node: Node) -> Node:
    copied = Node()
    _save(copied)
    copied.flag = True
    return copied


def hands_to_a_helper_only_unpacking(node: Node) -> Node:
    copied = Node()
    _unpack(copied)
    copied.flag = True
    return copied


def links_into_a_holder_with_an_unpacked_metaclass(node: Node) -> Node:
    copied = Node()
    holder = Holder()
    holder.child = copied
    copied.flag = True
    return copied


def links_into_a_holder_with_unpacked_options(node: Node) -> Node:
    copied = Node()
    holder = OptionsHolder()
    holder.child = copied
    copied.flag = True
    return copied


def links_into_a_holder_with_merged_options(node: Node) -> Node:
    copied = Node()
    holder = MergedHolder()
    holder.child = copied
    copied.flag = True
    return copied


def links_into_holders_with_known_keywords(node: Node) -> Node:
    copied = Node()
    tagged = TaggedHolder()
    tagged.child = copied
    empty = EmptyHolder()
    empty.child = copied
    copied.flag = True
    return copied
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
        // A starred element shifts the items: `second` is one of `nodes`.
        at("unpacks_a_starred_display", "second.flag = True"),
        // A project class's operator may return what it reads out of its operand.
        at("keeps_what_an_operator_reads_back", "copied.flag = True"),
        at("keeps_what_a_comparison_reads_back", "copied.flag = True"),
        // A project class's `__getitem__` may return the key it is handed.
        at(
            "keeps_what_a_custom_subscript_reads_back",
            "copied.flag = True",
        ),
        // The helper stores the item it unpacks out of a display of its parameter.
        at(
            "hands_to_a_helper_storing_an_unpacked_item",
            "copied.flag = True",
        ),
        // A metaclass from an unpacked mapping, or from a mapping whose keys are not known,
        // may install a `__setattr__`.
        at(
            "links_into_a_holder_with_an_unpacked_metaclass",
            "copied.flag = True",
        ),
        at(
            "links_into_a_holder_with_unpacked_options",
            "copied.flag = True",
        ),
        at(
            "links_into_a_holder_with_merged_options",
            "copied.flag = True",
        ),
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

/// Containers a class's constructor makes for each instance: what a function stores into one,
/// read from an object it made by calling the class, is held by that object, so the issue's
/// `make` returns a new object. A shallow copy's containers are the original's; a container
/// rebound, or one the constructor does not make on every path or lets other code reach, may be
/// anyone's. Other code is reached by a call handed the instance or a member of it, a store
/// running a setter, and a base's constructor running either or replacing the namespace.
const OWNED_MODEL: &str = r#"import abc

OLD: list[object] = []
OLD_LIST: list["Node"] = []
SHARED_STATE: dict[str, object] = {}


class Node:
    def __init__(self) -> None:
        self.flag = False
        self.parent: "Node | None" = None
        self.children: list["Node"] = []
        self.kids: set["Node"] = set()
        self.table: dict[str, "Node"] = {}
        self.marks: set[str] = {"new"}


class Hashed:
    def __init__(self) -> None:
        self.flag = False

    def __hash__(self) -> int:
        OLD.append(self)
        return 0


def make() -> Node:
    copied, part = Node(), Node()
    part.parent = copied
    copied.children.append(part)
    return copied


def keep(kids: list[Node]) -> None:
    OLD.append(kids)


def reset(node: Node) -> None:
    node.children = OLD_LIST


class Child(Node):
    pass


class Prepared:
    def __init__(self, ready: bool = False) -> None:
        self.setup()
        state = self.flag if ready else None
        if state:
            print(state)
        kids = list()
        self.children: list[Node] = kids

    def setup(self) -> None:
        self.flag = False


class Hooked:
    def __init__(self) -> None:
        self.children: list[Node] = []

    def __setattr__(self, name: str, value: object) -> None:
        OLD.append(value)
        object.__setattr__(self, name, value)


class Viewed:
    def __init__(self) -> None:
        self._children: list[Node] = []

    @property
    def children(self) -> list[Node]:
        OLD.append(self._children)
        return self._children


class Registered:
    def __init__(self) -> None:
        self.children: list[Node] = []
        self.register()

    def register(self) -> None:
        OLD.append(self.children)


class Sometimes:
    def __init__(self, fresh: bool) -> None:
        if fresh:
            self.children: list[Node] = []
            return
        self.flag = False


class Shared:
    def __init__(self) -> None:
        kids: list[Node] = []
        self.children = kids
        OLD.append(kids)


def register(cls: type) -> type:
    return cls


@register
class Decorated:
    def __init__(self) -> None:
        self.children: list[Node] = []


def _shared_init(self: object) -> None:
    self.children = OLD_LIST


class Replaced:
    def __init__(self) -> None:
        self.children: list[Node] = []

    __init__ = _shared_init


class Announced:
    def __init__(self) -> None:
        self.children: list[Node] = []
        self.ready = True

    @property
    def ready(self) -> bool:
        return bool(self.children)

    @ready.setter
    def ready(self, value: bool) -> None:
        OLD.append(self.children)


class Base:
    def __init__(self, flag: bool) -> None:
        self.flag = flag


class Derived(Base):
    def __init__(self) -> None:
        super().__init__(False)
        self.children: list[Node] = []


class Resets:
    def __init__(self) -> None:
        self.children = OLD_LIST


class ResetLater(Resets):
    def __init__(self) -> None:
        self.children: list[Node] = []
        super().__init__()


class Monostate:
    def __init__(self) -> None:
        self.__dict__ = SHARED_STATE


class Config(Monostate):
    def __init__(self) -> None:
        super().__init__()
        self.children: list[Node] = []


class Abstract(abc.ABC):
    def __init__(self) -> None:
        super().__init__()
        self.children: list[Node] = []


class Failure(Exception):
    def __init__(self) -> None:
        super().__init__("failed")
        self.children: list[Node] = []


class Meta(type):
    pass


class Metered(metaclass=Meta):
    def __init__(self) -> None:
        self.children: list[Node] = []
"#;

/// A module binding `super` to what heki cannot tell is the builtin.
const OWNED_SHADOWED: &str = r#"import builtins

from pkg.model import Base, Node

super = getattr(builtins, "super")


class Shadowed(Base):
    def __init__(self) -> None:
        super().__init__(False)
        self.children: list[Node] = []
"#;

const OWNED_PASSES: &str = r#"import copy
import types

from pkg.model import (
    OLD,
    OLD_LIST,
    Abstract,
    Announced,
    Child,
    Config,
    Decorated,
    Derived,
    Failure,
    Hashed,
    Hooked,
    Metered,
    Node,
    Prepared,
    Registered,
    Replaced,
    ResetLater,
    Shared,
    Sometimes,
    Viewed,
    keep,
    make,
    reset,
)
from pkg.shadowed import Shadowed


def pass_factory(node: Node) -> Node:
    copied = make()
    copied.flag = True
    return copied


def appends_to_its_own_list(node: Node) -> Node:
    copied = Node()
    part = Node()
    copied.children.append(part)
    part.flag = True
    return copied


def adds_through_each_method(node: Node) -> Node:
    copied = Node()
    first, second, third, fourth = Node(), Node(), Node(), Node()
    copied.children.insert(0, first)
    copied.kids.add(second)
    copied.table["k"] = third
    copied.table.setdefault("j", fourth)
    first.flag = True
    second.flag = True
    third.flag = True
    fourth.flag = True
    return copied


def appends_through_an_alias(node: Node) -> Node:
    copied = Node()
    part = Node()
    kids = copied.children
    kids.append(part)
    part.flag = True
    return copied


def appends_to_either_new_object(node: Node, left: bool) -> Node:
    copied = Node() if left else Node()
    part = Node()
    copied.children.append(part)
    part.flag = True
    return copied


def appends_to_an_inherited_list(node: Node) -> Child:
    copied = Child()
    part = Node()
    copied.children.append(part)
    part.flag = True
    return copied


def appends_after_a_setup_before_the_store(node: Node) -> Prepared:
    copied = Prepared()
    part = Node()
    copied.children.append(part)
    part.flag = True
    return copied


def appends_after_a_plain_base_constructor(node: Node) -> Derived:
    copied = Derived()
    part = Node()
    copied.children.append(part)
    part.flag = True
    return copied


def appends_to_a_list_a_setter_hands_on(node: Node) -> Announced:
    copied = Announced()
    part = Node()
    copied.children.append(part)
    part.flag = True
    return copied


def appends_after_an_abstract_base_constructor(node: Node) -> Abstract:
    copied = Abstract()
    part = Node()
    copied.children.append(part)
    part.flag = True
    return copied


def appends_after_a_library_base_constructor(node: Node) -> Failure:
    copied = Failure()
    part = Node()
    copied.children.append(part)
    part.flag = True
    return copied


def appends_to_a_list_of_a_class_with_a_metaclass(node: Node) -> Metered:
    copied = Metered()
    part = Node()
    copied.children.append(part)
    part.flag = True
    return copied


def appends_after_a_shadowed_super(node: Node) -> Shadowed:
    copied = Shadowed()
    part = Node()
    copied.children.append(part)
    part.flag = True
    return copied


def appends_to_a_list_a_base_constructor_rebinds(node: Node) -> ResetLater:
    copied = ResetLater()
    part = Node()
    copied.children.append(part)
    part.flag = True
    return copied


def appends_to_a_list_in_a_shared_namespace(node: Node) -> Config:
    copied = Config()
    part = Node()
    copied.children.append(part)
    part.flag = True
    return copied


def keeps_the_part_but_not_the_holder(node: Node) -> Node:
    copied = Node()
    part = Node()
    copied.children.append(part)
    OLD.append(part)
    copied.flag = True
    return copied


def appends_to_a_shallow_copy(node: Node) -> Node:
    copied = copy.copy(node)
    part = Node()
    copied.children.append(part)
    part.flag = True
    return copied


def appends_after_rebinding_to_the_input(node: Node, share: bool) -> Node:
    copied = Node()
    part = Node()
    if share:
        copied.children = node.children
    copied.children.append(part)
    part.flag = True
    return copied


def appends_after_rebinding_to_a_global(node: Node) -> Node:
    copied = Node()
    part = Node()
    copied.children = OLD_LIST
    copied.children.append(part)
    part.flag = True
    return copied


def appends_after_setattr(node: Node, name: str) -> Node:
    copied = Node()
    part = Node()
    setattr(copied, name, OLD_LIST)
    copied.children.append(part)
    part.flag = True
    return copied


def appends_after_a_namespace_write(node: Node) -> Node:
    copied = Node()
    part = Node()
    copied.__dict__["children"] = OLD_LIST
    copied.children.append(part)
    part.flag = True
    return copied


def appends_after_a_helper_keeps_the_list(node: Node) -> Node:
    copied = Node()
    part = Node()
    keep(copied.children)
    copied.children.append(part)
    part.flag = True
    return copied


def appends_after_a_helper_rebinds_the_list(node: Node) -> Node:
    copied = Node()
    part = Node()
    reset(copied)
    copied.children.append(part)
    part.flag = True
    return copied


def rebinds_in_a_loop(node: Node, items: list[int]) -> Node:
    copied = Node()
    for _ in items:
        part = Node()
        copied.children.append(part)
        part.flag = True
        copied.children = OLD_LIST
    return copied


def keeps_the_list_after_appending(node: Node) -> Node:
    copied = Node()
    part = Node()
    kids = copied.children
    kids.append(part)
    OLD.append(kids)
    part.flag = True
    return copied


def keeps_an_item_read_back(node: Node) -> Node:
    copied = Node()
    part = Node()
    copied.children.append(part)
    OLD.append(copied.children[0])
    part.flag = True
    return copied


def keeps_the_holder_after_appending(node: Node) -> Node:
    copied = Node()
    part = Node()
    copied.children.append(part)
    OLD.append(copied)
    part.flag = True
    return copied


def appends_to_a_list_of_an_input(node: Node, left: bool) -> Node:
    copied = Node() if left else node
    part = Node()
    copied.children.append(part)
    part.flag = True
    return copied


def appends_under_a_setattr_hook(node: Node) -> Hooked:
    copied = Hooked()
    part = Node()
    copied.children.append(part)
    part.flag = True
    return copied


def appends_through_a_property(node: Node) -> Viewed:
    copied = Viewed()
    part = Node()
    copied.children.append(part)
    part.flag = True
    return copied


def appends_to_a_list_the_constructor_hands_on(node: Node) -> Registered:
    copied = Registered()
    part = Node()
    copied.children.append(part)
    part.flag = True
    return copied


def appends_to_a_list_made_on_some_paths(node: Node) -> Sometimes:
    copied = Sometimes(True)
    part = Node()
    copied.children.append(part)
    part.flag = True
    return copied


def appends_to_a_list_a_replaced_constructor_binds(node: Node) -> Replaced:
    copied = Replaced()
    part = Node()
    copied.children.append(part)
    part.flag = True
    return copied


def appends_after_a_write_through_vars(node: Node) -> Node:
    copied = Node()
    part = Node()
    vars(copied)["children"] = OLD_LIST
    copied.children.append(part)
    part.flag = True
    return copied


def adds_an_object_whose_hash_keeps_it(node: Node) -> Node:
    copied = Node()
    part = Hashed()
    copied.kids.add(part)
    part.flag = True
    return copied


def appends_to_a_list_the_constructor_keeps(node: Node) -> Shared:
    copied = Shared()
    part = Node()
    copied.children.append(part)
    part.flag = True
    return copied


def appends_to_a_list_of_a_decorated_class(node: Node) -> Decorated:
    copied = Decorated()
    part = Node()
    copied.children.append(part)
    part.flag = True
    return copied


def appends_to_a_library_object(node: Node) -> object:
    copied = types.SimpleNamespace(children=[])
    part = Node()
    copied.children.append(part)
    part.flag = True
    return copied
"#;

/// The roots of a function past 64 allocations: a container is one more root, so with 64
/// objects made first none is decided and the store into one keeps what it stores.
fn many_objects_first() -> String {
    let mut lines = vec![
        "\n\ndef appends_after_many_objects(node: Node) -> Node:".to_owned(),
        "    copied = Node()".to_owned(),
        "    part = Node()".to_owned(),
    ];
    lines.extend((0..62).map(|i| format!("    other{i} = Node()")));
    lines.extend([
        "    copied.children.append(part)".to_owned(),
        "    part.flag = True".to_owned(),
        "    return copied".to_owned(),
    ]);
    lines.join("\n") + "\n"
}

#[test]
fn containers_the_constructor_made_hold_what_is_added() {
    let passes = format!("{OWNED_PASSES}{}", many_objects_first());
    let config = CONFIG.replace("[\"Node.flag\"]", "[\"Node.flag\", \"Hashed.flag\"]");
    let p = Project::new("fresh_links_owned");
    p.write("pyproject.toml", &config)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", OWNED_MODEL)
        .write("pkg/shadowed.py", OWNED_SHADOWED)
        .write("pkg/passes.py", &passes);
    let found = reports(&diagnostics(&p), &passes);
    let at = |func: &str, line: &str| (func.to_owned(), line.to_owned());
    let mut expected = vec![
        // A shallow copy's list is the input's.
        at("appends_to_a_shallow_copy", "part.flag = True"),
        // The attribute, or any attribute, is rebound before the append on some path, by the
        // function or a helper, or the list is kept first.
        at("appends_after_rebinding_to_the_input", "part.flag = True"),
        at("appends_after_rebinding_to_a_global", "part.flag = True"),
        at("appends_after_setattr", "part.flag = True"),
        at("appends_after_a_namespace_write", "part.flag = True"),
        at("appends_after_a_write_through_vars", "part.flag = True"),
        at("appends_after_a_helper_keeps_the_list", "part.flag = True"),
        at(
            "appends_after_a_helper_rebinds_the_list",
            "part.flag = True",
        ),
        at("rebinds_in_a_loop", "part.flag = True"),
        // The list, an item read back, or the holder is kept after the append.
        at("keeps_the_list_after_appending", "part.flag = True"),
        at("keeps_an_item_read_back", "part.flag = True"),
        at("keeps_the_holder_after_appending", "part.flag = True"),
        // Hashing the object stored runs project code, which keeps it.
        at("adds_an_object_whose_hash_keeps_it", "part.flag = True"),
        // The object may be the input.
        at("appends_to_a_list_of_an_input", "part.flag = True"),
        // Writing or reading the attribute runs code, the constructor hands the instance or a
        // member of it on, stores another attribute through a setter, makes the list on some
        // paths only, keeps it, or is replaced in the class body, the base's constructor
        // replaces the instance's namespace or binds the attribute too, a library base's may
        // run anything, `super` may be another object, a class decorator or a metaclass may
        // replace the class or its construction, or a library class makes the object.
        at("appends_under_a_setattr_hook", "part.flag = True"),
        at("appends_through_a_property", "part.flag = True"),
        at("appends_after_a_setup_before_the_store", "part.flag = True"),
        at("appends_to_a_list_a_setter_hands_on", "part.flag = True"),
        at(
            "appends_to_a_list_in_a_shared_namespace",
            "part.flag = True",
        ),
        at(
            "appends_to_a_list_a_base_constructor_rebinds",
            "part.flag = True",
        ),
        at(
            "appends_after_a_library_base_constructor",
            "part.flag = True",
        ),
        at(
            "appends_to_a_list_of_a_class_with_a_metaclass",
            "part.flag = True",
        ),
        at("appends_after_a_shadowed_super", "part.flag = True"),
        at(
            "appends_to_a_list_the_constructor_hands_on",
            "part.flag = True",
        ),
        at("appends_to_a_list_made_on_some_paths", "part.flag = True"),
        at(
            "appends_to_a_list_a_replaced_constructor_binds",
            "part.flag = True",
        ),
        at(
            "appends_to_a_list_the_constructor_keeps",
            "part.flag = True",
        ),
        at("appends_to_a_list_of_a_decorated_class", "part.flag = True"),
        at("appends_to_a_library_object", "part.flag = True"),
        // No root is left for the container.
        at("appends_after_many_objects", "part.flag = True"),
    ];
    expected.sort();
    assert_eq!(found, expected);
}

/// Constructors that keep the instance they make: an `__init__` storing `self` somewhere older,
/// or handing it to a helper that does, its own or a base's reached through
/// `super().__init__()` in another module, makes an object another part of the program holds, so
/// a write to it is the class's write. A decorated `__init__` may run anything, and so may a base
/// `__init__` another module rebinds, which also leaves the containers of the class unknown, as
/// a star import that may bind `list` does. A slice store into a container iterates what it
/// stores, which may run a project `__iter__` keeping it, and reading the instance's namespace
/// by name may rebind any attribute. A helper keeping nothing, a base that only stores
/// attributes, and an item store leave the object new.
const KEEPING_BASE: &str = r#"REGISTRY: list[object] = []


class Base:
    def __init__(self) -> None:
        self.flag = False
        REGISTRY.append(self)


class PlainBase:
    def __init__(self) -> None:
        self.flag = False


class ReboundBase:
    def __init__(self) -> None:
        self.flag = False
"#;

const KEEPING_REBIND: &str = r#"from pkg.base import REGISTRY, ReboundBase


def _publishing_init(self) -> None:
    self.flag = False
    REGISTRY.append(self)


ReboundBase.__init__ = _publishing_init
"#;

const KEEPING_COMPAT: &str = r#"import builtins

SHARED: builtins.list[object] = []


def list() -> builtins.list[object]:
    return SHARED
"#;

const KEEPING_STARRED: &str = r#"from pkg.compat import *


class Starred:
    def __init__(self) -> None:
        self.flag = False
        self.children = list()
"#;

const KEEPING_MODEL: &str = r#"import functools

from pkg.base import REGISTRY, Base, PlainBase, ReboundBase


def traced(init):
    @functools.wraps(init)
    def wrapper(self, *args, **kwargs):
        REGISTRY.append(self)
        init(self, *args, **kwargs)

    return wrapper


class Node:
    def __init__(self) -> None:
        self.flag = False

    def __iter__(self):
        REGISTRY.append(self)
        return iter(())


class Published:
    def __init__(self) -> None:
        self.flag = False
        REGISTRY.append(self)


class Derived(Base):
    def __init__(self) -> None:
        super().__init__()
        self.flag = False


class Setup:
    def __init__(self) -> None:
        self.flag = False
        self._setup()

    def _setup(self) -> None:
        self.ready = True


class Handing:
    def __init__(self) -> None:
        self.flag = False
        self._keep()

    def _keep(self) -> None:
        REGISTRY.append(self)


class Traced:
    @traced
    def __init__(self) -> None:
        self.flag = False


class Quiet(PlainBase):
    def __init__(self) -> None:
        super().__init__()
        self.ready = True


class Holder:
    def __init__(self) -> None:
        self.flag = False
        self.children: list[object] = []


class Reboundish(ReboundBase):
    def __init__(self) -> None:
        super().__init__()
        self.children: list[object] = []
"#;

const KEEPING_PASSES: &str = r#"from pkg.base import REGISTRY
from pkg.model import Derived, Handing, Holder, Node, Published, Quiet, Reboundish, Setup, Traced
from pkg.starred import Starred


def publishes_itself() -> Published:
    n = Published()
    n.flag = True
    return n


def base_publishes() -> Derived:
    d = Derived()
    d.flag = True
    return d


def helper_keeps_it() -> Handing:
    h = Handing()
    h.flag = True
    return h


def decorated_constructor() -> Traced:
    t = Traced()
    t.flag = True
    return t


def appends_under_a_rebound_base() -> Reboundish:
    h = Reboundish()
    part = Node()
    h.children.append(part)
    part.flag = True
    return h


def appends_to_a_starred_list() -> Starred:
    h = Starred()
    part = Node()
    h.children.append(part)
    part.flag = True
    return h


def slice_store() -> Holder:
    h = Holder()
    part = Node()
    h.children[:] = part
    part.flag = True
    return h


def through_getattr_dict() -> Holder:
    h = Holder()
    getattr(h, "__dict__")["children"] = REGISTRY
    part = Node()
    h.children.append(part)
    part.flag = True
    return h


def through_getattribute_dict() -> Holder:
    h = Holder()
    object.__getattribute__(h, "__dict__")["children"] = REGISTRY
    part = Node()
    h.children.append(part)
    part.flag = True
    return h


def setup_keeps_nothing() -> Setup:
    s = Setup()
    s.flag = True
    return s


def plain_base() -> Quiet:
    q = Quiet()
    q.flag = True
    return q


def appends_to_its_own_list() -> Holder:
    h = Holder()
    part = Node()
    h.children.append(part)
    part.flag = True
    h.flag = True
    return h


def item_store() -> Holder:
    h = Holder()
    part = Node()
    h.children[0] = part
    part.flag = True
    return h
"#;

const KEEPING_CONFIG: &str = r#"[project]
name = "t"
requires-python = ">=3.12"

[[tool.heki.write-owners]]
name = "flags"
targets = ["Node.flag", "Published.flag", "Derived.flag", "Handing.flag", "Traced.flag", "Setup.flag", "Quiet.flag", "Holder.flag"]
allowed-writers = ["pkg.model.*"]
fresh-objects = "allow"
"#;

#[test]
fn constructors_keeping_the_instance_they_make() {
    let p = Project::new("fresh_links_keeping");
    p.write("pyproject.toml", KEEPING_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/base.py", KEEPING_BASE)
        .write("pkg/rebind.py", KEEPING_REBIND)
        .write("pkg/compat.py", KEEPING_COMPAT)
        .write("pkg/starred.py", KEEPING_STARRED)
        .write("pkg/model.py", KEEPING_MODEL)
        .write("pkg/passes.py", KEEPING_PASSES);
    let found = reports(&diagnostics(&p), KEEPING_PASSES);
    let at = |func: &str, line: &str| (func.to_owned(), line.to_owned());
    let mut expected = vec![
        at("publishes_itself", "n.flag = True"),
        at("base_publishes", "d.flag = True"),
        at("helper_keeps_it", "h.flag = True"),
        at("decorated_constructor", "t.flag = True"),
        at("appends_under_a_rebound_base", "part.flag = True"),
        at("appends_to_a_starred_list", "part.flag = True"),
        at("slice_store", "part.flag = True"),
        at("through_getattr_dict", "part.flag = True"),
        at("through_getattribute_dict", "part.flag = True"),
    ];
    expected.sort();
    assert_eq!(found, expected);
}
