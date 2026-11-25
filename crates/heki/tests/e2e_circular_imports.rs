//! `IMP102` end to end: a `from M import name` that raises `ImportError` when a module of its
//! import cycle is imported first is reported once, naming those modules, with the imports that
//! lead to it. Each premise broken alone silences it. `tests/import_verify/` checks the same
//! shapes against CPython.

mod common;

use common::Project;
use serde_json::Value;

/// The `IMP102` reports of `heki check --select IMP102`, as JSON.
fn diagnostics(p: &Project) -> Vec<Value> {
    let (code, out, err) = p.check(&["--select", "IMP102", "--output-format", "json"]);
    // 2: a file heki could not read.
    assert!((0..=2).contains(&code), "{code}: {out}\n{err}");
    let all: Vec<Value> = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    (all.into_iter())
        .filter(|d| d["rule_code"] == "IMP102")
        .collect()
}

/// Each report as `path:line [target] entries`, the entries being the backticked names of the
/// message's "importing ... before" clause.
fn reports(p: &Project) -> Vec<String> {
    (diagnostics(p).iter())
        .map(|d| {
            let message = d["message"].as_str().unwrap();
            let entries = message.split(": importing ").nth(1).unwrap();
            let entries = entries.split(" while no module").next().unwrap();
            let entries: Vec<&str> = entries.split('`').skip(1).step_by(2).collect();
            format!(
                "{}:{} [{}] {}",
                d["path"].as_str().unwrap(),
                d["line"],
                d["target"].as_str().unwrap(),
                entries.join(" ")
            )
        })
        .collect()
}

/// The message of the one report and its related locations as `path:line message`.
fn the_report(p: &Project) -> (String, Vec<String>) {
    let found = diagnostics(p);
    assert_eq!(found.len(), 1, "{found:#?}");
    let related = (found[0]["related"].as_array().unwrap().iter())
        .map(|r| {
            format!(
                "{}:{} {}",
                r["path"].as_str().unwrap(),
                r["line"],
                r["message"].as_str().unwrap()
            )
        })
        .collect();
    (found[0]["message"].as_str().unwrap().to_owned(), related)
}

fn project(name: &str, files: &[(&str, &str)]) -> Project {
    let p = Project::new(name);
    for (path, text) in files {
        p.write(path, text);
    }
    p
}

const H7_A: &str = "from pkg.b import helper_b


def helper_a() -> int:
    return helper_b() + 1
";
const H7_B: &str = "from pkg.a import helper_a


def helper_b() -> int:
    return 1


def uses_a() -> int:
    return helper_a()
";
const H7_C: &str = "def helper_c() -> int:
    from pkg.d import helper_d

    return helper_d()
";
const H7_D: &str = "from pkg.c import helper_c


def helper_d() -> int:
    return 1


def uses_c() -> int:
    return helper_c()
";

fn h7() -> Project {
    project(
        "circular_imports_h7",
        &[
            ("pkg/__init__.py", ""),
            ("pkg/a.py", H7_A),
            ("pkg/b.py", H7_B),
            ("pkg/c.py", H7_C),
            ("pkg/d.py", H7_D),
        ],
    )
}

/// The H7 request: importing `pkg.a` first fails in `pkg/b.py`, importing `pkg.b` first fails
/// in `pkg/a.py`. `pkg.c` imports `pkg.d` only inside a function: no cycle loads.
#[test]
fn each_module_of_a_cycle_imported_first_fails_at_the_other() {
    let p = h7();
    assert_eq!(
        reports(&p),
        [
            "pkg/a.py:1 [pkg.b.helper_b] pkg.b",
            "pkg/b.py:1 [pkg.a.helper_a] pkg.a"
        ]
    );
    let found = diagnostics(&p);
    assert_eq!(found[1]["severity"], "error");
    assert_eq!(found[1]["certainty"], "heuristic");
    assert_eq!(
        found[1]["message"],
        "`<module>` imports `helper_a` from `pkg.a` while `pkg.a` is still loading, at its import \
         of `pkg.b`, and has not bound it: importing `pkg.a` while no module of the import \
         cycle has loaded raises `ImportError` here"
    );
    assert_eq!(
        found[1]["related"][0]["message"],
        "`pkg.a` imports `pkg.b` while it loads"
    );
    assert_eq!(found[1]["related"][0]["path"], "pkg/a.py");
}

