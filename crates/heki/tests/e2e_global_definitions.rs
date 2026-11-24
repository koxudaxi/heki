//! A `class` or `def` statement in a function (or a class body) that declares its name `global`,
//! end to end through the real binary. The statement binds the module's name, and CPython names
//! the definition as a module-level one (`Config`, `Config.run`), so heki does: contract targets,
//! annotations, imports, subclasses in other modules, and `heki inspect` name it so. Once the
//! module body runs such a function on every path, the name holds the definition for an exact
//! claim; a function the module body may not run (never at import, on one branch, nested in
//! another) leaves the name possibly unbound, and a later binding on some paths makes it one of
//! the objects. The code written in the definition keeps the function around it: its closure
//! cells, and with `attribution = "lexical"` the code of its class body.
//!
//! The fixtures mark each line with the report it must get ([`MARKS`]).

mod common;

use common::Project;
use serde_json::Value;

/// `(path, line, rule, open premises as (premise, cause))` of one report.
type Report = (String, u64, String, Vec<(String, String)>);

/// The mark ending a line, the rule reporting the line, and the premises its report leaves
/// open as `(premise, cause)` (none: exact).
type Mark = (
    &'static str,
    &'static str,
    &'static [(&'static str, &'static str)],
);

const MARKS: &[Mark] = &[
    ("# EFF101", "EFF101", &[]),
    (
        "# EFF101 [heuristic]",
        "EFF101 [heuristic]",
        &[("Every implementation", "incomplete-targets")],
    ),
    (
        "# EFF101 [heuristic] either",
        "EFF101 [heuristic]",
        &[("Every implementation", "implementation-without-the-fact")],
    ),
];

/// Every `EFF101` report, before reports are grouped (`heki dump-open-premises`).
fn reported(p: &Project) -> Vec<Report> {
    let (code, stdout, stderr) = p.heki(&["dump-open-premises", "--select", "EFF101", "."]);
    assert_eq!(code, 0, "{stderr}");
    let out: Value = serde_json::from_str(&stdout).unwrap();
    let mut found: Vec<Report> = out["reports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let open = (r["open_premises"].as_array().unwrap().iter())
                .map(|o| {
                    (
                        o["premise"].as_str().unwrap().to_owned(),
                        o["cause"].as_str().unwrap().to_owned(),
                    )
                })
                .collect();
            (
                r["path"].as_str().unwrap().to_owned(),
                r["line"].as_u64().unwrap(),
                common::with_tier(
                    r["rule"].as_str().unwrap(),
                    r["certainty"].as_str().unwrap(),
                )
                .to_owned(),
                open,
            )
        })
        .collect();
    found.sort();
    found
}

/// The reports the marks of `files` ask for.
fn marked(files: &[(&str, &str)]) -> Vec<Report> {
    let mut out = Vec::new();
    for (path, text) in files {
        for (i, line) in text.lines().enumerate() {
            let Some((_, rule, open)) = MARKS.iter().find(|(m, ..)| line.trim_end().ends_with(m))
            else {
                continue;
            };
            let open = (open.iter())
                .map(|(p, c)| ((*p).to_owned(), (*c).to_owned()))
                .collect();
            out.push(((*path).to_owned(), i as u64 + 1, (*rule).to_owned(), open));
        }
    }
    out.sort();
    out
}

fn project(name: &str, files: &[(&str, &str)]) -> Project {
    let p = Project::new(name);
    p.write("pyproject.toml", "[project]\nname = \"t\"\n");
    p.write("pkg/__init__.py", "");
    for (path, text) in files {
        p.write(path, text);
    }
    p
}

const REPRO_CONFIG: &str = r#"[project]
name = "h23"
version = "0"

[tool.heki.lint]
select = ["MUT204"]
unmatched-contract-entries = "error"

[[tool.heki.call-owners]]
name = "config-private-call"
targets = ["pkg.config.ParserConfig._*", "pkg.config.JsonParserConfig._*"]
forbidden-callers = ["pkg.use:*"]
"#;

const REPRO_MODULE: &str = r#"from __future__ import annotations


class ParserConfig:
    def __init__(self) -> None:
        self._seen = 0


def _define_configs() -> None:
    """Bind the subclasses at module level, as a function run once at import."""
    global JsonParserConfig  # noqa: PLW0603

    class JsonParserConfig(ParserConfig):
        def _bump(self) -> None:
            self._seen += 1


_define_configs()
"#;

const REPRO_USE: &str = r#"from __future__ import annotations

from pkg import config


def bumps(cfg: config.JsonParserConfig) -> None:
    cfg._bump()
"#;

