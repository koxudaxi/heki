//! Receiver typing end to end through the real binary: which writes and reads of guarded
//! attributes (`MUT201` / `MUT203`) heki sees through receivers whose class comes from
//! annotations, the items of containers, return types, flow, and structural typing.

mod common;

use common::Project;
use serde_json::Value;
use std::collections::BTreeSet;

const CONFIG: &str = r#"[project]
name = "t"

[tool.heki]
target-version = "py311"

[tool.heki.lint]
select = ["MUT201", "MUT203"]

[[tool.heki.write-owners]]
name = "store"
targets = ["Model.fields", "Model.name", "Node.kwargs", "Node.children_types"]
allowed-writers = ["pkg.store.*"]
exempt-methods = ["__deepcopy__"]
# The shapes type the objects a function creates too; whether such a write constructs its
# object is `fresh-objects`' concern.
fresh-objects = "report"

[[tool.heki.read-owners]]
name = "kids"
targets = ["Ref.children"]
allowed-readers = ["pkg.store.*"]
"#;

const MODEL: &str = r#"from __future__ import annotations

from typing import Any, Optional

from pydantic import BaseModel


class Model:
    def __init__(self, name: str) -> None:
        self.name = name
        self.fields: list[str] = []

    def clone(self) -> "Model":
        return Model(self.name)

    @property
    def twin(self) -> "Model":
        return self

    @classmethod
    def make(cls) -> "Model":
        return cls("x")


class Node(BaseModel):
    kwargs: Optional[dict[str, Any]] = None
    children_types: list["Node"] = []


class Ref:
    def __init__(self) -> None:
        self.children: list[Any] = []


def make_model() -> Model:
    return Model("m")


def make_untyped():
    return Model("m")


def copied_node(node: Node) -> Node:
    return Node.model_construct(**node.__dict__)
"#;

const STORE: &str = r#"from __future__ import annotations

from pkg.model import Model


class Store:
    def set_fields(self, model: Model, fields: list[str]) -> None:
        model.fields = fields
"#;

/// Ways a function gets hold of a guarded object before writing it, and two reads.
const CASES: &str = r#"from __future__ import annotations

import copy
from copy import deepcopy
from typing import Any, Optional, Union

from pkg.model import Model, Node, Ref, copied_node, make_model, make_untyped


class Holder:
    model: Model

    def __init__(self, model: Model, models: list[Model]) -> None:
        self.m2 = model
        self.models = models
        self.by_name: dict[str, Model] = {}


def annotated_param(model: Model) -> None:
    model.fields = []


def unannotated_param(model) -> None:
    model.fields = []


def local_ctor() -> None:
    m = Model("a")
    m.fields = ["x"]


def return_annotation() -> None:
    m = make_model()
    m.fields = ["x"]


def untyped_return() -> None:
    m = make_untyped()
    m.fields = ["x"]


def method_return(model: Model) -> None:
    m = model.clone()
    m.fields = ["x"]


def classmethod_return() -> None:
    m = Model.make()
    m.fields = ["x"]


def property_read(model: Model) -> None:
    model.twin.fields = ["x"]


def copy_copy(model: Model) -> None:
    m = copy.copy(model)
    m.fields = ["x"]


def deep_copy(model: Model) -> None:
    m = deepcopy(model)
    m.fields = ["x"]


def model_copy(node: Node) -> None:
    n = node.model_copy()
    n.kwargs = {"a": 1}


def project_copy_fn(node: Node) -> None:
    n = copied_node(node)
    n.children_types = []


def for_loop(models: list[Model]) -> None:
    for m in models:
        m.fields = []


def dict_subscript(models: dict[str, Model]) -> None:
    models["a"].fields = []


def list_subscript(models: list[Model]) -> None:
    models[0].fields.append("x")


def optional(model: Optional[Model]) -> None:
    if model is not None:
        model.fields = []


def union(model: Union[Model, None]) -> None:
    if model:
        model.fields.append("x")


def pep604(model: Model | None) -> None:
    assert model is not None
    model.fields.clear()