/// `--select ALL` and a prefix never run the opt-in rule.
#[test]
fn only_its_code_selects_the_rule() {
    let p = h7();
    for select in ["ALL", "IMP"] {
        let (_, out, err) = p.heki(&["check", "--select", select, "--output-format", "json"]);
        let all: Vec<Value> = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {err}"));
        assert!(all.iter().all(|d| d["rule_code"] != "IMP102"), "{all:#?}");
    }
}

/// A ring of three: importing `ring.a` first reaches `ring.c` while `ring.a` is paused, with the
/// two imports of the chain related; importing `ring.b` or `ring.c` first loads `ring.a` whole
/// before `ring.c` asks it.
#[test]
fn only_the_entries_whose_chain_reaches_the_import_paused_fail() {
    let p = project(
        "circular_imports_ring",
        &[
            ("ring/__init__.py", ""),
            (
                "ring/a.py",
                "import ring.b\n\n\ndef helper_a():\n    return 1\n",
            ),
            ("ring/b.py", "import ring.c\n"),
            ("ring/c.py", "from ring.a import helper_a\n"),
        ],
    );
    let (message, related) = the_report(&p);
    assert!(message.contains("importing `ring.a` while"), "{message}");
    assert_eq!(
        related,
        [
            "ring/a.py:1 `ring.a` imports `ring.b` while it loads",
            "ring/b.py:1 `ring.b` imports `ring.c` while it loads"
        ]
    );
}

/// A package importing a module that imports a name the package binds after: importing the
/// package or any of its modules loads the package first, so each of them fails there.
#[test]
fn an_entry_through_its_parent_package_fails_where_the_package_does() {
    let p = project(
        "circular_imports_parent",
        &[
            ("pkg/__init__.py", "from pkg.a import VALUE\n"),
            ("pkg/a.py", "import pkg.b\n\nVALUE = 1\n"),
            ("pkg/b.py", "from pkg import VALUE\n"),
        ],
    );
    assert_eq!(reports(&p), ["pkg/b.py:1 [pkg.VALUE] pkg pkg.a pkg.b"]);
    let (message, related) = the_report(&p);
    assert!(
        message.contains("importing `pkg`, `pkg.a`, or `pkg.b` while no module"),
        "{message}"
    );
    assert_eq!(
        related,
        [
            "pkg/__init__.py:1 `pkg` imports `pkg.a` while it loads",
            "pkg/a.py:1 `pkg.a` imports `pkg.b` while it loads"
        ]
    );
}

/// `from . import b` of a submodule takes it from `sys.modules` while it loads: no failure.
#[test]
fn a_from_import_of_a_submodule_does_not_fail() {
    let p = project(
        "circular_imports_submodules",
        &[
            ("pkg/__init__.py", ""),
            ("pkg/a.py", "from . import b\n"),
            ("pkg/b.py", "from . import a\n"),
        ],
    );
    assert_eq!(reports(&p), Vec::<String>::new());
}

/// A package outside the cycle whose `__init__` imports a module of it: importing the package,
/// or any of its modules, loads the cycle in the order the package does, so all of them fail
/// at the same import.
#[test]
fn a_package_loading_the_cycle_is_imported_first_too() {
    let p = project(
        "circular_imports_outer_package",
        &[
            ("pkg/__init__.py", "from pkg.core import Core\n"),
            (
                "pkg/core.py",
                "from pkg.util import helper\n\n\nclass Core:\n    pass\n",
            ),
            (
                "pkg/util.py",
                "from pkg.core import Core\n\n\ndef helper():\n    return 1\n",
            ),
        ],
    );
    assert_eq!(
        reports(&p),
        ["pkg/util.py:1 [pkg.core.Core] pkg pkg.core pkg.util"]
    );
    // The package may load the cycle on a branch only: it may have loaded before without it.
    p.write(
        "pkg/__init__.py",
        "import os\n\nif os.environ.get(\"X\"):\n    from pkg.core import Core\n",
    );
    assert_eq!(reports(&p), Vec::<String>::new());
}

