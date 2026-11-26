//! The size of a function's IR, end to end through `heki dump-ir`: code no path reaches, `with`
//! exits and `finally` bodies copied once per way out, and the budget past which a function is
//! left unmodeled.

mod common;

use common::Project;
use serde_json::Value;

/// `heki dump-ir` of the single module `source`.
fn dump_ir(name: &str, source: &str) -> Value {
    let p = Project::new(name);
    p.write("m.py", source);
    let (code, stdout, stderr) = p.heki(&["dump-ir", "m.py"]);
    assert_eq!(code, 0, "{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

fn functions(ir: &Value) -> &Vec<Value> {
    ir["functions"].as_array().unwrap()
}

fn function<'a>(ir: &'a Value, qualname: &str) -> &'a Value {
    functions(ir)
        .iter()
        .find(|f| f["qualname"] == qualname)
        .unwrap_or_else(|| panic!("no function `{qualname}`"))
}

/// Blocks, phis, and instructions of `f`.
fn units(f: &Value) -> usize {
    f["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| 1 + b["phis"].as_array().unwrap().len() + b["insts"].as_array().unwrap().len())
        .sum()
}

fn hazards(f: &Value) -> Vec<&str> {
    f["a1_hazards"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["kind"].as_str().unwrap())
        .collect()
}

// ============================================================================ dead code

/// A loop no path enters has no join: a name read after it is found without walking its
/// cycle forever.
#[test]
fn a_loop_no_path_reaches_has_a_small_ir() {
    let src = "\
import threading

LOCK = threading.Lock()


def after_return(xs):
    return 1
    for x in xs:
        pass
    print(xs)


def finally_after_returns(xs):
    with LOCK:
        if xs:
            return 1
        try:
            return g()
        finally:
            for x in xs:
                h(x)
            if xs:
                k()
            else:
                m()
";
    let ir = dump_ir("ir_size_dead_loops", src);
    for qualname in ["after_return", "finally_after_returns"] {
        let f = function(&ir, qualname);
        assert!(hazards(f).is_empty(), "{qualname}: {:?}", hazards(f));
        assert!(units(f) < 200, "{qualname}: {} units", units(f));
    }
    let p = Project::new("ir_size_dead_loops_check");
    p.write("m.py", src);
    let (code, _, stderr) = p.check(&["--extend-select", "EFF105,EFF106,EFF107,MUT106"]);
    assert_eq!(code, 0, "{stderr}");
}

/// The import-isolation pattern: a lock held around an early return, and a `finally` that
/// restores the import state with a generator expression and a loop.
#[test]
fn a_finally_body_after_early_returns_is_lowered_once_per_way_out() {
    let src = "\
import sys
import threading
from pathlib import Path

STATE_LOCK = threading.Lock()
_MISSING = object()


def load_isolated(sources, kind, strategy=None):
    with STATE_LOCK:
        entry = str(Path.cwd())
        added = entry not in sys.path
        if not added:
            return load(sources, kind, strategy)

        directory = Path(entry).resolve()
        cached = sys.path_importer_cache.get(entry, _MISSING)
        baseline = sys.modules.copy()
        sys.path.insert(0, entry)
        try:
            return load(sources, kind, strategy)
        finally:
            current = sys.modules.copy()
            local_names = sorted(
                (
                    name
                    for name, module in current.items()
                    if name not in baseline and from_directory(module, directory)
                ),
                key=depth,
                reverse=True,
            )
            for name in local_names:
                remove_module(name, current[name])
            remove_path(entry)
            if cached is _MISSING:
                sys.path_importer_cache.pop(entry, None)
            else:
                sys.path_importer_cache[entry] = cached


def load(sources, kind, strategy):
    return sources, kind, strategy


def from_directory(module, directory):
    return getattr(module, \"__file__\", None) == directory


def depth(name):
    return name.count(\".\")


def remove_module(name, module):
    sys.modules.pop(name, None)


def remove_path(entry):
    sys.path.remove(entry)
";
    let ir = dump_ir("ir_size_isolated_load", src);
    let f = function(&ir, "load_isolated");
    assert!(hazards(f).is_empty(), "{:?}", hazards(f));
    // The `finally` body runs after the `return`, and when the `try` body raises; the
    // normal completion no path reaches has no copy.
    let removals = f["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|b| b["insts"].as_array().unwrap())
        .filter(|i| i["kind"]["Call"]["callee"] == "remove_path")
        .count();
    assert_eq!(removals, 2);
    assert!(units(f) < 400, "{} units", units(f));
    let p = Project::new("ir_size_isolated_load_check");
    p.write("m.py", src);
    let (code, stdout, stderr) = p.check(&["--extend-select", "EFF105,EFF106,EFF107,MUT106"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
}

// ======================================================================= nested statements

/// A deterministic generator of functions nesting `with`, `try` / `finally`, `try` / `except`,
/// loops, and branches up to a depth, with `return` / `break` / `continue` taken on some or
/// every path (leaving code no path reaches). A `finally`
/// body holds no way out of itself and no `finally` of its own (PEP 765 warns on the first):
/// then each `finally` body is copied once per way out of its `try`, which keeps the IR
/// within a constant times the statements times the frames around them.
struct Shapes {
    state: u64,
    max_depth: usize,
    out: Vec<String>,
    statements: usize,
    frames: usize,
}

#[derive(Clone, Copy)]
struct Context {
    depth: usize,
    /// A `break` / `continue` here stays in a loop of the same `finally` body (or of none).
    in_loop: bool,
    in_finally: bool,
}

impl Shapes {
    fn pick(&mut self, n: usize) -> usize {
        self.state = self
            .state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.state >> 33) % n as u64) as usize
    }

    fn suite(&mut self, cx: Context, pad: &str) {
        for _ in 0..=self.pick(2) {
            self.statement(cx, pad);
        }
    }

    fn statement(&mut self, cx: Context, pad: &str) {
        self.statements += 1;
        let mut kinds = vec!["call", "assign"];
        if cx.depth < self.max_depth {
            for _ in 0..2 {
                kinds.extend(["with", "try_except", "for", "while", "if"]);
                if !cx.in_finally {
                    kinds.push("try_finally");
                }
            }
        }
        if cx.in_loop {
            kinds.extend(["break", "continue"]);
        }
        if !cx.in_finally {
            kinds.push("return");
        }
        let kind = kinds[self.pick(kinds.len())];
        let inner = format!("{pad}    ");
        let nested = Context {
            depth: cx.depth + 1,
            ..cx
        };
        let n = self.pick(3);
        match kind {
            "call" => self.out.push(format!("{pad}f{n}(x{n})")),
            "assign" => {
                let m = self.pick(3);
                self.out.push(format!("{pad}x{n} = g(x{m})"));
            }
            "break" | "continue" | "return" => {
                let exit = if kind == "return" { "return x2" } else { kind };
                // An exit taken on every path leaves the rest of its suite dead.
                if n == 0 {
                    self.out.push(format!("{pad}{exit}"));
                } else {
                    self.out.push(format!("{pad}if x{n}:\n{inner}{exit}"));
                }
            }
            "with" => {
                self.frames += 1;
                self.out.push(format!("{pad}with LOCK{n}:"));
                self.suite(nested, &inner);
            }
            "try_finally" => {
                self.frames += 1;
                self.out.push(format!("{pad}try:"));
                self.suite(nested, &inner);
                self.out.push(format!("{pad}finally:"));
                let finally = Context {
                    in_loop: false,
                    in_finally: true,
                    ..nested
                };
                self.suite(finally, &inner);
            }
            "try_except" => {
                self.out.push(format!("{pad}try:"));
                self.suite(nested, &inner);
                self.out.push(format!("{pad}except E:"));
                self.suite(nested, &inner);
            }
            "for" | "while" => {
                let header = if kind == "for" {
                    "for x2 in x0:"
                } else {
                    "while x1:"
                };
                self.out.push(format!("{pad}{header}"));
                let body = Context {
                    in_loop: true,
                    ..nested
                };
                self.suite(body, &inner);
            }
            _ => {
                self.out.push(format!("{pad}if x{n}:"));
                self.suite(nested, &inner);
                self.out.push(format!("{pad}else:"));
                self.suite(nested, &inner);
            }
        }
    }
}

#[test]
fn nested_exits_and_frames_keep_the_ir_linear() {
    for seed in 1..=4u64 {
        let mut shapes = Shapes {
            state: seed,
            max_depth: 6,
            out: Vec::new(),
            statements: 0,
            frames: 0,
        };
        let mut bounds = Vec::new();
        for i in 0..40 {
            shapes.statements = 0;
            shapes.frames = 0;
            shapes.out.push(format!("def case{i}(x0, x1, x2):"));
            let top = Context {
                depth: 0,
                in_loop: false,
                in_finally: false,
            };
            shapes.suite(top, "    ");
            shapes.out.push(String::new());
            bounds.push((shapes.statements, shapes.frames));
        }
        let ir = dump_ir(&format!("ir_size_nested_{seed}"), &shapes.out.join("\n"));
        for (i, &(statements, frames)) in bounds.iter().enumerate() {
            let f = function(&ir, &format!("case{i}"));
            assert!(
                hazards(f).is_empty(),
                "seed {seed} case{i}: {:?}",
                hazards(f)
            );
            let bound = 16 * statements * (frames + 1);
            assert!(
                units(f) <= bound,
                "seed {seed} case{i}: {} units, bound {bound}",
                units(f)
            );
        }
    }
}

// ============================================================================== budget

/// `depth` nested `try` statements, each returning early, so each `finally` body is copied
/// three times per copy of the one around it.
fn nested_finally(depth: usize) -> String {
    let mut lines = vec!["def nested(xs):".to_owned()];
    let mut pad = "    ".to_owned();
    for i in 0..depth {
        lines.push(format!("{pad}try:"));
        lines.push(format!("{pad}    if xs:"));
        lines.push(format!("{pad}        return {i}"));
        lines.push(format!("{pad}    g{i}(xs)"));
        lines.push(format!("{pad}finally:"));
        pad.push_str("    ");
    }
    lines.push(format!("{pad}for x in xs:"));
    lines.push(format!("{pad}    h(x)"));
    lines.push("\n\ndef small(xs):\n    return len(xs)\n".to_owned());
    lines.join("\n")
}

#[test]
fn a_function_past_its_ir_budget_is_one_opaque_call() {
    let src = nested_finally(14);
    let ir = dump_ir("ir_size_budget", &src);
    let f = function(&ir, "nested");
    assert_eq!(hazards(f), ["unmodeled"]);
    // The body is one call of an unknown callable with the parameters, whose result returns.
    let blocks = f["blocks"].as_array().unwrap();
    assert_eq!(blocks.len(), 1);
    let insts = blocks[0]["insts"].as_array().unwrap();
    assert_eq!(insts.len(), 2);
    assert_eq!(insts[1]["kind"]["Call"]["args"], serde_json::json!([0]));
    assert!(
        blocks[0]["terminator"]["Return"].is_u64(),
        "{:#}",
        blocks[0]
    );
    // The other functions of the module are modeled.
    assert!(hazards(function(&ir, "small")).is_empty());

    let p = Project::new("ir_size_budget_check");
    p.write("m.py", &src);
    let (code, stdout, stderr) = p.heki(&["assumptions", "."]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("[nested] A1 hazard: unmodeled"), "{stdout}");
    let (code, _, stderr) = p.check(&[]);
    assert_eq!(code, 0, "{stderr}");
}

/// Each block a name's lookup passes takes a unit: a call reading parameters no statement read
/// before walks back through every branch, and the walk itself ends past the budget.
#[test]
fn a_lookup_walk_past_the_ir_budget_leaves_the_function_unmodeled() {
    let params: Vec<String> = (0..200).map(|i| format!("a{i}")).collect();
    let params = params.join(", ");
    let mut lines = vec![format!("def walks(c, {params}):")];
    lines.extend(std::iter::repeat_n(
        "    if c:\n        pass".to_owned(),
        1000,
    ));
    lines.push(format!("    return f({params})"));
    let ir = dump_ir("ir_size_budget_walk", &(lines.join("\n") + "\n"));
    assert_eq!(hazards(function(&ir, "walks")), ["unmodeled"]);
}
