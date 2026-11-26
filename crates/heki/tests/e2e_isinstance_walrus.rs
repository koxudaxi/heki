//! `isinstance` narrows a value whatever class it had before, under every name of it: an
//! assignment expression's target names the value the test reads (`isinstance(v := x.f, C)`),
//! and so does a copy. An assignment expression in a comprehension or generator expression binds
//! the enclosing function's variable (PEP 572): before Python 3.12 a list comprehension has a
//! scope of its own, and a generator expression always does, so the variable is a closure cell
//! that scope stores. A read of it in that scope after its own store sees the stored value, with
//! what the scope's tests narrow it to, and a read in the enclosing function after the scope ran
//! sees what the scope stored. Reads a test does not guard on every path, and a variable stored
//! again with a value of unknown class, stay unknown.

mod common;

use common::Project;
use serde_json::Value;

const MODEL: &str = r#"from __future__ import annotations

from pkg.ref import Child


class DataModel(Child):
    _FLAG = True

    @property
    def _keys(self) -> bool:
        return self._FLAG

    @property
    def nullable(self) -> bool:
        return False
"#;

const REF: &str = r#"from __future__ import annotations

from typing import TYPE_CHECKING, Optional, Protocol


class Child:
    @property
    def reference(self) -> Reference | None:
        return None


if TYPE_CHECKING:

    class _Source(Child, Protocol):
        @property
        def nullable(self) -> bool:
            raise NotImplementedError

else:
    _Source = Child


class Reference:
    source: Optional[_Source] = None
"#;

const USE: &str = r#"from __future__ import annotations

from typing import TYPE_CHECKING

from pkg.model import DataModel

if TYPE_CHECKING:
    from pkg.ref import Reference


def walrus_or(references: list[Reference]) -> int:
    n = 0
    for reference in references:
        if not isinstance(root_model := reference.source, DataModel) or not root_model._keys:
            continue
        n += 1
    return n


def plain_or(references: list[Reference]) -> int:
    n = 0
    for reference in references:
        root_model = reference.source
        if not isinstance(root_model, DataModel) or not root_model._keys:
            continue
        n += 1
    return n


def walrus_and(reference: Reference) -> bool:
    return isinstance(root_model := reference.source, DataModel) and root_model._keys
"#;

const USE2: &str = r#"from __future__ import annotations

from typing import TYPE_CHECKING

from pkg.model import DataModel

if TYPE_CHECKING:
    from pkg.ref import Reference


class Holder:
    reference: Reference | None = None
    children: list[Holder] = []


def unknown_receiver(x) -> bool:  # noqa: ANN001
    return not isinstance(root_model := x.source, DataModel) or not root_model._keys


def cycle_pop(holders: list[Holder]) -> int:
    n = 0
    pending = [(holder, holder) for holder in holders]
    while pending:
        holder, parent = pending.pop()
        if (reference := holder.reference) is None:
            pending.extend((child, holder) for child in holder.children)
            continue
        if not isinstance(root_model := reference.source, DataModel) or not root_model._keys:
            continue
        pending.append((parent, holder))
        n += 1
    return n


def no_cycle_pop(holders: list[Holder]) -> int:
    n = 0
    pending = [(holder, holder) for holder in holders]
    while pending:
        holder, _parent = pending.pop()
        if (reference := holder.reference) is None:
            continue
        if not isinstance(root_model := reference.source, DataModel) or not root_model._keys:
            continue
        n += 1
    return n
"#;

const USE3: &str = r#"from __future__ import annotations

from typing import TYPE_CHECKING, Optional

from pkg.model import DataModel

if TYPE_CHECKING:
    from pkg.ref import Reference


class Holder:
    reference: Optional[Reference] = None
    key: Optional[Holder] = None


def walrus_in_comprehension(holders: list[Holder]) -> int:
    n = 0
    pending = [(key, holder) for holder in holders if (key := holder.key) is not None]
    while pending:
        data_type, _parent = pending.pop()
        if (reference := data_type.reference) is None:
            continue
        if not isinstance(root_model := reference.source, DataModel) or not root_model._keys:
            continue
        n += 1
    return n


