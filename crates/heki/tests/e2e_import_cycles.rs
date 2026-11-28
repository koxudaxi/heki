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

/// `pkg/<name>.py` for each `(name, imports)`: it imports `f_<m>` from each module `m` of
/// `imports` while it loads and defines `f_<name>`.
fn modules(p: &Project, files: &[(&str, &[&str])]) {
    for (name, imports) in files {
        let mut text: String = (imports.iter())
            .map(|m| format!("from pkg.{m} import f_{m}\n"))
            .collect();
        text.push_str(&format!("\n\ndef f_{name}() -> int:\n    return 1\n"));
        p.write(&format!("pkg/{name}.py"), &text);
    }
}

/// A project of the package `pkg` and its modules ([`modules`]).
fn package(name: &str, files: &[(&str, &[&str])]) -> Project {
    let p = project(name, &[("pkg/__init__.py", "")]);
    modules(&p, files);
    p
}

/// `heki check --select IMP101 --baseline b.toml <args>`: exit code and stderr.
fn ratchet(p: &Project, args: &[&str]) -> (i32, String) {
    let mut all = vec!["--select", "IMP101", "--baseline", "b.toml"];
    all.extend_from_slice(args);
    let (code, _, err) = p.check(&all);
    (code, err)
}

/// Record the baseline `b.toml` of the project as it is.
fn record(p: &Project) {
    let (code, err) = ratchet(p, &["--update-baseline"]);
    assert_eq!(code, 0, "{err}");
}

/// The baseline file `--update-baseline` writes for the `IMP101` groups `targets` (a heuristic
/// report each).
fn group_entries(targets: &[&str]) -> String {
    let mut text = "# heki Ratchet baseline: counts may only shrink (`heki check --update-baseline`).\nversion = 3\n".to_owned();
    if targets.is_empty() {
        text.push_str("entries = []\n");
    }
    for target in targets {
        text.push_str(&format!(
            "\n[[entries]]\nrule_code = \"IMP101\"\ntarget = \"{target}\"\ncount = 1\nheuristic = 1\n"
        ));
    }
    text
}

/// A recorded group is a group of modules, not the import its report sits at: after the
/// imports of the cycle are reversed, the report sits at another import of the same modules,
/// which the entry covers without a note.
#[test]
fn a_group_whose_cycle_is_reversed_is_covered() {
    let p = package(
        "import_cycles_baseline_reversed",
        &[("a", &["b"]), ("b", &["c"]), ("c", &["a"])],
    );
    assert_eq!(
        reports(&p, &[]),
        ["pkg/c.py:1 <module> [pkg.a, pkg.b, pkg.c]"]
    );
    record(&p);
    assert_eq!(p.read("b.toml"), group_entries(&["pkg.a, pkg.b, pkg.c"]));
    modules(&p, &[("a", &["c"]), ("b", &["a"]), ("c", &["b"])]);
    assert_eq!(
        reports(&p, &[]),
        ["pkg/b.py:1 <module> [pkg.a, pkg.b, pkg.c]"]
    );
    let (code, err) = ratchet(&p, &[]);
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains("note:"), "{err}");
    record(&p);
    assert_eq!(p.read("b.toml"), group_entries(&["pkg.a, pkg.b, pkg.c"]));
}

/// When the import a group's report sits at is removed and another cycle of the same modules
/// remains, the report moves to that cycle and the entry still covers it.
#[test]
fn a_group_whose_reported_import_is_removed_is_covered() {
    let p = package(
        "import_cycles_baseline_moved",
        &[("a", &["b"]), ("b", &["a", "c"]), ("c", &["a"])],
    );
    assert_eq!(
        reports(&p, &[]),
        ["pkg/b.py:1 <module> [pkg.a, pkg.b, pkg.c]"]
    );
    record(&p);
    modules(&p, &[("b", &["c"])]);
    assert_eq!(
        reports(&p, &[]),
        ["pkg/c.py:1 <module> [pkg.a, pkg.b, pkg.c]"]
    );
    let (code, err) = ratchet(&p, &[]);
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains("note:"), "{err}");
    record(&p);
    assert_eq!(p.read("b.toml"), group_entries(&["pkg.a, pkg.b, pkg.c"]));
}

