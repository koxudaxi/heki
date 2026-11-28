//! The chain a blocking-call report shows (`related`) and the reports output folds into one:
//! the witness takes a way the call's arguments allow (`isinstance` dispatch on an argument of
//! known class) and the normal path before cleanup code, and reports fold only when they reach
//! the same blocking call. The reports and their tiers are the same whichever chain is shown.

mod common;

use common::Project;
use serde_json::Value;

fn diagnostics(p: &Project, codes: &str) -> Vec<Value> {
    let (code, stdout, stderr) = p.check(&["--select", codes, "--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

/// `(tiered code, the callees of the chain)` of the report of the function `qualname`.
fn chain_of(diags: &[Value], qualname: &str) -> (String, Vec<String>) {
    let d = diags
        .iter()
        .find(|d| d["qualname"] == qualname)
        .unwrap_or_else(|| panic!("no report in `{qualname}`: {diags:#?}"));
    let callees = d["related"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let message = r["message"].as_str().unwrap();
            let callee = message.rsplit(" calls ").next().unwrap();
            callee.trim_matches('`').to_owned()
        })
        .collect();
    (common::tiered_code(d).to_owned(), callees)
}

const RENDER: &str = r#"import time


class Request:
    pass


class Response:
    pass


def wait_for_prompt(item):
    time.sleep(1)


def request_segments(item):
    wait_for_prompt(item)


def response_segments(item):
    time.sleep(1)


def append(item):
    if isinstance(item, Response):
        return response_segments(item)
    return request_segments(item)


def append_request(item):
    if isinstance(item, Request):
        return request_segments(item)
    return response_segments(item)
"#;

const SERVICE: &str = r#"from pkg.render import Request, Response, append, append_request


async def compact():
    append(Request())


async def answer():
    append(Response())


def render(message: Request):
    return append(message)


async def compact_through_helper():
    render(Request())


def make() -> Request:
    return Request()


async def typed():
    append_request(make())


async def unknown(message):
    append(message)


async def unpacked(items):
    append(*items)
"#;

#[test]
fn a_witness_takes_the_branch_the_arguments_allow() {
    let p = Project::new("witness_isinstance_branch");
    p.write("pkg/__init__.py", "")
        .write("pkg/render.py", RENDER)
        .write("pkg/service.py", SERVICE);
    let diags = diagnostics(&p, "EFF101");
    let request = [
        "pkg.render.request_segments",
        "pkg.render.wait_for_prompt",
        "time.sleep",
    ];
    let response = ["pkg.render.response_segments", "time.sleep"];
    let expect = |helpers: &[&str], tail: &[&str]| -> (String, Vec<String>) {
        (
            "EFF101 [heuristic]".to_owned(),
            helpers
                .iter()
                .chain(tail)
                .map(|c| (*c).to_owned())
                .collect(),
        )
    };
    // An instance of exactly `Request` fails `isinstance(item, Response)`: the shorter response
    // branch is not the one the call takes.
    assert_eq!(
        chain_of(&diags, "compact"),
        expect(&["pkg.render.append"], &request)
    );
    assert_eq!(
        chain_of(&diags, "answer"),
        expect(&["pkg.render.append"], &response)
    );
    // A helper handing its parameter on leaves the test to its callers.
    assert_eq!(
        chain_of(&diags, "compact_through_helper"),
        expect(&["pkg.service.render", "pkg.render.append"], &request)
    );
    // A value typed `Request` passes `isinstance(item, Request)`, subclasses included.
    assert_eq!(
        chain_of(&diags, "typed"),
        expect(&["pkg.render.append_request"], &request)
    );
    // Nothing known of the argument, or no argument bound to the parameter: the shortest way.
    for qualname in ["unknown", "unpacked"] {
        assert_eq!(
            chain_of(&diags, qualname),
            expect(&["pkg.render.append"], &response)
        );
    }
}

#[test]
fn a_declaration_keeping_the_effects_drops_their_ways() {
    let p = Project::new("witness_declared_async_safe");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.ecosystem]]\nsymbol = \"pkg.render.append\"\nasync-safe = true\n",
    )
    .write("pkg/__init__.py", "")
    .write("pkg/render.py", RENDER)
    .write("pkg/service.py", SERVICE);
    let diags = diagnostics(&p, "EFF101");
    let callers: Vec<&str> = diags
        .iter()
        .filter_map(|d| d["qualname"].as_str())
        .collect();
    assert_eq!(callers, ["typed"]);
}

