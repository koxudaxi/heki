//! The path and loop facts of `heki_ir::facts`, end to end through `heki dump-facts`: whether a
//! call runs whenever its function returns, over the edges into its handlers; whether it runs on
//! every iteration of a loop that completes; branches the module's constants decide; the trip
//! counts the code fixes, past 16 too; and where the items a loop steps through come from.

mod common;

use common::Project;
use serde_json::{Value, json};

/// `heki dump-facts` of the single module `source`.
fn dump_facts(name: &str, source: &str) -> Value {
    let p = Project::new(name);
    p.write("m.py", source);
    let (code, stdout, stderr) = p.heki(&["dump-facts", "m.py"]);
    assert_eq!(code, 0, "{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

fn function<'a>(facts: &'a Value, qualname: &str) -> &'a Value {
    facts["functions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["qualname"] == qualname)
        .unwrap_or_else(|| panic!("no function `{qualname}`"))
}

/// The facts of the call whose text is `text` in function `qualname`.
fn call<'a>(facts: &'a Value, qualname: &str, text: &str) -> &'a Value {
    function(facts, qualname)["calls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["text"] == text)
        .unwrap_or_else(|| panic!("no call `{text}` in `{qualname}`"))
}

fn known(must: &str) -> Value {
    json!({ "known": must })
}

#[test]
fn a_call_runs_whenever_its_function_returns_over_the_edges_into_handlers() {
    let facts = dump_facts(
        "paths_on_return",
        r#"import subprocess


def own_handler(cmd):
    try:
        r = subprocess.run(["git", cmd], timeout=10)
    except FileNotFoundError:
        return None
    return r


def caught_hop(db):
    try:
        raw = db.get_setting("k")
    except Exception:
        raw = {}
    return db.fetch(raw)


def earlier_try(path, t):
    try:
        cwd = path.cwd()
    except OSError:
        return None
    t.join(cwd)


def receiver_first(conn):
    try:
        rows = conn.execute("q").fetchall()
    except ValueError:
        return []
    return rows


def in_with(open_, p, sub):
    with open_(p) as fh:
        data = fh.read()
        sub.run(data)
    return 1
"#,
    );
    let expect = |qualname: &str, text: &str, called: &str, returned: &str| {
        let c = call(&facts, qualname, text);
        assert_eq!(
            c["on_every_return"],
            json!({ "called": known(called), "returned": known(returned) }),
            "{qualname}: {text}"
        );
    };
    // The call's own exception still means it was called.
    expect(
        "own_handler",
        "subprocess.run([\"git\", cmd], timeout=10)",
        "every",
        "not_every",
    );
    // Looking up a method of an object of unknown class may raise before anything is called.
    expect(
        "caught_hop",
        "db.get_setting(\"k\")",
        "not_every",
        "not_every",
    );
    expect("caught_hop", "db.fetch(raw)", "every", "every");
    // A handler of an earlier `try` returns without the call.
    expect("earlier_try", "t.join(cwd)", "not_every", "not_every");
    // The receiver's call runs before `fetchall` in the same statement.
    expect(
        "receiver_first",
        "conn.execute(\"q\")",
        "not_every",
        "not_every",
    );
    expect(
        "receiver_first",
        "conn.execute(\"q\").fetchall()",
        "not_every",
        "not_every",
    );
    // A `with` whose manager heki does not resolve may suppress what its body raises.
    expect("in_with", "fh.read()", "not_every", "not_every");
    expect("in_with", "sub.run(data)", "not_every", "not_every");
}

#[test]
fn a_call_runs_on_every_iteration_that_completes() {
    let facts = dump_facts(
        "paths_on_iteration",
        r#"def loops(rows, db, json):
    for row in rows:
        try:
            v = json.loads(row)
        except ValueError:
            continue
        db.execute(v)
    for row in rows:
        db.first(row)
        if row:
            break
        db.before_break(row)
    for row in rows:
        db.leaves(row)
        break
"#,
    );
    let iteration = |text: &str| call(&facts, "loops", text)["on_every_iteration"][0].clone();
    let expect = |text: &str, called: &str, returned: &str| {
        let it = iteration(text);
        assert_eq!(
            (it["called"].clone(), it["returned"].clone()),
            (known(called), known(returned)),
            "{text}"
        );
    };
    expect("json.loads(row)", "not_every", "not_every");
    // The handler goes on to the next item without the call.
    expect("db.execute(v)", "not_every", "not_every");
    expect("db.first(row)", "every", "every");
    // An iteration that breaks does not complete.
    expect("db.before_break(row)", "every", "every");
    // No iteration completes: the call runs at most once per run of the loop.
    expect("db.leaves(row)", "never_completes", "never_completes");
}

