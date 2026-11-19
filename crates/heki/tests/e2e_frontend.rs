//! End-to-end frontend tests through the real binary: `heki dump-semantic`, `heki dump-ir`,
//! `heki assumptions`, `heki rule` / `heki explain` and `heki check` over the syntax-tour corpus
//! in `tests/corpus/` (every statement and expression form `heki_semantic` scopes and `heki_ir`
//! lowers). The corpus itself is checked against CPython `symtable` by
//! `corpus_agrees_with_cpython_symtable`, so the scope facts asserted here are CPython's.

mod common;

use common::{Project, python, repo_root};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

const TOUR: &str = "tests/corpus/frontend_tour.py";
const TOUR_312: &str = "tests/corpus/frontend_tour_312_315.py";
const TOUR_314: &str = "tests/corpus/frontend_tour_314.py";
const TOUR_PEP810: &str = "tests/corpus/frontend_tour_pep810.py";
const TOUR_FUTURE: &str = "tests/corpus/frontend_tour_future.py";

// ================================================================================ harness

/// `heki <args>` in `dir`: `(exit code, stdout, stderr)`.
fn heki_in(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_heki"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

/// `heki <args>` from the repository root.
fn heki(args: &[&str]) -> (i32, String, String) {
    heki_in(&repo_root(), args)
}

/// `heki dump-semantic` / `heki dump-ir` of a file as JSON.
fn dump(cmd: &str, file: &str, version: &str) -> Value {
    let (code, stdout, stderr) = heki(&[cmd, file, "--target-version", version]);
    assert_eq!(code, 0, "{cmd} {file} {version}: {stderr}");
    serde_json::from_str(&stdout).unwrap()
}

fn semantic(file: &str, version: &str) -> Value {
    dump("dump-semantic", file, version)
}

fn ir(file: &str, version: &str) -> Value {
    dump("dump-ir", file, version)
}

/// `heki dump-*` of an inline source written into a throwaway project.
fn dump_source(cmd: &str, name: &str, source: &str, version: &str) -> Value {
    let p = Project::new(name);
    p.write("m.py", source);
    let (code, stdout, stderr) = p.heki(&[cmd, "m.py", "--target-version", version]);
    assert_eq!(code, 0, "{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

fn strs(v: &Value) -> Vec<&str> {
    v.as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect()
}

// ======================================================================= semantic helpers

/// The scope reached from the module by a path of `"<kind> <name>"` steps.
fn scope<'a>(model: &'a Value, path: &[&str]) -> &'a Value {
    let scopes = model["scopes"].as_array().unwrap();
    path.iter().fold(&scopes[0], |parent, step| {
        let (kind, name) = step.split_once(' ').unwrap();
        scopes
            .iter()
            .find(|s| s["parent"] == parent["id"] && s["kind"] == kind && s["name"] == name)
            .unwrap_or_else(|| panic!("no {kind} scope `{name}` under `{}`", parent["name"]))
    })
}

fn children<'a>(model: &'a Value, parent: &Value) -> Vec<&'a Value> {
    model["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["parent"] == parent["id"])
        .collect()
}

fn child_kinds(model: &Value, parent: &Value) -> Vec<(String, String)> {
    children(model, parent)
        .iter()
        .map(|s| {
            (
                s["kind"].as_str().unwrap().to_owned(),
                s["name"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn bindings(scope: &Value) -> BTreeSet<&str> {
    scope["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["name"].as_str().unwrap())
        .collect()
}

fn binding<'a>(scope: &'a Value, name: &str) -> &'a Value {
    scope["bindings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["name"] == name)
        .unwrap_or_else(|| panic!("no binding `{name}` in `{}`", scope["name"]))
}

fn reads(scope: &Value) -> BTreeMap<&str, &str> {
    scope["reads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r[0].as_str().unwrap(), r[1].as_str().unwrap()))
        .collect()
}

fn import<'a>(model: &'a Value, bound: &str) -> &'a Value {
    model["imports"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["bound_name"] == bound)
        .unwrap_or_else(|| panic!("no import binding `{bound}`"))
}

fn class<'a>(model: &'a Value, qualname: &str) -> &'a Value {
    model["classes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["qualname"] == qualname)
        .unwrap()
}

// ============================================================================= IR helpers

fn function<'a>(ir: &'a Value, qualname: &str) -> &'a Value {
    let found: Vec<&Value> = ir["functions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["qualname"] == qualname)
        .collect();
    assert_eq!(found.len(), 1, "functions named `{qualname}`");
    found[0]
}

fn qualnames(ir: &Value) -> Vec<&str> {
    ir["functions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["qualname"].as_str().unwrap())
        .collect()
}

/// `(block id, instruction kind, payload)` of every instruction, in block order.
fn insts(f: &Value) -> Vec<(u64, &str, &Value)> {
    let mut out = Vec::new();
    for bb in f["blocks"].as_array().unwrap() {
        for inst in bb["insts"].as_array().unwrap() {
            let (kind, payload) = inst["kind"].as_object().unwrap().iter().next().unwrap();
            out.push((bb["id"].as_u64().unwrap(), kind.as_str(), payload));
        }
    }
    out
}

/// Payloads of every `kind` instruction.
fn of_kind<'a>(f: &'a Value, kind: &str) -> Vec<&'a Value> {
    insts(f)
        .into_iter()
        .filter(|(_, k, _)| *k == kind)
        .map(|(_, _, p)| p)
        .collect()
}

fn kinds(f: &Value) -> BTreeSet<&str> {
    insts(f).into_iter().map(|(_, k, _)| k).collect()
}

fn var_name<'a>(f: &'a Value, var: &Value) -> Option<&'a str> {
    f["var_names"][var.as_u64().unwrap() as usize].as_str()
}

/// Predecessors of every block, including exception edges.
fn predecessors(f: &Value) -> Vec<BTreeSet<u64>> {
    let blocks = f["blocks"].as_array().unwrap();
    let mut preds = vec![BTreeSet::new(); blocks.len()];
    for bb in blocks {
        let id = bb["id"].as_u64().unwrap();
        let term = &bb["terminator"];
        let mut succs: Vec<u64> = Vec::new();
        if let Some(t) = term.get("Jump") {
            succs.push(t.as_u64().unwrap());
        }
        if let Some(b) = term.get("Branch") {
            succs.push(b["then_bb"].as_u64().unwrap());
            succs.push(b["else_bb"].as_u64().unwrap());
        }
        if let Some(e) = bb["exc_target"].as_u64() {
            succs.push(e);
        }
        for s in succs {
            preds[s as usize].insert(id);
        }
    }
    preds
}

/// The block holding the call of `callee`.
fn call_block(f: &Value, callee: &str) -> u64 {
    insts(f)
        .into_iter()
        .find(|(_, k, p)| *k == "Call" && p["callee"] == callee)
        .map(|(b, _, _)| b)
        .unwrap_or_else(|| panic!("no call of `{callee}`"))
}

