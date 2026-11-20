//! What the execution-effect and complexity advisories (`EFF103`, `EFF106`, `EFF107`,
//! `DATA101`, `OPT112`, `OPT113`) take as a query, a held lock, a task factory, a quadratic
//! rebuild, and a lookup that a precomputed index can replace (end to end, through the real
//! binary).

mod common;

use common::Project;
use serde_json::Value;

/// `(line, message)` of every diagnostic of `rule`.
fn reports(p: &Project, rule: &str) -> Vec<(u64, String)> {
    let (code, stdout, stderr) = p.check(&["--select", rule, "--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    diags
        .iter()
        .map(|d| {
            assert_eq!(d["rule_code"], rule, "{d:#}");
            (
                d["line"].as_u64().unwrap(),
                d["message"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
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
