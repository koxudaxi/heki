//! Effect boundaries on module bodies, end to end through the real binary: a scope naming
//! `<module>` checks the body of the modules it names, as a scope function is checked, with
//! what the bodies of its classes take from outside the project; a scope naming only the module
//! leaves its body out.

mod common;

use common::Project;
use serde_json::Value;

const GEN: &str = r#"from __future__ import annotations

import os
import sys

try:
    import yaml  # noqa: F401
except ImportError:
    yaml = None

if sys.version_info >= (3, 14):
    NEW = True
else:
    NEW = False

PLATFORM = sys.platform


def two_reads() -> tuple[str | None, str]:
    first = os.getenv("A")
    return first, sys.platform
"#;

/// Path, line, qualname, target, message, and sites of each `EFF201` report of `tree` under one
/// boundary with `scopes` and the rest of its keys, `boundary`.
fn reports(
    name: &str,
    scopes: &str,
    boundary: &str,
    tree: &[(&str, &str)],
) -> Vec<(String, u64, String, String, String, u64)> {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"EFF201\"]\n\n[[tool.heki.effect-boundaries]]\nname = \"b\"\nscopes = {scopes}\n{boundary}"
        ),
    )
    .write("pkg/__init__.py", "");
    for (path, text) in tree {
        p.write(path, text);
    }
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["qualname"].as_str().unwrap().to_owned(),
                d["target"].as_str().unwrap().to_owned(),
                d["message"].as_str().unwrap().to_owned(),
                d["sites"].as_u64().unwrap_or(1),
            )
        })
        .collect()
}

const ENVIRONMENT: &str = "forbid = [\"reads-environment\"]\nimport-probes = \"report\"\n";

/// Line, qualname, and sites of each report.
fn lines(found: &[(String, u64, String, String, String, u64)]) -> Vec<(u64, &str, u64)> {
    found
        .iter()
        .map(|(_, line, qualname, _, _, sites)| (*line, qualname.as_str(), *sites))
        .collect()
}

#[test]
fn a_scope_naming_module_bodies_checks_them_as_functions() {
    let found = reports(
        "boundary_module_body",
        "[\"pkg.*\", \"pkg.gen:*\", \"pkg.gen:<module>\"]",
        ENVIRONMENT,
        &[("pkg/gen.py", GEN)],
    );
    assert_eq!(lines(&found), [(11, "<module>", 3), (20, "two_reads", 2)]);
    assert_eq!(
        found[0].4,
        "`<module>` reads the environment (reads `sys.version_info`), which effect boundary `b` forbids"
    );
}

#[test]
fn a_scope_naming_only_the_module_leaves_its_body_out() {
    for (i, scopes) in [
        "[\"pkg.*\"]",
        "[\"pkg.gen:*\"]",
        "[\"pkg.gen\"]",
        "[\"pkg.gen:<*>\"]",
        "[\"pkg.other:<module>\"]",
    ]
    .into_iter()
    .enumerate()
    {
        let found = reports(
            &format!("boundary_module_body_out_{i}"),
            scopes,
            ENVIRONMENT,
            &[("pkg/gen.py", GEN), ("pkg/other.py", "X = 1\n")],
        );
        let module_bodies: Vec<_> = found.iter().filter(|f| f.2 == "<module>").collect();
        assert_eq!(module_bodies, Vec::<&_>::new(), "{scopes}");
    }
}

#[test]
fn every_form_naming_module_bodies() {
    for (i, scopes) in [
        "[\"pkg.gen:<module>\"]",
        "[\"pkg.gen.<module>\"]",
        "[\"pkg.*:<module>\"]",
        "[\"<module>\"]",
    ]
    .into_iter()
    .enumerate()
    {
        let found = reports(
            &format!("boundary_module_body_forms_{i}"),
            scopes,
            ENVIRONMENT,
            &[("pkg/gen.py", GEN)],
        );
        assert_eq!(lines(&found), [(11, "<module>", 3)], "{scopes}");
    }
}

#[test]
fn the_bodies_of_its_classes_read_with_the_module_body() {
    // A class body runs where its `class` statement runs, at import; a method runs only when it
    // is called.
    let settings = r#"import os
import sys


class Settings:
    home = os.getenv("HOME")

    class Inner:
        windows = sys.platform == "win32"

    def later(self) -> str | None:
        return os.getenv("LATER")
"#;
    let found = reports(
        "boundary_module_body_classes",
        "[\"pkg.settings:<module>\"]",
        ENVIRONMENT,
        &[("pkg/settings.py", settings)],
    );
    assert_eq!(lines(&found), [(6, "<module>", 2)]);
}

#[test]
fn the_module_body_reaches_through_its_calls() {
    let helpers = r#"import os
import time


def mode() -> str | None:
    return os.getenv("MODE")


def warm() -> None:
    time.sleep(1)
"#;
    let model =
        "class Registry:\n    def __init__(self) -> None:\n        self.items: list[str] = []\n";
    let boot = r#"from pkg.helpers import mode, warm
from pkg.model import Registry

MODE = mode()
warm()
REGISTRY = Registry()
REGISTRY.items = ["a"]
"#;
    let tree = [
        ("pkg/helpers.py", helpers),
        ("pkg/model.py", model),
        ("pkg/boot.py", boot),
    ];
    let found = reports(
        "boundary_module_body_calls",
        "[\"pkg.boot:<module>\"]",
        "forbid = [\"reads-environment\", \"sleep\", \"writes-targets\", \"calls\"]\nwrite-targets = [\"Registry.items\"]\ncall-targets = [\"pkg.helpers.warm\"]\n",
        &tree,
    );
    let got: Vec<(u64, &str)> = found
        .iter()
        .map(|(_, line, _, _, message, _)| (*line, message.as_str()))
        .collect();
    let forbids = |what: &str| format!("{what}, which effect boundary `b` forbids");
    let want = [
        (
            4,
            forbids("`<module>` reads the environment (calls `os.getenv` via `pkg.helpers.mode`)"),
        ),
        (5, forbids("`<module>` calls `pkg.helpers.warm`")),
        (
            5,
            forbids("`<module>` reaches sleep (`time.sleep` via `pkg.helpers.warm`)"),
        ),
        (
            7,
            forbids("`<module>` writes `Registry.items` (by attribute)"),
        ),
    ];
    let want: Vec<(u64, &str)> = want.iter().map(|(l, m)| (*l, m.as_str())).collect();
    assert_eq!(got, want);
}
