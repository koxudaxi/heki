//! The cost of the analyses past the IR, end to end through the real binary: `isinstance`
//! narrowing through phis, the passes of the type inference and of the classes of merged values,
//! the size of the Soundness lane's proof state, and the values `EFF107` follows from a task
//! factory.

mod common;

use common::Project;
use serde_json::Value;
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `heki check --output-format json` of the project, which must finish within `limit`.
fn check_within(p: &Project, limit: Duration) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_heki"))
        .args(["check", "--output-format", "json", "."])
        .current_dir(&p.dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Read while it runs: output larger than the pipe would otherwise block it.
    let mut stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        stdout.read_to_end(&mut out).unwrap();
        out
    });
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if started.elapsed() > limit {
            child.kill().unwrap();
            panic!("heki check ran past {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert!(matches!(status.code(), Some(0 | 1)), "{status:?}");
    serde_json::from_slice(&reader.join().unwrap()).unwrap()
}

/// A loop over a worklist that tests its item against `checks` classes, each test followed by
/// `continue`, and narrows another value in an inner `while ... and ...` loop.
fn narrowing_loop(checks: usize) -> String {
    let mut lines = vec![
        "import ast".to_owned(),
        String::new(),
        String::new(),
        "def scan(pending, current, tail, proven):".to_owned(),
        "    while pending:".to_owned(),
    ];
    for i in 0..checks {
        lines.push(format!("        if isinstance(current, ast.C{i}):"));
        lines.push("            continue".to_owned());
    }
    lines.extend(
        [
            "        if isinstance(current, ast.UnaryOp):",
            "            if proven:",
            "                while isinstance(tail, ast.UnaryOp) and isinstance(tail.op, ast.Not):",
            "                    tail = tail.operand",
            "                continue",
            "            continue",
        ]
        .map(str::to_owned),
    );
    lines.join("\n") + "\n"
}

/// Each `continue` makes the loop header's phis an input of themselves: the narrowing a
/// branch on such a phi implies is found once per entered block, not once per path.
#[test]
fn narrowing_through_a_loop_with_many_continues_takes_linear_time() {
    for checks in [4, 32] {
        let p = Project::new(&format!("bounds_narrowing_{checks}"));
        p.write("m.py", &narrowing_loop(checks));
        check_within(&p, Duration::from_secs(20));
    }
}

/// A function accumulating a list, after `lines` assignments that each branch on a condition
/// and bind a new name: its blocks and variables both grow with `lines`.
fn accumulator_after(lines: usize) -> String {
    let mut out = vec!["def accumulate(items, flag, f, g):".to_owned()];
    for i in 0..lines {
        out.push(format!("    v{i} = f({i}) if flag else g({i})"));
    }
    out.push("    acc = []".to_owned());
    out.push("    for i in items:".to_owned());
    out.push("        acc = acc + [i]".to_owned());
    out.push("    return acc".to_owned());
    out.join("\n") + "\n"
}

/// The proof state holds a fact per variable per block: a function whose state would pass its
/// budget gets no `OPT` finding, and a small one keeps its finding.
#[test]
fn a_function_past_its_proof_state_budget_gets_no_finding() {
    let opt = |lines: usize| -> usize {
        let p = Project::new(&format!("bounds_state_{lines}"));
        p.write("m.py", &accumulator_after(lines));
        check_within(&p, Duration::from_secs(20))
            .iter()
            .filter(|d| d["rule_code"] == "OPT101")
            .count()
    };
    assert_eq!(opt(2), 1);
    assert_eq!(opt(200), 0);
}

/// An `async def` that calls a blocking method of a local instance after a loop passing a
/// value back through `links` assignments, one link per pass of the type inference.
fn chain_before_blocking_call(links: usize) -> String {
    let mut out = vec![
        "import time".to_owned(),
        String::new(),
        String::new(),
        "class Slow:".to_owned(),
        "    def wait(self):".to_owned(),
        "        time.sleep(1)".to_owned(),
        String::new(),
        String::new(),
        "async def chain(items):".to_owned(),
        "    s = Slow()".to_owned(),
    ];
    for k in 0..=links {
        out.push(format!("    v{k} = None"));
    }
    out.push("    for i in items:".to_owned());
    for k in 0..links {
        out.push(format!("        v{k} = v{}", k + 1));
    }
    out.push(format!("        v{links} = 1"));
    out.push("    s.wait()".to_owned());
    out.push("    return v0".to_owned());
    out.join("\n") + "\n"
}

/// The type inference takes a bounded number of passes: past them it keeps no type of the
/// function, so the receiver of `s.wait()` is unknown and nothing is reported.
#[test]
fn types_past_the_inference_pass_bound_are_unknown() {
    let blocking = |links: usize| -> usize {
        let p = Project::new(&format!("bounds_passes_{links}"));
        p.write("m.py", &chain_before_blocking_call(links));
        check_within(&p, Duration::from_secs(20))
            .iter()
            .filter(|d| d["rule_code"] == "EFF101")
            .count()
    };
    assert_eq!(blocking(4), 1);
    assert_eq!(blocking(30), 0);
}

/// An `async def` handing a buffer it keeps in a loop to a reader that downloads only what has
/// no `read` method, after the loop passes a value back through `links` assignments, one link
/// per pass of the fixpoint of the values' classes.
fn chain_before_buffer_read(links: usize) -> String {
    let mut out = vec![
        "from io import BytesIO".to_owned(),
        "from urllib.request import urlopen".to_owned(),
        String::new(),
        String::new(),
        "def fetch(src):".to_owned(),
        "    if hasattr(src, \"read\"):".to_owned(),
        "        return src.read()".to_owned(),
        "    return urlopen(src).read()".to_owned(),
        String::new(),
        String::new(),
        "async def chain(items):".to_owned(),
        "    buf = BytesIO()".to_owned(),
    ];
    for k in 0..=links {
        out.push(format!("    v{k} = None"));
    }
    out.push("    for i in items:".to_owned());
    out.push("        if i:".to_owned());
    out.push("            buf = BytesIO()".to_owned());
    for k in 0..links {
        out.push(format!("        v{k} = v{}", k + 1));
    }
    out.push(format!("        v{links} = 1"));
    out.push("    fetch(buf)".to_owned());
    out.push("    return v0".to_owned());
    out.join("\n") + "\n"
}

/// The classes of the values a function merges take a bounded number of passes: past them no
/// merged value has a class, so the buffer the loop keeps no longer decides the reader's test.
#[test]
fn classes_past_the_pass_bound_are_unknown() {
    let blocking = |links: usize| -> usize {
        let p = Project::new(&format!("bounds_classes_{links}"));
        p.write("m.py", &chain_before_buffer_read(links));
        check_within(&p, Duration::from_secs(20))
            .iter()
            .filter(|d| d["rule_code"] == "EFF101")
            .count()
    };
    assert_eq!(blocking(4), 0);
    assert_eq!(blocking(12), 1);
}

/// A loop after a `return` has only its back edge: the names it passes around through copies
/// keep their header phi, so no walk along copies runs in a cycle.
#[test]
fn copies_around_a_loop_no_path_enters_form_no_cycle() {
    let src = "def f(x):\n    return\n    while True:\n        y = x\n        x = y\n        if y:\n            y()\n";
    let p = Project::new("bounds_dead_copies");
    p.write("m.py", src);
    let (code, stdout, stderr) = p.heki(&["dump-ir", "m.py"]);
    assert_eq!(code, 0, "{stderr}");
    let ir: Value = serde_json::from_str(&stdout).unwrap();
    let f = ir["functions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["qualname"] == "f")
        .unwrap();
    let copies: std::collections::HashMap<u64, u64> = f["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|b| b["insts"].as_array().unwrap())
        .filter_map(|i| {
            let c = i["kind"].get("Copy")?;
            Some((c["dst"].as_u64()?, c["src"].as_u64()?))
        })
        .collect();
    assert!(!copies.is_empty());
    for &start in copies.keys() {
        let mut cur = start;
        for _ in 0..=copies.len() {
            match copies.get(&cur) {
                Some(&src) => cur = src,
                None => break,
            }
        }
        assert!(
            !copies.contains_key(&cur),
            "a cycle of copies through {start}: {copies:?}"
        );
    }
    check_within(&p, Duration::from_secs(20));
}

/// Comparisons whose two operands are one value (`b1 = b0 == b0`), under an invalidation
/// contract: whether a branch depends on a flag, and the flag values on each edge, are decided
/// once per value instead of once per path to it.
#[test]
fn comparisons_sharing_operands_decide_each_value_once() {
    let mut lines = vec![
        "class Model:".to_owned(),
        "    def __init__(self):".to_owned(),
        "        self.value = 0".to_owned(),
        String::new(),
        String::new(),
        "def notify():".to_owned(),
        "    pass".to_owned(),
        String::new(),
        String::new(),
        "def chain(model: Model, z):".to_owned(),
        "    model.value = 1".to_owned(),
        "    b0 = True".to_owned(),
    ];
    for i in 1..=30 {
        lines.push(format!("    b{i} = b{} == b{}", i - 1, i - 1));
    }
    lines.push("    if b30:".to_owned());
    lines.push("        notify()".to_owned());
    let p = Project::new("bounds_shared_comparisons");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.invalidation]]\nname = \"m\"\ntargets = [\"Model.value\"]\ninvalidators = [\"m.notify\"]\n",
    )
    .write("m.py", &(lines.join("\n") + "\n"));
    check_within(&p, Duration::from_secs(20));
}

