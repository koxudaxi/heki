//! The cost of the analyses past the IR, end to end through the real binary: `isinstance`
//! narrowing through phis, the passes of the type inference and of the classes of merged values,
//! the closure cells child scopes read, the iterables child scopes receive from the functions
//! creating them, the fixpoint joining the stores of attributes, the names its functions store
//! and read, the stores through namespaces and `setattr`, the classes whose code may store into
//! an instance, the size of the Soundness lane's proof state, the values `EFF107` follows from a
//! task factory, the walk to the callable a builtin calls once per item, contracts with many call
//! targets and argument lists, the mappings a call unpacks, the walk to the uses handing a
//! reference to a call target on, the locks `EFF109` follows, the values class statements pass as
//! bases, and the bindings the deque rewrite's candidates check.
//!
//! A shape's cost is counted, not timed: `heki check --walk-steps` writes the steps of each
//! owner's walks (as `scripts/check_walk_shapes.py` reads them), and a shape is compared with the
//! same shape `SCALE` times smaller in the same test. Counts do not depend on the machine's
//! speed or load, so they are compared exactly: the steps grow at most about linearly, and no
//! owner spends its budget on the full shape unless the test names it. The only time limit is
//! `common::HANG`, past which a run has hung.

mod common;

use common::lsp::{Lsp, codes, uri};
use common::{Project, wait_or_hang};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;
use std::process::Stdio;

/// How many times smaller the shape is that a shape's steps are compared with. Linear walks take
/// `SCALE` times the steps on the full shape, quadratic ones `SCALE²` times.
const SCALE: usize = 8;

/// The most the steps of an owner's walks may grow from the shape `SCALE` times smaller to the
/// shape, as `scripts/check_walk_shapes.py` bounds them from half the size to the size: about
/// `n^1.33`.
const MAX_STEP_GROWTH: f64 = 2.0 * SCALE as f64;

/// The smallest budget of an owner (64 steps per IR unit, at least 4,096): walks that take fewer
/// steps on the full shape are cheap at any size, and may well grow faster than it.
const MIN_BOUNDED_STEPS: u64 = 4096;

/// The output of `heki <args>` in the project, which must exit with one of `codes`.
fn run(p: &Project, args: &[&str], codes: &[i32]) -> Vec<u8> {
    let mut child = common::heki_command()
        .args(args)
        .current_dir(&p.dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // Read while it runs: output larger than a pipe would otherwise block it.
    let read = |mut pipe: Box<dyn Read + Send>| {
        std::thread::spawn(move || {
            let mut out = Vec::new();
            pipe.read_to_end(&mut out).unwrap();
            out
        })
    };
    let stdout = read(Box::new(child.stdout.take().unwrap()));
    let stderr = read(Box::new(child.stderr.take().unwrap()));
    let status = wait_or_hang(&mut child, &format!("heki {args:?}"));
    let stderr = String::from_utf8_lossy(&stderr.join().unwrap()).into_owned();
    assert!(
        status.code().is_some_and(|c| codes.contains(&c)),
        "heki {args:?}: {status:?}\n{stderr}"
    );
    stdout.join().unwrap()
}

/// The JSON `heki <args>` prints in the project, for the commands that count no walk steps
/// (`inspect`, `dump-facts`, `dump-open-premises`).
fn heki_json(p: &Project, args: &[&str]) -> Value {
    serde_json::from_slice(&run(p, args, &[0])).unwrap()
}

/// `heki check --output-format json` of the project, run once without counting: a shape that
/// guards only against a run that never ends, or a size too small to count that a test checks
/// next to a full one.
fn check(p: &Project) -> Vec<Value> {
    let out = run(
        p,
        &["check", "--heuristic", "--output-format", "json", "."],
        &[0, 1],
    );
    serde_json::from_slice(&out).unwrap()
}

/// The steps an owner's walks took in one run, summed over its budgets: of every function, or of
/// one.
#[derive(Debug, Default, Clone, Copy)]
struct Steps {
    taken: u64,
    /// The sum of the budgets' limits (an unmetered budget counts what it took).
    limits: u64,
    spent: bool,
}

/// The steps of each owner, of all its functions (`None`) and of each function by qualified name
/// (an unmetered budget, over a whole rule run, under the empty name).
type StepsByOwner = BTreeMap<(String, Option<String>), Steps>;

/// `heki check --output-format json --walk-steps` of the project: its diagnostics and the steps
/// of its walks.
fn check_counted(p: &Project) -> (Vec<Value>, StepsByOwner) {
    let path = p.dir.join(".walk-steps.json");
    let out = run(
        p,
        &[
            "check",
            "--heuristic",
            "--output-format",
            "json",
            "--walk-steps",
            path.to_str().unwrap(),
            ".",
        ],
        &[0, 1],
    );
    (serde_json::from_slice(&out).unwrap(), walk_steps(&path))
}

fn walk_steps(path: &Path) -> StepsByOwner {
    let records: Vec<Value> =
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    let mut out = StepsByOwner::new();
    for r in &records {
        let owner = r["owner"].as_str().unwrap().to_owned();
        let taken = r["taken"].as_u64().unwrap();
        let limit = r["limit"].as_u64().unwrap_or(taken);
        let function = r["function"].as_str().unwrap_or_default().to_owned();
        for key in [(owner.clone(), None), (owner, Some(function))] {
            let steps = out.entry(key).or_default();
            steps.taken += taken;
            steps.limits += limit;
            steps.spent |= r["spent"].as_bool().unwrap();
        }
    }
    out
}

/// The owners and functions whose walks take more than [`MAX_STEP_GROWTH`] times the steps from
/// `small` to `large`. Where the large run spends a budget, its steps are those of the budget,
/// proportional to the function: the limits are compared, which grow faster when an owner makes
/// a budget per item of the function instead of one for all.
fn step_growth(small: &StepsByOwner, large: &StepsByOwner) -> Vec<String> {
    let mut out = Vec::new();
    for (key, l) in large {
        let Some(s) = small.get(key) else {
            continue;
        };
        if l.taken < MIN_BOUNDED_STEPS {
            continue;
        }
        let (before, after) = if l.spent {
            (s.limits, l.limits)
        } else {
            (s.taken, l.taken)
        };
        let growth = after as f64 / before.max(1) as f64;
        if growth > MAX_STEP_GROWTH {
            out.push(format!(
                "{key:?}: {before} steps at size / {SCALE}, {after} at size ({growth:.1} times)"
            ));
        }
    }
    out
}

/// `heki check --output-format json` of `shape(n)`, after checking its walks against those of
/// `shape(n / SCALE)`: each owner's steps grow at most [`MAX_STEP_GROWTH`] times, and no owner
/// spends its budget on the full shape.
fn check_linear(n: usize, shape: impl Fn(usize) -> Project) -> Vec<Value> {
    check_past_budgets(n, &[], shape)
}

/// [`check_linear`] of a shape made to pass the budgets of `owners`, which its full size spends:
/// a walk past its budget answers open instead of walking on. An owner spending its budget
/// where no test expects it is a walk that grew faster than the function.
fn check_past_budgets(n: usize, owners: &[&str], shape: impl Fn(usize) -> Project) -> Vec<Value> {
    let (small, large) = (shape(n / SCALE), shape(n));
    let (_, small_steps) = check_counted(&small);
    let (diags, large_steps) = check_counted(&large);
    let grown = step_growth(&small_steps, &large_steps);
    assert!(
        grown.is_empty(),
        "the walks take steps growing faster than the shape:\n{}",
        grown.join("\n")
    );
    let spent: Vec<&str> = large_steps
        .iter()
        .filter(|((_, function), steps)| function.is_none() && steps.spent)
        .map(|((owner, _), _)| owner.as_str())
        .collect();
    let mut expected = owners.to_vec();
    expected.sort_unstable();
    assert_eq!(
        spent, expected,
        "the owners spending their budgets on the full shape"
    );
    diags
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
    check_linear(32, |checks| {
        let p = Project::new(&format!("bounds_narrowing_{checks}"));
        p.write("m.py", &narrowing_loop(checks));
        p
    });
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
    let shape = |lines: usize| {
        let p = Project::new(&format!("bounds_state_{lines}"));
        p.write("m.py", &accumulator_after(lines));
        p
    };
    let opt = |found: Vec<Value>| found.iter().filter(|d| d["rule_code"] == "OPT101").count();
    assert_eq!(opt(check(&shape(2))), 1);
    assert_eq!(opt(check_linear(200, shape)), 0);
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
    let shape = |links: usize| {
        let p = Project::new(&format!("bounds_passes_{links}"));
        p.write("m.py", &chain_before_blocking_call(links));
        p
    };
    let blocking = |found: Vec<Value>| found.iter().filter(|d| d["rule_code"] == "EFF101").count();
    assert_eq!(blocking(check(&shape(4))), 1);
    assert_eq!(blocking(check_linear(30, shape)), 0);
}

/// A class whose `__init__` binds an attribute to a client, then `links` attributes in turn each
/// to the one before, and an `async def` calling the client's blocking method through the last.
fn attribute_chain_before_blocking_call(links: usize) -> String {
    let mut out = vec![
        "import httpx".to_owned(),
        String::new(),
        String::new(),
        "class Holder:".to_owned(),
        "    def __init__(self):".to_owned(),
        "        self.a0 = httpx.Client()".to_owned(),
    ];
    for k in 1..=links {
        out.push(format!("        self.a{k} = self.a{}", k - 1));
    }
    out.push(String::new());
    out.push("    async def handle(self):".to_owned());
    out.push(format!(
        "        self.a{links}.get(\"https://example.com\")"
    ));
    out.join("\n") + "\n"
}

/// The types of a module's attributes are a fixpoint over the functions storing them, each
/// inferred again a bounded number of times when an attribute it reads grew: past the bound the
/// stores reading attributes bind values of unknown type, so the receiver of the call is unknown
/// and nothing is reported.
#[test]
fn attributes_past_the_store_join_rounds_are_unknown() {
    let shape = |links: usize| {
        let p = Project::new(&format!("bounds_store_rounds_{links}"));
        p.write("m.py", &attribute_chain_before_blocking_call(links));
        p
    };
    let blocking = |found: Vec<Value>| found.iter().filter(|d| d["rule_code"] == "EFF101").count();
    assert_eq!(blocking(check(&shape(2))), 1);
    assert_eq!(blocking(check_linear(30, shape)), 0);
}

/// `classes` classes whose `__init__` binds an attribute to a client and another to the first,
/// and whose `reset` stores both again, one from what the other holds: each method reads names
/// being decided, so each is a function of the fixpoint. Each class's `async def` calls the
/// client's blocking method through the second attribute.
fn classes_storing_what_they_read(classes: usize) -> String {
    let mut out = vec!["import httpx".to_owned(), String::new()];
    for c in 0..classes {
        out.extend([
            format!("class Holder{c}:"),
            "    def __init__(self):".to_owned(),
            "        self.client = httpx.Client()".to_owned(),
            "        self.active = self.client".to_owned(),
            String::new(),
            "    def reset(self):".to_owned(),
            "        self.items = [self.active]".to_owned(),
            "        self.active = self.client".to_owned(),
            String::new(),
            "    async def handle(self):".to_owned(),
            "        self.active.get(\"https://example.com\")".to_owned(),
            String::new(),
        ]);
    }
    out.join("\n") + "\n"
}

/// The fixpoint over a module's storing functions takes steps in proportion to them: a visit
/// asks the reads of the function's last inference again, and infers it again only when one
/// gives something else.
#[test]
fn store_types_take_steps_linear_in_the_storing_functions() {
    let shape = |classes: usize| {
        let p = Project::new(&format!("bounds_store_fixpoint_{classes}"));
        p.write("m.py", &classes_storing_what_they_read(classes));
        p
    };
    let found = check_linear(400, shape);
    let blocking: Vec<&Value> = found
        .iter()
        .filter(|d| d["rule_code"] == "EFF101")
        .collect();
    assert_eq!(blocking.len(), 1);
    let message = blocking[0]["message"].as_str().unwrap();
    assert!(message.contains("(+399 more async call sites"), "{message}");
}

/// A class whose `__init__` binds an attribute to a client and makes `stores` stores of other
/// attributes, each one `store(k)` writes, and an `async def` calling the client's blocking
/// method.
fn many_attribute_stores(stores: usize, store: impl Fn(usize) -> String) -> String {
    let mut out = vec![
        "import httpx".to_owned(),
        String::new(),
        String::new(),
        "class Holder:".to_owned(),
        "    def __init__(self):".to_owned(),
        "        self.client = httpx.Client()".to_owned(),
    ];
    out.extend((0..stores).map(|k| format!("        {}", store(k))));
    out.push(String::new());
    out.push("    async def handle(self):".to_owned());
    out.push("        self.client.get(\"https://example.com\")".to_owned());
    out.join("\n") + "\n"
}

/// The stores through an instance's namespace are found in one pass over the function: the
/// aliases of every namespace it loads, and the names their keys hold from the function's table
/// of definitions. Stores of other attributes keep the client's type.
#[test]
fn stores_through_namespaces_are_found_in_one_pass() {
    let shape = |stores: usize| {
        let p = Project::new(&format!("bounds_namespace_stores_{stores}"));
        let text = many_attribute_stores(stores, |k| format!("self.__dict__[\"a{k}\"] = {k}"));
        p.write("m.py", &text);
        p
    };
    let blocking = |found: Vec<Value>| found.iter().filter(|d| d["rule_code"] == "EFF101").count();
    assert_eq!(blocking(check_linear(2000, shape)), 1);
}

/// The names `setattr` stores are read from the function's table of definitions, each in a
/// constant number of steps.
#[test]
fn names_setattr_stores_are_read_from_a_table() {
    let shape = |stores: usize| {
        let p = Project::new(&format!("bounds_setattr_stores_{stores}"));
        let text = many_attribute_stores(stores, |k| format!("setattr(self, \"a{k}\", {k})"));
        p.write("m.py", &text);
        p
    };
    let blocking = |found: Vec<Value>| found.iter().filter(|d| d["rule_code"] == "EFF101").count();
    assert_eq!(blocking(check_linear(2000, shape)), 1);
}

/// A class whose `__setstate__` hands its state through `copies` copies to `stores` stores under
/// a name the code does not give, then pads and (with `namespace`) writes through its namespace;
/// its `handle` calls a blocking method on the client `__init__` binds.
fn restoring_class(
    name: &str,
    copies: usize,
    stores: usize,
    padding: usize,
    namespace: bool,
) -> String {
    let mut lines = vec![
        format!("class {name}:"),
        "    def __init__(self):".to_owned(),
        "        self.client = httpx.Client()".to_owned(),
        String::new(),
        "    def __setstate__(self, state, key):".to_owned(),
        "        s0 = state".to_owned(),
    ];
    lines.extend((1..=copies).map(|i| format!("        s{i} = s{}", i - 1)));
    lines.extend((0..stores).map(|_| format!("        setattr(self, key, s{copies})")));
    lines.extend((0..padding).map(|i| format!("        pad{i} = {i}")));
    if namespace {
        lines.push("        ns = self.__dict__".to_owned());
        lines.push("        ns[\"a\"] = 1".to_owned());
    }
    lines.extend(
        [
            "",
            "    async def handle(self):",
            "        self.client.get(\"https://example.com\")",
            "",
            "",
        ]
        .map(str::to_owned),
    );
    lines.join("\n")
}

/// The store budget's steps of `Probe.__setstate__` in a project of one `Probe` class.
fn store_steps(p: &Project, copies: usize, padding: usize, namespace: bool) -> Steps {
    let text = restoring_class("Probe", copies, STORES, padding, namespace);
    p.write("m.py", &format!("import httpx\n\n\n{text}"));
    let (_, steps) = check_counted(p);
    steps[&(
        "store targets".to_owned(),
        Some("Probe.__setstate__".to_owned()),
    )]
}

const STORES: usize = 101;

/// The steps the walk finding the aliases of a namespace takes before it reads their uses.
const WALK_STEPS: i64 = 2;

/// The state copies and pads of a `__setstate__` whose stores leave exactly the steps of that
/// walk, so that the first read of a use of the namespace finds none.
///
/// The steps the stores take grow linearly with the copies, and what a spent budget leaves is
/// read from the steps taken: a walk that finds none counts its own step, so a spent function
/// took `left_after_the_stores + 1` steps more than the stores need. That is measured on
/// functions of three sizes that spend the budget, and the sizes solve for the steps of the walk.
fn copies_and_pads_leaving_the_walk() -> (usize, usize) {
    let p = Project::new("bounds_store_budget_probe");
    let at = |copies, pads| store_steps(&p, copies, pads, true);
    let plain = |copies| store_steps(&p, copies, 60, false).taken as i64;
    let (base, next) = (plain(300), plain(301));
    let stores_taken = |copies: usize| base + (next - base) * (copies as i64 - 300);
    let (copies, pads) = (520_usize, 0_usize);
    let (spent, more_copies, more_pads) =
        (at(copies, pads), at(copies + 1, pads), at(copies, pads + 1));
    assert!(spent.spent && more_copies.spent && more_pads.spent);
    let left = |taken: u64, copies: usize| taken as i64 - stores_taken(copies) - 1;
    let left_here = left(spent.taken, copies);
    let per_copy = left(more_copies.taken, copies + 1) - left_here;
    let per_pad = more_pads.taken as i64 - spent.taken as i64;
    assert!(per_copy < 0 && per_pad > 0, "{per_copy} {per_pad}");
    // Copies and pads change the steps left only by multiples of their common factor.
    fn gcd(a: i64, b: i64) -> i64 {
        if b == 0 { a.abs() } else { gcd(b, a % b) }
    }
    let common = gcd(per_copy, per_pad);
    assert_eq!(
        (WALK_STEPS - left_here) % common,
        0,
        "the steps of a copy ({per_copy}) and of a pad ({per_pad}) share the factor {common}, \
         which does not divide the steps of the walk ({WALK_STEPS}) less those left \
         ({left_here}): no size leaves exactly the steps of the walk"
    );
    for fewer in 0..per_pad {
        let left_after = left_here - per_copy * fewer;
        let more_pads = WALK_STEPS - left_after;
        if more_pads < 0 || more_pads % per_pad != 0 {
            continue;
        }
        let found = (
            copies - fewer as usize,
            pads + (more_pads / per_pad) as usize,
        );
        let walk_left = at(found.0, found.1);
        assert!(walk_left.spent, "{walk_left:?}");
        assert_eq!(left(walk_left.taken, found.0), WALK_STEPS);
        return found;
    }
    panic!("no size leaves the steps of the walk: {per_copy} {per_pad}");
}

/// The stores of a function share one budget with the walks that read what they store. Once a
/// `__setstate__` spends it on following its state, later stores and the namespace it writes are
/// stores of unknown names, so no attribute of the class keeps its type: where the stores spend
/// it (`Spent`), and where the first read of the namespace's uses does (`WalkLeft`). A function within it keeps
/// its attributes' types.
#[test]
fn a_function_past_its_store_budget_stores_attributes_of_unknown_names() {
    let (copies, pads) = copies_and_pads_leaving_the_walk();
    let p = Project::new("bounds_store_budget");
    let text = [
        "import httpx\n\n\n".to_owned(),
        restoring_class("Spent", copies, STORES + 20, 0, true),
        restoring_class("WalkLeft", copies, STORES, pads, true),
        restoring_class("Within", 10, 3, 0, true),
    ]
    .concat();
    p.write("m.py", text.trim_end_matches('\n'));
    let (found, steps) = check_counted(&p);
    let spent = |class: &str| {
        steps[&(
            "store targets".to_owned(),
            Some(format!("{class}.__setstate__")),
        )]
            .spent
    };
    assert!(spent("Spent") && spent("WalkLeft") && !spent("Within"));
    let blocking: Vec<&str> = found
        .iter()
        .filter(|d| d["rule_code"] == "EFF101")
        .map(|d| d["qualname"].as_str().unwrap())
        .collect();
    assert_eq!(blocking, vec!["Within.handle"], "{found:#?}");
}

/// A class whose `__init__` binds `pairs` pairs of attributes to clients, a `reset` binding
/// each first one to its second (`self.a0 = self.d0`), and an `async def` calling the client's
/// blocking method through the first; with `touched`, another module stores into `d0`.
fn one_function_storing_what_it_reads(pairs: usize, touched: bool) -> Project {
    let p = Project::new(&format!("bounds_store_dependencies_{pairs}_{touched}"));
    let mut out = vec![
        "import httpx".to_owned(),
        String::new(),
        String::new(),
        "class Holder:".to_owned(),
        "    def __init__(self):".to_owned(),
    ];
    for k in 0..pairs {
        out.push(format!("        self.d{k} = httpx.Client()"));
        out.push(format!("        self.a{k} = httpx.Client()"));
    }
    out.push(String::new());
    out.push("    def reset(self):".to_owned());
    out.extend((0..pairs).map(|k| format!("        self.a{k} = self.d{k}")));
    out.push(String::new());
    out.push("    async def handle(self):".to_owned());
    out.push("        self.a0.get(\"https://example.com\")".to_owned());
    p.write("m.py", &(out.join("\n") + "\n"));
    if touched {
        let other = "import httpx\n\nfrom m import Holder\n\n\ndef touch(holder: Holder):\n    holder.d0 = httpx.Client(timeout=1)\n";
        p.write("other.py", other);
    }
    p
}

/// What the stores of each attribute read is kept once per storing function, not once per
/// attribute: a function storing many attributes, each from another one, keeps its reads once,
/// and the attributes whose types another module's store may change are found in one walk over
/// the functions. The store into `d0` elsewhere makes the types of every attribute `reset`
/// stores those of the first round, which do not type `a0`.
#[test]
fn the_names_a_function_stores_and_reads_are_kept_once() {
    let blocking = |found: Vec<Value>| found.iter().filter(|d| d["rule_code"] == "EFF101").count();
    let alone = |pairs: usize| one_function_storing_what_it_reads(pairs, false);
    assert_eq!(blocking(check_linear(2000, alone)), 1);
    let touched = |pairs: usize| one_function_storing_what_it_reads(pairs, true);
    assert_eq!(blocking(check_linear(2000, touched)), 0);
}

/// A class whose `__init__` binds `links` attributes to clients, a method per attribute after
/// the first binding it to the one before (`def m1(self): self.a1 = self.a0`), and an
/// `async def` calling the client's blocking method through the last.
fn methods_storing_down_a_chain(links: usize) -> String {
    let mut out = vec![
        "import httpx".to_owned(),
        String::new(),
        String::new(),
        "class Holder:".to_owned(),
        "    def __init__(self):".to_owned(),
    ];
    out.extend((0..links).map(|k| format!("        self.a{k} = httpx.Client()")));
    for k in 1..links {
        out.push(String::new());
        out.push(format!("    def m{k}(self):"));
        out.push(format!("        self.a{k} = self.a{}", k - 1));
    }
    out.push(String::new());
    out.push("    async def handle(self):".to_owned());
    out.push(format!(
        "        self.a{}.get(\"https://example.com\")",
        links - 1
    ));
    out.join("\n") + "\n"
}

/// Methods each storing an attribute from the one before keep one dependency each, however
/// long the chain of their reads.
#[test]
fn a_chain_of_stores_across_methods_keeps_one_dependency_per_method() {
    let shape = |links: usize| {
        let p = Project::new(&format!("bounds_store_chain_{links}"));
        p.write("m.py", &methods_storing_down_a_chain(links));
        p
    };
    let blocking = |found: Vec<Value>| found.iter().filter(|d| d["rule_code"] == "EFF101").count();
    assert_eq!(blocking(check_linear(2000, shape)), 1);
}

/// A class whose `__init__` stores an attribute, with a chain of two thousand subclasses, and a
/// thousand functions reading the attribute on an instance of it: the classes whose code may
/// store into the instance (the subclasses and the bases of each) are walked once, each class
/// at most twice, not along the bases of each subclass in turn.
#[test]
fn the_classes_storing_into_a_deep_subclass_chain_are_walked_once() {
    let shape = |k: usize| {
        let (depth, functions) = (250 * k, 125 * k);
        let mut lines = vec![
            "import httpx".to_owned(),
            String::new(),
            String::new(),
            "class C0:".to_owned(),
            "    def __init__(self) -> None:".to_owned(),
            "        self.client = httpx.Client()".to_owned(),
        ];
        for i in 1..depth {
            lines.push(String::new());
            lines.push(String::new());
            lines.push(format!("class C{i}(C{}):", i - 1));
            lines.push("    pass".to_owned());
        }
        for i in 0..functions {
            lines.push(String::new());
            lines.push(String::new());
            lines.push(format!("async def use{i}(node: C0) -> None:"));
            lines.push("    node.client.get(\"https://example.com\")".to_owned());
        }
        let p = Project::new(&format!("bounds_store_family_{k}"));
        p.write("m.py", &(lines.join("\n") + "\n"));
        p
    };
    let blocking = |found: Vec<Value>| found.iter().filter(|d| d["rule_code"] == "EFF101").count();
    assert_eq!(blocking(check_linear(SCALE, shape)), 1);
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
        let found = check_linear(3_000, |bindings| {
            let p = Project::new(&format!("bounds_cells_{names}_{bindings}"));
            p.write("pyproject.toml", config).write(
                "m.py",
                &cells_read_after_each_binding(bindings, names, declared),
            );
            p
        });
        assert_eq!(
            found.iter().filter(|d| d["rule_code"] == "MUT203").count(),
            names
        );
    }
}