/// `import M` takes the module while it loads; only `from M import name` asks it for a name.
#[test]
fn an_import_of_the_module_does_not_fail() {
    let p = project(
        "circular_imports_import_form",
        &[
            ("pkg/__init__.py", ""),
            (
                "pkg/a.py",
                "import pkg.b\n\n\ndef helper_a():\n    return 1\n",
            ),
            (
                "pkg/b.py",
                "import pkg.a\n\n\ndef uses_a():\n    return pkg.a.helper_a()\n",
            ),
        ],
    );
    assert_eq!(reports(&p), Vec::<String>::new());
    p.write("pkg/b.py", "from pkg.a import helper_a\n");
    assert_eq!(reports(&p), ["pkg/b.py:1 [pkg.a.helper_a] pkg.a"]);
}

/// The shape of H7 with each premise broken once, by one file: the report goes, and CPython
/// imports each module cleanly (`tests/import_verify/fixtures/circular`).
#[test]
fn each_premise_broken_alone_leaves_no_report() {
    const A: &str = "from pkg.b import helper_b\n\n\ndef helper_a():\n    return 1\n";
    const B: &str = "def helper_b():\n    return 2\n\n\nfrom pkg.a import helper_a\n";
    let base = || {
        project(
            "circular_imports_premises",
            &[("pkg/__init__.py", ""), ("pkg/a.py", A), ("pkg/b.py", B)],
        )
    };
    // Importing `pkg.a` first fails in `pkg/b.py`; `pkg.b` binds `helper_b` before it imports.
    assert_eq!(reports(&base()), ["pkg/b.py:5 [pkg.a.helper_a] pkg.a"]);
    let broken: &[(&str, &str, &str)] = &[
        (
            "a handler takes the ImportError",
            "pkg/b.py",
            "def helper_b():\n    return 2\n\n\ntry:\n    from pkg.a import helper_a\nexcept ImportError:\n    helper_a = None\n",
        ),
        (
            "`except Exception` takes it",
            "pkg/b.py",
            "def helper_b():\n    return 2\n\n\ntry:\n    from pkg.a import helper_a\nexcept Exception:\n    helper_a = None\n",
        ),
        (
            "a `with` block may suppress it",
            "pkg/b.py",
            "import contextlib\n\n\ndef helper_b():\n    return 2\n\n\nwith contextlib.suppress(ImportError):\n    from pkg.a import helper_a\n",
        ),
        (
            "the module binds the name before the import it is paused at",
            "pkg/a.py",
            "def helper_a():\n    return 1\n\n\nfrom pkg.b import helper_b\n",
        ),
        (
            "the module binds the name on a branch before it",
            "pkg/a.py",
            "import os\n\nif os.environ.get(\"X\"):\n    helper_a = None\nfrom pkg.b import helper_b\n",
        ),
        (
            "a star import before it may bind the name",
            "pkg/a.py",
            "from pkg.common import *\nfrom pkg.b import helper_b\n",
        ),
        (
            "a module `__getattr__` before it supplies the name",
            "pkg/a.py",
            "def __getattr__(name):\n    return len\n\n\nfrom pkg.b import helper_b\n\n\ndef helper_a():\n    return 1\n",
        ),
        (
            "a function binds the name under `global`",
            "pkg/a.py",
            "from pkg.b import helper_b\n\n\ndef setup():\n    global helper_a\n    helper_a = len\n",
        ),
        (
            "the module may bind any name through `globals()`",
            "pkg/a.py",
            "from pkg.b import helper_b\n\nglobals()[\"helper_a\"] = len\n",
        ),
        (
            "another module binds the name in the module",
            "pkg/b.py",
            "def helper_b():\n    return 2\n\n\nimport pkg.a as _a\n\n_a.helper_a = len\nfrom pkg.a import helper_a\n",
        ),
        (
            "the import that pauses the module runs on one branch",
            "pkg/a.py",
            "import os\n\nif os.environ.get(\"X\"):\n    from pkg.b import helper_b\n\n\ndef helper_a():\n    return 1\n",
        ),
        (
            "the name is a submodule",
            "pkg/b.py",
            "def helper_b():\n    return 2\n\n\nfrom pkg.a import helper_a\n",
        ),
    ];
    for (why, path, text) in broken {
        let p = base();
        p.write("pkg/common.py", "def helper_a():\n    return 1\n");
        if *why == "the name is a submodule" {
            p.write("pkg/a/__init__.py", A);
            p.write("pkg/a/helper_a.py", "");
            std::fs::remove_file(p.dir.join("pkg/a.py")).unwrap();
        }
        p.write(path, text);
        assert_eq!(reports(&p), Vec::<String>::new(), "{why}");
    }
}

