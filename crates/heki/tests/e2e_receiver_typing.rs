//! Receiver typing end to end through the real binary: which writes and reads of guarded
//! attributes (`MUT201` / `MUT203`) heki sees through receivers whose class comes from
//! annotations, the items of containers, return types, flow, structural typing, the closure
//! cells of enclosing functions, and `type(obj)`.

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


def extended(source: Mapping[str, Model]) -> None:
    picked = []
    picked += [source[key] for key in source]
    for model in picked:
        model.fields = []


def extended_then_appended(source: Mapping[str, Model]) -> None:
    picked = []
    picked += []
    picked.append(source["a"])
    for model in picked:
        model.fields = []


def subtracted(source: Mapping[str, Model]) -> None:
    picked = [source["a"]]
    picked -= []
    for model in picked:
        model.fields = []


class Replacement:
    def __ror__(self, other):
        return []


def replaced(source: Mapping[str, Model]) -> None:
    picked = []
    kept = picked
    kept += []
    kept |= Replacement()
    kept.append(source["a"])
    for model in picked:
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

/// Return types: properties, methods along the MRO, `Self`, copies, partial objects, casts (which
/// type what nothing else types, as an annotation of a local does).
const RETURNS: &str = r#"from __future__ import annotations

import dataclasses
import functools
from dataclasses import dataclass
from typing import Any, cast

import typing_extensions

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


def cast_any_of_model(model: Model) -> None:
    cast(Any, model).fields = []


def cast_any_text_of_model(model: Model) -> None:
    cast("Any", model).fields = []


def cast_object_of_model(model: Model) -> None:
    typing_extensions.cast(object, model).fields = []


def cast_any_of_any(obj: Any) -> None:
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
                common::tiered_code(d).to_owned(),
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
            "extended",
            "extended_then_appended",
        ] {
            assert!(
                found.contains(name),
                "{name} not reported for {version}: {found:?}"
            );
        }
        // A read; items of `str`; an untyped parameter; `-=` on a list, which does not change
        // it in place (so it gives no list heki knows), and `|=` on one after a `+=` that does
        // (it gives what `Replacement.__ror__` returns, so the item goes to another list). (A
        // starred target before the written one leaves its position untrusted: any item of the
        // tuple may be there.)
        for name in ["e_read", "strings", "untyped", "subtracted", "replaced"] {
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
            "cast_any_of_model",
            "cast_any_text_of_model",
            "cast_object_of_model",
        ] {
            assert!(
                found.contains(name),
                "{name} not reported for {version}: {found:?}"
            );
        }
        // A getter without a return annotation, a cast to `Any` of a value nothing else types,
        // and a bound method read as an attribute name no class. A cast to a broader type keeps
        // the class its value is known to have.
        for name in ["loose", "cast_any", "cast_any_of_any", "bound_method"] {
            assert!(!found.contains(name), "{name} reported for {version}");
        }
    }
}

/// The call-owners shape of a cast: `typing.cast` returns its value unchanged, so a cast to
/// `Any` keeps the class its value is known to have, as an annotated local does; a value only an
/// annotation of `Any` types stays unresolved.
const CAST_CALLS: &str = r#"from typing import Any, cast

from pkg.imports import Imports


class Parser:
    def __init__(self) -> None:
        self.imports = Imports()


def direct(parser: Parser) -> None:
    parser.imports.append("a")


def through_cast_any(parser: Parser) -> None:
    cast("Any", parser).imports.append("b")


def through_cast_any_name(parser: Parser) -> None:
    cast(Any, parser).imports.append("c")


def through_local_any(parser: Parser) -> None:
    p: Any = parser
    p.imports.append("e")


def through_parameter_any(parser: Any) -> None:
    cast(Any, parser).imports.append("f")
"#;