def isinstance_check(obj: Any) -> None:
    if isinstance(obj, Model):
        obj.fields = []


def self_attr_init(h: Holder) -> None:
    h.m2.fields = []


def self_attr_class_annotation(h: Holder) -> None:
    h.model.fields = []


def self_list_attr(h: Holder) -> None:
    for m in h.models:
        m.fields.append("x")


def self_dict_attr(h: Holder) -> None:
    h.by_name["a"].fields = []


def tuple_unpack(pair: tuple[Model, int]) -> None:
    m, _ = pair
    m.fields = []


def walrus(models: list[Model]) -> None:
    if (m := models[0]) is not None:
        m.fields = []


def comprehension(models: list[Model]) -> None:
    [m.fields.clear() for m in models]


def setattr_const(model: Model) -> None:
    setattr(model, "fields", [])


def dunder_dict(model: Model) -> None:
    model.__dict__["fields"] = []


KEY = "fields"


def dunder_dict_const_name(model: Model) -> None:
    model.__dict__[KEY] = []


def dunder_dict_computed_name(model: Model, name: str) -> None:
    model.__dict__[name] = []


def alias_then_helper(model: Model) -> None:
    fields = model.fields
    _extend(fields)


def _extend(fields: list[str]) -> None:
    fields.append("id")


def dict_get(models: dict[str, Model]) -> None:
    m = models.get("a")
    if m is not None:
        m.fields = []


def items_loop(models: dict[str, Model]) -> None:
    for _, m in models.items():
        m.fields = []


def values_loop(models: dict[str, Model]) -> None:
    for m in models.values():
        m.fields = []


def enumerate_loop(models: list[Model]) -> None:
    for _, m in enumerate(models):
        m.fields = []


def iter_param(models: "Iterable[Model]") -> None:
    for m in models:
        m.fields = []


def nested_attr_write(node: Node) -> None:
    node.children_types[0].kwargs = {}


def subclass_instance() -> None:
    class Sub(Model):
        pass

    s = Sub("x")
    s.fields = []


def augassign(model: Model) -> None:
    model.fields += ["x"]


def delete_item(model: Model) -> None:
    del model.fields[0]


def lambda_callback(models: list[Model]) -> None:
    list(map(lambda m: m.fields.clear(), models))


def read_attr(ref: Ref) -> list[Any]:
    return ref.children


def read_in_generator(refs: list[Ref]) -> int:
    return sum(len(r.children) for r in refs)
"#;

/// The shapes of the store examples, the wrapping builtins, displays, comprehension scopes, and
/// declarations in other modules.
const MORE: &str = r#"from __future__ import annotations

import itertools
from itertools import chain
from typing import Iterator, Mapping, Sequence

from pkg.model import Model, Node
from pkg.shelf import Shelf, walk


def b_loop(models: list[Model]) -> None:
    for model in models:
        model.fields = []


def e_read(models: list[Model]) -> list[str]:
    return [m.name for m in models]


def zipped(models: list[Model], names: list[str]) -> None:
    for name, model in zip(names, models):
        model.fields = [name]


def reversed_sorted(models: Sequence[Model]) -> None:
    for model in reversed(models):
        model.fields = []
    for model in sorted(models, key=id):
        model.fields = []


def chained(first: Model, rest: list[Model], groups: list[list[Model]]) -> None:
    for model in chain((first,), rest):
        model.fields = []
    for model in itertools.chain.from_iterable(groups):
        model.fields = []


def stepped(models: Iterator[Model]) -> None:
    model = next(models, None)
    if model is not None:
        model.fields = []


def starred(first: Model, rest: list[Model]) -> None:
    for model in (first, *rest):
        model.fields = []


def appended(source: Mapping[str, Model]) -> None:
    picked = []
    for key in source:
        picked.append(source[key])
    for model in picked:
        model.fields = []


def generated(models: list[Model]) -> None:
    for model in (m for m in models if m.name):
        model.fields = []


def listed(models: list[Model]) -> None:
    for model in [m for m in models if m.name]:
        model.fields = []


