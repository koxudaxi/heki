//! The cost of the analyses past the IR, end to end through the real binary: `isinstance`
//! narrowing through phis, the passes of the type inference, the size of the Soundness lane's
//! proof state, the values `EFF107` follows from a task factory, and the walk to the callable a
//! builtin calls once per item.

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

/// `MUT112` selected (it reads the `is` tests of a function's branches).
const MUT112_CONFIG: &str =
    "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"MUT112\"]\n";

/// Functions whose branches all test the last of a long chain of copies of one `is` test
/// (`c3000 = c2999`, `if c3000:`): the test each copy is, through the chain, is found once per
/// value for all the branches.
#[test]
fn branches_on_a_long_copy_chain_of_an_is_test_read_it_once() {
    let links = 3_000;
    let mut lines = Vec::new();
    for k in 0..4 {
        lines.push(format!("def chain{k}(x):"));
        lines.push("    c0 = x is None".to_owned());
        for i in 1..=links {
            lines.push(format!("    c{i} = c{}", i - 1));
        }
        for _ in 0..links {
            lines.push(format!("    if c{links}:"));
            lines.push("        x = []".to_owned());
        }
        lines.push(String::new());
        lines.push(String::new());
    }
    let p = Project::new("bounds_is_test_copy_chain");
    p.write("pyproject.toml", MUT112_CONFIG)
        .write("m.py", &(lines.join("\n") + "\n"));
    check_within(&p, Duration::from_secs(20));
}

/// A function testing one value against `None` before each of many conditional expressions over
/// the last of a long chain of its copies: each phi input finds the tests it is on a side of by
/// the walk up the dominator tree from its edge, once per edge, and the value a copy copies
/// once per chain.
#[test]
fn phi_inputs_after_many_is_tests_find_their_sides_once_per_edge() {
    let tests = 600;
    let mut lines = vec!["def guards(x, flag):".to_owned(), "    a0 = x".to_owned()];
    for i in 1..=tests {
        lines.push(format!("    a{i} = a{}", i - 1));
    }
    for k in 0..tests {
        lines.push("    if x is None:".to_owned());
        lines.push("        raise ValueError(\"x is required\")".to_owned());
        lines.push(format!("    y{k} = [] if flag else a{tests}"));
    }
    lines.push(format!("    y{}.append(1)", tests - 1));
    let p = Project::new("bounds_is_test_sides");
    p.write("pyproject.toml", MUT112_CONFIG)
        .write("m.py", &(lines.join("\n") + "\n"));
    let found = check_within(&p, Duration::from_secs(20));
    let lines: Vec<u64> = found
        .iter()
        .filter(|d| d["rule_code"] == "MUT112")
        .map(|d| d["line"].as_u64().unwrap())
        .collect();
    assert_eq!(lines, vec![2 + tests as u64 + 3 * tests as u64 + 1]);
}

