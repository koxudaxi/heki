//! `MUT110` (`readonly-param-mutation`) end to end: a `Sequence` / `Mapping` / `AbstractSet` /
//! `ReadOnly` parameter handed to code that changes it in place — through `Optional` / `| None` /
//! `Annotated` / quoted annotations, aliases, keyword arguments, methods, and helpers one and
//! two calls deep — every exclusion (changes in the function itself, value-side changes, copies,
//! callee parameters annotated mutable or read-only, variadic parameters), the default selection
//! (`MUT110` runs, `MUT108` does not), `heki rule`, the incremental LSP server, and the premises
//! of a set ABC parameter's report.

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
fn mut110_readonly_parameters_handed_to_helpers() {
    let p = Project::new("mut110_params");
    p.write("pkg/__init__.py", "")
        .write("pkg/fill.py", FILL)
        .write("pkg/callers.py", CALLERS);
    // The default rules: `MUT110` runs without being selected.
    let diags = diagnostics(&p, &[]);
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
    let one = at(&diags, c, 21);
    assert_eq!(
        message(one),
        "`one_call` passes its read-only parameter `items` to `pkg.fill.push`, which changes it in place"
    );
    assert_eq!(
        one["help"],
        "pass a copy (`list(items)` / `dict(items)` / `set(items)`), or annotate the helper's parameter with a mutable type"
    );
    // `push` does not type `items`: whether `append` changes it in place is open, so the report
    // names no line and is heuristic.
    assert_eq!(one["certainty"], "heuristic");
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

// ====================================================================== the named line

/// The claim is the named line: whenever it runs during the call, it changes the caller's object.
/// So a change under a test of the argument's type is reported, and a parameter that is a copy on
/// one path before the call, or a helper's parameter rebound to a copy on one path before the
/// change, is not. A change made where a generator another function returns is iterated has no
/// line in the chain: that report is heuristic and names none.
const ANCHORED: &str = r#"from collections.abc import Sequence


def sort_values(values):
    values.sort()


def copy_then_sort(values, fresh):
    if fresh:
        values = list(values)
    values.sort()


def append_to_lists(values):
    if isinstance(values, list):
        values.append(1)


def copied_on_one_path(values: Sequence[float], fresh: bool) -> None:
    if fresh:
        values = list(values)
    sort_values(values)


def rebound_in_the_helper(values: Sequence[float]) -> None:
    copy_then_sort(values, True)


def under_a_type_test(values: Sequence[float]) -> None:
    append_to_lists(values)


def wipe(values):
    values.clear()
    yield 1


def adapter(values):
    return wipe(values)


def consume(values):
    list(adapter(values))


def through_a_generator(values: Sequence[float]) -> None:
    consume(values)
"#;

#[test]
fn mut110_names_a_line_that_changes_the_callers_object_whenever_it_runs() {
    let p = Project::new("mut110_anchored");
    p.write("pkg/__init__.py", "")
        .write("pkg/anchored.py", ANCHORED);
    let diags = diagnostics(&p, &[]);
    let a = "pkg/anchored.py";
    assert_eq!(
        found(&diags),
        vec![
            d(a, 30, "under_a_type_test", "values"),
            d(a, 47, "through_a_generator", "values"),
        ]
    );
    assert_eq!(
        message(&diags[0]),
        "`under_a_type_test` passes its read-only parameter `values` to `pkg.anchored.append_to_lists`; whenever line 16 of `append_to_lists` runs, it changes the caller's object in place"
    );
    assert_eq!(chain(&diags[0]), vec![(a.into(), 30), (a.into(), 16)]);
    assert_eq!(diags[0]["severity"], "warning");
    assert_eq!(
        message(&diags[1]),
        "`through_a_generator` passes its read-only parameter `values` to `pkg.anchored.consume`, which changes it in place"
    );
    assert_eq!(diags[1]["certainty"], "heuristic");
    assert_eq!(chain(&diags[1]), vec![(a.into(), 47)]);
    // A per-file ignore leaves the file without reports.
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nper-file-ignores = { \"pkg/anchored.py\" = [\"MUT110\"] }\n",
    );
    assert!(diagnostics(&p, &[]).is_empty());
}

// ============================================================== defaults and `heki rule`

const BOTH: &str = r#"from collections.abc import Sequence
from dataclasses import dataclass, field


def sort_values(values):
    values.sort()


def median(values: Sequence[float]) -> float:
    sort_values(values)  # reorders the caller's list
    return values[len(values) // 2]


@dataclass(frozen=True)
class Config:
    tags: list[str] = field(default_factory=list)


def configure(config: Config) -> None:
    config.tags.append("x")
"#;

#[test]
fn mut110_runs_by_default_and_mut108_does_not() {
    let p = Project::new("mut110_default");
    p.write("pkg/__init__.py", "").write("pkg/both.py", BOTH);
    let diags = diagnostics(&p, &[]);
    assert_eq!(
        found(&diags),
        vec![d("pkg/both.py", 10, "median", "values")]
    );
    assert_eq!(diags[0]["rule_code"], "MUT110");
    assert_eq!(diags[0]["rule_slug"], "readonly-param-mutation");
    assert!(diags[0]["fix"].is_null());
    // The `MUT` prefix selects it; the two rules of one analysis report separately.
    assert_eq!(found(&diagnostics(&p, &["--select", "MUT"])).len(), 1);
    let both = diagnostics(&p, &["--extend-select", "MUT108"]);
    let codes: Vec<&str> = both
        .iter()
        .map(|d| d["rule_code"].as_str().unwrap())
        .collect();
    assert_eq!(codes, vec!["MUT110", "MUT108"]);
    assert_eq!(
        found(&diagnostics(&p, &["--select", "MUT108"])),
        vec![d("pkg/both.py", 20, "configure", "Config.tags")]
    );
    assert!(diagnostics(&p, &["--ignore", "readonly-param-mutation"]).is_empty());

    let (code, doc, _) = p.heki(&["rule", "MUT110"]);
    assert_eq!(code, 0);
    assert!(
        doc.starts_with("# readonly-param-mutation (MUT110)\n\n"),
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

const LSP_HELPERS: &str = "def sort_values(values):\n    return sorted(values)\n";
const LSP_HELPERS_MUTATING: &str = "def sort_values(values):\n    values.sort()\n";
const LSP_HELPERS_ANNOTATED: &str =
    "def sort_values(values: list[float]) -> None:\n    values.sort()\n";
const LSP_STATS: &str = "from collections.abc import Sequence\n\nfrom pkg.helpers import sort_values\n\n\ndef median(values: Sequence[float]) -> float:\n    sort_values(values)\n    return values[len(values) // 2]\n";

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
fn mut110_lsp_follows_helper_edits_in_other_files() {
    let p = Project::new("mut110_lsp");
    p.write("pkg/__init__.py", "")
        .write("pkg/helpers.py", LSP_HELPERS)
        .write("pkg/stats.py", LSP_STATS);
    let stats = uri(&p.dir.join("pkg/stats.py"));
    let mut lsp = Lsp::start(&p.dir, &["server", "--stdio"]);
    lsp.request(
        "initialize",
        json!({"processId": null, "rootUri": uri(&p.dir), "capabilities": {}}),
    );
    lsp.notify("initialized", json!({}));
    lsp.ready();
    lsp.open(&stats, LSP_STATS);
    lsp.diagnostics(&stats, <[Value]>::is_empty);

    // The helper starts changing its parameter in place: the caller in another file hands it
    // a read-only parameter (the default rules, nothing selected).
    lsp.open(&uri(&p.dir.join("pkg/helpers.py")), LSP_HELPERS);
    save(&mut lsp, &p, "pkg/helpers.py", 2, LSP_HELPERS_MUTATING);
    let diags = lsp.diagnostics(&stats, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["MUT110"]);
    assert_eq!(
        diags[0]["range"]["start"],
        json!({"line": 6, "character": 4})
    );
    let related = diags[0]["relatedInformation"].as_array().unwrap();
    assert_eq!(related[0]["location"]["uri"], stats);

    // The helper's parameter is annotated mutable: the type checker reports the argument now,
    // so heki does not.
    save(&mut lsp, &p, "pkg/helpers.py", 3, LSP_HELPERS_ANNOTATED);
    lsp.diagnostics(&stats, <[Value]>::is_empty);
    lsp.stop(true);
}

/// A call that can run two implementations: one names no line, the other changes a list at a
/// line. The call is reported once, with the line.
const DISPATCHED: &str = r#"from collections.abc import Sequence


class Adder:
    def add(self, values):
        values.append(1)


class ListAdder(Adder):
    def add(self, values):
        if isinstance(values, list):
            values.append(2)


def fill(adder: Adder, values: Sequence[int]) -> None:
    adder.add(values)
"#;

#[test]
fn mut110_prefers_the_implementation_that_names_a_line() {
    let p = Project::new("mut110_dispatched");
    p.write("pkg/__init__.py", "")
        .write("pkg/dispatched.py", DISPATCHED);
    let diags = diagnostics(&p, &[]);
    let a = "pkg/dispatched.py";
    assert_eq!(found(&diags), vec![d(a, 16, "fill", "values")]);
    assert_eq!(diags[0]["certainty"], "exact", "{diags:#?}");
    assert_eq!(chain(&diags[0]).last(), Some(&(a.into(), 12)));
}

/// A helper defined once per platform: the definitions' changes join, and the report names the
/// first line.
const PER_PLATFORM: &str = r#"import sys
from collections.abc import Sequence

if sys.platform == "win32":

    def fill(values):
        if isinstance(values, list):
            values.append(1)

else:

    def fill(values):
        if isinstance(values, list):
            values.append(2)


def use(values: Sequence[int]) -> None:
    fill(values)
"#;

#[test]
fn mut110_joins_the_changes_of_each_definition() {
    let p = Project::new("mut110_per_platform");
    p.write("pkg/__init__.py", "")
        .write("pkg/platform.py", PER_PLATFORM);
    let diags = diagnostics(&p, &[]);
    let a = "pkg/platform.py";
    assert_eq!(found(&diags), vec![d(a, 18, "use", "values")]);
    assert_eq!(diags[0]["certainty"], "exact", "{diags:#?}");
    assert_eq!(chain(&diags[0]).last(), Some(&(a.into(), 8)));
}

/// Under R2 a value that passes `isinstance(xs, list)` may be an instance of a project subclass
/// of `list`; when one overrides the changing method (`reverse`), the line may change nothing,
/// so the report names no line and is heuristic.
const OVERRIDDEN: &str = r#"from collections.abc import Sequence


class Frozen(list):
    def reverse(self):
        pass


def reverse_lists(xs):
    if isinstance(xs, list):
        xs.reverse()


def use(xs: Sequence[int]) -> None:
    reverse_lists(xs)
"#;

/// A helper annotated `Mapping` is checked on its own, and the one it hands the argument to is
/// annotated `dict[str, int]`, which a type checker reports: a change reached only through them
/// is no report of the outer call.
const THROUGH_ANNOTATED: &str = r#"from collections.abc import Mapping


def put(xs: dict[str, int], n):
    xs.setdefault("z", n)


def relay(xs, n):
    checked(xs, n)


def checked(xs: Mapping[str, int], n):
    put(xs, n)


def entry(values: Mapping[str, int], n):
    alias = values
    relay(alias, n)
"#;

#[test]
fn mut110_leaves_open_a_project_subclass_overriding_the_change() {
    let p = Project::new("mut110_overridden");
    p.write("pkg/__init__.py", "")
        .write("pkg/overridden.py", OVERRIDDEN);
    let diags = diagnostics(&p, &[]);
    assert_eq!(found(&diags), vec![d("pkg/overridden.py", 15, "use", "xs")]);
    assert_eq!(diags[0]["certainty"], "heuristic", "{diags:#?}");
    assert_eq!(chain(&diags[0]), vec![("pkg/overridden.py".into(), 15)]);
}

#[test]
fn mut110_does_not_follow_a_change_through_annotated_helpers() {
    let p = Project::new("mut110_through_annotated");
    p.write("pkg/__init__.py", "")
        .write("pkg/annotated.py", THROUGH_ANNOTATED);
    assert!(diagnostics(&p, &[]).is_empty());
}

/// A positional argument binds a positional parameter only: past them it lands in `*args` (a new
/// tuple holding it), or the call raises `TypeError`; it never binds a keyword-only parameter.
const PAST_POSITIONALS: &str = r#"from collections.abc import Sequence


def log(msg, *args, extra=None):
    extra.append(msg)


def tag(first, *rest, label=None):
    first.append(1)
    label.append(1)


def strict(first, *, label=None):
    label.append(1)


def callers(a: Sequence[int], b: Sequence[int], c: Sequence[int], d: Sequence[int]) -> None:
    log("x", a)
    tag(b, c)
    strict(1, d)
"#;

#[test]
fn mut110_follows_a_position_only_into_the_parameter_it_binds() {
    let p = Project::new("mut110_past_positionals");
    p.write("pkg/__init__.py", "")
        .write("pkg/past.py", PAST_POSITIONALS);
    // `b` binds `first`; `a` and `c` land in `*args`, and `d` makes the call raise.
    assert_eq!(
        found(&diagnostics(&p, &[])),
        vec![d("pkg/past.py", 19, "callers", "b")]
    );
}

/// A metaclass may install `reverse` (or `__iadd__`) on the subclass it creates, and
/// `__getattribute__` decides what `xs.reverse` is: both leave the line open. An augmented
/// assignment calls `__iadd__` without `__getattribute__`, so beside `__getattribute__` its line
/// stays named.
const HELPERS: &str = r#"from collections.abc import Sequence


def reverse_lists(xs):
    if isinstance(xs, list):
        xs.reverse()
        xs.reverse()


def extend_lists(xs):
    if isinstance(xs, list):
        xs += [1]


def by_name(xs: Sequence[int]) -> None:
    reverse_lists(xs)


def by_operator(xs: Sequence[int]) -> None:
    extend_lists(xs)
"#;

#[test]
fn mut110_leaves_open_a_metaclass_or_getattribute_but_not_an_operator() {
    let subclasses = [
        (
            "metaclass",
            "class Meta(type):\n    pass\n\n\nclass Made(list, metaclass=Meta):\n    pass\n",
            ["heuristic", "heuristic"],
        ),
        (
            "getattribute",
            "class Lazy(list):\n    def __getattribute__(self, name):\n        return object.__getattribute__(self, name)\n",
            ["heuristic", "exact"],
        ),
    ];
    for (name, subclass, certainties) in subclasses {
        let p = Project::new(&format!("mut110_customized_{name}"));
        p.write("pkg/__init__.py", "")
            .write("pkg/helpers.py", HELPERS)
            .write("pkg/subclass.py", subclass);
        let diags = diagnostics(&p, &[]);
        let a = "pkg/helpers.py";
        assert_eq!(
            found(&diags),
            vec![d(a, 16, "by_name", "xs"), d(a, 20, "by_operator", "xs")],
            "{name}"
        );
        let got: Vec<&str> = diags
            .iter()
            .map(|d| d["certainty"].as_str().unwrap())
            .collect();
        assert_eq!(got, certainties, "{name}: {diags:#?}");
    }
}

// ======================================================================== the set ABCs

/// `typing.AbstractSet` and `collections.abc.Set` promise what `Mapping` does, in every spelling
/// `Mapping` takes: bare or subscripted, through an import alias or the module, quoted, inside
/// `Optional`, `| None`, `Union[..., None]`, and `Annotated`. `typing.Set` is the builtin `set`,
/// and `typing_extensions` names are read as `Mapping`'s are, as no read-only type.
const SET_SPELLINGS: &str = r#"import collections.abc
import typing
import typing_extensions
from collections.abc import Mapping, MutableSet, Set
from collections.abc import Set as ReadOnlySet
from typing import AbstractSet, Annotated, Optional, Union


def put(table):
    if isinstance(table, dict):
        table["k"] = 1


def grow(items):
    if isinstance(items, set):
        items.add(1)


def add_one(items: set[int]) -> None:
    items.add(1)


def relay(items: AbstractSet[int]) -> None:
    grow(items)


def spelled(
    a: AbstractSet[int],
    b: Set[int],
    c: ReadOnlySet[int],
    d: collections.abc.Set[int],
    e: typing.AbstractSet[int],
    f: Set,
    g: AbstractSet,
) -> None:
    grow(a)
    grow(b)
    grow(c)
    grow(d)
    grow(e)
    grow(f)
    grow(g)


def wrapped(
    a: "Set[int]",
    b: "typing.AbstractSet[int]",
    c: Optional[Set[int]],
    d: Set[int] | None,
    e: "Union[AbstractSet[int], None]",
    f: Annotated[Set[int], "doc"],
) -> None:
    grow(a)
    grow(b)
    grow(c)
    grow(d)
    grow(e)
    grow(f)


def not_read_only(
    a: set[int],
    b: typing.Set[int],
    c: MutableSet[int],
    d: frozenset[int],
    e: Set[int] | list[int],
    f: typing_extensions.AbstractSet[int],
    g: typing_extensions.Mapping[str, int],
) -> None:
    grow(a)
    grow(b)
    grow(c)
    grow(d)
    grow(e)
    grow(f)
    put(g)


def through_annotated_helpers(items: Set[int]) -> None:
    add_one(items)
    relay(items)


def mapping(table: Mapping[str, int]) -> None:
    put(table)
"#;

#[test]
fn mut110_reads_the_set_abcs_in_every_spelling_the_mapping_takes() {
    let p = Project::new("mut110_set_spellings");
    p.write("pkg/__init__.py", "")
        .write("pkg/spellings.py", SET_SPELLINGS);
    let diags = diagnostics(&p, &[]);
    let s = "pkg/spellings.py";
    assert_eq!(
        found(&diags),
        vec![
            // A helper whose parameter is read-only reports its own handing over; one annotated
            // `set[int]` is left to the type checker.
            d(s, 24, "relay", "items"),
            d(s, 36, "spelled", "a"),
            d(s, 37, "spelled", "b"),
            d(s, 38, "spelled", "c"),
            d(s, 39, "spelled", "d"),
            d(s, 40, "spelled", "e"),
            d(s, 41, "spelled", "f"),
            d(s, 42, "spelled", "g"),
            d(s, 53, "wrapped", "a"),
            d(s, 54, "wrapped", "b"),
            d(s, 55, "wrapped", "c"),
            d(s, 56, "wrapped", "d"),
            d(s, 57, "wrapped", "e"),
            d(s, 58, "wrapped", "f"),
            d(s, 85, "mapping", "table"),
        ]
    );
    for diag in &diags {
        assert_eq!(diag["certainty"], "exact", "{diag:#}");
    }
    assert_eq!(
        message(at(&diags, s, 36)),
        "`spelled` passes its read-only parameter `a` to `pkg.spellings.grow`; whenever line 16 of `grow` runs, it changes the caller's object in place"
    );
    assert_eq!(
        chain(at(&diags, s, 36)),
        vec![(s.into(), 36), (s.into(), 16)]
    );
    assert_eq!(
        at(&diags, s, 36)["help"],
        "pass a copy (`list(a)` / `dict(a)` / `set(a)`), or annotate the helper's parameter with a mutable type"
    );
}

/// Each premise of the claim for a set ABC parameter: reported, naming the line, when all three
/// hold; with one stripped, no exact report. "Read-only parameter": `set[int]`, `frozenset[int]`.
/// "Passed as itself": a copy, also on one path. "Changed at the line": a change no type test
/// makes a builtin set's, a rebinding of a frozenset, a helper that only reads, and (in a project
/// of its own) a project subclass of `set` overriding `add`.
const SET_PREMISES: &str = r#"from collections.abc import Set
from typing import AbstractSet


def grow(items):
    if isinstance(items, set):
        items.add(1)


def grow_unguarded(items):
    items.add(1)


def merge(items):
    if isinstance(items, set):
        items |= {1}


def rebind_frozen(items):
    if isinstance(items, frozenset):
        items |= {1}


def only_reads(items):
    return len(items)


def held(items: AbstractSet[int]) -> None:
    grow(items)


def held_alias(items: Set[int]) -> None:
    alias = items
    grow(alias)


def held_operator(items: Set[int]) -> None:
    merge(items)


def stripped_annotation(items: set[int], others: frozenset[int]) -> None:
    grow(items)
    grow(others)


def stripped_copy(items: Set[int], fresh: bool) -> None:
    grow(set(items))
    if fresh:
        items = set(items)
    grow(items)


def stripped_one_path(items: AbstractSet[int], flag: bool) -> None:
    grow(items if flag else set())


def stripped_unguarded(items: Set[int]) -> None:
    grow_unguarded(items)


def stripped_rebinding(items: Set[int]) -> None:
    rebind_frozen(items)


def stripped_reading(items: Set[int]) -> None:
    only_reads(items)
"#;

#[test]
fn mut110_set_abc_reports_hold_every_premise() {
    let p = Project::new("mut110_set_premises");
    p.write("pkg/__init__.py", "")
        .write("pkg/premises.py", SET_PREMISES);
    let s = "pkg/premises.py";
    let diags = diagnostics(&p, &[]);
    assert_eq!(
        found(&diags),
        vec![
            d(s, 29, "held", "items"),
            d(s, 34, "held_alias", "items"),
            d(s, 38, "held_operator", "items"),
            d(s, 58, "stripped_unguarded", "items"),
        ]
    );
    let certainties: Vec<&str> = diags
        .iter()
        .map(|d| d["certainty"].as_str().unwrap())
        .collect();
    assert_eq!(certainties, vec!["exact", "exact", "exact", "heuristic"]);
    assert_eq!(chain(&diags[0]), vec![(s.into(), 29), (s.into(), 7)]);
    assert_eq!(chain(&diags[2]), vec![(s.into(), 38), (s.into(), 16)]);
    assert_eq!(chain(&diags[3]), vec![(s.into(), 58)]);
    // The default run shows only the reports whose premises all hold.
    let exact = diagnostics(&p, &["--no-heuristic"]);
    assert_eq!(
        found(&exact),
        vec![
            d(s, 29, "held", "items"),
            d(s, 34, "held_alias", "items"),
            d(s, 38, "held_operator", "items"),
        ]
    );

    // A project subclass of `set` overriding `add` may be the argument under R2: the line may
    // change nothing, and the report names none.
    let quiet = "class Quiet(set):\n    def add(self, item):\n        pass\n\n\n";
    let p = Project::new("mut110_set_overridden");
    p.write("pkg/__init__.py", "")
        .write("pkg/premises.py", &format!("{quiet}{SET_PREMISES}"));
    let diags = diagnostics(&p, &[]);
    let held = at(&diags, s, 34);
    assert_eq!(held["certainty"], "heuristic", "{held:#}");
    assert_eq!(chain(held), vec![(s.into(), 34)]);
    // `|=` calls `__ior__`, which `Quiet` keeps.
    assert_eq!(at(&diags, s, 43)["certainty"], "exact");
}

/// Functions returning what a call returns: `forwarded` returns what `identity` (another
/// module) returns of its argument, and `three_deep` what `forwarded` returns, by name, through
/// a module, by keyword, and with positions before an unpacking.
const SAME: &str = r#"def identity(x):
    return x


class Holder:
    @staticmethod
    def keep(x):
        return x
"#;

const FORWARD: &str = r#"import copy
import sys

from pkg import same
from pkg.same import Holder, identity
from pkg.same import identity as rebound


def forwarded(x):
    return identity(x)


def three_deep(x):
    y = forwarded(x)
    return y


def through_module(x):
    return same.identity(x)


def by_keyword(x):
    return identity(x=x)


def pick_first(p, *rest):
    return p


def before_unpacking(x, *rest):
    return pick_first(x, *rest)


def after_unpacking(x, *rest):
    return pick_first(*rest, x)


def swapped(first, second):
    return pick_first(second, first)


def swapped_constant(x):
    return swapped(x, [1])


def copy_values(x):
    return list(x)


def copied(x):
    return copy.copy(x)


def forwards_a_copy(x):
    return copied(x)


def copy_or_forwarded(x, flag):
    if flag:
        return forwarded(x)
    return copy.copy(x)


def other_on_one_path(x, y, flag):
    if flag:
        return identity(x)
    return y


def a(x):
    return b(x)


def b(x):
    return a(x)


def through_a_method(x):
    return Holder.keep(x)


def pick(x):
    return x


if sys.version_info >= (3, 12):
    pick = copy_values


def through_an_alias(x):
    return pick(x)


def through_a_rebound_import(x):
    return rebound(x)


rebound = list
"#;

/// The calls handing a read-only parameter to `change` through the functions of [`FORWARD`]:
/// those returning it on every path report, the others (a copy, a copy one call further, a copy
/// on one path, another parameter on one path, a cycle that never returns, a constant where the callee's returned
/// parameter is bound, a method, a name bound on two paths, an import the module binds again, a
/// position past an unpacking) do not.
const FORWARD_USE: &str = r#"from collections.abc import Sequence

from pkg import forward
from pkg.forward import forwarded


def change(x):
    if isinstance(x, list):
        x.reverse()


def two(xs: Sequence[int]):
    change(forwarded(xs))


def three(xs: Sequence[int]):
    change(forward.three_deep(xs))


def module_attribute(xs: Sequence[int]):
    change(forward.through_module(xs))


def keyword(xs: Sequence[int]):
    change(forward.by_keyword(xs))


def before_unpacking(xs: Sequence[int]):
    change(forward.before_unpacking(xs))


def after_unpacking(xs: Sequence[int]):
    change(forward.after_unpacking(xs))


def swapped(xs: Sequence[int], ys: list[int]):
    change(forward.swapped(ys, xs))


def swapped_constant(xs: Sequence[int]):
    change(forward.swapped_constant(xs))


def copy(xs: Sequence[int]):
    change(forward.copy_values(xs))


def copied(xs: Sequence[int]):
    change(forward.forwards_a_copy(xs))


def copy_or_forwarded(xs: Sequence[int]):
    change(forward.copy_or_forwarded(xs, True))


def other_on_one_path(xs: Sequence[int], ys: list[int]):
    change(forward.other_on_one_path(xs, ys, True))


def cycle(xs: Sequence[int]):
    change(forward.a(xs))


def method(xs: Sequence[int]):
    change(forward.through_a_method(xs))


def alias(xs: Sequence[int]):
    change(forward.through_an_alias(xs))


def rebound(xs: Sequence[int]):
    change(forward.through_a_rebound_import(xs))
"#;

#[test]
fn mut110_follows_parameters_returned_through_calls() {
    let p = Project::new("mut110_forwarded");
    p.write("pkg/__init__.py", "")
        .write("pkg/same.py", SAME)
        .write("pkg/forward.py", FORWARD)
        .write("pkg/use.py", FORWARD_USE);
    let diags = diagnostics(&p, &[]);
    let u = "pkg/use.py";
    assert_eq!(
        found(&diags),
        vec![
            d(u, 13, "two", "xs"),
            d(u, 17, "three", "xs"),
            d(u, 21, "module_attribute", "xs"),
            d(u, 25, "keyword", "xs"),
            d(u, 29, "before_unpacking", "xs"),
            d(u, 37, "swapped", "xs"),
        ]
    );
    assert!(
        diags.iter().all(|d| d["certainty"] == "exact"),
        "{diags:#?}"
    );
    assert_eq!(chain(&diags[0]), vec![(u.into(), 13), (u.into(), 9)]);
}

/// A call of a name the project may bind again may run another object: `forwarded_too` (which
/// another module replaces), `through_rebound` (whose callee another module replaces), `via_own`
/// (whose callee its module binds again), and `replaced` itself (called directly) return no
/// parameter; `forwarded`, through names nothing binds again, still does.
#[test]
fn mut110_follows_no_call_of_a_name_bound_again() {
    let p = Project::new("mut110_forwarded_rebound");
    p.write("pkg/__init__.py", "")
        .write(
            "pkg/same.py",
            r#"def identity(x):
    return x


def replaced(x):
    return x


def forwarded(x):
    return identity(x)


def forwarded_too(x):
    return identity(x)


def through_rebound(x):
    return replaced(x)


def own(x):
    return x


def via_own(x):
    return own(x)


def swap():
    global own
    own = list
"#,
        )
        .write(
            "pkg/other.py",
            "from pkg import same\n\n\ndef replace():\n    same.forwarded_too = list\n    same.replaced = list\n",
        )
        .write(
            "pkg/use.py",
            r#"from collections.abc import Sequence

from pkg.same import forwarded, forwarded_too, replaced, through_rebound, via_own


def change(x):
    if isinstance(x, list):
        x.reverse()


def kept(xs: Sequence[int]):
    change(forwarded(xs))


def replaced_forwarder(xs: Sequence[int]):
    change(forwarded_too(xs))


def replaced_callee(xs: Sequence[int]):
    change(through_rebound(xs))


def bound_again(xs: Sequence[int]):
    change(via_own(xs))


def replaced_directly(xs: Sequence[int]):
    change(replaced(xs))
"#,
        );
    let diags = diagnostics(&p, &[]);
    assert_eq!(found(&diags), vec![d("pkg/use.py", 12, "kept", "xs")]);
    assert_eq!(diags[0]["certainty"], "exact");
}
