//! `MUT120` (`readonly-view-mutation`) end to end: a value held through a read-only view (a
//! protocol or class of properties without a setter) handed to a helper that rebinds or deletes
//! a property, one and two calls deep, and the container a property returns changed in place,
//! directly, through an alias, and through a helper, also on the elements of a `Sequence` of
//! views another view returns; every exclusion (a setter, a plain attribute, a method, a cached
//! property, a helper whose parameter is the view, a `Sequence` / `Mapping` / `AbstractSet`
//! property changed directly, copies, undeclared attributes, the implementing class), the split
//! from `MUT108`, the opt-in selection, suppressions, and `heki rule`. The incremental language
//! server's steps are in `lsp_oracle.rs` (`mut120`).

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

/// `(code, line, qualname, target)` of every diagnostic.
fn found(diags: &[Value]) -> Vec<(String, u64, String, String)> {
    diags
        .iter()
        .map(|d| {
            (
                d["rule_code"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["qualname"].as_str().unwrap().to_owned(),
                d["target"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn d(code: &str, line: u64, qualname: &str, target: &str) -> (String, u64, String, String) {
    (code.into(), line, qualname.into(), target.into())
}

fn at(diags: &[Value], line: u64) -> &Value {
    diags
        .iter()
        .find(|d| d["line"] == line)
        .unwrap_or_else(|| panic!("no diagnostic at line {line}: {diags:#?}"))
}

/// `(path, line)` of a diagnostic's related locations (its call chain).
fn chain(diag: &Value) -> Vec<(String, u64)> {
    diag["related"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["path"].as_str().unwrap().to_owned(),
                r["line"].as_u64().unwrap(),
            )
        })
        .collect()
}

// The views, and classes that are no read-only views.
const VIEWS: &str = r#"from __future__ import annotations

import abc
from collections.abc import Mapping, Sequence
from functools import cached_property
from typing import Protocol, TypeVar

T = TypeVar("T")


class FieldView(Protocol):
    @property
    def required(self) -> bool: ...

    @property
    def extras(self) -> dict[str, object]: ...

    @property
    def kwargs(self) -> Mapping[str, object]: ...

    @property
    def tables(self) -> dict[str, list[int]]: ...


class ModelView(Protocol):
    @property
    def fields(self) -> Sequence[FieldView]: ...


class NamedView(FieldView, Protocol):
    @property
    def name(self) -> str: ...


class BoxView(Protocol[T]):
    @property
    def items(self) -> list[T]: ...


class TagsView(abc.ABC):
    @property
    @abc.abstractmethod
    def tags(self) -> list[str]: ...


class WithSetter(Protocol):
    @property
    def required(self) -> bool: ...

    @required.setter
    def required(self, value: bool) -> None: ...

    @property
    def extras(self) -> dict[str, object]: ...


class WithPlain(Protocol):
    required: bool

    @property
    def extras(self) -> dict[str, object]: ...


class WithMethod(Protocol):
    @property
    def extras(self) -> dict[str, object]: ...

    def refresh(self) -> None: ...


class WithCached:
    @cached_property
    def extras(self) -> dict[str, object]:
        return {}
"#;

const HELPERS: &str = r#"from typing import Any

from pkg.views import FieldView


def flip(field):
    field.required = not field.required


def drop(field):
    del field.required


def set_flag(field):
    setattr(field, "required", True)


def flip_twice(field):
    flip(field)


def set_extra(field):
    field.extras["k"] = 1


def fill(extras):
    extras["k"] = 1


def read(field):
    return field.required


def any_flip(field: Any) -> None:
    field.required = True


def typed_flip(field: FieldView) -> None:
    flip(field)


def typed_write(field: FieldView) -> None:
    field.required = True  # ty: ignore[invalid-assignment]


def cache_on(field):
    field.cache = {}
"#;

const USE: &str = r#"from pkg.helpers import (
    any_flip,
    cache_on,
    drop,
    fill,
    flip,
    flip_twice,
    read,
    set_extra,
    set_flag,
    typed_flip,
    typed_write,
)
from pkg.views import (
    BoxView,
    FieldView,
    ModelView,
    NamedView,
    TagsView,
    WithCached,
    WithMethod,
    WithPlain,
    WithSetter,
)


def writes_through_the_view(model: ModelView) -> None:
    for field in model.fields:
        flip(field)


def changes_a_viewed_dict(model: ModelView) -> None:
    for field in model.fields:
        field.extras["init"] = False


def deletes(field: FieldView) -> None:
    drop(field)


def sets_by_name(field: FieldView) -> None:
    set_flag(field)


def two_deep(field: FieldView) -> None:
    flip_twice(field)


def any_helper(field: FieldView) -> None:
    any_flip(field)


def alias(field: FieldView) -> None:
    extras = field.extras
    extras["k"] = 1


def slot_helper(field: FieldView) -> None:
    set_extra(field)


def inherited(view: NamedView) -> None:
    view.extras.clear()


def generic(box: BoxView[int]) -> None:
    box.items.append(1)


def abstract_base(view: TagsView) -> None:
    view.tags.append("x")


def read_only_type_to_helper(field: FieldView) -> None:
    set_kwargs(field)


def set_kwargs(field):
    field.kwargs["k"] = 1


def view_typed_helper(field: FieldView) -> None:
    typed_flip(field)
    typed_write(field)


def reader(field: FieldView) -> bool:
    return read(field)


def copies(field: FieldView) -> None:
    extras = dict(field.extras)
    extras["k"] = 1
    fill(dict(field.extras))


def read_only_type_directly(field: FieldView) -> None:
    field.kwargs["k"] = 1  # ty: ignore[invalid-assignment]


def nested(field: FieldView) -> None:
    field.tables["a"].append(1)


def undeclared(field: FieldView) -> None:
    cache_on(field)


def no_views(a: WithSetter, b: WithPlain, c: WithMethod, d: WithCached) -> None:
    flip(a)
    a.extras["k"] = 1
    flip(b)
    b.extras["k"] = 1
    c.extras["k"] = 1
    d.extras["k"] = 1
"#;

fn view_project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("pkg/__init__.py", "")
        .write("pkg/views.py", VIEWS)
        .write("pkg/helpers.py", HELPERS)
        .write("pkg/use.py", USE);
    p
}

#[test]
fn mut120_helper_writes_and_returned_containers() {
    let p = view_project("mut120_views");
    let diags = diagnostics(&p, &["--select", "MUT120"]);
    assert_eq!(
        found(&diags),
        vec![
            // `typed_flip` sees the view and hands it to `flip`, which does not.
            d("MUT120", 39, "typed_flip", "FieldView.required"),
            d(
                "MUT120",
                29,
                "writes_through_the_view",
                "FieldView.required"
            ),
            d("MUT120", 34, "changes_a_viewed_dict", "FieldView.extras"),
            d("MUT120", 38, "deletes", "FieldView.required"),
            d("MUT120", 42, "sets_by_name", "FieldView.required"),
            d("MUT120", 46, "two_deep", "FieldView.required"),
            d("MUT120", 50, "any_helper", "FieldView.required"),
            d("MUT120", 55, "alias", "FieldView.extras"),
            d("MUT120", 59, "slot_helper", "FieldView.extras"),
            d("MUT120", 63, "inherited", "FieldView.extras"),
            d("MUT120", 67, "generic", "BoxView.items"),
            d("MUT120", 71, "abstract_base", "TagsView.tags"),
            // Through a helper, a property annotated `Mapping` counts.
            d("MUT120", 75, "read_only_type_to_helper", "FieldView.kwargs"),
        ]
    );
    let flipped = at(&diags, 29);
    assert_eq!(
        flipped["message"],
        "`writes_through_the_view` hands a `FieldView`, a read-only view, to `pkg.helpers.flip`, which rebinds or deletes its property `required`"
    );
    assert_eq!(flipped["certainty"], "heuristic");
    assert_eq!(chain(flipped), vec![("pkg/use.py".to_owned(), 29)]);
    assert!(
        flipped["help"]
            .as_str()
            .unwrap()
            .contains("annotate the helper's parameter with `FieldView`"),
        "{flipped:#?}"
    );
    // Two calls deep: the chain goes down to the helper whose body rebinds the property.
    assert_eq!(
        chain(at(&diags, 46)),
        vec![
            ("pkg/use.py".to_owned(), 46),
            ("pkg/helpers.py".to_owned(), 19)
        ]
    );
    assert_eq!(
        at(&diags, 34)["message"],
        "`changes_a_viewed_dict` changes `FieldView.extras`, a property of a read-only view, in place"
    );
    assert!(
        at(&diags, 59)["message"]
            .as_str()
            .unwrap()
            .ends_with("in place via `pkg.helpers.set_extra`"),
        "{diags:#?}"
    );
}

#[test]
fn mut120_and_mut108_report_their_own_kinds() {
    let p = Project::new("mut120_split");
    p.write("pkg/__init__.py", "").write(
        "pkg/m.py",
        r#"from dataclasses import dataclass, field
from typing import Protocol


class View(Protocol):
    @property
    def tags(self) -> list[str]: ...


@dataclass(frozen=True)
class Frozen:
    labels: list[str] = field(default_factory=list)


def f(v: View, c: Frozen) -> None:
    v.tags.append("a")
    c.labels.append("a")
"#,
    );
    let both = ["--select", "MUT108,MUT120"];
    assert_eq!(
        found(&diagnostics(&p, &both)),
        vec![
            d("MUT120", 16, "f", "View.tags"),
            d("MUT108", 17, "f", "Frozen.labels")
        ]
    );
    assert_eq!(
        found(&diagnostics(&p, &["--select", "MUT108"])),
        vec![d("MUT108", 17, "f", "Frozen.labels")]
    );
    assert_eq!(
        found(&diagnostics(&p, &["--select", "MUT120"])),
        vec![d("MUT120", 16, "f", "View.tags")]
    );
}

#[test]
fn mut120_is_opt_in_and_honors_suppressions() {
    let p = Project::new("mut120_opt_in");
    p.write("pkg/__init__.py", "").write(
        "pkg/v.py",
        "from typing import Protocol\n\n\nclass View(Protocol):\n    @property\n    def tags(self) -> list[str]: ...\n\n\ndef f(v: View) -> None:\n    v.tags.append(\"a\")\n    v.tags.append(\"b\")  # heki: ignore[MUT120]\n    v.tags.append(\"c\")  # noqa: MUT120\n    v.tags.append(\"d\")  # heki: ignore[readonly-view-mutation]\n",
    );
    // Neither the default rules, `ALL`, nor the `MUT` prefix enable it; its code or slug does.
    assert!(diagnostics(&p, &[]).is_empty());
    assert!(diagnostics(&p, &["--select", "MUT"]).is_empty());
    assert!(diagnostics(&p, &["--select", "ALL"]).is_empty());
    let expected = vec![d("MUT120", 10, "f", "View.tags")];
    assert_eq!(
        found(&diagnostics(&p, &["--extend-select", "MUT120"])),
        expected
    );
    assert_eq!(
        found(&diagnostics(
            &p,
            &["--extend-select", "readonly-view-mutation"]
        )),
        expected
    );
    p.write("heki.toml", "[lint]\nextend-select = [\"MUT120\"]\n");
    assert_eq!(found(&diagnostics(&p, &[])), expected);
    assert!(diagnostics(&p, &["--ignore", "MUT120"]).is_empty());

    let (code, doc, _) = p.heki(&["rule", "MUT120"]);
    assert_eq!(code, 0);
    assert!(
        doc.starts_with("# readonly-view-mutation (MUT120)\n\n"),
        "{doc}"
    );
    let sections = [
        "\n## What it does\n",
        "\n## What a report states\n",
        "\n## Why is this bad?\n",
        "\n## Example\n",
        "\nUse instead:\n",
        "\n## Fix safety\nNo automatic fix.\n",
        "\n## Options\n",
    ];
    let positions: Vec<usize> = sections
        .iter()
        .map(|s| doc.find(s).unwrap_or_else(|| panic!("{s}: {doc}")))
        .collect();
    assert!(positions.is_sorted(), "{doc}");
}

/// A property annotated `AbstractSet` / `collections.abc.Set` promises what one annotated
/// `Mapping` does: changed through a helper it counts, changed through the view the type checker
/// reports it.
const SET_VIEW: &str = r#"from collections.abc import Set
from typing import AbstractSet, Protocol


class TagsView(Protocol):
    @property
    def tags(self) -> AbstractSet[str]: ...

    @property
    def labels(self) -> Set[str]: ...


def tag(view):
    view.tags.add("x")


def handed_to_a_helper(view: TagsView) -> None:
    tag(view)


def through_the_view(view: TagsView) -> None:
    view.labels.add("x")
"#;

#[test]
fn mut120_reads_set_abc_properties_as_read_only() {
    let p = Project::new("mut120_set_view");
    p.write("pkg/__init__.py", "")
        .write("pkg/views.py", SET_VIEW);
    let diags = diagnostics(&p, &["--select", "MUT120"]);
    assert_eq!(
        found(&diags),
        vec![d("MUT120", 18, "handed_to_a_helper", "TagsView.tags")]
    );
    assert_eq!(
        diags[0]["message"],
        "`handed_to_a_helper` changes `TagsView.tags`, a property of a read-only view, in place via `pkg.views.tag`"
    );
}