#[test]
fn code_under_a_branch_the_module_constants_decide_is_dead() {
    let facts = dump_facts(
        "paths_dead_code",
        r#"from typing import TYPE_CHECKING

_IS_SYNC = False


def connect(sock):
    if not _IS_SYNC:
        sock.recv()
    else:
        sock.send()
    if TYPE_CHECKING:
        sock.close()
    sock.flush()
"#,
    );
    let called = |text: &str| call(&facts, "connect", text)["on_every_return"]["called"].clone();
    assert_eq!(called("sock.recv()"), known("every"));
    assert_eq!(called("sock.send()"), known("dead"));
    assert_eq!(called("sock.close()"), known("dead"));
    assert_eq!(called("sock.flush()"), known("every"));
}

#[test]
fn trip_counts_the_code_fixes_past_sixteen() {
    let facts = dump_facts(
        "paths_trip_counts",
        r#"NAMES = ("a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q")
ALPHABET = "abcdefghijklmnopqrstuvwxyz"


def counts(rows, db):
    for row in rows:
        db.one(row)
    for i in range(100):
        db.one(i)
    for row in rows[:50]:
        db.one(row)
    for row, i in zip(rows, range(40)):
        db.one(row)
    for name in NAMES:
        db.one(name)
    if len(rows) > 30:
        raise ValueError(rows)
    for row in rows:
        db.one(row)
    n = 0
    while n < 1000:
        n += 1
    for i in range(3):
        db.one(i)
    for i in (1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20):
        db.one(i)
    for c in ALPHABET:
        db.one(c)
    k = 0
    while 500 > k:
        k += 1
"#,
    );
    let trips: Vec<Value> = function(&facts, "counts")["loops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["fixed_trips"].clone())
        .collect();
    assert_eq!(
        trips,
        vec![
            json!("open"),
            json!({"known": 100}),
            json!({"known": 50}),
            json!({"known": 40}),
            json!({"known": 17}),
            json!({"known": 30}),
            json!({"known": 1000}),
            json!({"known": 3}),
            json!({"known": 20}),
            json!({"known": 26}),
            // The counter on the right of its test.
            json!({"known": 500}),
        ]
    );
}

#[test]
fn the_items_a_loop_steps_through_come_from_named_sources() {
    let facts = dump_facts(
        "paths_origins",
        r#"_IS_SYNC = False


class Store:
    def load(self, rows, conn, path, json):
        for row in sorted(rows):
            pass
        for row in conn.execute("SELECT 1"):
            pass
        for line in path.read_text().splitlines():
            pass
        for x in map(str, rows):
            pass
        for part in "a;b".split(";"):
            pass
        for x in json.loads(rows):
            pass
        for x in self.rows:
            pass
        kept = [r for r in rows if r]
        for x in kept:
            pass
        for x in (conn if _IS_SYNC else rows):
            pass
        for row in rows:
            for cell in row:
                pass
        for row in conn.fetchall():
            for cell in row:
                pass
        while rows:
            rows = rows[1:]
        for batch in [rows]:
            for row in batch:
                pass
        total = sum(len(x) for x in conn.query())
        for key in {row[0] for row in rows}:
            pass
"#,
    );
    let origins: Vec<Value> = function(&facts, "Store.load")["loops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|l| l["origin"].clone())
        .collect();
    let one = |source: &str| json!({"known": [{"source": source, "carries": "items"}]});
    assert_eq!(
        origins,
        vec![
            one("param 1"),
            one("call conn.execute(\"SELECT 1\")"),
            json!({"known": [{"source": "call path.read_text()", "carries": "items", "text": true}]}),
            json!({"known": [{"source": "param 1", "carries": "count"}]}),
            one("fixed"),
            // The reader resolves a call: `json.loads` is no catalog source.
            one("call json.loads(rows)"),
            one("receiver"),
            // The comprehension's own loop, then the loop over the list it fills.
            one("param 1"),
            one("param 1"),
            one("param 1"),
            // An inner loop comes before the loop around it.
            one("part of param 1"),
            one("param 1"),
            json!("open"),
            one("call conn.fetchall()"),
            json!("open"),
            // An item of a display is the element itself.
            one("param 1"),
            one("fixed"),
            // The set comprehension's own loop, then the loop over the set: equal keys merge,
            // so the count no longer follows the rows.
            one("param 1"),
            json!("open"),
        ]
    );
    // A generator expression's loop reads the iterable its creator passes.
    let genexpr = function(&facts, "Store.load.<locals>.<genexpr>");
    assert_eq!(
        genexpr["loops"][0]["origin"],
        json!({"known": [{"source": "call conn.query() of Store.load", "carries": "items"}]})
    );
}
