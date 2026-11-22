//! The premises a heuristic report of an exact claim leaves open (`heki dump-open-premises`,
//! `heki_rules::premises::OpenCause`): for each cause, a project whose report names its premise
//! and cause, and the same project with the cause closed in the code, which the exact rule
//! reports with nothing open. `heki check` writes none of it.

mod common;

use common::Project;
use serde_json::Value;

const EFF101: &str = "EFF101,EFF111";
const EFF103: &str = "EFF103,EFF113";

/// The reports `heki dump-open-premises` prints for `app.py` holding `source`.
fn reports(name: &str, select: &str, source: &str) -> Vec<Value> {
    let p = Project::new(name);
    p.write("app.py", source);
    let (code, stdout, stderr) = p.heki(&["dump-open-premises", "--select", select, "."]);
    assert_eq!(code, 0, "{stderr}");
    let out: Value = serde_json::from_str(&stdout).unwrap();
    out["reports"].as_array().unwrap().clone()
}

/// `(premise, cause)` of each open premise of `report`.
fn open_of(report: &Value) -> Vec<(String, String)> {
    report["open_premises"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| {
            (
                o["premise"].as_str().unwrap().to_owned(),
                o["cause"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// The one report of `source`: of `rule`, heuristic, of `exact`'s claim, with `open` open in
/// that order; returned for more checks.
fn heuristic(
    name: &str,
    select: &str,
    source: &str,
    rule: &str,
    exact: &str,
    open: &[(&str, &str)],
) -> Value {
    heuristic_of(name, select, source, rule, Value::from(exact), open)
}

/// [`heuristic`] for a rule whose heuristic reports keep its code: of its own claim.
fn own_heuristic(
    name: &str,
    select: &str,
    source: &str,
    rule: &str,
    open: &[(&str, &str)],
) -> Value {
    heuristic_of(name, select, source, rule, Value::Null, open)
}

fn heuristic_of(
    name: &str,
    select: &str,
    source: &str,
    rule: &str,
    exact_side: Value,
    open: &[(&str, &str)],
) -> Value {
    let found = reports(name, select, source);
    assert_eq!(found.len(), 1, "{found:#?}");
    let report = found[0].clone();
    assert_eq!(report["rule"], rule, "{report:#?}");
    assert_eq!(report["certainty"], "heuristic");
    assert_eq!(report["exact_side"], exact_side);
    let expected: Vec<(String, String)> = open
        .iter()
        .map(|(p, c)| ((*p).to_owned(), (*c).to_owned()))
        .collect();
    assert_eq!(open_of(&report), expected, "{report:#?}");
    report
}

/// The one report of `source`: exact, of `rule`, with nothing open.
fn exact(name: &str, select: &str, source: &str, rule: &str) {
    let found = reports(name, select, source);
    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(found[0]["rule"], rule, "{found:#?}");
    assert_eq!(found[0]["certainty"], "exact");
    assert_eq!(found[0]["exact_side"], Value::Null);
    assert!(open_of(&found[0]).is_empty(), "{found:#?}");
}

/// The open premise `i` of `report`: its closer, the line it is at, and its detail.
fn closer_line_detail(report: &Value, i: usize) -> (String, Option<u64>, Option<String>) {
    let o = &report["open_premises"][i];
    (
        o["closer"].as_str().unwrap().to_owned(),
        o["at"]["line"].as_u64(),
        o["detail"].as_str().map(str::to_owned),
    )
}

// --------------------------------------------------------------------------- EFF101's claim

#[test]
fn a_helper_that_returns_before_the_wait_leaves_every_return_open() {
    let open = "import time\n\n\ndef inner(flag):\n    if flag:\n        return None\n    time.sleep(1)\n\n\ndef outer(flag):\n    inner(flag)\n\n\nasync def handle(flag):\n    outer(flag)\n";
    let report = heuristic(
        "op_every_return",
        EFF101,
        open,
        "EFF111",
        "EFF101",
        &[("Every return", "not-made-on-every-return")],
    );
    // The open premise is at the call the helper does not make on every return, two down.
    assert_eq!(
        closer_line_detail(&report, 0),
        ("code".to_owned(), Some(7), None)
    );
    let closed = open.replace("    if flag:\n        return None\n", "");
    exact("op_every_return_closed", EFF101, &closed, "EFF101");
}

#[test]
fn a_call_behind_a_branch_the_code_decides_against_is_never_reached() {
    let open = "import time\n\n_IS_SYNC = False\n\n\nasync def handle():\n    if _IS_SYNC:\n        time.sleep(1)\n";
    heuristic(
        "op_never_reached",
        EFF101,
        open,
        "EFF111",
        "EFF101",
        &[("Every return", "never-reached")],
    );
    let closed = open.replace("_IS_SYNC = False", "_IS_SYNC = True");
    exact("op_never_reached_closed", EFF101, &closed, "EFF101");
}

#[test]
fn a_helper_that_always_raises_never_returns() {
    let open = "import time\n\n\ndef fail():\n    time.sleep(1)\n    raise ValueError(\"x\")\n\n\nasync def handle():\n    fail()\n";
    heuristic(
        "op_never_returns",
        EFF101,
        open,
        "EFF111",
        "EFF101",
        &[("Every return", "never-returns")],
    );
    let closed = open.replace("raise ValueError(\"x\")", "return None");
    exact("op_never_returns_closed", EFF101, &closed, "EFF101");
}

#[test]
fn a_receiver_typed_by_a_protocol_leaves_the_targets_incomplete() {
    let open = "import time\nfrom typing import Protocol\n\n\nclass Runner(Protocol):\n    def run(self) -> None: ...\n\n\nclass Impl:\n    def run(self) -> None:\n        time.sleep(1)\n\n\nasync def handle(obj: Runner):\n    obj.run()\n";
    let report = heuristic(
        "op_protocol",
        EFF101,
        open,
        "EFF111",
        "EFF101",
        &[("Every implementation", "incomplete-targets")],
    );
    assert_eq!(
        closer_line_detail(&report, 0),
        ("none".to_owned(), Some(15), None)
    );
    let closed = open.replace("obj: Runner", "obj: Impl");
    exact("op_protocol_closed", EFF101, &closed, "EFF101");
}

#[test]
fn an_override_that_does_not_wait_is_an_implementation_without_the_fact() {
    let open = "import time\n\n\nclass Base:\n    def run(self) -> None:\n        time.sleep(1)\n\n\nclass Quiet(Base):\n    def run(self) -> None:\n        return None\n\n\nasync def handle(obj: Base):\n    obj.run()\n";
    let report = heuristic(
        "op_override",
        EFF101,
        open,
        "EFF111",
        "EFF101",
        &[("Every implementation", "implementation-without-the-fact")],
    );
    assert_eq!(
        closer_line_detail(&report, 0),
        (
            "code".to_owned(),
            Some(15),
            Some("`app.Quiet.run`".to_owned())
        )
    );
    let closed = open.replace("        return None\n", "        time.sleep(2)\n");
    exact("op_override_closed", EFF101, &closed, "EFF101");
}

#[test]
fn a_function_a_builtin_calls_per_item_is_not_run_at_the_call() {
    let open = "import time\n\n\ndef load(x):\n    time.sleep(1)\n    return x\n\n\nasync def handle(xs):\n    return list(map(load, xs))\n";
    let report = heuristic(
        "op_per_item",
        EFF101,
        open,
        "EFF111",
        "EFF101",
        &[("Every implementation", "not-run-at-the-call")],
    );
    assert_eq!(closer_line_detail(&report, 0).2, None);
    let closed = open.replace("list(map(load, xs))", "[load(x) for x in xs]");
    exact("op_per_item_closed", EFF101, &closed, "EFF101");
}

#[test]
fn an_async_override_is_not_run_at_the_call() {
    let open = "import time\n\n\nclass Base:\n    def run(self):\n        time.sleep(1)\n\n\nclass Sub(Base):\n    async def run(self):\n        return None\n\n\nasync def handle(obj: Base):\n    obj.run()\n";
    let report = heuristic(
        "op_async_override",
        EFF101,
        open,
        "EFF111",
        "EFF101",
        &[("Every implementation", "not-run-at-the-call")],
    );
    assert_eq!(
        closer_line_detail(&report, 0).2.as_deref(),
        Some("`app.Sub.run`")
    );
    let closed = open.replace(
        "    async def run(self):\n        return None",
        "    def run(self):\n        time.sleep(2)",
    );
    exact("op_async_override_closed", EFF101, &closed, "EFF101");
}

#[test]
fn a_cache_may_not_run_the_body() {
    let open = "import functools\nimport time\n\n\n@functools.lru_cache(maxsize=None)\ndef step(n):\n    time.sleep(1)\n\n\nasync def handle(n):\n    step(n)\n";
    let report = heuristic(
        "op_decorated",
        EFF101,
        open,
        "EFF111",
        "EFF101",
        &[("Every implementation", "decorated")],
    );
    assert_eq!(
        closer_line_detail(&report, 0),
        ("none".to_owned(), Some(11), Some("`app.step`".to_owned()))
    );
    let closed = open.replace("@functools.lru_cache(maxsize=None)\n", "");
    exact("op_decorated_closed", EFF101, &closed, "EFF101");
}

#[test]
fn a_function_defined_twice_is_not_followed() {
    let open = "import sys\nimport time\n\nif sys.argv:\n    def step():\n        time.sleep(1)\nelse:\n    def step():\n        time.sleep(2)\n\n\nasync def handle():\n    step()\n";
    heuristic(
        "op_defined_twice",
        EFF101,
        open,
        "EFF111",
        "EFF101",
        &[("Every implementation", "defined-twice")],
    );
    let closed =
        "import time\n\n\ndef step():\n    time.sleep(1)\n\n\nasync def handle():\n    step()\n";
    exact("op_defined_twice_closed", EFF101, closed, "EFF101");
}

#[test]
fn a_wait_for_another_thread_may_end_at_once() {
    let open = "import queue\n\nQ = queue.Queue()\n\n\nasync def handle():\n    Q.get()\n";
    heuristic(
        "op_thread_wait",
        EFF101,
        open,
        "EFF111",
        "EFF101",
        &[("Waits", "waits-on-another-thread")],
    );
    let closed = "import time\n\n\nasync def handle():\n    time.sleep(1)\n";
    exact("op_thread_wait_closed", EFF101, closed, "EFF101");
}

#[test]
fn a_sleep_whose_length_the_caller_passes_may_not_wait() {
    let open = "import time\n\n\nasync def handle(delay):\n    time.sleep(delay)\n";
    heuristic(
        "op_conditional_wait",
        EFF101,
        open,
        "EFF111",
        "EFF101",
        &[("Waits", "conditional-wait")],
    );
    let closed = open.replace("time.sleep(delay)", "time.sleep(1)");
    exact("op_conditional_wait_closed", EFF101, &closed, "EFF101");
}

#[test]
fn each_premise_open_along_the_chain_is_recorded_at_its_call() {
    let open = "import time\n\n\nclass Base:\n    def run(self, delay) -> None:\n        if delay:\n            return None\n        time.sleep(delay)\n\n\nclass Quiet(Base):\n    def run(self, delay) -> None:\n        return None\n\n\nasync def handle(obj: Base, delay):\n    obj.run(delay)\n";
    let report = heuristic(
        "op_several",
        EFF101,
        open,
        "EFF111",
        "EFF101",
        &[
            ("Every implementation", "implementation-without-the-fact"),
            ("Every return", "not-made-on-every-return"),
            ("Waits", "conditional-wait"),
        ],
    );
    let lines: Vec<Option<u64>> = (0..3).map(|i| closer_line_detail(&report, i).1).collect();
    assert_eq!(lines, [Some(17), Some(8), Some(8)]);
    let closed = open
        .replace("        if delay:\n            return None\n", "")
        .replace("time.sleep(delay)", "time.sleep(1)")
        .replace("        return None\n", "        time.sleep(2)\n");
    exact("op_several_closed", EFF101, &closed, "EFF101");
}

// --------------------------------------------------------------------------- EFF103's claim

const DB: &str = "import sqlite3\n\nSQL = \"SELECT * FROM t WHERE id = ?\"\n\n\n";

fn db(body: &str) -> String {
    format!("{DB}{body}")
}

const QUERY_EACH_ITEM: &str = "def process(db: sqlite3.Cursor, items: list[str]):\n    for x in items:\n        db.execute(SQL, (x,))\n";

#[test]
fn a_helper_that_returns_before_the_query_leaves_every_return_open() {
    let open = db(
        "def query(db: sqlite3.Cursor, x, text):\n    try:\n        v = int(text)\n    except ValueError:\n        return None\n    return db.execute(SQL, (x,))\n\n\ndef process(db: sqlite3.Cursor, items: list[str], text: str):\n    for x in items:\n        query(db, x, text)\n",
    );
    let report = heuristic(
        "op_query_every_return",
        EFF103,
        &open,
        "EFF113",
        "EFF103",
        &[("Every return", "not-made-on-every-return")],
    );
    assert_eq!(closer_line_detail(&report, 0).1, Some(11));
    let closed = open.replace(
        "    try:\n        v = int(text)\n    except ValueError:\n        return None\n",
        "",
    );
    exact("op_query_every_return_closed", EFF103, &closed, "EFF103");
}

#[test]
fn a_query_through_a_protocol_leaves_the_targets_incomplete() {
    let open = db(
        "from typing import Protocol\n\n\nclass Store(Protocol):\n    def find(self, x): ...\n\n\nclass Sql:\n    def __init__(self, db: sqlite3.Cursor):\n        self.db = db\n\n    def find(self, x):\n        return self.db.execute(SQL, (x,))\n\n\ndef process(store: Store, items: list[str]):\n    for x in items:\n        store.find(x)\n",
    );
    heuristic(
        "op_query_protocol",
        EFF103,
        &open,
        "EFF113",
        "EFF103",
        &[("Every implementation", "incomplete-targets")],
    );
    let closed = open.replace("store: Store", "store: Sql");
    exact("op_query_protocol_closed", EFF103, &closed, "EFF103");
}

#[test]
fn a_session_lookup_may_be_answered_from_the_identity_map() {
    let open = "from sqlalchemy.orm import Session\n\n\nclass User:\n    pass\n\n\ndef process(session: Session, ids: list[int]):\n    out = []\n    for i in ids:\n        out.append(session.get(User, i))\n    return out\n";
    heuristic(
        "op_conditional_statement",
        EFF103,
        open,
        "EFF113",
        "EFF103",
        &[("Sends", "conditional-statement")],
    );
    let closed = open.replace("session.get(User, i)", "session.scalar(i)");
    exact("op_conditional_statement_closed", EFF103, &closed, "EFF103");
}

#[test]
fn a_continue_before_the_query_leaves_every_iteration_open() {
    let open = db(
        "def process(db: sqlite3.Cursor, items: list[str]):\n    for x in items:\n        if x == \"skip\":\n            continue\n        db.execute(SQL, (x,))\n",
    );
    let report = heuristic(
        "op_every_iteration",
        EFF103,
        &open,
        "EFF113",
        "EFF103",
        &[("Every iteration", "not-every-iteration")],
    );
    // At the loop.
    assert_eq!(
        closer_line_detail(&report, 0),
        ("code".to_owned(), Some(7), None)
    );
    let closed = open.replace("        if x == \"skip\":\n            continue\n", "");
    exact("op_every_iteration_closed", EFF103, &closed, "EFF103");
}

#[test]
fn a_query_a_builtin_runs_per_item_is_repeated_by_the_builtin() {
    let open = db(
        "def process(db: sqlite3.Cursor, items: list[str]):\n    return sorted(items, key=lambda x: db.execute(SQL, (x,)))\n",
    );
    let report = heuristic(
        "op_per_item_query",
        EFF103,
        &open,
        "EFF113",
        "EFF103",
        &[
            ("Every implementation", "not-run-at-the-call"),
            ("Every iteration", "repeated-by-a-builtin"),
        ],
    );
    assert_eq!(
        closer_line_detail(&report, 1),
        (
            "none".to_owned(),
            Some(7),
            Some("`builtins.sorted`".to_owned())
        )
    );
    let closed = db(
        "def process(db: sqlite3.Cursor, items: list[str]):\n    out = []\n    for x in items:\n        out.append(db.execute(SQL, (x,)))\n    return out\n",
    );
    exact("op_per_item_query_closed", EFF103, &closed, "EFF103");
}

#[test]
fn a_loop_over_a_fixed_display_is_fixed_by_the_code() {
    let open = db(
        "def process(db: sqlite3.Cursor, items: list[str]):\n    for x in items:\n        for k in [\"a\", \"b\", \"c\"]:\n            db.execute(SQL, (x, k))\n",
    );
    let report = heuristic(
        "op_fixed",
        EFF103,
        &open,
        "EFF113",
        "EFF103",
        &[("Origin", "fixed-by-the-code")],
    );
    // At the inner loop.
    assert_eq!(closer_line_detail(&report, 0).1, Some(8));
    let closed = db(QUERY_EACH_ITEM);
    exact("op_fixed_closed", EFF103, &closed, "EFF103");
}

#[test]
fn a_loop_over_a_slice_is_fixed_by_the_code() {
    let open = db(&QUERY_EACH_ITEM.replace("in items:", "in items[:1000]:"));
    heuristic(
        "op_slice",
        EFF103,
        &open,
        "EFF113",
        "EFF103",
        &[("Origin", "fixed-by-the-code")],
    );
    exact("op_slice_closed", EFF103, &db(QUERY_EACH_ITEM), "EFF103");
}

#[test]
fn a_loop_over_the_receiver_is_not_over_the_data() {
    let open = db(
        "class Store:\n    def __init__(self, db: sqlite3.Cursor, items: list[str]):\n        self.db = db\n        self.items = items\n\n    def process(self, items: list[str]):\n        for x in self.items:\n            self.db.execute(SQL, (x,))\n",
    );
    heuristic(
        "op_receiver",
        EFF103,
        &open,
        "EFF113",
        "EFF103",
        &[("Origin", "items-not-from-the-data")],
    );
    let closed = open.replace("in self.items:", "in items:");
    exact("op_receiver_closed", EFF103, &closed, "EFF103");
}

#[test]
fn a_loop_over_a_project_function_result_is_not_over_the_data() {
    let open = db(
        "def load(items):\n    return [i for i in items if i]\n\n\ndef process(db: sqlite3.Cursor, items: list[str]):\n    for x in load(items):\n        db.execute(SQL, (x,))\n",
    );
    heuristic(
        "op_project_items",
        EFF103,
        &open,
        "EFF113",
        "EFF103",
        &[("Origin", "items-not-from-the-data")],
    );
    let closed = open.replace("in load(items):", "in items:");
    exact("op_project_items_closed", EFF103, &closed, "EFF103");
}

#[test]
fn a_loop_over_a_library_result_the_catalog_does_not_size() {
    let open = db(
        "import json\n\n\ndef process(db: sqlite3.Cursor, text: str, items: list[str]):\n    for x in json.loads(text):\n        db.execute(SQL, (x,))\n",
    );
    let report = heuristic(
        "op_library_items",
        EFF103,
        &open,
        "EFF113",
        "EFF103",
        &[("Origin", "library-call-without-items")],
    );
    assert_eq!(closer_line_detail(&report, 0).0, "heki-catalog");
    let closed = open.replace("in json.loads(text):", "in items:");
    exact("op_library_items_closed", EFF103, &closed, "EFF103");
}

#[test]
fn a_while_loop_steps_through_no_items() {
    let open = db(
        "def process(db: sqlite3.Cursor, items: list[str]):\n    while items:\n        x = items.pop()\n        db.execute(SQL, (x,))\n",
    );
    let report = heuristic(
        "op_while",
        EFF103,
        &open,
        "EFF113",
        "EFF103",
        &[("Origin", "no-items-followed")],
    );
    // A `while` loop is located at its body when its test is a phi.
    assert_eq!(closer_line_detail(&report, 0).1, Some(8));
    exact("op_while_closed", EFF103, &db(QUERY_EACH_ITEM), "EFF103");
}

#[test]
fn each_loop_premise_open_is_recorded() {
    let open = db(
        "import json\n\n\ndef process(db: sqlite3.Cursor, text: str, items: list[str]):\n    for x in json.loads(text):\n        if x:\n            continue\n        db.execute(SQL, (x,))\n",
    );
    heuristic(
        "op_loop_several",
        EFF103,
        &open,
        "EFF113",
        "EFF103",
        &[
            ("Every iteration", "not-every-iteration"),
            ("Origin", "library-call-without-items"),
        ],
    );
    let closed = open
        .replace("in json.loads(text):", "in items:")
        .replace("        if x:\n            continue\n", "");
    exact("op_loop_several_closed", EFF103, &closed, "EFF103");
}

// -------------------------------------------------------------------- the OPT rewrites' claims

const OPT: &str = "OPT101,OPT102,OPT103,OPT110,OPT111,OPT120,OPT121,OPT130";

#[test]
fn an_accumulator_passed_to_unseen_code_leaves_the_rewrite_unproven() {
    let open = "def build(items, sink):\n    x = []\n    sink.keep(x)\n    for i in items:\n        x = x + [i]\n    return len(x)\n";
    let report = own_heuristic(
        "op_opt_unique",
        OPT,
        open,
        "OPT101",
        &[("Rewrite proven safe", "unique")],
    );
    // The obligation and its blocker, as the help names it.
    let (closer, line, detail) = closer_line_detail(&report, 0);
    assert_eq!((closer.as_str(), line), ("code", None));
    let detail = detail.unwrap();
    assert!(
        detail.starts_with("Unique(x): it may be shared"),
        "{detail}"
    );
    let closed = open.replace("    sink.keep(x)\n", "");
    exact("op_opt_unique_closed", OPT, &closed, "OPT101");
}

#[test]
fn a_read_of_the_accumulator_inside_the_loop_leaves_the_rewrite_unproven() {
    let open = "def build(items):\n    x = ()\n    for i in items:\n        x = x + (i,)\n        print(len(x))\n    return x\n";
    own_heuristic(
        "op_opt_reads",
        OPT,
        open,
        "OPT103",
        &[("Rewrite proven safe", "no-intermediate-reads")],
    );
    let closed = open.replace("        print(len(x))\n", "");
    exact("op_opt_reads_closed", OPT, &closed, "OPT103");
}

#[test]
fn a_probe_of_unknown_type_leaves_the_rewrite_unproven() {
    let open = "def count(items, n):\n    x = list(range(n * 7))\n    hits = 0\n    for i in items:\n        if i in x:\n            hits += 1\n    return hits\n";
    own_heuristic(
        "op_opt_probe",
        OPT,
        open,
        "OPT110",
        &[("Rewrite proven safe", "hash-consistent-probe")],
    );
    let closed = open.replace("if i in x:", "if len(str(i)) in x:");
    exact("op_opt_probe_closed", OPT, &closed, "OPT110");
}

#[test]
fn each_unproven_obligation_of_a_rewrite_is_recorded() {
    let open = "def build(items):\n    x = ()\n    for i in items:\n        x = x + (i,)\n        print(len(x))\n        locals()\n    return x\n";
    let report = own_heuristic(
        "op_opt_several",
        OPT,
        open,
        "OPT103",
        &[
            ("Rewrite proven safe", "frame-introspection"),
            ("Rewrite proven safe", "no-intermediate-reads"),
        ],
    );
    let detail = closer_line_detail(&report, 0).2.unwrap();
    assert!(
        detail.starts_with("A1HazardFree: A1 hazard: `locals`"),
        "{detail}"
    );
    let closed = open
        .replace("        print(len(x))\n", "")
        .replace("        locals()\n", "");
    exact("op_opt_several_closed", OPT, &closed, "OPT103");
}

// ------------------------------------------------------------------------------ OPT113's claim

const DATACLASSES: &str = "from collections.abc import Iterable\nfrom dataclasses import dataclass\n\n\n@dataclass\nclass Left:\n    key: int\n    other: int = 0\n\n\n@dataclass\nclass Right:\n    key: int\n    payload: int = 0\n\n\n@dataclass\nclass Prop:\n    raw: int\n    payload: int = 0\n\n    @property\n    def key(self):\n        return self.raw\n\n\ndef load(rows):\n    return list(rows)\n\n\n";

/// `join`, a nested-loop join of `lefts` with what the inner loop steps through, after the
/// classes `Left`, `Right`, and `Prop` (whose `key` is a property).
fn join(signature: &str, inner: &str, body: &str) -> String {
    format!(
        "{DATACLASSES}def join({signature}):\n    out = []\n    for a in lefts:\n        for b in {inner}:\n{body}    return out\n"
    )
}

const MATCH: &str =
    "            if a.key == b.key:\n                out.append((a.other, b.payload))\n";
const TYPED: &str = "lefts: list[Left], rights: list[Right]";

#[test]
fn a_key_that_is_a_property_may_run_code() {
    let open = join("lefts: list[Left], rights: list[Prop]", "rights", MATCH);
    let report = own_heuristic(
        "op_join_property",
        "OPT113",
        &open,
        "OPT113",
        &[("Equality of keys", "key-load-may-run-code")],
    );
    // At the load of the inner item's key.
    assert_eq!(
        closer_line_detail(&report, 0),
        ("none".to_owned(), Some(35), None)
    );
    exact(
        "op_join_property_closed",
        "OPT113",
        &join(TYPED, "rights", MATCH),
        "OPT113",
    );
}

#[test]
fn items_of_unknown_type_leave_the_key_loads_and_the_steps_open() {
    let open = join("lefts, rights", "rights", MATCH);
    let report = own_heuristic(
        "op_join_untyped",
        "OPT113",
        &open,
        "OPT113",
        &[
            ("Equality of keys", "item-type-unknown"),
            ("Idle on a mismatch", "iterable-type-unknown"),
        ],
    );
    assert_eq!(closer_line_detail(&report, 0).0, "annotation");
    assert_eq!(closer_line_detail(&report, 1).0, "annotation");
    exact(
        "op_join_untyped_closed",
        "OPT113",
        &join(TYPED, "rights", MATCH),
        "OPT113",
    );
}

#[test]
fn a_subscript_of_an_item_of_unknown_type_may_run_code() {
    let body = "            if a[\"id\"] == b[\"id\"]:\n                out.append(a)\n";
    let open = join("lefts: list[dict[str, int]], rights", "rights", body);
    own_heuristic(
        "op_join_subscript",
        "OPT113",
        &open,
        "OPT113",
        &[
            ("Equality of keys", "item-type-unknown"),
            ("Idle on a mismatch", "iterable-type-unknown"),
        ],
    );
    let closed = join(
        "lefts: list[dict[str, int]], rights: list[dict[str, int]]",
        "rights",
        body,
    );
    exact("op_join_subscript_closed", "OPT113", &closed, "OPT113");
}

#[test]
fn a_loop_over_the_receiver_does_not_step_through_inputs() {
    let open = format!(
        "{DATACLASSES}class Holder:\n    rows: list[Right] = []\n\n    def join(self, lefts: list[Left]):\n        out = []\n        for a in lefts:\n            for b in self.rows:\n                if a.key == b.key:\n                    out.append(a)\n        return out\n"
    );
    let report = own_heuristic(
        "op_join_receiver",
        "OPT113",
        &open,
        "OPT113",
        &[("Steps through inputs", "items-not-from-the-inputs")],
    );
    // At the inner loop's iterable.
    assert_eq!(closer_line_detail(&report, 0).1, Some(37));
    let closed = open
        .replace(
            "lefts: list[Left]):",
            "lefts: list[Left], rows: list[Right]):",
        )
        .replace("in self.rows:", "in rows:");
    exact("op_join_receiver_closed", "OPT113", &closed, "OPT113");
}

#[test]
fn a_loop_over_a_display_steps_through_items_the_code_fixes() {
    let body = "            if a == b:\n                out.append(a)\n";
    let display = format!(
        "[{}]",
        (1..=20)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let open = join("lefts: list[int]", &display, body);
    own_heuristic(
        "op_join_display",
        "OPT113",
        &open,
        "OPT113",
        &[("Steps through inputs", "fixed-by-the-code")],
    );
    let closed = join("lefts: list[int], rights: list[int]", "rights", body);
    exact("op_join_display_closed", "OPT113", &closed, "OPT113");
}

#[test]
fn work_on_a_mismatch_leaves_the_inner_loop_busy() {
    let before = format!("            out.append(None)\n{MATCH}");
    own_heuristic(
        "op_join_work",
        "OPT113",
        &join(TYPED, "rights", &before),
        "OPT113",
        &[("Idle on a mismatch", "work-on-a-mismatch")],
    );
    // A way out of the inner loop on a mismatch is work too.
    let stop = format!("{MATCH}            if b.payload < 0:\n                break\n");
    own_heuristic(
        "op_join_break",
        "OPT113",
        &join(TYPED, "rights", &stop),
        "OPT113",
        &[("Idle on a mismatch", "work-on-a-mismatch")],
    );
    exact(
        "op_join_work_closed",
        "OPT113",
        &join(TYPED, "rights", MATCH),
        "OPT113",
    );
}

#[test]
fn an_iterable_that_may_be_a_generator_runs_code_to_advance() {
    let open = join(
        "lefts: list[Left], rights: Iterable[Right]",
        "rights",
        MATCH,
    );
    own_heuristic(
        "op_join_iterable",
        "OPT113",
        &open,
        "OPT113",
        &[("Idle on a mismatch", "advancing-runs-code")],
    );
    exact(
        "op_join_iterable_closed",
        "OPT113",
        &join(TYPED, "rights", MATCH),
        "OPT113",
    );
}

#[test]
fn an_outer_loop_that_grows_the_inner_collection_or_rekeys_it() {
    let grow = format!(
        "{DATACLASSES}def join(lefts: list[Left], rights: list[Right], flag):\n    out = []\n    for a in lefts:\n        if flag and rights:\n            rights.append(rights[0])\n        for b in rights:\n{MATCH}    return out\n"
    );
    own_heuristic(
        "op_join_grow",
        "OPT113",
        &grow,
        "OPT113",
        &[("Same collection, same keys", "collection-may-change")],
    );
    let closed = grow.replace(
        "        if flag and rights:\n            rights.append(rights[0])\n",
        "",
    );
    exact("op_join_grow_closed", "OPT113", &closed, "OPT113");
    let rekey = format!("{MATCH}                b.key = -1\n");
    own_heuristic(
        "op_join_rekey",
        "OPT113",
        &join(TYPED, "rights", &rekey),
        "OPT113",
        &[("Same collection, same keys", "keys-may-change")],
    );
}

#[test]
fn each_premise_of_a_join_left_open_is_recorded() {
    let open = join(TYPED, "load(rights)", MATCH);
    own_heuristic(
        "op_join_several",
        "OPT113",
        &open,
        "OPT113",
        &[
            ("Same collection, same keys", "collection-may-change"),
            ("Steps through inputs", "items-not-from-the-inputs"),
            ("Equality of keys", "item-type-unknown"),
            ("Idle on a mismatch", "advancing-runs-code"),
        ],
    );
    exact(
        "op_join_several_closed",
        "OPT113",
        &join(TYPED, "rights", MATCH),
        "OPT113",
    );
}

// --------------------------------------------------------------------------- outputs

#[test]
fn heki_check_writes_no_open_premise() {
    let p = Project::new("op_check_json");
    p.write(
        "app.py",
        "import time\n\n\nasync def handle(delay):\n    time.sleep(delay)\n",
    );
    let (_, stdout, _) = p.check(&["--select", EFF101, "--output-format", "json"]);
    let out: Value = serde_json::from_str(&stdout).unwrap();
    let reports = out.as_array().unwrap();
    assert_eq!(reports.len(), 1, "{stdout}");
    let mut keys: Vec<&str> = reports[0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "blocker",
            "certainty",
            "column",
            "evidence",
            "fix",
            "help",
            "line",
            "message",
            "path",
            "qualname",
            "related",
            "rule_code",
            "rule_slug",
            "severity",
            "span",
            "target",
        ],
        "{stdout}"
    );
    assert!(!stdout.contains("open_premises") && !stdout.contains("conditional-wait"));
}