def walrus_in_comprehension_no_tuple(holders: list[Holder]) -> int:
    n = 0
    for data_type in [key for holder in holders if (key := holder.key) is not None]:
        if (reference := data_type.reference) is None:
            continue
        if not isinstance(root_model := reference.source, DataModel) or not root_model._keys:
            continue
        n += 1
    return n


def walrus_in_comprehension_direct(holders: list[Holder]) -> bool:
    return any(key._keys for holder in holders if (key := holder.key) is not None)


def plain_comprehension(holders: list[Holder]) -> int:
    n = 0
    for data_type in [holder.key for holder in holders if holder.key is not None]:
        if (reference := data_type.reference) is None:
            continue
        if not isinstance(root_model := reference.source, DataModel) or not root_model._keys:
            continue
        n += 1
    return n
"#;

/// More shapes of the same reads.
const SHAPES: &str = r#"from __future__ import annotations

from typing import Optional

from pkg.model import DataModel


class Slot:
    model: Optional[DataModel] = None
    other: Optional[Slot] = None


class Box:
    source: Slot


def walrus_on_known_class(box: Box) -> bool:
    return isinstance(v := box.source, DataModel) and v._keys


def walrus_copied(x) -> bool:  # noqa: ANN001
    if isinstance(v := x.source, DataModel):
        w = v
        return w._keys
    return False


def any_then_enclosing_read(slots: list[Slot]) -> bool:
    if any((m := slot.model) is not None for slot in slots):
        return m._keys
    return False


def list_then_enclosing_read(slots: list[Slot]) -> bool:
    found = [slot for slot in slots if (m := slot.model) is not None]
    return bool(found) and m._keys


def element_read(slots: list[Slot]) -> list[bool]:
    return [m._keys for slot in slots if (m := slot.model) is not None]


def generator_element_read(slots: list[Slot]) -> bool:
    return any(m._keys for slot in slots if (m := slot.model) is not None)


def element_read_narrowed(slots) -> list[bool]:  # noqa: ANN001
    return [m._keys for slot in slots if isinstance(m := slot.model, DataModel)]


def two_comprehensions_then_enclosing_read(slots: list[Slot], others: list[Slot]) -> bool:
    found = [slot for slot in slots if (m := slot.model) is not None]
    more = [slot for slot in others if (m := slot.model) is not None]
    return bool(found or more) and m._keys


def element_read_after_a_later_store(slots: list[Slot], other) -> list[bool]:  # noqa: ANN001
    return [
        m._keys
        for slot in slots
        if (m := other.source)
        if (m := slot.model) is not None
        if slot.other or slot.model
    ]
"#;

/// Reads the tests leave unknown, or narrow to more than one class.
const NEGATIVES: &str = r#"from __future__ import annotations

from pkg.model import DataModel
from pkg.shapes import Slot


class Alpha:
    _keys = True


def union_of_classes(x) -> bool:  # noqa: ANN001
    if isinstance(v := x.source, (Alpha, DataModel)):
        return v._keys
    return False


def not_dominating(x, flag: bool) -> bool:  # noqa: ANN001
    v = x.source
    if flag and isinstance(v, DataModel):
        flag = False
    return v._keys


def walrus_not_dominating(x, flag: bool) -> bool:  # noqa: ANN001
    if isinstance(v := x.source, DataModel) or flag:
        return v._keys
    return False


def rebound_in_comprehension(slots: list[Slot], other) -> list[bool]:  # noqa: ANN001
    return [m._keys for slot in slots if (m := slot.model) is not None if (m := other.source)]


def rebound_then_enclosing_read(slots: list[Slot], other) -> bool:  # noqa: ANN001
    found = [slot for slot in slots if (m := slot.model) is not None if (m := other.source)]
    return bool(found) and m._keys


