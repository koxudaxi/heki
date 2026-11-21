//! `MUT110` (`readonly-param-mutation`) end to end: a `Sequence` / `Mapping` / `ReadOnly`
//! parameter handed to code that changes it in place — through `Optional` / `| None` /
//! `Annotated` / quoted annotations, aliases, keyword arguments, methods, and helpers one and
//! two calls deep — every exclusion (changes in the function itself, value-side changes, copies,
//! callee parameters annotated mutable or read-only, variadic parameters), the default selection
//! (`MUT110` runs, `MUT108` does not), `heki rule`, and the incremental LSP server.

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
        "`one_call` passes its read-only parameter `items` to `pkg.fill.push`; whenever line 7 of `push` runs, it changes the caller's object in place"
    );
    assert_eq!(
        one["help"],
        "pass a copy (`list(items)` / `dict(items)`), or annotate the helper's parameter with a mutable type"
    );
    // The call, then the line that changes the argument.
    assert_eq!(
        chain(at(&diags, c, 21)),
        vec![(c.into(), 21), (f.into(), 7)]
    );
    // Two calls deep: the chain ends at the function that changes the argument.
    assert_eq!(
        chain(at(&diags, c, 25)),
        vec![(c.into(), 25), (f.into(), 11), (f.into(), 7)]
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
