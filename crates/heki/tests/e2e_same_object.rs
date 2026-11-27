//! A value a call hands back as itself is the object it was handed, for every reader of the
//! alias facts: `typing.cast(T, value)` (also `typing_extensions.cast`, and `t.cast` through a
//! module alias), a project function that returns its parameter on every path, one that returns
//! a cast of it, one that returns what such a function returns, and chains of them. Each row of [`ROWS`] changes a value written in every form
//! of [`SAME`], and every form reports what the plain form reports, with the same certainty.
//! The forms of [`OTHER`] hand back another object (a copy, another value, the argument on one
//! path only) and report nothing. A form that should report and does not is a gap of
//! [`GAPS`]; the list is exact, so a gap that closes fails the test until its line is removed.

mod common;

use common::Project;
use serde_json::Value;
use std::collections::BTreeSet;

/// One change, and the value it changes (`base`), written as `{v}` in `usage`.
struct Row {
    name: &'static str,
    base: &'static str,
    usage: &'static str,
}

/// The rules the forms must agree on (a change of a copy that nothing keeps is `MUT119`'s).
const RULES: &[&str] = &["MUT108", "MUT110", "MUT201", "MUT205"];

const ROWS: &[Row] = &[
    Row {
        name: "mut110_hand_off",
        base: "items",
        usage: "fill({v})",
    },
    Row {
        name: "mut108_in_place",
        base: "cfg.tags",
        usage: "{v}.append(\"x\")",
    },
    Row {
        name: "mut108_hand_off",
        base: "cfg.tags",
        usage: "fill({v})",
    },
    Row {
        name: "mut201_in_place",
        base: "store.items",
        usage: "{v}.append(2)",
    },
    Row {
        name: "mut201_hand_off",
        base: "store.items",
        usage: "fill({v})",
    },
];

