//! Regressions from the hand review of heki's reports and patches on an open-source corpus
//! (reproductions R01–R09 and R14–R18): each false positive stays fixed (the true negative),
//! next to the true positive it must not hide; and the fixes keep type checkers and isort
//! satisfied, and print the paths every other output format prints.

mod common;

use common::{Project, repo_file, report_path, run_python};
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
                common::tiered_code(d).to_owned(),
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

const STR_PIECES: &str = r#"class Buffer:
    def __init__(self) -> None:
        self.text = ""

    def join_piece(self, rows) -> None:
        for row in rows:
            self.text += "".join(row)

    def format_method(self, rows) -> None:
        for row in rows:
            self.text += "{}:{}".format(row, 1)

    def str_of_int(self, rows) -> None:
        for row in rows:
            self.text += str(len(row))

    def repr_of_int(self, rows) -> None:
        for row in rows:
            self.text += repr(len(row))

    def ascii_of_str(self, rows) -> None:
        for row in rows:
            self.text += ascii("x")

    def format_of_int(self, rows) -> None:
        for row in rows:
            self.text += format(len(row), "x")

    def percent_of_int(self, rows) -> None:
        for row in rows:
            self.text += "%d" % len(row)

    def field_of_int(self, rows) -> None:
        for row in rows:
            self.text += f"{len(row)}"

    def fields(self, rows) -> None:
        for row in rows:
            self.text += f"{row}!"

    def concatenation(self, rows) -> None:
        for row in rows:
            self.text += str(len(row)) + "!"

    def str_of_unknown(self, rows) -> None:
        for row in rows:
            self.text += str(row)

    def repr_of_unknown(self, rows) -> None:
        for row in rows:
            self.text += repr(row)

    def format_of_unknown(self, rows) -> None:
        for row in rows:
            self.text += format(row)

    def field_of_unknown(self, rows) -> None:
        for row in rows:
            self.text += f"{row}"

    def percent_of_unknown(self, rows) -> None:
        for row in rows:
            self.text += "%s" % row

    def percent_of_tuple(self, rows) -> None:
        for row in rows:
            self.text += "%s" % (len(row),)

    def join_with_unknown_separator(self, sep, rows) -> None:
        for row in rows:
            self.text += sep.join(row)

    def unknown_call(self, rows, convert) -> None:
        for row in rows:
            self.text += convert(row)

    def shadowed(self, rows, str) -> None:
        for row in rows:
            self.text += str(len(row))
"#;

#[test]
fn str_pieces_of_known_type_are_reported_and_a_piece_of_unknown_type_is_not() {
    // A `str` subclass instance returned by another class's `__str__`, `__repr__`, or
    // `__format__` (or an operand with `__rmod__`) is not an exact `str`: `str(row)` of an
    // unknown `row` stays silent.
    assert_reports(
        "reg_str_pieces",
        STR_PIECES,
        "OPT103",
        &[
            ("Buffer.join_piece", "OPT103"),
            ("Buffer.format_method", "OPT103"),
            ("Buffer.str_of_int", "OPT103"),
            ("Buffer.repr_of_int", "OPT103"),
            ("Buffer.ascii_of_str", "OPT103"),
            ("Buffer.format_of_int", "OPT103"),
            ("Buffer.percent_of_int", "OPT103"),
            ("Buffer.field_of_int", "OPT103"),
            ("Buffer.fields", "OPT103"),
            ("Buffer.concatenation", "OPT103"),
        ],
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
    // A display of at most 16 items that nothing grows is small whatever its items are; `*s`
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
        {"id": 1}, {"id": 2}, {"id": 3}, {"id": 4}, {"id": 5}, {"id": 6},
        {"id": 7}, {"id": 8}, {"id": 9}, {"id": 10}, {"id": 11}, {"id": 12},
        {"id": 13}, {"id": 14}, {"id": 15}, {"id": 16}, {"id": 17},
    ]
    for row in rows:
        assert row in expected


def collected(rows, probes) -> list:
    seen = []
    for r in rows:
        seen.append([r, r])
    return [p for p in probes if p in seen]


def ints(rows) -> None:
    expected = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17]
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

const ARITHMETIC_ON_THE_PREVIOUS_VALUE: &str = r#"import numpy as np
import pandas as pd


def compose(turns):
    q = np.array([1.0, 0.0, 0.0, 0.0])
    for turn in turns:
        w1, v1, w2, v2 = turn[0], turn[1:], q[0], q[1:]
        q = np.concatenate([[w1 * w2 - v1 @ v2], w1 * v2 + w2 * v1 + np.cross(v1, v2)])
    return q