/// Every case of each `match` slices the list the one before made: the objects a loop's
/// iterable may hold are collected once per variable instead of once per path to it.
#[test]
fn slices_of_slices_collect_each_origin_once() {
    let mut lines = vec![
        "import httpx".to_owned(),
        String::new(),
        String::new(),
        "def sizes(p):".to_owned(),
        "    x = [1, 2]".to_owned(),
    ];
    for _ in 0..10 {
        lines.push("    match p:".to_owned());
        for c in 0..6 {
            lines.push(format!("        case {c}:"));
            lines.push(format!("            x = x[{c}:]"));
        }
        lines.push("        case _:".to_owned());
        lines.push("            x = x[:1]".to_owned());
    }
    lines.push("    for _ in x:".to_owned());
    lines.push("        httpx.Client()".to_owned());
    let p = Project::new("bounds_slice_origins");
    p.write("m.py", &(lines.join("\n") + "\n"));
    // Two items at most: the client is made a fixed number of times.
    assert!(check_within(&p, Duration::from_secs(20)).is_empty());
}

/// Subscripts whose object and index are one value (`t1 = t0[t0]`) in a loop: whether the
/// receiver of `t30.index(x)` outlives an iteration is decided once per variable.
#[test]
fn subscripts_sharing_operands_decide_each_lifetime_once() {
    let mut lines = vec![
        "def scan(items, s, x):".to_owned(),
        "    for i in items:".to_owned(),
        "        t0 = s[0]".to_owned(),
    ];
    for i in 1..=30 {
        lines.push(format!("        t{i} = t{}[t{}]", i - 1, i - 1));
    }
    lines.push("        t30.index(x)".to_owned());
    // An object made in the iteration does not outlive it, whatever its index.
    lines.extend(
        [
            "",
            "",
            "def fresh(items, make, x):",
            "    for i in items:",
            "        t0 = make(i)",
            "        t1 = t0[t0]",
            "        t1.index(x)",
        ]
        .map(str::to_owned),
    );
    let p = Project::new("bounds_shared_subscripts");
    p.write("m.py", &(lines.join("\n") + "\n"));
    check_within(&p, Duration::from_secs(20));
}

