//! The size of a function's IR, end to end through `heki dump-ir`: code no path reaches, and
//! `with` exits and `finally` bodies copied once per way out.

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
    // normal completion no path reaches is a third copy.
    let removals = f["blocks"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|b| b["insts"].as_array().unwrap())
        .filter(|i| i["kind"]["Call"]["callee"] == "remove_path")
        .count();
    assert_eq!(removals, 3);
    assert!(units(f) < 400, "{} units", units(f));
    let p = Project::new("ir_size_isolated_load_check");
    p.write("m.py", src);
    let (code, stdout, stderr) = p.check(&["--extend-select", "EFF105,EFF106,EFF107,MUT106"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
}