/// A function returning the last of a long chain of conditional expressions, each choosing
/// between a call of the previous value and the previous value itself
/// (`x200 = identity(x199) if flag else x199`): each phi is walked once and referred to by the
/// walks that meet it, instead of every argument walking all the phis before it.
#[test]
fn returned_phis_of_calls_on_the_previous_value_are_walked_once() {
    let links = 200;
    let mut lines = vec![
        "def identity(x):".to_owned(),
        "    return x".to_owned(),
        String::new(),
        String::new(),
        "def chain(x0, flag, other):".to_owned(),
    ];
    for i in 1..=links {
        let prev = i - 1;
        lines.push(format!("    x{i} = identity(x{prev}) if flag else x{prev}"));
    }
    lines.push(format!("    return x{links} if other else []"));
    lines.extend(
        [
            "",
            "",
            "def use(shared, flag, other):",
            "    x = chain(shared, flag, other)",
            "    x.append(1)",
        ]
        .map(str::to_owned),
    );
    let p = Project::new("bounds_returned_phi_chain");
    p.write("pyproject.toml", MUT112_CONFIG)
        .write("m.py", &(lines.join("\n") + "\n"));
    let found = check_within(&p, Duration::from_secs(20));
    assert_eq!(
        found
            .iter()
            .filter(|d| d["rule_code"] == "MUT112")
            .map(|d| d["qualname"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["use"]
    );
}

/// One returned call handed the last 256 of a long chain of item reads (`z2000 = z1999[0]`) as
/// its arguments: each argument walks the chain, so past the budget of walked values
/// proportional to the source what the function returns is unknown, and nothing is reported
/// from it.
#[test]
fn origin_walks_past_their_budget_return_unknown() {
    let (links, args) = (2_000, 256);
    let mut lines = vec![
        "def pick(*rows):".to_owned(),
        "    return rows[0]".to_owned(),
        String::new(),
        String::new(),
        "def deep(z0, flag):".to_owned(),
    ];
    for i in 1..=links {
        lines.push(format!("    z{i} = z{}[0]", i - 1));
    }
    let picked: Vec<String> = (links - args + 1..=links)
        .map(|i| format!("z{i}"))
        .collect();
    let picked = picked.join(", ");
    lines.push(format!("    return pick({picked}) if flag else []"));
    lines.extend(
        [
            "",
            "",
            "def use(shared, flag):",
            "    x = deep(shared, flag)",
            "    x.append(1)",
        ]
        .map(str::to_owned),
    );
    let p = Project::new("bounds_origin_walk_budget");
    p.write("pyproject.toml", MUT112_CONFIG)
        .write("m.py", &(lines.join("\n") + "\n"));
    let found = check_within(&p, Duration::from_secs(20));
    assert!(
        found.iter().all(|d| d["rule_code"] != "MUT112"),
        "{found:#?}"
    );
}

/// A loop carrying a long chain of assignments in reverse (`v0 = v1`, ..., `v1499 = shared or []`,
/// the names bound to `None` before it): the value moves one link per iteration, and the
/// ownership fixpoint follows it in one sweep along the chain, visiting what a value grows
/// into instead of every instruction again for each link.
#[test]
fn ownership_through_a_long_chain_a_loop_carries_in_reverse_is_found_in_one_sweep() {
    let links = 1_500;
    let mut lines = vec!["def carry(shared, items):".to_owned()];
    for i in 0..=links {
        lines.push(format!("    v{i} = None"));
    }
    lines.push("    for _ in items:".to_owned());
    for i in 0..links {
        lines.push(format!("        v{i} = v{}", i + 1));
    }
    lines.push(format!("        v{links} = shared or []"));
    lines.push("    v0.append(1)".to_owned());
    let p = Project::new("bounds_reverse_loop_chain");
    p.write("pyproject.toml", MUT112_CONFIG)
        .write("m.py", &(lines.join("\n") + "\n"));
    let found = check_within(&p, Duration::from_secs(20));
    assert_eq!(
        found
            .iter()
            .filter(|d| d["rule_code"] == "MUT112")
            .map(|d| d["line"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        vec![2 * links as u64 + 5]
    );
}

/// A function appending each alias of a long chain to a list (one list for all of them, or a new
/// list per alias) and returning the last alias or its parameter, and a caller changing what it
/// returns.
fn alias_chain_stores(aliases: usize, one_list: bool) -> String {
    let mut lines = vec![
        "def keep(x, shared, flag):".to_owned(),
        "    a0 = x".to_owned(),
    ];
    for i in 1..=aliases {
        lines.push(format!("    a{i} = a{}", i - 1));
        if one_list {
            lines.push(format!("    shared.append(a{i})"));
        } else {
            lines.push(format!("    t{i} = []"));
            lines.push(format!("    t{i}.append(a{i})"));
        }
    }
    // Through the list's stores, or the parameter itself.
    let returned = if one_list {
        format!("a{aliases}")
    } else {
        "x".to_owned()
    };
    lines.push(format!("    return {returned} if flag else []"));
    lines.extend(
        [
            "",
            "",
            "def use(given, shared, flag):",
            "    x = keep(given, shared, flag)",
            "    x.append(1)",
        ]
        .map(str::to_owned),
    );
    lines.join("\n") + "\n"
}

/// Each alias of a long chain appended to one list: what the function returns follows the
/// stores of every alias in one walk, each value and place once, and the caller's change of the
/// result (an element of `shared` on one path, new on the other) is reported.
#[test]
fn stores_of_a_long_alias_chain_into_one_list_keep_each_value_once() {
    let p = Project::new("bounds_alias_chain_one_list");
    p.write("pyproject.toml", MUT112_CONFIG)
        .write("m.py", &alias_chain_stores(12_000, true));
    let found = check_within(&p, Duration::from_secs(20));
    assert_eq!(
        found
            .iter()
            .filter(|d| d["rule_code"] == "MUT112")
            .map(|d| d["qualname"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["use"]
    );
}

/// Each alias of a long chain appended to a new list of its own: the pairs of value and place
/// grow with the square of the chain, so past the budget proportional to the source what the
/// function returns (its parameter, or a new list) is unknown, and nothing is reported from it.
#[test]
fn stores_of_a_long_alias_chain_into_many_lists_past_the_budget_return_unknown() {
    let p = Project::new("bounds_alias_chain_many_lists");
    p.write("pyproject.toml", MUT112_CONFIG)
        .write("m.py", &alias_chain_stores(8_000, false));
    let found = check_within(&p, Duration::from_secs(20));
    assert!(
        found.iter().all(|d| d["rule_code"] != "MUT112"),
        "{found:#?}"
    );
}

/// A function returning the last of a long chain of calls, each handed the previous call's
/// result as two of its arguments (`x40 = choose(x39, x39, flag)`), with `choose` returning
/// either: what each call returns is resolved once, not once per path down the chain.
#[test]
fn returned_calls_handed_the_previous_result_twice_resolve_each_call_once() {
    let calls = 40;
    let mut lines = vec![
        "def choose(a, b, flag):".to_owned(),
        "    return a if flag else b".to_owned(),
        String::new(),
        String::new(),
        "def chain(x0, flag):".to_owned(),
    ];
    for i in 1..=calls {
        lines.push(format!("    x{i} = choose(x{}, x{}, flag)", i - 1, i - 1));
    }
    lines.push(format!("    return x{calls}"));
    lines.extend(
        [
            "",
            "",
            "def use(shared, flag):",
            "    x = chain(shared, flag) if flag else []",
            "    x.append(1)",
        ]
        .map(str::to_owned),
    );
    let p = Project::new("bounds_returned_call_chain");
    p.write("pyproject.toml", MUT112_CONFIG)
        .write("m.py", &(lines.join("\n") + "\n"));
    let found = check_within(&p, Duration::from_secs(20));
    let lines: Vec<u64> = found
        .iter()
        .filter(|d| d["rule_code"] == "MUT112")
        .map(|d| d["line"].as_u64().unwrap())
        .collect();
    assert_eq!(lines, vec![6 + calls as u64 + 5]);
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

/// A statement rebuilt `steps` times, each time one of two ways, then run; and a flag set
/// through `steps` assignments each `|` of the one before with itself.
fn rebuilt_statement_and_flags(steps: usize) -> String {
    let mut lines = vec![
        "import socket".to_owned(),
        "import sqlite3".to_owned(),
        String::new(),
        String::new(),
        "def migrate(conn: sqlite3.Connection, ids, flag):".to_owned(),
        "    sql = \"CREATE TABLE t (x)\"".to_owned(),
    ];
    for _ in 0..steps {
        lines.push("    sql = sql.upper() if flag else sql.lower()".to_owned());
    }
    lines.push("    for i in ids:".to_owned());
    lines.push("        conn.execute(sql)".to_owned());
    lines.extend(["".to_owned(), "".to_owned()]);
    lines.push("async def resolve(ip):".to_owned());
    lines.push("    f0 = socket.AI_NUMERICHOST".to_owned());
    for i in 1..=steps {
        lines.push(format!("    f{i} = f{} | f{}", i - 1, i - 1));
    }
    lines.push(format!(
        "    return socket.getaddrinfo(ip, 0, flags=f{steps})"
    ));
    lines.join("\n") + "\n"
}

/// The DDL walk decides each value once, and the flags of an argument visit each value once:
/// both are linear in the steps, not exponential.
#[test]
fn statements_and_flags_rebuilt_from_shared_values_are_decided_once() {
    for steps in [4, 40] {
        let p = Project::new(&format!("bounds_rebuilt_{steps}"));
        p.write("m.py", &rebuilt_statement_and_flags(steps));
        let diags = check_within(&p, Duration::from_secs(20));
        // The statement stays DDL and the host numeric.
        assert!(diags.is_empty(), "{diags:#?}");
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

/// Each list copies the one before (`x1 = list(x0)`): the size of the last is not followed
/// past a few constructors, and each is decided once.
#[test]
fn chains_of_builtin_constructors_are_followed_a_bounded_depth() {
    let mut lines = vec![
        "import httpx".to_owned(),
        String::new(),
        String::new(),
        "def copies():".to_owned(),
        "    x0 = [1, 2]".to_owned(),
    ];
    for i in 1..=3000 {
        lines.push(format!("    x{i} = list(x{})", i - 1));
    }
    lines.push("    for _ in x4:".to_owned());
    lines.push("        httpx.Client()".to_owned());
    lines.push("    for _ in x3000:".to_owned());
    lines.push("        httpx.Client()".to_owned());
    let p = Project::new("bounds_constructor_chain");
    p.write("m.py", &(lines.join("\n") + "\n"));
    // Four copies keep two items; three thousand are past the depth the sizes follow.
    let diags = check_within(&p, Duration::from_secs(20));
    let lines: Vec<u64> = diags.iter().map(|d| d["line"].as_u64().unwrap()).collect();
    assert_eq!(lines, [3009], "{diags:#?}");
}

/// Two lists that extend each other: the size of each asks for the other's at the same store,
/// and the question is answered once, as not fixed, instead of recursing until the stack ends.
#[test]
fn lists_that_extend_each_other_are_not_fixed() {
    let p = Project::new("bounds_mutual_extend");
    p.write(
        "m.py",
        "import sqlite3\n\n\ndef f(conn: sqlite3.Connection):\n    a = []\n    b = []\n    a.extend(b)\n    b.extend(a)\n    for x in a:\n        conn.execute(\"SELECT ?\", (x,))\n",
    );
    let found = check_within(&p, Duration::from_secs(20));
    let queries: Vec<&Value> = found
        .iter()
        .filter(|d| d["rule_code"] == "EFF103")
        .collect();
    assert_eq!(queries.len(), 1, "{found:#?}");
}

/// A length test repeated through `steps` assignments, each `and` of the one before with its
/// double negation: every value implies the same one cap, kept once, so the caps stay one per
/// measured value instead of doubling at each step.
fn repeated_length_test(steps: usize) -> String {
    let mut lines = vec![
        "import sqlite3".to_owned(),
        String::new(),
        String::new(),
        "def f(conn: sqlite3.Connection, xs):".to_owned(),
        "    b0 = len(xs) <= 6".to_owned(),
    ];
    for i in 1..=steps {
        lines.push(format!("    b{i} = b{} and not not b{}", i - 1, i - 1));
    }
    lines.push(format!("    if not b{steps}:"));
    lines.push("        raise ValueError(xs)".to_owned());
    lines.push("    for x in xs:".to_owned());
    lines.push("        conn.execute(\"SELECT ?\", (x,))".to_owned());
    lines.join("\n") + "\n"
}

#[test]
fn length_tests_repeated_through_assignments_keep_one_cap() {
    for steps in [4, 40] {
        let p = Project::new(&format!("bounds_repeated_len_{steps}"));
        p.write("m.py", &repeated_length_test(steps));
        let found = check_within(&p, Duration::from_secs(20));
        // The loop runs at most six times, so it is no N+1.
        assert!(
            !found.iter().any(|d| d["rule_code"] == "EFF103"),
            "{found:#?}"
        );
    }
}

/// A query a sort key runs, the key reached through `depth` nested `functools.partial` objects
/// and `copies` copies before the call.
fn partial_chain(depth: usize, copies: usize) -> String {
    let mut out = vec![
        "import functools".to_owned(),
        "import sqlite3".to_owned(),
        String::new(),
        String::new(),
        "def score(conn: sqlite3.Connection, uid):".to_owned(),
        "    return conn.execute(\"SELECT ?\", (uid,))".to_owned(),
        String::new(),
        String::new(),
        "def rank(conn: sqlite3.Connection, ids):".to_owned(),
        "    k0 = functools.partial(score, conn)".to_owned(),
    ];
    for k in 1..=depth {
        out.push(format!("    k{k} = functools.partial(k{})", k - 1));
    }
    out.push(format!("    c0 = k{depth}"));
    for c in 1..=copies {
        out.push(format!("    c{c} = c{}", c - 1));
    }
    out.push(format!("    return sorted(ids, key=c{copies})"));
    out.join("\n") + "\n"
}

/// The callable a builtin calls once per item is followed through at most 16 copies and 16
/// nested partial objects, a walk at every call handing one: past either bound the sort key is
/// not resolved, so its query is not reported.
#[test]
fn callables_past_the_partial_and_copy_bounds_are_not_followed() {
    let queries = |depth: usize, copies: usize| -> usize {
        let p = Project::new(&format!("bounds_partials_{depth}_{copies}"));
        p.write("m.py", &partial_chain(depth, copies));
        check_within(&p, Duration::from_secs(20))
            .iter()
            .filter(|d| d["rule_code"] == "EFF103")
            .count()
    };
    assert_eq!(queries(3, 3), 1);
    assert_eq!(queries(30, 3), 0);
    assert_eq!(queries(3, 30), 0);
}

/// A string constant of `bytes` bytes and `aliases` names bound one to the next, the last run
/// as a statement in a loop.
fn aliased_schema(bytes: usize, aliases: usize) -> String {
    let mut lines = vec![
        "import sqlite3".to_owned(),
        String::new(),
        format!("S0 = \"CREATE TABLE t (x{})\"", "x".repeat(bytes)),
    ];
    for i in 1..=aliases {
        lines.push(format!("S{i} = S{}", i - 1));
    }
    lines.extend([
        String::new(),
        String::new(),
        "def setup(conn: sqlite3.Connection, rows):".to_owned(),
        "    for row in rows:".to_owned(),
        format!("        conn.execute(S{aliases})"),
    ]);
    lines.join("\n") + "\n"
}

/// The module's string constants keep one copy of each text, however many names alias it: a
/// 1 MiB schema with 1,000 aliases is no gigabyte of copies.
#[test]
fn aliases_of_a_large_string_constant_share_its_text() {
    let p = Project::new("bounds_aliased_schema");
    p.write("m.py", &aliased_schema(1 << 20, 1000));
    // The statement is DDL: no query runs per row.
    let diags = check_within(&p, Duration::from_secs(20));
    assert!(diags.is_empty(), "{diags:#?}");
}

/// A function with a long chain of aliases of a class and as many attribute stores through the
/// last one (`a4000.x0 = 0`, ...): whether a store rebinds a class attribute follows each copy
/// once.
#[test]
fn stores_through_a_long_alias_chain_resolve_each_copy_once() {
    let links = 4_000;
    let mut lines = vec![
        "class K:".to_owned(),
        "    pass".to_owned(),
        String::new(),
        String::new(),
        "def configure():".to_owned(),
        "    a0 = K".to_owned(),
    ];
    for i in 1..=links {
        lines.push(format!("    a{i} = a{}", i - 1));
    }
    for i in 0..links {
        lines.push(format!("    a{links}.x{i} = 0"));
    }
    let p = Project::new("bounds_alias_chain_stores");
    p.write("m.py", &(lines.join("\n") + "\n"));
    assert!(check_within(&p, Duration::from_secs(20)).is_empty());
}

/// A strict `read-owners` target on a class with thousands of subclasses and as many functions
/// reading the attribute on an object of unknown class: the classes the target covers are found
/// once per rule and name, not once per read. Every read but one is in an allowed reader.
#[test]
fn strict_reads_of_a_target_with_many_subclasses_find_its_classes_once() {
    let classes = 3_000;
    let mut lines = vec![
        "class Base:".to_owned(),
        "    def __init__(self):".to_owned(),
        "        self.secret = 0".to_owned(),
    ];
    for i in 0..classes {
        lines.extend([
            String::new(),
            String::new(),
            format!("class Sub{i}(Base):"),
            "    pass".to_owned(),
        ]);
    }
    for i in 0..=classes {
        let name = if i == classes {
            "peek".to_owned()
        } else {
            format!("allowed{i}")
        };
        lines.extend([
            String::new(),
            String::new(),
            format!("def {name}(obj):"),
            "    return obj.secret".to_owned(),
        ]);
    }
    let p = Project::new("bounds_strict_reads");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.read-owners]]\nname = \"m\"\ntargets = [\"Base.secret\"]\nallowed-readers = [\"m.allowed*\", \"m.Base.*\"]\nunresolved = \"report\"\n",
    )
    .write("m.py", &(lines.join("\n") + "\n"));
    let found = check_within(&p, Duration::from_secs(20));
    let reported: Vec<&str> = found
        .iter()
        .filter(|d| d["rule_code"] == "MUT206")
        .map(|d| d["qualname"].as_str().unwrap())
        .collect();
    assert_eq!(reported, vec!["peek"]);
}

/// A class with thousands of subclasses and as many functions reading its class constant on an
/// instance (`field.__dict__[keys.KEY]`): whether the constant holds on every class the instance
/// may be is decided once per class and name, not once per read. The contract allows every
/// writer but one, which is reported through the constant.
#[test]
fn class_constant_reads_on_a_class_with_many_subclasses_decide_it_once() {
    let classes = 3_000;
    let mut lines = vec![
        "from typing import ClassVar".to_owned(),
        String::new(),
        String::new(),
        "class Field:".to_owned(),
        "    def __init__(self):".to_owned(),
        "        self.public = 0".to_owned(),
        String::new(),
        String::new(),
        "class Keys:".to_owned(),
        "    KEY: ClassVar[str] = \"public\"".to_owned(),
    ];
    for i in 0..classes {
        lines.extend([
            String::new(),
            String::new(),
            format!("class Keys{i}(Keys):"),
            "    pass".to_owned(),
        ]);
    }
    for i in 0..=classes {
        let name = if i == classes {
            "write".to_owned()
        } else {
            format!("allowed{i}")
        };
        lines.extend([
            String::new(),
            String::new(),
            format!("def {name}(keys: Keys, field: Field):"),
            "    field.__dict__[keys.KEY] = 1".to_owned(),
        ]);
    }
    let p = Project::new("bounds_class_constant_reads");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.write-owners]]\nname = \"m\"\ntargets = [\"Field.public\"]\nallowed-writers = [\"m.allowed*\"]\n",
    )
    .write("m.py", &(lines.join("\n") + "\n"));
    let found = check_within(&p, Duration::from_secs(20));
    let reported: Vec<&Value> = found
        .iter()
        .filter(|d| d["rule_code"] == "MUT201")
        .collect();
    assert_eq!(reported.len(), 1, "{found:#?}");
    assert_eq!(reported[0]["qualname"], "write");
}

/// A class with thousands of subclasses whose `reset` a call target naming no class names, and
/// a function calling `Base.reset(obj)` as many times: the class family of each call (the
/// classes defining the method it reaches, with their subclasses) is expanded once per target
/// and defining class, not once per call. Only one call is outside the allowed callers.
#[test]
fn calls_of_a_method_with_many_subclasses_expand_its_family_once() {
    let size = 5_000;
    let mut lines = vec![
        "class Base:".to_owned(),
        "    def reset(self):".to_owned(),
        "        pass".to_owned(),
    ];
    for i in 0..size {
        lines.extend([
            String::new(),
            String::new(),
            format!("class Sub{i}(Base):"),
            "    pass".to_owned(),
        ]);
    }
    lines.extend([String::new(), String::new(), "def run(obj):".to_owned()]);
    lines.extend((0..size).map(|_| "    Base.reset(obj)".to_owned()));
    lines.extend(["", "", "def other(obj):", "    Base.reset(obj)"].map(str::to_owned));
    let p = Project::new("bounds_call_target_families");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"resets\"\ntargets = [\"reset\"]\nallowed-callers = [\"m.run\"]\n",
    )
    .write("m.py", &(lines.join("\n") + "\n"));
    let found = check_within(&p, Duration::from_secs(20));
    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(found[0]["qualname"], "other");
}

/// Modules binding twelve thousand names, each to an attribute of the one before
/// (`a1 = a0.x`, `a2 = a1.x`, ...), about as many as a module body's IR budget models: whether
/// a binding is a string constant takes a bounded walk of copies and attribute loads, past
/// which it is none, not a walk down the whole chain.
#[test]
fn module_names_bound_along_a_long_attribute_chain_take_a_bounded_walk() {
    let links = 12_000;
    let mut lines = vec!["a0 = \"s\"".to_owned()];
    lines.extend((1..=links).map(|i| format!("a{i} = a{}.x", i - 1)));
    let source = lines.join("\n") + "\n";
    let p = Project::new("bounds_constant_attribute_chain");
    for module in ["m0.py", "m1.py", "m2.py", "m3.py"] {
        p.write(module, &source);
    }
    assert!(check_within(&p, Duration::from_secs(20)).is_empty());
}

/// One function making twenty thousand project calls (`Base.reset(obj, ...)` with eight
/// operands each), about as many as its IR budget models: `MUT202` maps each call instruction
/// to its call site and tests its operands' definitions, which it looks up in the function's
/// definitions once built, not by walking the function per operand.
#[test]
fn many_calls_in_one_function_look_up_their_operands_once() {
    let mut lines = vec![
        "class Base:".to_owned(),
        "    def reset(self, *args):".to_owned(),
        "        pass".to_owned(),
        String::new(),
        String::new(),
        "def run(obj):".to_owned(),
    ];
    lines.extend(
        (0..20_000).map(|_| "    Base.reset(obj, obj, obj, obj, obj, obj, obj, obj)".to_owned()),
    );
    let p = Project::new("bounds_many_call_operands");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT202\"]\n",
    )
    .write("m.py", &(lines.join("\n") + "\n"));
    assert!(check_within(&p, Duration::from_secs(20)).is_empty());
}

/// A helper that types nothing making `accesses` times the access `access` of a contract
/// section `owners` on `Guarded` (`read-owners` for `obj.secret`, `call-owners` for
/// `obj.open()`, `write-owners` for `obj.secret = 1`), in strict mode, and a caller handing it
/// an instance of an unrelated class `calls` times.
fn strict_accesses(
    name: &str,
    owners: &str,
    access: &str,
    accesses: usize,
    calls: usize,
) -> Project {
    let target = if owners == "call-owners" {
        "Guarded.open"
    } else {
        "Guarded.secret"
    };
    let contract = format!(
        "[[tool.heki.{owners}]]\nname = \"guarded\"\ntargets = [\"{target}\"]\nunresolved = \"report\"\n"
    );
    let mut lines = Vec::new();
    for class in ["Guarded", "Other"] {
        lines.extend([
            format!("class {class}:"),
            "    def __init__(self):".to_owned(),
            "        self.secret = 0".to_owned(),
            String::new(),
            "    def open(self):".to_owned(),
            "        return None".to_owned(),
            String::new(),
            String::new(),
        ]);
    }
    lines.push("def peek(obj):".to_owned());
    lines.extend((0..accesses).map(|_| format!("    {access}")));
    lines.extend([
        String::new(),
        String::new(),
        "def use(o: Other):".to_owned(),
    ]);
    lines.extend((0..calls).map(|_| "    peek(o)".to_owned()));
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT205\", \"MUT206\", \"MUT207\"]\n\n{contract}"
        ),
    )
    .write("m.py", &(lines.join("\n") + "\n"));
    p
}

