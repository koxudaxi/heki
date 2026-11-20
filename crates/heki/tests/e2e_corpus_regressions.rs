//! Regressions from the hand review of heki's reports and patches on an open-source corpus
//! (reproductions R01–R09 and R14–R18): each false positive stays fixed (the true negative),
//! next to the true positive it must not hide; and the fixes keep type checkers and isort
//! satisfied, and print the paths every other output format prints.

mod common;

use common::{Project, run_python};
use serde_json::Value;

/// `heki check --output-format json <args>` in `p`.
fn diagnostics(p: &Project, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["--output-format", "json"];
    all.extend_from_slice(args);
    let (code, stdout, stderr) = p.check(&all);
    assert!(code == 0 || code == 1, "exit {code}: {stderr}");
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"))
}

/// `(qualname, rule)` of every diagnostic, in report order.
fn reported(diags: &[Value]) -> Vec<(String, String)> {
    diags
        .iter()
        .map(|d| {
            (
                d["qualname"].as_str().unwrap().to_owned(),
                d["rule_code"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(q, c)| ((*q).to_owned(), (*c).to_owned()))
        .collect()
}

/// The diagnostics of `rule` in one module `case.py` are exactly `expected`.
fn assert_reports(name: &str, source: &str, rule: &str, expected: &[(&str, &str)]) -> Vec<Value> {
    let p = Project::new(name);
    p.write("case.py", source);
    let diags = diagnostics(&p, &["--select", rule]);
    assert_eq!(reported(&diags), pairs(expected), "{diags:#?}");
    diags
}

fn replacements(d: &Value) -> Vec<String> {
    d["fix"]
        .as_array()
        .map(|edits| {
            edits
                .iter()
                .map(|e| e["replacement"].as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

// =================================================================================== OPT103

const R01_GUARD_CAPPED: &str = r#"LIMIT = 64


def read_exactly_ten(stream) -> bytes:
    data = b""
    while len(data) < 10:
        data += stream.read(10)
    return data


def read_header(stream, done) -> bytes:
    head = b""
    while not done() and LIMIT >= len(head):
        head = head + stream.read(LIMIT)
    return head


def read_body(sock, content_length: int) -> bytes:
    buf = b""
    while len(buf) < content_length:
        buf += sock.recv(4096)
    return buf


def read_large(sock) -> bytes:
    buf = b""
    while len(buf) < 1_000_000:
        buf += sock.recv(4096)
    return buf


class Reader:
    def __init__(self, boundary: bytes) -> None:
        self._boundary_len = len(boundary) + 2

    def read_window(self, stream, size: int) -> bytes:
        chunk = b""
        while len(chunk) < self._boundary_len:
            chunk += stream.read(size)
        return chunk
"#;

#[test]
fn r01_a_loop_test_capping_the_accumulator_at_a_small_constant_is_not_quadratic() {
    // Not reported: `len(acc) < 10`, and `LIMIT >= len(acc)` with a module constant as one
    // `and` operand. Reported: a data-sized cap (a parameter), a constant above the small
    // limit, and a cap the code does not fix (an attribute: aiohttp's protocol-bounded
    // boundary length cannot be proven small).
    assert_reports(
        "reg_r01",
        R01_GUARD_CAPPED,
        "OPT103",
        &[
            ("read_body", "OPT103"),
            ("read_large", "OPT103"),
            ("Reader.read_window", "OPT103"),
        ],
    );
}

const R09_ATTRIBUTE_OF_THE_ITERATION: &str = r#"def rename(indexes) -> None:
    for idx in indexes:
        idx.name += "_history"


def extend_each(make, items) -> None:
    for item in items:
        resolver = make(item)
        resolver.parts += (item,)


def nested_receiver(rows) -> None:
    for row in rows:
        row.cell.text += "!"


class Buffer:
    def __init__(self) -> None:
        self.text = ""

    def feed(self, pieces) -> None:
        for piece in pieces:
            self.text += "|"

    def feed_alias(self, pieces) -> None:
        out = self
        for piece in pieces:
            out.text += "|"
"#;

#[test]
fn r09_an_attribute_of_the_loop_element_or_of_a_fresh_object_does_not_accumulate() {
    // Each iteration appends once to another object's attribute: not reported. An attribute
    // of an object that outlives the iteration (`self`, an alias bound before the loop)
    // accumulates: reported.
    assert_reports(
        "reg_r09",
        R09_ATTRIBUTE_OF_THE_ITERATION,
        "OPT103",
        &[("Buffer.feed", "OPT103"), ("Buffer.feed_alias", "OPT103")],
    );
}

// =================================================================================== OPT110

const R06_BYTES_DISPLAY: &str = r#"def bytes_display(r, values) -> None:
    s = [b"1", b"2", b"3"]
    r.sadd("a", *s)
    for value in values:
        assert value in s


def bytes_display_no_call(values) -> None:
    s = [b"1", b"2", b"3"]
    for value in values:
        assert value in s


def display_of_tuples(values) -> None:
    pairs = [(1, 2), (3, 4)]
    for value in values:
        assert value in pairs


def handed_to_unknown_code(r, values) -> None:
    s = [b"1", b"2", b"3"]
    r.sadd("a", s)
    for value in values:
        assert value in s


def data_sized(load, values) -> None:
    s = list(load())
    for value in values:
        assert value in s
"#;

#[test]
fn r06_small_displays_of_bytes_and_containers_are_not_reported() {
    // A display of at most 8 items that nothing grows is small whatever its items are; `*s`
    // passes the items, not the list. The same display handed to unknown code may grow, and a
    // list built at run time is data-sized.
    assert_reports(
        "reg_r06",
        R06_BYTES_DISPLAY,
        "OPT110",
        &[
            ("handed_to_unknown_code", "OPT110"),
            ("data_sized", "OPT110"),
        ],
    );
}

const UNHASHABLE_ELEMENTS: &str = r#"def heroes(rows) -> None:
    expected = [
        {"id": 1}, {"id": 2}, {"id": 3}, {"id": 4}, {"id": 5},
        {"id": 6}, {"id": 7}, {"id": 8}, {"id": 9},
    ]
    for row in rows:
        assert row in expected


def collected(rows, probes) -> list:
    seen = []
    for r in rows:
        seen.append([r, r])
    return [p for p in probes if p in seen]


def ints(rows) -> None:
    expected = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    for row in rows:
        assert row in expected
"#;

#[test]
fn opt110_does_not_advise_a_set_of_unhashable_elements() {
    // `set(expected)` would raise `TypeError` for lists of dicts or of lists: nothing to
    // advise. Hashable elements keep the report.
    assert_reports(
        "reg_unhashable",
        UNHASHABLE_ELEMENTS,
        "OPT110",
        &[("ints", "OPT110")],
    );
}

// ============================================================================ OPT112 / OPT113

const R04_SHRINK_ONLY: &str = r#"def drop_seen(entries) -> None:
    names = ["f", "g", "h"]
    for entry in entries:
        names.remove(entry)
    assert not names


def read_only(entries) -> list:
    names = ["f", "g", "h"]
    return [names.index(entry) for entry in entries]


def grown_first(entries, extra) -> None:
    names = ["f", "g", "h"]
    names.extend(extra)
    for entry in entries:
        names.remove(entry)
"#;

#[test]
fn r04_a_small_display_the_loop_only_shrinks_stays_small() {
    // The reported `remove()` itself shrinks the list: it never holds more than 3 items. A
    // display that grows before the loop is data-sized.
    assert_reports(
        "reg_r04",
        R04_SHRINK_ONLY,
        "OPT112",
        &[("grown_first", "OPT112")],
    );
}

const R18_DISPLAY_OF_CONTAINERS: &str = r#"def drain(calls: list[object]) -> None:
    expected_calls = [[{"id": 6}], [{"id": 3}], [{"id": 7}]]
    for call in expected_calls:
        calls.pop(calls.index(call))


def control(calls: list[object]) -> None:
    expected_calls = ["a", "b", "c"]
    for call in expected_calls:
        calls.pop(calls.index(call))


def enumerated(calls: list[object]) -> None:
    for i, call in enumerate([[1], [2]], 1):
        calls.pop(calls.index(call))


def loaded(calls: list[object], load) -> None:
    expected_calls = load()
    for call in expected_calls:
        calls.pop(calls.index(call))
"#;

#[test]
fn r18_a_loop_over_a_small_display_of_containers_runs_a_fixed_number_of_times() {
    assert_reports(
        "reg_r18",
        R18_DISPLAY_OF_CONTAINERS,
        "OPT112",
        &[("loaded", "OPT112")],
    );
}

const R02_R03_ALL_PAIRS: &str = r#"def check_pairs(models: list) -> None:
    for i, a in enumerate(models):
        for j, b in enumerate(models):
            if i == j:
                assert a == b
            else:
                assert a != b


def check_all_equal(models: list, wrap) -> None:
    for m1 in models:
        for m2 in models:
            assert m1 == m2
            assert wrap(m1) == wrap(m2)


def first_mismatch(orders, customers) -> None:
    for o in orders:
        for c in customers:
            if o.cid == c.id:
                print(o, c)
            else:
                break


def attach(orders, customers) -> list:
    out = []
    for o in orders:
        for c in customers:
            if o.cid == c.id:
                out.append((o, c))
            else:
                continue
    return out
"#;

#[test]
fn r02_r03_comparisons_whose_false_side_does_work_check_every_pair() {
    // An `else` body, an `assert` (its failure raises), or a `break` on a mismatch runs for
    // the pairs an index would skip: not joins. A join that goes on (`else: continue`) is.
    assert_reports(
        "reg_r02_r03",
        R02_R03_ALL_PAIRS,
        "OPT113",
        &[("attach", "OPT113")],
    );
}

// ============================================================================ OPT120 / OPT121

const R05_DRAINED_DISPLAY: &str = r#"import collections


def check_chunks(chunks) -> None:
    expected = [b"a", b""]
    for chunk in chunks:
        assert chunk == expected.pop(0)


def check_rows(cursor) -> None:
    expected = [(1,)]
    for row in cursor:
        assert row == expected.pop(0)


def bfs(start: int, children) -> list[int]:
    queue = [start]
    order = []
    while queue:
        node = queue.pop(0)
        order.append(node)
        queue.extend(children(node))
    return order


def uses_collections() -> collections.Counter:
    return collections.Counter()
"#;

#[test]
fn r05_a_small_display_the_loop_only_drains_is_neither_reported_nor_rewritten() {
    let diags = assert_reports(
        "reg_r05",
        R05_DRAINED_DISPLAY,
        "OPT120",
        &[("bfs", "OPT120")],
    );
    assert_eq!(
        replacements(&diags[0]),
        ["collections.deque([start])", "queue.popleft()"]
    );
    let p = Project::new("reg_r05_diff");
    p.write("case.py", R05_DRAINED_DISPLAY);
    let (_, diff, stderr) = p.check(&["--select", "OPT120", "--diff", "--include-size-dependent"]);
    assert!(stderr.contains("Would fix 1 issue(s)"), "{stderr}");
    assert!(!diff.contains("expected"), "{diff}");
}

const R07_REBUILT_PER_ITERATION: &str = r#"def group(moves, orient: str, default_vars: list[str]) -> list[list[str]]:
    out = []
    for move in moves:
        groupers = [*move.by, *default_vars]
        if move.group_by_orient:
            groupers.insert(0, orient)
        out.append(groupers)
    return out


def reverse(items) -> list:
    out = []
    for i in items:
        out.insert(0, i)
    return list(out)
"#;

#[test]
fn r07_a_list_rebuilt_in_every_iteration_does_not_accumulate_front_inserts() {
    assert_reports(
        "reg_r07",
        R07_REBUILT_PER_ITERATION,
        "OPT121",
        &[("reverse", "OPT121")],
    );
}

const R16_AND_TEST: &str = r#"def strip_and(items: list[str]) -> int:
    lines = []
    for item in items:
        lines.append(item)
    while lines and not lines[0]:
        del lines[0]
    return len(lines)


def strip_flag(items: list[str], flag) -> int:
    lines = []
    for item in items:
        lines.append(item)
    while lines and flag:
        del lines[0]
    return len(lines)


print(strip_and(["", "", "a"]), strip_flag(["x", "y"], False))
"#;

#[test]
fn r16_an_and_of_builtin_values_runs_no_user_code_and_blockers_name_their_line() {
    // `lines and not lines[0]` is a list or a bool: no `__bool__` / `__len__` runs, so `lines`
    // stays unique and the rewrite is proven. With an unknown operand the truth test may run
    // user code; the blocker names the `while` line, not the `def` line.
    let diags = assert_reports(
        "reg_r16",
        R16_AND_TEST,
        "OPT120",
        &[("strip_and", "OPT120"), ("strip_flag", "OPT120")],
    );
    assert_eq!(
        replacements(&diags[0]),
        [
            "import collections\n\n\n",
            "collections.deque([])",
            "lines.popleft()"
        ],
        "{:#}",
        diags[0]
    );
    assert_eq!(
        diags[1]["blocker"],
        "Unique(lines): it may be shared: passed to code heki cannot see (line 14)"
    );
    let p = Project::new("reg_r16_run");
    p.write("case.py", R16_AND_TEST);
    let before = run_python(&p.dir, "case.py");
    p.check(&["--select", "OPT120", "--fix", "--include-size-dependent"]);
    assert_eq!(
        before,
        run_python(&p.dir, "case.py"),
        "{}",
        p.read("case.py")
    );
}

// ================================================================================ DATA101

const R08_PASS_THROUGH_BRANCH: &str = r#"import numpy as np
import pandas as pd


def hull(groups, step: bool, draw) -> None:
    x = None
    for g in groups:
        if step:
            x = np.append(g, g[-1])
        draw(x)


def grow_sometimes(chunks):
    df = pd.DataFrame()
    for c in chunks:
        if c is not None:
            df = pd.concat([df, c])
    return df


def grow_always(rows):
    arr = np.array([])
    for r in rows:
        arr = np.append(arr, r)
    return arr
"#;

#[test]
fn r08_a_one_off_reallocation_on_one_branch_is_not_an_accumulation() {
    // `x` reaches the next iteration unchanged on the other branch, and its update does not
    // read `x`. An accumulator that grows on some iterations only is still quadratic.
    assert_reports(
        "reg_r08",
        R08_PASS_THROUGH_BRANCH,
        "DATA101",
        &[("grow_sometimes", "DATA101"), ("grow_always", "DATA101")],
    );
}

// ======================================================================== EFF102 / EFF103

const R14_BOUNDED_LOOPS: &str = r#"import urllib.request

from sqlalchemy.orm import Session

DOCUMENTS = {"twitter": 1, "canada": 2, "citm_catalog": 3}
ATTEMPTS = 5


def twice(sess: Session, stmt) -> None:
    for n, ident in enumerate((1, 2), 1):
        sess.execute(stmt)


def two_styles(sess: Session, stmt, order) -> None:
    for style, data in zip(order, ["m1", "m2"]):
        sess.execute(stmt)


def three_documents() -> None:
    for name, doc in DOCUMENTS.items():
        urllib.request.urlopen(f"https://example.com/{name}.json")


def three_hosts() -> None:
    hostnames = ["github.com", "aws.amazon.com", "ynet.co.il"]
    for hostname in hostnames:
        urllib.request.urlopen(f"https://{hostname}/")


def create_on_free_port():
    for _ in range(ATTEMPTS - 1):
        try:
            return urllib.request.urlopen("https://example.com/create")
        except OSError:
            continue
    return None


def per_name(names) -> None:
    for name in names:
        urllib.request.urlopen(f"https://example.com/{name}")


def grown_hosts(more) -> None:
    hostnames = ["github.com"]
    hostnames.extend(more)
    for hostname in hostnames:
        urllib.request.urlopen(f"https://{hostname}/")


def per_item_and_host(items) -> None:
    for item in items:
        for host in ("a.example.com", "b.example.com"):
            urllib.request.urlopen(f"https://{host}/{item}")


def per_row(sess: Session, stmt, rows) -> None:
    for row in rows:
        sess.execute(stmt)
"#;

#[test]
fn r14_loops_whose_trip_count_the_code_fixes_are_not_reported() {
    // `enumerate` / `zip` of small displays, a module constant dict, a local display, and a
    // retry loop over `range(ATTEMPTS - 1)`. Reported: data-sized loops, also around a bounded
    // inner loop, and a display that grows before the loop.
    let p = Project::new("reg_r14");
    p.write("case.py", R14_BOUNDED_LOOPS);
    let diags = diagnostics(&p, &["--select", "EFF102,EFF103"]);
    assert_eq!(
        reported(&diags),
        pairs(&[
            ("per_name", "EFF102"),
            ("grown_hosts", "EFF102"),
            ("per_item_and_host", "EFF102"),
            ("per_row", "EFF103"),
        ]),
        "{diags:#?}"
    );
}

// ======================================================================== fix quality

const R15_NO_IMPORT: &str = "\"\"\"A module without imports.\"\"\"\n\n\ndef bfs(start, children):\n    queue = [start]\n    order = []\n    while queue:\n        node = queue.pop(0)\n        order.append(node)\n        queue.extend(children(node))\n    return order\n\n\nprint(bfs(1, lambda n: [n + 1] if n < 4 else []))\n";

#[test]
fn r15_a_module_without_imports_gets_one_after_its_docstring_and_future_imports() {
    for (i, (head, fixed_head)) in [
        (
            "\"\"\"A module without imports.\"\"\"\n\n\n",
            "\"\"\"A module without imports.\"\"\"\n\nimport collections\n\n\n",
        ),
        (
            "\"\"\"Doc.\"\"\"\nfrom __future__ import annotations\n\n\n",
            "\"\"\"Doc.\"\"\"\nfrom __future__ import annotations\n\nimport collections\n\n\n",
        ),
        (
            "#!/usr/bin/env python\n",
            "#!/usr/bin/env python\nimport collections\n\n\n",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let body = R15_NO_IMPORT.split_once("\n\n\n").unwrap().1;
        let src = format!("{head}{body}");
        let p = Project::new(&format!("reg_r15_{i}"));
        p.write("case.py", &src);
        let before = run_python(&p.dir, "case.py");
        let (_, _, stderr) = p.check(&["--select", "OPT120", "--fix", "--include-size-dependent"]);
        assert!(stderr.contains("Fixed 1 issue(s)"), "{stderr}");
        let fixed = p.read("case.py");
        assert!(fixed.starts_with(fixed_head), "{fixed}");
        assert!(
            fixed.contains("queue = collections.deque([start])"),
            "{fixed}"
        );
        assert_eq!(before, run_python(&p.dir, "case.py"), "{fixed}");
    }
}

const IMPORT_BLOCKS: &str = "def bfs(start, children):\n    queue = [start]\n    order = []\n    while queue:\n        node = queue.pop(0)\n        order.append(node)\n        queue.extend(children(node))\n    return order\n";

#[test]
fn the_inserted_import_keeps_the_standard_library_block_sorted() {
    for (i, (imports, expected)) in [
        // Straight imports before `from` imports (isort's default).
        (
            "import atexit\nimport os\nfrom typing import Any\n",
            "import atexit\nimport collections\nimport os\nfrom typing import Any\n",
        ),
        // Sorted by module whatever the style (`force-sort-within-sections`, pytest).
        (
            "import atexit\nfrom collections.abc import Callable\nfrom contextlib import contextmanager\nimport fnmatch\n",
            "import atexit\nimport collections\nfrom collections.abc import Callable\nfrom contextlib import contextmanager\nimport fnmatch\n",
        ),
        // A section of its own before third-party imports.
        (
            "from __future__ import annotations\n\nimport requests\n",
            "from __future__ import annotations\n\nimport collections\n\nimport requests\n",
        ),
        // Last in the block.
        (
            "import abc\n\nimport requests\n",
            "import abc\nimport collections\n\nimport requests\n",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let p = Project::new(&format!("reg_sorted_{i}"));
        p.write("case.py", &format!("{imports}\n\n{IMPORT_BLOCKS}"));
        let (_, _, stderr) = p.check(&["--select", "OPT120", "--fix", "--include-size-dependent"]);
        assert!(stderr.contains("Fixed 1 issue(s)"), "{stderr}");
        let fixed = p.read("case.py");
        assert!(fixed.starts_with(expected), "{fixed}");
    }
}

const R17_ANNOTATIONS: &str = r#"from collections import deque
from collections.abc import MutableSequence


def annotated(parts: list[str]) -> list[str]:
    out: list[str] = []
    for part in parts:
        out.insert(0, part)
    return list(out)


def bare_list(parts: list[str]) -> list[str]:
    out: list = []
    for part in parts:
        out.insert(0, part)
    return list(out)


def abstract(parts: list[str]) -> list[str]:
    out: MutableSequence[str] = []
    for part in parts:
        out.insert(0, part)
    return list(out)


def declared_apart(parts: list[str]) -> list[str]:
    out: list[str]
    out = []
    for part in parts:
        out.insert(0, part)
    return list(out)


print(annotated(["a", "b"]), bare_list(["c"]), abstract(["d"]), declared_apart(["e"]))
"#;

#[test]
fn r17_a_deque_rewrite_rewrites_the_variables_annotation_or_is_blocked() {
    // `list[str]` / `list` annotations become `deque` ones (a type checker accepts the fixed
    // file); an annotation naming another type, or a declaration apart from the assignment
    // the rewrite changes, blocks the fix.
    let diags = assert_reports(
        "reg_r17",
        R17_ANNOTATIONS,
        "OPT121",
        &[
            ("annotated", "OPT121"),
            ("bare_list", "OPT121"),
            ("abstract", "OPT121"),
            ("declared_apart", "OPT121"),
        ],
    );
    assert_eq!(
        replacements(&diags[0]),
        ["deque([])", "out.appendleft(part)", "deque[str]"]
    );
    assert_eq!(
        replacements(&diags[1]),
        ["deque([])", "out.appendleft(part)", "deque"]
    );
    assert_eq!(
        diags[2]["blocker"],
        "DequeEquivalentOps(out): its annotation `MutableSequence[str]` (line 20) would not accept a `collections.deque`; rewrite it by hand"
    );
    assert_eq!(
        diags[3]["blocker"],
        "DequeEquivalentOps(out): `out` is declared with an annotation at line 27; rewrite it by hand"
    );
    let p = Project::new("reg_r17_run");
    p.write("case.py", R17_ANNOTATIONS);
    let before = run_python(&p.dir, "case.py");
    let (_, _, stderr) = p.check(&["--select", "OPT121", "--fix", "--include-size-dependent"]);
    assert!(stderr.contains("Fixed 2 issue(s)"), "{stderr}");
    let fixed = p.read("case.py");
    assert!(
        fixed.contains("    out: deque[str] = deque([])\n")
            && fixed.contains("    out: deque = deque([])\n"),
        "{fixed}"
    );
    assert_eq!(before, run_python(&p.dir, "case.py"), "{fixed}");
}

// ============================================================================== paths

#[test]
fn diff_headers_and_every_output_format_print_paths_from_the_working_directory() {
    // Run in `src` of a project rooted one level up: JSON, concise, and `--diff` all print
    // `pkg/a.py` (so `patch -p1` applies the diff where heki ran).
    let p = Project::new("reg_paths");
    p.write("src/pkg/__init__.py", "")
        .write("src/pkg/a.py", &format!("import os\n\n\n{IMPORT_BLOCKS}"));
    let run = |args: &[&str]| {
        let out = std::process::Command::new(env!("CARGO_BIN_EXE_heki"))
            .arg("check")
            .args(args)
            .current_dir(p.dir.join("src"))
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    let json: Value = serde_json::from_str(&run(&["--output-format", "json", "."])).unwrap();
    assert_eq!(json[0]["path"], "pkg/a.py", "{json:#}");
    let concise = run(&["--output-format", "concise", "."]);
    assert!(concise.starts_with("pkg/a.py:8:16: OPT120 "), "{concise}");
    let diff = run(&["--diff", "--include-size-dependent", "."]);
    assert!(
        diff.starts_with("--- a/pkg/a.py\n+++ b/pkg/a.py\n"),
        "{diff}"
    );
    // An absolute path inside the working directory prints the same way.
    let absolute = p.dir.join("src").canonicalize().unwrap();
    let concise = run(&["--output-format", "concise", absolute.to_str().unwrap()]);
    assert!(concise.starts_with("pkg/a.py:8:16: OPT120 "), "{concise}");
}

// ======================================================= declarations, star imports, elif

const STAR_MODULE: &str = "from os.path import *\n\n_OPS = [\"moveTo\", \"lineTo\", \"closePath\"]\nPUBLIC = [\"a\", \"b\"]\nfrom os import *\n\n\ndef ops(segs):\n    out = []\n    for s in segs:\n        out.append(_OPS.index(s))\n        out.append(PUBLIC.index(s))\n    return out\n";

const CLASS_ATTRIBUTES: &str = r#"class Base:
    detail_only_fields = []

    @classmethod
    def fields(cls, all_fields: list[str]):
        for field in cls.detail_only_fields:
            all_fields.remove(field)


class Pages(Base):
    detail_only_fields = ["parent"]


class Grown:
    names = ["a"]

    @classmethod
    def fields(cls, all_fields: list[str]):
        for field in cls.names:
            all_fields.remove(field)

    def add(self, x):
        self.names.append(x)
"#;

const TWO_KEYS: &str = r#"def two_keys(comps, dum):
    for i in range(len(comps)):
        for dx in dum:
            if dx[2] == i:
                print(dx)
            elif dx[3] == i:
                print(i)


def guarded(orders, customers):
    out = []
    for o in orders:
        for c in customers:
            if c is None:
                continue
            if o.cid == c.id:
                out.append((o, c))
    return out
"#;

#[test]
fn module_constants_class_attributes_and_elif_comparisons() {
    let p = Project::new("reg_decl_star_elif");
    p.write("pkg/__init__.py", "")
        .write("pkg/star.py", STAR_MODULE)
        .write("pkg/views.py", CLASS_ATTRIBUTES)
        .write("pkg/pairs.py", TWO_KEYS);
    let diags = diagnostics(&p, &["--select", "OPT112,OPT113"]);
    // A star import rebinds only the public names bound before it; a class attribute read
    // through `cls` is small when the class family only assigns small displays; an `elif`
    // comparison only sees the pairs the `if` before it rejected (a join filtered by
    // `continue` is still one).
    assert_eq!(
        reported(&diags),
        pairs(&[
            ("guarded", "OPT113"),
            ("ops", "OPT112"),
            ("Grown.fields", "OPT112"),
        ]),
        "{diags:#?}"
    );
    assert_eq!(diags[1]["target"], "PUBLIC");
}
