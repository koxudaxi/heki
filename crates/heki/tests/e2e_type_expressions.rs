//! What class a type expression names, end to end through the real binary: a name bound to a
//! type alias (`X: TypeAlias = A | B`, `type X = A | B`, an assignment of a subscript or a union
//! the module makes once) reads as its value wherever an annotation names it, in its module,
//! imported, or re-exported, also inside `type[...]`; a builtin container class subscripted with types and called (`frozenset[str]()`)
//! constructs the class; and `typing.cast` reads a subscripted type (`cast(list[Model], x)`) as
//! the same text in a string. A name that is no alias, an alias of type parameters, and a
//! subscript of any other class name nothing.
//!
//! Every fixture marks the lines an effect boundary reports: `# writes` for a write it forbids,
//! `# unknown` for a call it cannot resolve. Every other line must not be reported.

mod common;

use common::Project;
use serde_json::Value;

const CONFIG: &str = r#"[project]
name = "t"
requires-python = ">=3.12"

[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "passes"
scopes = ["pkg.passes:*"]
write-universe = ["Field.required"]
allow-writes = []
unresolved = "report"
"#;

const MODEL: &str = r#"from __future__ import annotations

import typing
from collections.abc import Callable
from typing import Generic, Literal, Optional, TypeAlias, TypeVar, Union

import typing_extensions


class Field:
    def __init__(self) -> None:
        self.required = False


class Added:
    def apply_to(self, field: Field) -> None:
        field.required = True


class Removed:
    def apply_to(self, field: Field) -> None:
        pass


class Kept:
    def apply_to(self, field: Field) -> None:
        pass


class Base:
    def run(self, field: Field) -> None:
        pass


class Windows(Base):
    def run(self, field: Field) -> None:
        pass


class Network(Base):
    def run(self, field: Field) -> None:
        field.required = True


T = TypeVar("T", bound=Added)


class Box(Generic[T]):
    def __init__(self) -> None:
        self.items: list[T] = []


Operation: TypeAlias = Added | Removed
Quoted: TypeAlias = "Added | Removed"
Spelled: typing.TypeAlias = typing.Union[Added, Removed]
Maybe: typing_extensions.TypeAlias = Optional[Added]
Implicit = Union[Added, Removed]
Operations = list[Added]
ByName = dict[str, Added]
OfAnAlias: TypeAlias = list[Operation]
Bounded = TypeVar("Bounded", bound=Operation)
Names = frozenset[str]
Hosts: TypeAlias = list[Windows]
Pairs: TypeAlias = tuple[Added, ...]
Forward: TypeAlias = list["Added"]
Applier: TypeAlias = Callable[[Field], None]
Empty: TypeAlias = tuple[()]
Kinds: TypeAlias = type[Added]

Rebound = Added | Kept
Rebound = Kept
Declared: object = Added | Kept
Text = "Added"
OfAVariable: TypeAlias = list[T]
Levels: TypeAlias = Literal[1, 2]
Computed: TypeAlias = type(None)
"#;

const STATEMENTS: &str = r#"from pkg.model import Added, Removed

type Statement = Added | Removed
type Pair[T] = tuple[T, T]
"#;

const PASSES: &str = r#"from __future__ import annotations

import collections
import typing
import typing as t
from collections import defaultdict
from typing import TypeAlias, cast

from pkg import model
from pkg.model import (
    Added,
    Applier,
    Base,
    Bounded,
    ByName,
    Declared,
    Field,
    Forward,
    Hosts,
    Implicit,
    Kept,
    Kinds,
    Maybe,
    Names,
    OfAnAlias,
    OfAVariable,
    Operation,
    Operations,
    Pairs,
    Quoted,
    Rebound,
    Removed,
    Spelled,
    Text,
    Windows,
)
from pkg.statements import Pair, Statement


def through_the_alias(field: Field, operation: Operation) -> None:
    operation.apply_to(field)  # writes


def through_a_quoted_alias(field: Field, operation: Quoted) -> None:
    operation.apply_to(field)  # writes


def through_a_spelled_alias(field: Field, operation: Spelled) -> None:
    operation.apply_to(field)  # writes


def through_an_optional_alias(field: Field, operation: Maybe) -> None:
    if operation is not None:
        operation.apply_to(field)  # writes


def through_an_implicit_alias(field: Field, operation: Implicit) -> None:
    operation.apply_to(field)  # writes


def through_a_statement(field: Field, operation: Statement) -> None:
    operation.apply_to(field)  # writes


def through_the_module(field: Field, operation: model.Operation) -> None:
    operation.apply_to(field)  # writes


def through_an_alias_bound_later(field: Field, operation: Local) -> None:
    operation.apply_to(field)  # writes


def through_a_union_with_an_alias(field: Field, operation: Operation | Kept) -> None:
    operation.apply_to(field)  # writes


def through_a_bound(field: Field, operation: Bounded) -> None:
    operation.apply_to(field)  # writes


def over_the_items(field: Field, operations: Operations) -> None:
    for operation in operations:
        operation.apply_to(field)  # writes


def over_the_values(field: Field, by_name: ByName) -> None:
    for operation in by_name.values():
        operation.apply_to(field)  # writes


def over_an_alias_of_an_alias(field: Field, operations: OfAnAlias) -> None:
    for operation in operations:
        operation.apply_to(field)  # writes


def over_items_an_alias_names(field: Field, operations: list[Operation]) -> None:
    for operation in operations:
        operation.apply_to(field)  # writes


def over_a_tuple_of_any_length(field: Field, operations: Pairs) -> None:
    for operation in operations:
        operation.apply_to(field)  # writes


def over_forward_references(field: Field, operations: Forward) -> None:
    for operation in operations:
        operation.apply_to(field)  # writes


def through_a_callable_alias(field: Field, apply: Applier) -> None:
    apply(field)  # unknown


def made(operation: object) -> Operation:
    return operation  # type: ignore[return-value]


def through_a_return(field: Field, value: object) -> None:
    made(value).apply_to(field)  # writes


def through_a_name_bound_twice(field: Field, operation: Rebound) -> None:
    operation.apply_to(field)  # unknown


def through_an_annotated_variable(field: Field, operation: Declared) -> None:
    operation.apply_to(field)  # unknown


def through_a_string_constant(field: Field, operation: Text) -> None:
    operation.apply_to(field)  # unknown


def over_the_items_of_a_type_variable(field: Field, operations: OfAVariable) -> None:
    for operation in operations:
        operation.apply_to(field)  # unknown


def over_a_generic_statement(field: Field, pair: Pair[Added]) -> None:
    for operation in pair:
        operation.apply_to(field)  # unknown


def through_a_union_with_a_class_object(field: Field, value: Kinds | Kept) -> None:
    value.apply_to(field)  # unknown


def builds_containers() -> int:
    names = frozenset[str]()
    seen = set[str]()
    items = list[int]()
    table = dict[str, int]()
    pair = tuple[int, ...]((1, 2))
    queue = collections.deque[int]()
    groups = defaultdict[str, list[int]](list)
    return len(names) + len(seen) + len(items) + len(table) + len(pair) + len(queue) + len(groups)


def builds_through_an_alias() -> frozenset[str]:
    return Names()


def builds_through_the_module() -> frozenset[str]:
    return model.Names()


def builds_through_an_alias_of_its_module() -> set[str]:
    return Strings()


def builds_a_project_generic() -> None:
    model.Box[model.Added]()  # unknown


def builds_through_typing() -> None:
    typing.List[int]()  # unknown


def casts_the_list(field: Field, hosts: list[Base]) -> None:
    for host in t.cast(list[Windows], hosts):
        host.run(field)


def casts_the_text(field: Field, hosts: list[Base]) -> None:
    for host in t.cast("list[Windows]", hosts):
        host.run(field)


def casts_a_mapping(field: Field, hosts: dict[str, Base]) -> None:
    for host in cast(dict[str, Windows], hosts).values():
        host.run(field)


def casts_an_optional(field: Field, host: Base | None) -> None:
    cast(Windows | None, host).run(field)


def casts_through_an_alias(field: Field, hosts: list[Base]) -> None:
    for host in cast(Hosts, hosts):
        host.run(field)


def casts_through_a_local(field: Field, hosts: list[Base]) -> None:
    kind = list[Windows]
    for host in cast(kind, hosts):
        host.run(field)


def casts_to_a_class_parameter(field: Field, host: Base, kind: type[Windows]) -> None:
    cast(kind, host).run(field)


def casts_to_an_item_of_classes(field: Field, host: Base) -> None:
    classes = (Windows,)
    cast(classes[0], host).run(field)


def casts_to_a_union_of_one_class(field: Field, host: Base) -> None:
    kind = Windows
    kind = kind | kind
    cast(kind, host).run(field)


def casts_to_a_union_past_the_bound(field: Field, host: Base) -> None:
    kind = Windows
    kind = kind | kind
    kind = kind | kind
    kind = kind | kind
    kind = kind | kind
    kind = kind | kind
    kind = kind | kind
    cast(kind, host).run(field)  # writes


def keeps_the_declared_items(field: Field, hosts: list[Base]) -> None:
    for host in hosts:
        host.run(field)  # writes


def casts_with_a_parameter(field: Field, hosts: list[Base], kind: type[Windows]) -> None:
    for host in cast(list[kind], hosts):  # type: ignore[valid-type]
        host.run(field)  # writes


Local: TypeAlias = Added | Removed
Strings = set[str]
"#;

/// `(line, mark)` of every `EFF201` report of `pkg/passes.py`: `writes` for a forbidden write,
/// `unknown` for a call heki cannot resolve.
fn reported(p: &Project) -> Vec<(u64, &'static str)> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<(u64, &'static str)> = diags
        .iter()
        .filter(|d| d["rule_code"] == "EFF201" && d["path"] == "pkg/passes.py")
        .map(|d| {
            let message = d["message"].as_str().unwrap();
            let mark = if message.contains("cannot resolve") {
                "unknown"
            } else {
                "writes"
            };
            (d["line"].as_u64().unwrap(), mark)
        })
        .collect();
    out.sort_unstable();
    out
}

