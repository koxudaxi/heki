//! `MUT108` (`readonly-type-mutation`) end to end: in-place changes of frozen dataclass and
//! `NamedTuple` fields, `Final` / `ReadOnly` attributes, `ReadOnly` `TypedDict` keys, and
//! `Sequence` / `Mapping` parameters handed to helpers — directly, through aliases, and through
//! helpers (one and two calls deep) — every exclusion (initialization, rebinding, deliberate
//! bypasses, copies, fresh instances, `compare=False`, non-container fields, mutable callee
//! parameters, value-side changes), PEP 681 `dataclass_transform` classes, the opt-in selection,
//! suppressions, `heki rule`, and the incremental LSP server.

mod common;

use common::Project;
use common::lsp::{Lsp, codes, uri};
use serde_json::{Value, json};

fn diagnostics(p: &Project, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["--output-format", "json"];
    all.extend_from_slice(args);
    let (code, stdout, stderr) = p.check(&all);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

/// `(path, line, qualname, target)` of every diagnostic.
fn found(diags: &[Value]) -> Vec<(String, u64, String, String)> {
    diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["qualname"].as_str().unwrap().to_owned(),
                d["target"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn d(path: &str, line: u64, qualname: &str, target: &str) -> (String, u64, String, String) {
    (path.into(), line, qualname.into(), target.into())
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

fn at<'v>(diags: &'v [Value], path: &str, line: u64) -> &'v Value {
    diags
        .iter()
        .find(|d| d["path"] == path && d["line"] == line)
        .unwrap_or_else(|| panic!("no diagnostic at {path}:{line}: {diags:#?}"))
}

fn message(diag: &Value) -> &str {
    diag["message"].as_str().unwrap()
}

// ============================================================== frozen dataclasses / NamedTuple

const MODEL: &str = r#"import dataclasses
from dataclasses import KW_ONLY, InitVar, dataclass, field
from typing import ClassVar, NamedTuple


def _dedupe(items):
    items.sort()


class Imports:
    def __init__(self) -> None:
        self.names: list[str] = []

    def extend(self, names: list[str]) -> None:
        self.names.extend(names)


@dataclass(frozen=True)
class Config:
    tags: list[str] = field(default_factory=list)
    names: "dict[str, int] | None" = None
    imports: Imports = field(default_factory=Imports)
    side: dict[str, int] = field(default_factory=dict, compare=False)
    memo: dict[str, int] = dataclasses.field(default_factory=dict, compare=False)
    seen: ClassVar[list[str]] = []
    _: KW_ONLY
    extra: InitVar[list[str] | None] = None

    def __post_init__(self, extra: list[str] | None) -> None:
        self.tags.sort()
        _dedupe(self.tags)
        self._normalize()
        object.__setattr__(self, "_cache", {})

    def _normalize(self) -> None:
        self.tags.reverse()

    def add(self, tag: str) -> None:
        self.tags.append(tag)

    def remember(self, key: str) -> None:
        self._cache[key] = 1
        self.side[key] = 1
        self.memo[key] = 1
        self.seen.append(key)
        self.imports.extend([key])


@dataclasses.dataclass(frozen=True)
class Index:
    names: dict[str, str]

    @classmethod
    def build(cls) -> "Index":
        index = cls({})
        index.names["a"] = "b"
        return index


class Sub(Config):
    pass


class Point(NamedTuple):
    xs: list[int]
    label: str = ""


@dataclass
class Plain:
    tags: list[str]


@dataclass(frozen=False)
class Thawed:
    tags: list[str]
"#;

const HELPERS: &str = r#"import sys

from pkg.model import Config


def add(items: list[str], x: str) -> None:
    items.append(x)


def add_any(items, x):
    items.append(x)


def forward(items, x):
    add_any(items, x)


def reset(c):
    c.tags.clear()


def reset_alias(c):
    tags = c.tags
    tags.clear()


def reset_deep(c):
    reset(c)


def clear_tags(c):
    _clear(c.tags)


def _clear(items):
    items.clear()


def rebind(c):
    c.tags = []


def bypass(c):
    object.__setattr__(c, "tags", [])


def deep(c):
    c.tags[0].append(1)


def typed(c: Config) -> None:
    c.tags.append("typed")


if sys.version_info >= (3, 13):

    def trim(c):
        c.tags.pop()

else:

    def trim(c):
        del c.tags[0]
"#;

const USE: &str = r#"from pkg.helpers import (
    add,
    add_any,
    bypass,
    clear_tags,
    deep,
    forward,
    rebind,
    reset,
    reset_alias,
    reset_deep,
    trim,
    typed,
)
from pkg.model import Config, Index, Plain, Point, Sub, Thawed

DEFAULT = Config()


def direct(config: Config) -> None:
    config.tags.append("x")
    config.names["k"] = 1


def via_alias(config: Config) -> None:
    tags = config.tags
    tags.append("x")


def via_helpers(config: Config) -> None:
    add(config.tags, "x")
    add_any(config.tags, "y")
    forward(config.tags, "z")
    reset(config)
    reset_alias(config)
    reset_deep(config)
    clear_tags(config)
    trim(config)


def shared_default() -> None:
    DEFAULT.tags.append("x")
    add(DEFAULT.tags, "y")


def subclass(sub: Sub, point: Point) -> None:
    sub.tags.append("x")
    point.xs.append(1)


def not_reported(config: Config, plain: Plain, thawed: Thawed) -> Config:
    config.tags = []
    del config.tags
    object.__setattr__(config, "tags", [])
    setattr(config, "tags", [])
    config.__dict__["tags"] = []
    copied = list(config.tags)
    copied.append("x")
    rebind(config)
    bypass(config)
    deep(config)
    typed(config)
    plain.tags.append("x")
    add_any(plain.tags, "x")
    thawed.tags.append("x")
    fresh = Config()
    fresh.tags.append("x")
    reset(fresh)
    add(fresh.tags, "y")
    Index.build()
    return fresh
"#;

fn frozen_project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("pkg/__init__.py", "")
        .write("pkg/model.py", MODEL)
        .write("pkg/helpers.py", HELPERS)
        .write("pkg/use.py", USE);
    p
}