def nested_views(by_group: dict[str, dict[str, Model]]) -> None:
    for group in by_group.values():
        for name, model in group.items():
            model.fields = [name]


def shelved(shelf: Shelf) -> None:
    for model in shelf.models:
        model.fields = []
    for model in walk(shelf):
        model.fields = []


def varargs(*models: Model) -> None:
    for model in models:
        model.fields = []


def nodes(node: Node) -> None:
    for child in node.children_types:
        child.kwargs = {}


def mixed(model: Model, node: Node) -> None:
    for item in (model, node):
        item.fields = []


def shifted(trio: tuple[str, Model, Model]) -> None:
    *_, last = trio
    last.fields = []


def strings(names: list[str]) -> None:
    for name in names:
        name.fields = []


def untyped(models) -> None:
    for model in models:
        model.fields = []
"#;

const SHELF: &str = r#"from __future__ import annotations

from functools import cached_property
from typing import Iterator, Self

from pkg.model import Model


class Shelf:
    models: list[Model]
    parent: Self | None = None

    @property
    def all_models(self) -> list[Model]:
        return list(self.models)

    @cached_property
    def first(self) -> Model:
        return self.models[0]

    @property
    def loose(self):
        return self.models[0]

    def picked(self) -> Model:
        return self.models[0]

    def renamed(self, name: str) -> Self:
        return self


class Wall(Shelf):
    def picked(self) -> Model:
        return Model("w")


def walk(shelf: Shelf) -> Iterator[Model]:
    yield from shelf.models
"#;

/// Flow: variables rebound in loops, a walrus inside `and` / `or`, unions, `isinstance`.
const FLOW: &str = r#"from __future__ import annotations

from typing import Optional, Union

from pkg.model import Model, Node, Ref


class Other:
    def __init__(self) -> None:
        self.fields: list[str] = []


class Chain:
    model: Model
    parent: Optional[Chain] = None


def rebound(chain: Chain) -> None:
    current: Chain | None = chain
    while current is not None:
        current.model.fields = []
        current = current.parent


def walrus_and(by_name: dict[str, Model], flag: bool) -> None:
    if flag and (model := by_name.get("a")):
        model.fields = []


def assigned_on_one_path(flag: bool) -> None:
    if flag:
        model = Model("a")
    model.fields = []


def union_param(value: Model | Other) -> None:
    value.fields = []


def union_typing(value: Union[Other, Model, None]) -> None:
    value.fields.append("x")


def narrowed_out(value: Model | Other) -> None:
    if isinstance(value, Other):
        value.fields = []


def narrowed_in(value: Model | Other) -> None:
    if not isinstance(value, Model):
        return
    value.fields = []


def narrowed_loop(values: list[Model | Other]) -> None:
    for value in values:
        if isinstance(value, Other) or not value.name:
            continue
        value.fields = []


def narrowed_any(value: object) -> None:
    if isinstance(value, Model):
        value.fields = []


def too_many(value: Model | Other | Ref | Node) -> None:
    value.fields = []
"#;

/// Return types: properties, methods along the MRO, `Self`, copies, partial objects, casts.
const RETURNS: &str = r#"from __future__ import annotations

import dataclasses
import functools
from dataclasses import dataclass
from typing import Any, cast

from pkg.model import Model, make_model
from pkg.shelf import Shelf, Wall


@dataclass
class Box:
    model: Model


def property_loop(shelf: Shelf) -> None:
    for model in shelf.all_models:
        model.fields = []


def cached(shelf: Shelf) -> None:
    shelf.first.fields = []


def overridden(shelf: Shelf) -> None:
    shelf.picked().fields = []


def self_chain(wall: Wall) -> None:
    for model in wall.renamed("x").models:
        model.fields = []


def self_attr(shelf: Shelf) -> None:
    parent = shelf.parent
    if parent is not None:
        parent.first.fields = []


def replaced(box: Box) -> None:
    dataclasses.replace(box).model.fields = []


def partial_call() -> None:
    make = functools.partial(make_model)
    make().fields = []