def solve(r, b):
    d = r[0:1]
    x = b[0:1, :]
    for j in range(1, len(b)):
        rf = r[0:j][::-1]
        a = (b[j, :] - np.dot(rf, x)) / (1 - np.dot(rf, d[::-1]))
        x = np.concatenate((x - np.outer(d[::-1], a), a[None, :]), axis=0)
        a = (r[j] - np.dot(rf, d)) / (1 - np.dot(rf, d[::-1]))
        d = np.concatenate((d - a * d[::-1], np.r_[a]))
    return x


def decay(rows, rate):
    acc = np.empty(0)
    for row in rows:
        acc = np.append(-acc * rate, row)
    return acc


def scale(rows, w):
    q = np.zeros(4)
    for row in rows:
        q = np.concatenate([[w * q[0]], w * q[1:]])
    return q


def scaled_parts(rows):
    acc = np.empty(0)
    for row in rows:
        acc = np.concatenate((acc, row * 2))
    return acc


def sliced(segments):
    pts = np.empty((0, 2))
    for seg in segments:
        pts = np.concatenate((pts[:-1], seg))
    return pts


def listed(parts):
    df = pd.DataFrame()
    for part in parts:
        df = pd.concat([df] + [part])
    return df


def converted(rows, batch):
    acc = np.empty((0, 3))
    for row in rows:
        acc = np.vstack(list(acc) + batch)
    return acc


def scaled_sometimes(rows, scale):
    acc = np.empty(0)
    for row in rows:
        base = acc * 2 if scale else acc
        acc = np.append(base, row)
    return acc
"#;

