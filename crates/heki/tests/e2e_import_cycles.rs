//! `IMP101` end to end: each group of project modules importing each other while they load is
//! reported once, at the import closing a shortest cycle from its first module by name. Imports
//! inside functions, under `TYPE_CHECKING`, and lazy imports the module body does not read count
//! only when `[tool.heki.imports] cycle-edges` names their class; the parent packages an import
//! loads first are steps, except those holding the importing module.

mod common;

use common::Project;
use serde_json::Value;

/// The `IMP101` reports of `heki check --select IMP101 <args>`, as JSON.
fn diagnostics(p: &Project, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["--select", "IMP101", "--output-format", "json"];
    all.extend_from_slice(args);
    let (code, out, err) = p.check(&all);
    assert!(code == 0 || code == 1, "{code}: {out}\n{err}");
    let all: Vec<Value> = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    (all.into_iter())
        .filter(|d| d["rule_code"] == "IMP101")
        .collect()
}

/// Each report as `path:line qualname [target]`.
fn reports(p: &Project, args: &[&str]) -> Vec<String> {
    (diagnostics(p, args).iter())
        .map(|d| {
            format!(
                "{}:{} {} [{}]",
                d["path"].as_str().unwrap(),
                d["line"],
                d["qualname"].as_str().unwrap(),
                d["target"].as_str().unwrap()
            )
        })
        .collect()
}

/// The message of the one report, and its related locations as `path:line message`.
fn the_report(p: &Project, args: &[&str]) -> (String, Vec<String>) {
    let found = diagnostics(p, args);
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
        "import_cycles_h7",
        &[
            ("pkg/__init__.py", ""),
            ("pkg/a.py", H7_A),
            ("pkg/b.py", H7_B),
            ("pkg/c.py", H7_C),
            ("pkg/d.py", H7_D),
        ],
    )
}

/// The H7 request: `pkg.a` and `pkg.b` import each other while they load, one report at the
/// import closing the cycle; `pkg.c` imports `pkg.d` only inside a function, which counts only
/// when `cycle-edges` asks for it.
#[test]
fn modules_importing_each_other_while_they_load_are_reported_once() {
    let p = h7();
    assert_eq!(reports(&p, &[]), ["pkg/b.py:1 <module> [pkg.a, pkg.b]"]);
    let (message, related) = the_report(&p, &[]);
    assert_eq!(
        message,
        "`<module>` imports `pkg.a` while `pkg.b` loads, which closes the import cycle `pkg.a` -> `pkg.b` -> `pkg.a`"
    );
    assert_eq!(
        related,
        ["pkg/a.py:1 `pkg.a` imports `pkg.b` while it loads"]
    );
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.imports]\ncycle-edges = [\"body\", \"lazy\", \"function\"]\n",
    );
    assert_eq!(
        reports(&p, &[]),
        [
            "pkg/b.py:1 <module> [pkg.a, pkg.b]",
            "pkg/d.py:1 <module> [pkg.c, pkg.d]"
        ]
    );
    let found = diagnostics(&p, &[]);
    assert_eq!(
        found[1]["related"][0]["message"],
        "`pkg.c` imports `pkg.d` in `helper_c`"
    );
}

/// `--select ALL` and a prefix never run the opt-in rule.
#[test]
fn only_its_code_selects_the_rule() {
    let p = h7();
    for select in ["ALL", "IMP"] {
        let (_, out, err) = p.heki(&["check", "--select", select, "--output-format", "json"]);
        let all: Vec<Value> = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {err}"));
        assert!(all.iter().all(|d| d["rule_code"] != "IMP101"), "{all:#?}");
    }
}

/// Three modules, each importing the next, the last importing the first: one report at the
/// last import, with the two others related.
#[test]
fn a_cycle_of_three_modules_is_one_report() {
    let p = project(
        "import_cycles_three",
        &[
            ("pkg/__init__.py", ""),
            ("pkg/a.py", "import pkg.b\n"),
            ("pkg/b.py", "from pkg import c\n"),
            ("pkg/c.py", "from . import a\n"),
        ],
    );
    let (message, related) = the_report(&p, &[]);
    assert_eq!(
        message,
        "`<module>` imports `pkg.a` while `pkg.c` loads, which closes the import cycle `pkg.a` -> `pkg.b` -> `pkg.c` -> `pkg.a`"
    );
    assert_eq!(
        related,
        [
            "pkg/a.py:1 `pkg.a` imports `pkg.b` while it loads",
            "pkg/b.py:1 `pkg.b` imports `pkg.c` while it loads"
        ]
    );
}

