//! Sizes the code states: loops and containers whose size a builtin constructor, a
//! comprehension, a list filled by a fixed loop, a slice, a length test, or a module constant
//! fixes are not data-sized for any rule that skips fixed sizes (`EFF102`, `EFF103`, `DATA101`,
//! `OPT112`, `OPT113`, `OPT130`), under both comprehension models (a function of its own before
//! 3.12, inlined after); the same shapes over data stay reported.

mod common;

use common::Project;
use serde_json::Value;

/// `(file:line, rule)` of every diagnostic `heki check --select <select>` reports, sorted.
fn reported(p: &Project, select: &str, version: &str) -> Vec<(String, String)> {
    let select = common::select_with_siblings(select);
    let (code, stdout, stderr) = p.check(&[
        "--output-format",
        "json",
        "--select",
        &select,
        "--target-version",
        version,
    ]);
    assert!(code == 0 || code == 1, "exit {code}: {stderr}");
    let diags: Vec<Value> =
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"));
    let mut out: Vec<(String, String)> = diags
        .iter()
        .map(|d| {
            (
                format!("{}:{}", d["path"].as_str().unwrap(), d["line"]),
                d["rule_code"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = expected
        .iter()
        .map(|(at, rule)| ((*at).to_owned(), (*rule).to_owned()))
        .collect();
    out.sort();
    out
}

/// The reports of `select` are `expected` with Python 3.11 and with 3.12 semantics.
fn assert_both_versions(p: &Project, select: &str, expected: &[(&str, &str)]) {
    for version in ["py311", "py312"] {
        assert_eq!(reported(p, select, version), pairs(expected), "{version}");
    }
}

const CASES: &str = r#"import sqlite3

import httpx
import numpy as np

ALL_RESULTS = list(range(7))
SENTENCE_END = ".;:"
CATEGORY_LABELS = {
    "preferences": "Preferences",
    "projects": "Active projects",
    "relationships": "People",
    "wishes": "Wishes",
    "notes": "Notes",
}


def first_cases(replay_cases):
    cases = [case for case in replay_cases if isinstance(case, dict)]
    for case in cases[:3]:
        httpx.Client().post("/judge", json=case)


def later_cases(replay_cases):
    for case in replay_cases[3:]:
        httpx.Client().post("/judge", json=case)


def fixtures_by_comprehension(path):
    conn = sqlite3.connect(path)
    fixtures = [name.upper() for name in ("project-a", "project-b")]
    for fixture in fixtures:
        conn.execute("SELECT * FROM t WHERE n = ?", (fixture,))


def rows_by_comprehension(path, names):
    conn = sqlite3.connect(path)
    rows = [name.upper() for name in names]
    for row in rows:
        conn.execute("SELECT * FROM t WHERE n = ?", (row,))


def saved_specs(path):
    conn = sqlite3.connect(path)
    saved = []
    for key, value in {"a": 1, "b": 2, "c": 3, "d": 4}.items():
        saved.append((key, value))
    for key, value in saved:
        conn.execute("SELECT * FROM t WHERE n = ?", (key,))


def saved_rows(path, rows):
    conn = sqlite3.connect(path)
    saved = []
    for row in rows:
        saved.append(row)
    for row in saved:
        conn.execute("SELECT * FROM t WHERE n = ?", (row,))


def split_branch(out_ray_params, depth):
    for is_p_wave in [False, True]:
        out_ray_params = np.insert(out_ray_params, 0, depth if is_p_wave else -depth)
    return out_ray_params


def plan(positions):
    histories = np.empty((1, 0, 3))
    steps = 8
    for stage in range(4):
        for _ in range(steps):
            histories = np.concatenate((histories, positions[:, None]), axis=1)
    return histories


def accumulate(chunks):
    out = np.empty(0)
    for chunk in chunks:
        out = np.concatenate([out, chunk])
    return out


def summary(results: list[int]):
    s = []
    for status in ALL_RESULTS:
        s.append(results.count(status))
    return s


def summary_of(results: list[int], statuses):
    s = []
    for status in list(statuses):
        s.append(results.count(status))
    return s


def round_robin(overflow: dict):
    cats = [c for c in CATEGORY_LABELS if overflow.get(c)]
    while cats:
        for cat in list(cats):
            if not overflow[cat]:
                cats.remove(cat)


def round_robin_of(overflow: dict):
    cats = [c for c in overflow if overflow.get(c)]
    while cats:
        for cat in list(cats):
            if not overflow[cat]:
                cats.remove(cat)


def bottom_nav(pages):
    bottom = [r for r in ("overview", "groups", "contacts", "radar") if any(p.route == r for p in pages)]
    return "".join(p.label for r in bottom for p in pages if p.route == r)


def side_nav(pages, routes):
    shown = [r for r in routes if any(p.route == r for p in pages)]
    return "".join(p.label for r in shown for p in pages if p.route == r)


def first_paths(items, wanted):
    hits = 0
    for item in items:
        for w in wanted:
            if w in set(item["paths"][:2]):
                hits += 1
    return hits


def all_paths(items, wanted):
    hits = 0
    for item in items:
        for w in wanted:
            if w in set(item["paths"][1:]):
                hits += 1
    return hits


def ends_sentence(chars):
    return [c for c in chars if c in tuple(SENTENCE_END)]
"#;

#[test]
fn calls_comprehensions_filled_lists_and_slices_fix_sizes() {
    // Not reported: a slice with a constant stop (`[:3]`, of a list of any size), a
    // comprehension over a 2-tuple, a list appended once per item of a 4-entry dict display,
    // fixed `range` and display loops around an accumulation, `list(range(7))`, a filtered
    // comprehension over a 5-entry dict, a filtered comprehension over a 4-tuple joined with
    // data, `set()` of a 2-item slice, `tuple()` of a 3-character constant. Reported: the same
    // shapes over data (`[3:]`, a comprehension over a parameter, a list appended per row,
    // `list(statuses)`, a slice from 1).
    let p = Project::new("sizes_shapes");
    p.write("pkg/__init__.py", "").write("pkg/cases.py", CASES);
    let c = "pkg/cases.py";
    let at = |line: u32| format!("{c}:{line}");
    let expected: Vec<(String, &str)> = vec![
        (at(25), "EFF102"),
        (at(39), "EFF103"),
        (at(57), "EFF103"),
        (at(78), "DATA101"),
        (at(92), "OPT112"),
        (at(109), "OPT112"),
        (at(119), "OPT113"),
        (at(135), "OPT130"),
    ];
    let expected: Vec<(&str, &str)> = expected.iter().map(|(a, r)| (a.as_str(), *r)).collect();
    assert_both_versions(&p, "EFF102,EFF103,DATA101,OPT112,OPT113,OPT130", &expected);
}

const GUARDED: &str = r#"import sqlite3


def prepare(data):
    cards = data.get("cards")
    if not isinstance(cards, list) or not 1 <= len(cards) <= 6:
        raise ValueError("cards require 1..6 entries")
    ids = []
    for card in cards:
        ids.append(card["id"])
    links = data.get("links")
    if not isinstance(links, list) or not 1 <= len(links) <= 12:
        raise ValueError("links require 1..12 arrows")
    for link in links:
        if ids.index(link["from"]) >= ids.index(link["to"]):
            raise ValueError("arrows point forward")


def prepare_unchecked(data):
    cards = data.get("cards")
    ids = []
    for card in cards:
        ids.append(card["id"])
    for link in data.get("links"):
        if ids.index(link["from"]) >= ids.index(link["to"]):
            raise ValueError("arrows point forward")


def guarded(conn: sqlite3.Connection, xs, flag):
    if len(xs) > 5 or flag:
        raise ValueError
    for x in xs:
        conn.execute("SELECT ?", (x,))


def asserted(conn: sqlite3.Connection, xs):
    assert len(xs) <= 3
    for x in xs:
        conn.execute("SELECT ?", (x,))


def shrunk(conn: sqlite3.Connection, xs):
    while len(xs) > 3:
        xs.pop()
    for x in xs:
        conn.execute("SELECT ?", (x,))


def exact(conn: sqlite3.Connection, xs):
    if len(xs) != 2:
        return
    for x in xs:
        conn.execute("SELECT ?", (x,))


def both_needed(conn: sqlite3.Connection, xs, flag):
    if len(xs) > 5 and flag:
        raise ValueError
    for x in xs:
        conn.execute("SELECT ?", (x,))


def not_on_every_path(conn: sqlite3.Connection, xs):
    if len(xs) > 5:
        print("many")
    for x in xs:
        conn.execute("SELECT ?", (x,))


def grown_after(conn: sqlite3.Connection, xs, more):
    if len(xs) > 5:
        raise ValueError
    xs.extend(more)
    for x in xs:
        conn.execute("SELECT ?", (x,))


def rebound(conn: sqlite3.Connection, xs, load):
    if len(xs) > 5:
        raise ValueError
    xs = load()
    for x in xs:
        conn.execute("SELECT ?", (x,))


def large(conn: sqlite3.Connection, xs):
    if len(xs) > 100:
        raise ValueError
    for x in xs:
        conn.execute("SELECT ?", (x,))


def exact_on_the_right(conn: sqlite3.Connection, xs):
    if 3 != len(xs):
        raise ValueError
    for x in xs:
        conn.execute("SELECT ?", (x,))


def checked_before_other_branches(conn: sqlite3.Connection, xs, flag):
    if len(xs) > 4:
        raise ValueError
    if flag:
        print("flag")
    for x in xs:
        conn.execute("SELECT ?", (x,))


def returned_after_the_loop(conn: sqlite3.Connection, data):
    cards = data.get("cards")
    if cards is None:
        return None
    if not 1 <= len(cards) <= 6:
        raise ValueError
    for card in cards:
        conn.execute("SELECT ?", (card,))
    return {"cards": cards}


def handed_on_before_the_loop(conn: sqlite3.Connection, data, keep):
    cards = data.get("cards")
    if len(cards) > 6:
        raise ValueError
    keep(cards)
    for card in cards:
        conn.execute("SELECT ?", (card,))


def named_test(conn: sqlite3.Connection, xs):
    small = len(xs) <= 4
    if not small:
        raise ValueError
    for x in xs:
        conn.execute("SELECT ?", (x,))


def grown_while_iterated(conn: sqlite3.Connection, xs, children):
    if len(xs) > 6:
        return
    for x in xs:
        xs.extend(children(x))
        conn.execute("SELECT ?", (x,))
"#;

#[test]
fn a_length_test_on_every_path_to_a_loop_caps_it() {
    // Not reported: lists a `raise` caps (also through `not`, `or`, and a comparison chain,
    // and across later branches), the list filled once per capped item, an `assert`, a `while`
    // loop that shrinks the list to its cap, an exact length (on either side), a test bound to
    // a name first, and a list the function returns after the loop. Reported: a cap that needs another condition (`and`), a
    // test that does not leave, a list grown after the test, while the loop iterates it, or
    // handed to other code before the loop, a name rebound after the test, and a cap past 16.
    let p = Project::new("sizes_guards");
    p.write("guarded.py", GUARDED);
    let g = |line: u32| format!("guarded.py:{line}");
    let expected: Vec<(String, &str)> = vec![
        (g(25), "OPT112"),
        (g(25), "OPT112"),
        (g(60), "EFF103"),
        (g(67), "EFF103"),
        (g(75), "EFF103"),
        (g(83), "EFF113"),
        (g(90), "EFF113"),
        (g(126), "EFF113"),
        (g(142), "EFF103"),
    ];
    let expected: Vec<(&str, &str)> = expected.iter().map(|(a, r)| (a.as_str(), *r)).collect();
    assert_both_versions(&p, "EFF103,OPT112", &expected);
}

const FILLED: &str = r#"import sqlite3

GROWN = ["a", "b"]


def grow():
    GROWN.append("c")


def into_a_parameter(conn: sqlite3.Connection, acc):
    for x in ("a", "b"):
        acc.append(x)
    for a in acc:
        conn.execute("SELECT ?", (a,))


def fed_by_itself(conn: sqlite3.Connection):
    acc = [1]
    for x in acc:
        acc.append(x)
    for a in acc:
        conn.execute("SELECT ?", (a,))


def kept_across_iterations(conn: sqlite3.Connection, data):
    acc = None
    for x in data:
        if acc is None:
            acc = []
        acc.append(x)
    for a in acc:
        conn.execute("SELECT ?", (a,))


def extended_by_data(conn: sqlite3.Connection, data):
    acc = []
    acc.extend(data)
    for a in acc:
        conn.execute("SELECT ?", (a,))


def outer_and_inner(conn: sqlite3.Connection, data):
    outer = []
    for d in data:
        inner = []
        for k in ("a", "b"):
            outer.append(k)
            inner.append(k)
        for i in inner:
            conn.execute("SELECT ?", (i,))
    for o in outer:
        conn.execute("SELECT ?", (o,))


def module_list_grown_elsewhere(conn: sqlite3.Connection):
    for g in GROWN:
        conn.execute("SELECT ?", (g,))


def nested_comprehension(conn: sqlite3.Connection):
    pairs = [(x, y) for x in ("a", "b") for y in range(3)]
    for pair in pairs:
        conn.execute("SELECT ?", pair)


def nested_over_data(conn: sqlite3.Connection, data):
    pairs = [(x, y) for x in ("a", "b") for y in data]
    for pair in pairs:
        conn.execute("SELECT ?", pair)


def keyed(conn: sqlite3.Connection):
    table = {k: k.upper() for k in ("a", "b", "c")}
    for k in table:
        conn.execute("SELECT ?", (k,))


def built_empty(conn: sqlite3.Connection):
    seen = list()
    for k in ("a", "b"):
        seen.append(k)
    for s in seen:
        conn.execute("SELECT ?", (s,))


def pairs_past_sixteen(conn: sqlite3.Connection):
    acc = []
    for i in range(10):
        acc.append(i)
        acc.append(-i)
    for a in acc:
        conn.execute("SELECT ?", (a,))


def retried(conn: sqlite3.Connection, fetch):
    errors = []
    for attempt in range(3):
        try:
            return fetch()
        except OSError as e:
            errors.append(e)
    for e in errors:
        conn.execute("SELECT ?", (str(e),))


class Poller:
    def poll(self, conn: sqlite3.Connection, read):
        seen = []
        while self._running:
            seen.append(read())
        for s in seen:
            conn.execute("SELECT ?", (s,))
"#;

#[test]
fn a_new_list_grows_only_by_the_items_a_fixed_loop_adds() {
    // Reported: a parameter the function appends to, a list that feeds its own loop, a list
    // carried across a data loop, `extend` of data, the list made outside the data loop (the
    // one made in each iteration is small, but the data loop runs its query per item), a module
    // list another function grows, a comprehension with a data clause, a list filled past 16,
    // and a list a daemon loop fills. Not reported: a nested comprehension of fixed clauses, a
    // dict comprehension, `list()` filled by a fixed loop, and a list a retry loop over a fixed
    // range fills.
    let p = Project::new("sizes_filled");
    p.write("filled.py", FILLED);
    let f = |line: u32| format!("filled.py:{line}");
    let expected: Vec<(String, &str)> = vec![
        (f(14), "EFF103"),
        (f(22), "EFF113"),
        (f(32), "EFF113"),
        (f(39), "EFF113"),
        (f(50), "EFF113"),
        (f(52), "EFF113"),
        (f(57), "EFF113"),
        (f(92), "EFF113"),
        (f(112), "EFF113"),
    ];
    // The items of a comprehension with a data clause are the parameter's when Python 3.12
    // inlines it; Python 3.11 builds it in a frame of its own, whose items have no origin.
    for (version, comprehension) in [("py311", "EFF113"), ("py312", "EFF103")] {
        let mut expected = expected.clone();
        expected.push((f(69), comprehension));
        let expected: Vec<(&str, &str)> = expected.iter().map(|(a, r)| (a.as_str(), *r)).collect();
        assert_eq!(
            reported(&p, "EFF103", version),
            pairs(&expected),
            "{version}"
        );
    }
}

const DEFS: &str = r#"STATUS_ORDER = ["available", "caveated", "approximate", "unavailable"]
QUEUE = ["a", "b"]
STATEMENTS = ("CREATE TABLE a (x)", "CREATE TABLE b (y)")
REBOUND = ("CREATE TABLE c (z)",)
"#;

const GROWERS: &str = r#"from pkg.defs import QUEUE, STATEMENTS


def more(x):
    QUEUE.append(x)


def chain():
    out = []
    out.append(STATEMENTS)
    return out
"#;

const GEN_DOCS: &str = r#"from pkg.defs import QUEUE, STATUS_ORDER
from pkg.metrics import METRICS

by_status = [m.name for s in STATUS_ORDER for m in METRICS if m.status == s]
queued = [m.name for q in QUEUE for m in METRICS if m.kind == q]
for s in STATUS_ORDER:
    for m in METRICS:
        if m.status == s:
            print(m)


def in_a_function():
    return [m.name for s in STATUS_ORDER for m in METRICS if m.status == s]


def through_a_generator(pages):
    return [p for p in pages if any(p.status == s for s in STATUS_ORDER)]
"#;

const MIGRATE: &str = r#"import sqlite3

from pkg.defs import REBOUND, STATEMENTS

LOCAL_STATEMENTS = ("CREATE TABLE c (z)", "CREATE TABLE d (w)")
LOCAL_QUEUE = ["a", "b"]


def migrate(path):
    conn = sqlite3.connect(path)
    for statement in STATEMENTS:
        conn.execute(statement)
    for statement in LOCAL_STATEMENTS:
        conn.execute(statement)


def chain():
    out = []
    out.append(LOCAL_STATEMENTS)
    return out


def rebound_elsewhere(conn: sqlite3.Connection):
    for statement in REBOUND:
        conn.execute(statement)


def queued(conn: sqlite3.Connection):
    items = [q.upper() for q in LOCAL_QUEUE]
    for item in items:
        conn.execute("SELECT ?", (item,))
"#;

#[test]
fn module_constants_through_imports_module_bodies_and_tuples() {
    // Not reported: a module body's comprehension and loop over an imported list, the same
    // comprehension in a function, a generator expression over it, and loops over tuples that
    // other functions (in this module or another) put in a list. Reported: an imported list
    // another module grows, a comprehension over a list of this module another module grows,
    // and a tuple another module rebinds.
    let p = Project::new("sizes_modules");
    p.write("pkg/__init__.py", "")
        .write("pkg/defs.py", DEFS)
        .write("pkg/growers.py", GROWERS)
        .write("pkg/metrics.py", "METRICS = load()\n")
        .write("pkg/gen_docs.py", GEN_DOCS)
        .write("pkg/migrate.py", MIGRATE)
        .write(
            "pkg/reload.py",
            "from pkg import defs\n\n\ndef reload(rows):\n    defs.REBOUND = tuple(rows)\n",
        )
        .write(
            "pkg/feed.py",
            "from pkg.migrate import LOCAL_QUEUE\n\n\ndef feed(x):\n    LOCAL_QUEUE.append(x)\n",
        );
    assert_both_versions(
        &p,
        "EFF103,EFF113,OPT113",
        &[
            ("pkg/gen_docs.py:5", "OPT113"),
            ("pkg/migrate.py:25", "EFF113"),
            ("pkg/migrate.py:31", "EFF113"),
        ],
    );
}

#[test]
fn an_imported_display_its_module_binds_again_is_no_constant() {
    // Reported: a list another module fixes, read where the importing module binds the name
    // again in a function. Not reported: the same read in a module that does not.
    let user = |extra: &str| {
        format!(
            "from pkg.defs import STATUS_ORDER\nfrom pkg.metrics import METRICS\n{extra}\n\ndef by_status():\n    return [m.name for s in STATUS_ORDER for m in METRICS if m.status == s]\n"
        )
    };
    let p = Project::new("sizes_rebound_imports");
    p.write("pkg/__init__.py", "")
        .write("pkg/defs.py", DEFS)
        .write("pkg/metrics.py", "METRICS = load()\n")
        .write(
            "pkg/reorders.py",
            &user("\n\ndef reorder(order):\n    global STATUS_ORDER\n    STATUS_ORDER = order\n"),
        )
        .write("pkg/keeps.py", &user(""));
    assert_both_versions(&p, "OPT113", &[("pkg/reorders.py:11", "OPT113")]);
}

const SLICED: &str = r#"import sqlite3

NAMES = ("b", "a", "c")
PAIRS = (("a", 1), ("b", 2))


def bounded(conn: sqlite3.Connection, xs):
    for x in xs[:3]:
        conn.execute("SELECT ?", (x,))
    for x in xs[1:4]:
        conn.execute("SELECT ?", (x,))
    for x in xs[-5:3]:
        conn.execute("SELECT ?", (x,))
    for x in xs[-3:-1]:
        conn.execute("SELECT ?", (x,))
    for x in xs[-3:]:
        conn.execute("SELECT ?", (x,))
    for x in xs[:8:2]:
        conn.execute("SELECT ?", (x,))


def unbounded(conn: sqlite3.Connection, xs, n):
    for x in xs[1:]:
        conn.execute("SELECT ?", (x,))
    for x in xs[1:-1]:
        conn.execute("SELECT ?", (x,))
    for x in xs[3::-1]:
        conn.execute("SELECT ?", (x,))
    for x in xs[:n]:
        conn.execute("SELECT ?", (x,))
    for x in xs[:40]:
        conn.execute("SELECT ?", (x,))


def built(conn: sqlite3.Connection):
    for name in sorted(NAMES, key=len):
        conn.execute("SELECT ?", (name,))
    for name in frozenset(NAMES):
        conn.execute("SELECT ?", (name,))
    for name in dict(PAIRS):
        conn.execute("SELECT ?", (name,))
    seen = set()
    for name in NAMES:
        seen.add(name)
    table = {}
    for name, n in PAIRS:
        table.setdefault(name, n)
    order = []
    order.insert(0, "z")
    order.extend(NAMES)
    order.extend(PAIRS)
    totals = dict(PAIRS)
    totals.update({"c": 3})
    for x in seen | {"d"}:
        conn.execute("SELECT ?", (x,))
    for x in seen:
        conn.execute("SELECT ?", (x,))
    for x in table:
        conn.execute("SELECT ?", (x,))
    for x in order:
        conn.execute("SELECT ?", (x,))
    for x in totals:
        conn.execute("SELECT ?", (x,))


def not_built(conn: sqlite3.Connection, rows, n):
    for ch in str(n):
        conn.execute("SELECT ?", (ch,))
    for row in sorted(rows):
        conn.execute("SELECT ?", (row,))
    order = []
    order.extend(rows)
    for x in order:
        conn.execute("SELECT ?", (x,))


def letters(conn: sqlite3.Connection):
    for ch in "abcdefghijklmnopqrstuvwxyz"[:3]:
        conn.execute("SELECT ?", (ch,))
"#;

#[test]
fn slices_constructors_and_additions_by_fixed_counts() {
    // Not reported: slices whose bounds cap them (`[:3]`, `[1:4]`, `[-5:3]`, `[-3:-1]`,
    // `[-3:]`, `[:8:2]`), `sorted` / `frozenset` / `dict` of tuples, and new containers filled
    // by `add`, `setdefault`, `insert`, `extend`, and `update` of fixed iterables. Reported:
    // slices to the end, to an end-relative stop, backwards, to a stop the code does not fix,
    // or past 16; a set built by an operator; `str()`, `sorted()` of data, and `extend` of
    // data.
    let p = Project::new("sizes_sliced");
    p.write("sliced.py", SLICED);
    let s = |line: u32| format!("sliced.py:{line}");
    let expected: Vec<(String, &str)> = [24, 26, 28, 30, 70]
        .into_iter()
        .map(|line| (s(line), "EFF103"))
        .chain([32, 55, 68, 74].into_iter().map(|line| (s(line), "EFF113")))
        .collect();
    let expected: Vec<(&str, &str)> = expected.iter().map(|(a, r)| (a.as_str(), *r)).collect();
    assert_both_versions(&p, "EFF103", &expected);
}

/// Two loops after an import line (`{import}`): one over the indices of a parameter, one over a
/// constant range.
const INDEXED: &str = r#"import sqlite3
{import}


def by_index(conn: sqlite3.Connection, rows):
    for i in range(len(rows)):
        conn.execute("SELECT ?", (rows[i],))


def three(conn: sqlite3.Connection):
    for i in range(3):
        conn.execute("SELECT ?", (i,))
"#;

#[test]
fn a_star_import_may_bind_the_builtins_a_size_rests_on() {
    // A star import binds the names its module exports, which heki does not read, so after one
    // any builtin may be another function: `range(len(rows))` yields one item per row and
    // `range(3)` three items only with the real builtins. Without a star import the first loop
    // is an exact `EFF103` and the second is not reported. After the star import of a module
    // that binds `range` (`shadow`), or of one that binds no builtin (`helpers`), neither size
    // is known: no exact report, and both loops count as data-sized (`EFF113`).
    let p = Project::new("sizes_star_import");
    p.write("pkg/__init__.py", "")
        .write("pkg/shadow.py", "def range(n):\n    return [0] * 1000\n")
        .write("pkg/helpers.py", "def helper() -> None:\n    pass\n");
    for (module, import) in [
        ("plain", ""),
        ("starred", "from pkg.shadow import *"),
        ("helpers_star", "from pkg.helpers import *"),
    ] {
        p.write(
            &format!("pkg/{module}.py"),
            &INDEXED.replace("{import}", import),
        );
    }
    assert_both_versions(
        &p,
        "EFF103",
        &[
            ("pkg/plain.py:7", "EFF103"),
            ("pkg/starred.py:7", "EFF113"),
            ("pkg/starred.py:12", "EFF113"),
            ("pkg/helpers_star.py:7", "EFF113"),
            ("pkg/helpers_star.py:12", "EFF113"),
        ],
    );
}