#[test]
fn mut108_frozen_fields_directly_through_aliases_and_helpers() {
    let p = frozen_project("mut108_frozen");
    let diags = diagnostics(&p, &["--select", "MUT108"]);
    let (h, m, u) = ("pkg/helpers.py", "pkg/model.py", "pkg/use.py");
    let t = "Config.tags";
    assert_eq!(
        found(&diags),
        vec![
            // A helper that sees the object's type reports its own change (not its callers).
            d(h, 52, "typed", t),
            // A method of the frozen class changing its own field after initialization.
            d(m, 39, "Config.add", t),
            d(u, 21, "direct", t),
            d(u, 22, "direct", "Config.names"),
            d(u, 27, "via_alias", t),
            // Annotated and unannotated helpers handed the field, one and two calls deep.
            d(u, 31, "via_helpers", t),
            d(u, 32, "via_helpers", t),
            d(u, 33, "via_helpers", t),
            // Helpers handed the object holding the field (one defined twice, conditionally).
            d(u, 34, "via_helpers", t),
            d(u, 35, "via_helpers", t),
            d(u, 36, "via_helpers", t),
            d(u, 37, "via_helpers", t),
            d(u, 38, "via_helpers", t),
            // A module-level default shared by every caller.
            d(u, 42, "shared_default", t),
            d(u, 43, "shared_default", t),
            // A subclass instance keeps the frozen field; a `NamedTuple` is frozen.
            d(u, 47, "subclass", t),
            d(u, 48, "subclass", "Point.xs"),
        ]
    );
    let own = message(at(&diags, u, 21));
    assert!(
        own.starts_with(
            "`direct` changes the field `Config.tags` of a frozen instance in place (container_mut): the class promises immutable instances, so they get shared"
        ),
        "{own}"
    );
    assert!(
        own.contains("different `==` results") && !own.contains("hash"),
        "{own}"
    );
    assert!(
        own.ends_with("copy it first or store an immutable type such as `tuple` / `frozenset`")
    );
    assert!(message(at(&diags, u, 27)).contains("in place (alias_container_mut)"));
    assert!(
        message(at(&diags, u, 34))
            .starts_with("`via_helpers` changes the field `Config.tags` of a frozen instance in place via `pkg.helpers.reset` (indirect_helper): ")
    );
    // The calls down to the change are related locations.
    assert_eq!(chain(at(&diags, u, 31)), vec![(u.into(), 31)]);
    assert_eq!(
        chain(at(&diags, u, 33)),
        vec![(u.into(), 33), (h.into(), 15)]
    );
    assert_eq!(
        chain(at(&diags, u, 36)),
        vec![(u.into(), 36), (h.into(), 28)]
    );
    assert_eq!(
        chain(at(&diags, u, 37)),
        vec![(u.into(), 37), (h.into(), 32)]
    );
    assert!(chain(at(&diags, u, 21)).is_empty());
    for d in &diags {
        assert_eq!(d["rule_slug"], "readonly-type-mutation");
        assert!(d["fix"].is_null(), "{d:#}");
    }
}