/// `import app.sub.leaf` runs `app/sub/__init__.py` first: when that imports the importer back,
/// the two modules import each other through the package, as CPython shows when it imports
/// `app.z` first. The packages holding a module are no step of its imports: `pkg/__init__.py`
/// importing `pkg.a`, which imports its sibling `pkg.b`, is no cycle. An import naming such a
/// package itself reads it while it loads: `from pkg import VALUE` in `pkg.b` closes a cycle
/// (and fails on CPython, whichever module is imported first).
#[test]
fn a_cycle_through_a_parent_package_is_reported_and_the_own_package_is_no_step() {
    let p = project(
        "import_cycles_parent",
        &[
            ("app/__init__.py", ""),
            ("app/z.py", "import app.sub.leaf\n"),
            ("app/sub/__init__.py", "from app import z\n"),
            ("app/sub/leaf.py", ""),
            ("pkg/__init__.py", "from pkg.a import VALUE\n"),
            ("pkg/a.py", "import pkg.b\n\nVALUE = 1\n"),
            ("pkg/b.py", ""),
        ],
    );
    assert_eq!(reports(&p, &[]), ["app/z.py:1 <module> [app.sub, app.z]"]);
    let (message, related) = the_report(&p, &[]);
    assert_eq!(
        message,
        "`<module>` imports `app.sub.leaf` while `app.z` loads, which loads its package `app.sub` first and closes the import cycle `app.sub` -> `app.z` -> `app.sub`"
    );
    assert_eq!(
        related,
        ["app/sub/__init__.py:1 `app.sub` imports `app.z` while it loads"]
    );
    p.write("pkg/b.py", "from pkg import VALUE\n");
    assert_eq!(
        reports(&p, &[]),
        [
            "app/z.py:1 <module> [app.sub, app.z]",
            "pkg/b.py:1 <module> [pkg, pkg.a, pkg.b]"
        ]
    );
}

/// An import under `if TYPE_CHECKING:` never runs: it closes a cycle only when `cycle-edges`
/// names `"type-checking"`.
#[test]
fn an_import_under_type_checking_closes_no_cycle_by_default() {
    let p = project(
        "import_cycles_type_checking",
        &[
            ("pkg/__init__.py", ""),
            (
                "pkg/model.py",
                "from pkg.store import Store\n\n\nclass Model:\n    store: Store\n",
            ),
            (
                "pkg/store.py",
                "from typing import TYPE_CHECKING\n\nif TYPE_CHECKING:\n    from pkg.model import Model\n\n\nclass Store:\n    def add(self, model: \"Model\") -> None:\n        pass\n",
            ),
        ],
    );
    assert_eq!(reports(&p, &[]), Vec::<String>::new());
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.imports]\ncycle-edges = [\"body\", \"type-checking\"]\n",
    );
    assert_eq!(
        reports(&p, &[]),
        ["pkg/store.py:4 <module> [pkg.model, pkg.store]"]
    );
    let (message, _) = the_report(&p, &[]);
    assert!(
        message.starts_with("`<module>` imports `pkg.model` under `TYPE_CHECKING`, which closes"),
        "{message}"
    );
}

/// A lazy import (Python 3.15) loads its module when its name is first used: one only a
/// function reads closes no cycle, one the module body reads does, and `"lazy"` counts both.
#[test]
fn a_lazy_import_closes_a_cycle_only_where_the_module_body_reads_it() {
    let p = project(
        "import_cycles_lazy",
        &[
            ("pkg/__init__.py", ""),
            ("pkg/a.py", "import pkg.b\n\nLIMIT = 3\n"),
            (
                "pkg/b.py",
                "lazy import pkg.a\n\n\ndef limit() -> int:\n    return pkg.a.LIMIT\n",
            ),
            ("pkg/c.py", "import pkg.d\n"),
            (
                "pkg/d.py",
                "__lazy_modules__ = [\"pkg.c\"]\nimport pkg.c\n\nUSED = pkg.c\n",
            ),
        ],
    );
    let py315 = ["--target-version", "py315"];
    assert_eq!(reports(&p, &py315), ["pkg/d.py:2 <module> [pkg.c, pkg.d]"]);
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.imports]\ncycle-edges = [\"body\", \"lazy\"]\n",
    );
    assert_eq!(
        reports(&p, &py315),
        [
            "pkg/b.py:1 <module> [pkg.a, pkg.b]",
            "pkg/d.py:2 <module> [pkg.c, pkg.d]"
        ]
    );
    let found = diagnostics(&p, &py315);
    assert!(
        (found[0]["message"].as_str().unwrap())
            .starts_with("`<module>` imports `pkg.a` at its first use, which closes"),
        "{found:#?}"
    );
}