/// A form of the value: statements before the change, and the expression it changes, `{p}`
/// standing for the row's value.
type Form = (&'static str, &'static str, &'static str);

/// The forms that hand back the row's value itself. The first is the plain form.
const SAME: &[Form] = &[
    ("plain", "", "{p}"),
    ("alias", "xs = {p}\n    ", "xs"),
    ("cast", "", "cast(list, {p})"),
    ("cast_subscripted", "", "cast(list[int], {p})"),
    ("cast_any", "", "cast(Any, {p})"),
    ("cast_bound", "xs = cast(list, {p})\n    ", "xs"),
    ("module_alias_cast", "", "t.cast(list, {p})"),
    (
        "typing_extensions_cast",
        "",
        "typing_extensions.cast(list, {p})",
    ),
    ("pass_through", "", "same({p})"),
    ("pass_through_bound", "xs = same({p})\n    ", "xs"),
    ("returns_a_cast", "", "same_cast({p})"),
    ("chain", "", "same(cast(list, same({p})))"),
    ("pass_through_of_a_pass_through", "", "via_same({p})"),
];

/// The forms that hand back another object, or the row's value on some paths only.
const OTHER: &[Form] = &[
    ("copy", "", "copy_of({p})"),
    ("cast_of_another_value", "", "cast(list, other)"),
    ("one_path_only", "", "maybe({p}, flag)"),
    ("copy_in_a_cast", "", "cast(list, list({p}))"),
];

/// Forms that hand back the row's value but report nothing in any row.
const GAPS: &[Form] = &[];

const HELPERS: &str = "\
from typing import cast


def fill(xs):
    if isinstance(xs, list):
        xs.append(1)


def same(x):
    return x


def same_cast(x) -> list[int]:
    return cast(list[int], x)


def via_same(x):
    return same(x)


def copy_of(x):
    return list(x)


def maybe(x, flag):
    if flag:
        return x
    return [1]
";

const STORE: &str = "\
from dataclasses import dataclass, field


class Store:
    def __init__(self) -> None:
        self.items: list[int] = []


@dataclass(frozen=True)
class Config:
    tags: list[str] = field(default_factory=list)
";

const HEAD: &str = "\
from collections.abc import Sequence
import typing as t
from typing import Any, cast

import typing_extensions

from app.helpers import copy_of, fill, maybe, same, same_cast, via_same
from app.store import Config, Store
";

const PYPROJECT: &str = "\
[project]
name = \"t\"
requires-python = \">=3.12\"

[[tool.heki.write-owners]]
name = \"store\"
targets = [\"app.store:Store.items\"]
allowed-writers = [\"app.store:Store.*\"]
unresolved = \"report\"
";

/// The name of the function writing `row` in `form`.
fn function(row: &Row, form: &Form) -> String {
    format!("{}__{}", row.name, form.0)
}

/// `app/use.py`: one function per row and form.
fn uses() -> String {
    let mut out = HEAD.to_owned();
    for row in ROWS {
        for form in SAME.iter().chain(OTHER).chain(GAPS) {
            let (_, prelude, expr) = form;
            let value = expr.replace("{p}", row.base);
            let body = format!(
                "{}{}",
                prelude.replace("{p}", row.base),
                row.usage.replace("{v}", &value)
            );
            out.push_str(&format!(
                "\n\ndef {}(items: Sequence[int], cfg: Config, store: Store, other: list[int], flag: bool) -> None:\n    {body}\n",
                function(row, form),
            ));
        }
    }
    out
}

fn project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("pyproject.toml", PYPROJECT)
        .write("app/__init__.py", "")
        .write("app/helpers.py", HELPERS)
        .write("app/store.py", STORE);
    p
}

/// The reports of every rule (`MUT108` runs only when named) in `path`, as
/// `(function, code, certainty)`.
fn reports(p: &Project, path: &str) -> BTreeSet<(String, String, String)> {
    let (_, out, err) = p.heki(&[
        "check",
        "--select",
        "ALL",
        "--extend-select",
        "MUT108",
        "--heuristic",
        "--output-format",
        "json",
        ".",
    ]);
    let all: Vec<Value> = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {err}"));
    let text = |d: &Value, key: &str| d[key].as_str().unwrap_or_default().to_owned();
    all.iter()
        .filter(|d| d["path"] == path)
        .map(|d| {
            (
                text(d, "qualname"),
                text(d, "rule_code"),
                text(d, "certainty"),
            )
        })
        .collect()
}

#[test]
fn a_value_handed_back_as_itself_is_the_same_object() {
    let p = project("same_object_forms");
    p.write("app/use.py", &uses());
    let found = reports(&p, "app/use.py");
    let of = |name: &str| -> BTreeSet<(String, String)> {
        (found.iter())
            .filter(|(f, code, _)| f == name && RULES.contains(&code.as_str()))
            .map(|(_, code, certainty)| (code.clone(), certainty.clone()))
            .collect()
    };
    let mut wrong = Vec::new();
    for row in ROWS {
        let plain = of(&function(row, &SAME[0]));
        assert!(
            !plain.is_empty(),
            "{}: the plain form reports nothing",
            row.name
        );
        for form in SAME {
            let got = of(&function(row, form));
            if got != plain {
                wrong.push(format!("{}: {got:?}, plain {plain:?}", function(row, form)));
            }
        }
        for form in OTHER.iter().chain(GAPS) {
            let got = of(&function(row, form));
            if !got.is_empty() {
                wrong.push(format!(
                    "{}: {got:?}, expected nothing",
                    function(row, form)
                ));
            }
        }
    }
    assert!(wrong.is_empty(), "{}", wrong.join("\n"));
}

/// The type checker reports a read-only parameter handed to a helper annotated with a mutable
/// type, and a change of it in the function itself. A value a call handed back is typed by that
/// call, so neither is a type checker's report, and `MUT110` reports both.
#[test]
fn mut110_does_not_leave_a_handed_back_value_to_the_type_checker() {
    let p = project("same_object_mut110");
    p.write(
        "app/use.py",
        &format!(
            "{HEAD}import sys

from app.helpers import fill_list


def to_annotated(items: Sequence[int]) -> None:
    fill_list(items)


def to_annotated_cast(items: Sequence[int]) -> None:
    fill_list(cast(list, items))


def to_annotated_pass_through(items: Sequence[int]) -> None:
    fill_list(same(items))


def own_change(items: Sequence[int]) -> None:
    if isinstance(items, list):
        items.append(1)


def own_change_cast(items: Sequence[int]) -> None:
    cast(list, items).append(1)


def own_change_module_alias_cast(items: Sequence[int]) -> None:
    xs = t.cast(list, items)
    xs.append(1)


def own_change_returns_a_cast(items: Sequence[int]) -> None:
    same_cast(items).append(1)


def own_change_pass_through(items: Sequence[int]) -> None:
    xs = same(items)
    if isinstance(xs, list):
        xs[0] = 1


def own_change_cast_any(items: Sequence[int]) -> None:
    cast(Any, items).append(1)


def own_change_cast_subscripted(items: Sequence[int]) -> None:
    cast(list[int], items).append(1)


def own_change_of_a_copy(items: Sequence[int]) -> None:
    cast(list, copy_of(items)).append(1)


def own_change_of_another_parameter(items: Sequence[int], other: list[int]) -> None:
    cast(list, other).append(1)


def own_change_of_an_unpacked_item(items: Sequence[list[int]]) -> None:
    cast(list, *items).append(1)


if sys.version_info >= (3, 13):
    def own_change_defined_twice(items: Sequence[int]) -> None:
        cast(list, items).append(2)
else:
    def own_change_defined_twice(items: Sequence[int]) -> None:
        cast(list, items).append(1)
"
        ),
    )
    .write(
        "app/helpers.py",
        &format!("{HELPERS}\n\ndef fill_list(xs: list[int]) -> None:\n    xs.append(1)\n"),
    );
    let got: BTreeSet<(String, String, String)> = reports(&p, "app/use.py")
        .into_iter()
        .filter(|(_, code, _)| code == "MUT110")
        .collect();
    // `cast(Any, items)` keeps `Sequence`: the value is no builtin container there, and the
    // change stays open. A subscripted cast type is read as an annotation. `cast(list, *items)`
    // hands back an item of `items`, not `items`.
    let expected: BTreeSet<(String, String, String)> = [
        "to_annotated_cast",
        "to_annotated_pass_through",
        "own_change_cast",
        "own_change_cast_subscripted",
        "own_change_module_alias_cast",
        "own_change_returns_a_cast",
        "own_change_pass_through",
        "own_change_defined_twice",
    ]
    .into_iter()
    .map(|f| (f.to_owned(), "MUT110".to_owned(), "exact".to_owned()))
    .collect();
    assert_eq!(got, expected);
    let (_, out, _) = p.check(&["--select", "MUT110", "app/use.py"]);
    assert!(
        out.contains(
            "`own_change_cast` changes its read-only parameter `items` in place, through a value a call hands back as itself"
        ),
        "{out}"
    );
}

/// `heki inspect` reads the same facts: a function changing its parameter through a cast is not
/// pure, and one storing what a cast or a pass-through function hands back stores its parameter.
#[test]
fn inspect_sees_a_change_and_a_store_through_a_handed_back_value() {
    let p = project("same_object_inspect");
    p.write(
        "app/use.py",
        &format!(
            "{HEAD}

def fill_through_cast(xs):
    if isinstance(xs, list):
        ys = cast(list[int], xs)
        ys.append(1)


def fill_through_maybe(xs, flag):
    ys: list[int] = maybe(xs, flag)
    ys.append(1)


class Box:
    def __init__(self) -> None:
        self.items: list[int] = []

    def keep_cast(self, items: list[int]) -> None:
        self.items = cast(list, items)

    def keep_pass_through(self, items: list[int]) -> None:
        self.items = same(items)

    def keep_copy(self, items: list[int]) -> None:
        self.items = copy_of(items)
"
        ),
    );
    let inspect = |name: &str| -> Value {
        let (_, out, err) = p.heki(&["inspect", "--output-format", "json", name]);
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {err}"))
    };
    let through_cast = inspect("app.use.fill_through_cast");
    assert_eq!(
        through_cast["purity"]["verdict"], "impure",
        "{through_cast}"
    );
    assert_eq!(through_cast["changes"]["params"][0]["param"], "xs");
    // The argument on one path only: an object of unknown origin, as the verdict assumes.
    let through_maybe = inspect("app.use.fill_through_maybe");
    assert_eq!(through_maybe["changes"]["params"], Value::Array(Vec::new()));
    for (method, stored) in [
        ("keep_cast", vec!["items"]),
        ("keep_pass_through", vec!["items"]),
        ("keep_copy", vec![]),
    ] {
        let got = inspect(&format!("app.use.Box.{method}"));
        assert_eq!(
            got["retains"]["stored"],
            serde_json::json!(stored),
            "{method}"
        );
    }
}

/// An attribute of a parameter that a function returns through a call of another function
/// returning it (`items_via`), or after code that stores no attribute (`checked`), is that
/// attribute: a change of what the call returns changes the argument's attribute. A function
/// storing an attribute after the call (`take_items`), or calling a function that may store it
/// between the read and the return (`kept_through_reset`, `kept_directly`), may return what the
/// attribute held before, so its result is an object of unknown origin.
#[test]
fn an_attribute_returned_through_a_call_is_the_attribute() {
    let p = project("same_object_attribute_through_a_call");
    p.write(
        "app/attrs.py",
        "\
def items_of(s):
    return s.items


def items_via(s):
    return items_of(s)


def take_items(s, other):
    items = items_of(s)
    other.items = []
    return items


def touch_via(s, other):
    items_via(s).append(1)


def touch_taken(s, other):
    take_items(s, other).append(1)


def reset(s):
    s.items = []


def kept_through_reset(s):
    items = items_of(s)
    reset(s)
    return items


def kept_directly(s):
    items = s.items
    reset(s)
    return items


def checked(s):
    items = s.items
    if items is None:
        raise ValueError
    return items


def touch_kept(s):
    kept_through_reset(s).append(1)


def touch_kept_directly(s):
    kept_directly(s).append(1)


def touch_checked(s):
    checked(s).append(1)
",
    );
    let changed = |name: &str| -> Vec<(String, Value)> {
        let (_, out, err) = p.heki(&["inspect", "--output-format", "json", name]);
        let got: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {err}"));
        (got["changes"]["params"].as_array().unwrap().iter())
            .map(|c| (c["param"].as_str().unwrap().to_owned(), c["what"].clone()))
            .collect()
    };
    let items = || serde_json::json!(["items"]);
    assert_eq!(changed("app.attrs.touch_via"), [("s".to_owned(), items())]);
    assert_eq!(
        changed("app.attrs.touch_taken"),
        [("other".to_owned(), items())]
    );
    // `reset` changes `s.items` in each; only a change of the attribute at the call of
    // `append` is a change made here.
    let changed_here = |name: &str| -> Vec<bool> {
        let (_, out, err) = p.heki(&["inspect", "--output-format", "json", name]);
        let got: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {err}"));
        (got["changes"]["params"].as_array().unwrap().iter())
            .map(|c| c["direct"].as_bool().unwrap())
            .collect()
    };
    for (name, here) in [
        ("touch_via", true),
        ("touch_checked", true),
        ("touch_kept", false),
        ("touch_kept_directly", false),
    ] {
        assert_eq!(changed_here(&format!("app.attrs.{name}")), [here], "{name}");
    }
}

/// A function a cast hands back is the function itself to `references = "calls"`: handing the
/// cast's result on hands the function on. The type a cast names is only named.
#[test]
fn a_reference_handed_on_through_a_cast_is_handed_on() {
    let p = Project::new("same_object_references");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\nrequires-python = \">=3.12\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"home\"\ntargets = [\"pkg.core.decide\"]\nallowed-callers = [\"pkg.core.*\"]\nreferences = \"calls\"\n",
    )
    .write("pkg/__init__.py", "")
    .write("pkg/core.py", "def decide(x):\n    return x\n")
    .write(
        "pkg/site.py",
        "\
from typing import Any, Callable, cast

from pkg.core import decide

HANDLERS: list[Any] = []


def register(f: Any) -> None:
    HANDLERS.append(f)


def hands_on_a_cast() -> None:
    register(cast(Callable[[int], int], decide))


def hands_on_a_bound_cast() -> None:
    g = cast(Any, decide)
    register(g)


def discards_a_cast() -> None:
    cast(Any, decide)
",
    );
    let (_, out, _) = p.check(&["--output-format", "concise", "."]);
    let lines: Vec<&str> = out.lines().filter(|l| l.contains("MUT204")).collect();
    assert_eq!(lines.len(), 2, "{out}");
    assert!(
        lines[0].contains("`hands_on_a_cast` hands on `pkg.core.decide`"),
        "{out}"
    );
    assert!(
        lines[1].contains("`hands_on_a_bound_cast` hands on `pkg.core.decide`"),
        "{out}"
    );
}
