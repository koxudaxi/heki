//! The cost of the analyses past the IR, end to end through the real binary: `isinstance`
//! narrowing through phis, the passes of the type inference and of the classes of merged values,
//! the closure cells child scopes read, the size of the Soundness lane's proof state, the values
//! `EFF107` follows from a task factory, the walk to the callable a builtin calls once per item,
//! contracts with many call targets and argument lists, the walk to the uses handing a reference to
//! a call target on, the locks `EFF109` follows, and the values class statements pass as bases.

mod common;

use common::Project;
use common::lsp::{Lsp, codes, uri};
use serde_json::{Value, json};
use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `heki check --output-format json` of the project, which must finish within `limit`.
fn check_within(p: &Project, limit: Duration) -> Vec<Value> {
    let mut child = Command::new(env!("CARGO_BIN_EXE_heki"))
        .args(["check", "--heuristic", "--output-format", "json", "."])
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

/// A function binding `names` variables in turn `bindings` times in all, each binding followed
/// by a generator expression reading every variable: closure cells bound many times, each read
/// by as many scopes. The values are of a wide container type, so that joining them costs more
/// than the rest of the work per binding. The function returns `_given` of a `Model` that a last
/// generator expression takes out of each variable. With `declared`, each variable is annotated
/// once.
fn cells_read_after_each_binding(bindings: usize, names: usize, declared: bool) -> String {
    let ty = format!("dict[str, list[tuple[{}]]]", vec!["Model"; 32].join(", "));
    let mut lines = vec![
        "class Model:".to_owned(),
        "    def __init__(self):".to_owned(),
        "        self._given = 1".to_owned(),
        String::new(),
        String::new(),
        format!("def load(i) -> {ty}:"),
        "    return {}".to_owned(),
        String::new(),
        String::new(),
        "def shape(xs, out):".to_owned(),
    ];
    if declared {
        lines.extend((0..names).map(|k| format!("    x{k}: {ty} = load({k})")));
    }
    let all: Vec<String> = (0..names).map(|k| format!("x{k}")).collect();
    for i in 0..bindings {
        lines.push(format!("    x{} = load({i})", i % names));
        lines.push(format!(
            "    out.append(sum(1 for _ in xs if {}))",
            all.join(" and ")
        ));
    }
    let given: Vec<String> = (0..names)
        .map(|k| format!("next(x{k}[\"k\"][0][0] for _ in xs)._given"))
        .collect();
    lines.push(format!("    return {}", given.join(", ")));
    lines.join("\n") + "\n"
}

/// The scopes created between the bindings of a closure cell read the values bound to it so far
/// as one joined fact, kept as the bindings are computed, not folded from every binding again per
/// scope. The cells keep their types: a forbidden read of `Model._given` per variable.
#[test]
fn scopes_reading_a_cell_bound_many_times_read_its_joined_fact() {
    let config = "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT203\"]\n\n\
                  [[tool.heki.read-owners]]\nname = \"given\"\ntargets = [\"m.Model._given\"]\n\
                  forbidden-readers = [\"m:shape\"]\n";
    for (names, declared) in [(1, false), (4, true)] {
        let p = Project::new(&format!("bounds_cells_{names}"));
        p.write("pyproject.toml", config).write(
            "m.py",
            &cells_read_after_each_binding(3_000, names, declared),
        );
        let found = check_within(&p, Duration::from_secs(20));
        assert_eq!(
            found.iter().filter(|d| d["rule_code"] == "MUT203").count(),
            names
        );
    }
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
            .filter(|d| d["rule_code"] == "EFF111")
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
/// the arguments of its 256 parameters: each argument walks the chain, so past the budget of
/// walked values proportional to the source what the function returns is unknown, and nothing
/// is reported from it.
#[test]
fn origin_walks_past_their_budget_return_unknown() {
    let (links, args) = (2_000, 256);
    let params: Vec<String> = (0..args).map(|i| format!("r{i}")).collect();
    let mut lines = vec![
        format!("def pick({}):", params.join(", ")),
        "    return r0".to_owned(),
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

/// A function reading attributes down a chain ten thousand deep (`x = x.child`) under `MUT112`:
/// whether each receiver holds an object the function makes is read from its own definition,
/// not again through every read before it, which took a walk as deep as the chain (and the
/// stack) per read and pass.
#[test]
fn attribute_reads_down_a_long_chain_decide_each_receiver_once() {
    let links = 10_000;
    let mut lines = vec![
        "def walk(node):".to_owned(),
        "    extra = node or []".to_owned(),
        "    extra.append(1)".to_owned(),
        "    x = node".to_owned(),
    ];
    lines.extend((0..links).map(|_| "    x = x.child".to_owned()));
    lines.push("    x.append(1)".to_owned());
    let p = Project::new("bounds_attribute_chain");
    p.write("pyproject.toml", MUT112_CONFIG)
        .write("m.py", &(lines.join("\n") + "\n"));
    let found: Vec<u64> = check_within(&p, Duration::from_secs(20))
        .iter()
        .filter(|d| d["rule_code"] == "MUT112")
        .map(|d| d["line"].as_u64().unwrap())
        .collect();
    assert_eq!(found, vec![3]);
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

/// A function of twelve thousand calls, each handed the previous call's result (`v1 = g(v0)`),
/// about as many as its IR budget models, with every rule selected: where each argument may come
/// from (the caller's parameters, objects it constructs) is decided once per value of the
/// function, not by a walk back down the chain at every call.
#[test]
fn calls_each_handed_the_previous_result_find_each_origin_once() {
    let calls = 12_400;
    let mut lines = vec![
        "def g(x):".to_owned(),
        "    return x".to_owned(),
        String::new(),
        String::new(),
        "def f(p, session):".to_owned(),
        "    v0 = p".to_owned(),
    ];
    for i in 1..=calls {
        lines.push(format!("    v{i} = g(v{})", i - 1));
    }
    lines.push(format!("    return v{calls}"));
    let p = Project::new("bounds_call_chain");
    let (_, rules, _) = p.heki(&["rule", "--all", "--output-format", "json"]);
    let codes: Vec<Value> = serde_json::from_str::<Vec<Value>>(&rules)
        .unwrap()
        .into_iter()
        .map(|rule| rule["code"].clone())
        .collect();
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = {}\n",
            Value::from(codes)
        ),
    )
    .write("m.py", &(lines.join("\n") + "\n"));
    // The function stays modeled, so that its calls are analyzed at all.
    let (_, hazards, _) = p.heki(&["assumptions", "--json", "m.py"]);
    let hazards: Vec<Value> = serde_json::from_str(&hazards).unwrap();
    assert!(
        hazards.iter().all(|h| h["kind"] != "unmodeled"),
        "{hazards:#?}"
    );
    assert!(check_within(&p, Duration::from_secs(20)).is_empty());
}

/// A client made lazily (`Conn.get`) on an object built before `depth` nested loops over the
/// data, reached through a thousand calls handing the object on (`c1 = wrap(c0)`, with `wrap`
/// typed to return a `Conn`) in the innermost loop.
fn lazy_client_under_nested_loops(depth: usize) -> String {
    let links = 1_000;
    let mut lines: Vec<String> = [
        "import requests",
        "",
        "",
        "class Conn:",
        "    def __init__(self):",
        "        self._session = None",
        "",
        "    def get(self, url):",
        "        if self._session is None:",
        "            self._session = requests.Session()",
        "        return self._session.get(url)",
        "",
        "",
        "def wrap(c: Conn) -> Conn:",
        "    return c",
        "",
        "",
        "def scan(urls):",
        "    c0 = Conn()",
    ]
    .map(str::to_owned)
    .into();
    for d in 0..depth {
        lines.push(format!("{}for u{d} in urls:", " ".repeat(4 + d)));
    }
    let indent = " ".repeat(4 + depth);
    for i in 1..=links {
        lines.push(format!("{indent}c{i} = wrap(c{})", i - 1));
    }
    lines.push(format!("{indent}c{links}.get(u0)"));
    lines.join("\n") + "\n"
}

/// Where a value comes from inside nested loops is decided once per loop around it, so the walks
/// grow with the depth of the loops: thirty loops deep (deeper than CPython compiles) they pass
/// the function's budget, and the object then counts as one the loops may construct, as `EFF102`
/// counts a call as running on every iteration unless heki shows that it runs once. In one loop
/// the client is made once per object.
#[test]
fn origins_past_the_walk_budget_count_as_made_in_every_loop() {
    let eff102 = |depth: usize| {
        let p = Project::new(&format!("bounds_nested_origins_{depth}"));
        p.write("m.py", &lazy_client_under_nested_loops(depth));
        check_within(&p, Duration::from_secs(20))
            .iter()
            .filter(|d| d["rule_code"] == "EFF102")
            .count()
    };
    assert_eq!(eff102(1), 0);
    assert_eq!(eff102(30), 1);
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
            .filter(|d| d["rule_code"] == "EFF111")
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

/// A shared value copied along a long chain of assignments (`v1 = v0`, `v2 = v1`, ...) before
/// `MUT114` reads which object each copy is: each copy's origin is found once.
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
        .filter(|d| d["rule_code"] == "MUT114")
        .map(|d| d["qualname"].as_str().unwrap())
        .collect();
    assert_eq!(reported, vec!["extend"], "{found:#?}");
}

/// Many attributes read from one shared value, each a value of its own, and changed after: the
/// memo and lazy-initialization writes of all of them are read in one walk of the function.
#[test]
fn many_shared_values_read_their_memo_writes_in_one_walk() {
    let mut lines = vec![
        "from functools import lru_cache".to_owned(),
        String::new(),
        String::new(),
        "@lru_cache(maxsize=None)".to_owned(),
        "def config(name):".to_owned(),
        "    return object()".to_owned(),
        String::new(),
        String::new(),
        "def touch(name):".to_owned(),
        "    root = config(name)".to_owned(),
    ];
    for i in 0..3000 {
        lines.push(format!("    a{i} = root.a{i}"));
        lines.push(format!("    a{i}.cache_size = {i}"));
    }
    let p = Project::new("bounds_many_shared_values");
    p.write("m.py", &(lines.join("\n") + "\n"));
    let found = check_within(&p, Duration::from_secs(20));
    let reported = found.iter().filter(|d| d["rule_code"] == "MUT114").count();
    assert_eq!(reported, 3000, "{found:#?}");
}

/// A helper initializing thousands of attributes of its argument lazily, each under its own
/// test (`if p.a0 is None: p.a0 = 0`): the walks that find what each test guards share the
/// function's budget, past which no lazy initialization is recognized and the changes count.
#[test]
fn lazy_initializations_past_the_budget_count_as_changes() {
    let mut lines = vec![
        "from functools import lru_cache".to_owned(),
        String::new(),
        String::new(),
        "@lru_cache(maxsize=None)".to_owned(),
        "def config(name):".to_owned(),
        "    return object()".to_owned(),
        String::new(),
        String::new(),
        "def fill(p):".to_owned(),
    ];
    for i in 0..3000 {
        lines.push(format!("    if p.a{i} is None:"));
        lines.push(format!("        p.a{i} = {i}"));
    }
    lines.extend(["", "", "def use(name):", "    fill(config(name))"].map(str::to_owned));
    let p = Project::new("bounds_many_lazy_initializations");
    p.write("m.py", &(lines.join("\n") + "\n"));
    let found = check_within(&p, Duration::from_secs(20));
    let reported: Vec<&str> = found
        .iter()
        .filter(|d| d["rule_code"] == "MUT114")
        .map(|d| d["qualname"].as_str().unwrap())
        .collect();
    assert_eq!(reported, vec!["use"], "{found:#?}");
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
        .filter(|d| d["rule_code"] == "EFF113")
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
            .filter(|d| d["rule_code"] == "EFF113")
            .count()
    };
    assert_eq!(queries(3, 3), 1);
    assert_eq!(queries(30, 3), 0);
    assert_eq!(queries(3, 30), 0);
}

/// Thousands of names unpacked from one display of the previous value
/// (`a0, a1, ... = acc, acc, ...`), each rebuilt entry by entry, in a function with as many
/// starred elements elsewhere: where each display starts unpacking is indexed once.
#[test]
fn names_unpacked_from_one_display_find_its_starred_elements_once() {
    let n = 3000;
    let names: Vec<String> = (0..n).map(|i| format!("a{i}")).collect();
    let mut lines = vec![
        "import numpy as np".to_owned(),
        String::new(),
        String::new(),
        "def rebuilt(rows):".to_owned(),
        "    acc = np.empty(0)".to_owned(),
        "    for row in rows:".to_owned(),
    ];
    for i in 0..n {
        lines.push(format!("        b{i} = [*row]"));
    }
    lines.push(format!(
        "        {} = {}",
        names.join(", "),
        vec!["acc"; n].join(", ")
    ));
    let scaled: Vec<String> = names.iter().map(|a| format!("{a} * 2")).collect();
    lines.push(format!(
        "        acc = np.concatenate(({}, [row]))",
        scaled.join(", ")
    ));
    lines.push("    return acc".to_owned());
    let p = Project::new("bounds_unpacked_display");
    p.write("m.py", &(lines.join("\n") + "\n"));
    let found = check_within(&p, Duration::from_secs(20));
    // Every operand is computed entry by entry from the previous array.
    assert!(
        !found.iter().any(|d| d["rule_code"] == "DATA101"),
        "{found:#?}"
    );
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

/// A helper filling thousands of items of its argument's attribute, each in a branch of its own
/// (`if flag: p.items[0] = 0`): the `not in` tests that would make a store a memo fill are found
/// in one walk of the blocks.
#[test]
fn item_stores_in_many_branches_find_membership_tests_once() {
    let mut lines = vec![
        "from functools import lru_cache".to_owned(),
        String::new(),
        String::new(),
        "@lru_cache(maxsize=None)".to_owned(),
        "def config(name):".to_owned(),
        "    return object()".to_owned(),
        String::new(),
        String::new(),
        "def fill(p, flag):".to_owned(),
    ];
    for i in 0..3000 {
        lines.push("    if flag:".to_owned());
        lines.push(format!("        p.items[{i}] = {i}"));
    }
    lines.extend(["", "", "def use(name):", "    fill(config(name), True)"].map(str::to_owned));
    let p = Project::new("bounds_branch_item_stores");
    p.write("m.py", &(lines.join("\n") + "\n"));
    let found = check_within(&p, Duration::from_secs(20));
    let reported: Vec<&str> = found
        .iter()
        .filter(|d| d["rule_code"] == "MUT114")
        .map(|d| d["qualname"].as_str().unwrap())
        .collect();
    assert_eq!(reported, vec!["use"], "{found:#?}");
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

/// A value one `if` / `elif` chain of thousands of arms assigns, then tested as many times
/// (`isinstance(src, str)`): the edges the tests of the merged value keep share the function's
/// budget, past which a test is no guard and the effect it guards is kept.
#[test]
fn tests_of_a_wide_merge_keep_their_edges_within_a_budget() {
    let n = 2000;
    let mut lines = vec![
        "import io".to_owned(),
        "from urllib.request import urlopen".to_owned(),
        String::new(),
        String::new(),
        "def fetch(src, kind):".to_owned(),
        "    if kind == 0:".to_owned(),
        "        src = io.StringIO()".to_owned(),
    ];
    for i in 1..n {
        lines.push(format!("    elif kind == {i}:"));
        lines.push("        src = io.StringIO()".to_owned());
    }
    for _ in 0..n {
        lines.push("    if isinstance(src, str):".to_owned());
        lines.push("        urlopen(src).close()".to_owned());
    }
    lines.extend(
        [
            "",
            "",
            "async def load(kind):",
            "    fetch(\"https://example.com\", kind)",
        ]
        .map(str::to_owned),
    );
    let p = Project::new("bounds_wide_merge_tests");
    p.write("m.py", &(lines.join("\n") + "\n"));
    let found = check_within(&p, Duration::from_secs(20));
    let reported: Vec<&str> = found
        .iter()
        .filter(|d| d["rule_code"] == "EFF111")
        .map(|d| d["qualname"].as_str().unwrap())
        .collect();
    assert_eq!(reported, vec!["load"], "{found:#?}");
}

/// `MUT113` selected (it reads the iterations of a method's own containers).
const MUT113_CONFIG: &str =
    "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"MUT113\"]\n";

/// A class with `containers` dicts, a generator kept on the instance iterating one of them
/// through `body`'s aliases, and a method changing the first.
fn kept_generator_over(containers: usize, body: &[String]) -> String {
    let mut lines = vec![
        "class Holder:".to_owned(),
        "    def __init__(self) -> None:".to_owned(),
    ];
    for i in 0..containers {
        lines.push(format!("        self.d{i}: dict[str, int] = {{}}"));
    }
    lines.push("        self._cursor = self._walk(1)".to_owned());
    lines.push(String::new());
    lines.push("    def _walk(self, n):".to_owned());
    lines.extend(body.iter().cloned());
    lines.push("            yield k".to_owned());
    lines.push(String::new());
    lines.push("    def drop(self, k: str) -> None:".to_owned());
    lines.push("        self.d0.pop(k, None)".to_owned());
    lines.join("\n") + "\n"
}

/// `MUT113` reports of `heki check` over `src`.
fn iterated_mutations(name: &str, src: &str) -> usize {
    let p = Project::new(name);
    p.write("pyproject.toml", MUT113_CONFIG).write("m.py", src);
    check_within(&p, Duration::from_secs(20))
        .iter()
        .filter(|d| d["rule_code"] == "MUT113")
        .count()
}

/// A loop rotating aliases of `containers` dicts (`t = a0; a0 = a1; ...; a599 = t`): the
/// containers a value may iterate reach each alias one pass after another, so finding them takes
/// a pass per alias. Past the pass limit no value iterates a container: no report.
#[test]
fn iterated_containers_past_the_pass_limit_are_unknown() {
    let rotating = |containers: usize| {
        let mut body: Vec<String> = (0..containers)
            .map(|i| format!("        a{i} = self.d{i}"))
            .collect();
        body.push("        while n:".to_owned());
        body.push("            t = a0".to_owned());
        body.extend((0..containers - 1).map(|i| format!("            a{i} = a{}", i + 1)));
        body.push(format!("            a{} = t", containers - 1));
        body.push("            n -= 1".to_owned());
        body.push("        for k in a0:".to_owned());
        kept_generator_over(containers, &body)
    };
    assert_eq!(iterated_mutations("bounds_rotation_3", &rotating(3)), 1);
    // Within the pass limit, and past it within the fact budget.
    assert_eq!(iterated_mutations("bounds_rotation_20", &rotating(20)), 1);
    assert_eq!(iterated_mutations("bounds_rotation_30", &rotating(30)), 0);
    // Past the fact budget too.
    assert_eq!(iterated_mutations("bounds_rotation_600", &rotating(600)), 0);
}

/// A chain of `containers` merges (`if n == i: x = self.di`): the merged value after the `i`-th
/// may iterate any of `i` containers, so the facts grow with the square of the chain. Past their
/// budget (proportional to the source) no value iterates a container: no report.
#[test]
fn iterated_containers_past_their_budget_are_unknown() {
    let merging = |containers: usize| {
        let mut body = vec!["        x = self.d0".to_owned()];
        for i in 1..containers {
            body.push(format!("        if n == {i}:"));
            body.push(format!("            x = self.d{i}"));
        }
        body.push("        for k in x:".to_owned());
        kept_generator_over(containers, &body)
    };
    assert_eq!(iterated_mutations("bounds_merges_3", &merging(3)), 1);
    assert_eq!(iterated_mutations("bounds_merges_2000", &merging(2000)), 0);
}

/// A loop over a private generator whose body makes `calls` calls of a method changing the
/// container it iterates: what each call runs on (the iterated call's arguments, a method of the
/// family) is looked up by its span once, not by a scan of the function's calls per call.
#[test]
fn a_loop_over_a_generator_looks_up_each_call_of_its_body_once() {
    let calls = 20_000;
    let mut lines = vec![
        "class Node:".to_owned(),
        "    def __init__(self) -> None:".to_owned(),
        "        self.items: dict[str, int] = {}".to_owned(),
        String::new(),
        "    def _walk(self):".to_owned(),
        "        for k in self.items:".to_owned(),
        "            yield k".to_owned(),
        String::new(),
        "    def touch(self) -> None:".to_owned(),
        "        self.items.pop(\"x\", None)".to_owned(),
        String::new(),
        String::new(),
        "def visit(n: Node):".to_owned(),
        "    for k in n._walk():".to_owned(),
    ];
    lines.extend((0..calls).map(|_| "        n.touch()".to_owned()));
    assert_eq!(
        iterated_mutations("bounds_loop_calls", &(lines.join("\n") + "\n")),
        1
    );
}

/// Loops over a container of `self` and over a private generator's result whose bodies hold
/// `branches` conditional blocks one after another: the blocks from which each loop goes on to
/// its next step are found by one walk back from its header, not by a walk from every block.
#[test]
fn a_loop_with_many_branches_finds_the_blocks_reaching_its_header_once() {
    let branches = 8_000;
    let body = |lines: &mut Vec<String>| {
        for _ in 0..branches {
            lines.push("            if k:".to_owned());
            lines.push("                pass".to_owned());
        }
    };
    let mut lines = vec![
        "class Table:".to_owned(),
        "    def __init__(self) -> None:".to_owned(),
        "        self.items: dict[str, int] = {}".to_owned(),
        String::new(),
        "    def _keys(self):".to_owned(),
        "        yield from self.items".to_owned(),
        String::new(),
        "    def scan(self):".to_owned(),
        "        for k in self.items:".to_owned(),
    ];
    body(&mut lines);
    lines.push(String::new());
    lines.push("    def scan_keys(self):".to_owned());
    lines.push("        for k in self._keys():".to_owned());
    body(&mut lines);
    assert_eq!(
        iterated_mutations("bounds_loop_branches", &(lines.join("\n") + "\n")),
        0
    );
}

/// A function consuming a private generator's result `consumptions` times in a row
/// (`list(node._walk())`): what it does with each call's result (handed on, iterated) is read
/// from one index of its call sites and iterations, not by a scan of the file's call sites per
/// call. Each result is consumed at once, so nothing runs while the generator is suspended.
#[test]
fn repeated_consumptions_of_a_generator_read_each_call_once() {
    let consumptions = 16_000;
    let mut lines = vec![
        "class Node:".to_owned(),
        "    def __init__(self) -> None:".to_owned(),
        "        self.items: dict[str, int] = {}".to_owned(),
        String::new(),
        "    def _walk(self):".to_owned(),
        "        for k in self.items:".to_owned(),
        "            yield k".to_owned(),
        String::new(),
        "    def drop(self, k: str) -> None:".to_owned(),
        "        self.items.pop(k, None)".to_owned(),
        String::new(),
        String::new(),
        "def consume(node: Node):".to_owned(),
    ];
    lines.extend((0..consumptions).map(|_| "    list(node._walk())".to_owned()));
    assert_eq!(
        iterated_mutations("bounds_consumptions", &(lines.join("\n") + "\n")),
        0
    );
}

/// A loop over `zip(node._walk(), ...)` of `results` generator calls whose body makes as many
/// calls: the loop is one fact shared by every result it steps, and what it does to them is found
/// once per instruction, not once per result.
#[test]
fn a_loop_over_many_generator_results_is_summarized_once() {
    let results = 8_000;
    let mut lines = vec![
        "class Node:".to_owned(),
        "    def __init__(self) -> None:".to_owned(),
        "        self.items: dict[str, int] = {}".to_owned(),
        String::new(),
        "    def _walk(self):".to_owned(),
        "        for k in self.items:".to_owned(),
        "            yield k".to_owned(),
        String::new(),
        "    def touch(self) -> None:".to_owned(),
        "        self.items.pop(\"x\", None)".to_owned(),
        String::new(),
        String::new(),
        "def visit(node: Node):".to_owned(),
        "    for row in zip(".to_owned(),
    ];
    lines.extend((0..results).map(|_| "        node._walk(),".to_owned()));
    lines.push("    ):".to_owned());
    lines.extend((0..results).map(|_| "        node.touch()".to_owned()));
    assert_eq!(
        iterated_mutations("bounds_zipped_results", &(lines.join("\n") + "\n")),
        1
    );
}

/// A loop over `zip(self.d0, ..., self.d1999)` whose body makes as many calls: the loop is one
/// fact for every container it steps, and what each call changes is read once, not once per
/// container.
#[test]
fn a_loop_over_many_self_containers_is_one_fact() {
    let containers = 2_000;
    let mut lines = vec![
        "class Table:".to_owned(),
        "    def __init__(self) -> None:".to_owned(),
    ];
    lines.extend((0..containers).map(|i| format!("        self.d{i}: dict[str, int] = {{}}")));
    lines.extend(
        [
            "",
            "    def touch(self) -> None:",
            "        pass",
            "",
            "    def drop(self) -> None:",
            "        self.d0.pop(\"x\", None)",
            "",
            "    def visit(self) -> None:",
        ]
        .map(str::to_owned),
    );
    let zipped: Vec<String> = (0..containers).map(|i| format!("self.d{i}")).collect();
    lines.push(format!("        for row in zip({}):", zipped.join(", ")));
    lines.extend((0..containers).map(|_| "            self.touch()".to_owned()));
    lines.push("            self.drop()".to_owned());
    assert_eq!(
        iterated_mutations("bounds_zipped_containers", &(lines.join("\n") + "\n")),
        1
    );
}

/// One `zip` of `containers` dicts of `self` stepped by as many `yield from`s in a private
/// generator, and a method changing the first dict while the generator is suspended: each
/// `yield from` steps every container, so past ten of them the iterations' facts pass their
/// budget and the generator has none. Ten give ten reports, two thousand none.
#[test]
fn iterations_past_the_fact_budget_are_unknown() {
    let zipped = |containers: usize| {
        let mut lines = vec![
            "class Table:".to_owned(),
            "    def __init__(self) -> None:".to_owned(),
        ];
        lines.extend((0..containers).map(|i| format!("        self.d{i}: dict[str, int] = {{}}")));
        lines.extend(
            [
                "",
                "    def drop(self) -> None:",
                "        self.d0.pop(\"x\", None)",
                "",
                "    def pump(self) -> None:",
                "        g = self._rows()",
                "        next(g)",
                "        self.drop()",
                "        next(g)",
                "",
                "    def _rows(self):",
            ]
            .map(str::to_owned),
        );
        let attrs: Vec<String> = (0..containers).map(|i| format!("self.d{i}")).collect();
        lines.push(format!("        z = zip({})", attrs.join(", ")));
        lines.extend((0..containers).map(|_| "        yield from z".to_owned()));
        lines.join("\n") + "\n"
    };
    assert_eq!(
        iterated_mutations("bounds_iteration_facts_few", &zipped(10)),
        10
    );
    assert_eq!(
        iterated_mutations("bounds_iteration_facts", &zipped(2_000)),
        0
    );
}

/// A private generator handed on through five thousand private wrappers, each returning the one
/// before's result (`def _w1(self): return self._w0()`), and a loop over the last one's result
/// calling a method that changes the generator's container: the walk from the generator to the
/// loop keeps its own stack, one entry per wrapper, instead of a native frame per wrapper.
#[test]
fn a_long_chain_of_wrappers_handing_on_a_generator_is_walked_on_its_own_stack() {
    let wrappers = 5_000;
    let mut lines = vec![
        "class Box:".to_owned(),
        "    def __init__(self) -> None:".to_owned(),
        "        self.items: dict[str, int] = {}".to_owned(),
        String::new(),
        "    def _w0(self):".to_owned(),
        "        yield from self.items".to_owned(),
    ];
    for i in 1..=wrappers {
        lines.push(String::new());
        lines.push(format!("    def _w{i}(self):"));
        lines.push(format!("        return self._w{}()", i - 1));
    }
    lines.extend(
        [
            "",
            "    def _drop(self, k: str) -> None:",
            "        self.items.pop(k, None)",
            "",
            "    def drain(self) -> None:",
        ]
        .map(str::to_owned),
    );
    lines.push(format!("        for k in self._w{wrappers}():"));
    lines.push("            self._drop(k)".to_owned());
    assert_eq!(
        iterated_mutations("bounds_wrapper_chain", &(lines.join("\n") + "\n")),
        1
    );
}

/// An edit through the language server changing every method of a class of five thousand
/// methods (and adding a blocking call to an `async def`, whose `EFF101` report shows the edit
/// was checked): which class families read the changed methods' summaries is decided once per
/// class, not by reading the class's methods again for each changed method.
#[test]
fn an_edit_changing_thousands_of_methods_reads_each_class_once() {
    let methods = 5_000;
    let module = |changed: bool| {
        let mut lines = vec![
            "import time".to_owned(),
            String::new(),
            String::new(),
            "class Table:".to_owned(),
            "    def __init__(self) -> None:".to_owned(),
            "        self.rows: dict[str, int] = {}".to_owned(),
        ];
        for i in 0..methods {
            lines.push(String::new());
            lines.push(format!("    def m{i}(self) -> None:"));
            lines.push(if changed {
                format!("        self.rows[\"k{i}\"] = {i}")
            } else {
                format!("        print({i})")
            });
        }
        if changed {
            lines.extend(
                ["", "", "async def pause() -> None:", "    time.sleep(1)"].map(str::to_owned),
            );
        }
        lines.join("\n") + "\n"
    };
    let p = Project::new("bounds_family_edit");
    p.write("pkg/__init__.py", "")
        .write("pkg/table.py", &module(false));
    let table = uri(&p.dir.join("pkg/table.py"));
    let mut lsp = Lsp::start(&p.dir, &["server", "--stdio"]);
    lsp.request(
        "initialize",
        json!({"processId": null, "rootUri": uri(&p.dir), "capabilities": {}}),
    );
    lsp.notify("initialized", json!({}));
    lsp.ready();
    lsp.open(&table, &module(false));
    lsp.diagnostics(&table, <[Value]>::is_empty);
    let changed = module(true);
    lsp.change(&table, 2, &changed);
    p.write("pkg/table.py", &changed);
    lsp.notify(
        "textDocument/didSave",
        json!({"textDocument": {"uri": table}, "text": changed}),
    );
    // Each wait fails past its time limit.
    let diags = lsp.diagnostics(&table, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["EFF101"]);
    lsp.settle();
    lsp.stop(true);
}

/// The lines of the `MUT113` diagnostics of the project, which must finish within `limit`.
fn mut113_lines(p: &Project, limit: Duration) -> Vec<u64> {
    check_within(p, limit)
        .iter()
        .filter(|d| d["rule_code"] == "MUT113")
        .map(|d| d["line"].as_u64().unwrap())
        .collect()
}

/// Methods that each change a typed dict of `self` thousands of times (`self.rows[str(i)] =
/// i`), the first after binding a new dict, called in a loop over it: the stores of new objects
/// before a change, and the blocks they dominate, are gathered once per function.
#[test]
fn many_changes_of_a_container_of_self_decide_their_rebinding_once() {
    let (methods, writes) = (3, 8_000);
    let mut lines = vec![
        "class Table:".to_owned(),
        "    def __init__(self) -> None:".to_owned(),
        "        self.rows: dict[str, int] = {}".to_owned(),
    ];
    for m in 0..methods {
        lines.push(String::new());
        lines.push(format!("    def fill{m}(self) -> None:"));
        if m == 0 {
            lines.push("        self.rows = {}".to_owned());
        }
        for i in 0..writes {
            lines.push(format!("        self.rows[str({i})] = {i}"));
        }
    }
    lines.push(String::new());
    lines.push("    def refresh(self) -> None:".to_owned());
    lines.push("        for key in self.rows:".to_owned());
    let mut calls = Vec::new();
    for m in 0..methods {
        lines.push(format!("            self.fill{m}()"));
        calls.push(lines.len() as u64);
    }
    let p = Project::new("bounds_self_container_changes");
    p.write("pyproject.toml", MUT113_CONFIG)
        .write("m.py", &(lines.join("\n") + "\n"));
    assert_eq!(mut113_lines(&p, Duration::from_secs(20)), calls[1..]);
}

/// A method storing under a key of a typed dict of `self` after each of hundreds of tests that
/// the key is there (`if key in self.rows: self.rows[key] = value`), called in a loop over it:
/// the sides of the tests are found once per function, and whether one dominates a store by
/// the dominator tree.
#[test]
fn many_key_tests_of_a_container_of_self_find_their_sides_once() {
    let tests = 600;
    let mut lines = vec![
        "class Table:".to_owned(),
        "    def __init__(self) -> None:".to_owned(),
        "        self.rows: dict[str, int] = {}".to_owned(),
        String::new(),
        "    def update(self, key: str, value: int) -> None:".to_owned(),
    ];
    for _ in 0..tests {
        lines.push("        if key in self.rows:".to_owned());
        lines.push("            self.rows[key] = value".to_owned());
    }
    lines.extend(
        [
            "",
            "    def insert(self, key: str, value: int) -> None:",
            "        self.rows[key] = value",
            "",
            "    def refresh(self) -> None:",
            "        for key in self.rows:",
            "            self.update(key, 1)",
            "            self.insert(key, 1)",
        ]
        .map(str::to_owned),
    );
    let p = Project::new("bounds_self_container_key_tests");
    p.write("pyproject.toml", MUT113_CONFIG)
        .write("m.py", &(lines.join("\n") + "\n"));
    assert_eq!(
        mut113_lines(&p, Duration::from_secs(20)),
        vec![lines.len() as u64]
    );
}

/// A module binding thousands of names, each to the one before (`a1 = a0`, `a2 = a1`, ...):
/// every alias is resolved once, and the last still names `object.__setattr__`.
#[test]
fn a_long_chain_of_module_aliases_resolves_each_once() {
    let names = 5_000;
    let mut lines = vec!["a0 = object.__setattr__".to_owned()];
    lines.extend((1..names).map(|i| format!("a{i} = a{}", i - 1)));
    lines.extend(
        [
            "",
            "",
            "class Node:",
            "    def __init__(self) -> None:",
            "        self.kind = \"\"",
            "",
            "",
            "def mark(node: Node) -> None:",
        ]
        .map(str::to_owned),
    );
    lines.push(format!("    a{}(node, \"kind\", \"x\")", names - 1));
    let p = Project::new("bounds_module_alias_chain");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\"]\n\n[[tool.heki.write-owners]]\nname = \"kinds\"\ntargets = [\"Node.kind\"]\nallowed-writers = [\"Node.*\"]\n",
    )
    .write("m.py", &(lines.join("\n") + "\n"));
    let diags = check_within(&p, Duration::from_secs(20));
    assert_eq!(diags.len(), 1, "{diags:#?}");
    assert_eq!(diags[0]["qualname"], "mark");
}

/// A method returning one of thousands of copies of `self.items` behind as many tests: the
/// value each copy copies is found once, not once per return.
#[test]
fn returns_of_a_long_copy_chain_find_its_root_once() {
    let copies = 3_000;
    let mut lines = vec![
        "class Node:".to_owned(),
        "    def __init__(self) -> None:".to_owned(),
        "        self.items: list[int] = []".to_owned(),
        String::new(),
        "    def members(self, flags: list[bool]) -> list[int]:".to_owned(),
        "        c0 = self.items".to_owned(),
    ];
    for i in 1..copies {
        lines.push(format!("        c{i} = c{}", i - 1));
        lines.push(format!("        if flags[{i}]:"));
        lines.push(format!("            return c{i}"));
    }
    lines.push(format!("        return c{}", copies - 1));
    lines.extend(
        [
            "",
            "",
            "def grow(node: Node) -> None:",
            "    node.members([]).append(1)",
        ]
        .map(str::to_owned),
    );
    let p = Project::new("bounds_getter_copy_chain");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\"]\n\n[[tool.heki.write-owners]]\nname = \"items\"\ntargets = [\"Node.items\"]\nallowed-writers = [\"Node.*\"]\n",
    )
    .write("m.py", &(lines.join("\n") + "\n"));
    let diags = check_within(&p, Duration::from_secs(20));
    assert_eq!(diags.len(), 1, "{diags:#?}");
    assert_eq!(diags[0]["qualname"], "grow");
}

/// `classes` classes with a private method, and `functions` functions calling the methods of
/// instances of them `calls` times each.
fn many_calls(classes: usize, functions: usize, calls: usize) -> (String, String) {
    let mut model = Vec::new();
    for c in 0..classes {
        model.extend([
            format!("class C{c}:"),
            "    def _m(self) -> int:".to_owned(),
            "        return 0".to_owned(),
            String::new(),
            String::new(),
        ]);
    }
    let names: Vec<String> = (0..classes).map(|c| format!("C{c}")).collect();
    let params: Vec<String> = (0..classes).map(|c| format!("c{c}: C{c}")).collect();
    let mut usage = vec![
        format!("from pkg.m import {}", names.join(", ")),
        String::new(),
        String::new(),
    ];
    for f in 0..functions {
        usage.push(format!("def f{f}({}) -> None:", params.join(", ")));
        usage.extend((0..calls).map(|k| format!("    c{}._m()", k % classes)));
        usage.extend([String::new(), String::new()]);
    }
    (model.join("\n"), usage.join("\n") + "\n")
}

/// The targets a call matches are looked up from the callee's names, once per call for all
/// `call-owners` contracts: thousands of targets, in one contract or in as many contracts,
/// dotted or sharing one module part (`pkg.m:X0._*`), cost no more per call than one.
#[test]
fn call_targets_are_looked_up_not_tried_one_by_one() {
    let (model, usage) = many_calls(10, 100, 100);
    let mut configs = Vec::new();
    for (form, module) in [("dotted", "pkg.m."), ("module", "pkg.m:")] {
        let targets: Vec<String> = (0..2000)
            .map(|i| format!("\"{module}X{i}._*\""))
            .chain(std::iter::once(format!("\"{module}C0._*\"")))
            .collect();
        let one_contract = format!(
            "[[tool.heki.call-owners]]\nname = \"many\"\ntargets = [{}]\nforbidden-callers = [\"pkg.use.f0\"]\n",
            targets.join(", ")
        );
        let many_contracts: String = targets
            .iter()
            .enumerate()
            .map(|(i, t)| {
                format!("[[tool.heki.call-owners]]\nname = \"c{i}\"\ntargets = [{t}]\nforbidden-callers = [\"pkg.use.f0\"]\n\n")
            })
            .collect();
        configs.push((format!("{form}_one"), one_contract));
        configs.push((format!("{form}_many"), many_contracts));
    }
    for (name, contracts) in configs {
        let p = Project::new(&format!("bounds_call_targets_{name}"));
        p.write(
            "pyproject.toml",
            &format!(
                "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n{contracts}"
            ),
        )
        .write("pkg/__init__.py", "")
        .write("pkg/m.py", &model)
        .write("pkg/use.py", &usage);
        let diags = check_within(&p, Duration::from_secs(20));
        // Only the forbidden caller's ten calls of `C0._m` are reported.
        assert_eq!(diags.len(), 10, "{name}: {diags:#?}");
        assert!(diags.iter().all(|d| d["qualname"] == "f0"), "{name}");
    }
}

/// Call targets with an argument list naming `targets` distinct parameters, the first `defined`
/// of whose functions exist, and `functions` functions each making `calls` calls of those
/// functions passing the named constant, and as many calls of a function of `wide` parameters
/// that no list names.
fn argument_marks(
    targets: usize,
    defined: usize,
    functions: usize,
    calls: usize,
    wide: usize,
) -> Project {
    let texts: Vec<String> = (0..targets)
        .map(|i| format!("\"pkg.m.f{i}(p{i}=True)\""))
        .collect();
    let mut model: Vec<String> = (0..defined)
        .map(|i| format!("def f{i}(p{i}=False, q=None):\n    return p{i}\n\n"))
        .collect();
    let params: String = (0..wide).map(|j| format!(", b{j}=None")).collect();
    model.push(format!("def g(x{params}):\n    return x\n"));
    let mut usage = vec!["from pkg import m\n\n".to_owned()];
    for f in 0..functions {
        usage.push(format!("def u{f}(x):\n"));
        for k in 0..calls {
            let i = (f * calls + k) % defined;
            usage.push(format!("    m.f{i}(p{i}=True)\n    m.g(x)\n"));
        }
        usage.push("\n\n".to_owned());
    }
    let p = Project::new(&format!("bounds_argument_marks_{targets}"));
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"marks\"\ntargets = [{}]\nforbidden-callers = [\"pkg.use.u0\"]\n",
            texts.join(", ")
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/m.py", &model.join("\n"))
    .write("pkg/use.py", &usage.concat());
    p
}

/// The parameters argument lists name are looked up from the names a call binds: twenty thousand
/// lists, each naming a parameter of its own, add no work per list to twenty thousand calls, of
/// their functions and of a function of many parameters none names.
#[test]
fn argument_lists_are_looked_up_from_the_bound_parameters() {
    let p = argument_marks(20_000, 1_000, 100, 100, 50);
    let diags = check_within(&p, Duration::from_secs(20));
    // Only the forbidden caller's hundred calls passing the constants are reported.
    assert_eq!(diags.len(), 100, "{diags:#?}");
    assert!(diags.iter().all(|d| d["qualname"] == "u0"));
}

/// Thousands of references to a `call-owners` target, each a rebinding of one name that a long
/// chain of phis carries to one `return` (`references = "calls"`): which values reach a use
/// handing them on is found in one walk of the function, not once per reference (a function
/// twice as long passes the IR's size budget).
#[test]
fn references_carried_by_a_long_phi_chain_are_found_in_one_walk() {
    let rebindings = 2000;
    let mut lines = vec![
        "def decide():".to_owned(),
        "    return 1".to_owned(),
        String::new(),
        String::new(),
        "def pick(flags):".to_owned(),
        "    chosen = None".to_owned(),
    ];
    for i in 0..rebindings {
        lines.push(format!("    if flags[{i}]:"));
        lines.push("        chosen = decide".to_owned());
    }
    lines.push("    return chosen".to_owned());
    let p = Project::new("bounds_reference_phi_chain");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"home\"\ntargets = [\"m.decide\"]\nallowed-callers = [\"m.decide\"]\nreferences = \"calls\"\n",
    )
    .write("m.py", &(lines.join("\n") + "\n"));
    let found = check_within(&p, Duration::from_secs(20));
    let handed = found
        .iter()
        .filter(|d| d["rule_code"] == "MUT204" && d["qualname"] == "pick")
        .count();
    assert_eq!(handed, rebindings);
}

/// Puts after each of many branches that may build a new unbounded queue: the queue of each put
/// is a phi of the previous one and a new queue, back to a bounded one.
fn queue_rebindings(steps: usize) -> String {
    let mut lines = vec![
        "import queue".to_owned(),
        String::new(),
        String::new(),
        "async def fill(flags, item):".to_owned(),
        "    q = queue.Queue(maxsize=1)".to_owned(),
    ];
    for i in 0..steps {
        lines.push(format!("    if flags[{i}]:"));
        lines.push("        q = queue.Queue()".to_owned());
        lines.push("    q.put(item)".to_owned());
    }
    lines.join("\n") + "\n"
}

/// The capacity of a function's value is walked once: each put reads what the walks of the
/// puts before it found instead of walking every rebinding again.
#[test]
fn puts_after_many_rebindings_of_a_queue_walk_each_value_once() {
    let p = Project::new("bounds_queue_rebindings");
    p.write("m.py", &queue_rebindings(1200));
    let found = check_within(&p, Duration::from_secs(20));
    assert!(
        found
            .iter()
            .any(|d| d["rule_code"] == "EFF111" && d["qualname"] == "fill"),
        "{found:#?}"
    );
}

/// A chain of helpers each handing its queue parameter to the next, named so that their order
/// is the opposite of the calls': the parameter of each takes what the one calling it binds.
fn queue_forwarding_chain(helpers: usize) -> String {
    let mut lines = vec![
        "import queue".to_owned(),
        String::new(),
        String::new(),
        "def f0(q: queue.Queue) -> None:".to_owned(),
        "    q.put(1)".to_owned(),
    ];
    for i in 1..helpers {
        lines.push(String::new());
        lines.push(String::new());
        lines.push(format!("def f{i}(q: queue.Queue) -> None:"));
        lines.push(format!("    f{}(q)", i - 1));
    }
    lines.extend([
        String::new(),
        String::new(),
        "async def entry() -> None:".to_owned(),
        format!("    f{}(queue.Queue(1))", helpers - 1),
    ]);
    lines.join("\n") + "\n"
}

/// The capacities of parameters are a worklist over what each depends on: a change of one
/// re-reads only the parameters it is passed to, whatever order they are in.
#[test]
fn queue_parameters_along_a_reversed_chain_of_helpers_resolve_in_linear_time() {
    let p = Project::new("bounds_queue_forwarding");
    p.write("m.py", &queue_forwarding_chain(6000));
    let found = check_within(&p, Duration::from_secs(20));
    assert!(
        found
            .iter()
            .any(|d| d["rule_code"] == "EFF111" && d["qualname"] == "entry"),
        "{found:#?}"
    );
}

/// A class whose annotated queue attributes each store the one before, with a method putting on
/// each: the capacity of every attribute names the one before it.
fn queue_attribute_chain(attrs: usize) -> String {
    let mut lines = vec![
        "import queue".to_owned(),
        String::new(),
        String::new(),
        "class Holder:".to_owned(),
    ];
    lines.extend((0..attrs).map(|i| format!("    a{i}: queue.Queue")));
    lines.extend([
        String::new(),
        "    def __init__(self) -> None:".to_owned(),
        "        self.a0 = queue.Queue(1)".to_owned(),
    ]);
    lines.extend((1..attrs).map(|i| format!("        self.a{i} = self.a{}", i - 1)));
    for i in 0..attrs {
        lines.push(String::new());
        lines.push(format!("    def p{i}(self) -> None:"));
        lines.push(format!("        self.a{i}.put(1)"));
    }
    lines.extend([
        String::new(),
        String::new(),
        "async def entry(h: Holder) -> None:".to_owned(),
        format!("    h.p{}()", attrs - 1),
    ]);
    lines.join("\n") + "\n"
}

/// The declarations keep the answer of each attribute and global they resolve: the puts on a
/// long chain of attributes resolve each attribute once, not the chain once per put.
#[test]
fn puts_along_a_chain_of_attributes_resolve_each_attribute_once() {
    let p = Project::new("bounds_queue_attribute_chain");
    p.write("m.py", &queue_attribute_chain(6000));
    let found = check_within(&p, Duration::from_secs(20));
    assert!(
        found
            .iter()
            .any(|d| d["rule_code"] == "EFF111" && d["qualname"] == "entry"),
        "{found:#?}"
    );
}

/// `heki inspect <args> --output-format json` of the project, which must finish within
/// `limit`.
fn inspect_within(p: &Project, args: &[&str], limit: Duration) -> Value {
    let mut child = Command::new(env!("CARGO_BIN_EXE_heki"))
        .arg("inspect")
        .args(args)
        .args(["--output-format", "json"])
        .current_dir(&p.dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
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
            panic!("heki inspect ran past {limit:?}");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(status.code(), Some(0));
    serde_json::from_slice(&reader.join().unwrap()).unwrap()
}

/// A lattice thirty levels deep, two functions a level, each calling both of the level below
/// (2^30 paths up from the bottom): the callers' walk lists each call once.
#[test]
fn callers_of_a_diamond_lattice_are_walked_once_each() {
    let mut lines = vec!["def bottom():".to_owned(), "    return 1".to_owned()];
    let below = |level: usize| {
        if level == 0 {
            vec!["bottom".to_owned()]
        } else {
            vec![format!("a{}", level - 1), format!("b{}", level - 1)]
        }
    };
    for level in 0..30 {
        for side in ["a", "b"] {
            lines.extend([
                String::new(),
                String::new(),
                format!("def {side}{level}():"),
            ]);
            lines.extend(below(level).iter().map(|f| format!("    {f}()")));
        }
    }
    let p = Project::new("bounds_caller_lattice");
    p.write("m.py", &(lines.join("\n") + "\n"));
    let r = inspect_within(
        &p,
        &["m.bottom", "--callers", "--depth", "64"],
        Duration::from_secs(20),
    );
    // Two calls of `bottom`, then four calls into each later level.
    assert_eq!(r["callers"]["count"], 2 + 4 * 29);
    assert_eq!(r["callers"]["truncated"], false);
}

/// Five thousand methods of one class, each calling the next on `self`, the last changing a
/// `self` attribute; the call chain of the change, and the walks to an unresolved call and to
/// a changed module global through as many functions, are walked once.
#[test]
fn chains_down_thousands_of_calls_of_one_module_are_walked_once() {
    let mut lines = vec![
        "G = 0".to_owned(),
        String::new(),
        String::new(),
        "class C:".to_owned(),
        "    def __init__(self):".to_owned(),
        "        self.items = []".to_owned(),
    ];
    for i in 0..4_999 {
        lines.extend([
            String::new(),
            format!("    def m{i}(self):"),
            format!("        self.m{}()", i + 1),
        ]);
    }
    lines.extend([
        String::new(),
        "    def m4999(self):".to_owned(),
        "        self.items.append(1)".to_owned(),
    ]);
    for i in 0..4_999 {
        lines.extend([
            String::new(),
            String::new(),
            format!("def f{i}(x):"),
            format!("    return f{}(x)", i + 1),
        ]);
    }
    lines.extend([
        String::new(),
        String::new(),
        "def f4999(x):".to_owned(),
        "    global G".to_owned(),
        "    G = 1".to_owned(),
        "    return x.anything()".to_owned(),
    ]);
    let p = Project::new("bounds_inspect_chains");
    p.write("m.py", &(lines.join("\n") + "\n"));
    let r = inspect_within(&p, &["m.C.m0"], Duration::from_secs(20));
    let items = &r["changes"]["self_attrs"][0];
    assert_eq!(items["chain"].as_array().unwrap().len(), 4_999);
    let f = inspect_within(&p, &["m.f0"], Duration::from_secs(20));
    assert_eq!(
        f["changes"]["globals"][0]["chain"]
            .as_array()
            .unwrap()
            .len(),
        4_999
    );
    // Impure: the unknown call does not decide the verdict, but `--callers` walks the module.
    assert_eq!(f["purity"]["verdict"], "impure");
    let last = inspect_within(
        &p,
        &["m.f4999", "--callers", "--depth", "100"],
        Duration::from_secs(20),
    );
    assert_eq!(last["callers"]["count"], 100);
}

/// A function choosing one of a thousand module globals before changing it, and one making
/// three thousand calls that each hand a global to a helper changing its argument: the roots of
/// a merged value are kept up to a limit, and the calls' hand-offs are looked up by call.
#[test]
fn shared_state_of_thousands_of_globals_is_walked_once() {
    let mut lines: Vec<String> = (0..1_000).map(|i| format!("G{i} = []")).collect();
    lines.extend([
        String::new(),
        String::new(),
        "def append(xs):".to_owned(),
        "    xs.append(1)".to_owned(),
        String::new(),
        String::new(),
        "def choose(k):".to_owned(),
        "    x = G0".to_owned(),
    ]);
    for i in 1..1_000 {
        lines.extend([format!("    if k == {i}:"), format!("        x = G{i}")]);
    }
    lines.extend([
        "    x.append(1)".to_owned(),
        String::new(),
        String::new(),
        "def hand():".to_owned(),
    ]);
    lines.extend((0..3_000).map(|i| format!("    append(G{})", i % 1_000)));
    let p = Project::new("bounds_shared_state");
    p.write("m.py", &(lines.join("\n") + "\n"));
    let choose = inspect_within(&p, &["m.choose"], Duration::from_secs(20));
    assert_eq!(
        choose["purity"]["because"],
        serde_json::json!(["changes_globals"])
    );
    let hand = inspect_within(&p, &["m.hand"], Duration::from_secs(20));
    // One entry per global, at its first hand-off.
    assert_eq!(hand["changes"]["globals"].as_array().unwrap().len(), 1_000);
}

/// `heki dump-facts` of the project's `m.py`, which must finish within `limit`.
fn dump_facts_within(p: &Project, limit: Duration) -> Value {
    let started = Instant::now();
    let (code, stdout, stderr) = p.heki(&["dump-facts", "m.py"]);
    assert!(
        started.elapsed() < limit,
        "heki dump-facts ran {:?}",
        started.elapsed()
    );
    assert_eq!(code, 0, "{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

/// The path facts of a function decide dominance once per region, over two nodes per block: a
/// `try` body of thousands of calls whose handler returns (thousands of exception edges into one
/// block), loops nested twenty deep with a call at each depth (each asked for every loop around
/// it), many loops one after another, and a long chain of `and` tests of module constants.
#[test]
fn path_facts_decide_each_region_once() {
    let calls = 3000;
    let mut lines = vec![
        "_IS_SYNC = False".to_owned(),
        String::new(),
        "def body(db):".to_owned(),
        "    try:".to_owned(),
    ];
    lines.extend((0..calls).map(|i| format!("        db.fetch({i})")));
    lines.extend(
        [
            "    except Exception:",
            "        return None",
            "    db.done()",
        ]
        .map(str::to_owned),
    );
    lines.push(String::new());
    lines.push("def nested(rows, db):".to_owned());
    for depth in 0..20 {
        let pad = "    ".repeat(depth + 1);
        lines.push(format!("{pad}db.at({depth})"));
        lines.push(format!("{pad}for x{depth} in rows:"));
    }
    lines.push(format!("{}db.inner()", "    ".repeat(21)));
    lines.push(String::new());
    lines.push("def sequential(rows, db):".to_owned());
    for i in 0..1000 {
        lines.push("    for row in rows:".to_owned());
        lines.push(format!("        db.each({i})"));
    }
    lines.push(String::new());
    lines.push("def tests(sock):".to_owned());
    lines.push(format!(
        "    if {}:",
        vec!["not _IS_SYNC"; 500].join(" and ")
    ));
    lines.push("        sock.recv()".to_owned());
    let p = Project::new("bounds_path_facts");
    p.write("m.py", &(lines.join("\n") + "\n"));
    let facts = dump_facts_within(&p, Duration::from_secs(20));
    let call = |qualname: &str, text: &str| -> Value {
        facts["functions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["qualname"] == qualname)
            .unwrap()["calls"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["text"] == text)
            .unwrap()
            .clone()
    };
    let called =
        |qualname: &str, text: &str| call(qualname, text)["on_every_return"]["called"].clone();
    assert_eq!(called("body", "db.fetch(1)"), json!({"known": "not_every"}));
    assert_eq!(called("body", "db.done()"), json!({"known": "not_every"}));
    assert_eq!(called("tests", "sock.recv()"), json!({"known": "every"}));
    let inner = call("nested", "db.inner()")["on_every_iteration"].clone();
    assert_eq!(inner.as_array().unwrap().len(), 20);
    assert_eq!(inner[19]["called"], json!({"known": "every"}));
    assert_eq!(inner[0]["called"], json!({"known": "not_every"}));
    let each = call("sequential", "db.each(999)")["on_every_iteration"][0].clone();
    assert_eq!(each["called"], json!({"known": "every"}));
}

/// Past the budget of the path facts (a loop dispatching on hundreds of `isinstance` tests, each
/// ending in `continue`), whether a length test is on every path to a later loop (past a branch
/// between them) is decided by the dominator tree of every edge: the cap still holds.
#[test]
fn length_caps_past_the_path_facts_budget_read_the_dominator_tree() {
    let mut lines = vec![
        "def spent(pending, rows, db):".to_owned(),
        "    while pending:".to_owned(),
        "        cur = pending.pop()".to_owned(),
    ];
    for i in 0..600 {
        lines.push(format!("        if isinstance(cur, C{i}):"));
        lines.push("            continue".to_owned());
    }
    lines.extend(
        [
            "        db.one(cur)",
            "    if len(rows) > 8:",
            "        raise ValueError(rows)",
            "    if db.ready():",
            "        db.one(None)",
            "    for row in rows:",
            "        db.one(row)",
        ]
        .map(str::to_owned),
    );
    let p = Project::new("bounds_caps_past_budget");
    p.write("m.py", &(lines.join("\n") + "\n"));
    let facts = dump_facts_within(&p, Duration::from_secs(20));
    let spent = &facts["functions"][0];
    let call = spent["calls"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["text"] == "db.one(row)")
        .unwrap();
    assert_eq!(call["on_every_return"]["called"], json!("open"));
    assert_eq!(
        spent["loops"][1]["fixed_trips"],
        json!({"known": {"capped": 8}})
    );
}

/// The origin walk decides each question once: a chain of thousands of wrappers each of the one
/// before, a list filled by appends in each of a thousand loops, and a phi of sixty branches each
/// choosing a different parameter (as many as the IR budget models with their phis).
#[test]
fn item_origins_decide_each_question_once() {
    let wrappers = 3000;
    let mut lines = vec!["def chain(rows):".to_owned(), "    x0 = rows".to_owned()];
    lines.extend((1..=wrappers).map(|i| format!("    x{i} = sorted(x{})", i - 1)));
    lines.push(format!("    for y in x{wrappers}:"));
    lines.push("        pass".to_owned());
    lines.push(String::new());
    lines.push("def filled(rows):".to_owned());
    lines.push("    out = []".to_owned());
    for _ in 0..1000 {
        lines.push("    for r in rows:".to_owned());
        lines.push("        out.append(r)".to_owned());
    }
    lines.push("    for y in out:".to_owned());
    lines.push("        pass".to_owned());
    lines.push(String::new());
    let params: Vec<String> = (0..60).map(|i| format!("p{i}")).collect();
    lines.push(format!("def chosen(flag, {}):", params.join(", ")));
    lines.push("    xs = p0".to_owned());
    for (i, p) in params[1..].iter().enumerate() {
        lines.push(format!("    if flag == {i}:"));
        lines.push(format!("        xs = {p}"));
    }
    lines.push("    for y in xs:".to_owned());
    lines.push("        pass".to_owned());
    let p = Project::new("bounds_item_origins");
    p.write("m.py", &(lines.join("\n") + "\n"));
    let facts = dump_facts_within(&p, Duration::from_secs(20));
    let last_origin = |qualname: &str| -> Value {
        let f = facts["functions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["qualname"] == qualname)
            .unwrap();
        f["loops"].as_array().unwrap().last().unwrap()["origin"].clone()
    };
    let one = |source: &str| json!({"known": [{"source": source, "carries": "items"}]});
    assert_eq!(last_origin("chain"), one("param 0"));
    assert_eq!(last_origin("filled"), one("param 0"));
    assert_eq!(
        last_origin("chosen")["known"].as_array().map(Vec::len),
        Some(60)
    );
}

/// `MUT107` reads what a `__hash__` reads on the instances of each writer's class, following its
/// `self` calls along that class's MRO: a hash calling a long chain of `self` helpers, and
/// hundreds of subclasses each changing the attribute the chain's end reads. The walk runs once per
/// writer class and visits each method once, as the family of each class lists the methods it
/// inherits.
#[test]
fn hash_reads_per_writer_class_visit_each_method_once() {
    let (helpers, classes) = (600, 600);
    let mut lines = vec![
        "class Node:".to_owned(),
        "    def __init__(self):".to_owned(),
        "        self.key = 0".to_owned(),
        String::new(),
        "    def __hash__(self):".to_owned(),
        "        return hash(self.h0())".to_owned(),
    ];
    for i in 0..helpers {
        let next = if i + 1 < helpers {
            format!("self.h{}()", i + 1)
        } else {
            "self.key".to_owned()
        };
        lines.extend([
            String::new(),
            format!("    def h{i}(self):"),
            format!("        return {next}"),
        ]);
    }
    for i in 0..classes {
        lines.extend([
            String::new(),
            String::new(),
            format!("class Sub{i}(Node):"),
            "    def touch(self):".to_owned(),
            "        self.key = 1".to_owned(),
        ]);
    }
    let p = Project::new("bounds_hash_reads_per_class");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"MUT107\"]\n",
    )
    .write("m.py", &(lines.join("\n") + "\n"));
    let found = check_within(&p, Duration::from_secs(20));
    let reported = found.iter().filter(|d| d["rule_code"] == "MUT107").count();
    assert_eq!(reported, classes);
}

/// The implementations of a call are checked over at most 64 classes (the receiver's class and
/// its project subclasses): a receiver class with more subclasses has no complete set of targets,
/// so a call that blocks through every one of them is not established (`EFF111`, not `EFF101`).
#[test]
fn implementations_past_the_class_bound_are_not_complete() {
    let blocking = |subclasses: usize| -> Vec<String> {
        let mut lines = vec![
            "import time".to_owned(),
            String::new(),
            String::new(),
            "class Base:".to_owned(),
            "    def run(self):".to_owned(),
            "        time.sleep(1)".to_owned(),
        ];
        for i in 0..subclasses {
            lines.extend([
                String::new(),
                String::new(),
                format!("class Sub{i}(Base):"),
                "    pass".to_owned(),
            ]);
        }
        lines.extend([
            String::new(),
            String::new(),
            "async def handler(b: Base):".to_owned(),
            "    b.run()".to_owned(),
        ]);
        let p = Project::new(&format!("bounds_dispatch_classes_{subclasses}"));
        p.write("m.py", &(lines.join("\n") + "\n"));
        check_within(&p, Duration::from_secs(20))
            .iter()
            .map(|d| d["rule_code"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(blocking(3), ["EFF101"]);
    assert_eq!(blocking(300), ["EFF111"]);
}

/// A project selecting `EFF109` with the class `C` (a helper `helper` taking `self._lock`, and
/// `count` locks of its own), then the method `body` of `C`.
fn locked_class(name: &str, count: usize, body: Vec<String>) -> Project {
    let mut lines = vec![
        "import threading".to_owned(),
        String::new(),
        String::new(),
        "class C:".to_owned(),
        "    def __init__(self):".to_owned(),
        "        self._lock = threading.Lock()".to_owned(),
    ];
    lines.extend((0..count).map(|i| format!("        self._l{i} = threading.Lock()")));
    lines.extend(
        [
            "",
            "    def helper(self):",
            "        with self._lock:",
            "            pass",
            "",
        ]
        .map(str::to_owned),
    );
    lines.extend(body);
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"EFF109\"]\n",
    )
    .write("m.py", &(lines.join("\n") + "\n"));
    p
}

/// Thousands of tests of `self`'s state before thousands of acquisitions of one lock and as many
/// calls: the tests on every path to each block come from one pass over the dominator tree, not
/// from a walk per acquisition or per call site.
#[test]
fn state_tests_before_many_acquisitions_and_calls_take_one_pass() {
    let n = 2_000;
    let mut body = vec!["    def run(self):".to_owned()];
    for i in 0..n {
        body.push(format!("        if self.f{i}:"));
        body.push("            return".to_owned());
    }
    for _ in 0..n {
        body.push("        with self._lock:".to_owned());
        body.push("            self.helper()".to_owned());
    }
    let p = locked_class("bounds_lock_tests", 0, body);
    let found = check_within(&p, Duration::from_secs(20));
    assert_eq!(
        found.iter().filter(|d| d["rule_code"] == "EFF109").count(),
        n
    );
}

/// Thousands of locks held at once over thousands of blocks: a block's state is a bit per
/// acquisition of the function.
#[test]
fn many_locks_held_at_once_keep_a_bit_each() {
    let n = 2_000;
    let mut body = vec!["    def run(self, xs):".to_owned()];
    body.extend((0..n).map(|i| format!("        self._l{i}.acquire()")));
    body.push("        self._lock.acquire()".to_owned());
    for i in 0..n {
        body.push(format!("        if xs[{i}]:"));
        body.push("            self.helper()".to_owned());
    }
    let p = locked_class("bounds_many_locks", n, body);
    let found = check_within(&p, Duration::from_secs(20));
    assert_eq!(
        found.iter().filter(|d| d["rule_code"] == "EFF109").count(),
        n
    );
}

/// A project selecting `EFF110` with the module `m` of `lines` after a coroutine `load` using a
/// session one operation at a time.
fn fanout_module(name: &str, lines: Vec<String>) -> Project {
    let mut all = vec![
        "import asyncio".to_owned(),
        String::new(),
        "from sqlalchemy.ext.asyncio import AsyncSession".to_owned(),
        String::new(),
        String::new(),
        "async def load(session: AsyncSession, reader: asyncio.StreamReader):".to_owned(),
        "    await reader.readline()".to_owned(),
        "    return await session.execute(\"select 1\")".to_owned(),
        String::new(),
        String::new(),
    ];
    all.extend(lines);
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\nrequires-python = \">=3.12\"\n\n[tool.heki.lint]\nextend-select = [\"EFF110\"]\n",
    )
    .write("m.py", &(all.join("\n") + "\n"));
    p
}

/// Thousands of fan-outs of one function on one session: whether the session is used before each
/// comes from one dataflow per object, not one per fan-out.
#[test]
fn many_fanouts_of_one_session_decide_it_once() {
    let n = 2_000;
    let mut lines =
        vec!["async def run(session: AsyncSession, xs, reader: asyncio.StreamReader):".to_owned()];
    for _ in 0..n {
        lines.push("    await asyncio.gather(load(session, None), load(session, None))".to_owned());
    }
    let p = fanout_module("bounds_many_fanouts", lines);
    let found = check_within(&p, Duration::from_secs(20));
    // The first fan-out can be the session's first operation; after it, every path has used it.
    assert_eq!(
        found.iter().filter(|d| d["rule_code"] == "EFF110").count(),
        1
    );
}

/// Hundreds of fan-outs of one function, each of a comprehension the function builds itself
/// (Python 3.12): what each list holds comes from one scan of the function, not one per fan-out.
/// Each comprehension is a loop the summaries model too, so the count stays where a debug build
/// runs the whole check well within the limit.
#[test]
fn many_comprehension_fanouts_scan_the_function_once() {
    let n = 500;
    let mut lines =
        vec!["async def run(session: AsyncSession, xs, reader: asyncio.StreamReader):".to_owned()];
    for _ in 0..n {
        lines.push("    await asyncio.gather(*[load(session, x) for x in xs])".to_owned());
    }
    let p = fanout_module("bounds_many_comprehension_fanouts", lines);
    let found = check_within(&p, Duration::from_secs(20));
    assert_eq!(
        found.iter().filter(|d| d["rule_code"] == "EFF110").count(),
        1
    );
}

/// Thousands of tasks a task group starts in one loop: whether a value is the loop's own comes
/// from one scan of the loop.
#[test]
fn many_tasks_of_one_loop_scan_it_once() {
    let n = 2_000;
    let mut lines = vec![
        "async def run(session: AsyncSession, xs, reader: asyncio.StreamReader):".to_owned(),
        "    async with asyncio.TaskGroup() as tg:".to_owned(),
        "        for x in xs:".to_owned(),
    ];
    for i in 0..n {
        lines.push(format!("            r{i} = x.r{i}"));
        lines.push(format!("            tg.create_task(load(r{i}, r{i}))"));
    }
    let p = fanout_module("bounds_many_tasks", lines);
    assert!(
        check_within(&p, Duration::from_secs(20))
            .iter()
            .all(|d| d["rule_code"] != "EFF110")
    );
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

/// A package function holding its `__path__` through a chain of merges, each merging the one
/// before with a new read of it and counting an item of it, ending with `end`.
fn path_alias_chain(aliases: usize, end: &str) -> String {
    let mut lines = vec![
        "def scan(flag, x):".to_owned(),
        "    p0 = __path__".to_owned(),
    ];
    for i in 1..=aliases {
        lines.push(format!("    p{i} = p{} if flag else __path__", i - 1));
        lines.push(format!("    c{i} = p{i}.count(x)"));
    }
    lines.push(format!("    {end}"));
    lines.join("\n") + "\n"
}

/// What the uses of a package's `__path__` do to it is decided once per value holding it: a
/// function holding it through as many merges as the IR budget models only reads it, or inserts
/// a directory before its own at the end, which takes `f"pkg.{name}"` outside the project.
#[test]
fn the_values_holding_a_package_path_are_each_decided_once() {
    for (end, reported) in [
        ("return c1".to_owned(), false),
        ("p1500.insert(0, x)".to_owned(), true),
    ] {
        let p = Project::new(&format!("bounds_package_path_{reported}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"EFF201\"]\n\n[[tool.heki.effect-boundaries]]\nname = \"env\"\nscopes = [\"probe.*\"]\nforbid = [\"reads-environment\"]\n",
        )
        .write("pkg/__init__.py", &path_alias_chain(1_500, &end))
        .write(
            "probe.py",
            "from importlib import import_module\n\n\ndef load(name):\n    return import_module(f\"pkg.{name}\")\n",
        );
        let found = check_within(&p, Duration::from_secs(20));
        assert_eq!(
            found.iter().any(|d| d["rule_code"] == "EFF201"),
            reported,
            "{found:?}"
        );
    }
}

/// Functions of one module each merging the `__path__` of two thousand packages through a chain
/// of conditional aliases (`v1 = v0 if flag else requests.p1.__path__`), the first chain starting
/// at a package of the project and inserting a directory before its own at the end.
fn merged_package_paths(functions: usize, packages: usize) -> String {
    let mut lines = vec!["import requests".to_owned(), "import pkg.plug".to_owned()];
    for f in 0..functions {
        let first = if f == 0 {
            "pkg.plug".to_owned()
        } else {
            format!("requests.f{f}_0")
        };
        lines.extend([
            String::new(),
            String::new(),
            format!("def merge{f}(flag):"),
            format!("    v0 = {first}.__path__"),
        ]);
        for i in 1..=packages {
            lines.push(format!(
                "    v{i} = v{} if flag else requests.f{f}_{i}.__path__",
                i - 1
            ));
        }
        lines.push(if f == 0 {
            format!("    v{packages}.insert(0, flag)")
        } else {
            format!("    return len(v{packages})")
        });
    }
    lines.join("\n") + "\n"
}

/// The values holding the paths of many packages are decided once for all of them: each
/// package's values reach every merge after its own, which walked once per package took time
/// quadratic in the packages of a function. The project's package at the head of the first chain
/// is still found changed.
#[test]
fn the_paths_of_many_packages_merged_in_one_function_are_decided_once() {
    let p = Project::new("bounds_merged_package_paths");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"EFF201\"]\n\n[[tool.heki.effect-boundaries]]\nname = \"env\"\nscopes = [\"probe.*\"]\nforbid = [\"reads-environment\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write("pkg/plug/__init__.py", "")
    .write("pkg/boot.py", &merged_package_paths(8, 2_000))
    .write("clean/__init__.py", "")
    .write(
        "probe.py",
        "from importlib import import_module\n\n\ndef plugin(name):\n    return import_module(f\"pkg.plug.{name}\")\n\n\ndef unrelated(name):\n    return import_module(f\"clean.{name}\")\n",
    );
    let found = check_within(&p, Duration::from_secs(10));
    assert_eq!(
        found
            .iter()
            .filter(|d| d["rule_code"] == "EFF201")
            .map(|d| d["qualname"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["plugin"]
    );
}

/// A function binding a long chain of aliases of a class and a long chain of attribute loads
/// from a parameter, then deriving classes from the last of each (`class C0(a8000)`, `class
/// D0(b8000)`): each base is followed along its copies through one table per function and through
/// a bounded number of attribute loads, so the classes of the alias derive from the class and
/// those of the attribute from an unknown base.
#[test]
fn class_statements_deriving_from_long_chains_name_each_base_in_bounded_steps() {
    let (links, classes) = (8_000, 200);
    let mut lines = vec![
        "class K:".to_owned(),
        "    pass".to_owned(),
        String::new(),
        String::new(),
        "def build(node):".to_owned(),
        "    a0 = K".to_owned(),
        "    b0 = node".to_owned(),
    ];
    lines.extend((1..=links).map(|i| format!("    a{i} = a{}", i - 1)));
    lines.extend((1..=links).map(|i| format!("    b{i} = b{}.child", i - 1)));
    for i in 0..classes {
        lines.push(format!("    class C{i}(a{links}):"));
        lines.push("        pass".to_owned());
        lines.push(format!("    class D{i}(b{links}):"));
        lines.push("        pass".to_owned());
    }
    lines.push("    return C0(), D0()".to_owned());
    let p = Project::new("bounds_class_base_chains");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"home\"\ntargets = [\"m.K\"]\nallowed-callers = [\"m.K.*\"]\n",
    )
    .write("m.py", &(lines.join("\n") + "\n"));
    let found: Vec<String> = check_within(&p, Duration::from_secs(20))
        .iter()
        .map(|d| d["message"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        found,
        vec!["`build` constructs `m.K` outside the call owners of `home`"]
    );
}
