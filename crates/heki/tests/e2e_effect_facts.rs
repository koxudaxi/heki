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
    // still blocks the event loop.
    assert_eq!(lines(&p, "EFF103"), vec![8, 22, 28]);
    assert_eq!(lines(&p, "EFF101"), vec![32]);
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
                "index `customers` in a `dict` from `customer.id` to the list of elements with that key before the outer loop, then read each outer key with `.get(key, [])` so that a key with no element keeps its empty list".to_owned()
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
