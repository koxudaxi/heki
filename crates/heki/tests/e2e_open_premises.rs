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
fn a_name_the_project_binds_again_leaves_the_targets_incomplete() {
    let open = "import time\n\n\ndef load() -> None:\n    time.sleep(1)\n\n\ndef quiet() -> None:\n    global load\n    load = print\n\n\nasync def handle():\n    load()\n";
    let report = heuristic(
        "op_rebound",
        EFF101,
        open,
        "EFF111",
        "EFF101",
        &[("Every implementation", "incomplete-targets")],
    );
    assert_eq!(
        closer_line_detail(&report, 0),
        ("none".to_owned(), Some(14), None)
    );
    let closed = open.replace("    load = print\n", "");
    exact("op_rebound_closed", EFF101, &closed, "EFF101");
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

// ----------------------------------------------------------------------------- DATA101's claim

/// A module importing numpy, with `body` after it.
fn arrays(body: &str) -> String {
    format!("import numpy as np\n\n\n{body}")
}

const GROWN_EACH_ITEM: &str = "def build(items: list[str]):\n    acc = np.array([])\n    for c in items:\n        acc = np.append(acc, c)\n    return acc\n";

#[test]
fn a_helper_that_may_return_another_value_is_not_the_catalog_call() {
    let open = arrays(
        "def grow(arr, c, flag):\n    if flag:\n        return arr\n    return np.append(arr, c)\n\n\ndef build(items: list[str], flag):\n    acc = np.array([])\n    for c in items:\n        acc = grow(acc, c, flag)\n    return acc\n",
    );
    let report = own_heuristic(
        "op_data_helper",
        "DATA101",
        &open,
        "DATA101",
        &[("Reallocates", "not-the-catalog-call")],
    );
    assert_eq!(
        closer_line_detail(&report, 0),
        ("none".to_owned(), None, None)
    );
    exact(
        "op_data_helper_closed",
        "DATA101",
        &arrays(GROWN_EACH_ITEM),
        "DATA101",
    );
}

#[test]
fn a_reset_on_one_path_leaves_the_value_not_carried() {
    let open = arrays(&GROWN_EACH_ITEM.replace(
        "        acc = np.append(acc, c)\n",
        "        acc = np.append(acc, c)\n        if c == \"0\":\n            acc = np.array([])\n",
    ));
    own_heuristic(
        "op_data_reset",
        "DATA101",
        &open,
        "DATA101",
        &[("Loop-carried on every path", "reset-on-a-path")],
    );
    exact(
        "op_data_reset_closed",
        "DATA101",
        &arrays(GROWN_EACH_ITEM),
        "DATA101",
    );
}

#[test]
fn a_previous_value_on_some_paths_only_is_not_held() {
    let open = arrays(
        "def build(items: list[str], other, flag):\n    acc = np.array([])\n    for c in items:\n        acc = np.append(acc if flag else other, c)\n    return acc\n",
    );
    own_heuristic(
        "op_data_held",
        "DATA101",
        &open,
        "DATA101",
        &[("Loop-carried on every path", "previous-value-not-held")],
    );
    exact(
        "op_data_held_closed",
        "DATA101",
        &arrays(GROWN_EACH_ITEM),
        "DATA101",
    );
}

#[test]
fn a_continue_before_the_update_leaves_every_iteration_open() {
    let open = arrays(&GROWN_EACH_ITEM.replace(
        "    for c in items:\n",
        "    for c in items:\n        if c == \"skip\":\n            continue\n",
    ));
    own_heuristic(
        "op_data_every",
        "DATA101",
        &open,
        "DATA101",
        &[("Every iteration", "not-every-iteration")],
    );
    exact(
        "op_data_every_closed",
        "DATA101",
        &arrays(GROWN_EACH_ITEM),
        "DATA101",
    );
}

#[test]
fn a_loop_not_over_the_data_leaves_the_origin_open() {
    for (name, shape, cause) in [
        (
            "op_data_fixed",
            GROWN_EACH_ITEM.replace("for c in items:", "for c in range(40):"),
            "fixed-by-the-code",
        ),
        (
            "op_data_receiver",
            "class Holder:\n    def build(self):\n        acc = np.array([])\n        for c in self.rows:\n            acc = np.append(acc, c)\n        return acc\n".to_owned(),
            "items-not-from-the-data",
        ),
        (
            "op_data_while",
            "def build(n: int):\n    acc = np.array([])\n    while n:\n        acc = np.append(acc, n)\n        n -= 1\n    return acc\n".to_owned(),
            "no-items-followed",
        ),
    ] {
        own_heuristic(name, "DATA101", &arrays(&shape), "DATA101", &[("Origin", cause)]);
    }
    exact(
        "op_data_origin_closed",
        "DATA101",
        &arrays(GROWN_EACH_ITEM),
        "DATA101",
    );
}

#[test]
fn each_premise_of_an_update_left_open_is_recorded() {
    let open = arrays(
        "def grow(arr, c, flag):\n    if flag:\n        return arr\n    return np.append(arr, c)\n\n\ndef build(items: list[str], flag):\n    acc = np.array([])\n    for c in items:\n        if c == \"skip\":\n            continue\n        acc = grow(acc, c, flag)\n        if c == \"0\":\n            acc = np.array([])\n    return acc\n",
    );
    own_heuristic(
        "op_data_several",
        "DATA101",
        &open,
        "DATA101",
        &[
            ("Reallocates", "not-the-catalog-call"),
            ("Every iteration", "not-every-iteration"),
            ("Loop-carried on every path", "reset-on-a-path"),
        ],
    );
    exact(
        "op_data_several_closed",
        "DATA101",
        &arrays(GROWN_EACH_ITEM),
        "DATA101",
    );
}

// ------------------------------------------------------------------------------ MUT110's claim

const APPEND_TO_LISTS: &str = "from collections.abc import Sequence\n\n\ndef append_to_lists(values):\n    if isinstance(values, list):\n        values.append(1)\n\n\ndef entry(values: Sequence[float]) -> None:\n    append_to_lists(values)\n";

#[test]
fn a_change_a_project_subclass_may_replace_names_no_line() {
    let open = APPEND_TO_LISTS.replace(
        "\n\ndef append_to_lists",
        "\n\nclass Frozen(list):\n    def append(self, item):\n        return None\n\n\ndef append_to_lists",
    );
    let report = own_heuristic(
        "op_mut110_subclass",
        "MUT110",
        &open,
        "MUT110",
        &[("Changed at the line", "not-a-builtin-container-change")],
    );
    assert_eq!(
        closer_line_detail(&report, 0),
        (
            "none".to_owned(),
            None,
            Some("`app.append_to_lists`".to_owned())
        )
    );
    exact(
        "op_mut110_subclass_closed",
        "MUT110",
        APPEND_TO_LISTS,
        "MUT110",
    );
}

#[test]
fn a_change_where_a_returned_generator_is_iterated_has_no_changing_body() {
    let open = "from collections.abc import Sequence\n\n\ndef wipe(values):\n    values.clear()\n    yield 1\n\n\ndef adapter(values):\n    return wipe(values)\n\n\ndef consume(values):\n    list(adapter(values))\n\n\ndef entry(values: Sequence[float]) -> None:\n    consume(values)\n";
    own_heuristic(
        "op_mut110_generator",
        "MUT110",
        open,
        "MUT110",
        &[("Changed at the line", "no-changing-body")],
    );
    let closed = open.replace(
        "    list(adapter(values))\n",
        "    if isinstance(values, list):\n        values.clear()\n",
    );
    exact("op_mut110_generator_closed", "MUT110", &closed, "MUT110");
}

// ------------------------------------------------------------------------------ MUT114's claim

/// A module with `table`, a cached function returning a list, `Frozen`, a list whose `append`
/// changes nothing, and `body` after them.
fn cached(decorator: &str, table: &str, body: &str) -> String {
    format!(
        "import functools\n\n\nclass Frozen(list):\n    def append(self, item):\n        return None\n\n\n{decorator}\n{table}\n\n\n{body}"
    )
}

const KEEPS: &str = "@functools.lru_cache(maxsize=None)";
const TABLE: &str = "def table(name) -> list[str]:\n    return [name, \"id\"]";
const APPEND: &str =
    "def use(name):\n    result = table(name)\n    result.append(\"x\")\n    return result\n";

#[test]
fn a_cache_that_keeps_nothing_leaves_the_cached_result_open() {
    let open = cached("@functools.lru_cache(maxsize=0)", TABLE, APPEND);
    let report = own_heuristic(
        "op_mut114_cached",
        "MUT114",
        &open,
        "MUT114",
        &[
            ("Cached", "cached-result-not-established"),
            ("Keyed", "cached-result-not-established"),
            ("Container kind", "cached-result-not-established"),
        ],
    );
    assert_eq!(
        closer_line_detail(&report, 0),
        ("none".to_owned(), None, Some("`app.table`".to_owned()))
    );
    exact(
        "op_mut114_cached_closed",
        "MUT114",
        &cached(KEEPS, TABLE, APPEND),
        "MUT114",
    );
    // A cached function without a key, or whose return names no container, is the same fact.
    for (name, table) in [
        (
            "op_mut114_keyed",
            "def table() -> list[str]:\n    return [\"fixed\", \"id\"]",
        ),
        (
            "op_mut114_kind",
            "def table(name):\n    return Frozen([name, \"id\"])",
        ),
    ] {
        let body = APPEND.replace(
            "table(name)",
            if name == "op_mut114_keyed" {
                "table()"
            } else {
                "table(name)"
            },
        );
        own_heuristic(
            name,
            "MUT114",
            &cached(KEEPS, table, &body),
            "MUT114",
            &[
                ("Cached", "cached-result-not-established"),
                ("Keyed", "cached-result-not-established"),
                ("Container kind", "cached-result-not-established"),
            ],
        );
    }
}

#[test]
fn a_subclass_the_annotation_admits_may_replace_the_change() {
    let frozen = "def table(name) -> list[str]:\n    return Frozen([name, \"id\"])";
    let report = own_heuristic(
        "op_mut114_subclass",
        "MUT114",
        &cached(KEEPS, frozen, APPEND),
        "MUT114",
        &[("Changed in place", "replaced-by-a-subclass")],
    );
    assert_eq!(closer_line_detail(&report, 0).0, "code");
    exact(
        "op_mut114_subclass_closed",
        "MUT114",
        &cached(KEEPS, TABLE, APPEND),
        "MUT114",
    );
}

#[test]
fn a_value_that_is_the_result_on_some_paths_only_is_not_the_same_object() {
    let open = APPEND
        .replace("def use(name):", "def use(name, flag):")
        .replace(
            "result = table(name)",
            "result = table(name) if not flag else []",
        );
    own_heuristic(
        "op_mut114_branch",
        "MUT114",
        &cached(KEEPS, TABLE, &open),
        "MUT114",
        &[("Same object", "not-the-result-on-every-path")],
    );
    exact(
        "op_mut114_branch_closed",
        "MUT114",
        &cached(KEEPS, TABLE, APPEND),
        "MUT114",
    );
}

#[test]
fn a_result_returned_through_a_wrapper_is_not_established_the_same_object() {
    let open = format!(
        "def wrapper(name):\n    return table(name)\n\n\n{}",
        APPEND.replace("result = table(name)", "result = wrapper(name)")
    );
    let report = own_heuristic(
        "op_mut114_wrapper",
        "MUT114",
        &cached(KEEPS, TABLE, &open),
        "MUT114",
        &[("Same object", "returned-through-a-function")],
    );
    assert_eq!(
        closer_line_detail(&report, 0).2,
        Some("`app.wrapper`".to_owned())
    );
    exact(
        "op_mut114_wrapper_closed",
        "MUT114",
        &cached(KEEPS, TABLE, APPEND),
        "MUT114",
    );
}

#[test]
fn a_change_that_is_not_one_of_the_kind_at_the_line() {
    let helper = format!(
        "def mutate(xs):\n    xs.append(\"x\")\n\n\n{}",
        APPEND.replace("    result.append(\"x\")\n", "    mutate(result)\n")
    );
    own_heuristic(
        "op_mut114_helper",
        "MUT114",
        &cached(KEEPS, TABLE, &helper),
        "MUT114",
        &[("Changed in place", "changed-in-a-called-function")],
    );
    own_heuristic(
        "op_mut114_add",
        "MUT114",
        &cached(KEEPS, TABLE, &APPEND.replace("result.append", "result.add")),
        "MUT114",
        &[("Changed in place", "not-a-change-of-the-kind")],
    );
    // An attribute store, on the result or on an item of it.
    let attribute = APPEND.replace("    result.append(\"x\")\n", "    result.tag = \"x\"\n");
    own_heuristic(
        "op_mut114_attribute",
        "MUT114",
        &cached(KEEPS, TABLE, &attribute),
        "MUT114",
        &[("Changed in place", "not-a-change-of-the-kind")],
    );
    own_heuristic(
        "op_mut114_item",
        "MUT114",
        &cached(
            KEEPS,
            TABLE,
            &attribute.replace("result.tag", "result[0].tag"),
        ),
        "MUT114",
        &[
            ("Same object", "not-the-result-on-every-path"),
            ("Changed in place", "not-a-change-of-the-kind"),
        ],
    );
    exact(
        "op_mut114_change_closed",
        "MUT114",
        &cached(KEEPS, TABLE, APPEND),
        "MUT114",
    );
}

#[test]
fn each_premise_of_a_cached_change_left_open_is_recorded() {
    let open = "def mutate(xs):\n    xs.append(\"x\")\n\n\ndef use(name, flag):\n    result = table(name) if not flag else []\n    mutate(result)\n    return result\n";
    own_heuristic(
        "op_mut114_several",
        "MUT114",
        &cached("@functools.lru_cache(maxsize=0)", TABLE, open),
        "MUT114",
        &[
            ("Same object", "not-the-result-on-every-path"),
            ("Cached", "cached-result-not-established"),
            ("Keyed", "cached-result-not-established"),
            ("Container kind", "cached-result-not-established"),
            ("Changed in place", "changed-in-a-called-function"),
        ],
    );
    exact(
        "op_mut114_several_closed",
        "MUT114",
        &cached(KEEPS, TABLE, APPEND),
        "MUT114",
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

/// A file `per-file-ignores` keeps a rule away from gets none of its reports, open premises
/// included; the other files' reports keep theirs.
#[test]
fn a_file_the_rule_is_ignored_in_gets_no_report() {
    let p = Project::new("open_per_file_ignores");
    let source = "import time\n\n\ndef helper(flag):\n    if flag:\n        return None\n    time.sleep(1)\n\n\nasync def handler(flag):\n    helper(flag)\n";
    p.write("app.py", source)
        .write("quiet.py", source)
        .write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nper-file-ignores = { \"quiet.py\" = [\"EFF111\"] }\n",
        );
    let (code, stdout, stderr) = p.heki(&["dump-open-premises", "--select", EFF101, "."]);
    assert_eq!(code, 0, "{stderr}");
    let out: Value = serde_json::from_str(&stdout).unwrap();
    let reports = out["reports"].as_array().unwrap();
    let paths: Vec<&str> = reports
        .iter()
        .map(|r| r["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, vec!["app.py"], "{stdout}");
    assert_eq!(
        open_of(&reports[0]),
        vec![(
            "Every return".to_owned(),
            "not-made-on-every-return".to_owned()
        )]
    );
}

/// A configuration heki cannot load (an unknown rule) prints no report and exits 2, as
/// `heki check` does.
#[test]
fn an_unknown_rule_is_a_configuration_error() {
    let p = Project::new("open_unknown_rule");
    p.write("app.py", "import time\n");
    let (code, stdout, stderr) = p.heki(&["dump-open-premises", "--select", "NOTARULE", "."]);
    assert_eq!(code, 2, "{stdout}{stderr}");
    assert!(
        stderr.contains("unknown rule selector `NOTARULE`"),
        "{stderr}"
    );
    assert!(stdout.is_empty(), "{stdout}");
}

// ------------------------------------------------------ conditions of the tier that are no premise

/// `(condition, cause, closer, detail)` of each open condition of `report`.
fn conditions_of(report: &Value) -> Vec<(String, String, String, Option<String>)> {
    report["open_conditions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| {
            assert_eq!(o["axis"], "profitability");
            (
                o["condition"].as_str().unwrap().to_owned(),
                o["cause"].as_str().unwrap().to_owned(),
                o["closer"].as_str().unwrap().to_owned(),
                o["detail"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

/// A proven size-dependent rewrite of each rule, in a loop over `ITER`, in a function taking
/// the parameters `SIG` (`trip_source`).
const TRIP_SHAPES: [(&str, &str); 6] = [
    (
        "OPT103",
        "def build(SIG):\n    x = ()\n    for i in ITER:\n        x = x + (i,)\n    return x\n",
    ),
    (
        "OPT110",
        "def count(SIG):\n    allowed = list(range(0, 50, 3))\n    hits = []\n    for p in ITER:\n        if int(p) in allowed:\n            hits.append(p)\n    return hits\n",
    ),
    (
        "OPT111",
        "def dedup(SIG):\n    seen = []\n    for v in ITER:\n        w = int(v) % 7\n        if w not in seen:\n            seen.append(w)\n    return seen\n",
    ),
    (
        "OPT120",
        "def drain(SIG):\n    q = [0, 1, 2, *items]\n    out = []\n    for _ in ITER:\n        out.append(q.pop(0))\n    return out\n",
    ),
    (
        "OPT121",
        "def front(SIG):\n    q = []\n    for i in ITER:\n        q.insert(0, i)\n    return list(q)\n",
    ),
    (
        "OPT130",
        "def rebuild(SIG):\n    keys = [1, 2, 3, 5, 8, 13, 21, 34, 55, 89, 144, 233, 377, 610, 987, 1597, 2584]\n    hits = 0\n    for p in ITER:\n        if p in set(keys):\n            hits += 1\n    return hits\n",
    ),
];

/// A module with the function of `shape` looping over `iterable`: a method of a class when
/// `method`, after a module-level list of 20 items (`STAGES`), a list the module grows
/// (`HOOKS`), and a project function (`stages`).
fn trip_source(shape: &str, iterable: &str, method: bool) -> String {
    let sig = "items, path, cursor: sqlite3.Cursor, config, obj, *args";
    let mut function = shape.replace("ITER", iterable).replace("SIG", sig);
    if method {
        function = function.replacen("(items", "(self, items", 1);
        let indented: Vec<String> = function
            .lines()
            .map(|l| {
                if l.is_empty() {
                    String::new()
                } else {
                    format!("    {l}")
                }
            })
            .collect();
        function = format!("class Model:\n{}\n", indented.join("\n"));
    }
    let stages: Vec<String> = (0..20).map(|i| i.to_string()).collect();
    format!(
        "import inspect\nimport os\nimport sqlite3\n\nSTAGES = [{}]\nHOOKS = []\n\n\ndef hook(f):\n    HOOKS.append(f)\n    return f\n\n\ndef stages(config):\n    return config.stages\n\n\n{function}",
        stages.join(", ")
    )
}

/// A size-dependent rewrite heki proves in a loop whose trip count the code fixes is heuristic
/// with every premise of its claim established: the record names the condition of the exact tier
/// that is no premise ("Trip count open"). The same rewrite over a loop the data sizes is exact.
#[test]
fn a_rewrite_in_a_loop_the_code_counts_leaves_the_trip_count_open() {
    for (rule, shape) in TRIP_SHAPES {
        let name = rule.to_lowercase();
        let fixed = reports(
            &format!("cond_{name}"),
            OPT,
            &trip_source(shape, "range(40)", false),
        );
        assert_eq!(fixed.len(), 1, "{rule}: {fixed:#?}");
        assert_eq!(fixed[0]["rule"], rule);
        assert_eq!(fixed[0]["certainty"], "heuristic", "{rule}");
        assert!(open_of(&fixed[0]).is_empty(), "{rule}: {:#?}", fixed[0]);
        assert_eq!(
            conditions_of(&fixed[0]),
            vec![(
                "Trip count open".to_owned(),
                "trip-count-fixed".to_owned(),
                "none".to_owned(),
                Some("the loop runs at most 40 times".to_owned())
            )],
            "{rule}"
        );
        exact(
            &format!("cond_{name}_closed"),
            OPT,
            &trip_source(shape, "items", false),
            rule,
        );
    }
}

/// The exact tier of a size-dependent rewrite asks for a loop whose items the data decides, as
/// `EFF103`'s "Origin" establishes them: a parameter (`*args` included), the lines of a file, the
/// rows of a query, the entries of a directory.
#[test]
fn a_rewrite_over_a_loop_the_data_sizes_is_exact() {
    let data = [
        "items",
        "args",
        "open(path)",
        "cursor.execute(\"SELECT id FROM t\")",
        "os.listdir(path)",
    ];
    for (rule, shape) in TRIP_SHAPES {
        for (i, iterable) in data.iter().enumerate() {
            let name = format!("cond_data_{}_{i}", rule.to_lowercase());
            exact(&name, OPT, &trip_source(shape, iterable, false), rule);
        }
    }
}

/// A loop over the program's structure runs as many times as it has parts (layers, stages, the
/// classes of an MRO), however large the input: a proven rewrite over it is heuristic with every
/// premise established, and records "Trip count open" with the cause of "Origin" that says why
/// its items are not the data's.
#[test]
fn a_rewrite_over_the_program_structure_leaves_the_trip_count_open() {
    let structure = [
        ("self.layers", true, "items-not-from-the-data", "none"),
        ("stages(config)", false, "items-not-from-the-data", "none"),
        (
            "range(config.num_layers)",
            false,
            "no-items-followed",
            "none",
        ),
        ("type(obj).__mro__", false, "no-items-followed", "none"),
        ("STAGES", false, "no-items-followed", "none"),
        ("HOOKS", false, "no-items-followed", "none"),
        (
            "inspect.getmro(type(obj))",
            false,
            "library-call-without-items",
            "heki-catalog",
        ),
    ];
    for (rule, shape) in TRIP_SHAPES {
        for (i, (iterable, method, cause, closer)) in structure.iter().enumerate() {
            let name = format!("cond_structure_{}_{i}", rule.to_lowercase());
            let found = reports(&name, OPT, &trip_source(shape, iterable, *method));
            assert_eq!(found.len(), 1, "{rule} {iterable}: {found:#?}");
            assert_eq!(found[0]["rule"], rule);
            assert_eq!(found[0]["certainty"], "heuristic", "{rule} {iterable}");
            assert!(open_of(&found[0]).is_empty(), "{rule} {iterable}");
            assert_eq!(
                conditions_of(&found[0]),
                vec![(
                    "Trip count open".to_owned(),
                    (*cause).to_owned(),
                    (*closer).to_owned(),
                    None
                )],
                "{rule} {iterable}"
            );
        }
    }
}

/// A `while` loop steps through no items "Origin" follows: front removals that drain a queue are
/// heuristic, and the same removals in a loop over a parameter are exact.
#[test]
fn front_removals_in_a_while_loop_leave_the_trip_count_open() {
    let open = "def drain(items):\n    q = list(items)\n    out = []\n    while q:\n        out.append(q.pop(0))\n    return out\n";
    let found = reports("cond_while_opt120", OPT, open);
    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(found[0]["rule"], "OPT120");
    assert_eq!(found[0]["certainty"], "heuristic");
    assert!(open_of(&found[0]).is_empty(), "{:#?}", found[0]);
    assert_eq!(
        conditions_of(&found[0]),
        vec![(
            "Trip count open".to_owned(),
            "no-items-followed".to_owned(),
            "none".to_owned(),
            None
        )]
    );
    let closed = open.replace("    while q:\n", "    for _ in items:\n");
    exact("cond_while_opt120_closed", OPT, &closed, "OPT120");
}

/// A join over loops whose trip count a length test caps (above the 16 under which heki reports
/// none) is heuristic with every premise established, at the loop the test caps.
#[test]
fn a_join_over_a_capped_loop_leaves_the_trip_count_open() {
    let closed = join(TYPED, "rights", MATCH);
    let open = closed.replace(
        "    out = []\n",
        "    if len(lefts) > 1000:\n        raise ValueError(lefts)\n    out = []\n",
    );
    let reports = reports("cond_opt113", "OPT113", &open);
    assert_eq!(reports.len(), 1, "{reports:#?}");
    assert_eq!(reports[0]["certainty"], "heuristic");
    assert!(open_of(&reports[0]).is_empty(), "{:#?}", reports[0]);
    assert_eq!(
        conditions_of(&reports[0]),
        vec![(
            "Trip count open".to_owned(),
            "trip-count-fixed".to_owned(),
            "none".to_owned(),
            None
        )]
    );
    let at = &reports[0]["open_conditions"][0]["at"];
    assert_eq!(
        at["line"],
        open.lines()
            .position(|l| l.contains("for a in lefts"))
            .unwrap()
            + 1
    );
    exact("cond_opt113_closed", "OPT113", &closed, "OPT113");
}