/// Eight `match` statements of sixteen cases that pass a value through unchanged: whether a
/// branch on it is exact on every path is decided once per value and depth, not once per
/// path through the cases.
#[test]
fn a_branch_on_a_value_many_cases_pass_through_is_decided_once_per_depth() {
    let mut lines = vec!["def decide(p, items):".to_owned(), "    x = 0".to_owned()];
    for _ in 0..8 {
        lines.push("    match p:".to_owned());
        for c in 0..16 {
            lines.push(format!("        case {c}:"));
            // The comment keeps the function's proof state within its budget.
            lines.push(format!(
                "            # {}",
                "the value passes through unchanged ".repeat(5)
            ));
            lines.push("            pass".to_owned());
        }
        lines.push("        case _:".to_owned());
        lines.push("            x = 's'".to_owned());
    }
    lines.extend(
        [
            "    acc = []",
            "    for i in items:",
            "        if x:",
            "            acc = acc + [i]",
            "    return acc",
        ]
        .map(str::to_owned),
    );
    let p = Project::new("bounds_exact_paths");
    p.write("m.py", &(lines.join("\n") + "\n"));
    let found = check_within(&p, Duration::from_secs(20));
    assert_eq!(
        found.iter().filter(|d| d["rule_code"] == "OPT101").count(),
        1,
        "{found:#?}"
    );
}

