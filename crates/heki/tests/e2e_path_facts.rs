//! The path and loop facts of `heki_ir::facts`, end to end through `heki dump-facts`: whether a
//! call runs whenever its function returns, over the edges into its handlers; whether it runs on
//! every iteration of a loop that completes; branches the module's constants decide; and the
//! trip counts the code fixes, past 16 too.

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
    // A caught hop may raise before it reaches what it calls.
    expect("caught_hop", "db.get_setting(\"k\")", "every", "not_every");
    expect("caught_hop", "db.fetch(raw)", "every", "every");
    // A handler of an earlier `try` returns without the call.
    expect("earlier_try", "t.join(cwd)", "not_every", "not_every");
    // The receiver's call runs before `fetchall` in the same statement.
    expect(
        "receiver_first",
        "conn.execute(\"q\")",
        "every",
        "not_every",
    );
    expect(
        "receiver_first",
        "conn.execute(\"q\").fetchall()",
        "not_every",
        "not_every",
    );
    // A `with` whose manager heki does not resolve may suppress what its body raises.
    expect("in_with", "fh.read()", "every", "not_every");
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
    expect("json.loads(row)", "every", "not_every");
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
        ]
    );
}