def partial_ctor() -> None:
    functools.partial(Model, "x")().fields = []


def cast_str(obj: object) -> None:
    cast("Model", obj).fields = []


def cast_class(obj: object) -> None:
    cast(Model, obj).fields = []


def loose(shelf: Shelf) -> None:
    shelf.loose.fields = []


def cast_any(obj: object) -> None:
    cast(Any, obj).fields = []


def bound_method(shelf: Shelf) -> None:
    shelf.picked.fields = []
"#;

fn project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("pyproject.toml", CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", MODEL)
        .write("pkg/store.py", STORE)
        .write("pkg/cases.py", CASES)
        .write("pkg/more.py", MORE)
        .write("pkg/shelf.py", SHELF)
        .write("pkg/returns.py", RETURNS)
        .write("pkg/flow.py", FLOW);
    p
}

/// `(qualname, rule)` of every diagnostic of `file`.
fn reported(p: &Project, args: &[&str], file: &str) -> BTreeSet<(String, String)> {
    let mut all = vec!["--output-format", "json"];
    all.extend_from_slice(args);
    let (code, stdout, stderr) = p.check(&all);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    diags
        .iter()
        .filter(|d| d["path"].as_str().unwrap().ends_with(file))
        .map(|d| {
            (
                d["qualname"].as_str().unwrap().to_owned(),
                d["rule_code"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn names(found: &BTreeSet<(String, String)>) -> BTreeSet<String> {
    found.iter().map(|(q, _)| q.clone()).collect()
}

/// The shapes reported for every target version, and the ones that stay silent.
const REPORTED_SHAPES: &[&str] = &[
    "annotated_param",
    "local_ctor",
    "return_annotation",
    "method_return",
    "classmethod_return",
    "property_read",
    "copy_copy",
    "deep_copy",
    "model_copy",
    "project_copy_fn",
    "for_loop",
    "dict_subscript",
    "list_subscript",
    "optional",
    "union",
    "pep604",
    "isinstance_check",
    "self_attr_init",
    "self_attr_class_annotation",
    "self_list_attr",
    "self_dict_attr",
    "tuple_unpack",
    "walrus",
    "comprehension",
    "setattr_const",
    "dunder_dict",
    "alias_then_helper",
    "dict_get",
    "items_loop",
    "values_loop",
    "enumerate_loop",
    "iter_param",
    "nested_attr_write",
    "subclass_instance",
    "augassign",
    "delete_item",
    "read_attr",
    "read_in_generator",
    "dunder_dict_const_name",
];

/// Silent: an unannotated parameter or return (no class to go by), names computed at run
/// time, and a lambda handed to `map` (its parameter has no type).
const SILENT_SHAPES: &[&str] = &[
    "unannotated_param",
    "untyped_return",
    "dunder_dict_computed_name",
    "lambda_callback",
];

#[test]
fn shapes_of_annotated_and_derived_receivers() {
    let p = project("receiver_shapes");
    for version in ["py311", "py312"] {
        let found = names(&reported(
            &p,
            &["--target-version", version],
            "pkg/cases.py",
        ));
        for name in REPORTED_SHAPES {
            assert!(
                found.contains(*name),
                "{name} not reported for {version}: {found:?}"
            );
        }
        for name in SILENT_SHAPES {
            assert!(!found.contains(*name), "{name} reported for {version}");
        }
        // The comprehension writes inside its own scope (Python 3.11) or in the function
        // (3.12): the report is the function's either way.
        assert!(!found.iter().any(|q| q.contains("<listcomp>")), "{found:?}");
    }
}

#[test]
fn items_of_wrapping_builtins_displays_scopes_and_other_modules() {
    let p = project("more_items");
    for version in ["py311", "py312"] {
        let found = names(&reported(&p, &["--target-version", version], "pkg/more.py"));
        for name in [
            "b_loop",
            "zipped",
            "reversed_sorted",
            "chained",
            "stepped",
            "starred",
            "appended",
            "generated",
            "listed",
            "nested_views",
            "shelved",
            "varargs",
            "nodes",
            "mixed",
            "shifted",
        ] {
            assert!(
                found.contains(name),
                "{name} not reported for {version}: {found:?}"
            );
        }
        // A read; items of `str`; an untyped parameter. (A starred target before the written
        // one leaves its position untrusted: any item of the tuple may be there.)
        for name in ["e_read", "strings", "untyped"] {
            assert!(!found.contains(name), "{name} reported for {version}");
        }
    }
}

#[test]
fn return_types_of_properties_methods_self_copies_partials_and_casts() {
    let p = project("returns");
    for version in ["py311", "py312"] {
        let found = names(&reported(
            &p,
            &["--target-version", version],
            "pkg/returns.py",
        ));
        for name in [
            "property_loop",
            "cached",
            "overridden",
            "self_chain",
            "self_attr",
            "replaced",
            "partial_call",
            "partial_ctor",
            "cast_str",
            "cast_class",
        ] {
            assert!(
                found.contains(name),
                "{name} not reported for {version}: {found:?}"
            );
        }
        // A getter without a return annotation, a cast to `Any`, and a bound method read as an
        // attribute name no class.
        for name in ["loose", "cast_any", "bound_method"] {
            assert!(!found.contains(name), "{name} reported for {version}");
        }
    }
}

const HELPERS_CONFIG: &str = r#"[project]
name = "t"

[tool.heki]
target-version = "py311"

[tool.heki.lint]
select = ["MUT201"]

[[tool.heki.write-owners]]
name = "store"
targets = ["DataType.alias", "DataType.items", "Field.data_type"]
allowed-writers = ["pkg.store.*"]
"#;

const HELPERS_STORE: &str = r#"from __future__ import annotations


class DataType:
    def __init__(self) -> None:
        self.alias: str | None = None
        self.items: list[str] = []


class BaseClassDataType(DataType):
    pass


class Field:
    def __init__(self, data_type: DataType) -> None:
        self.data_type = data_type


class Store:
    def name(self, data_type: DataType, alias: str) -> None:
        data_type.alias = alias


class Builder:
    def __init__(self, store: Store) -> None:
        self._store = store

    def name(self, data_type: DataType, alias: str) -> None:
        self._store.name(data_type, alias)
"#;

const HELPERS_USE: &str = r#"from __future__ import annotations

from typing import Protocol

from pkg.store import BaseClassDataType, Builder, DataType, Field


class Host(Protocol):
    @property
    def builder(self) -> Builder: ...


def rename(host: Host, data_type: DataType) -> None:
    host.builder.name(data_type, "x")


def rename_bases(host: Host, bases: list[BaseClassDataType]) -> None:
    for base in bases:
        rename(host, base)


def rename_fields(host: Host, fields: list[Field]) -> None:
    for field in fields:
        rename(host, field.data_type)


def set_alias(data_type: DataType) -> None:
    data_type.alias = "y"


def set_base_alias(base: BaseClassDataType) -> None:
    set_alias(base)


def _untyped(value) -> None:
    value.alias = "z"


def untyped_helper(base: BaseClassDataType) -> None:
    _untyped(base)


def _extend(items: list[str]) -> None:
    items.append("w")


def extend_items(data_type: DataType) -> None:
    _extend(data_type.items)
"#;

#[test]
fn helpers_typing_their_parameter_report_their_own_writes() {
    let p = Project::new("typed_helpers");
    p.write("pyproject.toml", HELPERS_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/store.py", HELPERS_STORE)
        .write("pkg/use.py", HELPERS_USE);
    let found = reported(&p, &[], "pkg/use.py");
    let expected: BTreeSet<(String, String)> = [
        // A helper typing its parameter reports its own write, which a caller passing a
        // subclass instance does not repeat.
        "set_alias",
        // An untyped helper's change is its caller's, which knows the class.
        "untyped_helper",
        // A helper changing the value of a guarded attribute in place.
        "extend_items",
    ]
    .iter()
    .map(|q| ((*q).to_owned(), "MUT201".to_owned()))
    .collect();
    // A write routed through the store's operations (`rename`) stays allowed for callers passing
    // a subclass instance or the value of a guarded attribute, whose own attributes it writes.
    assert_eq!(found, expected);
}

#[test]
fn flow_rebinding_walrus_unions_and_isinstance() {
    let p = project("flow");
    for version in ["py311", "py312"] {
        let found = names(&reported(&p, &["--target-version", version], "pkg/flow.py"));
        for name in [
            "rebound",
            "walrus_and",
            "assigned_on_one_path",
            "union_param",
            "union_typing",
            "narrowed_in",
            "narrowed_loop",
            "narrowed_any",
        ] {
            assert!(
                found.contains(name),
                "{name} not reported for {version}: {found:?}"
            );
        }
        // An `isinstance` check leaving only an unguarded class; more classes than a union
        // keeps.
        for name in ["narrowed_out", "too_many"] {
            assert!(!found.contains(name), "{name} reported for {version}");
        }
    }
}

const PROTOCOL_CONFIG: &str = r#"[project]
name = "t"

[tool.heki]
target-version = "py311"

[tool.heki.lint]
select = ["MUT201", "EFF101"]

[[tool.heki.write-owners]]
name = "store"
targets = ["Field.data_type", "Field.name"]
allowed-writers = ["pkg.store.*"]
"#;

const PROTOCOL_VIEWS: &str = r#"from __future__ import annotations

from typing import Protocol


class FieldView(Protocol):
    data_type: str

    def rename(self, name: str) -> None: ...


class Pauser(Protocol):
    def pause(self) -> None: ...


class Named(Protocol):
    name: str


def write_view(view: FieldView) -> None:
    view.data_type = "y"


def extend_view(views: list[FieldView]) -> None:
    for view in views:
        view.data_type = "z"


async def run(pauser: Pauser) -> None:
    pauser.pause()


def rename_named(named: Named) -> None:
    named.name = "n"
"#;

const PROTOCOL_IMPLS: &str = r#"from __future__ import annotations

import time


class Field:
    def __init__(self) -> None:
        self.data_type = "x"
        self.name = ""

    def rename(self, name: str) -> None:
        self.name = name


class SubField(Field):
    pass


class Unrelated:
    def rename(self, name: str) -> None:
        pass


class Sleepy:
    def pause(self) -> None:
        time.sleep(1)
"#;

/// Classes that all have a `name`: more than the candidate limit implement `Named`.
fn many_named() -> String {
    (0..9)
        .map(|i| {
            format!(
                "class Named{i}:\n    def __init__(self) -> None:\n        self.name = \"\"\n\n\n"
            )
        })
        .collect()
}

#[test]
fn protocol_receivers_dispatch_to_the_classes_implementing_them() {
    let p = Project::new("protocols");
    p.write("pyproject.toml", PROTOCOL_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/views.py", PROTOCOL_VIEWS)
        .write("pkg/impls.py", PROTOCOL_IMPLS)
        .write("pkg/named.py", &many_named());
    let found = reported(&p, &[], "pkg/views.py");
    let expected: BTreeSet<(String, String)> = [
        // `Field` has every member of `FieldView` (`SubField` is reached through it, `Unrelated`
        // lacks `data_type`).
        ("write_view", "MUT201"),
        ("extend_view", "MUT201"),
        // `Sleepy.pause` blocks.
        ("run", "EFF101"),
    ]
    .iter()
    .map(|(q, r)| ((*q).to_owned(), (*r).to_owned()))
    .collect();
    // `Named` has more implementations than the candidate limit: nothing is dispatched.
    assert_eq!(found, expected);
}

/// Protocol-typed objects handed to helpers that do not type them, and protocols whose
/// implementations heki cannot list.
const PROTOCOL_HANDED: &str = r#"from __future__ import annotations

from typing import Any, Protocol

from pkg.views import FieldView, Named


class Marker(Protocol):
    pass


def rewrite(field: Any) -> None:
    field.data_type = "w"


def rename(obj) -> None:
    obj.name = "m"


class Writer:
    def write(self, view: FieldView) -> None:
        rewrite(view)


def rename_named(named: Named) -> None:
    rename(named)


def mark(marker: Marker) -> None:
    marker.name = "k"
"#;

#[test]
fn a_protocol_typed_argument_ties_a_helper_write_to_the_implementations() {
    let p = Project::new("protocols_handed");
    p.write(
        "pyproject.toml",
        &format!("{PROTOCOL_CONFIG}unresolved = \"report\"\n").replace(
            "select = [\"MUT201\", \"EFF101\"]",
            "select = [\"MUT201\", \"MUT205\"]",
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/views.py", PROTOCOL_VIEWS)
    .write("pkg/impls.py", PROTOCOL_IMPLS)
    .write("pkg/named.py", &many_named())
    .write("pkg/handed.py", PROTOCOL_HANDED);
    let expected: BTreeSet<(String, String)> = [
        // `FieldView` is `Field`: the helper's write is the caller's, and no caller leaves the
        // helper's object unknown.
        ("Writer.write", "MUT201"),
        // `Named` has more implementations than the candidate limit and `Marker` no member:
        // their objects are of unknown class, handed on or written directly.
        ("rename", "MUT205"),
        ("mark", "MUT205"),
    ]
    .iter()
    .map(|(q, r)| ((*q).to_owned(), (*r).to_owned()))
    .collect();
    assert_eq!(reported(&p, &[], "pkg/handed.py"), expected);
    let views: BTreeSet<(String, String)> = [
        ("write_view", "MUT201"),
        ("extend_view", "MUT201"),
        ("rename_named", "MUT205"),
    ]
    .iter()
    .map(|(q, r)| ((*q).to_owned(), (*r).to_owned()))
    .collect();
    assert_eq!(reported(&p, &[], "pkg/views.py"), views);
}

/// Helpers that do not type their parameter, handed objects typed with a protocol (whose
/// implementations heki lists or not) and with one of its implementations.
const PROTOCOL_TARGET_HANDED: &str = r#"from __future__ import annotations

from pkg.impls import Field
from pkg.views import FieldView, Named


def retype(obj) -> None:
    obj.data_type = "w"


def rename(obj) -> None:
    obj.name = "m"


def through_view(view: FieldView) -> None:
    retype(view)


def through_named(named: Named) -> None:
    rename(named)


def through_field(field: Field) -> None:
    retype(field)
"#;

/// A target on a protocol covers the writes through an object typed with it, made directly or
/// by a helper the object is handed to: the object is an instance of the protocol as well as
/// of its implementations. An object typed with an implementation is no instance of the
/// protocol's class family.
#[test]
fn a_protocol_target_covers_the_writes_of_helpers_handed_a_protocol_typed_object() {
    let p = Project::new("protocol_targets_handed");
    p.write(
        "pyproject.toml",
        &PROTOCOL_CONFIG
            .replace("select = [\"MUT201\", \"EFF101\"]", "select = [\"MUT201\"]")
            .replace(
                "targets = [\"Field.data_type\", \"Field.name\"]",
                "targets = [\"FieldView.data_type\", \"Named.name\"]",
            ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/views.py", PROTOCOL_VIEWS)
    .write("pkg/impls.py", PROTOCOL_IMPLS)
    .write("pkg/named.py", &many_named())
    .write("pkg/handed.py", PROTOCOL_TARGET_HANDED);
    let mut expected: BTreeSet<(String, String)> = [
        ("write_view", "MUT201"),
        ("extend_view", "MUT201"),
        ("rename_named", "MUT201"),
    ]
    .iter()
    .map(|(q, r)| ((*q).to_owned(), (*r).to_owned()))
    .collect();
    assert_eq!(reported(&p, &[], "pkg/views.py"), expected);
    expected = [("through_view", "MUT201"), ("through_named", "MUT201")]
        .iter()
        .map(|(q, r)| ((*q).to_owned(), (*r).to_owned()))
        .collect();
    assert_eq!(reported(&p, &[], "pkg/handed.py"), expected);
}