/// Thousands of strict reads in a helper whose caller calls it thousands of times: whether a
/// read is a finding is decided once per rule, function, object, and name, not once per read,
/// each walking the same callers.
#[test]
fn strict_reads_of_one_name_in_a_helper_walk_its_callers_once() {
    let p = strict_accesses(
        "bounds_strict_helper_reads",
        "read-owners",
        "obj.secret",
        4_000,
        4_000,
    );
    assert!(check_within(&p, Duration::from_secs(20)).is_empty());
}

/// The same for strict calls (`MUT207`).
#[test]
fn strict_calls_of_one_name_in_a_helper_walk_its_callers_once() {
    let p = strict_accesses(
        "bounds_strict_helper_calls",
        "call-owners",
        "obj.open()",
        4_000,
        4_000,
    );
    assert!(check_within(&p, Duration::from_secs(20)).is_empty());
}

/// The same for strict writes (`MUT205`).
#[test]
fn strict_writes_of_one_name_in_a_helper_walk_its_callers_once() {
    let p = strict_accesses(
        "bounds_strict_helper_writes",
        "write-owners",
        "obj.secret = 1",
        4_000,
        4_000,
    );
    assert!(check_within(&p, Duration::from_secs(20)).is_empty());
}

/// `links` functions in one module, each making `calls` other calls and handing its argument to
/// the next, the last changing it in place, below a function with a read-only parameter.
fn argument_handed_down(links: usize, calls: usize) -> String {
    let other = vec!["g()"; calls].join("; ");
    let mut lines = vec![
        "from collections.abc import Sequence".to_owned(),
        String::new(),
        String::new(),
        "def g():".to_owned(),
        "    pass".to_owned(),
        String::new(),
        String::new(),
        "def entry(xs: Sequence[int]):".to_owned(),
        "    f0(xs)".to_owned(),
    ];
    for i in 0..links - 1 {
        lines.extend([
            String::new(),
            String::new(),
            format!("def f{i}(x):"),
            format!("    {other}; f{}(x)", i + 1),
        ]);
    }
    lines.extend([
        String::new(),
        String::new(),
        format!("def f{}(x):", links - 1),
        "    x.append(1)".to_owned(),
    ]);
    lines.join("\n") + "\n"
}

