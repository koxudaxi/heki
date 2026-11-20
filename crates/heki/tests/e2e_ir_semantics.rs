//! Python semantics the IR models, end to end through the real binary: each shape is checked by
//! the rule that reads the fact (a report where CPython runs the effect, none where it does not)
//! and, where the rules cannot show it, by the IR itself (`heki dump-ir`). The Soundness lane's
//! counterexamples of the same shapes are in `e2e_soundness.rs`
//! (`tests/difftest/ir_counterexamples.py`).

mod common;

use common::Project;
use serde_json::Value;

/// `heki check --output-format json <args>` of the project.
fn diagnostics(p: &Project, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["--output-format", "json"];
    all.extend_from_slice(args);
    let (code, stdout, stderr) = p.check(&all);
    assert!(code == 0 || code == 1, "exit {code}: {stderr}");
    serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"))
}

/// The `(qualname, rule)` pairs `rules` report on the single module `source`.
fn reports(name: &str, source: &str, rules: &str) -> Vec<(String, String)> {
    let p = Project::new(name);
    p.write("case.py", source);
    diagnostics(&p, &["--select", rules])
        .iter()
        .map(|d| {
            (
                d["qualname"].as_str().unwrap().to_owned(),
                d["rule_code"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(q, c)| ((*q).to_owned(), (*c).to_owned()))
        .collect()
}

/// `heki dump-ir` of `source` for Python `version`.
fn dump_ir(name: &str, source: &str, version: &str) -> Value {
    let p = Project::new(name);
    p.write("m.py", source);
    let (code, stdout, stderr) = p.heki(&["dump-ir", "m.py", "--target-version", version]);
    assert_eq!(code, 0, "{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

fn function<'a>(ir: &'a Value, qualname: &str) -> &'a Value {
    ir["functions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["qualname"] == qualname)
        .unwrap_or_else(|| panic!("no function `{qualname}`"))
}

// ======================================================================= replacement fields

#[test]
fn a_yield_in_a_replacement_field_makes_a_generator() {
    let src = "\
import time


def rows():
    f\"{(yield 1)}\"
    time.sleep(1)


async def creates():
    rows()


async def iterates():
    for _ in rows():
        pass
";
    let ir = dump_ir("ir_fstring_yield", src, "py315");
    assert_eq!(function(&ir, "rows")["kind"], "generator");
    // Calling a generator function only creates the generator; iterating it runs the body.
    assert_eq!(
        reports("ir_fstring_yield", src, "EFF101"),
        pairs(&[("iterates", "EFF101")])
    );
}

// ======================================================================== scopes and frames

#[test]
fn a_yield_in_a_definition_header_makes_the_enclosing_function_a_generator() {
    let src = "\
import time


def rows():
    def unused(x=(yield)):
        pass
    time.sleep(1)


async def creates():
    rows()
";
    let ir = dump_ir("ir_header_yield", src, "py315");
    assert_eq!(function(&ir, "rows")["kind"], "generator");
    assert_eq!(function(&ir, "rows.<locals>.unused")["kind"], "sync");
    assert!(reports("ir_header_yield", src, "EFF101").is_empty());
}

#[test]
fn scopes_nested_in_a_comprehension_read_its_iteration_variable() {
    let src = "\
import time


def item():
    time.sleep(1)


async def generator_in_comprehension(items):
    return [list(item() for _ in (0,)) for item in items]


async def lambda_in_comprehension(items):
    return [(lambda: item())() for item in items]


async def module_function(items):
    return [list(item() for _ in (0,)) for other in items]
";
    // Inside the comprehension `item` is its iteration variable (a cell of the frame), not
    // the module's blocking function.
    assert_eq!(
        reports("ir_comprehension_cells", src, "EFF101"),
        pairs(&[("module_function", "EFF101")])
    );
    let ir = dump_ir("ir_comprehension_cells", src, "py315");
    let captured = function(&ir, "lambda_in_comprehension")["captured_vars"].clone();
    assert_eq!(captured.as_array().unwrap().len(), 1, "{captured}");
    assert!(
        captured[0].as_str().unwrap().ends_with("_item"),
        "{captured}"
    );
}

// =================================================================== evaluation order

#[test]
fn a_chained_comparison_evaluates_each_operand_once_and_stops_at_a_false_link() {
    let src = "def chain(a, b, c):\n    return a < (b) <= c\n";
    let ir = dump_ir("ir_chained_compare", src, "py315");
    let f = function(&ir, "chain");
    let compares: Vec<&Value> = f["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|b| b["insts"].as_array().unwrap())
        .filter(|i| i["kind"].get("Compare").is_some())
        .collect();
    let text = |i: &Value| {
        let (s, e) = (
            i["span"]["start"].as_u64().unwrap(),
            i["span"]["end"].as_u64().unwrap(),
        );
        &src[s as usize..e as usize]
    };
    assert_eq!(
        compares.iter().map(|i| text(i)).collect::<Vec<_>>(),
        ["a < (b", "b) <= c"]
    );
    // `b` is evaluated once: the second comparison reads the first one's right operand.
    assert_eq!(
        compares[0]["kind"]["Compare"]["rhs"],
        compares[1]["kind"]["Compare"]["lhs"]
    );
    // The first comparison's block branches to the second or to the result.
    let first_block = f["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| {
            b["insts"]
                .as_array()
                .unwrap()
                .iter()
                .any(|i| i == compares[0])
        })
        .unwrap();
    assert!(
        first_block["terminator"].get("Branch").is_some(),
        "{first_block:#}"
    );
}

// ============================================================================ match patterns

const INVALIDATION: &str = "[project]\nname = \"t\"\n\n[[tool.heki.invalidation]]\nname = \"m\"\ntargets = [\"Model.value\"]\ninvalidators = [\"case.notify\"]\n";

const MODEL: &str =
    "class Model:\n    def __init__(self):\n        self.value = 0\n\n\ndef notify():\n    pass\n";

#[test]
fn a_refutable_case_may_not_match() {
    let src = format!(
        "{MODEL}\n\ndef value_case(model: Model, code):\n    model.value = 1\n    match code:\n        case 0:\n            notify()\n\n\ndef class_case(model: Model, code):\n    model.value = 1\n    match code:\n        case int():\n            notify()\n\n\ndef wildcard(model: Model, code):\n    model.value = 1\n    match code:\n        case 0:\n            notify()\n        case _:\n            notify()\n\n\ndef capture(model: Model, code):\n    model.value = 1\n    match code:\n        case found:\n            notify()\n"
    );
    let p = Project::new("ir_refutable_cases");
    p.write("pyproject.toml", INVALIDATION)
        .write("case.py", &src);
    let got: Vec<String> = diagnostics(&p, &["--select", "MUT109"])
        .iter()
        .map(|d| d["qualname"].as_str().unwrap().to_owned())
        .collect();
    // A path where `code` matches no case returns without the invalidator; a wildcard or
    // capture pattern always matches.
    assert_eq!(got, ["value_case", "class_case"]);
}

#[test]
fn patterns_bind_the_matched_items() {
    let src = "def items(subject):\n    match subject:\n        case [first, *middle, last]:\n            return first, middle, last\n        case {\"k\": value, **rest}:\n            return value, rest\n";
    let ir = dump_ir("ir_pattern_items", src, "py315");
    let f = function(&ir, "items");
    let insts: Vec<&Value> = f["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|b| b["insts"].as_array().unwrap())
        .collect();
    let def_of = |var: &Value| {
        insts
            .iter()
            .find(|i| i["kind"].as_object().unwrap().values().next().unwrap()["dst"] == *var)
            .map(|i| i["kind"].clone())
    };
    let copy_of = |name: &str| -> Value {
        let idx = f["var_names"]
            .as_array()
            .unwrap()
            .iter()
            .position(|n| n == name)
            .unwrap();
        def_of(&Value::from(idx)).unwrap()["Copy"]["src"].clone()
    };
    let subscript_index = |name: &str| {
        let load = def_of(&copy_of(name)).unwrap();
        let index = &load["SubscriptLoad"]["index"];
        def_of(index).unwrap()["Const"]["literal"]["int"]
            .as_i64()
            .unwrap()
    };
    assert_eq!(subscript_index("first"), 0);
    assert_eq!(subscript_index("last"), -1);
    assert!(def_of(&copy_of("middle")).unwrap()["AllocBuiltin"]["kind"] == "list");
    assert!(
        def_of(&copy_of("rest"))
            .unwrap()
            .get("DictLiteral")
            .is_some()
    );
    let value = def_of(&copy_of("value")).unwrap();
    assert_eq!(value["SubscriptLoad"]["key_const"], "k");
}

// ===================================================================== frame introspection

#[test]
fn a_reference_to_an_introspection_function_is_an_a1_hazard() {
    let src = "\
import inspect as ins
from inspect import currentframe
from builtins import locals as snap_import

snap = locals


def alias(items):
    snapshot = locals
    return snapshot()


def module_alias():
    return snap()


def imported():
    return currentframe(), snap_import()


def module_attribute():
    frame = ins.currentframe
    return frame()


def passed(codes):
    return list(map(eval, codes))


def vars_alias():
    v = vars
    return v()


def local_named_locals():
    locals = {}
    return locals


def vars_of(obj):
    return vars(obj)


def local_import():
    from builtins import locals as here
    return here()


def unrelated_here():
    return here()
";
    let p = Project::new("ir_a1_references");
    p.write("m.py", src);
    let (_, stdout, _) = p.heki(&["assumptions", "."]);
    let hazards: Vec<&str> = stdout
        .lines()
        .filter(|l| l.contains("A1 hazard:"))
        .map(|l| l.split_once(' ').unwrap().1)
        .collect();
    // A variable named `locals`, `vars(obj)`, and another function's local `here` read no
    // frame.
    assert_eq!(
        hazards,
        [
            "[<module>] A1 hazard: locals",
            "[alias] A1 hazard: locals",
            "[module_alias] A1 hazard: locals",
            "[imported] A1 hazard: inspect.currentframe",
            "[imported] A1 hazard: locals",
            "[module_attribute] A1 hazard: inspect.currentframe",
            "[passed] A1 hazard: eval",
            "[vars_alias] A1 hazard: vars()",
            "[local_import] A1 hazard: locals",
        ],
        "{stdout}"
    );
}

// ================================================================================== del

#[test]
fn only_deleting_the_global_unfixes_a_module_constant() {
    let src = "\
import httpx

N = 2
M = 2


def unrelated(N):
    del N


def rebinds():
    global M
    del M


def fixed():
    for _ in range(N):
        httpx.Client()


def unfixed():
    for _ in range(M):
        httpx.Client()
";
    assert_eq!(
        reports("ir_local_del", src, "EFF102"),
        pairs(&[("unfixed", "EFF102")])
    );
}

// ============================================================================== robustness

#[test]
fn many_sequential_branches_do_not_exhaust_the_stack() {
    // The first read of `x` looks through one merge per `if` before it.
    let src = format!(
        "def f(x):\n{}    return x\n",
        "    if True:\n        pass\n".repeat(60_000)
    );
    let p = Project::new("ir_deep_merges");
    p.write("case.py", &src);
    let (code, _, stderr) = p.check(&[]);
    assert_eq!(code, 0, "{stderr}");
}

#[test]
fn a_length_cap_beyond_i64_is_no_cap() {
    let src = "def f(xs):\n    while len(xs) <= 9223372036854775807:\n        break\n";
    let ir = dump_ir("ir_huge_len_cap", src, "py315");
    assert!(function(&ir, "f")["loops"][0].get("len_caps").is_none());
}

// ============================================================================ loop sizes

#[test]
fn a_loop_is_fixed_only_when_the_code_fixes_its_iterable() {
    let src = "\
import httpx


def shadowed_range(range):
    for _ in range(1):
        httpx.Client()


def builtin_range():
    for _ in range(1):
        httpx.Client()


def grown_constructor(items):
    xs = list(\"a\")
    xs.extend(items)
    for x in xs:
        httpx.Client()


def returned_alias(items):
    xs = []
    other = min(xs, xs)
    other.extend(items)
    for x in xs:
        httpx.Client()


def drained_queue(inactive_users):
    while inactive_users:
        inactive_users.pop()
        httpx.Client()


class Worker:
    def serve(self):
        while self._running:
            httpx.Client()
";
    assert_eq!(
        reports("ir_loop_sizes", src, "EFF102"),
        pairs(&[
            ("shadowed_range", "EFF102"),
            ("grown_constructor", "EFF102"),
            ("returned_alias", "EFF102"),
            ("drained_queue", "EFF102"),
        ])
    );
}

#[test]
fn unpacking_reads_every_item_before_it_writes_a_target() {
    let src =
        "def f(xs):\n    xs[1], x = xs\n    head, *rest, last = xs\n    return x, rest, last\n";
    let ir = dump_ir("ir_unpacking", src, "py315");
    let f = function(&ir, "f");
    let kinds: Vec<String> = f["blocks"][0]["insts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| {
            i["kind"]
                .as_object()
                .unwrap()
                .keys()
                .next()
                .unwrap()
                .clone()
        })
        .collect();
    // `xs[1], x = xs`: both items are read, then `xs[1]` is stored, then `x` is bound.
    let first_store = kinds.iter().position(|k| k == "SubscriptStore").unwrap();
    let loads_before = kinds[..first_store]
        .iter()
        .filter(|k| *k == "SubscriptLoad")
        .count();
    assert_eq!(loads_before, 2, "{kinds:?}");
    // `*rest` is a new list; `last` is item -1.
    let insts = f["blocks"][0]["insts"].as_array().unwrap();
    let ints: Vec<i64> = insts
        .iter()
        .filter_map(|i| i["kind"]["Const"]["literal"]["int"].as_i64())
        .collect();
    // Items 0 and 1, the store's index 1, then items 0 and -1.
    assert_eq!(ints, [0, 1, 1, 0, -1]);
    assert!(
        insts
            .iter()
            .any(|i| i["kind"]["AllocBuiltin"]["kind"] == "list"),
        "{insts:#?}"
    );
}

#[test]
fn an_unpacking_comprehension_consumes_each_iterable() {
    let src = "\
import time


def rows():
    time.sleep(1)
    yield 1


async def flattened(items):
    return [*rows() for _ in items]


async def flattened_set(items):
    return {*rows() for _ in items}


async def yielded_from(items):
    return list(*rows() for _ in items)


async def appended(items):
    return [rows() for _ in items]


def merged(maps):
    return {**m for m in maps}
";
    // PEP 798: `[*it for ...]` extends the list with each `it`, `(*it for ...)` yields from it;
    // a plain element only holds the generator.
    assert_eq!(
        reports("ir_unpacking_comprehensions", src, "EFF101"),
        pairs(&[
            ("flattened", "EFF101"),
            ("flattened_set", "EFF101"),
            ("yielded_from", "EFF101"),
        ])
    );
    let ir = dump_ir("ir_unpacking_comprehensions", src, "py315");
    let methods: Vec<String> = function(&ir, "merged")["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|b| b["insts"].as_array().unwrap())
        .filter_map(|i| {
            i["kind"]["MethodCall"]["method"]
                .as_str()
                .map(str::to_owned)
        })
        .collect();
    assert_eq!(methods, ["update"]);
}

// ============================================================================ suspensions

#[test]
fn async_iteration_and_async_with_suspend_while_a_lock_is_held() {
    let src = "\
import asyncio
import threading


async def iterates(stream):
    with threading.Lock():
        async for item in stream:
            pass


async def comprehension(stream):
    with threading.Lock():
        return [x async for x in stream]


async def enters(session):
    with threading.Lock():
        async with session:
            pass


async def after_release(stream):
    with threading.Lock():
        pass
    async for item in stream:
        pass


async def async_lock(stream):
    async with asyncio.Lock():
        async for item in stream:
            pass
";
    assert_eq!(
        reports("ir_async_suspensions", src, "EFF106"),
        pairs(&[
            ("iterates", "EFF106"),
            ("comprehension", "EFF106"),
            ("enters", "EFF106"),
        ])
    );
}

#[test]
fn a_saved_coroutine_runs_where_it_is_awaited() {
    let src = "\
import asyncio
import time


async def child():
    time.sleep(1)


async def saved():
    pending = child()
    await pending


async def merged(flag):
    pending = child() if flag else asyncio.sleep(0)
    await pending


async def scheduled():
    task = asyncio.ensure_future(child())
    return task


async def created():
    return child()
";
    let p = Project::new("ir_saved_coroutine");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.effect-boundaries]]\nname = \"b\"\nscopes = [\"case.saved\", \"case.merged\", \"case.created\"]\nforbid = [\"sleep\"]\n",
    )
    .write("case.py", src);
    let got: Vec<String> = diagnostics(&p, &["--select", "EFF201"])
        .iter()
        .map(|d| d["qualname"].as_str().unwrap().to_owned())
        .collect();
    // A coroutine only created and returned runs in the caller.
    assert_eq!(got, ["saved", "merged"]);
}

// ============================================================================ exceptions

/// The instructions of `f` with the phi (if any) defining `var`'s incoming values.
fn phi_inputs(f: &Value, var: &Value) -> Option<Vec<Value>> {
    f["blocks"].as_array().unwrap().iter().find_map(|b| {
        b["phis"]
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["dst"] == *var)
            .map(|p| {
                p["incoming"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|i| i[1].clone())
                    .collect()
            })
    })
}

#[test]
fn a_handler_sees_the_bindings_of_every_point_that_may_raise() {
    let src = "\
import time


async def rebound_after(check):
    action = time.sleep
    try:
        1 // 0
        action = abs
    except ZeroDivisionError:
        action(1)


async def raises_after_rebinding(check):
    action = time.sleep
    try:
        1 // 0
        action = abs
        check()
    except ZeroDivisionError:
        action(1)
";
    // Only `1 // 0` raises in `rebound_after`: its handler sees `time.sleep`.
    let got = reports("ir_handler_bindings", src, "EFF101");
    assert!(
        got.contains(&("rebound_after".to_owned(), "EFF101".to_owned())),
        "{got:?}"
    );
    // `check()` raises after the rebinding too: the handler merges both bindings.
    let ir = dump_ir("ir_handler_bindings", src, "py315");
    let f = function(&ir, "raises_after_rebinding");
    let call = f["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|b| b["insts"].as_array().unwrap())
        .find(|i| i["kind"]["Call"]["callee"] == "action")
        .unwrap();
    let inputs = phi_inputs(f, &call["kind"]["Call"]["callee_var"]).expect("a phi");
    let names: Vec<&str> = inputs
        .iter()
        .map(|v| {
            f["var_names"][v.as_u64().unwrap() as usize]
                .as_str()
                .unwrap()
        })
        .collect();
    assert_eq!(names, ["action", "action"]);
    assert_ne!(inputs[0], inputs[1]);
}

#[test]
fn reading_a_lazy_import_at_module_level_may_raise() {
    let src = "lazy import optional_backend\n\ntry:\n    optional_backend\nexcept ImportError:\n    optional_backend = None\n";
    let ir = dump_ir("ir_lazy_first_use", src, "py315");
    let module = &ir["functions"][0];
    let load = module["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| {
            b["insts"].as_array().unwrap().iter().any(|i| {
                i["kind"]["LoadGlobal"]["name"] == "optional_backend"
                    && i["kind"]["LoadGlobal"]["is_lazy_import"] == true
            })
        })
        .expect("the read loads the lazy global");
    // PEP 810: the module is imported at the first use, which raises into the handler.
    assert!(load["exc_target"].is_u64(), "{load:#}");
}

#[test]
fn a_return_break_or_continue_runs_the_finally_body() {
    let src = format!(
        "{MODEL}\n\ndef returns(model: Model):\n    try:\n        model.value = 1\n        return\n    finally:\n        notify()\n\n\ndef breaks(model: Model, items):\n    for _ in items:\n        try:\n            model.value = 1\n            break\n        finally:\n            notify()\n\n\ndef continues(model: Model, items):\n    for _ in items:\n        try:\n            model.value = 1\n            continue\n        finally:\n            notify()\n\n\ndef forgets(model: Model):\n    try:\n        model.value = 1\n        return\n    finally:\n        pass\n"
    );
    let p = Project::new("ir_finally_paths");
    p.write("pyproject.toml", INVALIDATION)
        .write("case.py", &src);
    let got: Vec<String> = diagnostics(&p, &["--select", "MUT109"])
        .iter()
        .map(|d| d["qualname"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(got, ["forgets"]);
}

#[test]
fn a_jump_out_of_a_with_block_exits_it() {
    let src = "\
import asyncio
import threading


async def breaks(items):
    for _ in items:
        with threading.Lock():
            break
    else:
        return
    await asyncio.sleep(0)


async def returns(flag):
    with threading.Lock():
        if flag:
            return
    await asyncio.sleep(0)


async def holds(items):
    for _ in items:
        with threading.Lock():
            await asyncio.sleep(0)
";
    assert_eq!(
        reports("ir_with_exits", src, "EFF106"),
        pairs(&[("holds", "EFF106")])
    );
}

#[test]
fn every_except_star_clause_may_run() {
    let src = "\
def star(it):
    it = iter(it)
    try:
        raise ExceptionGroup(\"g\", [ValueError(), KeyError()])
    except* ValueError:
        list(it)
    except* KeyError:
        list(it)


def plain(it):
    it = iter(it)
    try:
        raise ValueError
    except ValueError:
        list(it)
    except KeyError:
        list(it)
";
    // An exception group can match both clauses; plain clauses are exclusive.
    assert_eq!(
        reports("ir_except_star", src, "MUT106"),
        pairs(&[("star", "MUT106")])
    );
}

#[test]
fn a_generator_expression_starts_its_first_iteration_where_it_is_created() {
    let src = "\
import sqlalchemy.orm


async def later_async_clause(query: sqlalchemy.orm.Query, stream):
    async for y in (y for row in query async for y in stream):
        pass


async def plain(query: sqlalchemy.orm.Query):
    return (row for row in query)
";
    // `iter(query)` runs when the generator expression is created, whatever its later
    // clauses are: the query executes on the event loop.
    assert_eq!(
        reports("ir_genexpr_first_iteration", src, "EFF101"),
        pairs(&[("later_async_clause", "EFF101"), ("plain", "EFF101")])
    );
}

#[test]
fn unreachable_code_gives_no_value_to_a_merge() {
    let src = "\
import httpx


class Pool:
    def open(self):
        return httpx.Client()


def first_open(pool: Pool, hosts):
    for _ in hosts:
        try:
            return pool.open()
        except OSError:
            continue


def after_continue(pool: Pool, hosts):
    for _ in hosts:
        pool.open()
        continue
        pool = None
";
    // No path reaches the end of the `try` or the code after `continue`: the loop header
    // merges nothing from there, so `pool` keeps its type and both calls are resolved.
    assert_eq!(
        reports("ir_dead_code_merges", src, "EFF102"),
        pairs(&[("first_open", "EFF102"), ("after_continue", "EFF102")])
    );
    let ir = dump_ir("ir_dead_code_merges", src, "py315");
    for name in ["first_open", "after_continue"] {
        let header = function(&ir, name)["loops"][0]["header"].as_u64().unwrap();
        let phis = &function(&ir, name)["blocks"][header as usize]["phis"];
        assert_eq!(phis.as_array().unwrap().len(), 0, "{name}: {phis}");
    }
}

#[test]
fn a_comprehension_walrus_binds_where_its_function_binds_the_name() {
    let src = "\
latest = None


def track(values):
    global latest
    return next((latest := v) for v in values)


def cell(values):
    found = None
    next((found := v) for v in values)
    return found
";
    let ir = dump_ir("ir_comprehension_walrus_global", src, "py315");
    let stores = |qualname: &str, kind: &str| -> Vec<String> {
        function(&ir, qualname)["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|b| b["insts"].as_array().unwrap())
            .filter_map(|i| i["kind"][kind]["name"].as_str().map(str::to_owned))
            .collect()
    };
    assert_eq!(
        stores("track.<locals>.<genexpr>", "StoreGlobal"),
        ["latest"]
    );
    assert_eq!(stores("cell.<locals>.<genexpr>", "StoreDeref"), ["found"]);
}