#[test]
fn a_cast_keeps_the_class_its_value_is_known_to_have() {
    let p = Project::new("receiver_casts");
    p.write(
        "pyproject.toml",
        r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT204", "MUT207"]

[[tool.heki.call-owners]]
name = "imports"
targets = ["pkg.imports:Imports.append"]
allowed-callers = ["pkg.imports:*"]
unresolved = "report"
"#,
    )
    .write("pkg/__init__.py", "")
    .write(
        "pkg/imports.py",
        "class Imports:\n    def __init__(self) -> None:\n        self.items: list[str] = []\n\n\n\
         \x20   def append(self, name: str) -> None:\n        self.items.append(name)\n",
    )
    .write("pkg/parser.py", CAST_CALLS);
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let found: Vec<(u64, &str)> = diags
        .iter()
        .map(|d| {
            (
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        found,
        vec![
            (12, "MUT204"),
            (16, "MUT204"),
            (20, "MUT204"),
            (25, "MUT204"),
            (29, "MUT207"),
        ]
    );
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
        // `Sleepy.pause` blocks; a protocol's implementations are never complete (heuristic).
        ("run", "EFF101 [heuristic]"),
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

const CELLS_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT203", "MUT204", "MUT206", "MUT207"]

[[tool.heki.read-owners]]
name = "given"
targets = ["pkg.model.Model._given"]
forbidden-readers = ["pkg.cells:*"]
unresolved = "report"

[[tool.heki.call-owners]]
name = "private"
targets = ["pkg.model.Model._*", "pkg.model.Schema._*"]
forbidden-callers = ["pkg.cells:*", "pkg.classes:*"]
unresolved = "report"
"#;

const CELLS_MODEL: &str = r#"from __future__ import annotations


class Model:
    def __init__(self) -> None:
        self._given = {"a"}

    def _takes(self, name: str) -> bool:
        return bool(name)


class Other:
    def __init__(self) -> None:
        self._given = {"b"}


class Factory:
    def build(self, name: str) -> Model:
        return Model()


class Schema:
    @classmethod
    def _first_typed(cls, value: object) -> object:
        return value


class SubSchema(Schema):
    @classmethod
    def _first_typed(cls, value: object) -> object:
        return value


class Plain:
    pass


def untyped():
    return object()
"#;

/// Variables of an enclosing function read by the scopes nested in it, through their closure
/// cells: `self` of a class with no target, a parameter typed with the target's class, and
/// cells that hold more than one binding.
const CELLS: &str = r#"from __future__ import annotations

from pkg.model import Factory, Model, Other, untyped


class Allocation:
    def __init__(self) -> None:
        self._given: set[str] = set()

    def _takes(self, name: str) -> bool:
        return name in self._given

    def listcomp(self, names: list[str]) -> list[str]:
        return [n for n in names if n in self._given]

    def setcomp(self, names: list[str]) -> set[str]:
        return {n for n in names if self._takes(n)}

    def dictcomp(self, names: list[str]) -> dict[str, bool]:
        return {n: n in self._given for n in names}

    def genexpr(self, names: list[str]) -> bool:
        return any(n in self._given for n in names)

    def lambda_(self, names: list[str]) -> list[str]:
        return list(filter(lambda n: self._takes(n), names))

    def nested_def(self, names: list[str]) -> list[str]:
        def keep(n: str) -> bool:
            return n in self._given

        return [n for n in names if keep(n)]

    def nested_comprehensions(self, groups: list[list[str]]) -> list[list[str]]:
        return [[n for n in group if n in self._given] for group in groups]

    def genexpr_in_listcomp(self, groups: list[list[str]]) -> list[bool]:
        return [any(self._takes(n) for n in group) for group in groups]


def listcomp(model: Model, names: list[str]) -> list[str]:
    return [n for n in names if n in model._given]


def setcomp(model: Model, names: list[str]) -> set[str]:
    return {n for n in names if model._takes(n)}


def dictcomp(model: Model, names: list[str]) -> dict[str, bool]:
    return {n: n in model._given for n in names}


def genexpr(model: Model, names: list[str]) -> list[object]:
    return list(model._given for _ in names)


def lambda_(model: Model) -> object:
    return lambda: model._given


def nested_def(model: Model) -> object:
    def inner() -> object:
        return model._given

    return inner


def two_levels(model: Model) -> object:
    def outer() -> list[str]:
        return [n for n in "ab" if n in model._given]

    return outer


def class_body(model: Model) -> object:
    class Local:
        seen = model._given

    return Local


def iteration_variable(models: list[Model]) -> list[object]:
    return [lambda: m._given for m in models]


def generator_in_comprehension(models: list[Model], names: list[str]) -> list[bool]:
    return [any(n in m._given for n in names) for m in models]


def same_name(model: Model, others: list[Other]) -> object:
    readers = [lambda: model._given for model in others]
    return readers, lambda: model._given


def bound_later() -> object:
    f = lambda: m._given
    m = Model()
    return f


def rebound_known(model: Model) -> object:
    f = lambda: model._given
    model = Other()
    return f


def outputs(factory: Factory, names: list[str]) -> None:
    for built in [factory.build(n) for n in names]:
        print(built._given)


def bound_after_generator(names: list[str]) -> object:
    gen = (factory.build(n) for n in names)
    factory = Factory()
    return next(gen)._given


def declared_bound_before_generator(names: list[str]) -> object:
    factory: object = Factory()
    gen = (factory.build(n) for n in names)
    return next(gen)._given


def declared_bound_after_generator(names: list[str]) -> object:
    gen = (factory.build(n) for n in names)
    factory: object = Factory()
    return next(gen)._given


def rebound_unknown(model: Model) -> object:
    f = lambda: model._given
    model = untyped()
    return f


def nonlocal_store(model: Model) -> object:
    def reset() -> None:
        nonlocal model
        model = untyped()

    reset()
    return lambda: model._given, list(model._given for _ in "ab")


def walrus(names: list[str]) -> object:
    found = Model()
    [(found := untyped()) for _ in names]
    return lambda: found._given


def shadowed(model: Model, models: list[Model]) -> object:
    readers = [lambda: model for model in models]

    def reset() -> None:
        nonlocal model
        model = untyped()

    reset()
    return readers, lambda: model._given
"#;

/// A function whose nested function is too large to model and rebinds a variable of the
/// function through `nonlocal`: nothing heki sees creates that nested function.
fn unmodeled_rebinding() -> String {
    let mut lines = vec![
        "def unmodeled(model: Model) -> object:".to_owned(),
        "    def huge(xs):".to_owned(),
        "        def reset() -> None:".to_owned(),
        "            nonlocal model".to_owned(),
        "            model = untyped()".to_owned(),
        String::new(),
        "        reset()".to_owned(),
    ];
    let mut pad = "        ".to_owned();
    for i in 0..14 {
        lines.push(format!("{pad}try:"));
        lines.push(format!("{pad}    if xs:"));
        lines.push(format!("{pad}        return {i}"));
        lines.push(format!("{pad}    print(xs)"));
        lines.push(format!("{pad}finally:"));
        pad.push_str("    ");
    }
    lines.push(format!("{pad}print(xs)"));
    lines.push(String::new());
    lines.push("    return huge, lambda: model._given".to_owned());
    format!("\n\n{}\n", lines.join("\n"))
}

/// `type(obj)` and the objects whose class it cannot tie to one class.
const CLASS_OF: &str = r#"from __future__ import annotations

from typing import Protocol

from pkg.model import Plain, Schema


class HasFirst(Protocol):
    def _first_typed(self, value: object) -> object: ...


def call(schema: Schema) -> object:
    return type(schema)._first_typed(schema)


def constructed(schema: Schema) -> object:
    return type(schema)()._first_typed(schema)


def protocol(value: HasFirst) -> object:
    return type(value)._first_typed(value)


def union(value: Schema | Plain) -> object:
    return type(value)._first_typed(value)


def unknown(value) -> object:
    return type(value)._first_typed(value)


def created(value: Schema) -> object:
    return type("Made", (Schema,), {})._first_typed(value)
"#;

fn cells_project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("pyproject.toml", CELLS_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", CELLS_MODEL)
        .write("pkg/cells.py", &format!("{CELLS}{}", unmodeled_rebinding()))
        .write("pkg/classes.py", CLASS_OF);
    p
}

fn pairs(items: &[(&str, &str)]) -> BTreeSet<(String, String)> {
    items
        .iter()
        .map(|(q, r)| ((*q).to_owned(), (*r).to_owned()))
        .collect()
}

/// A closure cell holds every value the function owning it binds to it: a scope nested in a
/// function (a comprehension or generator expression with a scope of its own, a lambda, a
/// nested function or class) reads `self` and the function's other variables with their
/// class, in a comprehension before 3.12 as in an inlined one. A variable a nested scope
/// rebinds, or bound to a value of unknown class, stays of unknown class.
#[test]
fn closure_cells_hold_the_values_the_enclosing_function_binds() {
    let p = cells_project("closure_cells");
    for version in ["py311", "py312"] {
        let mut expected = pairs(&[
            // `self` is an `Allocation`, which has no target: no shape reports.
            ("listcomp", "MUT203"),
            ("setcomp", "MUT204"),
            ("dictcomp", "MUT203"),
            ("genexpr", "MUT203"),
            ("lambda_.<locals>.<lambda>", "MUT203"),
            ("nested_def.<locals>.inner", "MUT203"),
            ("two_levels.<locals>.outer", "MUT203"),
            ("class_body", "MUT203"),
            // A comprehension's iteration variable, in a comprehension's own scope before 3.12
            // and a cell of the function from 3.12, read by the scopes nested in the
            // comprehension; the function's own variable of the name, read by a scope outside
            // it.
            ("generator_in_comprehension", "MUT203"),
            ("same_name.<locals>.<lambda>", "MUT203"),
            // Read once bound, whenever the lambda runs.
            ("bound_later.<locals>.<lambda>", "MUT203"),
            // A `Model` or an `Other`.
            ("rebound_known.<locals>.<lambda>", "MUT203"),
            // What the comprehension builds, and the generator yields from a cell bound after
            // the generator is made: `Model`s.
            ("outputs", "MUT203"),
            ("bound_after_generator", "MUT203"),
            // A declared variable holds the narrower value bound to it; the declaration stands
            // for a binding not computed yet when the generator is made.
            ("declared_bound_before_generator", "MUT203"),
            ("declared_bound_after_generator", "MUT206"),
            ("rebound_unknown.<locals>.<lambda>", "MUT206"),
            ("nonlocal_store.<locals>.<lambda>", "MUT206"),
            ("nonlocal_store.<locals>.<genexpr>", "MUT206"),
            ("walrus.<locals>.<lambda>", "MUT206"),
            // `reset` rebinds the function's `model`, not the comprehension's.
            ("shadowed.<locals>.<lambda>", "MUT206"),
            ("unmodeled.<locals>.<lambda>", "MUT206"),
        ]);
        // The lambda is nested in the comprehension's own scope before 3.12.
        expected.extend(pairs(&[if version == "py311" {
            (
                "iteration_variable.<locals>.<listcomp>.<locals>.<lambda>",
                "MUT203",
            )
        } else {
            ("iteration_variable.<locals>.<lambda>", "MUT203")
        }]));
        assert_eq!(
            reported(&p, &["--target-version", version], "pkg/cells.py"),
            expected,
            "{version}"
        );
    }
}

/// `type(obj)` of an instance of a project class is a class object that may be a subclass, as
/// `cls` in a class method is: calling a method on it dispatches to the overrides too. It is
/// unknown for a protocol, a union, an object of unknown class, and a class `type` creates.
#[test]
fn type_of_an_instance_is_its_class_or_a_subclass() {
    let p = cells_project("type_of_instance");
    let expected = pairs(&[
        ("call", "MUT204"),
        ("constructed", "MUT204"),
        ("protocol", "MUT207"),
        ("union", "MUT207"),
        ("unknown", "MUT207"),
        ("created", "MUT207"),
    ]);
    assert_eq!(reported(&p, &[], "pkg/classes.py"), expected);
    let (_, stdout, _) = p.check(&["--output-format", "concise"]);
    assert!(
        stdout.contains(
            "`call` calls `pkg.model.Schema._first_typed` or `pkg.model.SubSchema._first_typed`"
        ),
        "{stdout}"
    );
}

const CLASS_OBJECT_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT203", "MUT204", "EFF201"]

[[tool.heki.call-owners]]
name = "dict-builder"
targets = ["pkg.types.DataType(is_dict=True)"]
forbidden-callers = ["pkg.uses:*"]

[[tool.heki.call-owners]]
name = "private"
targets = ["pkg.types.DataType._*", "pkg.types.Handler.__call__"]
forbidden-callers = ["pkg.uses:*"]

[[tool.heki.read-owners]]
name = "members"
targets = ["pkg.types.DataType.HOLDS_MEMBERS"]
forbidden-readers = ["pkg.uses:*"]

[[tool.heki.effect-boundaries]]
name = "pure"
scopes = ["pkg.uses.class_attribute", "pkg.uses.named_class"]
forbid = ["reads-targets"]
read-targets = ["pkg.types.DataType.HOLDS_MEMBERS"]
"#;

const CLASS_OBJECT_TYPES: &str = r#"from __future__ import annotations

from typing import Protocol


class DataType:
    HOLDS_MEMBERS = True

    def __init__(self, *, is_dict: bool = False, is_list: bool = False) -> None:
        self.is_dict = is_dict
        self.is_list = is_list

    @classmethod
    def _of(cls, flag: bool) -> DataType:
        return cls(is_list=flag)


class Leaf(DataType):
    pass


class Other:
    HOLDS_MEMBERS = False

    def __init__(self, *, is_dict: bool = False) -> None:
        self.is_dict = is_dict


class Members(Protocol):
    HOLDS_MEMBERS: bool

    def __init__(self, *, is_dict: bool = False) -> None: ...


class Handler:
    def __call__(self, value: object) -> None:
        print(value)
"#;

/// Values typed `type[C]` (a parameter, a property, a return, the items of a container, a
/// local, an attribute) and the class objects that read class attributes.
const CLASS_OBJECT_USES: &str = r#"from __future__ import annotations

import typing
from typing import Any, Optional, Type, Union

from pkg import types
from pkg.types import DataType, Handler, Leaf, Members, Other


class Config:
    kind: type[DataType] = DataType


class Parser:
    def __init__(self, data_type: type[DataType]) -> None:
        self._data_type = data_type

    @property
    def data_type(self) -> type[DataType]:
        return self._data_type

    @property
    def maybe_type(self) -> Optional[type[DataType]]:
        return self._data_type

    @property
    def untyped(self):
        return self._data_type

    @property
    def handler(self) -> Handler:
        return Handler()

    def property_call(self) -> DataType:
        return self.data_type(is_dict=True)

    def property_other_arguments(self) -> DataType:
        return self.data_type(is_list=True)

    def optional_property(self) -> DataType | None:
        maybe = self.maybe_type
        return maybe(is_dict=True) if maybe else None

    def property_alias(self) -> DataType:
        factory = self.data_type
        return factory(is_dict=True)

    def untyped_property(self) -> object:
        return self.untyped(is_dict=True)

    def property_instance(self) -> None:
        self.handler(1)

    def stored_attribute(self) -> DataType:
        factory = self._data_type
        return factory(is_dict=True)


class Child(Parser):
    def super_property(self) -> DataType:
        return super().data_type(is_dict=True)


def parameter(factory: type[DataType]) -> DataType:
    return factory(is_dict=True)


def typing_type(factory: Type[DataType]) -> DataType:
    return factory(is_dict=True)


def typing_module(factory: typing.Type[DataType]) -> DataType:
    return factory(is_dict=True)


def optional(factory: Optional[type[DataType]]) -> DataType | None:
    return factory(is_dict=True) if factory else None


def pep604(factory: type[DataType] | None) -> DataType | None:
    return factory(is_dict=True) if factory else None


def string(factory: "type[DataType]") -> DataType:
    return factory(is_dict=True)


def string_argument(factory: type["DataType"]) -> DataType:
    return factory(is_dict=True)


def subclass(factory: type[Leaf]) -> DataType:
    return factory(is_dict=True)


def module_attribute(factory: type[types.DataType]) -> DataType:
    return factory(is_dict=True)


def other_arguments(factory: type[DataType]) -> DataType:
    return factory(is_list=True)


def union(factory: type[DataType] | type[Other]) -> object:
    return factory(is_dict=True)


def union_argument(factory: type[Union[DataType, Other]]) -> object:
    return factory(is_dict=True)


def protocol(factory: type[Members]) -> object:
    return factory(is_dict=True)


def any_class(factory: type[Any]) -> object:
    return factory(is_dict=True)


def returned() -> type[DataType]:
    return DataType


def return_annotation() -> DataType:
    return returned()(is_dict=True)


def items(kinds: list[type[DataType]]) -> None:
    for kind in kinds:
        kind(is_dict=True)


def registry(by_name: dict[str, type[DataType]], name: str) -> DataType:
    return by_name[name](is_dict=True)


def local_annotation(name: str) -> DataType:
    kind: type[DataType] = Leaf if name else DataType
    return kind(is_dict=True)


def lookup(name: str) -> object:
    return name


def narrower_class(leaf: Leaf) -> DataType:
    kind: type[DataType] = type(leaf)
    return kind(is_dict=True)


def declared_over_other(name: str) -> DataType:
    kind: type[DataType] = lookup(name)
    return kind(is_dict=True)


DEFAULT_KIND: type[DataType] = Leaf


def module_default() -> DataType:
    return DEFAULT_KIND(is_dict=True)


def class_attribute_annotation(config: Config) -> DataType:
    kind = config.kind
    return kind(is_dict=True)


def class_method(model_type: type[DataType]) -> DataType:
    return model_type._of(True)


def class_attribute(model_type: type[DataType]) -> bool:
    return model_type.HOLDS_MEMBERS


def named_class() -> bool:
    return DataType.HOLDS_MEMBERS


def through_module() -> bool:
    return types.DataType.HOLDS_MEMBERS


def type_of(data: DataType) -> bool:
    return type(data).HOLDS_MEMBERS


def getattr_class(model_type: type[DataType]) -> bool:
    return getattr(model_type, "HOLDS_MEMBERS")


def other_class(model_type: type[Other]) -> bool:
    return model_type.HOLDS_MEMBERS


def protocol_class(model_type: type[Members]) -> bool:
    return model_type.HOLDS_MEMBERS


def instance(data: DataType) -> bool:
    return data.HOLDS_MEMBERS


class Builder(DataType):
    @classmethod
    def own_class(cls) -> bool:
        return cls.HOLDS_MEMBERS
"#;

/// A value typed `type[C]` / `typing.Type[C]` (also optional, quoted, or inside a container) is
/// the class object of `C` or a subclass, as `cls` is: calling it constructs `C`, a class method
/// called on it runs `C`'s, and reading an attribute through it reads `C`'s class attribute, as a
/// read through any class object does (for `read-owners` and an effect boundary's
/// `read-targets`). A call of a property's value (`self.data_type(...)`) calls what the
/// property's annotation says it returns. A class object of a protocol, of `Any`, or of either of
/// two classes is of no known class.
#[test]
fn class_object_annotations_construct_call_and_read_their_class() {
    let p = Project::new("class_objects");
    p.write("pyproject.toml", CLASS_OBJECT_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/types.py", CLASS_OBJECT_TYPES)
        .write("pkg/uses.py", CLASS_OBJECT_USES);
    let expected = pairs(&[
        ("Parser.property_call", "MUT204"),
        ("Parser.optional_property", "MUT204"),
        ("Parser.property_alias", "MUT204"),
        ("Parser.property_instance", "MUT204"),
        ("Parser.stored_attribute", "MUT204"),
        ("Child.super_property", "MUT204"),
        ("parameter", "MUT204"),
        ("typing_type", "MUT204"),
        ("typing_module", "MUT204"),
        ("optional", "MUT204"),
        ("pep604", "MUT204"),
        ("string", "MUT204"),
        ("string_argument", "MUT204"),
        ("subclass", "MUT204"),
        ("module_attribute", "MUT204"),
        ("return_annotation", "MUT204"),
        ("items", "MUT204"),
        ("registry", "MUT204"),
        ("local_annotation", "MUT204"),
        ("narrower_class", "MUT204"),
        ("declared_over_other", "MUT204"),
        ("module_default", "MUT204"),
        ("class_attribute_annotation", "MUT204"),
        ("class_method", "MUT204"),
        ("class_attribute", "MUT203"),
        ("class_attribute", "EFF201"),
        ("named_class", "MUT203"),
        ("named_class", "EFF201"),
        ("through_module", "MUT203"),
        ("type_of", "MUT203"),
        ("getattr_class", "MUT203"),
        ("instance", "MUT203"),
        ("Builder.own_class", "MUT203"),
    ]);
    assert_eq!(reported(&p, &[], "pkg/uses.py"), expected);
}
