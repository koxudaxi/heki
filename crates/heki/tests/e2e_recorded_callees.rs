//! Rules reading the project calls the summaries resolved, end to end through the real binary:
//! a call on a value an `isinstance` test narrows runs the narrowed class's method (`DATA101`,
//! `MUT114`, `MUT106`), and a callable an offloading API runs on a worker thread is a call of it
//! with the arguments after it (`acc = await asyncio.to_thread(grow, acc)`).
//!
//! Every fixture marks the lines that must be reported with the rule code in a comment
//! (`# DATA101`); every other line must not be.

mod common;

use common::Project;
use serde_json::Value;

/// `(path, line, rule)` of every diagnostic of `rules`.
fn reported(p: &Project, rules: &[&str]) -> Vec<(String, u64, String)> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json", "--extend-select", "MUT106"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<(String, u64, String)> = diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
            )
        })
        .filter(|(_, _, r)| rules.contains(&r.as_str()))
        .collect();
    out.sort();
    out
}

/// The `(path, line, rule)` marks of `files`: a line ending in `# <RULE>` for each of `rules`.
fn marked(files: &[(&str, &str)], rules: &[&str]) -> Vec<(String, u64, String)> {
    let mut out = Vec::new();
    for (path, text) in files {
        for (i, line) in text.lines().enumerate() {
            for rule in rules {
                if line.trim_end().ends_with(&format!("# {rule}")) {
                    out.push(((*path).to_owned(), i as u64 + 1, (*rule).to_owned()));
                }
            }
        }
    }
    out.sort();
    out
}

fn check_marks(name: &str, files: &[(&str, &str)], rules: &[&str]) {
    let p = Project::new(name);
    p.write("pkg/__init__.py", "");
    for (path, text) in files {
        p.write(path, text);
    }
    assert_eq!(reported(&p, rules), marked(files, rules));
}

const NARROWED: &str = r#"import functools

import numpy


class Grower:
    def grow(self, acc):
        return numpy.append(acc, 1)


class Keeper:
    def grow(self, acc):
        numpy.append([0], 1)
        return acc


class Catalog:
    @functools.cache
    def entries(self):
        return []


class Drain:
    def take(self, it):
        list(it)


def narrowed(items, g):
    acc = numpy.array([])
    for _ in items:
        if isinstance(g, Grower):
            acc = g.grow(acc)  # DATA101
    return acc


def narrowed_keeper(items, g):
    acc = numpy.array([])
    for _ in items:
        if isinstance(g, Keeper):
            acc = g.grow(acc)
    return acc


def shared(c):
    if isinstance(c, Catalog):
        c.entries().clear()  # MUT114


def consumed(lines, t):
    it = iter(lines)
    if isinstance(t, Drain):
        t.take(it)
        return list(it)  # MUT106
    return None
"#;

#[test]
fn a_call_on_a_narrowed_value_runs_the_narrowed_class_method() {
    check_marks(
        "narrowed_callees",
        &[("pkg/narrowed.py", NARROWED)],
        &["DATA101", "MUT114", "MUT106"],
    );
}

const WORKER: &str = r#"import asyncio

import numpy


def step(acc):
    numpy.append([0], 1)
    return acc


def grow(acc):
    return numpy.append(acc, 1)


async def on_worker(items):
    acc = numpy.array([])
    for _ in items:
        acc = await asyncio.to_thread(grow, acc)  # DATA101
    return acc


async def on_worker_step(items):
    acc = numpy.array([])
    for _ in items:
        acc = await asyncio.to_thread(step, acc)
    return acc
"#;

#[test]
fn a_callable_run_on_a_worker_takes_the_arguments_after_it() {
    check_marks("worker_callees", &[("pkg/worker.py", WORKER)], &["DATA101"]);
}

/// A worker's callable binds the arguments after it in the call (`MUT114` reads them so): a
/// `functools.partial` handed to an offloading API is not followed, as the arguments it holds
/// would come first.
const WORKER_PARTIAL: &str = r#"import asyncio
import functools
from functools import lru_cache


@lru_cache
def cached():
    return [1, 2]


def change_first(first, second):
    first.append(1)


async def held_list_changed():
    await asyncio.to_thread(functools.partial(change_first, []), cached())


async def list_after_the_callable():
    await asyncio.to_thread(change_first, [], cached())


async def cached_list_changed():
    await asyncio.to_thread(change_first, cached(), [])  # MUT114
"#;

#[test]
fn a_partial_run_on_a_worker_is_not_bound_to_the_arguments_after_it() {
    check_marks(
        "worker_partial",
        &[("pkg/worker_partial.py", WORKER_PARTIAL)],
        &["MUT114"],
    );
}