#[test]
fn a_test_an_abc_or_protocol_answers_is_not_decided_by_the_mro() {
    let p = Project::new("witness_isinstance_protocol");
    p.write("pkg/__init__.py", "").write(
        "pkg/render.py",
        r#"import time
from typing import Protocol, runtime_checkable


@runtime_checkable
class Renders(Protocol):
    def render(self) -> str: ...


class Request:
    def render(self) -> str:
        return ""


def slow(item):
    time.sleep(1)


def slower(item):
    slow(item)


def append(item):
    if isinstance(item, Renders):
        return slow(item)
    return slower(item)


async def compact():
    append(Request())
"#,
    );
    let diags = diagnostics(&p, "EFF101");
    // `Request` is no subclass of the protocol, yet an instance of it: the test stays open.
    assert_eq!(
        chain_of(&diags, "compact").1,
        ["pkg.render.append", "pkg.render.slow", "time.sleep"]
    );
}

const STORE: &str = r#"import os
from pathlib import Path


def write_atomic(path: Path, data: bytes) -> None:
    temporary = path.with_suffix(".tmp")
    try:
        temporary.write_bytes(data)
        with open(temporary, "rb+") as f:
            os.fsync(f.fileno())
    except OSError:
        temporary.unlink()
        raise


def write_with_finally(path: Path, data: bytes) -> None:
    temporary = path.with_suffix(".tmp")
    replaced = False
    try:
        temporary.write_bytes(data)
        temporary.replace(path)
        replaced = True
    finally:
        if not replaced:
            temporary.unlink()


def remove_on_error(path: Path) -> None:
    try:
        print(path)
    except OSError:
        path.unlink()


async def save(path: Path) -> None:
    write_atomic(path, b"x")


async def save_record(path: Path) -> None:
    write_with_finally(path, b"x")


async def report(path: Path) -> None:
    remove_on_error(path)
"#;

#[test]
fn a_witness_takes_the_normal_path_before_cleanup_code() {
    let p = Project::new("witness_cleanup");
    p.write("pkg/__init__.py", "").write("pkg/store.py", STORE);
    let diags = diagnostics(&p, "EFF105");
    let last = |qualname: &str| chain_of(&diags, qualname).1.last().cloned().unwrap();
    assert_ne!(last("save"), "pathlib.Path.unlink");
    assert_ne!(last("save_record"), "pathlib.Path.unlink");
    // Only the cleanup reaches the effect: it is the witness, and the report stays.
    assert_eq!(
        chain_of(&diags, "report"),
        (
            "EFF105".to_owned(),
            vec![
                "pkg.store.remove_on_error".to_owned(),
                "pathlib.Path.unlink".to_owned()
            ]
        )
    );
}

#[test]
fn reports_fold_only_when_they_reach_the_same_blocking_call() {
    let p = Project::new("witness_fold_keys");
    p.write("pkg/__init__.py", "").write(
        "pkg/logs.py",
        r#"import time


def log(line: str) -> None:
    time.sleep(0.1)


def save(path: str) -> None:
    time.sleep(0.1)


def pause() -> None:
    time.sleep(0.1)


def first() -> None:
    pause()


def second() -> None:
    pause()
"#,
    );
    let mut service = String::from("from pkg.logs import first, log, save, second\n");
    for i in 0..11 {
        let helper = if i % 2 == 0 { "first" } else { "second" };
        service.push_str(&format!(
            "\n\nasync def handle_{i}() -> None:\n    log('x')\n\n\nasync def store_{i}() -> None:\n    save('c.json')\n\n\nasync def wait_{i}() -> None:\n    {helper}()\n"
        ));
    }
    p.write("pkg/service.py", &service);
    let diags = diagnostics(&p, "EFF101");
    let mut shown: Vec<(String, String)> = diags
        .iter()
        .map(|d| {
            let message = d["message"].as_str().unwrap();
            let more = message.rsplit_once("(+").map_or("", |(_, m)| m);
            (d["qualname"].as_str().unwrap().to_owned(), more.to_owned())
        })
        .collect();
    shown.sort();
    // `log` and `save` each make their own call of `time.sleep`; `first` and `second` reach the
    // one call `pause` makes.
    assert_eq!(
        shown,
        [
            (
                "handle_0".to_owned(),
                "10 more async call sites that always run `time.sleep`)".to_owned()
            ),
            (
                "store_0".to_owned(),
                "10 more async call sites that always run `time.sleep`)".to_owned()
            ),
            (
                "wait_0".to_owned(),
                "10 more async call sites that always run `time.sleep`)".to_owned()
            ),
        ]
    );
}