/// Operators whose two operands are one value (`t1 = t0 + t0`), thirty deep, in a loop that
/// reallocates the result: whether each operand is computed entry by entry from the previous
/// value, and so whether `DATA101` reads the previous value through it, is decided once per
/// value.
#[test]
fn operators_sharing_operands_decide_each_derivation_once() {
    let mut lines = vec!["import numpy as np".to_owned()];
    for (name, first, last) in [
        ("rebuilt", "acc[1:]", "(t30, row)"),
        ("grown", "[acc]", "t30"),
    ] {
        lines.extend(["", ""].map(str::to_owned));
        lines.push(format!("def {name}(rows):"));
        lines.push("    acc = np.empty(0)".to_owned());
        lines.push("    for row in rows:".to_owned());
        lines.push(format!("        t0 = {first}"));
        for i in 1..=30 {
            lines.push(format!("        t{i} = t{} + t{}", i - 1, i - 1));
        }
        lines.push(format!("        acc = np.concatenate({last})"));
        lines.push("    return acc".to_owned());
    }
    let p = Project::new("bounds_shared_operators");
    p.write("m.py", &(lines.join("\n") + "\n"));
    let found = check_within(&p, Duration::from_secs(20));
    // Arithmetic on a slice of the previous array builds a new one; a list doubled thirty times
    // holds the previous array.
    let reported: Vec<&str> = found
        .iter()
        .filter(|d| d["rule_code"] == "DATA101")
        .map(|d| d["qualname"].as_str().unwrap())
        .collect();
    assert_eq!(reported, vec!["grown"], "{found:#?}");
}

/// An item of a display taken at a constant index far past its end (`[acc][10**12]`): the
/// display's own length bounds the look for starred elements before the index.
#[test]
fn an_index_past_a_display_is_read_without_counting_up_to_it() {
    let p = Project::new("bounds_display_index");
    p.write(
        "m.py",
        "import numpy as np\n\n\ndef grown(rows):\n    acc = np.empty(0)\n    for row in rows:\n        acc = np.concatenate(([acc][1000000000000] + [row],))\n    return acc\n",
    );
    let found = check_within(&p, Duration::from_secs(20));
    let reported: Vec<&str> = found
        .iter()
        .filter(|d| d["rule_code"] == "DATA101")
        .map(|d| d["qualname"].as_str().unwrap())
        .collect();
    assert_eq!(reported, vec!["grown"], "{found:#?}");
}

/// A function of `loops` loops one after another, each with its own first-iteration flag.
fn flagged_loops(loops: usize) -> String {
    let mut lines = vec![
        "import requests".to_owned(),
        String::new(),
        String::new(),
        "def scan(urls):".to_owned(),
    ];
    for _ in 0..loops {
        lines.extend(
            [
                "    first = True",
                "    for u in urls:",
                "        if first:",
                "            requests.get(u)",
                "            first = False",
            ]
            .map(str::to_owned),
        );
    }
    lines.join("\n") + "\n"
}

/// Each lazy initialization keeps the blocks it covers, a cell per block of the function: the
/// cells a function's lazy initializations take are bounded by its source size, and past the
/// bound none of them is recognized, so every loop's call is reported again.
#[test]
fn lazy_initializations_past_their_budget_are_not_recognized() {
    for (loops, expected) in [(4, 0), (600, 600)] {
        let p = Project::new(&format!("bounds_flags_{loops}"));
        p.write("m.py", &flagged_loops(loops));
        let found = check_within(&p, Duration::from_secs(20));
        let eff102 = found.iter().filter(|d| d["rule_code"] == "EFF102").count();
        assert_eq!(eff102, expected, "{loops} loops");
    }
}