// ================================================= `Final` / `ReadOnly` attributes and keys

const DECL: &str = r#"from dataclasses import dataclass, field
from typing import Annotated, ClassVar, Final, NotRequired, Required, TypedDict

from typing_extensions import ReadOnly

REGISTRY: Final = {}
NAMES: Final[list[str]] = []


class Movie(TypedDict):
    names: ReadOnly[list[str]]
    cast: Required[ReadOnly[list[str]]]
    crew: NotRequired[Annotated[ReadOnly[dict[str, str]], "doc"]]
    tags: list[str]
    title: ReadOnly[str]


class Sequel(Movie):
    year: int


class Box:
    LIMITS: Final = []
    SIZES: ClassVar[Final[list[int]]] = []

    def __init__(self) -> None:
        self.items: Final[list[int]] = []
        self.loose: Final = []
        self.items.append(0)
        self.notes: list[str] = []

    def push(self, v: int) -> None:
        self.items.append(v)
        self.loose.append(v)
        self.LIMITS.append(v)
        self.SIZES.append(v)
        self.notes.append(str(v))


class Crate(Box):
    pass


@dataclass
class Record:
    rows: Final[list[int]] = field(default_factory=list)
    tags: list[str] = field(default_factory=list)
"#;

const RUN: &str = r#"from pkg.decl import NAMES, REGISTRY, Box, Crate, Movie, Record, Sequel


def keyed(d):
    d["names"].append("x")


def add(items):
    items.append("y")


def movies(movie: Movie, sequel: Sequel) -> None:
    movie["names"].append("a")
    names = movie["names"]
    names.append("b")
    movie["cast"].append("c")
    movie["crew"]["k"] = "v"
    keyed(movie)
    add(movie["names"])
    sequel["names"].append("d")
    movie["tags"].append("e")
    movie["names"] = []


def boxes(box: Box, crate: Crate, record: Record) -> None:
    box.items.append(1)
    crate.items.append(2)
    record.rows.append(3)
    record.tags.append("x")


def constants() -> None:
    REGISTRY["k"] = 1
    NAMES.append("x")
"#;