/// Two groups of modules importing each other: one report each, in the order of their first
/// modules.
#[test]
fn each_group_is_reported_once() {
    let p = project(
        "import_cycles_groups",
        &[
            ("pkg/__init__.py", ""),
            ("pkg/a.py", "import pkg.b\nimport pkg.c\n"),
            ("pkg/b.py", "import pkg.a\nimport pkg.c\n"),
            ("pkg/c.py", "import pkg.a\n"),
            ("pkg/x.py", "import pkg.y\n"),
            ("pkg/y.py", "import pkg.x\n"),
            ("pkg/z.py", "import pkg.a\nimport pkg.x\n"),
        ],
    );
    assert_eq!(
        reports(&p, &[]),
        [
            "pkg/b.py:1 <module> [pkg.a, pkg.b, pkg.c]",
            "pkg/y.py:1 <module> [pkg.x, pkg.y]"
        ]
    );
    let found = diagnostics(&p, &[]);
    assert_eq!(
        found[0]["message"],
        "`<module>` imports `pkg.a` while `pkg.b` loads, which closes the import cycle `pkg.a` -> `pkg.b` -> `pkg.a`, one of the cycles of 3 modules that import each other"
    );
}

/// Of several imports of one module, the cycle steps through the one that runs earliest (in
/// the module body before inside a function), and of those through the first written.
#[test]
fn a_cycle_steps_through_the_import_that_runs_earliest() {
    let p = project(
        "import_cycles_earliest",
        &[
            ("pkg/__init__.py", ""),
            (
                "pkg/a.py",
                "def load():\n    import pkg.b\n\n\nimport pkg.b\nimport pkg.b as again\n",
            ),
            ("pkg/b.py", "import pkg.a\n"),
        ],
    );
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.imports]\ncycle-edges = [\"function\", \"body\"]\n",
    );
    let (_, related) = the_report(&p, &[]);
    assert_eq!(
        related,
        ["pkg/a.py:5 `pkg.a` imports `pkg.b` while it loads"]
    );
}

/// Where a group is reported depends on which modules import which, not on the order the
/// imports are written in: a baseline entry keeps matching after the imports are reordered.
#[test]
fn the_report_of_a_group_stays_where_it_is_when_imports_are_reordered() {
    let p = project(
        "import_cycles_baseline",
        &[
            ("pkg/__init__.py", ""),
            ("pkg/a.py", "import pkg.b\nimport pkg.c\n"),
            ("pkg/b.py", "import pkg.a\n"),
            ("pkg/c.py", "import pkg.a\n"),
        ],
    );
    let before = reports(&p, &[]);
    assert_eq!(before, ["pkg/b.py:1 <module> [pkg.a, pkg.b, pkg.c]"]);
    assert_eq!(
        p.check(&[
            "--select",
            "IMP101",
            "--baseline",
            "b.toml",
            "--update-baseline"
        ])
        .0,
        0
    );
    p.write("pkg/a.py", "import pkg.c\nimport pkg.b\n");
    p.write("pkg/b.py", "\nimport json\nimport pkg.a\n");
    assert_eq!(
        reports(&p, &[]),
        ["pkg/b.py:3 <module> [pkg.a, pkg.b, pkg.c]"]
    );
    let (code, _, err) = p.check(&["--select", "IMP101", "--baseline", "b.toml"]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains("All checks passed"), "{err}");
}

/// `from . import _speedups` of a name the package binds nowhere, with no module of that name
/// among the project's files, imports a compiled extension module: it reads nothing of the
/// package. Once the package binds the name, the import reads the package while it loads.
#[test]
fn a_name_the_package_binds_nowhere_closes_no_cycle_through_it() {
    let p = project(
        "import_cycles_extension",
        &[
            ("ext/__init__.py", "from .a import VALUE\n"),
            ("ext/a.py", "from . import _speedups\n\nVALUE = 1\n"),
        ],
    );
    assert_eq!(reports(&p, &[]), Vec::<String>::new());
    p.write(
        "ext/__init__.py",
        "_speedups = None\nfrom .a import VALUE\n",
    );
    assert_eq!(reports(&p, &[]), ["ext/a.py:1 <module> [ext, ext.a]"]);
}

/// `cycle-edges` takes the timing classes by name.
#[test]
fn an_unknown_timing_class_is_a_configuration_error() {
    let p = h7();
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.imports]\ncycle-edges = [\"body\", \"functions\"]\n",
    );
    let (code, _, err) = p.check(&["--select", "IMP101"]);
    assert_eq!(code, 2, "{err}");
    assert!(
        err.contains(
            "unknown `cycle-edges` value `functions` in imports (expected `body`, `lazy`, `function`, or `type-checking`)"
        ),
        "{err}"
    );
    assert!(err.contains("did you mean `function`?"), "{err}");
}