/// `except` clauses that pass an `ImportError` on, `ModuleNotFoundError` (a subclass) among
/// them, leave the report.
#[test]
fn a_handler_of_other_exceptions_leaves_the_report() {
    let p = project(
        "circular_imports_other_handlers",
        &[
            ("pkg/__init__.py", ""),
            (
                "pkg/a.py",
                "from pkg.b import helper_b\n\n\ndef helper_a():\n    return 1\n",
            ),
            (
                "pkg/b.py",
                "try:\n    from pkg.a import helper_a\nexcept (KeyError, ModuleNotFoundError):\n    helper_a = None\n",
            ),
        ],
    );
    assert_eq!(reports(&p), ["pkg/b.py:2 [pkg.a.helper_a] pkg.a"]);
}

/// The name a partly loaded module lacks raises a plain `ImportError`: only a handler taking
/// any import error, and whose body does not raise again, takes it. A handler of a missing
/// module, one that raises again, and a `finally` body leave the report; a handler of the
/// `try` around a nested one takes what the nested one passes on.
#[test]
fn only_a_handler_of_any_import_error_takes_the_failure() {
    const A: &str = "from pkg.b import helper_b\n\n\ndef helper_a():\n    return 1\n";
    let wrapped = |handler: &str| {
        format!(
            "def helper_b():\n    return 2\n\n\ntry:\n    from pkg.a import helper_a\n{handler}"
        )
    };
    let nested = |inner: &str, outer: &str| {
        format!(
            "def helper_b():\n    return 2\n\n\ntry:\n    try:\n        from pkg.a import helper_a\n{inner}{outer}"
        )
    };
    let reported = ["pkg/b.py:6 [pkg.a.helper_a] pkg.a"];
    let nested_reported = ["pkg/b.py:7 [pkg.a.helper_a] pkg.a"];
    let cases: &[(&str, String, &[&str])] = &[
        (
            "a handler of a missing module",
            wrapped("except ModuleNotFoundError:\n    helper_a = None\n"),
            &reported,
        ),
        (
            "a tuple of a missing module and another class",
            wrapped("except (ModuleNotFoundError, OSError):\n    helper_a = None\n"),
            &reported,
        ),
        (
            "an import error handler",
            wrapped("except ImportError:\n    helper_a = None\n"),
            &[],
        ),
        (
            "a tuple holding the import error",
            wrapped("except (OSError, ImportError):\n    helper_a = None\n"),
            &[],
        ),
        (
            "a handler that raises it again",
            wrapped("except ImportError:\n    raise\n"),
            &reported,
        ),
        (
            "a handler that raises another error",
            wrapped("except ImportError as e:\n    raise ImportError(\"no helper\") from e\n"),
            &reported,
        ),
        (
            "a handler that raises again on one branch only",
            wrapped(
                "except ImportError:\n    if helper_b():\n        raise\n    helper_a = None\n",
            ),
            &[],
        ),
        (
            "a `finally` body",
            wrapped("finally:\n    pass\n"),
            &reported,
        ),
        (
            "a clause that certainly matches hides the later ones",
            wrapped("except ImportError:\n    raise\nexcept Exception:\n    helper_a = None\n"),
            &reported,
        ),
        (
            "the outer handler takes what the nested one passes on",
            nested(
                "    except ModuleNotFoundError:\n        helper_a = None\n",
                "except ImportError:\n    helper_a = None\n",
            ),
            &[],
        ),
        (
            "the outer handler takes what the nested one raises again",
            nested(
                "    except ImportError:\n        raise\n",
                "except Exception:\n    helper_a = None\n",
            ),
            &[],
        ),
        (
            "an outer handler of a missing module",
            nested(
                "    except OSError:\n        helper_a = None\n",
                "except ModuleNotFoundError:\n    helper_a = None\n",
            ),
            &nested_reported,
        ),
    ];
    for (why, b, expected) in cases {
        let p = project(
            "circular_imports_handled_failures",
            &[("pkg/__init__.py", ""), ("pkg/a.py", A), ("pkg/b.py", b)],
        );
        assert_eq!(reports(&p), *expected, "{why}");
    }
}