/// The chain of a parameter's change through twelve thousand functions of one module, each
/// with ten other calls: the walk to the function whose body makes the change indexes the
/// module's calls by caller once, not once per function it passes.
#[test]
fn a_change_handed_down_through_one_module_indexes_its_calls_once() {
    let p = Project::new("bounds_change_chain");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT110\"]\n",
    )
    .write("m.py", &argument_handed_down(12_000, 10));
    let found = check_within(&p, Duration::from_secs(20));
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["rule_code"], "MUT110");
    assert_eq!(found[0]["related"].as_array().unwrap().len(), 12_000);
}

/// Eight thousand functions of one module, each handing its read-only parameter to a helper
/// that changes it itself, and to one that hands it to that helper: the reports' chains share
/// one index of the module's calls, and a chain that ends at its first function indexes none.
#[test]
fn many_reports_into_one_module_share_its_index_of_calls() {
    let mut lines = vec![
        "from collections.abc import Sequence".to_owned(),
        String::new(),
        String::new(),
        "def change(x):".to_owned(),
        "    x.append(1)".to_owned(),
        String::new(),
        String::new(),
        "def relay(x):".to_owned(),
        "    change(x)".to_owned(),
    ];
    for i in 0..8_000 {
        lines.extend([
            String::new(),
            String::new(),
            format!("def entry{i}(xs: Sequence[int]):"),
            "    change(xs)".to_owned(),
            "    relay(xs)".to_owned(),
        ]);
    }
    let p = Project::new("bounds_change_chains_into_one_module");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT110\"]\n",
    )
    .write("m.py", &(lines.join("\n") + "\n"));
    let found = check_within(&p, Duration::from_secs(20));
    assert_eq!(found.len(), 16_000);
    let hops: Vec<usize> = found
        .iter()
        .map(|d| d["related"].as_array().unwrap().len())
        .collect();
    assert_eq!(hops.iter().filter(|&&n| n == 2).count(), 8_000);
}
