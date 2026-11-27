//! Expressions as deep as their source, end to end through the real binary under a 1 MiB stack
//! for every thread: chains of ten thousand operators, unions of ten thousand classes, and calls
//! and subscripts nested as deep are parsed, lowered, analyzed, and dropped as a short chain is,
//! with the same report, on the main thread (a single file) and on the worker threads (the files
//! of a project).

mod common;

use common::Project;
use serde_json::Value;
use std::process::Command;

/// The stack every thread of `heki` gets: the main thread's on Windows, and less than the 2 MiB
/// worker threads have by default.
const STACK: usize = 1024 * 1024;

/// How deep the expressions of [`deep_module`] are.
const DEEP: usize = 10_000;

/// A module of async functions each running a blocking call on an expression `n` deep (a chain
/// of `n` operands of `+`, calls nested `n` deep, a chain of `n` subscripts, subscripts nested `n`
/// deep), one calling a blocking method of a parameter annotated with a union of `n` classes,
/// and a module-level union of `n` classes. Each expression is on one line, so a report on it is
/// at the same place whatever `n`.
fn deep_module(n: usize) -> String {
    let union = vec!["C"; n].join(" | ");
    let chain = vec!["a"; n].join(" + ");
    let calls = format!("{}a{}", "f(".repeat(n), ")".repeat(n));
    let subscripts = format!("a{}", "[0]".repeat(n));
    let nested = format!("{}0{}", "a[".repeat(n), "]".repeat(n));
    format!(
        "import subprocess
import time


class C:
    def run(self):
        time.sleep(1)


def f(a):
    return a


U = {union}


async def chain(a):
    subprocess.run({chain})


async def calls(a):
    subprocess.run({calls})


async def subscripts(a):
    subprocess.run({subscripts})


async def nested(a):
    subprocess.run({nested})


async def union(x: {union}):
    x.run()
"
    )
}

/// `heki check --output-format json` of the project `p` with a stack of [`STACK`] for every
/// thread: `ulimit -s` sets the main thread's where a shell runs it (a Windows program's main
/// thread has that much), and `RUST_MIN_STACK` the worker threads'.
fn check_on_small_stacks(p: &Project) -> Vec<Value> {
    let heki = env!("CARGO_BIN_EXE_heki");
    let args = ["check", "--output-format", "json", "."];
    let mut command = if cfg!(windows) {
        let mut command = Command::new(heki);
        command.args(args);
        command
    } else {
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!("ulimit -s {} && exec \"$0\" \"$@\"", STACK / 1024))
            .arg(heki)
            .args(args);
        command
    };
    let out = command
        .env_remove("GITHUB_ACTIONS")
        .env_remove("HEKI_OUTPUT_FORMAT")
        .env("RUST_MIN_STACK", STACK.to_string())
        .current_dir(&p.dir)
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stderr}");
    serde_json::from_slice(&out.stdout).unwrap()
}

/// What a report says and where, without the spans it covers, which grow with the expressions.
fn places(found: &[Value]) -> Vec<String> {
    let mut places: Vec<String> = found
        .iter()
        .map(|d| {
            format!(
                "{}:{}:{} {} {} {}",
                d["path"], d["line"], d["column"], d["rule_code"], d["qualname"], d["message"]
            )
        })
        .collect();
    places.sort();
    places
}

/// The reports on the deep module and on the same module three deep, in a project of the
/// module alone, analyzed on the main thread.
#[test]
fn deep_expressions_of_a_single_file_are_reported_as_short_ones() {
    let report = |n: usize| {
        let p = Project::new(&format!("deep_expressions_single_{n}"));
        p.write("m.py", &deep_module(n));
        places(&check_on_small_stacks(&p))
    };
    let short = report(3);
    assert_eq!(short.len(), 5, "{short:#?}");
    assert_eq!(report(DEEP), short);
}

/// The same in a project of two such modules, analyzed on the worker threads.
#[test]
fn deep_expressions_of_a_project_are_reported_as_short_ones() {
    let report = |n: usize| {
        let p = Project::new(&format!("deep_expressions_project_{n}"));
        p.write("a.py", &deep_module(n));
        p.write("b.py", &deep_module(n));
        places(&check_on_small_stacks(&p))
    };
    let short = report(3);
    assert_eq!(short.len(), 10, "{short:#?}");
    assert_eq!(report(DEEP), short);
}