def stored_by_a_function_too(slots: list[Slot]) -> bool:
    found = any((m := slot.model) is not None for slot in slots)

    def reset() -> None:
        nonlocal m
        m = Alpha()

    reset()
    return found and m._keys
"#;

/// The read-owners and call-owners contracts over the private members of `DataModel` and
/// `Reference` (and the reads of those of `Alpha`), with the `pkg.use*`, `pkg.shapes`, and
/// `pkg.negatives` modules forbidden.
const CONFIG: &str = r#"[lint]
select = ["MUT203", "MUT204", "MUT206"]

[[read-owners]]
name = "host-private-read"
targets = ["pkg.model.DataModel._[!_]*", "pkg.ref.Reference._[!_]*", "pkg.negatives.Alpha._[!_]*"]
forbidden-readers = ["pkg.use:*", "pkg.use2:*", "pkg.use3:*", "pkg.shapes:*", "pkg.negatives:*"]
own-receiver = "allow"
unresolved = "report"
attribution = "lexical"

[[call-owners]]
name = "host-private-call"
targets = ["pkg.model.DataModel._[!_]*", "pkg.ref.Reference._[!_]*"]
forbidden-callers = ["pkg.use:*", "pkg.use2:*", "pkg.use3:*", "pkg.shapes:*", "pkg.negatives:*"]
own-receiver = "allow"
unresolved = "report"
attribution = "lexical"
"#;

const PYPROJECT: &str = r#"[project]
name = "pkg"
version = "0"
requires-python = ">=3.11"
"#;

fn project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("pyproject.toml", PYPROJECT)
        .write("heki.toml", CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", MODEL)
        .write("pkg/ref.py", REF)
        .write("pkg/use.py", USE)
        .write("pkg/use2.py", USE2)
        .write("pkg/use3.py", USE3)
        .write("pkg/shapes.py", SHAPES)
        .write("pkg/negatives.py", NEGATIVES);
    p
}