#[test]
fn a_value_rebuilt_from_arithmetic_on_the_previous_one_is_not_an_accumulation() {
    // Not reported: the reallocation combines only arithmetic on the previous value or on its
    // items (a new array whose every entry changed, also after unpacking
    // `w2, v2 = q[0], q[1:]`), so the loop already works in proportion to its size and has no
    // unchanged parts to collect. Reported: arithmetic on the new part only, a slice of the
    // previous value, a list concatenation holding it (`[df] + [part]`, `list(acc) + batch`),
    // and a branch that passes it on as it is.
    assert_reports(
        "reg_data101_arithmetic",
        ARITHMETIC_ON_THE_PREVIOUS_VALUE,
        "DATA101",
        &[
            ("scaled_parts", "DATA101"),
            ("sliced", "DATA101"),
            ("listed", "DATA101"),
            ("converted", "DATA101"),
            ("scaled_sometimes", "DATA101"),
        ],
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

const POLLS_AND_RETRIES: &str = r#"import sqlite3
import threading
import time
from time import sleep

import requests


def retry_counted() -> requests.Response | None:
    i = 1
    while i < 6:
        print(f"Attempt {i}...")
        try:
            return requests.get("https://example.com/data")
        except requests.RequestException:
            pass
        i += 1
    return None


def wait_for_health(base_url: str) -> None:
    for _ in range(30):
        try:
            if requests.get(f"{base_url}/health", timeout=2).status_code == 200:
                break
        except requests.RequestException:
            pass
        sleep(1)


def get_with_tries(uri: str, tries: int = 3) -> requests.Response:
    while True:
        tries -= 1
        response = requests.get(uri)
        if response.status_code == 200:
            return response
        if not tries:
            raise RuntimeError(response.status_code)
        time.sleep(15)


def job_done(url: str) -> bool:
    return requests.get(url).json()["done"]


def poll_job(url: str) -> None:
    while not job_done(url):
        time.sleep(5)


def serve_commands(stop: threading.Event, path: str) -> None:
    while not stop.is_set():
        sqlite3.connect(path).execute("SELECT 1")


def try_each_proxy(url: str, proxies: list[str]) -> requests.Response | None:
    for attempt, proxy in enumerate(proxies):
        try:
            return requests.get(url, proxies={"https": proxy})
        except requests.RequestException:
            time.sleep(attempt)
    return None


def per_url_paced(urls: list[str]) -> None:
    for url in urls:
        requests.get(url)
        time.sleep(1)


def per_message(read) -> None:
    while True:
        requests.post("https://example.com/hook", json=read())


def per_message_yielding(read) -> None:
    while True:
        requests.post("https://example.com/hook", json=read())
        time.sleep(0)


def per_message_unless_idle(read) -> None:
    while True:
        if message := read():
            requests.post("https://example.com/hook", json=message)
            continue
        time.sleep(1)
"#;

#[test]
fn loops_that_wait_for_a_condition_or_count_attempts_are_not_data_sized() {
    // Retries over a counter the code bounds, waits for a health check or a job, a loop until
    // a stop event, and attempts over a tuple target: none is a loop over data. Reported: a
    // loop over items, also one that sleeps per item, and resident loops that run without
    // waiting on some path (`sleep(0)` only yields).
    assert_reports(
        "reg_polls_and_retries",
        POLLS_AND_RETRIES,
        "EFF102,EFF103",
        &[
            ("per_url_paced", "EFF102"),
            ("per_message", "EFF102"),
            ("per_message_yielding", "EFF102"),
            ("per_message_unless_idle", "EFF102"),
        ],
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
        let out = common::heki_command()
            .args(["check", "--heuristic", "--extend-select", "OPT120"])
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
    // A star import of a module whose exports heki does not read (`os`) may rebind every name
    // bound before it, `_private` ones too (its `__all__` may list one), so they hold values of
    // unknown type; a class attribute read through `cls` is small when the class family only
    // assigns small displays; an `elif` comparison only sees the pairs the `if` before it
    // rejected (a join filtered by `continue` is still one).
    assert_eq!(
        reported(&diags),
        pairs(&[("guarded", "OPT113"), ("Grown.fields", "OPT112")]),
        "{diags:#?}"
    );
}

// ============================================================== the IR facts themselves

#[test]
fn loop_trip_counts_and_caps_the_code_fixes() {
    let p = Project::new("reg_loop_facts");
    p.write("m.py", &repo_file("tests/corpus/loop_facts.py"));
    let (code, stdout, stderr) = p.heki(&["dump-ir", "m.py", "--target-version", "py315"]);
    assert_eq!(code, 0, "{stderr}");
    let ir: Value = serde_json::from_str(&stdout).unwrap();
    // `STEP = LIMIT // 2` is not folded; `gone` is deleted.
    assert_eq!(
        ir["constants"],
        serde_json::json!({"ints": {"LIMIT": 8, "WIDTH": 1}, "bools": {"DEBUG": false}, "dynamic": false})
    );
    let small = |n: u64| serde_json::json!({ "const_small": n });
    let unbounded = serde_json::json!("unbounded");
    let expected = [
        ("strings", small(3)),
        ("sorted_keys", small(2)),
        ("reversed_list", small(3)),
        ("mapped", small(2)),
        ("filtered", small(3)),
        ("other_builtin", unbounded.clone()),
        ("enumerate_start", small(2)),
        ("zip_strict", small(2)),
        ("negative", small(3)),
        ("added", small(8)),
        ("unary_plus", small(3)),
        ("floor_division", unbounded.clone()),
        ("local_constant", small(3)),
        ("deleted", unbounded.clone()),
        // Past 16 copies a range object is not followed; a long copy chain of a display
        // still resolves to it.
        ("deep_copies", unbounded.clone()),
        ("deep_ints", unbounded.clone()),
        ("deep_display", small(1)),
        // A parameter named `range` is not the builtin; a constructor's result that grows,
        // or a container `min` returns and something grows, holds more than its input.
        ("shadowed_range", unbounded.clone()),
        ("grown_constructor", unbounded.clone()),
        ("ungrown_constructor", small(2)),
        ("returned_alias", unbounded.clone()),
        // A generator expression's loop runs over the iterable its creator passed it.
        ("generator_over_range.<locals>.<genexpr>", small(3)),
        // A collection drained in the loop is no flag, whatever its name; flags are matched
        // by whole words.
        ("drained_queue", unbounded.clone()),
        ("drained_active", unbounded.clone()),
        ("Worker.serve", serde_json::json!("daemon_polling")),
        ("Worker.until_shutdown", serde_json::json!("daemon_polling")),
        // An event is a flag too; its name decides as a plain flag's does.
        (
            "Worker.until_stop_event",
            serde_json::json!("daemon_polling"),
        ),
        ("Worker.until_ready_event", unbounded.clone()),
        // A `while` loop over a counter the code starts, steps, and limits with constants,
        // counting up or down, also to zero; not past 16 trips, not when a path back skips
        // the step, and not from a parameter or past the limit it tests for.
        ("counted_up", small(5)),
        ("counted_inclusive", small(6)),
        ("counted_down", small(3)),
        ("counted_mirrored", small(4)),
        ("counted_down_inclusive", small(4)),
        ("counted_once", small(1)),
        ("counted_down_strict", small(3)),
        ("counter_tested_by_identity", unbounded.clone()),
        ("counter_doubled", unbounded.clone()),
        ("counter_steps_differ", unbounded.clone()),
        ("counter_merged_before", unbounded.clone()),
        ("counted_past_small", unbounded.clone()),
        ("counter_skipped", unbounded.clone()),
        ("counter_from_parameter", unbounded.clone()),
        ("countdown_past_zero", unbounded.clone()),
        // A loop that waits on every path back (a sleep, a wait with a timeout) polls or
        // retries: `while True:` stays resident. A `for` loop reading its target steps
        // through items however it waits; a path back without the wait, a sleep an exception
        // can skip, `sleep(0)`, and `asyncio.wait(tasks)` pace nothing.
        ("paced_until", serde_json::json!("retry_pattern")),
        ("paced_attempts", serde_json::json!("retry_pattern")),
        ("paced_forever", serde_json::json!("daemon_polling")),
        ("paced_by_timed_wait", serde_json::json!("daemon_polling")),
        ("paced_by_timeout", serde_json::json!("daemon_polling")),
        ("zero_timeout", unbounded.clone()),
        // A coroutine waits where it awaits; a sleep it only schedules waits elsewhere.
        (
            "paced_by_awaited_sleep",
            serde_json::json!("daemon_polling"),
        ),
        ("sleep_only_scheduled", unbounded.clone()),
        (
            "paced_by_computed_delay",
            serde_json::json!("retry_pattern"),
        ),
        ("paced_over_items", unbounded.clone()),
        ("sleeps_on_one_path", unbounded.clone()),
        // A path back the module's constants rule out is no path back.
        (
            "paced_past_a_decided_branch",
            serde_json::json!("daemon_polling"),
        ),
        ("sleeps_after_a_raise", unbounded.clone()),
        ("zero_sleep", unbounded.clone()),
        ("waits_for_tasks", unbounded.clone()),
        // A retry body over a target with an attempt in a tuple.
        ("attempt_in_a_tuple", serde_json::json!("retry_pattern")),
    ];
    let functions = ir["functions"].as_array().unwrap();
    let loop_of = |name: &str| -> &Value {
        let f = functions.iter().find(|f| f["qualname"] == name).unwrap();
        &f["loops"][0]
    };
    for (name, bound) in expected {
        assert_eq!(loop_of(name)["bound"], bound, "{name}");
    }
    // A starred target binds a name too; what the body does with an attribute target is not
    // followed, so the loop steps through items.
    let unpacked = functions
        .iter()
        .find(|f| f["qualname"] == "unpacked_targets")
        .unwrap();
    let unpacked_bounds: Vec<&Value> = unpacked["loops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|lp| &lp["bound"])
        .collect();
    assert_eq!(unpacked_bounds, [&unbounded, &unbounded]);
    // `10 > len(buf)` and `len(buf) <= 9` cap at 10; `!=`, `len(buf) > 0`, and a starred
    // `len(*parts)` cap nothing.
    for name in ["cap_gt", "cap_lte"] {
        assert_eq!(loop_of(name)["len_caps"][0]["limit"], 10, "{name}");
    }
    for name in ["cap_ne", "cap_positive", "cap_starred"] {
        assert!(loop_of(name).get("len_caps").is_none(), "{name}");
    }
    // `globals()` at module level: no name is fixed.
    p.write(
        "dynamic.py",
        "HOSTS = [\"a\", \"b\"]\nglobals()[\"X\"] = 1\n\n\ndef each():\n    for h in HOSTS:\n        pass\n",
    );
    let (_, stdout, _) = p.heki(&["dump-ir", "dynamic.py", "--target-version", "py315"]);
    let ir: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(ir["constants"], serde_json::json!({"dynamic": true}));
}

// =========================================================== sizes read across modules

const CONSTS: &str = "HOSTS = [\"a.example.com\", \"b.example.com\"]\nGROWN = [\"x.example.com\"]\nLEVELS = [\"low\", \"mid\", \"high\"]\n\n\ndef add(host):\n    GROWN.append(host)\n";

const USES: &str = r#"import urllib.request

from pkg import consts
from pkg.consts import GROWN, HOSTS, LEVELS


def each_host():
    for h in HOSTS:
        urllib.request.urlopen(f"https://{h}/")


def each_grown():
    for h in GROWN:
        urllib.request.urlopen(f"https://{h}/")


def through_the_module():
    for h in consts.HOSTS:
        urllib.request.urlopen(f"https://{h}/")


def via_call(make):
    for h in make().hosts:
        urllib.request.urlopen(f"https://{h}/")


def ranks(xs) -> list:
    levels = LEVELS
    out = []
    for x in xs:
        out.append(levels.index(x))
    return out
"#;

const ATTRIBUTES: &str = r#"class Base:
    names = ["a", "b"]


class Child(Base):
    def scan(self, rows: list[str]) -> None:
        for n in super().names:
            rows.remove(n)


class Picks:
    def __init__(self) -> None:
        self.order = ["a", "b"]

    def reset(self) -> None:
        self.order = ["c"]

    def drop(self) -> None:
        del self.gone

    def sizes(self):
        yield len(self.order)

    def later(self):
        items = [1]
        return lambda: items


def scan(p: Picks, rows) -> None:
    for r in rows:
        p.order.index(r)


class Mixed:
    def __init__(self) -> None:
        self.pair = ["a"]

    def flip(self) -> None:
        self.pair = ("b",)


def counter():
    count = 0

    class Local:
        def bump(self) -> None:
            nonlocal count
            count = 1

    return Local


def captured(xs) -> list:
    names = ["a", "b"]

    def show():
        return names

    out = []
    for x in xs:
        out.append(names.index(x))
    return out


def item_store(xs) -> list:
    names = ["a", "b"]
    names[0] = "c"
    out = []
    for x in xs:
        out.append(names.index(x))
    return out


def index_store(xs, k) -> list:
    names = ["a", "b"]
    names[k] = ["c", "d", "e"]
    out = []
    for x in xs:
        out.append(names.index(x))
    return out


def returned(xs) -> list:
    names = ["a", "b"]
    for x in xs:
        names.index(x)
    return names


def drain_slices() -> list:
    q = ["a", "b"]
    out = []
    while q:
        out.append(q[0])
        q = q[1:]
    return out
"#;

#[test]
fn sizes_of_other_modules_attributes_and_escaping_displays() {
    // Imported and `module.NAME` displays that nothing grows bound their loops; one another
    // function grows does not. A class attribute stored twice with small displays stays
    // small; `super().names` is not resolved. A display a closure captures or the function
    // returns may grow, and so may a list stored at an unknown index (`names[k]` may be a
    // slice); one the loop only slices or stores at `names[0]` stays small.
    let p = Project::new("reg_cross_sizes");
    p.write("pkg/__init__.py", "")
        .write("pkg/consts.py", CONSTS)
        .write("pkg/uses.py", USES)
        .write("pkg/attrs.py", ATTRIBUTES);
    let diags = diagnostics(&p, &["--select", "EFF102,OPT112,OPT120"]);
    assert_eq!(
        reported(&diags),
        pairs(&[
            ("Child.scan", "OPT112"),
            ("captured", "OPT112"),
            ("index_store", "OPT112"),
            ("returned", "OPT112"),
            ("each_grown", "EFF102"),
            ("via_call", "EFF102"),
        ]),
        "{diags:#?}"
    );
}

const MORE_DEQUES: &str = r#"import collections


def bfs(start, children) -> list:
    queue = [start] if start else []
    order = []
    while queue:
        node = queue.pop(0)
        order.append(node)
        queue.extend(children(node))
    return order


def quoted(parts) -> list:
    out: "list[str]" = []
    for part in parts:
        out.insert(0, part)
    return list(out)


def optional(parts) -> list:
    out: list[str] | None = []
    for part in parts:
        out.insert(0, part)
    return list(out)


print(bfs(1, lambda n: [n + 1] if n < 3 else []), quoted(["a", "b"]))
"#;

#[test]
fn conditional_creations_and_annotation_spellings() {
    // Both displays of a conditional creation become deques (no annotation to rewrite); a
    // quoted annotation stays quoted; a union is not rewritten mechanically.
    let diags = assert_reports(
        "reg_more_deques",
        MORE_DEQUES,
        "OPT120,OPT121",
        &[
            ("bfs", "OPT120"),
            ("quoted", "OPT121"),
            ("optional", "OPT121"),
        ],
    );
    assert_eq!(
        replacements(&diags[0]),
        [
            "collections.deque([start])",
            "collections.deque([])",
            "queue.popleft()"
        ]
    );
    assert_eq!(
        replacements(&diags[1]),
        [
            "collections.deque([])",
            "out.appendleft(part)",
            "\"collections.deque[str]\""
        ]
    );
    assert_eq!(
        diags[2]["blocker"],
        "DequeEquivalentOps(out): its annotation `list[str] | None` (line 22) would not accept a `collections.deque`; rewrite it by hand"
    );
    let p = Project::new("reg_more_deques_run");
    p.write("case.py", MORE_DEQUES);
    let before = run_python(&p.dir, "case.py");
    let (_, _, stderr) = p.check(&[
        "--select",
        "OPT120,OPT121",
        "--fix",
        "--include-size-dependent",
    ]);
    assert!(stderr.contains("Fixed 2 issue(s)"), "{stderr}");
    assert_eq!(
        before,
        run_python(&p.dir, "case.py"),
        "{}",
        p.read("case.py")
    );
}

#[test]
fn the_inserted_import_skips_comments_parenthesized_and_guarded_imports() {
    // A comment above an import stays with it, `from os import path, sep` and a parenthesized
    // import are one statement each, and an import inside `try:` ends the block.
    let imports = "# standard library\nimport atexit\nfrom os import path, sep\nfrom typing import (\n    Any,\n    cast,\n)\n\ntry:\n    import ujson\nexcept ImportError:\n    ujson = None\n";
    let p = Project::new("reg_import_shapes");
    p.write("case.py", &format!("{imports}\n\n{IMPORT_BLOCKS}"));
    let (_, _, stderr) = p.check(&["--select", "OPT120", "--fix", "--include-size-dependent"]);
    assert!(stderr.contains("Fixed 1 issue(s)"), "{stderr}");
    let fixed = p.read("case.py");
    assert!(
        fixed.starts_with(
            "# standard library\nimport atexit\nimport collections\nfrom os import path, sep\n"
        ),
        "{fixed}"
    );
}

#[test]
fn absolute_paths_print_from_the_working_directory_through_links_and_stay_absolute_outside() {
    let p = Project::new("reg_paths_absolute");
    p.write("src/pkg/__init__.py", "")
        .write("src/pkg/a.py", &format!("import os\n\n\n{IMPORT_BLOCKS}"))
        .write("src/other.py", &format!("import os\n\n\n{IMPORT_BLOCKS}"));
    let run = |cwd: &std::path::Path, arg: &std::path::Path| {
        let out = common::heki_command()
            .args(["check", "--heuristic", "--extend-select", "OPT120"])
            .args(["--output-format", "concise"])
            .arg(arg)
            .current_dir(cwd)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    // The temporary directory may be reached through a symbolic link (`/var` on macOS): the
    // path as given and the canonical working directory still match.
    let src = p.dir.join("src");
    let out = run(&src, &src);
    assert!(
        out.contains("\npkg/a.py:8:16: OPT120 ") || out.starts_with("other.py"),
        "{out}"
    );
    assert!(out.contains("other.py:8:16: OPT120 "), "{out}");
    // Outside the working directory a path stays absolute, as given.
    let out = run(&src.join("pkg"), &src);
    assert!(out.contains("a.py:8:16: OPT120 "), "{out}");
    let other = format!("{}:8:16: OPT120 ", report_path(&src.join("other.py")));
    assert!(out.lines().any(|l| l.starts_with(&other)), "{out}");
}

/// A path given through a symbolic link into the working directory prints relative to it, on
/// every platform (not only where the temporary directory itself is a link).
// Unix only: creating a symbolic link on Windows needs a privilege (or Developer Mode) that a
// test run cannot assume.
#[cfg(unix)]
#[test]
fn absolute_paths_through_a_link_into_the_working_directory_print_relative() {
    let p = Project::new("reg_paths_link");
    p.write("src/pkg/__init__.py", "")
        .write("src/pkg/a.py", &format!("import os\n\n\n{IMPORT_BLOCKS}"));
    let src = p.dir.join("src");
    let link = p.dir.join("link");
    std::os::unix::fs::symlink(&src, &link).unwrap();
    let out = common::heki_command()
        .args(["check", "--heuristic", "--extend-select", "OPT120"])
        .args(["--output-format", "concise"])
        .arg(&link)
        .current_dir(&src)
        .output()
        .unwrap();
    let out = String::from_utf8(out.stdout).unwrap();
    assert!(out.starts_with("pkg/a.py:8:16: OPT120 "), "{out}");
}