/// A nested function an initializer calls writes a name chosen at run time on the last of a long
/// chain of copies (`s1 = s0`, `s2 = s1`, ...) of each of two closure cells: the initializer's
/// `self`, and another of its parameters. The parameter each chain holds is read from the copy
/// roots of the function at any length, so the write on the initializer's own object is part of
/// its initialization, and only the other one is reported (`MUT205`).
#[test]
fn a_long_copy_chain_of_a_closure_cell_holds_the_enclosing_parameter() {
    let config = "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT205\"]\n\n\
                  [[tool.heki.write-owners]]\nname = \"imports\"\ntargets = [\"m.Model.imports\"]\n\
                  unresolved = \"report\"\n";
    let copies = 2_000;
    let found = check_linear(copies, |copies| {
        let mut lines = vec![
            "class Model:".to_owned(),
            "    def __init__(self, names, peer):".to_owned(),
            "        self.imports = []".to_owned(),
            String::new(),
            "        def add(name):".to_owned(),
            "            s0 = self".to_owned(),
            "            p0 = peer".to_owned(),
        ];
        for i in 1..=copies {
            lines.push(format!("            s{i} = s{}", i - 1));
            lines.push(format!("            p{i} = p{}", i - 1));
        }
        lines.push(format!("            setattr(s{copies}, name, None)"));
        lines.push(format!("            setattr(p{copies}, name, None)"));
        lines.push(String::new());
        lines.push("        for name in names:".to_owned());
        lines.push("            add(name)".to_owned());
        let p = Project::new(&format!("bounds_captured_copy_chain_{copies}"));
        p.write("pyproject.toml", config)
            .write("m.py", &(lines.join("\n") + "\n"));
        p
    });
    let reported: Vec<u64> = found
        .iter()
        .filter(|d| d["rule_code"] == "MUT205")
        .map(|d| d["line"].as_u64().unwrap())
        .collect();
    // The write on the other parameter's copy, after both chains.
    assert_eq!(reported, vec![2 * copies as u64 + 9], "{found:#?}");
}

/// A display changed in place by a long chain of augmented assignments (`items += []`,
/// `items *= 1`, ...) before an item is appended and the items are written: each result is the
/// display, decided once from the result before it at any length, so the appended item keeps its
/// class (`MUT201`).
#[test]
fn a_long_chain_of_in_place_operators_gives_the_display_back() {
    let config = "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\"]\n\n\
                  [[tool.heki.write-owners]]\nname = \"fields\"\ntargets = [\"Model.fields\"]\n\
                  allowed-writers = [\"m.Model.*\"]\n";
    let found = check_linear(4_000, |operators| {
        let mut lines = vec![
            "class Model:".to_owned(),
            "    def __init__(self) -> None:".to_owned(),
            "        self.fields: list[str] = []".to_owned(),
            String::new(),
            String::new(),
            "def extend(model: Model) -> None:".to_owned(),
            "    items = []".to_owned(),
        ];
        for i in 0..operators {
            let op = if i % 2 == 0 { "+= []" } else { "*= 1" };
            lines.push(format!("    items {op}"));
        }
        lines.push("    items.append(model)".to_owned());
        lines.push("    for each in items:".to_owned());
        lines.push("        each.fields = []".to_owned());
        let p = Project::new(&format!("bounds_in_place_chain_{operators}"));
        p.write("pyproject.toml", config)
            .write("m.py", &(lines.join("\n") + "\n"));
        p
    });
    let reported: Vec<&str> = found
        .iter()
        .filter(|d| d["rule_code"] == "MUT201")
        .map(|d| d["qualname"].as_str().unwrap())
        .collect();
    assert_eq!(reported, vec!["extend"], "{found:#?}");
}

/// A function creating `children` child scopes, each over a value it binds just before and
/// opening a session per item, then one over a display of two items: comprehensions, which are
/// functions of their own before 3.12, or generator expressions.
fn child_scopes_over_bound_values(children: usize, comprehension: bool) -> String {
    let child = |iterable: &str| {
        if comprehension {
            format!("[requests.Session() for _ in {iterable}]")
        } else {
            format!("list(requests.Session() for _ in {iterable})")
        }
    };
    let mut lines = vec![
        "import requests".to_owned(),
        String::new(),
        String::new(),
        "def load(i):".to_owned(),
        "    return [i]".to_owned(),
        String::new(),
        String::new(),
        "def shape(out):".to_owned(),
    ];
    for i in 0..children {
        lines.push(format!("    rows = load({i})"));
        lines.push(format!("    out.append({})", child("rows")));
    }
    lines.push(format!("    out.append({})", child("[1, 2]")));
    lines.join("\n") + "\n"
}