/// `(path:line, rule, qualname)` of every report of `heki check` for Python `version`.
fn reports(p: &Project, version: &str) -> Vec<(String, String, String)> {
    let mut out: Vec<_> = diagnostics(p, version)
        .iter()
        .map(|d| {
            (
                format!("{}:{}", d["path"].as_str().unwrap(), d["line"]),
                d["rule_code"].as_str().unwrap().to_owned(),
                d["qualname"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

/// The diagnostics of `heki check` for Python `version`.
fn diagnostics(p: &Project, version: &str) -> Vec<Value> {
    let (code, stdout, stderr) = p.check(&[
        "--output-format",
        "json",
        "--target-version",
        version,
        "pkg",
    ]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

fn r(at: &str, rule: &str, qualname: &str) -> (String, String, String) {
    (at.to_owned(), rule.to_owned(), qualname.to_owned())
}

/// The reads of `DataModel._keys` (`MUT203`, and the property call, `MUT204`) at `at`.
fn private_read(at: &str, qualname: &str) -> [(String, String, String); 2] {
    [r(at, "MUT203", qualname), r(at, "MUT204", qualname)]
}

/// The repro: `isinstance` narrows the walrus target of a value of unknown class
/// (`unknown_receiver`), and a walrus target bound in a comprehension condition keeps the class of
/// its value in the items built from it (`walrus_in_comprehension*`). `Holder` has no `_keys`, so
/// `walrus_in_comprehension_direct` reads no private member. A list comprehension has a scope of
/// its own before 3.12; a generator expression has one in every version.
#[test]
fn isinstance_narrows_a_walrus_target_and_a_comprehension_keeps_its_class() {
    let p = project("isinstance_walrus_repro");
    for version in ["py311", "py312"] {
        let found: Vec<_> = reports(&p, version)
            .into_iter()
            .filter(|(at, _, _)| at.starts_with("pkg/use"))
            .collect();
        let expected: Vec<_> = [
            private_read("pkg/use.py:14", "walrus_or"),
            private_read("pkg/use.py:24", "plain_or"),
            private_read("pkg/use.py:31", "walrus_and"),
            private_read("pkg/use2.py:17", "unknown_receiver"),
            private_read("pkg/use2.py:28", "cycle_pop"),
            private_read("pkg/use2.py:42", "no_cycle_pop"),
            private_read("pkg/use3.py:23", "walrus_in_comprehension"),
            private_read("pkg/use3.py:34", "walrus_in_comprehension_no_tuple"),
            private_read("pkg/use3.py:49", "plain_comprehension"),
        ]
        .into_iter()
        .flatten()
        .collect();
        assert_eq!(found, expected, "{version}");
    }
}

/// The walrus target of a value of a known class unrelated to the tested one, a copy of a
/// narrowed target, the enclosing function's reads after a generator expression or a list
/// comprehension stored the variable (two of them here), and the reads in their elements,
/// narrowed by `isinstance` too, and seeing the last of the comprehension's stores on every path
/// (also through an `or`).
#[test]
fn every_name_of_a_narrowed_or_stored_value_has_its_class() {
    let p = project("isinstance_walrus_shapes");
    for version in ["py311", "py312"] {
        let found: Vec<_> = reports(&p, version)
            .into_iter()
            .filter(|(at, _, _)| at.starts_with("pkg/shapes.py"))
            .collect();
        let expected: Vec<_> = [
            private_read("pkg/shapes.py:18", "walrus_on_known_class"),
            private_read("pkg/shapes.py:24", "walrus_copied"),
            private_read("pkg/shapes.py:30", "any_then_enclosing_read"),
            private_read("pkg/shapes.py:36", "list_then_enclosing_read"),
            private_read("pkg/shapes.py:40", "element_read"),
            private_read("pkg/shapes.py:44", "generator_element_read"),
            private_read("pkg/shapes.py:48", "element_read_narrowed"),
            private_read("pkg/shapes.py:54", "two_comprehensions_then_enclosing_read"),
            private_read("pkg/shapes.py:59", "element_read_after_a_later_store"),
        ]
        .into_iter()
        .flatten()
        .collect();
        assert_eq!(found, expected, "{version}");
    }
}

/// A tuple of classes narrows to their union, which reads the members of both; a test that does
/// not hold on every path to the read narrows nothing there; a variable a comprehension stores
/// again with a value of unknown class is unknown in it and after it, and so is one a nested
/// function stores too (`nonlocal`).
#[test]
fn narrowing_keeps_every_class_and_only_where_the_test_holds() {
    let p = project("isinstance_walrus_negatives");
    for version in ["py311", "py312"] {
        let found: Vec<_> = reports(&p, version)
            .into_iter()
            .filter(|(at, _, _)| at.starts_with("pkg/negatives.py"))
            .collect();
        let expected = [
            r("pkg/negatives.py:13", "MUT203", "union_of_classes"),
            r("pkg/negatives.py:13", "MUT204", "union_of_classes"),
            r("pkg/negatives.py:21", "MUT206", "not_dominating"),
            r("pkg/negatives.py:26", "MUT206", "walrus_not_dominating"),
            r("pkg/negatives.py:31", "MUT206", "rebound_in_comprehension"),
            r(
                "pkg/negatives.py:36",
                "MUT206",
                "rebound_then_enclosing_read",
            ),
            r("pkg/negatives.py:47", "MUT206", "stored_by_a_function_too"),
        ];
        assert_eq!(found, expected, "{version}");
        // The one read is of both classes' members.
        let union_read = diagnostics(&p, version)
            .into_iter()
            .find(|d| d["qualname"] == "union_of_classes" && d["rule_code"] == "MUT203")
            .unwrap();
        let message = union_read["message"].as_str().unwrap();
        assert!(
            message.contains("`pkg.model.DataModel._keys`")
                && message.contains("`pkg.negatives.Alpha._keys`"),
            "{version}: {message}"
        );
    }
}