/// A coroutine waiting `waits` times on one future, then awaiting its completion as many times,
/// then waiting once more.
fn waits_then_awaits(waits: usize) -> String {
    let mut lines = vec![
        "import asyncio".to_owned(),
        "from concurrent.futures import Future".to_owned(),
        String::new(),
        String::new(),
        "async def drain(f: Future):".to_owned(),
    ];
    lines.extend((0..waits).map(|_| "    f.result()".to_owned()));
    lines.extend((0..waits).map(|_| "    await asyncio.wrap_future(f)".to_owned()));
    lines.push("    return f.result()".to_owned());
    lines.join("\n") + "\n"
}

/// Each wait on a future looks up once whether an `await` before it in its block saw the future
/// finish: only the waits before the first `await` block (reported as one root cause past ten).
#[test]
fn waits_on_a_future_look_up_the_awaits_before_them_once() {
    for waits in [4, 400] {
        let p = Project::new(&format!("bounds_waits_{waits}"));
        p.write("m.py", &waits_then_awaits(waits));
        let found = check_within(&p, Duration::from_secs(20));
        let lines: Vec<u64> = found
            .iter()
            .filter(|d| d["rule_code"] == "EFF101")
            .map(|d| d["line"].as_u64().unwrap())
            .collect();
        let expected: Vec<u64> = if waits > 10 {
            vec![6]
        } else {
            (6..6 + waits as u64).collect()
        };
        assert_eq!(lines, expected, "{waits} waits");
        if waits > 10 {
            let more = format!("(+{} more async call sites", waits - 1);
            assert!(found[0]["message"].as_str().unwrap().contains(&more));
        }
    }
}

/// Task factories at the end of a long chain of copies of one coroutine (`EFF107`): what each
/// call's argument is, and the values its task reaches, are indexed once per function instead of
/// searched once per call.
#[test]
fn task_factories_at_the_end_of_a_copy_chain_take_linear_time() {
    let (copies, calls) = (4000, 400);
    let mut lines = vec![
        "import asyncio".to_owned(),
        String::new(),
        String::new(),
        "async def work():".to_owned(),
        "    pass".to_owned(),
        String::new(),
        String::new(),
        "async def spawn(background):".to_owned(),
        "    v0 = work()".to_owned(),
    ];
    lines.extend((1..=copies).map(|i| format!("    v{i} = v{}", i - 1)));
    lines.extend((0..calls).map(|j| format!("    t{j} = asyncio.create_task(v{copies})")));
    // Every task but the first is kept, so that one report shows the rule ran.
    lines.extend((1..calls).map(|j| format!("    background.add(t{j})")));
    let p = Project::new("bounds_task_copies");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"EFF107\"]\n",
    )
    .write("m.py", &(lines.join("\n") + "\n"));
    let found = check_within(&p, Duration::from_secs(20));
    let tasks: Vec<&Value> = found
        .iter()
        .filter(|d| d["rule_code"] == "EFF107")
        .collect();
    assert_eq!(tasks.len(), 1, "{tasks:?}");
    assert_eq!(tasks[0]["target"], "asyncio.create_task->t0");
}

/// A shared value copied along a long chain of assignments (`v1 = v0`, `v2 = v1`, ...) before
/// `MUT202` reads which object each copy is: each copy's origin is found once.
#[test]
fn a_long_copy_chain_of_a_shared_value_finds_each_origin_once() {
    let mut lines = vec![
        "from functools import lru_cache".to_owned(),
        String::new(),
        String::new(),
        "@lru_cache(maxsize=None)".to_owned(),
        "def rows(name):".to_owned(),
        "    return [[name]]".to_owned(),
        String::new(),
        String::new(),
        "def extend(name):".to_owned(),
        "    v0 = rows(name)".to_owned(),
    ];
    for i in 1..=4000 {
        lines.push(format!("    v{i} = v{}", i - 1));
    }
    lines.push("    v4000.append([name])".to_owned());
    let p = Project::new("bounds_shared_copy_chain");
    p.write("m.py", &(lines.join("\n") + "\n"));
    let found = check_within(&p, Duration::from_secs(20));
    let reported: Vec<&str> = found
        .iter()
        .filter(|d| d["rule_code"] == "MUT202")
        .map(|d| d["qualname"].as_str().unwrap())
        .collect();
    assert_eq!(reported, vec!["extend"], "{found:#?}");
}