/// A group whose closing import is inside a function (`cycle-edges` counts those) stays covered
/// when the function is renamed: the entry names no function.
#[test]
fn a_group_closed_in_a_renamed_function_is_covered() {
    let p = project(
        "import_cycles_baseline_renamed",
        &[
            (
                "pyproject.toml",
                "[project]\nname = \"t\"\n\n[tool.heki.imports]\ncycle-edges = [\"body\", \"function\"]\n",
            ),
            ("pkg/__init__.py", ""),
            ("pkg/a.py", "import pkg.b\n"),
            ("pkg/b.py", "def load():\n    import pkg.a\n"),
        ],
    );
    assert_eq!(reports(&p, &[]), ["pkg/b.py:2 load [pkg.a, pkg.b]"]);
    record(&p);
    p.write("pkg/b.py", "def load_all():\n    import pkg.a\n");
    assert_eq!(reports(&p, &[]), ["pkg/b.py:2 load_all [pkg.a, pkg.b]"]);
    let (code, err) = ratchet(&p, &[]);
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains("note:"), "{err}");
    record(&p);
    assert_eq!(p.read("b.toml"), group_entries(&["pkg.a, pkg.b"]));
}

/// A group that loses modules is covered by the entry of the group it was: the entry can
/// shrink to it, and `--update-baseline` records it. With `report-unused-suppressions`, the
/// entry matches no report once none of its modules is in a group.
#[test]
fn a_group_that_loses_modules_is_covered_and_its_entry_can_shrink() {
    let p = package(
        "import_cycles_baseline_shrink",
        &[("a", &["b"]), ("b", &["c"]), ("c", &["d"]), ("d", &["a"])],
    );
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nreport-unused-suppressions = true\n",
    );
    record(&p);
    assert_eq!(
        p.read("b.toml"),
        group_entries(&["pkg.a, pkg.b, pkg.c, pkg.d"])
    );
    modules(&p, &[("c", &["a"]), ("d", &[])]);
    assert_eq!(
        reports(&p, &[]),
        ["pkg/c.py:1 <module> [pkg.a, pkg.b, pkg.c]"]
    );
    let (code, err) = ratchet(&p, &[]);
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains(
            "note: baseline entry IMP101 `pkg.a, pkg.b, pkg.c, pkg.d` can shrink to `pkg.a, pkg.b, pkg.c` (`--update-baseline`)"
        ),
        "{err}"
    );
    record(&p);
    assert_eq!(p.read("b.toml"), group_entries(&["pkg.a, pkg.b, pkg.c"]));
    let (code, err) = ratchet(&p, &[]);
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains("note:"), "{err}");
    modules(&p, &[("c", &[])]);
    assert_eq!(reports(&p, &[]), Vec::<String>::new());
    let (code, err) = ratchet(&p, &[]);
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains(
            "error: baseline entry IMP101 `pkg.a, pkg.b, pkg.c` matched no report (`--update-baseline` removes it)"
        ),
        "{err}"
    );
    record(&p);
    assert_eq!(p.read("b.toml"), group_entries(&[]));
}

/// A group that splits into two is covered by its entry, which can shrink to both.
#[test]
fn a_group_that_splits_is_covered_and_its_entry_can_shrink() {
    let p = package(
        "import_cycles_baseline_split",
        &[("a", &["b"]), ("b", &["c"]), ("c", &["d"]), ("d", &["a"])],
    );
    record(&p);
    modules(&p, &[("b", &["a"]), ("d", &["c"])]);
    assert_eq!(
        reports(&p, &[]),
        [
            "pkg/b.py:1 <module> [pkg.a, pkg.b]",
            "pkg/d.py:1 <module> [pkg.c, pkg.d]"
        ]
    );
    let (code, err) = ratchet(&p, &[]);
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains(
            "note: baseline entry IMP101 `pkg.a, pkg.b, pkg.c, pkg.d` can shrink to `pkg.a, pkg.b` and `pkg.c, pkg.d` (`--update-baseline`)"
        ),
        "{err}"
    );
    record(&p);
    assert_eq!(
        p.read("b.toml"),
        group_entries(&["pkg.a, pkg.b", "pkg.c, pkg.d"])
    );
    let (code, err) = ratchet(&p, &[]);
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains("note:"), "{err}");
}