/// A module of a project package that the project lacks raises `ModuleNotFoundError`, and a
/// relative import climbing past its top-level package a plain `ImportError`: a handler taking
/// what the import raises lets the module body go on to the import that fails, and without one
/// the module stops there.
#[test]
fn a_missing_module_a_handler_takes_lets_the_run_go_on() {
    const A: &str = "from pkg.b import helper_b\n\n\ndef helper_a():\n    return 1\n";
    let b = |import: &str, handler: &str| {
        format!(
            "def helper_b():\n    return 2\n\n\ntry:\n    {import}\n{handler}from pkg.a import helper_a\n"
        )
    };
    let reported = &["pkg/b.py:9 [pkg.a.helper_a] pkg.a"][..];
    let missing = "from pkg._speedups import fast";
    let past_the_top = "from ... import fast";
    for (import, handler, expected) in [
        (
            missing,
            "except ModuleNotFoundError:\n    fast = None\n",
            reported,
        ),
        (missing, "finally:\n    pass\n", &[][..]),
        (
            past_the_top,
            "except ImportError:\n    fast = None\n",
            reported,
        ),
        (
            past_the_top,
            "except ModuleNotFoundError:\n    fast = None\n",
            &[][..],
        ),
    ] {
        let p = project(
            "circular_imports_missing_handled",
            &[
                ("pkg/__init__.py", ""),
                ("pkg/a.py", A),
                ("pkg/b.py", &b(import, handler)),
            ],
        );
        assert_eq!(reports(&p), expected, "{import} {handler}");
    }
}

/// A function that loads a module of the cycle, called before a module of the cycle imports
/// the cycle: importing `pkg.e` first loads `pkg.x` out of the order the imports show, which
/// works, so only `pkg.m` imported first fails. Without the function's import the call loads
/// nothing, and `pkg.e` imported first fails too.
#[test]
fn code_that_may_load_the_cycle_out_of_order_stops_a_run() {
    let p = project(
        "circular_imports_late",
        &[
            ("pkg/__init__.py", ""),
            ("pkg/loader.py", "def load():\n    import pkg.x\n"),
            (
                "pkg/e.py",
                "from pkg.loader import load\n\nload()\nimport pkg.m\n",
            ),
            (
                "pkg/m.py",
                "import pkg.x\n\n\ndef name():\n    return 1\n\n\nimport pkg.e\n",
            ),
            ("pkg/x.py", "from pkg.m import name\n"),
        ],
    );
    assert_eq!(reports(&p), ["pkg/x.py:1 [pkg.m.name] pkg.m"]);
    p.write("pkg/loader.py", "def load():\n    pass\n");
    assert_eq!(reports(&p), ["pkg/x.py:1 [pkg.m.name] pkg.e pkg.m"]);
}