fn bounds(f: &Value) -> Vec<Value> {
    f["loops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|lp| lp["bound"].clone())
        .collect()
}

fn loop_carried_names(f: &Value, header: &Value) -> BTreeSet<String> {
    f["loop_carried"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|lc| &lc["loop_header"] == header)
        .map(|lc| lc["var_name"].as_str().unwrap().to_owned())
        .collect()
}

// ================================================================== semantic: the 3.11 tour

#[test]
fn semantic_module_imports_and_classes() {
    let m = semantic(TOUR, "py312");
    assert_eq!(m["has_future_annotations"], false);
    let module = &m["scopes"][0];
    let module_bindings = bindings(module);
    for name in [
        "generator_stop",
        "os",
        "cabc",
        "mygc",
        "insp",
        "sibling",
        "renamed_thing",
        "Decimal",
        "nt",
        "posix",
        "fractions",
        "__not_mangled",
        "_",
        "ANNOTATED",
        "UNSET",
        "PARENTHESIZED",
        // PEP 709: module-level inlined comprehension targets are module symbols.
        "row",
        "v",
    ] {
        assert!(module_bindings.contains(name), "module binds `{name}`");
    }
    // PEP 572: a walrus in a module-level comprehension stores a global without binding it.
    assert!(!module_bindings.contains("last_row"));
    assert!(!module_bindings.contains("seen"));
    assert_eq!(reads(module)["row"], "global");
    assert!(strs(&module["inlined_comp_only_reads"]).contains(&"row"));

    // `from __future__ import <not annotations>`, relative and dotted imports.
    assert_eq!(import(&m, "generator_stop")["module"], "__future__");
    assert_eq!(import(&m, "sibling")["level"], 1);
    assert_eq!(import(&m, "renamed_thing")["level"], 2);
    assert_eq!(import(&m, "renamed_thing")["module"], "pkg.mod");
    assert_eq!(import(&m, "renamed_thing")["imported_name"], "thing");
    assert_eq!(import(&m, "os")["imported_name"], "os.path");
    assert_eq!(import(&m, "mygc")["imported_name"], "gc");
    // `if typing.TYPE_CHECKING:` / `if TYPE_CHECKING:` bodies, not their `elif` / `else`.
    assert_eq!(import(&m, "Decimal")["in_type_checking"], true);
    assert_eq!(import(&m, "fractions")["in_type_checking"], true);
    assert_eq!(import(&m, "nt")["in_type_checking"], false);
    assert_eq!(import(&m, "posix")["in_type_checking"], false);
    // `from os.path import *` binds nothing statically.
    assert!(
        m["imports"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["imported_name"] != "*")
    );

    // Lightweight CHA: dotted / subscripted bases are kept, call bases are not.
    let forms = class(&m, "ClassForms");
    assert_eq!(
        strs(&forms["bases"]),
        ["Base", "cabc.Sized", "typing.Generic"]
    );
    assert_eq!(strs(&forms["methods"]), ["method", "prop", "static"]);
    let nested = class(&m, "ClassForms.Nested");
    assert_eq!(nested["parent_class"], forms["id"]);
    assert_eq!(strs(&class(&m, "Meta")["bases"]), ["type"]);
}

#[test]
fn semantic_functions_closures_globals_and_mangling() {
    let m = semantic(TOUR, "py312");

    let params = scope(&m, &["function parameters"]);
    for name in ["a", "b", "c", "d", "args", "kwargs"] {
        assert_eq!(binding(params, name)["is_parameter"], true, "{name}");
    }
    let lambdas = children(&m, scope(&m, &["function expression_forms"]));
    let with_params = lambdas.iter().find(|s| !bindings(s).is_empty()).unwrap();
    assert_eq!(
        bindings(with_params),
        BTreeSet::from(["kw", "rest", "x", "y", "z"])
    );
    // The lambda defaults `y=a` / `z=b` are evaluated in the enclosing function.
    assert_eq!(reads(with_params).get("a"), None);

    // `nonlocal` (twice) and free reads, also through an intermediate function.
    let bump = scope(&m, &["function closures", "function bump"]);
    assert_eq!(binding(bump, "counter")["is_nonlocal_decl"], true);
    assert_eq!(reads(bump)["counter"], "enclosing");
    let later = scope(&m, &["function closures", "function reads_later"]);
    assert_eq!(reads(later)["defined_later"], "enclosing");
    let innermost = scope(
        &m,
        &["function closures", "function middle", "function innermost"],
    );
    assert_eq!(reads(innermost)["counter"], "enclosing");

    // `global` (twice): module-bound names read as globals, others as builtins, also from an
    // inner function of the declaring one.
    let globals = scope(&m, &["function global_forms"]);
    assert_eq!(binding(globals, "COUNTER")["is_global_decl"], true);
    assert_eq!(reads(globals)["COUNTER"], "global");
    assert_eq!(reads(globals)["FRESH_GLOBAL"], "builtin");
    let inner = scope(&m, &["function enclosing_global", "function inner"]);
    assert_eq!(reads(inner)["COUNTER"], "global");
    assert_eq!(reads(inner)["ANOTHER_FRESH"], "builtin");

    // Private name mangling: class bodies and their methods, nested classes, but not
    // all-underscore class names nor module-level names.
    let forms = scope(&m, &["class ClassForms"]);
    assert!(bindings(forms).contains("_ClassForms__private"));
    let method = scope(&m, &["class ClassForms", "function method"]);
    assert_eq!(reads(method)["_ClassForms__class_level_name"], "builtin");
    let nested = scope(&m, &["class ClassForms", "class Nested"]);
    assert!(bindings(nested).contains("_Nested__inner"));
    assert!(bindings(scope(&m, &["class _"])).contains("__kept"));

    // Every pattern kind binds its capture names.
    let matched = scope(&m, &["function match_forms"]);
    for name in [
        "flag",
        "first",
        "rest",
        "op",
        "args",
        "extra",
        "origin_like",
        "py",
        "px",
        "py2",
    ] {
        assert!(bindings(matched).contains(name), "match binds `{name}`");
    }
    // A class method sees the enclosing function's local class.
    let local = scope(
        &m,
        &[
            "function nested_class_factory",
            "class Local",
            "function method",
        ],
    );
    assert_eq!(reads(local)["Local"], "enclosing");
}

#[test]
fn semantic_comprehension_scopes_follow_pep709() {
    // Python 3.11: every comprehension is its own scope.
    let m311 = semantic(TOUR, "py311");
    let comps = scope(&m311, &["function comprehensions"]);
    let kinds: BTreeSet<(String, String)> = child_kinds(&m311, comps).into_iter().collect();
    for name in ["<listcomp>", "<setcomp>", "<dictcomp>", "<genexpr>"] {
        assert!(
            kinds.contains(&("comprehension".to_owned(), name.to_owned())),
            "{name}: {kinds:?}"
        );
    }
    // The walrus target of a comprehension binds in the function and is nonlocal inside.
    assert!(bindings(comps).contains("last"));
    assert!(!bindings(comps).contains("r"));
    let walrus_comp = children(&m311, comps)
        .into_iter()
        .find(|s| bindings(s).contains("last"))
        .unwrap();
    assert_eq!(binding(walrus_comp, "last")["is_nonlocal_decl"], true);

    // Python 3.12+: list / set / dict comprehensions are inlined, generator expressions are not.
    let m312 = semantic(TOUR, "py312");
    let comps = scope(&m312, &["function comprehensions"]);
    let kinds: Vec<(String, String)> = child_kinds(&m312, comps);
    assert!(
        kinds
            .iter()
            .all(|(k, n)| k == "comprehension" && n == "<genexpr>"),
        "{kinds:?}"
    );
    assert!(bindings(comps).is_superset(&BTreeSet::from(["r", "c", "last", "hit"])));
    let twice = children(&m312, comps)
        .into_iter()
        .find(|s| bindings(s).contains("hit"))
        .unwrap();
    assert_eq!(binding(twice, "hit")["is_nonlocal_decl"], true);
    let nested_gen = children(&m312, comps)
        .into_iter()
        .find(|s| reads(s).contains_key("cols"))
        .unwrap();
    assert_eq!(reads(nested_gen)["cols"], "enclosing");

    // An inlined comprehension target stays isolated when the function reads the same name
    // outside it, and materializes when the function binds it too.
    let shadows = scope(&m312, &["function comprehension_target_shadows_global"]);
    assert!(!bindings(shadows).contains("row"));
    assert_ne!(reads(shadows)["row"], "local");
    let also_local = scope(&m312, &["function comprehension_target_also_local"]);
    assert_eq!(reads(also_local)["row"], "local");

    // A module-level generator expression declares its walrus target global.
    let module_gen = children(&m312, &m312["scopes"][0])
        .into_iter()
        .find(|s| s["kind"] == "comprehension")
        .unwrap();
    assert_eq!(binding(module_gen, "seen")["is_global_decl"], true);

    // Class-body comprehensions cannot read class-level names (CPython 3.12 symtable).
    let forms = scope(&m312, &["class ClassForms"]);
    assert!(!reads(forms).contains_key("size"));
    assert_eq!(reads(forms)["n"], "local");
    assert!(bindings(forms).is_superset(&BTreeSet::from(["n", "_", "names", "squares"])));
}

#[test]
fn semantic_annotation_scopes_follow_pep649() {
    // 3.14+: every `def` gets a sibling `__annotate__` scope; module / class annotations share
    // one per body (the corpus keeps them last, where CPython opens it too).
    let m314 = semantic(TOUR, "py314");
    let module = &m314["scopes"][0];
    let top = child_kinds(&m314, module);
    assert_eq!(
        top.last().unwrap(),
        &("annotation".to_owned(), "__annotate__".to_owned())
    );
    let position = |name: &str| top.iter().position(|(k, n)| k == "function" && n == name);
    let params = position("parameters").unwrap();
    assert_eq!(
        top[params - 1],
        ("annotation".to_owned(), "__annotate__".to_owned())
    );
    let annotate = children(&m314, module).last().copied().unwrap();
    assert_eq!(reads(annotate)["int"], "builtin");
    let forms = scope(&m314, &["class ClassForms"]);
    assert_eq!(
        child_kinds(&m314, forms).last().unwrap(),
        &("annotation".to_owned(), "__annotate__".to_owned())
    );
    let class_annotate = children(&m314, forms).last().copied().unwrap();
    assert_eq!(reads(class_annotate)["int"], "builtin");

    // 3.12: annotations are evaluated in place (module and class bodies, not function bodies).
    let m312 = semantic(TOUR, "py312");
    assert!(
        child_kinds(&m312, &m312["scopes"][0])
            .iter()
            .all(|(k, _)| k != "annotation")
    );
    assert_eq!(reads(&m312["scopes"][0])["int"], "builtin");
    let params = scope(&m312, &["function parameters"]);
    assert!(!reads(params).contains_key("int"));

    // `from __future__ import annotations`: annotations are never evaluated.
    for version in ["py312", "py314"] {
        let future = semantic(TOUR_FUTURE, version);
        assert_eq!(future["has_future_annotations"], true);
        let all: Vec<&Value> = future["scopes"].as_array().unwrap().iter().collect();
        assert!(all.iter().all(|s| s["kind"] != "annotation"), "{version}");
        assert!(
            all.iter().all(|s| !reads(s).contains_key("Limit")),
            "{version}"
        );
    }
}

// ========================================================= semantic: 3.12+ / 3.14+ / 3.15 syntax

#[test]
fn semantic_newer_syntax() {
    // PEP 695 type aliases and generic functions / methods.
    let m = semantic(TOUR_312, "py312");
    assert!(bindings(&m["scopes"][0]).is_superset(&BTreeSet::from(["Alias", "Pair", "Variadic"])));
    let alias = scope(&m, &["function Alias"]);
    assert_eq!(reads(alias)["list"], "builtin");
    let pair = scope(&m, &["type_parameters Pair"]);
    assert_eq!(bindings(pair), BTreeSet::from(["K", "V"]));
    let pair_value = scope(&m, &["type_parameters Pair", "function Pair"]);
    assert_eq!(reads(pair_value)["K"], "enclosing");
    let variadic = scope(&m, &["type_parameters Variadic"]);
    assert_eq!(bindings(variadic), BTreeSet::from(["P", "Ts"]));
    let generic = scope(&m, &["type_parameters generic_function"]);
    assert_eq!(bindings(generic), BTreeSet::from(["P", "T", "Ts"]));
    let body = scope(
        &m,
        &[
            "type_parameters generic_function",
            "function generic_function",
        ],
    );
    assert_eq!(binding(body, "value")["is_parameter"], true);
    let method = scope(
        &m,
        &[
            "class Plain",
            "type_parameters generic_method",
            "function generic_method",
        ],
    );
    assert_eq!(reads(method)["value"], "local");
    // PEP 701 nested quotes and nested format specs.
    let fstrings = scope(&m, &["function pep701_fstrings"]);
    assert_eq!(
        reads(fstrings).keys().copied().collect::<Vec<_>>(),
        ["items", "name", "width"]
    );

    // PEP 750 template strings (with nested format specs) and PEP 758 handlers.
    let m = semantic(TOUR_314, "py314");
    let templates = scope(&m, &["function templates"]);
    assert_eq!(
        reads(templates).keys().copied().collect::<Vec<_>>(),
        [
            "concatenated",
            "literal_only",
            "name",
            "plain",
            "precision",
            "spec",
            "width"
        ]
    );
    let handler = scope(&m, &["function unparenthesized_except"]);
    assert_eq!(reads(handler)["TypeError"], "builtin");

    // PEP 810 lazy imports and PEP 798 unpacking comprehensions.
    let m = semantic(TOUR_PEP810, "py315");
    let module = &m["scopes"][0];
    for name in ["json", "ET", "OrderedDict", "dd"] {
        assert_eq!(binding(module, name)["is_lazy_import"], true, "{name}");
        assert_eq!(import(&m, name)["is_lazy"], true, "{name}");
    }
    assert_eq!(binding(module, "os")["is_lazy_import"], false);
    assert_eq!(import(&m, "ET")["imported_name"], "xml.etree.ElementTree");
    let use_lazy = scope(&m, &["function use_lazy"]);
    assert!(bindings(use_lazy).is_superset(&BTreeSet::from(["row", "group"])));
    assert_eq!(reads(use_lazy)["json"], "global");
}

/// Forms where heki deliberately differs from CPython `symtable`, so they are not in the
/// verified corpus: CPython evaluates type-parameter bounds, constraints and defaults in lazy
/// scopes of their own and gives generic classes synthetic `.type_params` / `.generic_base`
/// symbols; heki visits them in the type-parameter scope. Only the facts both agree on are
/// asserted.
#[test]
fn semantic_type_parameter_bounds_defaults_and_generic_classes() {
    let source = "\
type Constrained[T: (int, str), *Ts = *tuple[int, ...], **P = [int, str]] = dict[T, P]


def bounded[T: Base = Default](value: T) -> T:
    return value


class Generic[K: Hashable, V = object](Base, metaclass=Meta):
    def get(self, key: K) -> V:
        return key
";
    let m = dump_source("dump-semantic", "fe_type_params", source, "py314");
    let alias = scope(&m, &["type_parameters Constrained"]);
    assert_eq!(bindings(alias), BTreeSet::from(["P", "T", "Ts"]));
    let bounded = scope(&m, &["type_parameters bounded"]);
    assert_eq!(bindings(bounded), BTreeSet::from(["T"]));
    let generic = scope(&m, &["type_parameters Generic"]);
    assert_eq!(bindings(generic), BTreeSet::from(["K", "V"]));
    let class_body = scope(&m, &["type_parameters Generic", "class Generic"]);
    assert_eq!(bindings(class_body), BTreeSet::from(["get"]));
    let get = scope(
        &m,
        &["type_parameters Generic", "class Generic", "function get"],
    );
    assert_eq!(reads(get)["key"], "local");
    let info = class(&m, "Generic");
    assert_eq!(strs(&info["bases"]), ["Base"]);
    assert_eq!(strs(&info["methods"]), ["get"]);
    // The bound / default expressions are read somewhere in the model (heki: the
    // type-parameter scope; CPython: their own lazy scopes).
    let all_reads: BTreeSet<&str> = m["scopes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|s| reads(s).into_keys())
        .collect();
    for name in ["Base", "Default", "Hashable", "Meta", "object", "tuple"] {
        assert!(all_reads.contains(name), "{name}");
    }
}

// ============================================================================ IR: statements

#[test]
fn ir_control_flow_statements() {
    let ir = ir(TOUR, "py315");
    assert_eq!(ir["has_star_import"], true);
    assert_eq!(ir["has_future_annotations"], false);
    let module = &ir["functions"][ir["module_fn"].as_u64().unwrap() as usize];
    assert_eq!(module["is_module_body"], true);

    // `for` / `while` with `else`, `continue`, `break` and `elif` chains.
    let f = function(&ir, "control_flow");
    let loops = f["loops"].as_array().unwrap();
    assert_eq!(loops.len(), 3);
    // `continue` and the loop tail are both back edges of the `for` loop.
    assert_eq!(loops[0]["back_edges"].as_array().unwrap().len(), 2);
    // `while other: return total` has no back edge at all.
    assert!(loops[2]["back_edges"].as_array().unwrap().is_empty());
    let carried: BTreeSet<String> = f["loop_carried"]
        .as_array()
        .unwrap()
        .iter()
        .map(|lc| lc["var_name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(carried, BTreeSet::from(["flag".into(), "total".into()]));
    assert!(
        f["loop_carried"]
            .as_array()
            .unwrap()
            .iter()
            .all(|lc| lc["self_accumulating"] == true)
    );
    let ops: BTreeSet<&str> = of_kind(f, "Compare")
        .iter()
        .map(|c| c["op"].as_str().unwrap())
        .collect();
    assert_eq!(ops, BTreeSet::from(["eq", "gt", "is", "lt_e"]));
    let first = function(&ir, "first_or_none");
    assert!(
        first["loops"][0]["back_edges"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    let elif = function(&ir, "elif_without_else");
    assert_eq!(of_kind(elif, "Compare").len(), 2);

    // Code after `continue` / `break` / `return` is lowered into unreachable blocks.
    let dead = function(&ir, "dead_code");
    let preds = predecessors(dead);
    let print_blocks: Vec<u64> = insts(dead)
        .into_iter()
        .filter(|(_, k, p)| *k == "Call" && p["callee"] == "print")
        .map(|(b, _, _)| b)
        .collect();
    assert_eq!(print_blocks.len(), 2);
    for b in print_blocks {
        assert!(preds[b as usize].is_empty(), "block {b} is unreachable");
    }

    // `raise ... from`, bare `raise`, `assert` with and without a message.
    let raises = function(&ir, "raises");
    let terms: Vec<&Value> = raises["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|bb| bb["terminator"].get("Raise"))
        .collect();
    assert_eq!(terms.len(), 4);
    assert_eq!(terms.iter().filter(|t| t.is_null()).count(), 2);

    // `try` / `except` (tuple, named, bare) / `else` / `finally` / `except*`.
    let t = function(&ir, "try_forms");
    let caught: Vec<(Option<&str>, Vec<&str>)> = of_kind(t, "CatchException")
        .iter()
        .map(|c| {
            (
                c["dst"].as_u64().map(|_| var_name(t, &c["dst"]).unwrap()),
                strs(&c["exc_types"]),
            )
        })
        .collect();
    assert_eq!(
        caught,
        vec![
            (Some("exc"), vec!["(KeyError, IndexError)"]),
            (None, vec!["ValueError"]),
            (None, vec![]),
            (None, vec!["Exception"]),
            (Some("group"), vec!["OSError"]),
        ]
    );
    // Exceptions in the `try` body go to the handlers; in `else`, only to `finally`.
    let blocks = t["blocks"].as_array().unwrap();
    let body = call_block(t, "fn");
    let handler_test = blocks[body as usize]["exc_target"].as_u64().unwrap();
    assert!(
        insts(t)
            .iter()
            .any(|(b, k, p)| *b == handler_test && *k == "LoadGlobal" && p["name"] == "KeyError")
    );
    let else_block = insts(t)
        .into_iter()
        .find(|(_, k, p)| {
            *k == "Call" && p["callee"] == "log" && var_name(t, &p["args"][0]) == Some("value")
        })
        .map(|(b, _, _)| b)
        .unwrap();
    let finally = blocks[else_block as usize]["exc_target"].as_u64().unwrap();
    assert!(
        insts(t).iter().any(|(b, k, p)| *b == finally
            && *k == "Const"
            && p["literal"] == json!({"str": "done"}))
    );
    assert!(
        !blocks[finally as usize]["phis"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // `with` items (several, tuple and list targets) enter in order and exit in reverse.
    let w = function(&ir, "with_forms");
    assert_eq!(of_kind(w, "EnterWith").len(), 4);
    let exits = of_kind(w, "ExitWith");
    assert_eq!(exits.len(), 4);
    let first_with = of_kind(w, "EnterWith");
    assert_eq!(exits[0]["mgr"], first_with[1]["mgr"]);
    assert_eq!(exits[1]["mgr"], first_with[0]["mgr"]);
    let data = call_block(w, "open");
    assert!(w["blocks"][data as usize]["exc_target"].is_null());
    let read = insts(w)
        .into_iter()
        .find(|(_, k, p)| *k == "MethodCall" && p["method"] == "read")
        .map(|(b, _, _)| b)
        .unwrap();
    assert!(w["blocks"][read as usize]["exc_target"].is_u64());
    let names: BTreeSet<&str> = of_kind(w, "Copy")
        .iter()
        .filter_map(|c| var_name(w, &c["dst"]))
        .collect();
    assert!(names.is_superset(&BTreeSet::from(["fh", "first", "second", "head", "tail"])));
}

#[test]
fn ir_match_delete_and_assignment_statements() {
    let ir = ir(TOUR, "py315");

    // Every pattern kind writes its captures; guards branch to the next case.
    let m = function(&ir, "match_forms");
    let captured: BTreeSet<&str> = of_kind(m, "Copy")
        .iter()
        .filter_map(|c| var_name(m, &c["dst"]))
        .collect();
    for name in [
        "flag",
        "first",
        "rest",
        "op",
        "args",
        "extra",
        "origin_like",
        "py",
        "px",
        "py2",
    ] {
        assert!(captured.contains(name), "{name}");
    }
    let loaded: BTreeSet<&str> = of_kind(m, "LoadGlobal")
        .iter()
        .map(|l| l["name"].as_str().unwrap())
        .collect();
    assert!(loaded.contains("cabc"));
    assert!(
        of_kind(m, "AttrLoad")
            .iter()
            .any(|a| a["attr"] == "__name__")
    );

    // `del` of names (defined in the block or not), attributes, subscripts, slices, nested
    // tuples and lists.
    let d = function(&ir, "delete_forms");
    let deleted: Vec<(&str, bool)> = of_kind(d, "Delete")
        .iter()
        .map(|x| (x["name"].as_str().unwrap(), x["var"].is_u64()))
        .collect();
    assert_eq!(
        deleted,
        vec![("temp", true), ("other", true), ("flag", false)]
    );
    assert_eq!(of_kind(d, "AttrDelete")[0]["attr"], "attr");
    let subs: Vec<&Value> = of_kind(d, "SubscriptDelete");
    assert_eq!(subs.len(), 4);
    assert_eq!(subs[0]["key_const"], "k");

    // Unpacking targets, chained assignment and every augmented operator (in place).
    let a = function(&ir, "assignment_forms");
    let in_place: BTreeSet<&str> = of_kind(a, "BinOp")
        .iter()
        .filter(|b| b["in_place"] == true)
        .map(|b| b["op"].as_str().unwrap())
        .collect();
    assert_eq!(
        in_place,
        BTreeSet::from([
            "add",
            "sub",
            "mult",
            "div",
            "floor_div",
            "mod",
            "pow",
            "l_shift",
            "r_shift",
            "bit_and",
            "bit_or",
            "bit_xor",
            "mat_mult"
        ])
    );
    let stores: Vec<&Value> = of_kind(a, "SubscriptStore");
    assert!(stores.iter().any(|s| s["key_const"] == "key"));
    assert_eq!(of_kind(a, "AttrStore").len(), 2);
    let targets: BTreeSet<&str> = of_kind(a, "Copy")
        .iter()
        .filter_map(|c| var_name(a, &c["dst"]))
        .collect();
    assert!(targets.is_superset(&BTreeSet::from([
        "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "x"
    ])));
}

#[test]
fn ir_functions_classes_closures_and_globals() {
    let ir = ir(TOUR, "py315");

    let p = function(&ir, "parameters");
    let params: Vec<(&str, bool, bool, bool, Option<&str>)> = p["params"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| {
            (
                x["name"].as_str().unwrap(),
                x["has_default"].as_bool().unwrap(),
                x["is_vararg"].as_bool().unwrap(),
                x["is_kwarg"].as_bool().unwrap(),
                x["annotation"].as_str(),
            )
        })
        .collect();
    assert_eq!(
        params,
        vec![
            ("a", false, false, false, None),
            ("b", true, false, false, None),
            ("c", false, false, false, None),
            ("d", true, false, false, None),
            ("args", false, true, false, Some("int")),
            ("kwargs", false, false, true, Some("str")),
        ]
    );
    assert_eq!(p["return_annotation"], "None");
    // Defaults are evaluated by the enclosing (module) body.
    let module = &ir["functions"][0];
    assert!(of_kind(module, "Call").iter().any(|c| c["callee"] == "len"));

    assert_eq!(
        strs(&function(&ir, "decorated")["decorators"]),
        ["trace", "trace"]
    );
    let forms = function(&ir, "ClassForms");
    assert_eq!(forms["is_class_body"], true);
    assert_eq!(strs(&forms["decorators"]), ["decorate"]);
    assert_eq!(
        strs(&function(&ir, "ClassForms.prop")["decorators"]),
        ["property"]
    );
    assert_eq!(
        strs(&function(&ir, "ClassForms.static")["decorators"]),
        ["staticmethod"]
    );
    let method = function(&ir, "ClassForms.method");
    let nested = function(&ir, "ClassForms.Nested.method");
    assert_eq!(method["owner_class_qualname"], "ClassForms");
    assert_eq!(nested["owner_class_qualname"], "ClassForms.Nested");
    assert_ne!(method["owner_class"], nested["owner_class"]);
    // Class bases (including a call) are evaluated by the enclosing body.
    assert!(
        of_kind(module, "Call")
            .iter()
            .any(|c| c["callee"] == "make_base")
    );

    // Closures: captured locals (also one defined after the inner function), `nonlocal`
    // stores, and a grandchild reading through an intermediate function.
    let closures = function(&ir, "closures");
    assert_eq!(
        strs(&closures["captured_vars"]),
        ["counter", "defined_later"]
    );
    let bump = function(&ir, "closures.<locals>.bump");
    assert_eq!(of_kind(bump, "StoreDeref")[0]["name"], "counter");
    assert_eq!(of_kind(bump, "LoadDeref")[0]["name"], "counter");
    let innermost = function(&ir, "closures.<locals>.middle.<locals>.innermost");
    assert_eq!(of_kind(innermost, "LoadDeref")[0]["name"], "counter");
    let closure_loop = function(&ir, "closure_loop");
    assert_eq!(
        strs(&closure_loop["captured_vars"]),
        ["counter", "flag", "source"]
    );
    let inner = function(&ir, "closure_loop.<locals>.inner");
    assert_eq!(inner["kind"], "generator");
    let k = kinds(inner);
    assert!(k.is_superset(&BTreeSet::from([
        "StoreDeref",
        "YieldFrom",
        "Yield",
        "Delete"
    ])));
    // Both `del` statements sit in blocks that do not define the name themselves.
    let deleted: BTreeSet<(&str, bool)> = of_kind(inner, "Delete")
        .iter()
        .map(|x| (x["name"].as_str().unwrap(), x["var"].is_u64()))
        .collect();
    assert_eq!(
        deleted,
        BTreeSet::from([("item", false), ("received", false)])
    );

    // `global`: loads and stores go to the module namespace.
    let g = function(&ir, "global_forms");
    let stored: BTreeSet<&str> = of_kind(g, "StoreGlobal")
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert_eq!(stored, BTreeSet::from(["COUNTER", "FRESH_GLOBAL"]));
    assert!(
        of_kind(g, "LoadGlobal")
            .iter()
            .any(|l| l["name"] == "FRESH_GLOBAL")
    );
}

// =========================================================================== IR: expressions

#[test]
fn ir_expressions() {
    let ir = ir(TOUR, "py315");
    let e = function(&ir, "expression_forms");
    let consts: Vec<&Value> = of_kind(e, "Const").iter().map(|c| &c["literal"]).collect();
    // `3j` and integers beyond `i64` are opaque; `-1` folds into one constant, `-1.5` does not.
    assert_eq!(consts[0], "other");
    assert_eq!(consts[1], "ellipsis");
    assert!(consts.contains(&&json!({"int": -1})));
    assert!(consts.contains(&&json!({"float": "1.5"})));
    assert!(consts.contains(&&json!("bytes")));
    assert!(consts.iter().filter(|c| **c == "other").count() >= 3);
    let unary: BTreeSet<&str> = of_kind(e, "UnaryOp")
        .iter()
        .map(|u| u["op"].as_str().unwrap())
        .collect();
    assert_eq!(unary, BTreeSet::from(["invert", "not", "u_add", "u_sub"]));
    let compare: BTreeSet<&str> = of_kind(e, "Compare")
        .iter()
        .map(|c| c["op"].as_str().unwrap())
        .collect();
    assert_eq!(
        compare,
        BTreeSet::from([
            "gt", "gt_e", "in", "is_not", "lt", "lt_e", "not_eq", "not_in"
        ])
    );
    let binops: BTreeSet<&str> = of_kind(e, "BinOp")
        .iter()
        .map(|b| b["op"].as_str().unwrap())
        .collect();
    assert_eq!(
        binops,
        BTreeSet::from([
            "add",
            "bit_and",
            "bit_or",
            "bit_xor",
            "div",
            "floor_div",
            "l_shift",
            "mat_mult",
            "mod",
            "pow",
            "r_shift"
        ])
    );
    // Short-circuit `or` / `and` and conditional expressions merge through phis.
    assert!(
        e["blocks"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|bb| !bb["phis"].as_array().unwrap().is_empty())
            .count()
            >= 3
    );
    let slices: Vec<(Value, Value)> = of_kind(e, "SubscriptLoad")
        .iter()
        .filter(|s| s["is_slice"] == true)
        .map(|s| {
            (
                s["slice_lower_const"].clone(),
                s["slice_upper_const"].clone(),
            )
        })
        .collect();
    assert_eq!(
        slices,
        vec![
            (json!(1), json!(2)),
            (Value::Null, Value::Null),
            (Value::Null, Value::Null),
            (Value::Null, Value::Null),
            (json!(1), json!(-1)),
        ]
    );
    // f-strings (nested format specs, implicit concatenation) build a `str`.
    assert!(
        of_kind(e, "AllocBuiltin")
            .iter()
            .any(|a| a["kind"] == "str")
    );
    for name in ["width", "precision"] {
        assert!(e["var_names"].as_array().unwrap().iter().any(|v| v == name));
    }
    let len_call = of_kind(e, "Call")
        .into_iter()
        .find(|c| c["callee"] == "len")
        .unwrap();
    let kwarg_names: Vec<Option<&str>> = len_call["kwargs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|kw| kw[0].as_str())
        .collect();
    assert_eq!(kwarg_names, vec![Some("key"), None]);
    assert!(
        of_kind(e, "Call")
            .iter()
            .any(|c| c["callee"] == "lambda: a")
    );
    let method = of_kind(e, "MethodCall")
        .into_iter()
        .find(|m| m["method"] == "method")
        .unwrap();
    assert_eq!(method["args"].as_array().unwrap().len(), 2);
    // Lambdas are closures; `y=a` / `z=b` defaults are evaluated by the enclosing function.
    assert_eq!(of_kind(e, "MakeClosure").len(), 3);
    assert_eq!(strs(&e["captured_vars"]), ["a"]);
    let lambda = ir["functions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| {
            f["qualname"] == "expression_forms.<locals>.<lambda>"
                && f["params"].as_array().unwrap().len() == 5
        })
        .unwrap();
    let params: Vec<(&str, bool)> = lambda["params"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["name"].as_str().unwrap(),
                p["has_default"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        params,
        vec![
            ("x", false),
            ("y", true),
            ("z", true),
            ("rest", false),
            ("kw", false)
        ]
    );

    // `async def` with `async with`, `async for`, `await`, `yield` and async comprehensions.
    let a = function(&ir, "async_forms");
    assert_eq!(a["kind"], "async_generator");
    assert!(
        of_kind(a, "EnterWith")
            .iter()
            .all(|w| w["is_async"] == true)
    );
    assert!(of_kind(a, "MakeIter").iter().all(|i| i["is_async"] == true));
    assert!(
        of_kind(a, "MethodCall")
            .iter()
            .any(|m| m["method"] == "get" && m["is_awaited"] == true)
    );
    assert!(of_kind(a, "Await").len() >= 3);
    assert_eq!(
        function(&ir, "async_forms.<locals>.<genexpr>")["kind"],
        "async_generator"
    );

    // Generators: bare `yield`, `yield value`, `yield from`.
    let g = function(&ir, "generators");
    assert_eq!(g["kind"], "generator");
    let yields: Vec<bool> = of_kind(g, "Yield")
        .iter()
        .map(|y| y["value"].is_u64())
        .collect();
    assert_eq!(yields, vec![false, true]);
    assert_eq!(of_kind(g, "YieldFrom").len(), 1);
}

#[test]
fn ir_comprehensions_follow_pep709() {
    // 3.12+: inlined loops in the enclosing function (nested `for` clauses are nested loops).
    let ir315 = ir(TOUR, "py315");
    let c = function(&ir315, "comprehensions");
    let depths: Vec<u64> = c["loops"]
        .as_array()
        .unwrap()
        .iter()
        .map(|lp| lp["depth"].as_u64().unwrap())
        .collect();
    assert_eq!(depths.iter().filter(|d| **d == 2).count(), 2);
    let methods: BTreeSet<&str> = of_kind(c, "MethodCall")
        .iter()
        .map(|m| m["method"].as_str().unwrap())
        .collect();
    assert!(methods.is_superset(&BTreeSet::from(["append", "add"])));
    assert_eq!(of_kind(c, "SubscriptStore").len(), 2);
    assert!(
        of_kind(c, "Call")
            .iter()
            .all(|call| !call["callee"].as_str().unwrap().starts_with("__comp_"))
    );
    // Generator expressions are always child functions with their outer iterable evaluated
    // eagerly.
    let gens = of_kind(c, "MakeGenExpr");
    assert_eq!(gens.len(), 2);
    assert!(
        gens.iter()
            .all(|g| var_name(c, &g["outer_iter"]) == Some("rows"))
    );
    assert_eq!(strs(&c["captured_vars"]), ["cols"]);
    assert!(
        !qualnames(&ir315)
            .iter()
            .any(|q| q.ends_with("<listcomp>") || q.ends_with("<dictcomp>"))
    );

    // 3.11: every comprehension is a child function called with its outer iterable.
    let ir311 = ir(TOUR, "py311");
    let c = function(&ir311, "comprehensions");
    let comp_calls: Vec<&Value> = of_kind(c, "Call")
        .into_iter()
        .filter(|call| call["callee"].as_str().unwrap().starts_with("__comp_"))
        .collect();
    assert_eq!(comp_calls.len(), 6);
    for kind in ["<listcomp>", "<setcomp>", "<dictcomp>"] {
        let child = ir311["functions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["qualname"] == format!("comprehensions.<locals>.{kind}"))
            .unwrap();
        assert_eq!(child["var_names"][0], ".0");
        assert!(
            child["var_names"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == ".comp_acc")
        );
        assert!(!child["loops"].as_array().unwrap().is_empty());
    }
    assert!(qualnames(&ir311).contains(&"<module>.<locals>.<listcomp>"));
    // Class-body comprehensions are plain functions, not methods of the class.
    let class_comps: Vec<&Value> = ir311["functions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|f| f["qualname"] == "ClassForms.<locals>.<listcomp>")
        .collect();
    assert_eq!(class_comps.len(), 2);
    assert!(class_comps.iter().all(|f| f["owner_class"].is_null()));
}

#[test]
fn ir_loop_bounds_and_loop_carried_values() {
    let ir = ir(TOUR, "py315");
    let small = |n: u64| json!({ "const_small": n });
    assert_eq!(
        bounds(function(&ir, "loop_bounds")),
        vec![
            small(3),
            small(2),
            json!("unbounded"),
            small(2),
            json!("unbounded"),
            json!("unbounded"),
            small(4),
            json!("unbounded"),
            small(5),
            small(3),
            json!("unbounded"),
            json!("unbounded"),
            json!("unbounded"),
            json!("unbounded"),
            json!("retry_pattern"),
            json!("retry_pattern"),
            json!("unbounded"),
            json!("unbounded"),
            json!("unbounded"),
            json!("unbounded"),
            json!("unbounded"),
            json!("daemon_polling"),
            json!("daemon_polling"),
            json!("daemon_polling"),
            json!("unbounded"),
            json!("daemon_polling"),
            json!("retry_pattern"),
        ]
    );
    assert_eq!(
        bounds(function(&ir, "sleep_detection")),
        vec![
            json!("retry_pattern"),
            json!("retry_pattern"),
            json!("retry_pattern"),
            json!("unbounded"),
            json!("retry_pattern"),
            json!("unbounded"),
        ]
    );
    assert_eq!(
        bounds(function(&ir, "async_retry")),
        vec![json!("daemon_polling")]
    );

    // CPython's `base64.encode` shape: a nested `while` whose condition short-circuits. Only
    // the values the inner loop updates are carried by it; `write` (read after it) is not.
    let f = function(&ir, "nested_while_with_boolop");
    let inner = f["loops"]
        .as_array()
        .unwrap()
        .iter()
        .find(|lp| lp["depth"] == 2)
        .unwrap();
    assert_eq!(
        loop_carried_names(f, &inner["header"]),
        BTreeSet::from(["chunk".into(), "more".into()])
    );
    let chunk = f["loop_carried"]
        .as_array()
        .unwrap()
        .iter()
        .find(|lc| lc["var_name"] == "chunk")
        .unwrap();
    assert_eq!(chunk["self_accumulating"], true);
    // A reassigned value that is not built from itself is carried but not accumulating.
    let dead = function(&ir, "dead_code");
    assert!(
        dead["loop_carried"]
            .as_array()
            .unwrap()
            .iter()
            .all(|lc| lc["var_name"] == "xs" && lc["self_accumulating"] == false)
    );
}

#[test]
fn ir_newer_syntax() {
    // PEP 810: `lazy import` loads keep the flag through the module namespace.
    let ir = ir(TOUR_PEP810, "py315");
    let module = &ir["functions"][0];
    let imports: Vec<(&str, bool)> = of_kind(module, "Import")
        .iter()
        .map(|i| (i["name"].as_str().unwrap(), i["is_lazy"].as_bool().unwrap()))
        .collect();
    assert_eq!(
        imports,
        vec![
            ("json", true),
            ("xml.etree.ElementTree", true),
            ("OrderedDict", true),
            ("defaultdict", true),
            ("os", false),
        ]
    );
    let f = function(&ir, "use_lazy");
    let lazy: BTreeMap<&str, bool> = of_kind(f, "LoadGlobal")
        .iter()
        .map(|l| {
            (
                l["name"].as_str().unwrap(),
                l["is_lazy_import"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        lazy,
        BTreeMap::from([
            ("ET", true),
            ("OrderedDict", true),
            ("dd", true),
            ("json", true),
            ("list", false),
            ("os", false),
        ])
    );
    // PEP 798 `{**group for group in groups}`: a dict comprehension element without a key,
    // in place and as a 3.11-style comprehension function.
    let merged = of_kind(f, "SubscriptStore");
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0]["key_const"], Value::Null);
    let ir311 = self::ir(TOUR_PEP810, "py311");
    let dictcomp = function(&ir311, "use_lazy.<locals>.<dictcomp>");
    assert_eq!(of_kind(dictcomp, "SubscriptStore").len(), 1);

    // PEP 750 template strings and PEP 758 unparenthesized handlers.
    let ir = self::ir(TOUR_314, "py314");
    let t = function(&ir, "templates");
    let loaded: BTreeSet<&str> = insts(t)
        .into_iter()
        .filter(|(_, k, _)| *k == "Copy")
        .filter_map(|(_, _, p)| var_name(t, &p["dst"]))
        .collect();
    assert_eq!(
        loaded,
        BTreeSet::from(["plain", "spec", "literal_only", "concatenated"])
    );
    assert!(of_kind(t, "Const").len() >= 4);
    let handler = function(&ir, "unparenthesized_except");
    assert_eq!(
        strs(&of_kind(handler, "CatchException")[0]["exc_types"]),
        ["ValueError, TypeError"]
    );

    // PEP 695 `type` statements bind an opaque value.
    let ir = self::ir(TOUR_312, "py312");
    let module = &ir["functions"][0];
    let stored: Vec<&str> = of_kind(module, "StoreGlobal")
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    assert!(stored.starts_with(&["Alias", "Pair", "Variadic"]));
    let opaque = of_kind(module, "Const")
        .iter()
        .filter(|c| c["literal"] == "other")
        .count();
    assert_eq!(opaque, 3);

    // `from __future__ import annotations`: annotation text is still recorded.
    let ir = self::ir(TOUR_FUTURE, "py314");
    assert_eq!(ir["has_future_annotations"], true);
    let f = function(&ir, "annotated");
    assert_eq!(f["return_annotation"], "list[Limit]");
    assert_eq!(
        f["local_annotations"][0],
        json!({"target": "local", "annotation": "dict[str, Limit]", "span": f["local_annotations"][0]["span"]})
    );
    assert_eq!(
        function(&ir, "Annotated.method")["return_annotation"],
        "Annotated"
    );
}

/// Input the parser accepts although CPython rejects it at compile time: `break` / `continue`
/// outside a loop lower to nothing instead of a jump.
#[test]
fn ir_loop_jumps_outside_loops_are_ignored() {
    let ir = dump_source(
        "dump-ir",
        "fe_jumps",
        "def f(x):\n    continue\n    break\n    return x\n",
        "py315",
    );
    let f = function(&ir, "f");
    assert!(f["loops"].as_array().unwrap().is_empty());
    assert_eq!(f["blocks"].as_array().unwrap().len(), 1);
    assert_eq!(f["blocks"][0]["terminator"], json!({"Return": 0}));
}

// ============================================================= `heki assumptions` / A1 hazards

#[test]
fn ir_records_a1_hazards() {
    let ir = ir(TOUR, "py315");
    let f = function(&ir, "a1_hazards");
    let kinds: Vec<&str> = f["a1_hazards"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| h["kind"].as_str().unwrap())
        .collect();
    assert_eq!(
        kinds,
        vec![
            "eval",
            "exec",
            "locals",
            "vars()",
            "sys._getframe",
            "inspect.currentframe",
            "inspect.currentframe",
            "gc.get_objects",
            "gc.get_objects",
            "gc.get_referrers",
            "sys._getframe",
            ".f_globals",
            ".f_locals",
            ".f_locals",
            ".cr_frame",
            ".ag_frame",
            ".gi_frame",
        ]
    );
    // Only that function is affected.
    let others: usize = ir["functions"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|g| g["qualname"] != "a1_hazards")
        .map(|g| g["a1_hazards"].as_array().unwrap().len())
        .sum();
    assert_eq!(others, 0);
}

#[test]
fn assumptions_lists_a1_hazards_as_text_and_json() {
    let p = Project::new("fe_assumptions");
    p.write("pkg/tour.py", &common::repo_file(TOUR))
        .write("pkg/clean.py", "def f():\n    return 1\n")
        .write("pkg/notes.txt", "eval('x')\n")
        .write("pkg/broken.py", "def f(:\n")
        .write("pkg/ignored/skip.py", "eval('1')\n")
        .write(".hekiignore", "pkg/ignored/\n");
    std::fs::write(p.dir.join("pkg/latin1.py"), b"x = '\xff'\n").unwrap();

    let (code, stdout, stderr) = p.heki(&["assumptions", "pkg"]);
    assert_eq!(code, 0, "{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 17, "{stdout}");
    assert!(lines.iter().all(|l| l.starts_with("pkg/tour.py:")));
    assert!(
        lines[0].ends_with("[a1_hazards] A1 hazard: eval"),
        "{}",
        lines[0]
    );
    assert!(
        stderr.contains("Checked 4 file(s)")
            && stderr.contains("17 A1 hazard(s), 2 parse/read error(s)"),
        "{stderr}"
    );
    assert!(
        stderr.contains("warning: pkg/broken.py: 2 syntax error(s)"),
        "{stderr}"
    );
    assert!(stderr.contains("warning: pkg/latin1.py: "), "{stderr}");

    // JSON records, sorted by position; a single file, a duplicate path and a missing one.
    let (code, stdout, stderr) = p.heki(&[
        "assumptions",
        "pkg/tour.py",
        "pkg/tour.py",
        "pkg/notes.txt",
        "missing.py",
        "--json",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let records: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    assert_eq!(records.len(), 17);
    assert_eq!(records[0]["kind"], "eval");
    assert_eq!(records[0]["qualname"], "a1_hazards");
    assert_eq!(records[0]["path"], "pkg/tour.py");
    let positions: Vec<(u64, u64)> = records
        .iter()
        .map(|r| (r["line"].as_u64().unwrap(), r["column"].as_u64().unwrap()))
        .collect();
    let mut sorted = positions.clone();
    sorted.sort();
    assert_eq!(positions, sorted);
    assert_eq!(records[0]["column"], 5);
    assert!(stderr.is_empty(), "{stderr}");

    // `--target-version` changes the IR (3.11 comprehension functions), not the hazards; an
    // unknown tag falls back to the configured profile.
    for version in ["py311", "py39"] {
        let (code, stdout, _) = p.heki(&[
            "assumptions",
            "pkg/tour.py",
            "--json",
            "--target-version",
            version,
        ]);
        assert_eq!(code, 0);
        assert_eq!(
            serde_json::from_str::<Vec<Value>>(&stdout).unwrap().len(),
            17
        );
    }
    let (_, _, stderr311) = p.heki(&["assumptions", "pkg/tour.py", "--target-version", "py311"]);
    let (_, _, stderr312) = p.heki(&["assumptions", "pkg/tour.py"]);
    let fn_count = |s: &str| -> u64 {
        let start = s.find("file(s) (").unwrap() + "file(s) (".len();
        s[start..]
            .split_whitespace()
            .next()
            .unwrap()
            .parse()
            .unwrap()
    };
    assert!(
        fn_count(&stderr311) > fn_count(&stderr312),
        "{stderr311}{stderr312}"
    );

    // The configuration of the first path's directory is loaded; a broken one is an error.
    p.write("pkg/heki.toml", "target-version = [\n");
    let (code, _, stderr) = p.heki(&["assumptions", "pkg/tour.py"]);
    assert_eq!(code, 2);
    assert!(
        stderr.starts_with("heki error: failed to parse TOML"),
        "{stderr}"
    );
    p.write("pkg/heki.toml", "target-version = \"py311\"\n");
    let (_, _, configured) = p.heki(&["assumptions", "pkg/tour.py"]);
    assert_eq!(fn_count(&configured), fn_count(&stderr311));
}

// ================================================================ `heki rule` / `heki explain`

#[test]
fn rule_and_explain_commands() {
    let (code, all, _) = heki(&["rule", "--all"]);
    assert_eq!(code, 0);
    let lines: Vec<&str> = all.lines().collect();
    assert_eq!(lines.len(), 17, "{all}");
    assert!(lines[0].starts_with("MUT101   cached-property-stale-mutation "));
    assert!(
        all.lines()
            .any(|l| l.starts_with("OPT101") && l.contains("[default=true  fix=true ]")),
        "{all}"
    );
    assert!(
        all.lines()
            .any(|l| l.starts_with("EFF105") && l.contains("[default=false fix=false]")),
        "{all}"
    );
    // Without a rule, `rule` lists them all too.
    assert_eq!(heki(&["rule"]).1, all);

    let (code, by_code, _) = heki(&["rule", "EFF101"]);
    assert_eq!(code, 0);
    assert!(
        by_code.starts_with("EFF101 (async-blocking-call)\n"),
        "{by_code}"
    );
    assert_eq!(heki(&["explain", "async-blocking-call"]).1, by_code);
    assert_eq!(heki(&["rule", "eff101"]).1, by_code);

    let (code, stdout, stderr) = heki(&["explain", "NOPE999"]);
    assert_eq!(code, 2);
    assert!(stdout.is_empty());
    assert_eq!(stderr, "unknown rule: NOPE999\n");
}

// ======================================================================== dump error paths

#[test]
fn dump_commands_report_read_and_syntax_errors() {
    let p = Project::new("fe_dump_errors");
    p.write("broken.py", "def f(:\n    pass\n")
        .write("ok.py", "x = [i for i in range(3)]\n");
    std::fs::write(p.dir.join("latin1.py"), b"x = '\xff'\n").unwrap();
    for cmd in ["dump-semantic", "dump-ir"] {
        let (code, stdout, stderr) = p.heki(&[cmd, "missing.py"]);
        assert_eq!(code, 2);
        assert!(stdout.is_empty());
        assert!(stderr.starts_with("heki error: "), "{stderr}");
        let (code, _, stderr) = p.heki(&[cmd, "latin1.py"]);
        assert_eq!(code, 2);
        assert!(stderr.contains("valid UTF-8"), "{stderr}");
        let (code, _, stderr) = p.heki(&[cmd, "broken.py"]);
        assert_eq!(code, 2);
        assert!(stderr.starts_with("heki error: parse errors: "), "{stderr}");

        // The default and unknown target versions mean Python 3.15.
        let (_, default, _) = p.heki(&[cmd, "ok.py"]);
        assert_eq!(
            p.heki(&[cmd, "ok.py", "--target-version", "py99"]).1,
            default
        );
        assert_eq!(
            p.heki(&[cmd, "ok.py", "--target-version", "3.15"]).1,
            default
        );
        assert_ne!(
            p.heki(&[cmd, "ok.py", "--target-version", "py311"]).1,
            default
        );
    }
}

// ============================================================================== symtable

/// The corpus is a CPython-verified oracle: `heki dump-semantic` agrees with CPython
/// `symtable` on every scope, binding and read classification (also run manually with
/// CPython 3.11 / 3.12 / 3.14, where each file is checked when that version parses it).
#[test]
fn corpus_agrees_with_cpython_symtable() {
    let root = repo_root();
    let out = Command::new(python())
        .arg(root.join("tests/symtable_verify/verify_symtable.py"))
        .arg("--heki")
        .arg(env!("CARGO_BIN_EXE_heki"))
        .arg(root.join("tests/corpus"))
        .output()
        .unwrap_or_else(|e| panic!("cannot run `{}` (set HEKI_TEST_PYTHON): {e}", python()));
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stdout}{stderr}");
    assert!(stdout.contains("with 0 disagreements"), "{stdout}");
}

// =================================================================== `heki check` over the IR

/// The rules and the soundness lane walk every instruction kind the frontend emits.
#[test]
fn check_analyzes_every_instruction_kind() {
    let p = Project::new("fe_check");
    p.write("tour.py", &common::repo_file(TOUR))
        .write(
            "loops.py",
            "\
def keyed_pop(mapping, keys):
    for key in keys:
        mapping.pop(\"k\")
        mapping.pop(key)


def accumulate(items, source):
    counter = 0

    def run():
        nonlocal counter
        out = []
        for item in items:
            out = out + [item]
            counter = len(out)
            pending = (x for x in source)
            del pending
            yield from source
        return out

    return run
",
        )
        .write(
            "shared.py",
            "\
from functools import lru_cache


@lru_cache
def shared():
    return {\"items\": []}


def should_copy():
    return True


def flag_unknown():
    m = shared().model_copy(deep=should_copy())
    m[\"items\"].append(1)


def flag_true():
    m = shared().model_copy(deep=True)
    m[\"items\"].append(1)
",
        );
    for version in ["py311", "py315"] {
        let (code, stdout, stderr) =
            p.check(&["--output-format", "json", "--target-version", version]);
        assert_eq!(code, 1, "{stderr}");
        let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
        let got: Vec<(&str, u64, &str, &str)> = diags
            .iter()
            .filter(|d| d["path"] != "tour.py")
            .map(|d| {
                (
                    d["path"].as_str().unwrap(),
                    d["line"].as_u64().unwrap(),
                    d["rule_code"].as_str().unwrap(),
                    d["qualname"].as_str().unwrap(),
                )
            })
            .collect();
        // `out = out + [item]` is still provably a private list although the loop also stores
        // a `nonlocal`, builds a generator, deletes a name and delegates with `yield from`;
        // `pop("k")` is not a front pop; an unknown `deep=` flag is a shallow copy.
        assert_eq!(
            got,
            vec![
                ("loops.py", 14, "OPT101", "accumulate.<locals>.run"),
                ("shared.py", 15, "MUT202", "flag_unknown"),
            ],
            "{version}: {stdout}"
        );
    }
}