#[test]
fn mut108_final_readonly_attributes_and_typed_dict_keys() {
    let p = Project::new("mut108_declared");
    p.write("pkg/__init__.py", "")
        .write("pkg/decl.py", DECL)
        .write("pkg/run.py", RUN);
    let diags = diagnostics(&p, &["--select", "MUT108"]);
    let (decl, run) = ("pkg/decl.py", "pkg/run.py");
    assert_eq!(
        found(&diags),
        vec![
            // After `__init__`, not in it; a bare `Final`, class-level constants, `ClassVar`,
            // and plain attributes are not read-only containers.
            d(decl, 33, "Box.push", "Box.items"),
            // `ReadOnly` keys: directly, through an alias, inside `Required` / `NotRequired` /
            // `Annotated`, through helpers handed the key or the dict, on a subclass.
            d(run, 13, "movies", "Movie.names"),
            d(run, 15, "movies", "Movie.names"),
            d(run, 16, "movies", "Movie.cast"),
            d(run, 17, "movies", "Movie.crew"),
            d(run, 18, "movies", "Movie.names"),
            d(run, 19, "movies", "Movie.names"),
            d(run, 20, "movies", "Movie.names"),
            d(run, 26, "boxes", "Box.items"),
            d(run, 27, "boxes", "Box.items"),
            // A `Final` field of a dataclass.
            d(run, 28, "boxes", "Record.rows"),
        ]
    );
    let key = message(at(&diags, run, 13));
    assert!(
        key.starts_with("`movies` changes the `ReadOnly` key `names` of `Movie` in place (container_mut): the declaration promises that the value is only read once it is set, but every holder of the object sees the change"),
        "{key}"
    );
    assert!(
        message(at(&diags, decl, 33)).starts_with(
            "`Box.push` changes `Box.items`, declared read-only (`Final` / `ReadOnly`), in place (container_mut): "
        )
    );
    assert_eq!(chain(at(&diags, run, 18)), vec![(run.into(), 18)]);
}

// ======================================================= `Sequence` / `Mapping` parameters

const FILL: &str = r#"from collections import deque
from collections.abc import MutableSequence, Sequence
from typing import Any


def push(items, x):
    items.append(x)


def push_via(items, x):
    push(items, x)


def push_any(items: Any, x: int) -> None:
    items.append(x)


def push_list(items: list[int], x: int) -> None:
    items.append(x)


def push_deque(items: deque[int], x: int) -> None:
    items.append(x)


def push_mutable(items: MutableSequence[int], x: int) -> None:
    items.append(x)


def push_seq(items: Sequence[int], x: int) -> None:
    push(items, x)


def put(table, key):
    table[key] = 1


def put_value(table):
    table["a"].append(1)


def count(items):
    return len(items)


class Collector:
    def _fill(self, out, value):
        out.append(value)

    def collect(self, items: Sequence[int]) -> None:
        self._fill(items, 1)
"#;

const CALLERS: &str = r#"from collections.abc import Mapping, MutableSequence, Sequence
import typing
from typing import Annotated, Optional

from pkg.fill import (
    Collector,
    count,
    push,
    push_any,
    push_deque,
    push_list,
    push_mutable,
    push_seq,
    push_via,
    put,
    put_value,
)


def one_call(items: Sequence[int]) -> None:
    push(items, 1)


def two_calls(items: Sequence[int]) -> None:
    push_via(items, 1)


def any_param(items: Sequence[int]) -> None:
    push_any(items, 1)


def mapping(table: Mapping[str, int], values: Mapping[str, list[int]]) -> None:
    put(table, "k")
    put_value(values)
    values["a"].append(1)


def wrapped(
    a: Optional[Sequence[int]],
    b: Sequence[int] | None,
    c: Annotated[Sequence[int], "doc"],
    d: "typing.Sequence[int]",
) -> None:
    push(a, 1)
    push(b, 1)
    push(c, 1)
    push(d, 1)


def aliased(items: Sequence[int]) -> None:
    alias = items
    push(alias, 1)
    push(x=1, items=items)