/// `from pkg import b`, where the package binds `b` after importing its submodule `pkg.b`
/// (`from pkg.b import b`): while the package is still loading the import takes the submodule,
/// and does not fail.
#[test]
fn a_name_the_package_binds_from_its_submodule_does_not_fail() {
    let p = project(
        "circular_imports_submodule_attribute",
        &[
            (
                "pkg/__init__.py",
                "from pkg.a import value\nfrom pkg.b import b\n",
            ),
            ("pkg/a.py", "from pkg import b\n\nvalue = 1\n"),
            ("pkg/b.py", "def b():\n    return 1\n"),
        ],
    );
    assert_eq!(reports(&p), Vec::<String>::new());
    let (code, out, err) = p.heki(&[
        "inspect",
        "--imports",
        "pkg.a",
        "--output-format",
        "json",
        ".",
    ]);
    assert_eq!(code, 0, "{err}");
    let report: Value = serde_json::from_str(&out).unwrap();
    let site = &report["modules"][0]["imports"][0];
    assert_eq!(site["target"], "pkg");
    assert_eq!(site["may_load"], serde_json::json!(["pkg.b"]));
}

/// The H7 shape where `pkg.a` runs `prefix` before its import of `pkg.b`.
fn h7_with_prefix(name: &str, prefix: &str) -> Project {
    project(
        name,
        &[
            ("pkg/__init__.py", ""),
            (
                "pkg/a.py",
                &format!(
                    "{prefix}\nfrom pkg.b import helper_b\n\n\ndef helper_a():\n    return 1\n"
                ),
            ),
            (
                "pkg/b.py",
                "def helper_b():\n    return 2\n\n\nfrom pkg.a import helper_a\n",
            ),
            ("pkg/other.py", ""),
            ("pkg/plugins/__init__.py", ""),
            ("pkg/plugins/x.py", "import pkg.b\n"),
        ],
    )
}

/// What may load a module of the cycle before the import that pauses `pkg.a` stops the run of
/// `pkg.a` imported first: an import heki cannot name (also in a module outside the cycle that
/// `pkg.a` loads), one of several or under a package, a module it could not read, and an import
/// that raises whenever it runs. Others do not.
#[test]
fn imports_the_run_cannot_place_stop_it() {
    let stopping = [
        "import importlib\nimport os\nimportlib.import_module(os.environ[\"M\"])",
        "import importlib\nimport os\nimportlib.import_module(\"pkg.b\" if os.environ.get(\"X\") else \"json\")",
        "import importlib\nimport os\nimportlib.import_module(f\"pkg.plugins.{os.environ['P']}\")",
        "import pkg.broken",
        "from pkg.missing import gone",
    ];
    for (i, prefix) in stopping.iter().enumerate() {
        let p = h7_with_prefix(&format!("circular_imports_unplaced_{i}"), prefix);
        p.write("pkg/broken.py", "def broken(:\n");
        assert_eq!(reports(&p), Vec::<String>::new(), "{prefix}");
    }
    // A module outside the cycle whose load imports a module heki cannot name.
    let p = h7_with_prefix("circular_imports_unplaced_outside", "import pkg.other");
    p.write(
        "pkg/other.py",
        "import importlib\nimport os\n\nimportlib.import_module(os.environ[\"M\"])\n",
    );
    assert_eq!(reports(&p), Vec::<String>::new());
    let passing = [
        "import json\nimport pkg.other",
        "import importlib\nimportlib.import_module(\"pkg.other\")",
    ];
    for (i, prefix) in passing.iter().enumerate() {
        let p = h7_with_prefix(&format!("circular_imports_placed_{i}"), prefix);
        assert_eq!(
            reports(&p),
            ["pkg/b.py:5 [pkg.a.helper_a] pkg.a"],
            "{prefix}"
        );
    }
}

