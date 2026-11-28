//! A call of a function a caller hands over, end to end through the real binary: a helper that
//! calls its parameter, directly, in a nested function it calls, through a choice of parameters,
//! or by handing it on, runs the function the call binds there (EFF101, effect boundaries), and
//! an effect boundary judges the parameter's call once every followed call binds it to known
//! functions.

mod common;

use common::Project;
use serde_json::Value;

const MODEL: &str = r#"class Field:
    def __init__(self) -> None:
        self.required = False


class Marker:
    def mark(self, field: Field) -> None:
        field.required = True
"#;

const HELPERS: &str = r#"from collections.abc import Callable

from pkg.model import Field

REGISTRY = []


def each(fields: list[Field], visit: Callable[[Field], None]) -> None:
    for field in fields:
        visit(field)


async def each_async(fields: list[Field], visit) -> None:
    for field in fields:
        await visit(field)


def run_all(fields: list[Field], visit) -> None:
    each(fields, visit)


def nested(fields: list[Field], fn) -> None:
    def one(field: Field) -> None:
        fn(field)

    for field in fields:
        one(field)


def mapped(fields: list[Field], fn) -> None:
    def one(field: Field) -> None:
        fn(field)

    list(map(one, fields))


def deep(fields: list[Field], fn) -> None:
    def middle() -> None:
        def inner(field: Field) -> None:
            fn(field)

        for field in fields:
            inner(field)

    middle()


def chosen(fields: list[Field], key, node_of=None) -> None:
    fn = key if node_of is None else node_of
    for field in fields:
        fn(field)


def keep(fn):
    def one(field: Field) -> None:
        fn(field)

    return one


def stores(fn) -> None:
    REGISTRY.append(fn)


def rebinds(fields: list[Field], visit) -> None:
    visit = REGISTRY[0]
    visit(fields[0])


def later(fn):
    yield fn()


def generated(fields: list[Field], fn) -> None:
    later(fn)


def drain(fn) -> None:
    for _ in fn():
        pass


def start(fn):
    return fn()


def mark(field: Field, *, deep: bool = False) -> None:
    field.required = True


def marks(field: Field):
    field.required = True
    yield field


async def mark_async(field: Field) -> None:
    field.required = True
"#;

const PASSES: &str = r#"from functools import partial

from pkg.helpers import (
    chosen, deep, drain, each, each_async, generated, keep, mapped, mark, mark_async, marks,
    nested, rebinds, run_all, start, stores,
)
from pkg.model import Field, Marker


def run_name(fields: list[Field]) -> None:
    each(fields, mark)


def run_partial(fields: list[Field]) -> None:
    each(fields, partial(mark, deep=True))


def run_bound(fields: list[Field], marker: Marker) -> None:
    each(fields, marker.mark)


def run_hand_on(fields: list[Field]) -> None:
    run_all(fields, mark)


def run_nested(fields: list[Field]) -> None:
    nested(fields, mark)


def run_mapped(fields: list[Field]) -> None:
    mapped(fields, mark)


def run_deep(fields: list[Field]) -> None:
    deep(fields, mark)


def run_choice(fields: list[Field]) -> None:
    chosen(fields, mark)


async def run_awaited(fields: list[Field]) -> None:
    await each_async(fields, mark_async)


def run_own(fields: list[Field], visit) -> None:
    each(fields, visit)


def run_unpacked(fields: list[Field], fns: list) -> None:
    each(fields, *fns)


def run_escaping(fields: list[Field]) -> None:
    keep(mark)(fields[0])


def run_rebound(fields: list[Field]) -> None:
    rebinds(fields, mark)


def run_stored(fields: list[Field]) -> None:
    stores(mark)


def run_generator(fields: list[Field]) -> None:
    generated(fields, mark)


def run_drained(fields: list[Field]) -> None:
    drain(partial(marks, fields[0]))


async def run_started(fields: list[Field]) -> None:
    await start(partial(mark_async, fields[0]))
"#;

const BLOCKING: &str = r#"import asyncio

import requests


def fetch() -> int:
    return requests.get("https://example.com").status_code


def fetch_url(url: str) -> int:
    return requests.get(url).status_code


def maybe(fn, enabled: bool):
    if enabled:
        return fn()
    return None


def lazy(fn, urls):
    def one(url):
        return fn(url)

    return map(one, urls)


def retry(fn, times: int = 3):
    for _ in range(times):
        try:
            return fn()
        except OSError:
            continue
    raise RuntimeError


def call(fn):
    return fn()


def register(fn) -> None:
    fn.registered = True


async def through_retry() -> int:
    return retry(fetch)


async def through_call() -> int:
    return call(fetch)


async def through_lambda() -> int:
    return call(lambda: fetch())


async def through_register() -> None:
    register(fetch)


async def through_maybe(enabled: bool) -> None:
    maybe(fetch, enabled)


async def through_lazy(urls: list[str]) -> list[int]:
    return list(lazy(fetch_url, urls))


async def through_thread() -> int:
    return await asyncio.to_thread(call, fetch)