/// A group with a module its entry lacks is new, and the note names the module; its entry is
/// not stale, since its modules are in a group. `--update-baseline` refuses.
#[test]
fn a_group_that_grows_is_a_regression() {
    let p = package(
        "import_cycles_baseline_grow",
        &[("a", &["b"]), ("b", &["a"]), ("c", &[])],
    );
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nreport-unused-suppressions = true\n",
    );
    record(&p);
    modules(&p, &[("b", &["a", "c"]), ("c", &["a"])]);
    assert_eq!(
        reports(&p, &[]),
        ["pkg/b.py:1 <module> [pkg.a, pkg.b, pkg.c]"]
    );
    let (code, err) = ratchet(&p, &[]);
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains(
            "note: IMP101 `pkg.a, pkg.b, pkg.c` is a new group: `pkg.c` joined the baseline group `pkg.a, pkg.b`"
        ),
        "{err}"
    );
    assert!(!err.contains("can shrink"), "{err}");
    assert!(!err.contains("matched no report"), "{err}");
    let (code, err) = ratchet(&p, &["--update-baseline"]);
    assert_eq!(code, 1, "{err}");
    assert!(err.contains("not updated: 1 entry(ies) grew"), "{err}");
    assert_eq!(p.read("b.toml"), group_entries(&["pkg.a, pkg.b"]));
}

/// Two recorded groups that become one are a new group, and so is one of modules no entry
/// holds: the notes name the groups it joins and the modules of none.
#[test]
fn two_groups_that_become_one_are_a_regression() {
    let p = package(
        "import_cycles_baseline_merge",
        &[
            ("a", &["b"]),
            ("b", &["a"]),
            ("c", &["d"]),
            ("d", &["c"]),
            ("e", &[]),
        ],
    );
    record(&p);
    assert_eq!(
        p.read("b.toml"),
        group_entries(&["pkg.a, pkg.b", "pkg.c, pkg.d"])
    );
    modules(&p, &[("b", &["a", "c"]), ("c", &["b", "d"])]);
    modules(&p, &[("f", &["g"]), ("g", &["f"])]);
    let (code, err) = ratchet(&p, &[]);
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains(
            "note: IMP101 `pkg.a, pkg.b, pkg.c, pkg.d` is a new group: it joins the baseline groups `pkg.a, pkg.b` and `pkg.c, pkg.d`\n"
        ),
        "{err}"
    );
    assert!(
        err.contains(
            "note: IMP101 `pkg.f, pkg.g` is a new group: no baseline entry holds any of its modules"
        ),
        "{err}"
    );
    modules(&p, &[("b", &["a", "e"]), ("e", &["c"]), ("c", &["b", "d"])]);
    let (code, err) = ratchet(&p, &[]);
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains(
            "note: IMP101 `pkg.a, pkg.b, pkg.c, pkg.d, pkg.e` is a new group: it joins the baseline groups `pkg.a, pkg.b` and `pkg.c, pkg.d` with `pkg.e`"
        ),
        "{err}"
    );
    assert!(!err.contains("can shrink"), "{err}");
    let (code, err) = ratchet(&p, &["--update-baseline"]);
    assert_eq!(code, 1, "{err}");
    assert_eq!(
        p.read("b.toml"),
        group_entries(&["pkg.a, pkg.b", "pkg.c, pkg.d"])
    );
}

/// An entry written when entries of groups named the file and function of the import closing
/// the shown cycle is read as the entry of its group, and `--update-baseline` rewrites it.
#[test]
fn an_entry_naming_the_closing_import_is_read_as_its_group() {
    let p = package(
        "import_cycles_baseline_old_form",
        &[("a", &["c"]), ("b", &["a"]), ("c", &["b"])],
    );
    p.write(
        "b.toml",
        "version = 2\n\n[[entries]]\npath = \"pkg/c.py\"\nqualname = \"<module>\"\nrule_code = \"IMP101\"\ntarget = \"pkg.a, pkg.b, pkg.c\"\ncount = 1\n",
    );
    assert_eq!(
        reports(&p, &[]),
        ["pkg/b.py:1 <module> [pkg.a, pkg.b, pkg.c]"]
    );
    let (code, err) = ratchet(&p, &[]);
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains("note: baseline entry"), "{err}");
    modules(&p, &[("b", &[]), ("c", &["a"])]);
    let (code, err) = ratchet(&p, &[]);
    assert_eq!(code, 0, "{err}");
    assert!(
        err.contains(
            "note: baseline entry IMP101 `pkg.a, pkg.b, pkg.c` can shrink to `pkg.a, pkg.c` (`--update-baseline`)"
        ),
        "{err}"
    );
    record(&p);
    assert_eq!(p.read("b.toml"), group_entries(&["pkg.a, pkg.c"]));
}