/// The iterable a child scope receives is sized where the function creating it evaluates it, on
/// one index of that function for all its child scopes, both when the IR's loop bounds are
/// refined and when a rule asks whether a loop is data-sized: a session per item of every child
/// scope but the one over the display.
#[test]
fn child_scopes_size_their_iterables_on_one_index_of_their_creator() {
    let config = "[project]\nname = \"t\"\n\n[tool.heki]\ntarget-version = \"py311\"\n\n\
                  [tool.heki.lint]\nselect = [\"EFF102\"]\n";
    for (comprehension, children) in [(true, 8_000), (false, 5_000)] {
        let found = check_linear(children, |children| {
            let p = Project::new(&format!("bounds_child_scopes_{comprehension}_{children}"));
            p.write("pyproject.toml", config).write(
                "m.py",
                &child_scopes_over_bound_values(children, comprehension),
            );
            p
        });
        assert_eq!(
            found.iter().filter(|d| d["rule_code"] == "EFF102").count(),
            children
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
    let shape = |links: usize| {
        let p = Project::new(&format!("bounds_classes_{links}"));
        p.write("m.py", &chain_before_buffer_read(links));
        p
    };
    let blocking = |found: Vec<Value>| found.iter().filter(|d| d["rule_code"] == "EFF111").count();
    assert_eq!(blocking(check(&shape(4))), 0);
    assert_eq!(blocking(check_linear(12, shape)), 1);
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
    check(&p);
}

/// `MUT112` selected (it reads the `is` tests of a function's branches).
const MUT112_CONFIG: &str =
    "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"MUT112\"]\n";

/// Functions whose branches all test the last of a long chain of copies of one `is` test
/// (`c3000 = c2999`, `if c3000:`): the test each copy is, through the chain, is found once per
/// value for all the branches.
#[test]
fn branches_on_a_long_copy_chain_of_an_is_test_read_it_once() {
    check_linear(3_000, |links| {
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
        let p = Project::new(&format!("bounds_is_test_copy_chain_{links}"));
        p.write("pyproject.toml", MUT112_CONFIG)
            .write("m.py", &(lines.join("\n") + "\n"));
        p
    });
}

/// A function testing one value against `None` before each of many conditional expressions over
/// the last of a long chain of its copies: each phi input finds the tests it is on a side of by
/// the walk up the dominator tree from its edge, once per edge, and the value a copy copies
/// once per chain.
#[test]
fn phi_inputs_after_many_is_tests_find_their_sides_once_per_edge() {
    let tests = 600;
    let found = check_linear(tests, |tests| {
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
        let p = Project::new(&format!("bounds_is_test_sides_{tests}"));
        p.write("pyproject.toml", MUT112_CONFIG)
            .write("m.py", &(lines.join("\n") + "\n"));
        p
    });
    let lines: Vec<u64> = found
        .iter()
        .filter(|d| d["rule_code"] == "MUT112")
        .map(|d| d["line"].as_u64().unwrap())
        .collect();
    assert_eq!(lines, vec![2 + tests as u64 + 3 * tests as u64 + 1]);
}

/// A function returning the last of a long chain of conditional expressions, each choosing
/// between a call of the previous value and the previous value itself
/// (`x1000 = identity(x999) if flag else x999`): each phi is walked once and referred to by the
/// walks that meet it, instead of every argument walking all the phis before it. The calls each
/// argument may be the result of are those of every link before it, so the iterations' facts
/// grow with the square of the chain: they pass their budget instead.
#[test]
fn returned_phis_of_calls_on_the_previous_value_are_walked_once() {
    let found = check_past_budgets(1_000, &["consumption::IterFlow"], |links| {
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
        let p = Project::new(&format!("bounds_returned_phi_chain_{links}"));
        p.write("pyproject.toml", MUT112_CONFIG)
            .write("m.py", &(lines.join("\n") + "\n"));
        p
    });
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
    // 2,000 links and 256 arguments at the full size.
    let found = check_linear(SCALE, |k| {
        let (links, args) = (250 * k, 32 * k);
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
        let p = Project::new(&format!("bounds_origin_walk_budget_{k}"));
        p.write("pyproject.toml", MUT112_CONFIG)
            .write("m.py", &(lines.join("\n") + "\n"));
        p
    });
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
    let found = check_linear(links, |links| {
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
        let p = Project::new(&format!("bounds_reverse_loop_chain_{links}"));
        p.write("pyproject.toml", MUT112_CONFIG)
            .write("m.py", &(lines.join("\n") + "\n"));
        p
    });
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
    let found = check_linear(12_000, |aliases| {
        let p = Project::new(&format!("bounds_alias_chain_one_list_{aliases}"));
        p.write("pyproject.toml", MUT112_CONFIG)
            .write("m.py", &alias_chain_stores(aliases, true));
        p
    });
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
    let found = check_linear(8_000, |aliases| {
        let p = Project::new(&format!("bounds_alias_chain_many_lists_{aliases}"));
        p.write("pyproject.toml", MUT112_CONFIG)
            .write("m.py", &alias_chain_stores(aliases, false));
        p
    });
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
    let shape = |links: usize| {
        let mut lines = vec![
            "def walk(node):".to_owned(),
            "    extra = node or []".to_owned(),
            "    extra.append(1)".to_owned(),
            "    x = node".to_owned(),
        ];
        lines.extend((0..links).map(|_| "    x = x.child".to_owned()));
        lines.push("    x.append(1)".to_owned());
        let p = Project::new(&format!("bounds_attribute_chain_{links}"));
        p.write("pyproject.toml", MUT112_CONFIG)
            .write("m.py", &(lines.join("\n") + "\n"));
        p
    };
    let found: Vec<u64> = check_linear(10_000, shape)
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
    let found = check_linear(calls, |calls| {
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
        let p = Project::new(&format!("bounds_returned_call_chain_{calls}"));
        p.write("pyproject.toml", MUT112_CONFIG)
            .write("m.py", &(lines.join("\n") + "\n"));
        p
    });
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
    let (_, rules, _) =
        Project::new("bounds_call_chain_rules").heki(&["rule", "--all", "--output-format", "json"]);
    let codes: Vec<Value> = serde_json::from_str::<Vec<Value>>(&rules)
        .unwrap()
        .into_iter()
        .map(|rule| rule["code"].clone())
        .collect();
    let config = format!(
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = {}\n",
        Value::from(codes)
    );
    let shape = |calls: usize| {
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
        let p = Project::new(&format!("bounds_call_chain_{calls}"));
        p.write("pyproject.toml", &config)
            .write("m.py", &(lines.join("\n") + "\n"));
        p
    };
    let calls = 12_400;
    // The function stays modeled, so that its calls are analyzed at all.
    let (_, hazards, _) = shape(calls).heki(&["assumptions", "--json", "m.py"]);
    let hazards: Vec<Value> = serde_json::from_str(&hazards).unwrap();
    assert!(
        hazards.iter().all(|h| h["kind"] != "unmodeled"),
        "{hazards:#?}"
    );
    assert!(check_linear(calls, shape).is_empty());
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
    let shape = |depth: usize| {
        let p = Project::new(&format!("bounds_nested_origins_{depth}"));
        p.write("m.py", &lazy_client_under_nested_loops(depth));
        p
    };
    let eff102 = |found: Vec<Value>| found.iter().filter(|d| d["rule_code"] == "EFF102").count();
    assert_eq!(eff102(check(&shape(1))), 0);
    assert_eq!(eff102(check_past_budgets(30, &["guards"], shape)), 1);
}

/// Comparisons whose two operands are one value (`b1 = b0 == b0`), under an invalidation
/// contract: whether a branch depends on a flag, and the flag values on each edge, are decided
/// once per value instead of once per path to it.
#[test]
fn comparisons_sharing_operands_decide_each_value_once() {
    check_linear(30, |depth| {
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
        for i in 1..=depth {
            lines.push(format!("    b{i} = b{} == b{}", i - 1, i - 1));
        }
        lines.push(format!("    if b{depth}:"));
        lines.push("        notify()".to_owned());
        let p = Project::new(&format!("bounds_shared_comparisons_{depth}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[[tool.heki.invalidation]]\nname = \"m\"\ntargets = [\"Model.value\"]\ninvalidators = [\"m.notify\"]\n",
        )
        .write("m.py", &(lines.join("\n") + "\n"));
        p
    });
}

/// Every case of each `match` slices the list the one before made: the objects a loop's
/// iterable may hold are collected once per variable instead of once per path to it.
#[test]
fn slices_of_slices_collect_each_origin_once() {
    let found = check_linear(10, |matches| {
        let mut lines = vec![
            "import httpx".to_owned(),
            String::new(),
            String::new(),
            "def sizes(p):".to_owned(),
            "    x = [1, 2]".to_owned(),
        ];
        for _ in 0..matches {
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
        let p = Project::new(&format!("bounds_slice_origins_{matches}"));
        p.write("m.py", &(lines.join("\n") + "\n"));
        p
    });
    // Two items at most: the client is made a fixed number of times.
    assert!(found.is_empty(), "{found:#?}");
}

/// Subscripts whose object and index are one value (`t1 = t0[t0]`) in a loop: whether the
/// receiver of `t30.index(x)` outlives an iteration is decided once per variable.
#[test]
fn subscripts_sharing_operands_decide_each_lifetime_once() {
    check_linear(30, |depth| {
        let mut lines = vec![
            "def scan(items, s, x):".to_owned(),
            "    for i in items:".to_owned(),
            "        t0 = s[0]".to_owned(),
        ];
        for i in 1..=depth {
            lines.push(format!("        t{i} = t{}[t{}]", i - 1, i - 1));
        }
        lines.push(format!("        t{depth}.index(x)"));
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
        let p = Project::new(&format!("bounds_shared_subscripts_{depth}"));
        p.write("m.py", &(lines.join("\n") + "\n"));
        p
    });
}

/// Eight `match` statements of sixteen cases that pass a value through unchanged: whether a
/// branch on it is exact on every path is decided once per value and depth, not once per
/// path through the cases.
#[test]
fn a_branch_on_a_value_many_cases_pass_through_is_decided_once_per_depth() {
    let found = check_linear(8, |matches| {
        let mut lines = vec!["def decide(p, items):".to_owned(), "    x = 0".to_owned()];
        for _ in 0..matches {
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
        let p = Project::new(&format!("bounds_exact_paths_{matches}"));
        p.write("m.py", &(lines.join("\n") + "\n"));
        p
    });
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
    let found = check_linear(30, |depth| {
        let mut lines = vec!["import numpy as np".to_owned()];
        for (name, first, last) in [
            ("rebuilt", "acc[1:]", format!("(t{depth}, row)")),
            ("grown", "[acc]", format!("t{depth}")),
        ] {
            lines.extend(["", ""].map(str::to_owned));
            lines.push(format!("def {name}(rows):"));
            lines.push("    acc = np.empty(0)".to_owned());
            lines.push("    for row in rows:".to_owned());
            lines.push(format!("        t0 = {first}"));
            for i in 1..=depth {
                lines.push(format!("        t{i} = t{} + t{}", i - 1, i - 1));
            }
            lines.push(format!("        acc = np.concatenate({last})"));
            lines.push("    return acc".to_owned());
        }
        let p = Project::new(&format!("bounds_shared_operators_{depth}"));
        p.write("m.py", &(lines.join("\n") + "\n"));
        p
    });
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
    let found = check(&p);
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
    let shape = |loops: usize| {
        let p = Project::new(&format!("bounds_flags_{loops}"));
        p.write("m.py", &flagged_loops(loops));
        p
    };
    let eff102 = |found: Vec<Value>| found.iter().filter(|d| d["rule_code"] == "EFF102").count();
    assert_eq!(eff102(check(&shape(4))), 0);
    assert_eq!(eff102(check_linear(600, shape)), 600);
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
    let shape = |waits: usize| {
        let p = Project::new(&format!("bounds_waits_{waits}"));
        p.write("m.py", &waits_then_awaits(waits));
        p
    };
    for waits in [4, 400] {
        let found = if waits > 10 {
            check_linear(waits, shape)
        } else {
            check(&shape(waits))
        };
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
    let shape = |steps: usize| {
        let p = Project::new(&format!("bounds_rebuilt_{steps}"));
        p.write("m.py", &rebuilt_statement_and_flags(steps));
        p
    };
    // The statement stays DDL and the host numeric.
    for diags in [check(&shape(4)), check_linear(40, shape)] {
        assert!(diags.is_empty(), "{diags:#?}");
    }
}

/// Task factories at the end of a long chain of copies of one coroutine (`EFF107`): what each
/// call's argument is, and the values its task reaches, are indexed once per function instead of
/// searched once per call.
#[test]
fn task_factories_at_the_end_of_a_copy_chain_take_linear_time() {
    // 4,000 copies and 400 calls at the full size.
    let found = check_linear(SCALE, |k| {
        let (copies, calls) = (500 * k, 50 * k);
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
        let p = Project::new(&format!("bounds_task_copies_{k}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"EFF107\"]\n",
        )
        .write("m.py", &(lines.join("\n") + "\n"));
        p
    });
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
    let found = check_linear(4000, |copies| {
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
        for i in 1..=copies {
            lines.push(format!("    v{i} = v{}", i - 1));
        }
        lines.push(format!("    v{copies}.append([name])"));
        let p = Project::new(&format!("bounds_shared_copy_chain_{copies}"));
        p.write("m.py", &(lines.join("\n") + "\n"));
        p
    });
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
    let found = check_linear(3000, |values| {
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
        for i in 0..values {
            lines.push(format!("    a{i} = root.a{i}"));
            lines.push(format!("    a{i}.cache_size = {i}"));
        }
        let p = Project::new(&format!("bounds_many_shared_values_{values}"));
        p.write("m.py", &(lines.join("\n") + "\n"));
        p
    });
    let reported = found.iter().filter(|d| d["rule_code"] == "MUT114").count();
    assert_eq!(reported, 3000, "{found:#?}");
}

/// A helper initializing thousands of attributes of its argument lazily, each under its own
/// test (`if p.a0 is None: p.a0 = 0`): the walks that find what each test guards share the
/// function's budget, past which no lazy initialization is recognized and the changes count.
#[test]
fn lazy_initializations_past_the_budget_count_as_changes() {
    let found = check_linear(3000, |attributes| {
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
        for i in 0..attributes {
            lines.push(format!("    if p.a{i} is None:"));
            lines.push(format!("        p.a{i} = {i}"));
        }
        lines.extend(["", "", "def use(name):", "    fill(config(name))"].map(str::to_owned));
        let p = Project::new(&format!("bounds_many_lazy_initializations_{attributes}"));
        p.write("m.py", &(lines.join("\n") + "\n"));
        p
    });
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
    let diags = check_linear(3000, |copies| {
        let mut lines = vec![
            "import httpx".to_owned(),
            String::new(),
            String::new(),
            "def copies():".to_owned(),
            "    x0 = [1, 2]".to_owned(),
        ];
        for i in 1..=copies {
            lines.push(format!("    x{i} = list(x{})", i - 1));
        }
        lines.push("    for _ in x4:".to_owned());
        lines.push("        httpx.Client()".to_owned());
        lines.push(format!("    for _ in x{copies}:"));
        lines.push("        httpx.Client()".to_owned());
        let p = Project::new(&format!("bounds_constructor_chain_{copies}"));
        p.write("m.py", &(lines.join("\n") + "\n"));
        p
    });
    // Four copies keep two items; three thousand are past the depth the sizes follow.
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
    let found = check(&p);
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
    let shape = |steps: usize| {
        let p = Project::new(&format!("bounds_repeated_len_{steps}"));
        p.write("m.py", &repeated_length_test(steps));
        p
    };
    for found in [check(&shape(4)), check_linear(40, shape)] {
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
    let shape = |depth: usize, copies: usize| {
        let p = Project::new(&format!("bounds_partials_{depth}_{copies}"));
        p.write("m.py", &partial_chain(depth, copies));
        p
    };
    let queries = |found: Vec<Value>| found.iter().filter(|d| d["rule_code"] == "EFF113").count();
    assert_eq!(queries(check(&shape(3, 3))), 1);
    assert_eq!(queries(check_linear(30, |depth| shape(depth, 3))), 0);
    assert_eq!(queries(check_linear(30, |copies| shape(3, copies))), 0);
}

/// Thousands of names unpacked from one display of the previous value
/// (`a0, a1, ... = acc, acc, ...`), each rebuilt entry by entry, in a function with as many
/// starred elements elsewhere: where each display starts unpacking is indexed once.
#[test]
fn names_unpacked_from_one_display_find_its_starred_elements_once() {
    let found = check_linear(3000, |n| {
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
        let p = Project::new(&format!("bounds_unpacked_display_{n}"));
        p.write("m.py", &(lines.join("\n") + "\n"));
        p
    });
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
    // A copy per alias costs the text's size times the aliases: both grow with the shape.
    let diags = check_linear(SCALE, |k| {
        let p = Project::new(&format!("bounds_aliased_schema_{k}"));
        p.write("m.py", &aliased_schema(k << 17, 125 * k));
        p
    });
    // The statement is DDL: no query runs per row.
    assert!(diags.is_empty(), "{diags:#?}");
}

/// A helper filling thousands of items of its argument's attribute, each in a branch of its own
/// (`if flag: p.items[0] = 0`): the `not in` tests that would make a store a memo fill are found
/// in one walk of the blocks.
#[test]
fn item_stores_in_many_branches_find_membership_tests_once() {
    let found = check_linear(3000, |stores| {
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
        for i in 0..stores {
            lines.push("    if flag:".to_owned());
            lines.push(format!("        p.items[{i}] = {i}"));
        }
        lines.extend(["", "", "def use(name):", "    fill(config(name), True)"].map(str::to_owned));
        let p = Project::new(&format!("bounds_branch_item_stores_{stores}"));
        p.write("m.py", &(lines.join("\n") + "\n"));
        p
    });
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
    let found = check_linear(4_000, |links| {
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
        let p = Project::new(&format!("bounds_alias_chain_stores_{links}"));
        p.write("m.py", &(lines.join("\n") + "\n"));
        p
    });
    assert!(found.is_empty(), "{found:#?}");
}

/// A strict `read-owners` target on a class with thousands of subclasses and as many functions
/// reading the attribute on an object of unknown class: the classes the target covers are found
/// once per rule and name, not once per read. Every read but one is in an allowed reader.
#[test]
fn strict_reads_of_a_target_with_many_subclasses_find_its_classes_once() {
    let found = check_linear(3_000, |classes| {
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
        let p = Project::new(&format!("bounds_strict_reads_{classes}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[[tool.heki.read-owners]]\nname = \"m\"\ntargets = [\"Base.secret\"]\nallowed-readers = [\"m.allowed*\", \"m.Base.*\"]\nunresolved = \"report\"\n",
        )
        .write("m.py", &(lines.join("\n") + "\n"));
        p
    });
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
    let found = check_linear(3_000, |classes| {
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
        let p = Project::new(&format!("bounds_class_constant_reads_{classes}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[[tool.heki.write-owners]]\nname = \"m\"\ntargets = [\"Field.public\"]\nallowed-writers = [\"m.allowed*\"]\n",
        )
        .write("m.py", &(lines.join("\n") + "\n"));
        p
    });
    let reported: Vec<&Value> = found
        .iter()
        .filter(|d| d["rule_code"] == "MUT201")
        .collect();
    assert_eq!(reported.len(), 1, "{found:#?}");
    assert_eq!(reported[0]["qualname"], "write");
}

/// `MUT119` selected (it reads the new containers functions return).
const MUT119_CONFIG: &str =
    "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"MUT119\"]\n";

/// A function returning the last of a long chain of copies of a new dict, each copy rebound on a
/// branch and read by a builtin that only inspects it, and a caller changing what it returns
/// (`MUT119`): the walk back to where the dict is made and the walk forward over its uses visit
/// each value once.
#[test]
fn a_new_dict_returned_through_a_long_chain_of_copies_is_walked_once() {
    // The largest chain whose function the IR budget still models.
    let found = check_linear(2_000, |links| {
        let mut lines = vec!["def make(flag):".to_owned(), "    d0 = {}".to_owned()];
        for i in 1..=links {
            lines.push(format!("    d{i} = d{}", i - 1));
            lines.push("    if flag:".to_owned());
            lines.push(format!("        d{i} = d{}", i - 1));
            lines.push(format!("    len(d{i})"));
        }
        lines.push(format!("    return d{links}"));
        lines.push(String::new());
        lines.push(String::new());
        lines.push("def use():".to_owned());
        lines.push("    make(True).update(a=1)".to_owned());
        let p = Project::new(&format!("bounds_fresh_return_chain_{links}"));
        p.write("pyproject.toml", MUT119_CONFIG)
            .write("m.py", &(lines.join("\n") + "\n"));
        p
    });
    let reported = found.iter().filter(|d| d["rule_code"] == "MUT119").count();
    assert_eq!(reported, 1, "{found:#?}");
}

/// A library class with a catalogued attribute, thousands of project subclasses, and as many
/// functions reading the attribute on an instance of it: whether every class the instance may be
/// takes the row is decided once per class and name, over at most `DISPATCH_CLASS_LIMIT`
/// classes, not once per read over every subclass. Past the limit the read stays untyped; on a
/// subclass with no subclasses of its own the row types it.
#[test]
fn catalogued_attribute_reads_on_a_class_with_many_subclasses_decide_it_once() {
    let found = check_linear(3_000, |classes| {
        let mut lines = vec!["from acme import Client".to_owned()];
        for i in 0..classes {
            lines.extend([
                String::new(),
                String::new(),
                format!("class Client{i}(Client):"),
                "    pass".to_owned(),
            ]);
        }
        for i in 0..=classes {
            let (name, class) = if i == classes {
                ("leaf".to_owned(), "Client0")
            } else {
                (format!("read{i}"), "Client")
            };
            lines.extend([
                String::new(),
                String::new(),
                format!("async def {name}(c: {class}) -> None:"),
                "    c.chat.completions.create(model=\"m\")".to_owned(),
            ]);
        }
        let p = Project::new(&format!("bounds_catalogued_attribute_reads_{classes}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[[tool.heki.ecosystem]]\nsymbol = \"acme.Completions.create\"\neffects = [\"blocking_net\"]\n\n[[tool.heki.ecosystem]]\nsymbol = \"acme.Client.chat\"\nattribute-type = \"acme.Chat\"\n\n[[tool.heki.ecosystem]]\nsymbol = \"acme.Chat.completions\"\nattribute-type = \"acme.Completions\"\n",
        )
        .write("m.py", &(lines.join("\n") + "\n"));
        p
    });
    let reported: Vec<&Value> = found
        .iter()
        .filter(|d| d["rule_code"] == "EFF101")
        .collect();
    assert_eq!(reported.len(), 1, "{found:#?}");
    assert_eq!(reported[0]["qualname"], "leaf");
}

/// A dataclass whose field defaults to an `object()` sentinel, with thousands of subclasses,
/// each tested against the sentinel and deep-copied in functions of its own, and the base
/// deep-copied as often (`MUT118`): what a deep copy of each class gives, and which tests read the
/// sentinel on each class holding it, are decided once per class, and a report lists a few tests.
#[test]
fn deep_copies_of_a_sentinel_class_with_many_subclasses_decide_each_class_once() {
    let classes = 3_000;
    let found = check_linear(classes, |classes| {
        let mut lines = vec![
            "import copy".to_owned(),
            "from dataclasses import dataclass".to_owned(),
            String::new(),
            "MISSING = object()".to_owned(),
            String::new(),
            String::new(),
            "@dataclass".to_owned(),
            "class Spec:".to_owned(),
            "    value: object = MISSING".to_owned(),
        ];
        for i in 0..classes {
            lines.extend([
                String::new(),
                String::new(),
                format!("class Spec{i}(Spec):"),
                "    pass".to_owned(),
                String::new(),
                String::new(),
                format!("def given{i}(s: Spec{i}):"),
                "    return s.value is not MISSING".to_owned(),
                String::new(),
                String::new(),
                format!("def isolated{i}(s: Spec{i}):"),
                "    return copy.deepcopy(s)".to_owned(),
                String::new(),
                String::new(),
                format!("def isolated_base{i}(s: Spec):"),
                "    return copy.deepcopy(s)".to_owned(),
            ]);
        }
        let p = Project::new(&format!("bounds_sentinel_subclasses_{classes}"));
        p.write("m.py", &(lines.join("\n") + "\n"));
        p
    });
    let reported: Vec<&Value> = found
        .iter()
        .filter(|d| d["rule_code"] == "MUT118")
        .collect();
    assert_eq!(reported.len(), 2 * classes);
    assert!(
        reported
            .iter()
            .all(|d| d["related"].as_array().unwrap().len() == 5)
    );
}

/// A chain of thousands of classes, each holding the next in a typed attribute and each
/// deep-copied, the last holding a tested `object()` sentinel (`MUT118`): what each class reaches
/// below its attributes is joined once per class (`heki_ir::walk::Closure`), not walked again from
/// each copy, so every copy reports it in linear time.
#[test]
fn deep_copies_along_a_long_chain_of_held_classes_join_one_fact_per_class() {
    let classes = 3_000;
    let found = check_linear(classes, |classes| {
        let mut lines = vec![
            "import copy".to_owned(),
            String::new(),
            "MISSING = object()".to_owned(),
        ];
        for i in 0..classes {
            let init = if i + 1 == classes {
                "        self.mark = MISSING".to_owned()
            } else {
                "        self.next = following".to_owned()
            };
            let params = if i + 1 == classes {
                "self".to_owned()
            } else {
                format!("self, following: \"C{}\"", i + 1)
            };
            lines.extend([
                String::new(),
                String::new(),
                format!("class C{i}:"),
                format!("    def __init__({params}):"),
                init,
                String::new(),
                String::new(),
                format!("def isolated{i}(c: C{i}):"),
                "    return copy.deepcopy(c)".to_owned(),
            ]);
        }
        lines.extend([
            String::new(),
            String::new(),
            format!("def marked(c: C{}):", classes - 1),
            "    return c.mark is MISSING".to_owned(),
        ]);
        let p = Project::new(&format!("bounds_sentinel_chain_{classes}"));
        p.write("m.py", &(lines.join("\n") + "\n"));
        p
    });
    let reported = found.iter().filter(|d| d["rule_code"] == "MUT118").count();
    assert_eq!(reported, classes);
}

/// Functions binding thousands of names each to the one before, from an attribute load, and
/// comparing each with an `object()` sentinel by identity (`MUT118`'s tests): what each operand
/// copies is read from the function's copy-root table, not walked back along the chain per
/// comparison.
#[test]
fn identity_tests_of_a_long_alias_chain_read_each_copy_root_once() {
    check_linear(8_000, |names| {
        let mut lines = vec![
            "MISSING = object()".to_owned(),
            String::new(),
            String::new(),
            "class Node:".to_owned(),
            "    def __init__(self):".to_owned(),
            "        self.mark = MISSING".to_owned(),
        ];
        for f in 0..4 {
            lines.extend([
                String::new(),
                String::new(),
                format!("def scan{f}(node: Node):"),
                "    v0 = node.mark".to_owned(),
            ]);
            lines.extend((1..names).map(|i| format!("    v{i} = v{} ", i - 1)));
            lines.extend((0..names).map(|i| format!("    x{i} = v{i} is MISSING")));
            lines.push("    return node".to_owned());
        }
        let p = Project::new(&format!("bounds_alias_identity_tests_{names}"));
        p.write("m.py", &(lines.join("\n") + "\n"));
        p
    });
}

/// A class with thousands of subclasses whose `reset` a call target naming no class names, and
/// a function calling `Base.reset(obj)` as many times: the class family of each call (the
/// classes defining the method it reaches, with their subclasses) is expanded once per target
/// and defining class, not once per call. Only one call is outside the allowed callers.
#[test]
fn calls_of_a_method_with_many_subclasses_expand_its_family_once() {
    let found = check_linear(5_000, |size| {
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
        let p = Project::new(&format!("bounds_call_target_families_{size}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"resets\"\ntargets = [\"reset\"]\nallowed-callers = [\"m.run\"]\n",
        )
        .write("m.py", &(lines.join("\n") + "\n"));
        p
    });
    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(found[0]["qualname"], "other");
}

/// A class whose initializer calls a thousand methods, each running one public helper that
/// writes, reads, and calls on its own `self` a thousand times, under owner contracts judging by
/// the call graph and lexically: whether the helper only runs inside the class's own
/// initialization is decided once per contract and object, not once per access.
#[test]
fn accesses_of_a_helper_only_initialization_runs_ask_its_callers_once() {
    let module = |size: usize| {
        let mut lines = vec![
            "class Model:".to_owned(),
            "    def __init__(self):".to_owned(),
            "        self.items = []".to_owned(),
            "        self.name = \"x\"".to_owned(),
        ];
        lines.extend((0..size).map(|i| format!("        self.step{i}()")));
        lines.extend(
            [
                "",
                "    def seal(self):",
                "        pass",
                "",
                "    def fill(self):",
            ]
            .map(str::to_owned),
        );
        for _ in 0..size {
            lines.push("        self.items.append(self.name)".to_owned());
            lines.push("        self.seal()".to_owned());
        }
        for i in 0..size {
            lines.extend([
                String::new(),
                format!("    def step{i}(self):"),
                "        self.fill()".to_owned(),
            ]);
        }
        lines.join("\n") + "\n"
    };
    for attribution in ["call-graph", "lexical"] {
        let contract = |section: &str, name: &str, target: &str| {
            format!(
                "[[tool.heki.{section}]]\nname = \"{name}\"\ntargets = [\"{target}\"]\nattribution = \"{attribution}\"\n\n"
            )
        };
        let config = format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\", \"MUT203\", \"MUT204\"]\n\n{}{}{}",
            contract("write-owners", "items", "Model.items"),
            contract("read-owners", "name", "Model.name"),
            contract("call-owners", "seal", "Model.seal"),
        );
        let found = check_linear(1_000, |size| {
            let p = Project::new(&format!(
                "bounds_initialization_helper_{}_{size}",
                attribution.len()
            ));
            p.write("pyproject.toml", &config)
                .write("m.py", &module(size));
            p
        });
        assert!(found.is_empty(), "{attribution}: {found:#?}");
    }
}

/// Modules binding twelve thousand names, each to an attribute of the one before
/// (`a1 = a0.x`, `a2 = a1.x`, ...), about as many as a module body's IR budget models: whether
/// a binding is a string constant takes a bounded walk of copies and attribute loads, past
/// which it is none, not a walk down the whole chain.
#[test]
fn module_names_bound_along_a_long_attribute_chain_take_a_bounded_walk() {
    let found = check_linear(12_000, |links| {
        let mut lines = vec!["a0 = \"s\"".to_owned()];
        lines.extend((1..=links).map(|i| format!("a{i} = a{}.x", i - 1)));
        let source = lines.join("\n") + "\n";
        let p = Project::new(&format!("bounds_constant_attribute_chain_{links}"));
        for module in ["m0.py", "m1.py", "m2.py", "m3.py"] {
            p.write(module, &source);
        }
        p
    });
    assert!(found.is_empty(), "{found:#?}");
}

/// One function making twenty thousand project calls (`Base.reset(obj, ...)` with eight
/// operands each), about as many as its IR budget models: `MUT202` maps each call instruction
/// to its call site and tests its operands' definitions, which it looks up in the function's
/// definitions once built, not by walking the function per operand.
#[test]
fn many_calls_in_one_function_look_up_their_operands_once() {
    let found = check_linear(20_000, |calls| {
        let mut lines = vec![
            "class Base:".to_owned(),
            "    def reset(self, *args):".to_owned(),
            "        pass".to_owned(),
            String::new(),
            String::new(),
            "def run(obj):".to_owned(),
        ];
        lines.extend(
            (0..calls).map(|_| "    Base.reset(obj, obj, obj, obj, obj, obj, obj, obj)".to_owned()),
        );
        let p = Project::new(&format!("bounds_many_call_operands_{calls}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT202\"]\n",
        )
        .write("m.py", &(lines.join("\n") + "\n"));
        p
    });
    assert!(found.is_empty(), "{found:#?}");
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
    let found = check_linear(4_000, |n| {
        strict_accesses(
            &format!("bounds_strict_helper_reads_{n}"),
            "read-owners",
            "obj.secret",
            n,
            n,
        )
    });
    assert!(found.is_empty(), "{found:#?}");
}

/// The same for strict calls (`MUT207`).
#[test]
fn strict_calls_of_one_name_in_a_helper_walk_its_callers_once() {
    let found = check_linear(4_000, |n| {
        strict_accesses(
            &format!("bounds_strict_helper_calls_{n}"),
            "call-owners",
            "obj.open()",
            n,
            n,
        )
    });
    assert!(found.is_empty(), "{found:#?}");
}

/// The same for strict writes (`MUT205`).
#[test]
fn strict_writes_of_one_name_in_a_helper_walk_its_callers_once() {
    let found = check_linear(4_000, |n| {
        strict_accesses(
            &format!("bounds_strict_helper_writes_{n}"),
            "write-owners",
            "obj.secret = 1",
            n,
            n,
        )
    });
    assert!(found.is_empty(), "{found:#?}");
}

/// A value one `if` / `elif` chain of thousands of arms assigns, then tested as many times
/// (`isinstance(src, str)`): the edges the tests of the merged value keep share the function's
/// budget, past which a test is no guard and the effect it guards is kept.
#[test]
fn tests_of_a_wide_merge_keep_their_edges_within_a_budget() {
    let path_facts = "heki_effect::summary::Extractor::path_facts";
    let found = check_past_budgets(2000, &[path_facts], |n| {
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
        let p = Project::new(&format!("bounds_wide_merge_tests_{n}"));
        p.write("m.py", &(lines.join("\n") + "\n"));
        p
    });
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

/// A project of `src` with [`MUT113_CONFIG`].
fn mut113_project(name: &str, src: &str) -> Project {
    let p = Project::new(name);
    p.write("pyproject.toml", MUT113_CONFIG).write("m.py", src);
    p
}

fn mut113_count(found: &[Value]) -> usize {
    found.iter().filter(|d| d["rule_code"] == "MUT113").count()
}

/// `MUT113` reports of `heki check` over `src`.
fn iterated_mutations(name: &str, src: &str) -> usize {
    mut113_count(&check(&mut113_project(name, src)))
}

/// `MUT113` reports of `heki check` over `src(n)`, after checking that its cost grows about
/// linearly from `src(n / SCALE)` ([`check_linear`]).
fn iterated_mutations_linear(name: &str, n: usize, src: impl Fn(usize) -> String) -> usize {
    mut113_count(&check_linear(n, |n| {
        mut113_project(&format!("{name}_{n}"), &src(n))
    }))
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
    assert_eq!(
        iterated_mutations_linear("bounds_rotation", 600, rotating),
        0
    );
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
    assert_eq!(iterated_mutations_linear("bounds_merges", 2000, merging), 0);
}

/// A loop over a private generator whose body makes `calls` calls of a method changing the
/// container it iterates: what each call runs on (the iterated call's arguments, a method of the
/// family) is looked up by its span once, not by a scan of the function's calls per call.
#[test]
fn a_loop_over_a_generator_looks_up_each_call_of_its_body_once() {
    let src = |calls: usize| {
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
        lines.join("\n") + "\n"
    };
    assert_eq!(
        iterated_mutations_linear("bounds_loop_calls", 20_000, src),
        1
    );
}

/// Loops over a container of `self` and over a private generator's result whose bodies hold
/// `branches` conditional blocks one after another: the blocks from which each loop goes on to
/// its next step are found by one walk back from its header, not by a walk from every block.
#[test]
fn a_loop_with_many_branches_finds_the_blocks_reaching_its_header_once() {
    let src = |branches: usize| {
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
        lines.join("\n") + "\n"
    };
    assert_eq!(
        iterated_mutations_linear("bounds_loop_branches", 8_000, src),
        0
    );
}

/// A function consuming a private generator's result `consumptions` times in a row
/// (`list(node._walk())`): what it does with each call's result (handed on, iterated) is read
/// from one index of its call sites and iterations, not by a scan of the file's call sites per
/// call. Each result is consumed at once, so nothing runs while the generator is suspended.
#[test]
fn repeated_consumptions_of_a_generator_read_each_call_once() {
    let src = |consumptions: usize| {
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
        lines.join("\n") + "\n"
    };
    assert_eq!(
        iterated_mutations_linear("bounds_consumptions", 16_000, src),
        0
    );
}

/// A loop over `zip(node._walk(), ...)` of `results` generator calls whose body makes as many
/// calls: the loop is one fact shared by every result it steps, and what it does to them is found
/// once per instruction, not once per result.
#[test]
fn a_loop_over_many_generator_results_is_summarized_once() {
    let src = |results: usize| {
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
        lines.join("\n") + "\n"
    };
    assert_eq!(
        iterated_mutations_linear("bounds_zipped_results", 8_000, src),
        1
    );
}

/// A loop over `zip(self.d0, ..., self.d1999)` whose body makes as many calls: the loop is one
/// fact for every container it steps, and what each call changes is read once, not once per
/// container.
#[test]
fn a_loop_over_many_self_containers_is_one_fact() {
    let src = |containers: usize| {
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
        lines.join("\n") + "\n"
    };
    assert_eq!(
        iterated_mutations_linear("bounds_zipped_containers", 2_000, src),
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
        iterated_mutations_linear("bounds_iteration_facts", 2_000, zipped),
        0
    );
}

/// A private generator handed on through five thousand private wrappers, each returning the one
/// before's result (`def _w1(self): return self._w0()`), and a loop over the last one's result
/// calling a method that changes the generator's container: the walk from the generator to the
/// loop keeps its own stack, one entry per wrapper, instead of a native frame per wrapper.
#[test]
fn a_long_chain_of_wrappers_handing_on_a_generator_is_walked_on_its_own_stack() {
    let src = |wrappers: usize| {
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
        lines.join("\n") + "\n"
    };
    assert_eq!(
        iterated_mutations_linear("bounds_wrapper_chain", 5_000, src),
        1
    );
}

/// An edit through the language server changing every method of a class of five thousand
/// methods (and adding a blocking call to an `async def`, whose `EFF101` report shows the edit
/// was checked): which class families read the changed methods' summaries is decided once per
/// class, not by reading the class's methods again for each changed method.
#[test]
fn an_edit_changing_thousands_of_methods_reads_each_class_once() {
    let module = |methods: usize, changed: bool| {
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
    let methods = 5_000;
    // The walks the server runs on the edited module, counted through `heki check`.
    check_linear(methods, |methods| {
        let p = Project::new(&format!("bounds_family_edit_steps_{methods}"));
        p.write("pkg/__init__.py", "")
            .write("pkg/table.py", &module(methods, true));
        p
    });
    let p = Project::new("bounds_family_edit");
    p.write("pkg/__init__.py", "")
        .write("pkg/table.py", &module(methods, false));
    let table = uri(&p.dir.join("pkg/table.py"));
    let mut lsp = Lsp::start(&p.dir, &["server", "--stdio"]);
    lsp.request(
        "initialize",
        json!({"processId": null, "rootUri": uri(&p.dir), "capabilities": {}}),
    );
    lsp.notify("initialized", json!({}));
    lsp.ready();
    lsp.open(&table, &module(methods, false));
    lsp.diagnostics(&table, <[Value]>::is_empty);
    let changed = module(methods, true);
    lsp.change(&table, 2, &changed);
    p.write("pkg/table.py", &changed);
    lsp.notify(
        "textDocument/didSave",
        json!({"textDocument": {"uri": table}, "text": changed}),
    );
    let diags = lsp.diagnostics(&table, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["EFF101"]);
    lsp.settle();
    lsp.stop(true);
}

/// The lines of the `MUT113` diagnostics among `found`.
fn mut113_lines(found: &[Value]) -> Vec<u64> {
    found
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
    let methods = 3;
    // The module and the lines of its calls.
    let module = |writes: usize| {
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
        (lines.join("\n") + "\n", calls)
    };
    let writes = 8_000;
    let found = check_linear(writes, |writes| {
        mut113_project(
            &format!("bounds_self_container_changes_{writes}"),
            &module(writes).0,
        )
    });
    assert_eq!(mut113_lines(&found), module(writes).1[1..]);
}

/// A method storing under a key of a typed dict of `self` after each of hundreds of tests that
/// the key is there (`if key in self.rows: self.rows[key] = value`), called in a loop over it:
/// the sides of the tests are found once per function, and whether one dominates a store by
/// the dominator tree.
#[test]
fn many_key_tests_of_a_container_of_self_find_their_sides_once() {
    let module = |tests: usize| {
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
        lines
    };
    let tests = 600;
    let found = check_linear(tests, |tests| {
        mut113_project(
            &format!("bounds_self_container_key_tests_{tests}"),
            &(module(tests).join("\n") + "\n"),
        )
    });
    assert_eq!(mut113_lines(&found), vec![module(tests).len() as u64]);
}

/// A module binding thousands of names, each to the one before (`a1 = a0`, `a2 = a1`, ...):
/// every alias is resolved once, and the last still names `object.__setattr__`.
#[test]
fn a_long_chain_of_module_aliases_resolves_each_once() {
    let diags = check_linear(5_000, |names| {
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
        let p = Project::new(&format!("bounds_module_alias_chain_{names}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\"]\n\n[[tool.heki.write-owners]]\nname = \"kinds\"\ntargets = [\"Node.kind\"]\nallowed-writers = [\"Node.*\"]\n",
        )
        .write("m.py", &(lines.join("\n") + "\n"));
        p
    });
    assert_eq!(diags.len(), 1, "{diags:#?}");
    assert_eq!(diags[0]["qualname"], "mark");
}

/// A method returning one of thousands of copies of `self.items` behind as many tests: the
/// value each copy copies is found once, not once per return.
#[test]
fn returns_of_a_long_copy_chain_find_its_root_once() {
    let diags = check_linear(3_000, |copies| {
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
        let p = Project::new(&format!("bounds_getter_copy_chain_{copies}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\"]\n\n[[tool.heki.write-owners]]\nname = \"items\"\ntargets = [\"Node.items\"]\nallowed-writers = [\"Node.*\"]\n",
        )
        .write("m.py", &(lines.join("\n") + "\n"));
        p
    });
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
/// dotted or sharing one module part (`pkg.m:X0._*`), cost no more per call than one. Trying
/// each target costs the calls times the targets, so both grow with the shape.
#[test]
fn call_targets_are_looked_up_not_tried_one_by_one() {
    // `count` targets under the module part `module`, and `C0`'s methods: in one contract
    // (`one`), or in as many.
    let contracts = |count: usize, module: &str, one: bool| {
        let targets: Vec<String> = (0..count)
            .map(|i| format!("\"{module}X{i}._*\""))
            .chain(std::iter::once(format!("\"{module}C0._*\"")))
            .collect();
        if one {
            return format!(
                "[[tool.heki.call-owners]]\nname = \"many\"\ntargets = [{}]\nforbidden-callers = [\"pkg.use.f0\"]\n",
                targets.join(", ")
            );
        }
        targets
            .iter()
            .enumerate()
            .map(|(i, t)| {
                format!("[[tool.heki.call-owners]]\nname = \"c{i}\"\ntargets = [{t}]\nforbidden-callers = [\"pkg.use.f0\"]\n\n")
            })
            .collect()
    };
    for (form, module) in [("dotted", "pkg.m."), ("module", "pkg.m:")] {
        for one in [true, false] {
            let name = format!("{form}_{}", if one { "one" } else { "many" });
            // 2,000 targets and 100 functions of 100 calls at the full size.
            let diags = check_linear(2_000, |targets| {
                let (model, usage) = many_calls(10, targets / 20, 100);
                let p = Project::new(&format!("bounds_call_targets_{name}_{targets}"));
                p.write(
                    "pyproject.toml",
                    &format!(
                        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n{}",
                        contracts(targets, module, one)
                    ),
                )
                .write("pkg/__init__.py", "")
                .write("pkg/m.py", &model)
                .write("pkg/use.py", &usage);
                p
            });
            // Only the forbidden caller's ten calls of `C0._m` are reported.
            assert_eq!(diags.len(), 10, "{name}: {diags:#?}");
            assert!(diags.iter().all(|d| d["qualname"] == "f0"), "{name}");
        }
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
/// their functions and of a function of many parameters none names. The lists and the calls
/// grow with the shape.
#[test]
fn argument_lists_are_looked_up_from_the_bound_parameters() {
    let diags = check_linear(20_000, |lists| {
        argument_marks(lists, 1_000, lists / 200, 100, 50)
    });
    // Only the forbidden caller's hundred calls passing the constants are reported.
    assert_eq!(diags.len(), 100, "{diags:#?}");
    assert!(diags.iter().all(|d| d["qualname"] == "u0"));
}

/// A long chain of mappings, each merging the one before it (`f2 = {**f1, "kind": True}`) or
/// a phi of it and a display merging it (`f3 = f2 if flag else {**f2}`), each unpacked into a
/// call matching an argument list: the ways of each mapping are decided once and kept, so the
/// walk from each stops at the one before it instead of walking the chain again.
#[test]
fn a_long_chain_of_merged_mappings_is_decided_once_per_link() {
    let found = check_linear(2000, |links| {
        let mut lines = vec![
            "def plain(kind=False):".to_owned(),
            "    return kind".to_owned(),
            String::new(),
            String::new(),
            "def chain(flag):".to_owned(),
            "    f0 = {\"kind\": True}".to_owned(),
        ];
        for i in 0..links {
            lines.push(match i % 2 {
                0 => format!("    f{} = {{**f{i}, \"kind\": True}}", i + 1),
                _ => format!("    f{} = f{i} if flag else {{**f{i}}}", i + 1),
            });
            lines.push(format!("    plain(**f{})", i + 1));
        }
        let p = Project::new(&format!("bounds_merged_mappings_{links}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n\
             [[tool.heki.call-owners]]\nname = \"c\"\ntargets = [\"plain(kind=True)\"]\n",
        )
        .write("m.py", &(lines.join("\n") + "\n"));
        p
    });
    let reported = found.iter().filter(|d| d["rule_code"] == "MUT204").count();
    assert_eq!(reported, 2000, "{found:#?}");
}

/// Thousands of references to a `call-owners` target, each a rebinding of one name that a long
/// chain of phis carries to one use, `last` (with `references`): which values reach a use taking
/// them is found in one walk of the function, not once per reference (a function twice as long
/// passes the IR's size budget).
fn reference_phi_chain(name: &str, rebindings: usize, references: &str, last: &str) -> Project {
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
    lines.push(last.to_owned());
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!("[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"home\"\ntargets = [\"m.decide\"]\nallowed-callers = [\"m.decide\"]\nreferences = \"{references}\"\n"),
    )
    .write("m.py", &(lines.join("\n") + "\n"));
    p
}

#[test]
fn references_carried_by_a_long_phi_chain_are_found_in_one_walk() {
    for (references, last, verb) in [
        ("calls", "    return chosen", "hands on"),
        ("reads", "    return chosen.__name__", "reads"),
    ] {
        let found = check_linear(2000, |rebindings| {
            reference_phi_chain(
                &format!("bounds_reference_phi_chain_{references}_{rebindings}"),
                rebindings,
                references,
                last,
            )
        });
        let taken = found
            .iter()
            .filter(|d| {
                d["rule_code"] == "MUT204"
                    && d["qualname"] == "pick"
                    && d["message"].as_str().is_some_and(|m| m.contains(verb))
            })
            .count();
        assert_eq!(taken, 2000, "{references}");
    }
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
    let found = check_linear(1200, |steps| {
        let p = Project::new(&format!("bounds_queue_rebindings_{steps}"));
        p.write("m.py", &queue_rebindings(steps));
        p
    });
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
    let found = check_linear(6000, |helpers| {
        let p = Project::new(&format!("bounds_queue_forwarding_{helpers}"));
        p.write("m.py", &queue_forwarding_chain(helpers));
        p
    });
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
    let found = check_linear(6000, |attrs| {
        let p = Project::new(&format!("bounds_queue_attribute_chain_{attrs}"));
        p.write("m.py", &queue_attribute_chain(attrs));
        p
    });
    assert!(
        found
            .iter()
            .any(|d| d["rule_code"] == "EFF111" && d["qualname"] == "entry"),
        "{found:#?}"
    );
}

/// `heki inspect <args> --output-format json` of the project.
fn inspect(p: &Project, args: &[&str]) -> Value {
    let mut all = vec!["inspect"];
    all.extend_from_slice(args);
    all.extend(["--output-format", "json"]);
    heki_json(p, &all)
}

/// A lattice thirty levels deep, two functions a level, each calling both of the level below
/// (2^30 paths up from the bottom): the callers' walk lists each call once.
#[test]
fn callers_of_a_diamond_lattice_are_walked_once_each() {
    let shape = |levels: usize| {
        let mut lines = vec!["def bottom():".to_owned(), "    return 1".to_owned()];
        let below = |level: usize| {
            if level == 0 {
                vec!["bottom".to_owned()]
            } else {
                vec![format!("a{}", level - 1), format!("b{}", level - 1)]
            }
        };
        for level in 0..levels {
            for side in ["a", "b"] {
                lines.extend([
                    String::new(),
                    String::new(),
                    format!("def {side}{level}():"),
                ]);
                lines.extend(below(level).iter().map(|f| format!("    {f}()")));
            }
        }
        let p = Project::new(&format!("bounds_caller_lattice_{levels}"));
        p.write("m.py", &(lines.join("\n") + "\n"));
        p
    };
    check_linear(30, shape);
    let r = inspect(&shape(30), &["m.bottom", "--callers", "--depth", "64"]);
    // Two calls of `bottom`, then four calls into each later level.
    assert_eq!(r["callers"]["count"], 2 + 4 * 29);
    assert_eq!(r["callers"]["truncated"], false);
}

/// Five thousand methods of one class, each calling the next on `self`, the last changing a
/// `self` attribute; the call chain of the change, the walks to an unresolved call and to a
/// changed module global through as many functions, and the walk past thousands of functions
/// that each make an unresolved call, are walked once, and the last lists only the first ones.
#[test]
fn chains_down_thousands_of_calls_of_one_module_are_walked_once() {
    // The last function of each chain is named apart, so that every size has it.
    let shape = |n: usize| {
        let mut lines = vec![
            "G = 0".to_owned(),
            String::new(),
            String::new(),
            "class C:".to_owned(),
            "    def __init__(self):".to_owned(),
            "        self.items = []".to_owned(),
        ];
        let next = |i: usize, prefix: &str| {
            if i + 2 == n {
                format!("{prefix}_end")
            } else {
                format!("{prefix}{}", i + 1)
            }
        };
        for i in 0..n - 1 {
            lines.extend([
                String::new(),
                format!("    def m{i}(self):"),
                format!("        self.{}()", next(i, "m")),
            ]);
        }
        lines.extend([
            String::new(),
            "    def m_end(self):".to_owned(),
            "        self.items.append(1)".to_owned(),
        ]);
        for i in 0..n - 1 {
            lines.extend([
                String::new(),
                String::new(),
                format!("def f{i}(x):"),
                format!("    return {}(x)", next(i, "f")),
            ]);
        }
        lines.extend([
            String::new(),
            String::new(),
            "def f_end(x):".to_owned(),
            "    global G".to_owned(),
            "    G = 1".to_owned(),
            "    return x.anything()".to_owned(),
        ]);
        for i in 0..n - 1 {
            lines.extend([
                String::new(),
                String::new(),
                format!("def g{i}(x):"),
                "    x.anything()".to_owned(),
                format!("    return {}(x)", next(i, "g")),
            ]);
        }
        lines.extend([
            String::new(),
            String::new(),
            "def g_end(x):".to_owned(),
            "    return x.anything()".to_owned(),
        ]);
        let p = Project::new(&format!("bounds_inspect_chains_{n}"));
        p.write("m.py", &(lines.join("\n") + "\n"));
        p
    };
    let n = 5_000;
    check_linear(n, shape);
    let p = shape(n);
    let r = inspect(&p, &["m.C.m0"]);
    let items = &r["changes"]["self_attrs"][0];
    assert_eq!(items["chain"].as_array().unwrap().len(), n - 1);
    let f = inspect(&p, &["m.f0"]);
    assert_eq!(
        f["changes"]["globals"][0]["chain"]
            .as_array()
            .unwrap()
            .len(),
        n - 1
    );
    // Impure: the unknown call does not decide the verdict, but it is named.
    assert_eq!(f["purity"]["verdict"], "impure");
    assert_eq!(f["purity"]["unknown_at"]["fqn"], "m.f_end");
    assert_eq!(
        f["purity"]["unknown_at"]["chain"].as_array().unwrap().len(),
        n - 1
    );
    assert_eq!(f["purity"]["unknown_reach"]["truncated"], false);
    let g = inspect(&p, &["m.g0"]);
    let unknown = &g["purity"]["unknown_reach"];
    let entries = unknown["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 100);
    assert_eq!(entries[99]["fqn"], "m.g99");
    assert_eq!(entries[99]["chain"].as_array().unwrap().len(), 99);
    assert_eq!(unknown["truncated"], true);
    let last = inspect(&p, &["m.f_end", "--callers", "--depth", "100"]);
    assert_eq!(last["callers"]["count"], 100);
}

/// A function choosing one of a thousand module globals before changing it, and one making
/// three thousand calls that each hand a global to a helper changing its argument: the roots of
/// a merged value are kept up to a limit, and the calls' hand-offs are looked up by call.
#[test]
fn shared_state_of_thousands_of_globals_is_walked_once() {
    // 1,000 globals and 3,000 calls at the full size.
    let shape = |k: usize| {
        let (globals, calls) = (125 * k, 375 * k);
        let mut lines: Vec<String> = (0..globals).map(|i| format!("G{i} = []")).collect();
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
        for i in 1..globals {
            lines.extend([format!("    if k == {i}:"), format!("        x = G{i}")]);
        }
        lines.extend([
            "    x.append(1)".to_owned(),
            String::new(),
            String::new(),
            "def hand():".to_owned(),
        ]);
        lines.extend((0..calls).map(|i| format!("    append(G{})", i % globals)));
        let p = Project::new(&format!("bounds_shared_state_{k}"));
        p.write("m.py", &(lines.join("\n") + "\n"));
        p
    };
    check_linear(SCALE, shape);
    let p = shape(SCALE);
    let choose = inspect(&p, &["m.choose"]);
    assert_eq!(
        choose["purity"]["because"],
        serde_json::json!(["changes_globals"])
    );
    let hand = inspect(&p, &["m.hand"]);
    // One entry per global, at its first hand-off.
    assert_eq!(hand["changes"]["globals"].as_array().unwrap().len(), 1_000);
}

/// `heki dump-facts` of the project's `m.py`.
fn dump_facts(p: &Project) -> Value {
    heki_json(p, &["dump-facts", "m.py"])
}

/// The path facts of a function decide dominance once per region, over two nodes per block: a
/// `try` body of thousands of calls whose handler returns (thousands of exception edges into one
/// block), loops nested twenty deep with a call at each depth (each asked for every loop around
/// it), many loops one after another, and a long chain of `and` tests of module constants.
#[test]
fn path_facts_decide_each_region_once() {
    // 3,000 calls, 20 nested loops, 1,000 loops, and 500 tests at the full size.
    let shape = |k: usize| {
        let (calls, depth, loops, tests) = (375 * k, 20 * k / SCALE, 125 * k, 500 * k / SCALE);
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
        for d in 0..depth {
            let pad = "    ".repeat(d + 1);
            lines.push(format!("{pad}db.at({d})"));
            lines.push(format!("{pad}for x{d} in rows:"));
        }
        lines.push(format!("{}db.inner()", "    ".repeat(depth + 1)));
        lines.push(String::new());
        lines.push("def sequential(rows, db):".to_owned());
        for i in 0..loops {
            lines.push("    for row in rows:".to_owned());
            lines.push(format!("        db.each({i})"));
        }
        lines.push(String::new());
        lines.push("def tests(sock):".to_owned());
        lines.push(format!(
            "    if {}:",
            vec!["not _IS_SYNC"; tests].join(" and ")
        ));
        lines.push("        sock.recv()".to_owned());
        let p = Project::new(&format!("bounds_path_facts_{k}"));
        p.write("m.py", &(lines.join("\n") + "\n"));
        p
    };
    check_linear(SCALE, shape);
    let facts = dump_facts(&shape(SCALE));
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
    let shape = |tests: usize| {
        let mut lines = vec![
            "def spent(pending, rows, db):".to_owned(),
            "    while pending:".to_owned(),
            "        cur = pending.pop()".to_owned(),
        ];
        for i in 0..tests {
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
        let p = Project::new(&format!("bounds_caps_past_budget_{tests}"));
        p.write("m.py", &(lines.join("\n") + "\n"));
        p
    };
    let owners = [
        "heki_effect::summary::Extractor::path_facts",
        "heki_ir::facts::FnIndex::every_path_passes",
    ];
    check_past_budgets(600, &owners, shape);
    let facts = dump_facts(&shape(600));
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
    // 3,000 wrappers, 1,000 loops, and 60 parameters at the full size.
    let shape = |k: usize| {
        let (wrappers, loops, params) = (375 * k, 125 * k, 60 * k / SCALE);
        let mut lines = vec!["def chain(rows):".to_owned(), "    x0 = rows".to_owned()];
        lines.extend((1..=wrappers).map(|i| format!("    x{i} = sorted(x{})", i - 1)));
        lines.push(format!("    for y in x{wrappers}:"));
        lines.push("        pass".to_owned());
        lines.push(String::new());
        lines.push("def filled(rows):".to_owned());
        lines.push("    out = []".to_owned());
        for _ in 0..loops {
            lines.push("    for r in rows:".to_owned());
            lines.push("        out.append(r)".to_owned());
        }
        lines.push("    for y in out:".to_owned());
        lines.push("        pass".to_owned());
        lines.push(String::new());
        let params: Vec<String> = (0..params).map(|i| format!("p{i}")).collect();
        lines.push(format!("def chosen(flag, {}):", params.join(", ")));
        lines.push("    xs = p0".to_owned());
        for (i, p) in params[1..].iter().enumerate() {
            lines.push(format!("    if flag == {i}:"));
            lines.push(format!("        xs = {p}"));
        }
        lines.push("    for y in xs:".to_owned());
        lines.push("        pass".to_owned());
        let p = Project::new(&format!("bounds_item_origins_{k}"));
        p.write("m.py", &(lines.join("\n") + "\n"));
        p
    };
    check_linear(SCALE, shape);
    let facts = dump_facts(&shape(SCALE));
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
    // The walk runs once per writer class, so only the helpers grow with the shape.
    let classes = 600;
    let found = check_linear(600, |helpers| {
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
        let p = Project::new(&format!("bounds_hash_reads_per_class_{helpers}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"MUT107\"]\n",
        )
        .write("m.py", &(lines.join("\n") + "\n"));
        p
    });
    let reported = found.iter().filter(|d| d["rule_code"] == "MUT107").count();
    assert_eq!(reported, classes);
}

/// The implementations of a call are checked over at most 64 classes (the receiver's class and
/// its project subclasses): a receiver class with more subclasses has no complete set of targets,
/// so a call that blocks through every one of them is not established (`EFF111`, not `EFF101`).
#[test]
fn implementations_past_the_class_bound_are_not_complete() {
    let shape = |subclasses: usize| {
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
        p
    };
    let codes = |found: Vec<Value>| -> Vec<String> {
        found
            .iter()
            .map(|d| d["rule_code"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(codes(check(&shape(3))), ["EFF101"]);
    assert_eq!(codes(check_linear(300, shape)), ["EFF111"]);
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
    // The path facts of thousands of early returns pass their budget; the tests do not need them.
    let path_facts = "heki_effect::summary::Extractor::path_facts";
    let found = check_past_budgets(n, &[path_facts], |n| {
        let mut body = vec!["    def run(self):".to_owned()];
        for i in 0..n {
            body.push(format!("        if self.f{i}:"));
            body.push("            return".to_owned());
        }
        for _ in 0..n {
            body.push("        with self._lock:".to_owned());
            body.push("            self.helper()".to_owned());
        }
        locked_class(&format!("bounds_lock_tests_{n}"), 0, body)
    });
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
    let found = check_linear(n, |n| {
        let mut body = vec!["    def run(self, xs):".to_owned()];
        body.extend((0..n).map(|i| format!("        self._l{i}.acquire()")));
        body.push("        self._lock.acquire()".to_owned());
        for i in 0..n {
            body.push(format!("        if xs[{i}]:"));
            body.push("            self.helper()".to_owned());
        }
        locked_class(&format!("bounds_many_locks_{n}"), n, body)
    });
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
    let found = check_linear(2_000, |n| {
        let mut lines = vec![
            "async def run(session: AsyncSession, xs, reader: asyncio.StreamReader):".to_owned(),
        ];
        for _ in 0..n {
            lines.push(
                "    await asyncio.gather(load(session, None), load(session, None))".to_owned(),
            );
        }
        fanout_module(&format!("bounds_many_fanouts_{n}"), lines)
    });
    // The first fan-out can be the session's first operation; after it, every path has used it.
    assert_eq!(
        found.iter().filter(|d| d["rule_code"] == "EFF110").count(),
        1
    );
}

/// Hundreds of fan-outs of one function, each of a comprehension the function builds itself
/// (Python 3.12): what each list holds comes from one scan of the function, not one per fan-out.
/// Each comprehension is a loop the summaries model too, so the count stays where a debug build
/// runs the whole check in a few seconds.
#[test]
fn many_comprehension_fanouts_scan_the_function_once() {
    let found = check_linear(500, |n| {
        let mut lines = vec![
            "async def run(session: AsyncSession, xs, reader: asyncio.StreamReader):".to_owned(),
        ];
        for _ in 0..n {
            lines.push("    await asyncio.gather(*[load(session, x) for x in xs])".to_owned());
        }
        fanout_module(&format!("bounds_many_comprehension_fanouts_{n}"), lines)
    });
    assert_eq!(
        found.iter().filter(|d| d["rule_code"] == "EFF110").count(),
        1
    );
}

/// Thousands of tasks a task group starts in one loop: whether a value is the loop's own comes
/// from one scan of the loop.
#[test]
fn many_tasks_of_one_loop_scan_it_once() {
    let found = check_linear(2_000, |n| {
        let mut lines = vec![
            "async def run(session: AsyncSession, xs, reader: asyncio.StreamReader):".to_owned(),
            "    async with asyncio.TaskGroup() as tg:".to_owned(),
            "        for x in xs:".to_owned(),
        ];
        for i in 0..n {
            lines.push(format!("            r{i} = x.r{i}"));
            lines.push(format!("            tg.create_task(load(r{i}, r{i}))"));
        }
        fanout_module(&format!("bounds_many_tasks_{n}"), lines)
    });
    assert!(found.iter().all(|d| d["rule_code"] != "EFF110"));
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
    let found = check_linear(12_000, |links| {
        let p = Project::new(&format!("bounds_change_chain_{links}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT110\"]\n",
        )
        .write("m.py", &argument_handed_down(links, 10));
        p
    });
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["rule_code"], "MUT110");
    assert_eq!(found[0]["related"].as_array().unwrap().len(), 12_000);
}

/// Eight thousand functions of one module, each handing its read-only parameter to a helper
/// that changes it itself, and to one that hands it to that helper: the reports' chains share
/// one index of the module's calls, and a chain that ends at its first function indexes none.
#[test]
fn many_reports_into_one_module_share_its_index_of_calls() {
    let found = check_linear(8_000, |entries| {
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
        for i in 0..entries {
            lines.extend([
                String::new(),
                String::new(),
                format!("def entry{i}(xs: Sequence[int]):"),
                "    change(xs)".to_owned(),
                "    relay(xs)".to_owned(),
            ]);
        }
        let p = Project::new(&format!("bounds_change_chains_into_one_module_{entries}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT110\"]\n",
        )
        .write("m.py", &(lines.join("\n") + "\n"));
        p
    });
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
    for reported in [false, true] {
        let found = check_linear(1_500, |aliases| {
            let end = if reported {
                format!("p{aliases}.insert(0, x)")
            } else {
                "return c1".to_owned()
            };
            let p = Project::new(&format!("bounds_package_path_{reported}_{aliases}"));
            p.write(
                "pyproject.toml",
                "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"EFF201\"]\n\n[[tool.heki.effect-boundaries]]\nname = \"env\"\nscopes = [\"probe.*\"]\nforbid = [\"reads-environment\"]\n",
            )
            .write("pkg/__init__.py", &path_alias_chain(aliases, &end))
            .write(
                "probe.py",
                "from importlib import import_module\n\n\ndef load(name):\n    return import_module(f\"pkg.{name}\")\n",
            );
            p
        });
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
    let found = check_linear(2_000, |packages| {
        let p = Project::new(&format!("bounds_merged_package_paths_{packages}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"EFF201\"]\n\n[[tool.heki.effect-boundaries]]\nname = \"env\"\nscopes = [\"probe.*\"]\nforbid = [\"reads-environment\"]\n",
        )
        .write("pkg/__init__.py", "")
        .write("pkg/plug/__init__.py", "")
        .write("pkg/boot.py", &merged_package_paths(8, packages))
        .write("clean/__init__.py", "")
        .write(
            "probe.py",
            "from importlib import import_module\n\n\ndef plugin(name):\n    return import_module(f\"pkg.plug.{name}\")\n\n\ndef unrelated(name):\n    return import_module(f\"clean.{name}\")\n",
        );
        p
    });
    assert_eq!(
        found
            .iter()
            .filter(|d| d["rule_code"] == "EFF201")
            .map(|d| d["qualname"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["plugin"]
    );
}

/// One dict display copied along a long chain of assignments, each copy unpacked with `**` into a
/// call of a target with an argument list: whether anything changes the display is decided once
/// for the display, not once per call (which spends the function's budget, past which the
/// calls pass nothing).
#[test]
fn calls_unpacking_copies_of_one_display_decide_it_once() {
    let copies = 2000;
    let found = check_linear(copies, |copies| {
        let mut lines = vec![
            "def plain(kind=False):".to_owned(),
            "    return kind".to_owned(),
            String::new(),
            String::new(),
            "def unpack():".to_owned(),
            "    f0 = {\"kind\": True}".to_owned(),
        ];
        for i in 1..=copies {
            lines.push(format!("    f{i} = f{}", i - 1));
        }
        for i in 0..=copies {
            lines.push(format!("    plain(**f{i})"));
        }
        let p = Project::new(&format!("bounds_unpacked_display_copies_{copies}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"c\"\ntargets = [\"m.plain(kind=True)\"]\nallowed-callers = [\"m.plain\"]\n",
        )
        .write("m.py", &(lines.join("\n") + "\n"));
        p
    });
    let calls = found.iter().filter(|d| d["rule_code"] == "MUT204").count();
    assert_eq!(calls, copies + 1);
}

/// `functions` functions of one module, each making one project call.
fn functions_each_calling(functions: usize) -> String {
    let mut lines = vec!["def g(x):".to_owned(), "    return x".to_owned()];
    for i in 0..functions {
        lines.extend([
            String::new(),
            String::new(),
            format!("def call{i}(x):"),
            "    return g(x)".to_owned(),
        ]);
    }
    lines.join("\n") + "\n"
}

/// A function handing the read-only value a `readonly-returns` producer returns to `calls`
/// calls of a function that only reads it, then once to one that changes it.
fn read_only_value_handed_to_calls(calls: usize) -> String {
    let mut lines = vec![
        "def load():".to_owned(),
        "    return [1, 2, 3]".to_owned(),
        String::new(),
        String::new(),
        "def use(xs):".to_owned(),
        "    return len(xs)".to_owned(),
        String::new(),
        String::new(),
        "def grow(xs):".to_owned(),
        "    xs.append(1)".to_owned(),
        String::new(),
        String::new(),
        "def shape():".to_owned(),
        "    values = load()".to_owned(),
    ];
    lines.extend((0..calls).map(|_| "    use(values)".to_owned()));
    lines.push("    grow(values)".to_owned());
    lines.join("\n") + "\n"
}

/// `MUT202` finds the call site of each call instruction in an index of the module's call sites
/// by function and position, built once per module (forty thousand functions each making a
/// call), and the facts a call runs, at the call and where the object it returns is used, once
/// per call target of a function (twenty thousand calls handed one read-only value): neither is
/// scanned again per function or per call. The one call handing the value to a function that
/// changes it is still found.
#[test]
fn call_sites_and_the_facts_they_run_are_looked_up_not_scanned() {
    let found = check_linear(40_000, |functions| {
        let p = Project::new(&format!("bounds_functions_each_calling_{functions}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT202\"]\n",
        )
        .write("m.py", &functions_each_calling(functions));
        p
    });
    assert!(found.is_empty(), "{found:?}");

    let calls = 20_000;
    let found = check_linear(calls, |calls| {
        let p = Project::new(&format!("bounds_read_only_value_handed_to_calls_{calls}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT202\"]\n\n\
             [[tool.heki.readonly-returns]]\nname = \"r\"\nproducers = [\"m.load\"]\n",
        )
        .write("m.py", &read_only_value_handed_to_calls(calls));
        p
    });
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["rule_code"], "MUT202");
    assert_eq!(found[0]["line"], calls + 15);
}

/// A function calling `scopes` generator expressions, each reading `Model._given` and calling
/// `Model.peek` of its parameter.
fn guarded_accesses_in_generator_expressions(scopes: usize) -> String {
    let mut lines = vec![
        "class Model:".to_owned(),
        "    def __init__(self):".to_owned(),
        "        self._given = 1".to_owned(),
        String::new(),
        "    def peek(self):".to_owned(),
        "        return 1".to_owned(),
        String::new(),
        String::new(),
        "def shape(xs, out, m: Model):".to_owned(),
    ];
    lines.extend(
        (0..scopes)
            .map(|_| "    out.append(sum(1 for _ in xs if m._given and m.peek()))".to_owned()),
    );
    lines.join("\n") + "\n"
}

/// The generator expressions of a function share one qualname, so each of their call sites runs
/// the reads and calls of all of them. `MUT203` and `MUT204` walk the callers once for the
/// accesses of one function that are reported at the same places, and a report counts the sites
/// of all of them without adding them to each report one by one: three thousand generator
/// expressions take time linear in them. Each call site is still reported, standing for every
/// access.
#[test]
fn guarded_accesses_in_many_generator_expressions_walk_their_callers_once() {
    let found = check_linear(3_000, |scopes| {
        let p = Project::new(&format!(
            "bounds_guarded_accesses_in_generator_expressions_{scopes}"
        ));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT203\", \"MUT204\"]\n\n\
             [[tool.heki.read-owners]]\nname = \"given\"\ntargets = [\"m.Model._given\"]\n\
             forbidden-readers = [\"m:shape\"]\n\n\
             [[tool.heki.call-owners]]\nname = \"peek\"\ntargets = [\"m.Model.peek\"]\n\
             forbidden-callers = [\"m:shape\"]\n",
        )
        .write("m.py", &guarded_accesses_in_generator_expressions(scopes));
        p
    });
    for code in ["MUT203", "MUT204"] {
        let reports: Vec<&Value> = found.iter().filter(|d| d["rule_code"] == code).collect();
        assert_eq!(reports.len(), 3_000, "{code}");
        assert!(reports.iter().all(|d| d["sites"] == 3_000), "{code}");
    }
}

/// A function binding a long chain of aliases of a class and a long chain of attribute loads
/// from a parameter, then deriving classes from the last of each (`class C0(a8000)`, `class
/// D0(b8000)`): each base is followed along its copies through one table per function and through
/// a bounded number of attribute loads, so the classes of the alias derive from the class and
/// those of the attribute from an unknown base.
#[test]
fn class_statements_deriving_from_long_chains_name_each_base_in_bounded_steps() {
    // 8,000 links and 200 classes at the full size: a walk down the chain per class costs their
    // product.
    let shape = |k: usize| {
        let (links, classes) = (1_000 * k, 25 * k);
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
        let p = Project::new(&format!("bounds_class_base_chains_{k}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"home\"\ntargets = [\"m.K\"]\nallowed-callers = [\"m.K.*\"]\n",
        )
        .write("m.py", &(lines.join("\n") + "\n"));
        p
    };
    let found: Vec<String> = check_linear(SCALE, shape)
        .iter()
        .map(|d| d["message"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        found,
        vec!["`build` constructs `m.K` outside the call owners of `home`"]
    );
}

/// A dict display of `width` entries unpacked with `**` by `calls` calls of a target with an
/// argument list.
fn wide_display_unpacked(width: usize, calls: usize) -> Project {
    let entries: Vec<String> = (0..width)
        .map(|i| format!("\"k{i}\": {i}"))
        .chain(["\"kind\": True".to_owned()])
        .collect();
    let mut lines = vec![
        "def plain(kind=False, **options):".to_owned(),
        "    return kind".to_owned(),
        String::new(),
        String::new(),
        "def unpack():".to_owned(),
        format!("    flags = {{{}}}", entries.join(", ")),
    ];
    lines.extend((0..calls).map(|_| "    plain(**flags)".to_owned()));
    let p = Project::new(&format!("bounds_wide_display_{width}_{calls}"));
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"c\"\ntargets = [\"m.plain(kind=True)\"]\nallowed-callers = [\"m.plain\"]\n",
    )
    .write("m.py", &(lines.join("\n") + "\n"));
    p
}

/// Each call binds every entry of the display it unpacks, which the source writes once: the
/// entries are taken from the function's budget, so a display as wide as the calls are many
/// costs work linear in the function (not width times calls), and past the budget a call passes
/// nothing the display holds. A narrow display leaves every call its argument.
#[test]
fn a_wide_display_unpacked_by_many_calls_is_bound_within_the_budget() {
    let narrow = check(&wide_display_unpacked(40, 40));
    assert_eq!(narrow.len(), 40, "{narrow:#?}");
    let wide = check_past_budgets(8000, &["summary::walks"], |n| wide_display_unpacked(n, n));
    assert!((1..8000).contains(&wide.len()), "{}", wide.len());
}

/// The project `name` of the module `m.py` of `source`, configured by `config`.
fn module_project(name: &str, config: &str, source: &str) -> Project {
    let p = Project::new(name);
    p.write("pyproject.toml", config).write("m.py", source);
    p
}

/// `EFF112` selected on top of the default rules.
const EFF112_CONFIG: &str =
    "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"EFF112\"]\n";

/// `EFF114` selected on top of the default rules.
const EFF114_CONFIG: &str =
    "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"EFF114\"]\n";

/// The project `name` of `source` with [`EFF114_CONFIG`], the size in the name.
fn eff114_project(name: &str, n: usize, source: &str) -> Project {
    module_project(&format!("{name}_{n}"), EFF114_CONFIG, source)
}

/// The `EFF112` reports of `heki check` with [`EFF112_CONFIG`] on the module `m.py` of
/// `source(n)`, after checking that its cost grows about linearly from `source(n / SCALE)`
/// ([`check_linear`]).
fn set_order_outputs(name: &str, n: usize, source: impl Fn(usize) -> String) -> usize {
    let found = check_linear(n, |n| {
        module_project(&format!("{name}_{n}"), EFF112_CONFIG, &source(n))
    });
    found.iter().filter(|d| d["rule_code"] == "EFF112").count()
}

const SET_ORDER_HEAD: &str = "import json\nfrom pathlib import Path\n\n\ndef names(table: dict[str, int]) -> set[str]:\n    return set(table)\n\n\n";

/// A chain of four thousand functions each returning what the one below returns, the last a
/// list in a set's order, written at the top: each function's origin comes from its callee's
/// summary once.
#[test]
fn an_order_returned_up_thousands_of_functions_is_decided_once_each() {
    let found = set_order_outputs("bounds_order_returned_up", 4_000, |n| {
        let mut lines = vec![SET_ORDER_HEAD.to_owned()];
        lines.push("def f0(table: dict[str, int]) -> list[str]:".to_owned());
        lines.push("    return [k for k in names(table)]".to_owned());
        for i in 1..n {
            lines.extend([
                String::new(),
                String::new(),
                format!("def f{i}(table: dict[str, int]) -> list[str]:"),
                format!("    return f{}(table)", i - 1),
            ]);
        }
        lines.extend([
            String::new(),
            String::new(),
            "def top(table: dict[str, int], path: Path) -> None:".to_owned(),
            format!("    path.write_text(json.dumps(f{}(table)))", n - 1),
        ]);
        lines.join("\n") + "\n"
    });
    assert_eq!(found, 1);
}

/// A value in a set's order handed down four thousand helpers to the one that writes it: each
/// helper's written parameter comes from its callee's summary once.
#[test]
fn an_order_handed_down_thousands_of_helpers_is_decided_once_each() {
    let found = set_order_outputs("bounds_order_handed_down", 4_000, |n| {
        let mut lines = vec![SET_ORDER_HEAD.to_owned()];
        lines.push("def h0(path: Path, data: object) -> None:".to_owned());
        lines.push("    path.write_text(json.dumps(data))".to_owned());
        for i in 1..n {
            lines.extend([
                String::new(),
                String::new(),
                format!("def h{i}(path: Path, data: object) -> None:"),
                format!("    h{}(path, data)", i - 1),
            ]);
        }
        lines.extend([
            String::new(),
            String::new(),
            "def top(table: dict[str, int], path: Path) -> None:".to_owned(),
            format!("    h{}(path, [k for k in names(table)])", n - 1),
        ]);
        lines.join("\n") + "\n"
    });
    assert_eq!(found, 1);
}

/// Five hundred comprehensions over one set (each a loop the summaries model too), each handed
/// to one writer through one list: the flows of one function share their nodes.
#[test]
fn many_comprehensions_over_one_set_share_their_flows() {
    let n = 500;
    let found = set_order_outputs("bounds_many_comprehensions", n, |n| {
        let mut lines = vec![SET_ORDER_HEAD.to_owned()];
        lines.extend([
            "def write(path: Path, parts: list[object]) -> None:".to_owned(),
            "    path.write_text(json.dumps(parts))".to_owned(),
            String::new(),
            String::new(),
            "def top(table: dict[str, int], path: Path) -> None:".to_owned(),
            "    found = names(table)".to_owned(),
            "    parts = []".to_owned(),
        ]);
        for _ in 0..n {
            lines.push("    parts.append([k for k in found])".to_owned());
        }
        lines.push("    write(path, parts)".to_owned());
        lines.join("\n") + "\n"
    });
    assert_eq!(found, n);
}

/// The `EFF114` reports of `heki check` with [`EFF114_CONFIG`] on the module `m.py` of
/// `source(n)`, after checking that its cost grows about linearly from `source(n / SCALE)`
/// ([`check_linear`]).
fn recursion_handlers(name: &str, n: usize, source: impl Fn(usize) -> String) -> usize {
    let found = check_linear(n, |n| eff114_project(name, n, &source(n)));
    found.iter().filter(|d| d["rule_code"] == "EFF114").count()
}

/// Two thousand `try` statements in one function, each calling into one recursion: each body's
/// blocks are found from one pass over the function, and each clause is walked once.
#[test]
fn many_try_statements_into_one_cycle_are_decided_once_each() {
    let n = 2_000;
    let found = recursion_handlers("bounds_many_try_statements", n, |n| {
        let mut lines = vec![
            "def rec(x):".to_owned(),
            "    return rec(x)".to_owned(),
            String::new(),
            String::new(),
            "def shape(x):".to_owned(),
        ];
        for _ in 0..n {
            lines.extend(
                [
                    "    try:",
                    "        rec(x)",
                    "    except Exception:",
                    "        pass",
                ]
                .map(str::to_owned),
            );
        }
        lines.join("\n") + "\n"
    });
    assert_eq!(found, n);
}

/// A call cycle through four thousand functions, each calling the next inside a `try`: the cycle
/// is found once, by the components of the propagation.
#[test]
fn a_cycle_through_thousands_of_functions_is_found_once() {
    let n = 4_000;
    let found = recursion_handlers("bounds_cycle_of_functions", n, |n| {
        let mut lines = Vec::new();
        for i in 0..n {
            lines.extend([
                format!("def f{i}(x):"),
                "    try:".to_owned(),
                format!("        return f{}(x)", (i + 1) % n),
                "    except Exception:".to_owned(),
                "        return None".to_owned(),
                String::new(),
                String::new(),
            ]);
        }
        lines.join("\n") + "\n"
    });
    assert_eq!(found, n);
}

/// A value chosen among a set and hundreds of sorted lists by a chain of branches, then
/// iterated and handed to a writer: whether it is such a set is decided once per value.
#[test]
fn a_phi_of_hundreds_of_branches_is_decided_once() {
    let found = set_order_outputs("bounds_order_phi", 400, |n| {
        let mut lines = vec![SET_ORDER_HEAD.to_owned()];
        lines.extend([
            "def write(path: Path, data: object) -> None:".to_owned(),
            "    path.write_text(json.dumps(data))".to_owned(),
            String::new(),
            String::new(),
            "def top(table: dict[str, int], path: Path, k: int) -> None:".to_owned(),
            "    found = names(table)".to_owned(),
        ]);
        for i in 0..n {
            lines.push(format!("    if k == {i}:"));
            lines.push("        found = sorted(table)".to_owned());
        }
        lines.push("    write(path, [x for x in found])".to_owned());
        lines.join("\n") + "\n"
    });
    assert_eq!(found, 1);
}

/// A loop over a set storing each element as a key of a mapping a thousand times, each store
/// after reads of a thousand other mappings by the same key and of the stored mapping itself by
/// another: whether the key was read from the stored mapping before is looked up by mapping and
/// key, once per store.
#[test]
fn stores_after_reads_by_the_same_key_are_decided_once_each() {
    let found = set_order_outputs("bounds_order_stores", 1_000, |n| {
        let mut lines = vec![SET_ORDER_HEAD.to_owned()];
        lines.extend([
            "def filled(table: dict[str, int], other: dict[str, int], k: str) -> dict:".to_owned(),
            "    fields = {}".to_owned(),
            "    for name in names(table):".to_owned(),
        ]);
        for i in 0..n {
            lines.push(format!("        a{i} = {{}}"));
            lines.push(format!("        x{i} = a{i}[name] if other else fields[k]"));
            lines.push("        fields[name] = other".to_owned());
        }
        lines.extend([
            "    return fields".to_owned(),
            String::new(),
            String::new(),
            "def top(table: dict[str, int], other: dict[str, int], path: Path) -> None:".to_owned(),
            "    path.write_text(json.dumps(filled(table, other, \"k\")))".to_owned(),
        ]);
        lines.join("\n") + "\n"
    });
    assert_eq!(found, 1);
}

/// A lattice thirty levels deep, two functions a level, each calling both of the level below
/// inside a `try`, above a function calling itself (2^30 paths down to it): whether a call
/// reaches the cycle is a join over its callees.
#[test]
fn a_lattice_above_a_cycle_joins_each_call_once() {
    let levels = 30;
    let found = recursion_handlers("bounds_lattice_above_a_cycle", levels, |levels| {
        let mut lines = vec![
            "def bottom(x):".to_owned(),
            "    return bottom(x)".to_owned(),
        ];
        for level in 0..levels {
            let below = if level == 0 {
                vec!["bottom".to_owned()]
            } else {
                vec![format!("a{}", level - 1), format!("b{}", level - 1)]
            };
            for side in ["a", "b"] {
                lines.extend([
                    String::new(),
                    String::new(),
                    format!("def {side}{level}(x):"),
                    "    try:".to_owned(),
                ]);
                lines.extend(below.iter().map(|f| format!("        {f}(x)")));
                lines.extend(["    except Exception:", "        return None"].map(str::to_owned));
            }
        }
        lines.join("\n") + "\n"
    });
    assert_eq!(found, 2 * levels);
}

/// A ladder of `helpers` helpers above a function calling itself, each calling the one below,
/// and as many handlers, each calling one helper inside a `try`, the top one's first or last.
fn ladder_of_helpers(helpers: usize, top_first: bool) -> String {
    let mut lines = vec![
        "def rec(x):".to_owned(),
        "    return rec(x)".to_owned(),
        String::new(),
        String::new(),
        "def h0(x):".to_owned(),
        "    return rec(x)".to_owned(),
    ];
    for i in 1..helpers {
        lines.extend([
            String::new(),
            String::new(),
            format!("def h{i}(x):"),
            format!("    return h{}(x)", i - 1),
        ]);
    }
    let handlers: Vec<usize> = match top_first {
        true => (0..helpers).rev().collect(),
        false => (0..helpers).collect(),
    };
    for i in handlers {
        lines.extend([
            String::new(),
            String::new(),
            format!("def g{i}(x):"),
            "    try:".to_owned(),
            format!("        return h{i}(x)"),
            "    except Exception:".to_owned(),
            "        return None".to_owned(),
        ]);
    }
    lines.join("\n") + "\n"
}

/// A ladder of four thousand helpers above a function calling itself, and as many handlers, in
/// either order ([`ladder_of_helpers`]): each helper's first call down to the cycle is decided
/// once for all the chains through it, and a report lists the first eight calls of its chain
/// (four thousand calls for the top one).
#[test]
fn chains_down_a_ladder_of_helpers_are_walked_once_and_listed_in_part() {
    let n = 4_000;
    for top_first in [false, true] {
        let diags = check_linear(n, |helpers| {
            eff114_project(
                &format!("bounds_ladder_of_helpers_{top_first}"),
                helpers,
                &ladder_of_helpers(helpers, top_first),
            )
        });
        let reports: Vec<&Value> = diags
            .iter()
            .filter(|d| d["rule_code"] == "EFF114")
            .collect();
        assert_eq!(reports.len(), n);
        let related = |d: &Value| d["related"].as_array().unwrap().len();
        assert!(reports.iter().all(|d| related(d) <= 9));
        let top = reports
            .iter()
            .find(|d| d["qualname"] == format!("g{}", n - 1))
            .unwrap();
        assert_eq!(related(top), 9);
        assert!(
            top["message"]
                .as_str()
                .unwrap()
                .contains(&format!("the recursion of `rec` below `h{}`", n - 1))
        );
    }
}

/// A ladder of helpers above a function calling itself, longer than the steps of a small
/// function's budget, and two handlers calling a helper inside a `try`: the walk down from the top
/// helper spends the first handler's budget, which reports nothing and decides no helper; the
/// walk from four thousand helpers above the cycle fits the second handler's.
#[test]
fn chains_down_more_helpers_than_the_budget_steps_are_not_reported() {
    let shape = |n: usize| {
        let mut lines = vec![
            "def rec(x):".to_owned(),
            "    return rec(x)".to_owned(),
            String::new(),
            String::new(),
            "def h0(x):".to_owned(),
            "    return rec(x)".to_owned(),
        ];
        for i in 1..n {
            lines.extend([
                String::new(),
                String::new(),
                format!("def h{i}(x):"),
                format!("    return h{}(x)", i - 1),
            ]);
        }
        for (handler, helper) in [("top", n - 1), ("within", 3_999.min(n - 1))] {
            lines.extend([
                String::new(),
                String::new(),
                format!("def {handler}(x):"),
                "    try:".to_owned(),
                format!("        return h{helper}(x)"),
                "    except Exception:".to_owned(),
                "        return None".to_owned(),
            ]);
        }
        eff114_project(
            "bounds_ladder_past_the_budget",
            n,
            &(lines.join("\n") + "\n"),
        )
    };
    let reported: Vec<Value> = check_past_budgets(4_500, &["EFF114"], shape)
        .into_iter()
        .filter(|d| d["rule_code"] == "EFF114")
        .map(|d| d["qualname"].clone())
        .collect();
    assert_eq!(reported, ["within"]);
}

/// A clause of a tuple of three thousand classes, the last one taking `RecursionError`: each
/// class is looked up in the function's table of definitions, not found by a scan of the block
/// testing the clause.
#[test]
fn a_clause_of_thousands_of_classes_looks_each_up_once() {
    let found = recursion_handlers("bounds_wide_except_tuple", 3_000, |n| {
        let classes = vec!["KeyError"; n].join(", ");
        format!(
            "def rec(x):\n    return rec(x)\n\n\ndef shape(x):\n    try:\n        rec(x)\n    except ({classes}, Exception):\n        pass\n"
        )
    });
    assert_eq!(found, 1);
}

/// `MUT101` selected: the facts of its exact claim are computed (`OptInFacts::kept_values`).
const MUT101_CONFIG: &str = "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT101\"]\n";

/// The functions `MUT101` reports with [`MUT101_CONFIG`] in the module `m.py` of `source(n)`,
/// after checking its cost against that of `source(n / SCALE)`, with the budgets of `owners`
/// spent on the full shape ([`check_past_budgets`]).
fn kept_value_changes(
    name: &str,
    n: usize,
    owners: &[&str],
    source: impl Fn(usize) -> String,
) -> Vec<String> {
    check_past_budgets(n, owners, |n| {
        module_project(&format!("{name}_{n}"), MUT101_CONFIG, &source(n))
    })
    .iter()
    .filter(|d| d["rule_code"] == "MUT101")
    .map(|d| d["qualname"].as_str().unwrap().to_owned())
    .collect()
}

/// A `@cached_property` reading an attribute through a chain of thousands of `self` helpers,
/// each reading it on every path and calling the next: each helper's reads on every path come
/// from its own callee's summary once, not again along the chain.
#[test]
fn reads_on_every_path_through_a_long_chain_of_self_helpers_are_joined_once() {
    let found = kept_value_changes("bounds_must_read_chain", 3_000, &[], |helpers| {
        let mut lines = vec![
            "from functools import cached_property".to_owned(),
            String::new(),
            String::new(),
            "class C:".to_owned(),
            "    def __init__(self):".to_owned(),
            "        self.a = 0".to_owned(),
            String::new(),
            "    @cached_property".to_owned(),
            "    def p(self):".to_owned(),
            "        return self._h0()".to_owned(),
        ];
        for i in 0..helpers {
            lines.push(String::new());
            lines.push(format!("    def _h{i}(self):"));
            lines.push(format!("        return self.a + self._h{}()", i + 1));
        }
        lines.extend([
            String::new(),
            format!("    def _h{helpers}(self):"),
            "        return self.a".to_owned(),
            String::new(),
            "    def bump(self):".to_owned(),
            "        self.a += 1".to_owned(),
        ]);
        lines.join("\n") + "\n"
    });
    assert_eq!(found, vec!["C.bump"]);
}

/// A getter reading a thousand constant keys of one attribute, on alternating sides of
/// branches: each key's reads are asked together once, and past the function's budget nothing
/// is established.
#[test]
fn reads_on_every_path_of_many_keys_stay_within_the_function_budget() {
    let path_facts = "heki_effect::summary::Extractor::path_facts";
    let found = kept_value_changes("bounds_must_read_keys", 1_000, &[path_facts], |keys| {
        let mut lines = vec![
            "from functools import cached_property".to_owned(),
            String::new(),
            String::new(),
            "class C:".to_owned(),
            "    def __init__(self):".to_owned(),
            "        self.d = {}".to_owned(),
            "        self.flag = False".to_owned(),
            String::new(),
            "    @cached_property".to_owned(),
            "    def p(self):".to_owned(),
            "        total = 0".to_owned(),
        ];
        for i in 0..keys {
            lines.push("        if self.flag:".to_owned());
            lines.push(format!("            total += self.d[\"k{i}\"]"));
            lines.push("        else:".to_owned());
            lines.push(format!("            total -= self.d[\"k{i}\"]"));
        }
        lines.extend([
            "        return total".to_owned(),
            String::new(),
            "    def put(self, value):".to_owned(),
            "        self.d[\"k0\"] = value".to_owned(),
        ]);
        lines.join("\n") + "\n"
    });
    assert_eq!(found, vec!["C.put"]);
}

/// A writer counting through thousands of branches after its write, each merging two
/// arithmetic results of the one before: whether an operator's operand is a builtin value the
/// function built is decided once per variable for the whole function, not by a walk back
/// through the merges per operator.
#[test]
fn built_values_after_a_write_are_decided_once_per_variable() {
    let found = kept_value_changes("bounds_built_values", 1_000, &[], |branches| {
        let mut lines = vec![
            "from functools import cached_property".to_owned(),
            String::new(),
            String::new(),
            "class C:".to_owned(),
            "    def __init__(self):".to_owned(),
            "        self.a = 0".to_owned(),
            "        self.flag = False".to_owned(),
            String::new(),
            "    @cached_property".to_owned(),
            "    def p(self):".to_owned(),
            "        return self.a".to_owned(),
            String::new(),
            "    def bump(self):".to_owned(),
            "        self.a = 1".to_owned(),
            "        total = 0".to_owned(),
        ];
        for _ in 0..branches {
            lines.push("        if self.flag:".to_owned());
            lines.push("            total = total + 1".to_owned());
            lines.push("        else:".to_owned());
            lines.push("            total = -total".to_owned());
        }
        lines.push("        return None".to_owned());
        lines.join("\n") + "\n"
    });
    assert_eq!(found, vec!["C.bump"]);
}

/// A writer building a dict display of tens of thousands of keys after its write, each hashed:
/// what the keys' special methods may run, and whether the other operands an implementation
/// would be handed are inert, are decided once per key, not again per pair of keys.
#[test]
fn special_methods_of_a_large_display_after_a_write_are_decided_once_per_key() {
    let diagnostics = check_past_budgets(20_000, &[], |keys| {
        let entries: Vec<String> = (0..keys).map(|i| format!("\"k{i}\": {i}")).collect();
        let source = format!(
            "from functools import cached_property\n\n\nclass C:\n    def __init__(self):\n        \
             self.a = 0\n\n    @cached_property\n    def p(self):\n        return self.a\n\n    \
             def bump(self):\n        self.a = 1\n        table = {{{}}}\n        return table\n",
            entries.join(", ")
        );
        module_project(
            &format!("bounds_large_display_{keys}"),
            MUT101_CONFIG,
            &source,
        )
    });
    let found: Vec<(String, String)> = diagnostics
        .iter()
        .filter(|d| d["rule_code"] == "MUT101")
        .map(|d| {
            (
                d["qualname"].as_str().unwrap().to_owned(),
                d["certainty"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(found, vec![("C.bump".to_owned(), "exact".to_owned())]);
}

/// A writer making thousands of project calls and library calls after its write: whether a
/// callable a library call is handed has a site of its own is looked up once per argument, not
/// by a scan of every site of the function (which took time quadratic in its calls).
#[test]
fn library_calls_after_a_write_look_up_handed_callables_once() {
    let found = kept_value_changes("bounds_handed_callables", 4_000, &[], |calls| {
        let mut lines = vec![
            "from functools import cached_property".to_owned(),
            String::new(),
            String::new(),
            "def helper(x):".to_owned(),
            "    return x".to_owned(),
            String::new(),
            String::new(),
            "class C:".to_owned(),
            "    def __init__(self):".to_owned(),
            "        self.d = {}".to_owned(),
            String::new(),
            "    @cached_property".to_owned(),
            "    def p(self):".to_owned(),
            "        return self.d[\"k\"]".to_owned(),
            String::new(),
            "    def put(self, value):".to_owned(),
            "        self.d[\"k\"] = value".to_owned(),
        ];
        for _ in 0..calls {
            lines.push("        helper(value)".to_owned());
            lines.push("        len(value)".to_owned());
        }
        lines.join("\n") + "\n"
    });
    assert_eq!(found, ["C.put"]);
}

/// The parameters `p0, p1, ...` of a function of `params` parameters.
fn parameters(params: usize) -> String {
    (0..params)
        .map(|i| format!("p{i}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// A function constructing an object and writing its attribute, then handing thousands of its
/// parameters to a project helper: the construction walk takes a step per value it is asked
/// about, each parameter too, and past its budget no object is known to be new, so the write is
/// reported. With a few parameters the write constructs its object and is allowed.
#[test]
fn objects_past_the_construction_budget_are_not_new() {
    let shape = |params: usize| {
        let ps = parameters(params);
        let p = Project::new(&format!("bounds_construction_{params}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\"]\n\n\
             [[tool.heki.write-owners]]\nname = \"attrs\"\ntargets = [\"m.Model.attr\"]\n\
             allowed-writers = [\"m.Model.*\"]\n",
        )
        .write(
            "m.py",
            &format!(
                "class Model:\n    def __init__(self):\n        self.attr = 0\n\n\n\
                 def keep(*items):\n    return items\n\n\n\
                 def build({ps}):\n    m = Model()\n    m.attr = 1\n    keep({ps})\n    return m\n"
            ),
        );
        p
    };
    let writes = |found: &[Value]| found.iter().filter(|d| d["rule_code"] == "MUT201").count();
    assert_eq!(writes(&check(&shape(8))), 0);
    let found = check_past_budgets(4_096, &["construction::Recorder"], shape);
    assert_eq!(writes(&found), 1);
}

/// A function constructing an object, handing it to hundreds of project helpers, each call
/// under its own `if`, then writing its attribute: the data flow of what may keep the object
/// holds, at each block, the objects handed to every helper before it, so it grows with the
/// blocks times the helpers and passes the budget, and the write is reported. With a few
/// helpers it constructs its object and is allowed.
#[test]
fn objects_handed_to_many_helpers_pass_the_construction_budget() {
    let shape = |helpers: usize| {
        let mut lines = vec![
            "class Model:".to_owned(),
            "    def __init__(self):".to_owned(),
            "        self.attr = 0".to_owned(),
        ];
        for i in 0..helpers {
            lines.push(format!("def keep{i}(m):"));
            lines.push("    return 1".to_owned());
        }
        lines.push("def build(flags):".to_owned());
        lines.push("    m = Model()".to_owned());
        for i in 0..helpers {
            lines.push(format!("    if flags[{i}]:"));
            lines.push(format!("        keep{i}(m)"));
        }
        lines.push("    m.attr = 1".to_owned());
        lines.push("    return m".to_owned());
        let p = Project::new(&format!("bounds_construction_helpers_{helpers}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\"]\n\n\
             [[tool.heki.write-owners]]\nname = \"attrs\"\ntargets = [\"m.Model.attr\"]\n\
             allowed-writers = [\"m.Model.*\"]\n",
        )
        .write("m.py", &(lines.join("\n") + "\n"));
        p
    };
    let writes = |found: &[Value]| found.iter().filter(|d| d["rule_code"] == "MUT201").count();
    assert_eq!(writes(&check(&shape(64))), 0);
    let found = check_past_budgets(512, &["construction::Recorder"], shape);
    assert_eq!(writes(&found), 1);
}

/// A function of thousands of parameters a nested function captures: the walk to the values
/// that may be grown starts from each captured variable, and past its budget anything may be
/// grown, so the module list the function reads is no constant, and another function's query
/// per item of it is reported. With a few parameters the list keeps its size.
#[test]
fn values_past_the_budget_of_the_grown_variables_may_be_grown() {
    let shape = |params: usize| {
        let ps = parameters(params);
        let p = Project::new(&format!("bounds_grown_variables_{params}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"EFF103\", \"EFF113\"]\n",
        )
        .write(
            "m.py",
            &format!(
                "import sqlite3\n\nSTATUS = [\"a\", \"b\"]\n\n\n\
                 def query(conn: sqlite3.Connection):\n    for s in STATUS:\n        \
                 conn.execute(\"SELECT ?\", (s,))\n\n\n\
                 def build({ps}):\n    def inner():\n        return ({ps})\n    \
                 return inner, STATUS[0]\n"
            ),
        );
        p
    };
    let queries = |found: &[Value]| -> Vec<(String, u64)> {
        (found.iter())
            .map(|d| {
                (
                    d["rule_code"].as_str().unwrap().to_owned(),
                    d["line"].as_u64().unwrap(),
                )
            })
            .collect()
    };
    assert_eq!(queries(&check(&shape(8))), []);
    let grown_vars = "heki_ir::facts::FnIndex::grown_vars";
    let found = check_past_budgets(6_000, &[grown_vars], shape);
    assert_eq!(queries(&found), [("EFF113".to_owned(), 8)]);
}

/// Open premises along shared chains are decided once per call: thousands of `async def`s each
/// reach a sleep through the same chain of hundreds of helpers, none of which makes the next
/// call on every return. Each report walks only its own chain, and each call on it is looked up
/// and explained once for all of them.
#[test]
fn open_premises_along_shared_chains_are_explained_once_per_call() {
    // A chain of 200 helpers and 4,000 handlers at the full size: a walk of the chain per report
    // costs their product.
    let shape = |k: usize| {
        let (depth, handlers) = (25 * k, 500 * k);
        let mut lines = vec![
            "import time".to_owned(),
            String::new(),
            "def helper0(flag):".to_owned(),
            "    if flag:".to_owned(),
            "        return None".to_owned(),
            "    time.sleep(1)".to_owned(),
        ];
        for i in 1..depth {
            lines.push(format!("def helper{i}(flag):"));
            lines.push(format!("    helper{}(flag)", i - 1));
        }
        for j in 0..handlers {
            lines.push(format!("async def handler{j}(flag):"));
            lines.push(format!("    helper{}(flag)", depth - 1));
        }
        let p = Project::new(&format!("bounds_open_premises_chains_{k}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"EFF111\"]\n",
        )
        .write("m.py", &(lines.join("\n") + "\n"));
        p
    };
    check_linear(SCALE, shape);
    let out = heki_json(
        &shape(SCALE),
        &["dump-open-premises", "--select", "EFF111", "."],
    );
    let reports = out["reports"].as_array().unwrap();
    assert_eq!(reports.len(), 4000);
    assert_eq!(
        reports[0]["open_premises"][0]["cause"],
        json!("not-made-on-every-return")
    );
}

/// The open premises of many heuristic reports in one function are explained within budgets
/// proportional to it: hundreds of joins of one outer loop (`OPT113`) share one explanation
/// budget, as do a hundred accumulators reset on a path (`DATA101`), and hundreds of changes
/// of values that are a cached function's result on some paths only (`MUT114`) look the cached
/// function up in one table of the function's callees.
#[test]
fn open_premises_of_many_reports_in_one_function_are_explained_within_its_budgets() {
    // 400 joins, 100 accumulators, and 400 changes at the full size.
    let sizes = |k: usize| (50 * k, 100 * k / SCALE, 50 * k);
    let shape = |k: usize| {
        let (joins, accumulators, changes) = sizes(k);
        let mut lines = vec![
            "import functools".to_owned(),
            "import numpy as np".to_owned(),
        ];
        lines.push("def join(lefts, rights):".to_owned());
        lines.push("    out = []".to_owned());
        lines.push("    for a in lefts:".to_owned());
        for i in 0..joins {
            lines.push(format!("        for b{i} in rights:"));
            lines.push(format!("            if a.key == b{i}.key:"));
            lines.push("                out.append(a)".to_owned());
        }
        lines.push("    return out".to_owned());
        lines.push("def build(items: list[str]):".to_owned());
        for i in 0..accumulators {
            lines.push(format!("    acc{i} = np.array([])"));
        }
        lines.push("    for c in items:".to_owned());
        for i in 0..accumulators {
            lines.push(format!("        acc{i} = np.append(acc{i}, c)"));
            lines.push(format!("        if c == \"{i}\":"));
            lines.push(format!("            acc{i} = np.array([])"));
        }
        lines.push("    return acc0".to_owned());
        lines.push("@functools.lru_cache(maxsize=None)".to_owned());
        lines.push("def table(name) -> list[str]:".to_owned());
        lines.push("    return [name, \"id\"]".to_owned());
        lines.push("def use(name, flag):".to_owned());
        for i in 0..changes {
            lines.push(format!("    r{i} = table(name) if flag else []"));
            lines.push(format!("    r{i}.append(\"x\")"));
        }
        lines.push("    return None".to_owned());
        let p = Project::new(&format!("bounds_open_premises_one_function_{k}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"OPT113\", \"DATA101\", \"MUT114\"]\n",
        )
        .write("m.py", &(lines.join("\n") + "\n"));
        p
    };
    check_linear(SCALE, shape);
    let out = heki_json(
        &shape(SCALE),
        &[
            "dump-open-premises",
            "--select",
            "OPT113,DATA101,MUT114",
            ".",
        ],
    );
    let (joins, accumulators, changes) = sizes(SCALE);
    let reports = out["reports"].as_array().unwrap();
    let count = |rule: &str, cause: &str| {
        reports
            .iter()
            .filter(|r| {
                r["rule"] == rule
                    && r["open_premises"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|o| o["cause"] == cause)
            })
            .count()
    };
    assert_eq!(count("OPT113", "item-type-unknown"), joins);
    assert_eq!(count("DATA101", "reset-on-a-path"), accumulators);
    assert_eq!(count("MUT114", "not-the-result-on-every-path"), changes);
}

/// A function binding names each to an item of the one before (`e = e["k"]`) five thousand deep,
/// each changed in place, under a `mutates-params` boundary: which parameter's items each changed
/// object may be is decided once per value, not by a walk back up the chain at every change.
#[test]
fn changes_down_a_long_item_chain_decide_each_element_once() {
    let shape = |links: usize| {
        let mut lines = vec!["def walk(p):".to_owned(), "    e = p".to_owned()];
        for i in 0..links {
            lines.push("    e = e[\"k\"]".to_owned());
            lines.push(format!("    e.append({i})"));
        }
        let p = Project::new(&format!("bounds_element_chain_{links}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[[tool.heki.effect-boundaries]]\nname = \"pure\"\nscopes = [\"m.*\"]\nforbid = [\"mutates-params\"]\n",
        )
        .write("m.py", &(lines.join("\n") + "\n"));
        p
    };
    let found: Vec<u64> = check_linear(5_000, shape)
        .iter()
        .filter(|d| d["rule_code"] == "EFF201")
        .map(|d| d["line"].as_u64().unwrap())
        .collect();
    assert_eq!(found, vec![1]);
}

/// A scope under a `mutates-params` boundary calling two thousand helpers in turn before handing
/// its parameter to the last of them, each helper handing its own to the one before, the first
/// changing it: the calls are found in the scope's order, against the order the parameter flows
/// in. Which scope parameter each helper's parameters hold is joined from the calls into it, a
/// helper visited again only when what it is handed grows, not in passes over every call until
/// nothing changes.
#[test]
fn parameter_changes_down_a_chain_found_in_reverse_are_joined_once() {
    let shape = |helpers: usize| {
        let mut lines = vec!["def h0(x):".to_owned(), "    x.append(1)".to_owned()];
        for i in 1..=helpers {
            lines.push(format!("def h{i}(x=None):"));
            lines.push(format!("    h{}(x)", i - 1));
        }
        lines.push("def entry(p):".to_owned());
        lines.extend((0..helpers).map(|i| format!("    h{i}()")));
        lines.push(format!("    h{helpers}(p)"));
        let p = Project::new(&format!("bounds_reversed_param_chain_{helpers}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"EFF201\"]\n\n[[tool.heki.effect-boundaries]]\nname = \"pure\"\nscopes = [\"m.entry\"]\nforbid = [\"mutates-params\"]\n",
        )
        .write("m.py", &(lines.join("\n") + "\n"));
        p
    };
    let found: Vec<String> = check_linear(2_000, shape)
        .iter()
        .filter(|d| d["rule_code"] == "EFF201")
        .map(|d| d["qualname"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(found, vec!["entry"]);
}

/// A scope under a `mutates-params` boundary changing its parameter and calling a helper, then a
/// thousand parameterless functions each calling that helper: a caller handing on no parameter
/// holds no fact, so the helper is not visited again for each of them, and the scope's own change
/// is reported.
#[test]
fn many_callers_handing_on_no_parameter_leave_a_scopes_change_reported() {
    let shape = |callers: usize| {
        let mut lines = vec![
            "def sink():".to_owned(),
            "    pass".to_owned(),
            "def entry(p):".to_owned(),
            "    p.append(1)".to_owned(),
            "    sink()".to_owned(),
        ];
        lines.extend((0..callers).map(|i| format!("    c{i}()")));
        for i in 0..callers {
            lines.push(format!("def c{i}():"));
            lines.push("    sink()".to_owned());
        }
        let p = Project::new(&format!("bounds_callers_handing_no_parameter_{callers}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"EFF201\"]\n\n[[tool.heki.effect-boundaries]]\nname = \"pure\"\nscopes = [\"m.entry\"]\nforbid = [\"mutates-params\"]\n",
        )
        .write("m.py", &(lines.join("\n") + "\n"));
        p
    };
    let found: Vec<String> = check_linear(1_000, shape)
        .iter()
        .filter(|d| d["rule_code"] == "EFF201")
        .map(|d| d["qualname"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(found, vec!["entry"]);
}

/// A function writing an invalidation target four thousand times, each write followed by a call
/// of a helper that announces nothing (`MUT109`): what the calls after a write announce is
/// answered once per call site along the chain of next calls, not walked to the end again for
/// each write.
#[test]
fn writes_before_many_calls_follow_the_calls_after_them_once() {
    let shape = |writes: usize| {
        let mut lines = vec![
            "class Model:".to_owned(),
            "    def __init__(self):".to_owned(),
            "        self.value = 0".to_owned(),
            "def helper():".to_owned(),
            "    pass".to_owned(),
            "def notify():".to_owned(),
            "    pass".to_owned(),
            "def touch(model: Model):".to_owned(),
        ];
        for i in 0..writes {
            lines.push(format!("    model.value = {i}"));
            lines.push("    helper()".to_owned());
        }
        let p = Project::new(&format!("bounds_writes_before_calls_{writes}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT109\"]\n\n[[tool.heki.invalidation]]\nname = \"m\"\ntargets = [\"Model.value\"]\ninvalidators = [\"m.notify\"]\n",
        )
        .write("m.py", &(lines.join("\n") + "\n"));
        p
    };
    let found = check_linear(4_000, shape);
    assert_eq!(
        found.iter().filter(|d| d["rule_code"] == "MUT109").count(),
        4_000
    );
}

/// A loop over two thousand `if` statements, each writing an invalidation target and calling a
/// helper, every other one announcing the write (`MUT109`): what every path from a point
/// announces is a data flow over the blocks, and the next call on every path comes from the
/// post-dominators, each block visited a bounded number of times. The writes announced on no
/// path past them are reported.
#[test]
fn writes_under_many_branches_of_a_loop_decide_each_block_a_few_times() {
    // The source, and the lines of the writes no `notify()` follows.
    let source = |writes: usize| {
        let mut lines = vec![
            "class Model:".to_owned(),
            "    def __init__(self):".to_owned(),
            "        self.value = 0".to_owned(),
            "def helper():".to_owned(),
            "    pass".to_owned(),
            "def notify():".to_owned(),
            "    pass".to_owned(),
            "def touch(model: Model, items):".to_owned(),
            "    for i in items:".to_owned(),
        ];
        let mut unannounced = Vec::new();
        for i in 0..writes {
            lines.push(format!("        if i == {i}:"));
            lines.push(format!("            model.value = {i}"));
            if i % 2 == 0 {
                lines.push("            helper()".to_owned());
                lines.push("            notify()".to_owned());
            } else {
                unannounced.push(lines.len() as u64);
                lines.push("            helper()".to_owned());
            }
        }
        (lines.join("\n") + "\n", unannounced)
    };
    let shape = |writes: usize| {
        let p = Project::new(&format!("bounds_writes_under_branches_{writes}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT109\"]\n\n[[tool.heki.invalidation]]\nname = \"m\"\ntargets = [\"Model.value\"]\ninvalidators = [\"m.notify\"]\n",
        )
        .write("m.py", &source(writes).0);
        p
    };
    let found: Vec<u64> = check_linear(2_000, shape)
        .iter()
        .filter(|d| d["rule_code"] == "MUT109")
        .map(|d| d["line"].as_u64().unwrap())
        .collect();
    assert_eq!(found, source(2_000).1);
}

/// Five thousand shallow copies of one parameter, each changing one of its items, and a loop
/// rebinding an item of a copy on one branch per test before changing it: which copy each
/// changed object is below, and whether a store before it rebinds its level, are decided once
/// per value, not by a walk over every store at every load.
#[test]
fn changes_below_many_copies_decide_each_copy_once() {
    // 5,000 copies and 2,000 tests at the full size.
    let shape = |k: usize| {
        let (copies, tests) = (625 * k, 250 * k);
        let mut lines = vec!["def copies(p: dict[str, list[int]]):".to_owned()];
        for i in 0..copies {
            lines.push(format!("    c{i} = dict(p)"));
            lines.push(format!("    c{i}[\"k\"].append({i})"));
        }
        lines.extend(
            [
                "",
                "",
                "def rebound(p: dict[str, list[int]], xs):",
                "    c = dict(p)",
                "    for x in xs:",
            ]
            .map(str::to_owned),
        );
        for i in 0..tests {
            lines.push(format!("        if x == {i}:"));
            lines.push("            c[\"k\"] = []".to_owned());
        }
        lines.push("        c[\"k\"].append(x)".to_owned());
        let p = Project::new(&format!("bounds_copies_{k}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"MUT115\"]\n",
        )
        .write("m.py", &(lines.join("\n") + "\n"));
        p
    };
    let found = check_linear(SCALE, shape)
        .iter()
        .filter(|d| d["rule_code"] == "MUT115")
        .count();
    assert_eq!(found, 5_000 + 1);
}

/// A copied class with a chain of two thousand subclasses, copied and changed below the copy in
/// a thousand functions: the classes a value of it may be are walked once per question, not
/// once per subclass along each subclass's bases.
#[test]
fn copies_of_a_class_with_a_deep_subclass_chain_walk_each_class_once() {
    // A chain of 2,000 classes and 1,000 functions at the full size: a walk along each class's
    // bases per function costs their product.
    let shape = |k: usize| {
        let (depth, functions) = (250 * k, 125 * k);
        let mut lines = vec![
            "import copy".to_owned(),
            String::new(),
            String::new(),
            "class C0:".to_owned(),
            "    def __init__(self) -> None:".to_owned(),
            "        self.children: list[int] = []".to_owned(),
        ];
        for i in 1..depth {
            lines.push(String::new());
            lines.push(String::new());
            lines.push(format!("class C{i}(C{}):", i - 1));
            lines.push("    pass".to_owned());
        }
        for i in 0..functions {
            lines.push(String::new());
            lines.push(String::new());
            lines.push(format!("def adopt{i}(node: C0) -> None:"));
            lines.push("    clone = copy.copy(node)".to_owned());
            lines.push("    clone.children.append(1)".to_owned());
        }
        let p = Project::new(&format!("bounds_copied_hierarchy_{k}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"MUT115\"]\n",
        )
        .write("m.py", &(lines.join("\n") + "\n"));
        p
    };
    let found = check_linear(SCALE, shape)
        .iter()
        .filter(|d| d["rule_code"] == "MUT115")
        .count();
    assert_eq!(found, 1_000);
}

/// `heki dump-recommend-facts` of the project. The facts only `heki recommend` reads are walked
/// by no owner `heki check --walk-steps` counts.
fn recommend_facts(p: &Project) -> Value {
    heki_json(p, &["dump-recommend-facts", "."])
}

/// A chain of `links` helpers, each calling the next and, in a loop, a library name of its own
/// that the catalog lacks and a method of its unannotated parameter, below as many `async def`s
/// that each call the head of the chain.
fn blind_calls_below_async_defs(links: usize) -> String {
    let mut lines = vec!["import stripe".to_owned()];
    for i in 0..links {
        lines.extend([
            String::new(),
            String::new(),
            format!("def h{i}(client, items):"),
            "    for item in items:".to_owned(),
            format!("        stripe.f{i}(item)"),
            format!("        client.m{i}(item)"),
        ]);
        if i + 1 < links {
            lines.push(format!("    h{}(client, items)", i + 1));
        }
        lines.extend([
            String::new(),
            String::new(),
            format!("async def entry{i}(client, items):"),
            "    h0(client, items)".to_owned(),
        ]);
    }
    lines.join("\n") + "\n"
}

/// Which functions run on an event loop and which below a loop is one walk each over the calls,
/// whatever the number of blind calls and of `async def`s above them: one walk per blind call
/// took time quadratic in the chain.
#[test]
fn the_reach_of_blind_calls_is_one_walk_over_the_calls() {
    let links = 2_000;
    let p = Project::new("bounds_blind_calls_below_async_defs");
    p.write("app.py", &blind_calls_below_async_defs(links));
    let facts = recommend_facts(&p);
    let totals = &facts["blind_spots"]["totals"];
    let all = json!({"calls": links, "awaited": 0, "on_event_loop": links, "in_loops": links});
    assert_eq!(totals["uncatalogued"], all, "{totals:#}");
    assert_eq!(totals["unresolved"], all, "{totals:#}");
    assert_eq!(totals["uncatalogued_symbols"], links);
    // Every function reaches a blind call: one closure over the calls.
    let module = &facts["census"]["modules"][0];
    assert_eq!(module["unseen"].as_array().unwrap().len(), 2 * links);
}

/// `classes` classes, each initializing three attributes, a module of as many functions each
/// changing the attributes of one class, and one of as many functions each reading them.
fn classes_written_in_one_module(classes: usize) -> [(String, String); 3] {
    let mut models = Vec::new();
    let mut store = vec!["import app.models as m".to_owned()];
    let mut views = vec!["import app.models as m".to_owned()];
    for i in 0..classes {
        models.extend([
            format!("class C{i}:"),
            "    def __init__(self):".to_owned(),
            "        self.items = []".to_owned(),
            "        self.state = 0".to_owned(),
            "        self.name = ''".to_owned(),
            String::new(),
        ]);
        store.extend([
            String::new(),
            format!("def put{i}(c: m.C{i}, x):"),
            "    c.items.append(x)".to_owned(),
            "    c.state = x".to_owned(),
        ]);
        views.extend([
            String::new(),
            format!("def show{i}(c: m.C{i}):"),
            "    return c.items, c.state, c.name".to_owned(),
        ]);
    }
    [
        ("app/models.py".to_owned(), models.join("\n") + "\n"),
        ("app/store.py".to_owned(), store.join("\n") + "\n"),
        ("app/views.py".to_owned(), views.join("\n") + "\n"),
    ]
}

/// The census of the attributes of thousands of classes is one pass over the functions' own
/// sites, grouped by class and attribute.
#[test]
fn the_census_of_many_classes_is_one_pass_over_their_sites() {
    let classes = 3_000;
    let p = Project::new("bounds_census_classes");
    for (path, text) in classes_written_in_one_module(classes) {
        p.write(&path, &text);
    }
    let facts = recommend_facts(&p);
    let totals = &facts["census"]["totals"];
    assert_eq!(
        totals,
        &json!({
            "classes": classes, "attributes": 3 * classes, "write_sites": 2 * classes,
            "initializer_sites": 3 * classes, "read_sites": 4 * classes, "modules": 3,
            "functions": 3 * classes,
        }),
        "{totals:#}"
    );
}

/// `heki recommend --output-format json` of the project, which counts no walk steps: its shapes
/// bound how many trials it runs.
fn recommend(p: &Project) -> Value {
    heki_json(p, &["recommend", "--output-format", "json", "."])
}

/// Thousands of `async def`s, each calling a library name of its own that the catalog lacks:
/// the hypothetical-catalog run gives entries to the first 64 by reach only, and the lists keep
/// their first 50.
#[test]
fn the_trial_of_library_names_without_an_entry_tries_a_bounded_number() {
    let names = 3_000;
    let mut lines = vec!["import stripe".to_owned()];
    for i in 0..names {
        lines.extend([
            String::new(),
            String::new(),
            format!("async def e{i}(x):"),
            format!("    stripe.f{i}(x)"),
        ]);
    }
    let p = Project::new("bounds_recommend_uncatalogued");
    p.write("app.py", &(lines.join("\n") + "\n"));
    let report = recommend(&p);
    let blind = &report["blind_spots"];
    let totals = &blind["totals"];
    assert_eq!(
        (
            &totals["uncatalogued_symbols"],
            &totals["tried"],
            &totals["not_tried"]
        ),
        (&json!(names), &json!(64), &json!(names - 64)),
        "{totals:#}"
    );
    let symbols = blind["uncatalogued"][0]["symbols"].as_array().unwrap();
    assert_eq!(symbols.len(), 50);
    assert!(
        symbols
            .iter()
            .all(|s| s["if_waits"] == json!({"EFF101": 1, "EFF111": 0})),
        "{symbols:#?}"
    );
    assert_eq!(blind["truncated"], json!(true));
}

/// Thousands of directories, each holding a module that a top-level file imports by a name
/// below it: each directory is a source-root candidate, and the trial tries the first eight.
#[test]
fn the_source_root_trial_tries_a_bounded_number_of_directories() {
    let dirs = 2_000;
    let p = Project::new("bounds_recommend_source_roots");
    let mut main = String::new();
    for i in 0..dirs {
        p.write(&format!("d{i}/m{i}.py"), "def f():\n    pass\n");
        main.push_str(&format!("import m{i}\n"));
    }
    p.write("main.py", &main);
    let report = recommend(&p);
    let blind = &report["blind_spots"];
    assert_eq!(blind["totals"]["source_root_candidates"], json!(dirs));
    let roots = blind["source_roots"].as_array().unwrap();
    assert_eq!(roots.len(), 50);
    let tried: Vec<&Value> = roots.iter().filter(|r| r["in_block"] == true).collect();
    assert_eq!(tried.len(), 8);
    assert_eq!(
        roots[8]["reason"],
        json!("not tried: only the first 8 directories by imports are")
    );
    assert_eq!(report["config"]["src"].as_array().unwrap().len(), 8);
}

/// The `write-owners` candidates of thousands of classes, each attribute written in one module
/// and read in two others, are ranked and capped before the contract trial: the trial declares
/// the first 32.
#[test]
fn contract_candidates_of_thousands_of_classes_are_capped_before_the_trial() {
    let classes = 3_000;
    let p = Project::new("bounds_recommend_classes");
    let [models, store, views] = classes_written_in_one_module(classes);
    let api = views.1.replace("def show", "def dump");
    for (path, text) in [models, store, views, ("app/api.py".to_owned(), api)] {
        p.write(&path, &text);
    }
    let report = recommend(&p);
    // `items` and `state` written by the store, `name` only while constructed.
    assert_eq!(
        report["contracts_not_tried"],
        json!([{"kind": "write-owners", "tried": 32, "candidates": 3 * classes - 32}])
    );
    let tried = report["contracts"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["kind"] == "write-owners")
        .count();
    assert_eq!(tried, 32);
    assert_eq!(
        report["runs"],
        json!([{"label": "discovery"}, {"label": "contract-trial"},
               {"label": "verification"}])
    );
}

/// Thousands of modules in packages of fifty, whose functions reach no blocking call, next to
/// one that does: each package is one scope, found from the modules' names in one pass, and
/// the trial declares the first 32 scopes.
#[test]
fn boundary_candidates_of_thousands_of_modules_are_one_pass_over_their_names() {
    let (modules, packages) = (2_000, 40);
    let p = Project::new("bounds_recommend_modules");
    p.write("app/__init__.py", "").write(
        "app/net.py",
        "import requests\n\n\ndef get(u):\n    return requests.get(u, timeout=5)\n",
    );
    let body: String = (0..3)
        .map(|j| format!("def f{j}(x):\n    return x + {j}\n\n\n"))
        .collect();
    for i in 0..modules {
        p.write(&format!("app/p{}/__init__.py", i % packages), "")
            .write(&format!("app/p{}/m{i}.py", i % packages), &body);
    }
    let report = recommend(&p);
    assert_eq!(
        report["contracts_not_tried"],
        json!([{"kind": "effect-boundaries", "tried": 32, "candidates": packages - 32}])
    );
    let first = &report["contracts"][0];
    assert_eq!(
        (&first["target"], &first["evidence"]["modules"]["count"]),
        (&json!("app.p0"), &json!(modules / packages + 1))
    );
}

/// A list front-inserted at `inserts` sites of a loop, then bound back and forth between two
/// variables `16 * inserts` times after the loop: every insert is a candidate of the deque
/// rewrite (`OPT121`), and each checks every binding of both variables.
fn front_inserts_then_rebindings(inserts: usize) -> String {
    let mut lines = vec![
        "def fill(items):".to_owned(),
        "    q = []".to_owned(),
        "    for x in items:".to_owned(),
    ];
    lines.extend((0..inserts).map(|_| "        q.insert(0, x)".to_owned()));
    for _ in 0..16 * inserts {
        lines.push("    r = q".to_owned());
        lines.push("    q = r".to_owned());
    }
    lines.push("    return list(q)".to_owned());
    lines.join("\n") + "\n"
}

/// The deque rewrite's candidates of one function share one budget and one answer per value
/// about the objects the variables a list reaches may hold: a binding is walked once, not once
/// per candidate, and every insert keeps its fix.
#[test]
fn deque_candidates_of_one_list_walk_its_bindings_once() {
    let found = check_linear(16, |inserts| {
        let p = Project::new(&format!("bounds_deque_bindings_{inserts}"));
        p.write("m.py", &front_inserts_then_rebindings(inserts));
        p
    });
    let fixed = found
        .iter()
        .filter(|d| d["rule_code"] == "OPT121" && !d["fix"].is_null())
        .count();
    assert_eq!(fixed, 16, "{found:#?}");
}

/// `OPT103` and `OPT111` selected.
const OPT_CONFIG: &str =
    "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"OPT103\", \"OPT111\"]\n";

/// The iterables of the loops of [`rewrite_candidates_over_mixed_iterables`]: parameters, a
/// structural attribute, and the end of a chain of copies read plain, as text, and as pieces.
const MIXED_ITERABLES: [&str; 8] = [
    "items",
    "self.layers",
    "{z}",
    "{z}.upper()",
    "{z}.split()",
    "{z}[0]",
    "extra",
    "self.layers",
];

/// One method of eight loops over iterables of mixed origin (a parameter, `self.layers`, and the
/// last of `chain` copies of a parameter), each holding a tuple concatenation and a
/// de-duplicating append to a list of its own.
fn rewrite_candidates_over_mixed_iterables(chain: usize) -> String {
    let mut lines = vec![
        "class Net:".to_owned(),
        "    def __init__(self):".to_owned(),
        "        self.layers = [1, 2, 3]".to_owned(),
        String::new(),
        "    def run(self, items, extra, source):".to_owned(),
        "        z0 = source".to_owned(),
    ];
    for i in 1..=chain {
        lines.push(format!("        z{i} = z{}", i - 1));
    }
    let end = format!("z{chain}");
    for (i, iterable) in MIXED_ITERABLES.iter().enumerate() {
        lines.push(format!("        t{i} = ()"));
        lines.push(format!("        seen{i} = []"));
        lines.push(format!(
            "        for x{i} in {}:",
            iterable.replace("{z}", &end)
        ));
        lines.push(format!("            t{i} = t{i} + (x{i},)"));
        lines.push(format!("            if x{i} not in seen{i}:"));
        lines.push(format!("                seen{i}.append(x{i})"));
    }
    lines.push("        return t0".to_owned());
    lines.join("\n") + "\n"
}

fn rewrite_candidates_project(name: &str, chain: usize) -> Project {
    let p = Project::new(name);
    p.write("pyproject.toml", OPT_CONFIG)
        .write("m.py", &rewrite_candidates_over_mixed_iterables(chain));
    p
}

/// The trip counts of the rewrite candidates of one method (`OPT103`, `OPT111`) are asked of
/// loops sharing a long chain of copies and iterables of mixed origin within one walk budget
/// proportional to the method, and every candidate is still reported.
#[test]
fn rewrite_candidates_over_loops_sharing_iterables_take_linear_time() {
    // 400 copies at the full size.
    let found = check_linear(SCALE, |k| {
        rewrite_candidates_project(&format!("bounds_opt_rewrites_{k}"), 50 * k)
    });
    let count = |rule: &str| found.iter().filter(|d| d["rule_code"] == rule).count();
    assert_eq!(count("OPT103"), MIXED_ITERABLES.len(), "{found:#?}");
    assert_eq!(count("OPT111"), MIXED_ITERABLES.len(), "{found:#?}");
}

/// The open premises of those candidates are explained by the same walks: every report is
/// listed, and each de-duplicating append names what it leaves open.
#[test]
fn open_premises_of_rewrite_candidates_over_shared_iterables_are_explained() {
    let p = rewrite_candidates_project("bounds_opt_rewrites_premises", 400);
    let out = heki_json(
        &p,
        &["dump-open-premises", "--select", "OPT103,OPT111", "."],
    );
    let reports = out["reports"].as_array().unwrap();
    assert_eq!(reports.len(), 2 * MIXED_ITERABLES.len(), "{out:#?}");
    for r in reports.iter().filter(|r| r["rule"] == "OPT111") {
        assert_eq!(r["certainty"], "heuristic", "{r:#?}");
        assert!(!r["open_premises"].as_array().unwrap().is_empty(), "{r:#?}");
    }
}
