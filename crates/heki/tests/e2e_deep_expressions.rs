//! Expressions as deep as their source, end to end through the real binary started with a 1 MiB
//! stack for its main thread and for the threads spawned without a size: chains of ten thousand
//! operators, unions of ten thousand classes, and calls and subscripts nested as deep are parsed,
//! lowered, analyzed, and dropped as a short chain is, with the same report, on the thread
//! running the command (a single file) and on the thread pool (the files of a project), whose
//! stacks heki sizes itself.

mod common;

use common::Project;
use common::lsp::{Lsp, uri, uri_in};
use serde_json::{Value, json};
use std::process::Command;

/// The stack `heki` starts with: the main thread's on Windows, and less than the 2 MiB threads
/// have by default.
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

/// `heki check --output-format json` of the project `p` started with a stack of [`STACK`]:
/// `ulimit -s` sets the main thread's where a shell runs it (a Windows program's main thread has
/// that much), and `RUST_MIN_STACK` that of the threads spawned without a size.
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
/// module alone, analyzed on the thread running the command.
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

/// The same in a project of two such modules, analyzed on the thread pool.
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

/// The same through the language server, started with a stack of [`STACK`] for the threads
/// spawned without a size: the module is indexed from the disk and checked once opened, on the
/// threads of the server.
#[test]
fn deep_expressions_in_the_language_server_are_reported_as_short_ones() {
    let report = |n: usize| {
        let p = Project::new(&format!("deep_expressions_server_{n}"));
        let text = deep_module(n);
        p.write("m.py", &text);
        let stack = STACK.to_string();
        let mut lsp = Lsp::start_with_env(
            &p.dir,
            &["server", "--stdio"],
            &[("RUST_MIN_STACK", stack.as_str())],
        );
        lsp.request(
            "initialize",
            json!({"processId": null, "rootUri": uri(&p.dir), "capabilities": {}}),
        );
        lsp.notify("initialized", json!({}));
        lsp.ready();
        let m = uri_in(&p.dir, "m.py");
        lsp.open(&m, &text);
        let found = lsp.diagnostics(&m, |d| !d.is_empty());
        lsp.stop(true);
        let mut places: Vec<String> = found
            .iter()
            .map(|d| {
                let start = &d["range"]["start"];
                format!(
                    "{}:{} {} {}",
                    start["line"], start["character"], d["code"], d["message"]
                )
            })
            .collect();
        places.sort();
        places
    };
    let short = report(3);
    assert_eq!(short.len(), 5, "{short:#?}");
    assert_eq!(report(DEEP), short);
}

/// A host that refuses the analysis stack (a limit on virtual memory; here a stack larger than
/// any address space, through the test hook `HEKI_ANALYSIS_STACK`): the command and the thread
/// pool run on the platform's default stack after one warning, with the same report.
#[test]
fn a_host_refusing_the_analysis_stack_runs_on_the_default_stack() {
    const WARNING: &str = "warning: this host refuses threads with a stack of 256 MiB";
    let p = Project::new("deep_expressions_refused_stack");
    p.write("a.py", &deep_module(3));
    p.write("b.py", &deep_module(3));
    let check = |stack: Option<usize>| {
        let mut command = Command::new(env!("CARGO_BIN_EXE_heki"));
        command
            .args(["check", "--output-format", "json", "."])
            .env_remove("GITHUB_ACTIONS")
            .env_remove("HEKI_OUTPUT_FORMAT")
            .current_dir(&p.dir);
        if let Some(stack) = stack {
            command.env("HEKI_ANALYSIS_STACK", stack.to_string());
        }
        let out = command.output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(out.status.code(), Some(1), "{stderr}");
        let found: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
        (places(&found), stderr.matches(WARNING).count())
    };
    let (sized, sized_warnings) = check(None);
    assert_eq!(sized.len(), 10, "{sized:#?}");
    assert_eq!(sized_warnings, 0);
    assert_eq!(check(Some(1 << 62)), (sized, 1));
}