/// A run over part of the project sees part of a group: its entry covers what the run sees,
/// and stays as it is.
#[test]
fn a_run_over_part_of_a_group_keeps_its_entry() {
    let p = package("import_cycles_baseline_part", &[("a", &["b"])]);
    p.write("other/__init__.py", "");
    p.write(
        "pkg/b.py",
        "from pkg.a import f_a\nfrom other.c import f_c\n\n\ndef f_b() -> int:\n    return 1\n",
    );
    p.write(
        "other/c.py",
        "from pkg.a import f_a\n\n\ndef f_c() -> int:\n    return 1\n",
    );
    record(&p);
    assert_eq!(p.read("b.toml"), group_entries(&["other.c, pkg.a, pkg.b"]));
    assert_eq!(
        reports(&p, &["pkg"]),
        ["pkg/b.py:1 <module> [pkg.a, pkg.b]"]
    );
    let (code, err) = ratchet(&p, &["pkg"]);
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains("baseline entry"), "{err}");
    let (code, err) = ratchet(&p, &["pkg", "--update-baseline"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(p.read("b.toml"), group_entries(&["other.c, pkg.a, pkg.b"]));
}

/// A `--touched` run judges the entry of a group when it shows a group the entry holds.
#[test]
fn a_touched_run_judges_the_entry_of_a_group_it_shows() {
    let p = package(
        "import_cycles_baseline_touched",
        &[("a", &["b"]), ("b", &["c"]), ("c", &["d"]), ("d", &["a"])],
    );
    record(&p);
    modules(&p, &[("c", &["a"]), ("d", &[])]);
    let shrink = "note: baseline entry IMP101 `pkg.a, pkg.b, pkg.c, pkg.d` can shrink to `pkg.a, pkg.b, pkg.c` (`--update-baseline`)";
    let (code, err) = ratchet(&p, &["--touched", "pkg/c.py"]);
    assert_eq!(code, 0, "{err}");
    assert!(err.contains(shrink), "{err}");
    let (code, err) = ratchet(&p, &["--touched", "pkg/d.py"]);
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains(shrink), "{err}");
}

/// The packages `pkg` (`a` and `b` import each other, `c` imports `a`) and `other` (`c` and `d`
/// import each other): two groups, `pkg.a, pkg.b` and `other.c, other.d`.
fn two_groups(name: &str) -> Project {
    let p = package(name, &[("a", &["b"]), ("b", &["a"])]);
    p.write("other/__init__.py", "");
    p.write(
        "other/c.py",
        "from other.d import f_d\n\n\ndef f_c() -> int:\n    return 1\n",
    );
    p.write(
        "other/d.py",
        "from other.c import f_c\n\n\ndef f_d() -> int:\n    return 1\n",
    );
    p
}