async def through_executor() -> int:
    loop = asyncio.get_running_loop()
    return await loop.run_in_executor(None, call, fetch)
"#;

fn project(name: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        r#"[project]
name = "t"

[tool.heki.lint]
select = ["EFF101", "EFF201"]

[[tool.heki.effect-boundaries]]
name = "no-field-writes"
scopes = ["pkg.passes:run_*"]
write-universe = ["Field.required"]
unresolved = "report"
"#,
    )
    .write("pkg/__init__.py", "")
    .write("pkg/model.py", MODEL)
    .write("pkg/helpers.py", HELPERS)
    .write("pkg/passes.py", PASSES)
    .write("pkg/blocking.py", BLOCKING);
    p
}

/// `(rule, function, what)` of every report, sorted: a boundary's write or unresolved call, or
/// the function an `EFF101` report's caller runs, with the report's certainty.
fn found(p: &Project) -> Vec<(String, String, String)> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json", "pkg"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<_> = (diags.iter())
        .map(|d| {
            let message = d["message"].as_str().unwrap();
            let what = if message.contains("cannot resolve") {
                let called = message.split('`').nth(3).unwrap_or_default();
                format!("unresolved {called}")
            } else if message.contains("writes `Field.required`") {
                "write".to_owned()
            } else {
                let hop = d["related"][0]["message"].as_str().unwrap();
                let ran = hop.split('`').nth(3).unwrap_or_default();
                format!("{ran}, {}", d["certainty"].as_str().unwrap())
            };
            let rule = d["rule_code"].as_str().unwrap().to_owned();
            (rule, d["qualname"].as_str().unwrap().to_owned(), what)
        })
        .collect();
    out.sort();
    out
}

#[test]
fn a_function_handed_to_a_helper_that_calls_it_runs_there() {
    let p = project("handed_callables");
    let r = |rule: &str, q: &str, what: &str| (rule.to_owned(), q.to_owned(), what.to_owned());
    let unresolved = |q: &str, called: &str| r("EFF201", q, &format!("unresolved {called}"));
    // Whether and on which paths a helper calls its parameter is the helper's: what it runs is
    // reached, never run on every path of the caller.
    let expected = vec![
        r("EFF101", "through_call", "pkg.blocking.fetch, heuristic"),
        r(
            "EFF101",
            "through_lambda",
            "pkg.blocking.through_lambda.<locals>.<lambda>, heuristic",
        ),
        r(
            "EFF101",
            "through_lazy",
            "pkg.blocking.fetch_url, heuristic",
        ),
        r("EFF101", "through_maybe", "pkg.blocking.fetch, heuristic"),
        r("EFF101", "through_retry", "pkg.blocking.fetch, heuristic"),
        r("EFF201", "run_awaited", "write"),
        r("EFF201", "run_bound", "write"),
        r("EFF201", "run_choice", "write"),
        r("EFF201", "run_deep", "write"),
        // A generator function's body runs where what it returns is iterated, and a coroutine's
        // where it is awaited: the helper calling them runs neither, so the call stays unresolved.
        unresolved("run_drained", "fn"),
        // The nested function runs away from the function binding its cell: nothing binds it.
        unresolved("run_escaping", "fn"),
        r("EFF201", "run_hand_on", "write"),
        r("EFF201", "run_mapped", "write"),
        r("EFF201", "run_name", "write"),
        r("EFF201", "run_nested", "write"),
        // The scope's own parameter: its callers are outside the reach.
        unresolved("run_own", "visit"),
        r("EFF201", "run_partial", "write"),
        // The parameter is rebound before the call: what the call passes is not called.
        unresolved("run_rebound", "visit"),
        unresolved("run_started", "fn"),
        // `*` unpacking hides which value binds the parameter.
        unresolved("run_unpacked", "visit"),
        // Nothing for `run_stored` (the helper keeps the function), `run_generator` (a
        // generator's body does not run at the call handing it the function),
        // `through_register` (the helper does not call its parameter), or `through_thread` and
        // `through_executor` (the helper runs on a worker thread, and so does what it calls).
    ];
    assert_eq!(found(&p), expected);
}

/// The helpers of [`HELPERS`] and their callers in another module, with the helper changed in
/// place: the callers' reports follow what the helper does with its parameter.
#[test]
fn a_helper_that_stops_calling_its_parameter_runs_nothing_it_is_handed() {
    let p = project("handed_callables_stops");
    p.write(
        "pkg/helpers.py",
        &HELPERS.replace(
            "    for field in fields:\n        visit(field)\n\n\nasync",
            "    REGISTRY.append(visit)\n\n\nasync",
        ),
    );
    let calls_each = |(_, q, _): &(String, String, String)| {
        [
            "run_name",
            "run_partial",
            "run_bound",
            "run_hand_on",
            "run_own",
            "run_unpacked",
        ]
        .contains(&q.as_str())
    };
    assert_eq!(found(&p).iter().filter(|f| calls_each(f)).count(), 0);
}