def not_reported(items: Sequence[int], mutable: MutableSequence[int], *rest: Sequence[int]) -> None:
    items.append(1)
    alias = items
    alias.append(2)
    push_list(items, 1)
    push_deque(items, 1)
    push_mutable(items, 1)
    push_seq(items, 1)
    push(list(items), 1)
    push(mutable, 1)
    push(rest, 1)
    count(items)
    Collector().collect(items)
"#;

#[test]
fn mut108_readonly_parameters_handed_to_helpers() {
    let p = Project::new("mut108_params");
    p.write("pkg/__init__.py", "")
        .write("pkg/fill.py", FILL)
        .write("pkg/callers.py", CALLERS);
    let diags = diagnostics(&p, &["--select", "MUT108"]);
    let (c, f) = ("pkg/callers.py", "pkg/fill.py");
    assert_eq!(
        found(&diags),
        vec![
            d(c, 21, "one_call", "items"),
            d(c, 25, "two_calls", "items"),
            d(c, 29, "any_param", "items"),
            // The mapping itself, not a list inside it.
            d(c, 33, "mapping", "table"),
            // `Optional`, `| None`, `Annotated`, and quoted annotations.
            d(c, 44, "wrapped", "a"),
            d(c, 45, "wrapped", "b"),
            d(c, 46, "wrapped", "c"),
            d(c, 47, "wrapped", "d"),
            // A copy of the parameter, and a keyword argument.
            d(c, 52, "aliased", "items"),
            d(c, 53, "aliased", "items"),
            // A callee with a read-only parameter reports its own handing over (not its
            // callers), also for a method.
            d(f, 31, "push_seq", "items"),
            d(f, 51, "Collector.collect", "items"),
        ]
    );
    let one = message(at(&diags, c, 21));
    assert_eq!(
        one,
        "`one_call` passes `items` to `pkg.fill.push`, which changes it in place: `items` is annotated read-only (`Sequence` / `Mapping`), so the caller expects its argument to stay as it is; pass a copy (`list(items)` / `dict(items)`) or annotate `items` with a mutable type"
    );
    assert_eq!(chain(at(&diags, c, 21)), vec![(c.into(), 21)]);
    // Two calls deep: the chain ends at the function that changes the argument.
    assert_eq!(
        chain(at(&diags, c, 25)),
        vec![(c.into(), 25), (f.into(), 11)]
    );
    let related = &at(&diags, c, 25)["related"];
    assert_eq!(
        related[1]["message"], "`push_via` calls `pkg.fill.push`",
        "{related:#}"
    );
}

// ============================================================ PEP 681 `dataclass_transform`

const RECORDS_LIB: &str = r#"from __future__ import annotations

from dataclasses import dataclass, field
from typing import Any, dataclass_transform, overload

from typing_extensions import dataclass_transform as transform


@overload
def record(cls: type, /) -> type: ...


@overload
def record(*, eq: bool = True) -> Any: ...


@transform(frozen_default=True, field_specifiers=(field,))
def record(cls: Any = None, /, *, eq: bool = True, frozen: bool = True) -> Any:
    def wrap(target: Any) -> Any:
        return dataclass(target, eq=eq, frozen=frozen)

    return wrap if cls is None else wrap(cls)


@dataclass_transform(field_specifiers=(field,))
def mutable_record(cls: Any) -> Any:
    return dataclass(cls)


@dataclass_transform(frozen_default=True)
class ModelBase:
    def __init_subclass__(cls, *, frozen: bool = True) -> None:
        pass


@dataclass_transform(frozen_default=True)
class ModelMeta(type):
    pass


class SubMeta(ModelMeta):
    pass
"#;

const RECORDS: &str = r#"from pkg import _records
from pkg._records import ModelBase, ModelMeta, SubMeta, mutable_record, record


@record
class Plan:
    steps: list[str]


@record(eq=False)
class Batch:
    jobs: list[str]


@_records.record(frozen=False)
class Draft:
    lines: list[str]