/// The request's reproduction: the target names the method of the class the function binds, so
/// no entry is unmatched, and the call through the annotation is reported.
#[test]
fn the_class_a_function_binds_is_a_class_of_the_module() {
    let p = Project::new("global_definitions_repro");
    p.write("pyproject.toml", REPRO_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/config.py", REPRO_MODULE)
        .write("pkg/use.py", REPRO_USE);
    let (code, stdout, stderr) = p.heki(&["check", "--output-format", "json", "."]);
    assert_eq!(code, 1, "{stdout}\n{stderr}");
    assert!(!stderr.contains("names no"), "{stderr}");
    let reports: Value = serde_json::from_str(&stdout).unwrap();
    let found: Vec<(&str, &str, &str)> = (reports.as_array().unwrap().iter())
        .map(|r| {
            (
                r["rule_code"].as_str().unwrap(),
                r["qualname"].as_str().unwrap(),
                r["message"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        found,
        [(
            "MUT204",
            "bumps",
            "`bumps` calls `pkg.config.JsonParserConfig._bump` inside the forbidden callers of `config-private-call`"
        )]
    );
    // `heki inspect` finds the method under the module's name, as a method.
    let (code, stdout, stderr) = p.heki(&[
        "inspect",
        "pkg.config.JsonParserConfig._bump",
        "--output-format",
        "json",
    ]);
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    let inspected: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(inspected["function"]["qualname"], "JsonParserConfig._bump");
    assert_eq!(inspected["function"]["kind"], "method");
}

/// Definitions under `global`, each bound by a function the module body runs on every path or
/// not, or by a class body.
const DEFINITIONS: &str = r#"import os
import time


def _always():
    global Always, always_fn, counter

    counter = 0

    class Always:
        def __init__(self):
            time.sleep(1)

    def always_fn():
        time.sleep(1)


def _never():
    global Never

    class Never:
        def __init__(self):
            time.sleep(1)


def _branch():
    global Branch

    class Branch:
        def __init__(self):
            time.sleep(1)


def _outer():
    def _inner():
        global Nested

        class Nested:
            def __init__(self):
                time.sleep(1)

    _inner()


def _late():
    global Late

    class Late:
        def __init__(self):
            time.sleep(1)


def _sometimes():
    global Sometimes

    if os.environ.get("Z"):

        class Sometimes:
            def __init__(self):
                time.sleep(1)


def _reset():
    global Reset

    class Reset:
        def __init__(self):
            time.sleep(1)

    del Reset


class Other:
    def __init__(self):
        pass


class Blocking:
    def __init__(self):
        time.sleep(1)


class Typed[T](Blocking):
    pass


class Holder:
    global from_class_body

    def from_class_body():
        time.sleep(1)


_always()
if os.environ.get("X"):
    _branch()
_outer()
_late()
if os.environ.get("Y"):
    Late = Other
_sometimes()
_reset()
Other()
"#;

const USES: &str = r#"from pkg import defs
from pkg.defs import Always


async def through_the_module():
    defs.Always()  # EFF101


async def imported():
    Always()  # EFF101


async def a_def():
    defs.always_fn()  # EFF101


async def bound_by_a_class_body():
    defs.from_class_body()  # EFF101


async def no_method_of_the_class(h: defs.Holder):
    h.from_class_body()


async def never_run_at_import():
    defs.Never()  # EFF101 [heuristic]


async def run_on_a_branch():
    defs.Branch()  # EFF101 [heuristic]


async def nested_function():
    defs.Nested()  # EFF101 [heuristic]


async def bound_again_on_a_branch():
    defs.Late()  # EFF101 [heuristic] either


async def made_on_some_paths():
    defs.Sometimes()  # EFF101 [heuristic]


async def deleted_after():
    defs.Reset()  # EFF101 [heuristic]


async def a_generic_class_at_module_level():
    defs.Typed()  # EFF101


class Sub(defs.Always):
    pass


async def subclass_in_another_module():
    Sub()  # EFF101
"#;

/// The name holds the definition once the module body has run its function on every path (also
/// a `def`, a class body's binding, an import of the name, and a subclass in another module);
/// otherwise a call through it is no exact target.
#[test]
fn the_name_holds_the_definition_once_the_module_body_runs_its_function() {
    let files = [("pkg/defs.py", DEFINITIONS), ("pkg/use.py", USES)];
    let p = project("global_definitions_exact", &files);
    assert_eq!(reported(&p), marked(&files));
}

/// Without the `global` declarations the classes are local to their functions: nothing names
/// them through the module, and the class body's `def` is a method of its class; the classes the
/// module body defines keep their reports.
#[test]
fn local_classes_stay_local() {
    let local = DEFINITIONS
        .lines()
        .filter(|l| !l.trim_start().starts_with("global "))
        .collect::<Vec<_>>()
        .join("\n");
    let uses = USES
        .lines()
        .map(|l| l.split("  # ").next().unwrap_or(l))
        .collect::<Vec<_>>()
        .join("\n");
    let p = project(
        "global_definitions_local",
        &[("pkg/defs.py", &local), ("pkg/use.py", &uses)],
    );
    let found: Vec<(String, u64)> = (reported(&p).into_iter())
        .map(|(_, line, rule, _)| (rule, line))
        .collect();
    let line_of = |text: &str| USES.lines().position(|l| l.trim() == text).unwrap() as u64 + 1;
    assert_eq!(
        found,
        [
            ("EFF101".to_owned(), line_of("h.from_class_body()")),
            ("EFF101".to_owned(), line_of("defs.Typed()  # EFF101")),
        ]
    );
}

/// The code of a closure in such a definition keeps the function around it: the cells it
/// changes are that function's.
#[test]
fn closures_keep_the_function_around_them() {
    let p = project(
        "global_definitions_cells",
        &[(
            "pkg/store.py",
            r#"class Store:
    def __init__(self):
        self.items = []

    def setup(self):
        global helper

        def helper():
            self.items.append(1)

        helper()


def make(rows):
    global Holder

    class Holder:
        def fill(self):
            rows.append(1)

    Holder().fill()
"#,
        )],
    );
    for (symbol, cell, owner) in [
        ("pkg.store.helper", "self", "pkg.store.Store.setup"),
        ("pkg.store.Holder.fill", "rows", "pkg.store.make"),
    ] {
        let (code, stdout, stderr) =
            p.heki(&["inspect", "--heuristic", symbol, "--output-format", "json"]);
        assert_eq!(code, 0, "{stdout}\n{stderr}");
        let v: Value = serde_json::from_str(&stdout).unwrap();
        let changed: Vec<(&str, Option<&str>)> = (v["changes"]["globals"].as_array().unwrap())
            .iter()
            .map(|g| (g["name"].as_str().unwrap(), g["cell_of"].as_str()))
            .collect();
        assert_eq!(changed, [(cell, Some(owner))], "{symbol}");
    }
}

/// With `attribution = "lexical"`, a lambda in the class body is the code of the function
/// running the class statement, not of the module body.
#[test]
fn lexical_attribution_judges_the_class_body_in_its_function() {
    let p = Project::new("global_definitions_lexical");
    let config = |allowed: &str| {
        format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n\
             [[tool.heki.call-owners]]\nname = \"private\"\ntargets = [\"pkg.base.Engine._run\"]\n\
             allowed-callers = [\"{allowed}\"]\nattribution = \"lexical\"\n"
        )
    };
    p.write("pyproject.toml", &config("pkg.base:_define"))
        .write("pkg/__init__.py", "")
        .write(
            "pkg/base.py",
            r#"class Engine:
    def _run(self):
        return 1


def _define(engine: Engine):
    global Holder

    class Holder:
        value = (lambda: engine._run())()


_define(Engine())
"#,
        );
    let (code, stdout, stderr) = p.heki(&["check", "."]);
    assert_eq!(code, 0, "{stdout}\n{stderr}");
    p.write("pyproject.toml", &config("pkg.base:Engine"));
    let (code, stdout, stderr) = p.heki(&["check", "."]);
    assert_eq!(code, 1, "{stdout}\n{stderr}");
    assert!(
        stdout.contains("`_define` calls `pkg.base.Engine._run` outside the call owners"),
        "{stdout}"
    );
}

/// Two classes making each other through `global` declarations (`class A` holding `global B;
/// class B`, whose body holds `global A; class A`): with `attribution = "lexical"`, the walk
/// from a lambda in the inner class body to the code running it ends at the module body.
#[test]
fn lexical_attribution_ends_at_classes_making_each_other() {
    let p = Project::new("global_definitions_lexical_cycle");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n\
         [[tool.heki.call-owners]]\nname = \"private\"\ntargets = [\"pkg.base.Engine._run\"]\n\
         allowed-callers = [\"pkg.base:Engine\"]\nattribution = \"lexical\"\n",
    )
    .write("pkg/__init__.py", "")
    .write(
        "pkg/base.py",
        r#"class Engine:
    def _run(self):
        return 1


class A:
    global B

    class B:
        global A

        class A:
            value = (lambda: Engine()._run())()
"#,
    );
    let (code, stdout, stderr) = p.heki(&["check", "."]);
    assert_eq!(code, 1, "{stdout}\n{stderr}");
    assert!(
        stdout.contains("`<module>` calls `pkg.base.Engine._run` outside the call owners"),
        "{stdout}"
    );
}