/// A run over part of the project that reports a group records it, and keeps the entries of
/// the groups it does not cover.
#[test]
fn a_run_over_part_of_the_project_records_its_own_groups() {
    let p = two_groups("import_cycles_baseline_part_records");
    record(&p);
    let both = group_entries(&["other.c, other.d", "pkg.a, pkg.b"]);
    assert_eq!(p.read("b.toml"), both);
    let (code, err) = ratchet(&p, &["pkg", "--update-baseline"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(p.read("b.toml"), both);
}

/// A run covering every file of a recorded group decides it: dropped when no group is left,
/// replaced by the groups within it; the groups of other files stay.
#[test]
fn a_run_covering_a_group_drops_or_replaces_its_entry() {
    let p = two_groups("import_cycles_baseline_part_decides");
    p.write(
        "b.toml",
        &group_entries(&["other.c, other.d", "pkg.a, pkg.b, pkg.c"]),
    );
    modules(&p, &[("c", &["a"])]);
    let (code, err) = ratchet(&p, &["pkg", "--update-baseline"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        p.read("b.toml"),
        group_entries(&["other.c, other.d", "pkg.a, pkg.b"])
    );
    modules(&p, &[("a", &[]), ("b", &[])]);
    let (code, err) = ratchet(&p, &["pkg", "--update-baseline"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(p.read("b.toml"), group_entries(&["other.c, other.d"]));
}

/// A group an entry is kept for that a run does not cover entirely is not dropped, and the part
/// of it the run sees is not recorded beside it.
#[test]
fn a_group_with_a_file_the_run_does_not_cover_is_kept() {
    let p = two_groups("import_cycles_baseline_part_kept");
    p.write(
        "b.toml",
        &group_entries(&["other.c, other.d, pkg.a, pkg.b"]),
    );
    modules(&p, &[("a", &[]), ("b", &[])]);
    let (code, err) = ratchet(&p, &["pkg", "--update-baseline"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        p.read("b.toml"),
        group_entries(&["other.c, other.d, pkg.a, pkg.b"])
    );
}

/// `per-file-ignores` turning `IMP101` off on a file of a recorded group leaves the entry
/// unjudged; one on none of its files is reported when it matches no group.
#[test]
fn a_group_is_unused_only_where_the_rule_runs_on_all_its_files() {
    let p = two_groups("import_cycles_baseline_unused");
    p.write(
        "heki.toml",
        "[lint]\nreport-unused-suppressions = true\nper-file-ignores = { \"pkg/**\" = [\"IMP101\"] }\n",
    );
    p.write(
        "b.toml",
        &group_entries(&["other.c, other.d", "pkg.a, pkg.b"]),
    );
    let (code, err) = ratchet(&p, &[]);
    assert_eq!(code, 0, "{err}");
    assert!(!err.contains("matched no report"), "{err}");
    p.write("other/d.py", "def f_d() -> int:\n    return 1\n");
    let (code, err) = ratchet(&p, &[]);
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains("baseline entry IMP101 `other.c, other.d` matched no report"),
        "{err}"
    );
    assert!(!err.contains("`pkg.a, pkg.b` matched no report"), "{err}");
}

/// The help of the one report.
fn the_help(p: &Project, args: &[&str]) -> String {
    let found = diagnostics(p, args);
    assert_eq!(found.len(), 1, "{found:#?}");
    found[0]["help"].as_str().unwrap().to_owned()
}

/// A ring `r.a` -> ... -> `r.f` -> `r.a` with an inner cycle `r.b` -> `r.c` -> `r.d` -> `r.b`:
/// only the imports of `r.c` in `r.b` and of `r.d` in `r.c` are on both cycles.
fn ring_with_inner_cycle() -> Project {
    let mut files = vec![("r/__init__.py", "")];
    files.extend([
        ("r/a.py", "import r.b\n"),
        ("r/b.py", "import r.c\n"),
        ("r/c.py", "import r.d\n"),
        ("r/d.py", "import r.e\nimport r.b\n"),
        ("r/e.py", "import r.f\n"),
        ("r/f.py", "import r.a\n"),
    ]);
    project("import_cycles_cut", &files)
}

/// The help names the first import of the reported cycle that every cycle of the group goes
/// through, which heki checked with one search: moved into a function, or made lazy on Python
/// 3.15, it leaves the modules without a cycle. The import closing the reported cycle is not on
/// the inner cycle, so it is not named.
#[test]
fn the_import_on_every_cycle_of_a_group_is_named() {
    let p = ring_with_inner_cycle();
    assert_eq!(
        the_help(&p, &[]),
        "every import cycle of these modules goes through the import of `r.c` in `r.b`: move it \
         into the functions of `r.b` that use it"
    );
    assert_eq!(
        the_help(&p, &["--target-version", "py315"]),
        "every import cycle of these modules goes through the import of `r.c` in `r.b`: move it \
         into the functions of `r.b` that use it, or make it `lazy`"
    );
    p.write("r/b.py", "def load():\n    import r.c\n\n    return r.c\n");
    assert!(reports(&p, &[]).is_empty());
    p.write(
        "r/b.py",
        "lazy import r.c\n\n\ndef load():\n    return r.c\n",
    );
    assert!(reports(&p, &["--target-version", "py315"]).is_empty());
}

/// The import closing the reported cycle is named as this import when every cycle goes through
/// it; one inside a `try` statement, where Python rejects `lazy`, is only to be moved.
#[test]
fn the_reported_import_on_every_cycle_is_this_import() {
    let p = h7();
    let py315 = ["--target-version", "py315"];
    assert_eq!(
        the_help(&p, &py315),
        "every import cycle of these modules goes through this import: move it into the \
         functions that use it, or make it `lazy`"
    );
    p.write(
        "pkg/b.py",
        &format!(
            "try:\n    from pkg.a import helper_a\nexcept ImportError:\n    raise\n{}",
            &H7_B[26..]
        ),
    );
    assert_eq!(
        the_help(&p, &py315),
        "every import cycle of these modules goes through this import: move it into the \
         functions that use it"
    );
}

/// When no one import is on every cycle, or the change would make the import one that
/// `cycle-edges` counts, the help stays the general one.
#[test]
fn a_group_without_one_import_on_every_cycle_keeps_the_general_help() {
    const GENERAL: &str = "break the cycle at one of its imports: move what the modules share \
        into a module none of them imports, or import the name only where it is used";
    let p = project(
        "import_cycles_no_cut",
        &[
            ("q/__init__.py", ""),
            ("q/a.py", "import q.b\nimport q.c\n"),
            ("q/b.py", "import q.a\n"),
            ("q/c.py", "import q.a\n"),
        ],
    );
    assert_eq!(the_help(&p, &["--target-version", "py315"]), GENERAL);
    let p = project(
        "import_cycles_counted_cut",
        &[
            ("pkg/__init__.py", ""),
            ("pkg/a.py", H7_A),
            ("pkg/b.py", H7_B),
        ],
    );
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.imports]\ncycle-edges = [\"body\", \"function\"]\n",
    );
    assert_eq!(the_help(&p, &[]), GENERAL);
    assert_eq!(
        the_help(&p, &["--target-version", "py315"]),
        "every import cycle of these modules goes through this import: make it `lazy`"
    );
}

/// The help of a group without an import heki can name.
const GENERAL_CYCLE_HELP: &str = "break the cycle at one of its imports: move what the modules \
    share into a module none of them imports, or import the name only where it is used";

/// A cycle of `pkg.a` and `pkg.b` where `pkg.b` reads the module `pkg.a` while it loads: only
/// the import of `pkg.b` in `pkg.a` can be named, and only while nothing that runs while
/// `pkg.a` loads reads the name it binds.
fn cycle_through(a: &str) -> Project {
    let b = "import pkg.a\n\n\nclass Base:\n    pass\n\n\ndef route(f):\n    return f\n\n\n\
             routes = []\nOWNER = pkg.a\n";
    project(
        "import_cycles_load_time_use",
        &[("pkg/__init__.py", ""), ("pkg/a.py", a), ("pkg/b.py", b)],
    )
}

/// An import whose name only functions read is named: moved into them, or made lazy, it no
/// longer runs while its module loads, and the module still loads.
#[test]
fn an_import_only_functions_read_is_named() {
    let p = cycle_through("from pkg.b import Base\n\n\ndef make() -> object:\n    return Base()\n");
    let named = "every import cycle of these modules goes through the import of `pkg.b` in \
                 `pkg.a`: move it into the functions of `pkg.a` that use it";
    assert_eq!(the_help(&p, &[]), named);
    assert_eq!(
        the_help(&p, &["--target-version", "py315"]),
        format!("{named}, or make it `lazy`")
    );
}

/// An import a class statement reads for its base is needed while the module loads: moved into
/// a function, the base is unbound, and made lazy, it loads at the class statement. Neither
/// change is offered, and the help is the general one. So for a generic class, whose bases its
/// type-parameter scope reads.
#[test]
fn an_import_a_base_class_reads_is_not_named() {
    let p = cycle_through("from pkg.b import Base\n\n\nclass A(Base):\n    pass\n");
    assert_eq!(the_help(&p, &[]), GENERAL_CYCLE_HELP);
    assert_eq!(
        the_help(&p, &["--target-version", "py315"]),
        GENERAL_CYCLE_HELP
    );
    p.write(
        "pkg/a.py",
        "from pkg.b import Base\n\n\nclass A[T](Base):\n    kind = T\n",
    );
    assert_eq!(
        the_help(&p, &["--target-version", "py315"]),
        GENERAL_CYCLE_HELP
    );
}

/// A decorator runs while the module loads: the import of the decorator, and an import a
/// decorator of the module reads in its body, are not named.
#[test]
fn an_import_a_decorator_reads_is_not_named() {
    let p = cycle_through("from pkg.b import route\n\n\n@route\ndef index() -> None:\n    pass\n");
    assert_eq!(the_help(&p, &[]), GENERAL_CYCLE_HELP);
    p.write(
        "pkg/a.py",
        "from pkg.b import routes\n\n\ndef register(f):\n    routes.append(f)\n    return \
         f\n\n\nclass Views:\n    @register\n    def index(self) -> None:\n        pass\n",
    );
    assert_eq!(the_help(&p, &[]), GENERAL_CYCLE_HELP);
}

/// On Python 3.15 a `from` import of the module body may be made lazy, but not when the module
/// reads its name while it loads, there or in a function it calls: the import would load at
/// that read. An annotation is such a read before Python 3.14 only, where `def` evaluates it.
#[test]
fn a_lazy_import_is_not_offered_for_a_name_read_while_the_module_loads() {
    let py315 = ["--target-version", "py315"];
    let p = cycle_through("from pkg.b import Base\n\nDEFAULT = Base()\n");
    assert_eq!(the_help(&p, &py315), GENERAL_CYCLE_HELP);
    p.write(
        "pkg/a.py",
        "from pkg.b import Base\n\n\ndef make() -> object:\n    return Base()\n\n\nDEFAULT = \
         make()\n",
    );
    assert_eq!(the_help(&p, &py315), GENERAL_CYCLE_HELP);
    p.write(
        "pkg/a.py",
        "from pkg.b import Base\n\n\ndef make(kind: Base) -> None:\n    pass\n",
    );
    assert_eq!(
        the_help(&p, &["--target-version", "py313"]),
        GENERAL_CYCLE_HELP
    );
    assert_eq!(
        the_help(&p, &py315),
        "every import cycle of these modules goes through the import of `pkg.b` in `pkg.a`: \
         move it into the functions of `pkg.a` that use it, or make it `lazy`"
    );
}

/// Another module that takes the name an import binds while it loads needs the import where
/// it is: moved into a function, the module no longer has the name, and made lazy, it loads at
/// that read. So for a package that re-exports the name for its modules, a star import, and a
/// read off the module object; a name of the same module that no import binds is no such read,
/// nor is a name the module rebinds after the import with no code run in between, which other
/// modules see only once it is rebound. Code run in between may load a module reading it.
#[test]
fn an_import_whose_name_another_module_takes_while_it_loads_is_not_named() {
    let py315 = ["--target-version", "py315"];
    let p = cycle_through("from pkg.b import Base\n\n\ndef make() -> object:\n    return Base()\n");
    p.write("pkg/__init__.py", "from pkg.a import Base\n");
    p.write("pkg/c.py", "from pkg import Base\n\nDEFAULT = Base()\n");
    assert_eq!(the_help(&p, &[]), GENERAL_CYCLE_HELP);
    assert_eq!(the_help(&p, &py315), GENERAL_CYCLE_HELP);
    p.write("pkg/__init__.py", "");
    p.write("pkg/c.py", "from pkg.a import *\n");
    assert_eq!(the_help(&p, &py315), GENERAL_CYCLE_HELP);
    p.write("pkg/c.py", "import pkg.a\n\nDEFAULT = pkg.a.Base()\n");
    assert_eq!(the_help(&p, &py315), GENERAL_CYCLE_HELP);
    let named = "every import cycle of these modules goes through the import of `pkg.b` in \
                 `pkg.a`: move it into the functions of `pkg.a` that use it, or make it `lazy`";
    p.write("pkg/c.py", "import pkg.a as a\n\nDEFAULT = a.make()\n");
    assert_eq!(the_help(&p, &py315), named);
    p.write(
        "pkg/a.py",
        "from pkg.b import Base\n\n\ndef Base() -> object:\n    return object()\n",
    );
    p.write("pkg/c.py", "from pkg.a import Base\n\nDEFAULT = Base()\n");
    assert_eq!(the_help(&p, &py315), named);
}

/// A later statement rebinding the name hides the import's binding from other modules only
/// when no code runs between them: another import, a call, or a truth test there may load a
/// module that reads the name while the module loads. A `del` binds nothing in its place.
#[test]
fn code_between_an_import_and_its_rebinding_leaves_the_name_to_other_modules() {
    let py315 = ["--target-version", "py315"];
    let rebound = |between: &str| {
        format!("from pkg.b import Base\n{between}\n\ndef Base() -> object:\n    return object()\n")
    };
    let p = cycle_through(&rebound(""));
    p.write("pkg/c.py", "from pkg.a import Base\n\nDEFAULT = Base()\n");
    assert_eq!(
        the_help(&p, &py315),
        "every import cycle of these modules goes through the import of `pkg.b` in `pkg.a`: \
         move it into the functions of `pkg.a` that use it, or make it `lazy`"
    );
    for between in [
        "import json\n",
        "READY = print()\n",
        "FLAGS = []\nif FLAGS:\n    pass\n",
    ] {
        p.write("pkg/a.py", &rebound(between));
        assert_eq!(the_help(&p, &py315), GENERAL_CYCLE_HELP, "{between}");
    }
    p.write("pkg/a.py", "from pkg.b import Base\n\ndel Base\n");
    assert_eq!(the_help(&p, &py315), GENERAL_CYCLE_HELP);
}

/// A module that takes the name only in a function leaves the import free to be made lazy: it
/// loads at that read, after the modules have loaded. Moved into a function, the module no
/// longer has the name, so that change is not offered. A star import takes no private name,
/// but code the module object is passed to may.
#[test]
fn an_import_whose_name_another_module_takes_in_a_function_may_be_made_lazy() {
    let py315 = ["--target-version", "py315"];
    let lazy = "every import cycle of these modules goes through the import of `pkg.b` in \
                `pkg.a`: make it `lazy`";
    let p = cycle_through("from pkg.b import Base\n\n\ndef make() -> object:\n    return Base()\n");
    p.write(
        "pkg/__init__.py",
        "def base() -> type:\n    from pkg.a import Base\n\n    return Base\n",
    );
    assert_eq!(the_help(&p, &[]), GENERAL_CYCLE_HELP);
    assert_eq!(the_help(&p, &py315), lazy);
    p.write("pkg/__init__.py", "");
    p.write(
        "pkg/c.py",
        "import pkg.a\n\n\ndef make() -> object:\n    return pkg.a.Base()\n",
    );
    assert_eq!(the_help(&p, &py315), lazy);
    p.write(
        "pkg/a.py",
        "from pkg.b import Base as _Base\n\n\ndef make() -> object:\n    return _Base()\n",
    );
    p.write("pkg/c.py", "from pkg.a import *\n");
    assert_eq!(
        the_help(&p, &py315),
        "every import cycle of these modules goes through the import of `pkg.b` in `pkg.a`: \
         move it into the functions of `pkg.a` that use it, or make it `lazy`"
    );
    p.write(
        "pkg/c.py",
        "import pkg.a\n\n\ndef base() -> object:\n    return getattr(pkg.a, \"_Base\")\n",
    );
    assert_eq!(the_help(&p, &[]), GENERAL_CYCLE_HELP);
    assert_eq!(the_help(&p, &py315), lazy);
}

/// An augmented assignment of the module body reads its target while the module loads: the
/// import binding it is not named, moved or made lazy.
#[test]
fn an_import_an_augmented_assignment_reads_is_not_named() {
    let p = cycle_through("from pkg.b import routes\n\nroutes += []\n");
    assert_eq!(the_help(&p, &[]), GENERAL_CYCLE_HELP);
    assert_eq!(
        the_help(&p, &["--target-version", "py315"]),
        GENERAL_CYCLE_HELP
    );
}