/// When a function of the project may load a module of the cycle, code `pkg.a` runs before its
/// import of `pkg.b` may call it, and stops the run; what runs no code does not.
#[test]
fn code_that_may_call_a_loading_function_stops_the_run() {
    let loader = "def load():\n    import pkg.b\n";
    let silent = [
        "S = {1, 2}",
        "D = {\"a\": 1}",
        "class C:\n    pass",
        "import os\nif os.environ.get(\"X\"):\n    raise RuntimeError(\"no\")",
        "import os\nPATH = os.sep",
        "import os\nX = len(os.sep)",
    ];
    for (i, prefix) in silent.iter().enumerate() {
        let p = h7_with_prefix(&format!("circular_imports_code_{i}"), prefix);
        p.write("pkg/loader.py", loader);
        assert_eq!(reports(&p), Vec::<String>::new(), "{prefix}");
    }
    // A function importing a module heki cannot name may load one of the cycle too.
    let p = h7_with_prefix("circular_imports_code_any", silent[0]);
    p.write(
        "pkg/loader.py",
        "import importlib\n\n\ndef load(name):\n    return importlib.import_module(name)\n",
    );
    assert_eq!(reports(&p), Vec::<String>::new());
    let inert = [
        "__all__ = [\"helper_a\"]\nVALUE = (1, 2)\nNOTHING = {}",
        "from typing import TYPE_CHECKING\nif TYPE_CHECKING:\n    import json",
        "FLAG = None\nFLAG is None",
        "def helper():\n    return 1",
    ];
    for (i, prefix) in inert.iter().enumerate() {
        let p = h7_with_prefix(&format!("circular_imports_inert_{i}"), prefix);
        p.write("pkg/loader.py", loader);
        assert_eq!(
            reports(&p),
            ["pkg/b.py:5 [pkg.a.helper_a] pkg.a"],
            "{prefix}"
        );
    }
    // `TYPE_CHECKING` a star import of `typing` binds, read as a global by `pkg.b` before its
    // failing import (in `pkg.a`, the star import may bind `helper_a`), also when `pkg.b`
    // deletes the name at its end.
    for (i, end) in ["", "\ndel TYPE_CHECKING\n"].iter().enumerate() {
        let p = h7_with_prefix(&format!("circular_imports_inert_star_{i}"), "");
        p.write("pkg/loader.py", loader);
        p.write(
            "pkg/b.py",
            &format!(
                "from typing import *\n\nif TYPE_CHECKING:\n    import json\n\n\ndef helper_b():\n    return 2\n\n\nfrom pkg.a import helper_a\n{end}"
            ),
        );
        assert_eq!(reports(&p), ["pkg/b.py:11 [pkg.a.helper_a] pkg.a"], "{end}");
    }
}

/// A module of the cycle that loads whole before the failing import runs its code after its
/// last import: with a function that may load the cycle, that stops the run too.
#[test]
fn code_after_the_last_import_of_a_module_loaded_whole_stops_the_run() {
    let p = project(
        "circular_imports_code_after",
        &[
            ("pkg/__init__.py", ""),
            (
                "pkg/a.py",
                "import pkg.b\nimport pkg.c\n\n\ndef f():\n    return 1\n",
            ),
            ("pkg/b.py", "import pkg.a\n\nprint(\"loaded\")\n"),
            ("pkg/c.py", "from pkg.a import f\n"),
        ],
    );
    assert_eq!(reports(&p), ["pkg/c.py:1 [pkg.a.f] pkg.a pkg.b"]);
    // Importing `pkg.b` first fails before `pkg.b` runs its code.
    p.write("pkg/loader.py", "def load():\n    import pkg.c\n");
    assert_eq!(reports(&p), ["pkg/c.py:1 [pkg.a.f] pkg.b"]);
}

/// `from pkg import b`, where the package binds `b` and has a module `pkg.b`: the import may load
/// the submodule, a module of the cycle that has not loaded, which stops the run.
#[test]
fn a_submodule_a_from_import_may_load_stops_the_run() {
    let p = project(
        "circular_imports_maybe_submodule",
        &[
            ("pkg/__init__.py", "b = None\n"),
            (
                "pkg/a.py",
                "from pkg import b\nimport pkg.b\n\n\ndef helper_a():\n    return 1\n",
            ),
            ("pkg/b.py", "from pkg.a import helper_a\n"),
        ],
    );
    assert_eq!(reports(&p), Vec::<String>::new());
    p.write("pkg/__init__.py", "");
    assert_eq!(reports(&p), ["pkg/b.py:1 [pkg.a.helper_a] pkg.a"]);
}
