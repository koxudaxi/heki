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