@mutable_record
class Scratch:
    notes: list[str]


class Model(ModelBase):
    tags: list[str]


class Loose(ModelBase, frozen=False):
    tags: list[str]


class Stricter(Loose):
    pass


class Thing(metaclass=ModelMeta):
    tags: list[str]


class Other(metaclass=SubMeta):
    tags: list[str]


class Child(Thing):
    pass


def change(plan: Plan, batch: Batch, draft: Draft, scratch: Scratch, model: Model, loose: Loose, stricter: Stricter, thing: Thing, other: Other, child: Child) -> None:
    plan.steps.append("x")
    batch.jobs.append("x")
    draft.lines.append("x")
    scratch.notes.append("x")
    model.tags.append("x")
    loose.tags.append("x")
    stricter.tags.append("x")
    thing.tags.append("x")
    other.tags.append("x")
    child.tags.append("x")
"#;

const BOTH_BRANCHES: &str = r#"from typing import NamedTuple


class Options(NamedTuple):
    credentials: list[str]


def add(options: Options | None) -> Options:
    if options is None:
        options = Options([])
    else:
        options = Options(options.credentials[:])
    options.credentials.insert(0, "x")
    return options


def maybe(options: Options, flag: bool) -> Options:
    if flag:
        options = Options([])
    options.credentials.append("y")
    return options
"#;

#[test]
fn mut108_dataclass_transform_classes_and_fresh_branches() {
    let p = Project::new("mut108_transform");
    p.write("pkg/__init__.py", "")
        .write("pkg/_records.py", RECORDS_LIB)
        .write("pkg/records.py", RECORDS)
        .write("pkg/both.py", BOTH_BRANCHES);
    let diags = diagnostics(&p, &["--select", "MUT108"]);
    let r = "pkg/records.py";
    assert_eq!(
        found(&diags),
        vec![
            // A fresh instance on one branch only: the parameter may be the changed object.
            d("pkg/both.py", 20, "maybe", "Options.credentials"),
            // A decorator function carrying `dataclass_transform(frozen_default=True)` (on the
            // implementation of an overloaded function), with and without a call; not with
            // `frozen=False`, nor through a transform without `frozen_default`.
            d(r, 50, "change", "Plan.steps"),
            d(r, 51, "change", "Batch.jobs"),
            // A base class carrying it; not with the class keyword `frozen=False`, which its
            // subclasses inherit.
            d(r, 54, "change", "Model.tags"),
            // A metaclass carrying it (or a subclass of it), and a subclass of such a class.
            d(r, 57, "change", "Thing.tags"),
            d(r, 58, "change", "Other.tags"),
            d(r, 59, "change", "Thing.tags"),
        ]
    );
}

// ========================================================= defaults, suppressions, `heki rule`

