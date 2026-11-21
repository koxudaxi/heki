//! What the execution-effect and complexity advisories (`EFF103`, `EFF106`, `EFF107`,
//! `DATA101`, `OPT112`, `OPT113`) take as a query, a held lock, a task factory, a quadratic
//! rebuild, and a lookup that a precomputed index can replace (end to end, through the real
//! binary).

mod common;

use common::Project;
use serde_json::Value;

/// `(line, message)` of every diagnostic of `rule`.
/// `(line, field)` of every `rule` report, `field` being `message` or `help`.
fn reports_with(p: &Project, rule: &str, field: &str) -> Vec<(u64, String)> {
    let (code, stdout, stderr) = p.check(&["--select", rule, "--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    diags
        .iter()
        .map(|d| {
            assert_eq!(d["rule_code"], rule, "{d:#}");
            (
                d["line"].as_u64().unwrap(),
                d[field].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn reports(p: &Project, rule: &str) -> Vec<(u64, String)> {
    reports_with(p, rule, "message")
}

fn helps(p: &Project, rule: &str) -> Vec<(u64, String)> {
    reports_with(p, rule, "help")
}

fn lines(p: &Project, rule: &str) -> Vec<u64> {
    reports(p, rule).into_iter().map(|(line, _)| line).collect()
}

/// The lines of the reports of a database query in a loop, established (`EFF103`) or not
/// (`EFF113`).
fn query_lines(p: &Project) -> Vec<u64> {
    let mut out = lines(p, "EFF103");
    out.extend(lines(p, "EFF113"));
    out.sort_unstable();
    out
}

#[test]
fn ddl_is_recognized_where_the_statement_runs() {
    let p = Project::new("ddl_at_execution");
    p.write(
        "app.py",
        r#"import sqlite3

import redis


def keys(client: redis.Redis, items: list[str]) -> None:
    for item in items:
        client.get("created_at")


def refresh(cur: sqlite3.Cursor) -> None:
    cur.execute("CREATE TABLE IF NOT EXISTS t (x)")


def fetch(cur: sqlite3.Cursor) -> None:
    cur.execute("SELECT x FROM t")


def through_helpers(cur: sqlite3.Cursor, items: list[str]) -> None:
    for item in items:
        refresh(cur)
        fetch(cur)


def direct(cur: sqlite3.Cursor, items: list[str]) -> None:
    for item in items:
        cur.execute("pragma foreign_keys = ON")
        cur.execute("CREATED_AT")


async def handler(cur: sqlite3.Cursor) -> None:
    refresh(cur)
"#,
    );
    // A Redis key is no statement; a helper running only DDL runs no query; a DDL statement
    // still blocks the event loop (on a cursor only an annotation names, which may be in
    // memory: not established).
    assert_eq!(query_lines(&p), vec![8, 22, 28]);
    assert_eq!(lines(&p, "EFF111"), vec![32]);
}

const DDL_AT_RUN_TIME: &str = r#"import sqlite3

import sqlalchemy
from sqlalchemy import text
from sqlalchemy.engine import Connection

ADDITIVE = (
    "CREATE TABLE IF NOT EXISTS a (x)",
    """CREATE INDEX a_x ON a(x)""",
)
SCHEMA = """
CREATE TABLE refs (name TEXT);
CREATE INDEX refs_name ON refs(name);
PRAGMA user_version=5
"""
CURRENT = SCHEMA
CURRENT_ADDITIVE = ADDITIVE
LEGACY = "CREATE TABLE b (y)\r\nCREATE TABLE c (z)"
SEED = ("CREATE TABLE d (x)", "INSERT INTO d VALUES (1)")
SEED_LINES = "CREATE TABLE f (x)\rINSERT INTO f VALUES (1)\n"


def drop(conn: sqlite3.Connection, table: str) -> None:
    conn.execute(f"DROP TABLE IF EXISTS {table}")


def migrate(conn: Connection, columns: list[tuple[str, str]]) -> None:
    for table, column in columns:
        conn.execute(text(f"ALTER TABLE {table} ADD COLUMN {column}"))
        conn.execute(sqlalchemy.text("ALTER TABLE " f"{table} ADD COLUMN {column}"))
        conn.execute(text(f"CREATE INDEX IF NOT EXISTS i ON t(x)"))


def formatted(conn: sqlite3.Connection, tables: list[str]) -> None:
    for t in tables:
        conn.execute("DROP TABLE %s" % t)
        conn.execute("DROP VIEW {}".format(t))
        drop(conn, t)


def per_database(paths: list[str], legacy: bool, fail) -> None:
    for path in paths:
        conn = sqlite3.connect(path)
        for statement in ADDITIVE:
            conn.execute(statement)
        for statement in CURRENT.split(";"):
            conn.execute(statement.strip())
        schema = LEGACY if legacy else SCHEMA
        for line in schema.splitlines():
            conn.execute(line)
        statements = ("CREATE TABLE e (x)", "CREATE TABLE g (x)")
        again = statements
        for statement in again:
            conn.execute(statement)
        named = CURRENT_ADDITIVE
        for statement in named:
            conn.execute(statement)
        if legacy:
            chosen = LEGACY
        elif path:
            chosen = SCHEMA
        else:
            fail(path)
        for line in chosen.splitlines():
            conn.execute(line)


def queries(conn: sqlite3.Connection, ids: list[int], kind: str, verb: str, q, build, make) -> None:
    sql = "CREATE TABLE h (x)"
    for i in ids:
        conn.execute(f"SELECT * FROM t WHERE id = {i}")  # reported
        conn.execute(f"CREATE{kind} x")  # reported
        conn.execute(f"{verb} TABLE x")  # reported
        conn.execute("COMMIT")  # reported
        conn.execute(q.sql)  # reported
        conn.execute(build(i))  # reported
        conn.execute(sql)  # reported
        sql = sql + ", y"
        for statement in SEED:
            conn.execute(statement)  # reported
        statements = ["CREATE TABLE e (x)"]
        statements.append("SELECT 1")
        for statement in statements:
            conn.execute(statement)  # reported
        for statement in (*ADDITIVE, "SELECT 1"):
            conn.execute(statement)  # reported
        for line in SEED_LINES.splitlines():
            conn.execute(line)  # reported
        for word in SCHEMA.split():
            conn.execute(word)  # reported
        for piece in f"CREATE TABLE {kind} (x); SELECT 1".split(";"):
            conn.execute(piece)  # reported
        for statement in make():
            conn.execute(statement)  # reported
        deep = "CREATE TABLE t (x" DEEP
        conn.execute(deep)  # reported
"#;

#[test]
fn ddl_built_at_run_time_or_taken_from_constants_is_ddl() {
    let p = Project::new("ddl_built_at_run_time");
    // A statement built from more parts than the walk follows is not followed.
    let source = DDL_AT_RUN_TIME.replace("DEEP", &" + kind".repeat(70));
    p.write("app.py", &source);
    // An f-string (implicitly concatenated, or without a field), `%`, or `.format()` starting
    // with a DDL keyword, SQLAlchemy's `text()` of one, a helper running one, and the
    // statements of module constants (a tuple's items, the pieces of a split schema, either of
    // two schemas, through names, also where a path leaves the name unbound) are DDL. Reported: a query, a head whose first word may go
    // on or is a field, an unknown or extended statement, a constant tuple with a query, a list
    // that grew, a starred display, a query on a line of its own (`\r` ends a line, as for
    // `str.splitlines()`), a split into words, a split of a head, items of a call, and a
    // statement built from 70 parts.
    let reported: Vec<u64> = (1..)
        .zip(source.lines())
        .filter(|(_, line)| line.ends_with("# reported"))
        .map(|(n, _)| n)
        .collect();
    assert_eq!(query_lines(&p), reported);
}

const SCHEMA_OWNER: &str = r#"import sqlite3

SQL = "CREATE TABLE t (x)"
STATEMENTS = ("CREATE TABLE u (x)",)


def setup(conn: sqlite3.Connection, rows: list[int]) -> None:
    for row in rows:
        conn.execute(SQL)
        for statement in STATEMENTS:
            conn.execute(statement)
"#;

#[test]
fn a_constant_another_module_rebinds_is_no_fixed_statement() {
    let p = Project::new("ddl_rebound_elsewhere");
    p.write("pkg/__init__.py", "");
    p.write("pkg/db.py", SCHEMA_OWNER);
    // Reading the constants elsewhere leaves them DDL.
    p.write(
        "pkg/other.py",
        "from pkg import db\n\n\ndef peek():\n    return db.SQL, db.STATEMENTS\n",
    );
    assert_eq!(query_lines(&p), Vec::<u64>::new());
    // Another module rebinding them may make them queries.
    p.write(
        "pkg/other.py",
        "from pkg import db\n\n\ndef reconfigure():\n    db.SQL = \"SELECT 1\"\n    db.STATEMENTS = (\"SELECT 1\",)\n",
    );
    assert_eq!(query_lines(&p), vec![9, 11]);
}

#[test]
fn a_reentrant_lock_stays_held_after_a_nested_with_exits() {
    let p = Project::new("reentrant_lock");
    p.write(
        "app.py",
        r#"import asyncio
import threading


async def nested() -> None:
    lock = threading.RLock()
    with lock:
        with lock:
            pass
        await asyncio.sleep(0)


async def one_after_another() -> None:
    lock = threading.RLock()
    with lock:
        pass
    with lock:
        pass
    await asyncio.sleep(0)


async def both_exited() -> None:
    lock = threading.RLock()
    with lock:
        with lock:
            pass
    await asyncio.sleep(0)
"#,
    );
    let found = reports(&p, "EFF106");
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].0, 10);
    assert!(found[0].1.contains("taken at line 7"), "{found:?}");
}

#[test]
fn task_factories_are_found_under_the_names_they_are_imported_as() {
    let p = Project::new("task_factory_aliases");
    p.write(
        "app.py",
        r#"import asyncio as aio
from asyncio import create_task as spawn
from asyncio import ensure_future as later


async def work() -> None: ...


def create_task(coro):
    return coro


async def aliased() -> None:
    task = spawn(work())


async def module_alias() -> None:
    task = aio.create_task(work())


async def future_alias() -> None:
    fut = later(work())


async def own_function() -> None:
    task = create_task(work())
"#,
    );
    assert_eq!(lines(&p, "EFF107"), vec![14, 18, 22]);
}

#[test]
fn a_list_a_helper_changes_in_the_loop_needs_an_index_kept_up_to_date() {
    let p = Project::new("opt112_helper_changes");
    p.write(
        "app.py",
        r#"def change(xs):
    xs.reverse()


def look(xs):
    return len(xs)


def positions(names: list[str], winners: list[str]) -> list[int]:
    out = []
    for winner in winners:
        change(names)
        out.append(names.index(winner))
    return out


def counts(names: list[str], winners: list[str]) -> list[int]:
    out = []
    for winner in winners:
        look(names)
        out.append(names.count(winner))
    return out


class Shuffler:
    def change(self, xs):
        xs.reverse()


def narrowed(names: list[str], winners: list[str], s) -> list[int]:
    out = []
    for winner in winners:
        if isinstance(s, Shuffler):
            s.change(names)
        out.append(names.index(winner))
    return out
"#,
    );
    let found = helps(&p, "OPT112");
    assert_eq!(found.len(), 3, "{found:?}");
    // A helper method of the class an `isinstance` test narrows the receiver to.
    assert_eq!(found[2].0, 35);
    assert!(
        found[2].1.ends_with("the loop changes the list, so keep a `dict` (value → index) up to date with each change"),
        "{found:?}"
    );
    assert_eq!(found[0].0, 13);
    assert!(
        found[0].1.ends_with("the loop changes the list, so keep a `dict` (value → index) up to date with each change"),
        "{found:?}"
    );
    assert_eq!(found[1].0, 21);
    assert!(
        found[1]
            .1
            .ends_with("count once before the loop with `collections.Counter(names)`"),
        "{found:?}"
    );
}

#[test]
fn a_join_using_every_match_needs_an_index_of_all_matching_elements() {
    let p = Project::new("opt113_all_matches");
    p.write(
        "app.py",
        r#"def pairs(left: list[int], right: list[int]) -> list[tuple[int, int]]:
    out = []
    for a in left:
        for b in right:
            if a == b:
                out.append((a, b))
    return out


def first(left: list[int], right: list[int]) -> list[int]:
    out = []
    for a in left:
        for b in right:
            if a == b:
                out.append(a)
                break
    return out


def by_key(orders, customers):
    for order in orders:
        for customer in customers:
            if order.customer_id == customer.id:
                order.customers.append(customer)
"#,
    );
    let advice = helps(&p, "OPT113");
    assert_eq!(
        advice,
        vec![
            (
                5,
                "group the elements of `right` by value in a `dict` (value → list of equal elements) before the outer loop, then read each outer key with `.get(key, [])` so that a key with no element keeps its empty list".to_owned()
            ),
            (14, "build a `set` of `right` before the outer loop".to_owned()),
            (
                23,
                // `order.customers.append(customer)` hands the inner item to a method heki does
                // not know, which may change its key.
                "if `customers` is the same collection, with the same keys, on every iteration of the outer loop and the keys are hashable, index `customers` in a `dict` from `customer.id` to the list of elements with that key before the outer loop, then read each outer key with `.get(key, [])` so that a key with no element keeps its empty list".to_owned()
            ),
        ]
    );
}

#[test]
fn an_accumulation_through_a_helper_needs_the_helper_to_reallocate_it() {
    let p = Project::new("data101_helpers");
    p.write(
        "app.py",
        r#"import numpy
import pandas as pd


def step(acc):
    numpy.append([0], 1)
    return acc


def grow(acc):
    return numpy.append(acc, 1)


def grow_twice(acc):
    return grow(acc)


def combine(parts):
    return pd.concat(parts)


def unchanged(items: list[int]):
    acc = numpy.array([])
    for _ in items:
        acc = step(acc)
    return acc


def through_helpers(items: list[int], frames: list[pd.DataFrame]):
    acc = numpy.array([])
    df = pd.DataFrame()
    for _ in items:
        acc = grow_twice(acc)
    for part in frames:
        df = combine([df, part])
    return acc, df


class Stepper:
    def step(self, acc):
        numpy.append([0], 1)
        return acc


def unchanged_when_narrowed(items: list[int], s):
    acc = numpy.array([])
    for _ in items:
        if isinstance(s, Stepper):
            acc = s.step(acc)
    return acc
"#,
    );
    assert_eq!(lines(&p, "DATA101"), vec![33, 35]);
}

#[test]
fn an_accumulation_through_a_method_a_module_function_or_a_merged_part_is_quadratic() {
    let p = Project::new("data101_call_forms");
    p.write("app/__init__.py", "")
        .write(
            "app/grow.py",
            r#"import numpy


def grow(acc):
    return numpy.append(acc, 1)


def window(acc, edges):
    return numpy.append(edges, 0)
"#,
        )
        .write(
            "app/acc.py",
            r#"import numpy

from app import grow as helpers


class Series:
    def _grow(self, acc):
        return numpy.append(acc, 1)

    def collect(self, items):
        acc = numpy.array([])
        for _ in items:
            acc = self._grow(acc)
        return acc


def through_module(items):
    acc = numpy.array([])
    for _ in items:
        acc = helpers.grow(acc)
    return acc


def merged_part(rows):
    acc = numpy.array([])
    for row in rows:
        if row:
            v = 1
        else:
            v = 2
        acc = numpy.append(acc, v)
    return acc


def edges_only(items, edge):
    acc = numpy.array([])
    for _ in items:
        acc = helpers.window(acc, [edge, edge])
    return acc
"#,
        );
    // `window` rebuilds the display it is handed, not the accumulator it ignores.
    assert_eq!(lines(&p, "DATA101"), vec![13, 20, 31]);
}