/// `(line, mark)` of every line of `text` ending in `# writes` or `# unknown`.
fn marked(text: &str) -> Vec<(u64, &'static str)> {
    let mut out = Vec::new();
    for (i, line) in (1u64..).zip(text.lines()) {
        let line = line.trim_end();
        if line.ends_with("# writes") {
            out.push((i, "writes"));
        } else if line.ends_with("# unknown") {
            out.push((i, "unknown"));
        }
    }
    out
}

#[test]
fn a_type_expression_names_its_class_in_annotations_calls_and_casts() {
    let p = Project::new("type_expressions");
    p.write("pyproject.toml", CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", MODEL)
        .write("pkg/statements.py", STATEMENTS)
        .write("pkg/passes.py", PASSES);
    assert_eq!(reported(&p), marked(PASSES));
}

/// Aliases another module names, reached through re-exports and module aliases: an alias that is
/// also a module alias of its class (`Operations = list[Added]` binds the class `list`) keeps the
/// type of its items, and inside `type[...]` an alias or a bound type variable names its class.
/// A union inside `type[...]` names no one class.
const ALIASES_MODEL: &str = r#"from typing import TypeVar


class Field:
    def __init__(self) -> None:
        self.required = False


class Added:
    def apply_to(self, field: Field) -> None:
        field.required = True


class Removed:
    def apply_to(self, field: Field) -> None:
        pass


Operations = list[Added]
type Single = Added
type Either = Added | Removed
AddedT = TypeVar("AddedT", bound=Added)
"#;

const ALIASES_EXPORTS: &str = r#"from pkg.model import Operations, Single

Listed = Operations
"#;

const ALIASES_REEXPORTS: &str = "from pkg.exports import Listed, Operations, Single\n";

const ALIASES_PASSES: &str = r#"from pkg import exports, reexports
from pkg.model import AddedT, Either, Field, Operations, Single


def over_the_items(field: Field, operations: Operations) -> None:
    for operation in operations:
        operation.apply_to(field)  # writes


def over_reexported_items(field: Field, operations: exports.Operations) -> None:
    for operation in operations:
        operation.apply_to(field)  # writes


def over_items_reexported_twice(field: Field, operations: reexports.Operations) -> None:
    for operation in operations:
        operation.apply_to(field)  # writes


def over_items_of_a_reexported_module_alias(field: Field, operations: reexports.Listed) -> None:
    for operation in operations:
        operation.apply_to(field)  # writes


def builds_the_class_an_alias_names(field: Field, kind: type[Single]) -> None:
    kind().apply_to(field)  # writes


def builds_the_class_a_reexported_alias_names(field: Field, kind: type[reexports.Single]) -> None:
    kind().apply_to(field)  # writes


def builds_the_class_of_a_bound(field: Field, kind: type[AddedT]) -> None:
    kind().apply_to(field)  # writes


def builds_a_class_of_a_union(field: Field, kind: type[Either]) -> None:
    kind().apply_to(field)  # unknown


def builds_through_a_reexported_alias() -> int:
    return len(reexports.Operations())
"#;

#[test]
fn an_alias_of_another_module_keeps_its_type_through_reexports_and_class_objects() {
    let p = Project::new("type_expressions_reexported");
    p.write("pyproject.toml", CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", ALIASES_MODEL)
        .write("pkg/exports.py", ALIASES_EXPORTS)
        .write("pkg/reexports.py", ALIASES_REEXPORTS)
        .write("pkg/passes.py", ALIASES_PASSES);
    assert_eq!(reported(&p), marked(ALIASES_PASSES));
}

/// The reproduction of the request: a method called on a union a `TypeAlias` names writes what
/// the union written out does, and a function calling `frozenset[str]()` reaches nothing heki
/// cannot resolve.
const REQUEST_MODEL: &str = r#"from typing import TypeAlias


class Field:
    def __init__(self) -> None:
        self.required = False


class Added:
    def apply_to(self, field: Field) -> None:
        field.required = True


class Removed:
    def apply_to(self, field: Field) -> None:
        field.required = False


Operation: TypeAlias = Added | Removed


def makes_a_set() -> frozenset[str]:
    return frozenset[str]()
"#;

const REQUEST_PASSES: &str = r#"from pkg.model import Added, Field, Operation, Removed, makes_a_set


def through_the_alias(field: Field, operation: Operation) -> None:
    operation.apply_to(field)  # writes


def written_out(field: Field, operation: Added | Removed) -> None:
    operation.apply_to(field)  # writes


def builds_an_empty_set(field: Field) -> frozenset[str]:
    return makes_a_set()
"#;

#[test]
fn the_request_resolves_the_alias_and_the_subscripted_class() {
    let p = Project::new("type_expressions_request");
    p.write("pyproject.toml", CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", REQUEST_MODEL)
        .write("pkg/passes.py", REQUEST_PASSES);
    assert_eq!(reported(&p), marked(REQUEST_PASSES));
}

const DECLARED_MODEL: &str = r#"from typing import TypeAlias, TypeVar, Union


class Field:
    def __init__(self) -> None:
        self.required = False


class Added:
    def apply_to(self, field: Field) -> None:
        field.required = True


class Removed:
    def apply_to(self, field: Field) -> None:
        pass


class Renamed:
    def apply_to(self, field: Field) -> None:
        pass


class Moved:
    def apply_to(self, field: Field) -> None:
        pass


class Split:
    def apply_to(self, field: Field) -> None:
        pass


class Merged:
    def apply_to(self, field: Field) -> None:
        pass


class Dropped:
    def apply_to(self, field: Field) -> None:
        pass


class Kept:
    def apply_to(self, field: Field) -> None:
        pass


class Extra:
    def apply_to(self, field: Field) -> None:
        pass


class Overridden:
    def apply_to(self, field: Field) -> None:
        pass


class Twice(Overridden):
    def apply_to(self, field: Field) -> None:
        pass


Seven: TypeAlias = Added | Removed | Renamed | Moved | Split | Merged | Dropped
type Eight = Added | Removed | Renamed | Moved | Split | Merged | Dropped | Kept
Nine: TypeAlias = Union[Added, Removed, Renamed, Moved, Split, Merged, Dropped, Kept, Extra]
Crowded: TypeAlias = Added | Removed | Renamed | Moved | Split | Merged | Dropped | Overridden
Constrained = TypeVar("Constrained", Added, Removed, Renamed, Moved)


class Holder:
    operation: Seven


def made(operation: object) -> Seven:
    return operation  # type: ignore[return-value]
"#;

const DECLARED_PASSES: &str = r#"from typing import Optional, Union

from pkg.model import (
    Added,
    Constrained,
    Crowded,
    Eight,
    Extra,
    Field,
    Holder,
    Kept,
    Moved,
    Nine,
    Removed,
    Renamed,
    Seven,
    Split,
    made,
)


def through_an_alias_of_seven(field: Field, operation: Seven) -> None:
    operation.apply_to(field)  # writes


def through_a_statement_of_eight(field: Field, operation: Eight) -> None:
    operation.apply_to(field)  # writes


def written_out(field: Field, operation: Added | Removed | Renamed | Moved) -> None:
    operation.apply_to(field)  # writes


def spelled_with_none(field: Field, operation: Optional[Union[Added, Removed, Renamed, Moved]]) -> None:
    if operation is not None:
        operation.apply_to(field)  # writes


def an_alias_and_a_class(field: Field, operation: Seven | Kept) -> None:
    operation.apply_to(field)  # writes


def a_constrained_variable(field: Field, operation: Constrained) -> None:
    operation.apply_to(field)  # writes


def over_the_items(field: Field, operations: list[Seven]) -> None:
    for operation in operations:
        operation.apply_to(field)  # writes


def through_an_attribute(field: Field, holder: Holder) -> None:
    holder.operation.apply_to(field)  # writes


def through_a_return(field: Field, value: object) -> None:
    made(value).apply_to(field)  # writes


def a_store_of_a_member(field: Field, operation: Seven, flag: bool) -> None:
    if flag:
        operation = Removed()
    operation.apply_to(field)  # writes


def narrowed_away(field: Field, operation: Seven) -> None:
    if isinstance(operation, Added):
        return
    operation.apply_to(field)


def narrowed_to_four(field: Field, operation: Seven) -> None:
    if isinstance(operation, (Removed, Renamed, Moved, Split)):
        operation.apply_to(field)


def three_stores(field: Field, flag: int) -> None:
    if flag == 1:
        operation = Added()
    elif flag == 2:
        operation = Removed()
    else:
        operation = Renamed()
    operation.apply_to(field)  # writes


def nine_classes(field: Field, operation: Nine) -> None:
    operation.apply_to(field)  # unknown


def an_alias_and_two_classes(field: Field, operation: Seven | Kept | Extra) -> None:
    operation.apply_to(field)  # unknown


def more_targets_than_the_limit(field: Field, operation: Crowded) -> None:
    operation.apply_to(field)  # unknown


def a_store_of_another_class(field: Field, operation: Seven, flag: bool) -> None:
    if flag:
        operation = Kept()
    operation.apply_to(field)  # unknown


def four_stores(field: Field, flag: int) -> None:
    if flag == 1:
        operation = Added()
    elif flag == 2:
        operation = Removed()
    elif flag == 3:
        operation = Renamed()
    else:
        operation = Moved()
    operation.apply_to(field)  # unknown
"#;

/// A union an annotation declares keeps up to eight classes (`resolver::CHA_CANDIDATE_LIMIT`),
/// written out, spelled with `Union` and `None`, through a `TypeAlias`, a `type` statement, an
/// alias joined with a class, a constrained `TypeVar`, a container's items, an attribute's
/// annotation, and a return annotation: a call on it runs each member's method. A store of one
/// of its classes keeps it, and `isinstance` keeps the part it lets through. Nine classes, a
/// union whose calls run more than eight methods (a subclass overrides one), and a store of
/// another class have no tracked type. Unions joined from stores keep at most three classes.
#[test]
fn a_declared_union_keeps_up_to_the_candidate_limit() {
    let p = Project::new("type_expressions_declared_unions");
    p.write("pyproject.toml", CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", DECLARED_MODEL)
        .write("pkg/passes.py", DECLARED_PASSES);
    assert_eq!(reported(&p), marked(DECLARED_PASSES));
}

/// A parameter annotated with an alias of an iterator is a single-pass iterator, as one annotated
/// with the iterator itself (`MUT106`); an alias of a list is not.
const ROWS: &str = r#"from collections.abc import Iterator
from typing import TypeAlias

Rows: TypeAlias = Iterator[int]
Listed: TypeAlias = list[int]


def twice(rows: Rows) -> tuple[int, int]:
    return sum(rows), max(rows)


def a_list_twice(rows: Listed) -> tuple[int, int]:
    return sum(rows), max(rows)
"#;

#[test]
fn an_alias_of_an_iterator_is_consumed_once() {
    let p = Project::new("type_expressions_iterators");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT106\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write("pkg/rows.py", ROWS);
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
    assert_eq!(found, vec![(9, "MUT106")]);
}