#[test]
fn mut108_is_opt_in_and_honors_suppressions() {
    let p = Project::new("mut108_opt_in");
    p.write("pkg/__init__.py", "")
        .write(
            "pkg/cfg.py",
            "from dataclasses import dataclass, field\n\n\n@dataclass(frozen=True)\nclass Config:\n    tags: list[str] = field(default_factory=list)\n\n\ndef f(c: Config) -> None:\n    c.tags.append(\"a\")\n    c.tags.append(\"b\")  # heki: ignore[MUT108]\n    c.tags.append(\"c\")  # noqa: MUT108\n    c.tags.append(\"d\")  # heki: ignore[readonly-type-mutation]\n",
        );
    // Neither the default rules nor the `MUT` prefix enable it; its code or slug does.
    assert!(diagnostics(&p, &[]).is_empty());
    assert!(diagnostics(&p, &["--select", "MUT"]).is_empty());
    let expected = vec![d("pkg/cfg.py", 10, "f", "Config.tags")];
    assert_eq!(
        found(&diagnostics(&p, &["--extend-select", "MUT108"])),
        expected
    );
    assert_eq!(
        found(&diagnostics(
            &p,
            &["--extend-select", "readonly-type-mutation"]
        )),
        expected
    );
    p.write("heki.toml", "[lint]\nextend-select = [\"MUT108\"]\n");
    assert_eq!(found(&diagnostics(&p, &[])), expected);
    assert!(diagnostics(&p, &["--ignore", "MUT108"]).is_empty());

    let (code, doc, _) = p.heki(&["rule", "MUT108"]);
    assert_eq!(code, 0);
    assert!(
        doc.starts_with("# readonly-type-mutation (MUT108)\n\n"),
        "{doc}"
    );
    let sections = [
        "\n## What it does\n",
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

// ================================================================================== LSP

const LSP_MODEL: &str = "from dataclasses import dataclass, field\n\n\n@dataclass(frozen=True)\nclass Config:\n    tags: list[str] = field(default_factory=list)\n";
const LSP_MODEL_THAWED: &str = "from dataclasses import dataclass, field\n\n\n@dataclass\nclass Config:\n    tags: list[str] = field(default_factory=list)\n";
const LSP_HELPERS: &str = "def add(items, x):\n    return len(items)\n";
const LSP_HELPERS_MUTATING: &str = "def add(items, x):\n    items.append(x)\n";
const LSP_APP: &str = "from pkg.helpers import add\nfrom pkg.model import Config\n\n\ndef build(config: Config) -> None:\n    add(config.tags, \"x\")\n";

/// Edit, write, and save `rel` (the way an editor saves a changed document).
fn save(lsp: &mut Lsp, p: &Project, rel: &str, version: i32, text: &str) {
    let u = uri(&p.dir.join(rel));
    lsp.change(&u, version, text);
    p.write(rel, text);
    lsp.notify(
        "textDocument/didSave",
        json!({"textDocument": {"uri": u}, "text": text}),
    );
}

#[test]
fn mut108_lsp_follows_helper_and_frozen_edits_in_other_files() {
    let p = Project::new("mut108_lsp");
    p.write("heki.toml", "[lint]\nextend-select = [\"MUT108\"]\n")
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", LSP_MODEL)
        .write("pkg/helpers.py", LSP_HELPERS)
        .write("pkg/app.py", LSP_APP);
    let app = uri(&p.dir.join("pkg/app.py"));
    let mut lsp = Lsp::start(&p.dir, &["server", "--stdio"]);
    lsp.request(
        "initialize",
        json!({"processId": null, "rootUri": uri(&p.dir), "capabilities": {}}),
    );
    lsp.notify("initialized", json!({}));
    lsp.ready();
    lsp.open(&app, LSP_APP);
    lsp.diagnostics(&app, <[Value]>::is_empty);

    // The helper starts changing its parameter in place: the caller in another file hands it
    // a frozen field.
    lsp.open(&uri(&p.dir.join("pkg/helpers.py")), LSP_HELPERS);
    save(&mut lsp, &p, "pkg/helpers.py", 2, LSP_HELPERS_MUTATING);
    let diags = lsp.diagnostics(&app, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["MUT108"]);
    assert_eq!(
        diags[0]["range"]["start"],
        json!({"line": 5, "character": 4})
    );
    assert!(
        diags[0]["message"]
            .as_str()
            .unwrap()
            .contains("via `pkg.helpers.add` (indirect_helper)"),
        "{diags:#?}"
    );
    let related = diags[0]["relatedInformation"].as_array().unwrap();
    assert_eq!(related[0]["location"]["uri"], app);

    // The class stops being frozen: its declarations change, and the importer is re-checked.
    lsp.open(&uri(&p.dir.join("pkg/model.py")), LSP_MODEL);
    save(&mut lsp, &p, "pkg/model.py", 2, LSP_MODEL_THAWED);
    lsp.diagnostics(&app, <[Value]>::is_empty);
    lsp.stop(true);
}
