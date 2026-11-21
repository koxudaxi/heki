//! Effect boundaries on what a function takes from the environment it runs in, end to end
//! through the real binary: `call-targets` naming functions outside the project by their full
//! name (`os.getenv`), `read-targets` naming module attributes (`sys.platform`), the
//! `reads-environment` token with the ecosystem catalog's list, and import probes
//! (`import-probes = "report"`).

mod common;

use common::Project;
use serde_json::Value;

const GENERATE: &str = r#"from __future__ import annotations

import importlib.util
import os
import platform
import sys
from importlib import import_module
from importlib.metadata import version


def by_getenv() -> str | None:
    return os.getenv("X")


def by_environ() -> str | None:
    return os.environ.get("X")


def by_environ_item() -> str:
    return os.environ["X"]


def by_version_info() -> bool:
    return sys.version_info >= (3, 14)


def by_platform() -> bool:
    return sys.platform == "win32"


def by_os_name() -> bool:
    return os.name == "nt"


def by_platform_system() -> str:
    return platform.system()


def by_find_spec() -> bool:
    return importlib.util.find_spec("yaml") is not None


def by_import_module() -> object:
    return import_module("yaml")


def by_metadata() -> str:
    return version("pydantic")


def by_guarded_import() -> bool:
    try:
        import yaml  # noqa: F401
    except ImportError:
        return False
    return True


def pure(x: int) -> int:
    return x + 1
"#;

/// Line, target, message, and sites of each `EFF201` report of `tree` under `boundary`.
fn reports(name: &str, boundary: &str, tree: &[(&str, &str)]) -> Vec<(u64, String, String, u64)> {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"EFF201\"]\n\n[[tool.heki.effect-boundaries]]\nname = \"env\"\nscopes = [\"pkg.generate.*\"]\n{boundary}"
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
                d["line"].as_u64().unwrap(),
                d["target"].as_str().unwrap().to_owned(),
                d["message"].as_str().unwrap().to_owned(),
                d["sites"].as_u64().unwrap_or(1),
            )
        })
        .collect()
}

fn lines_and_messages(found: &[(u64, String, String, u64)]) -> Vec<(u64, &str)> {
    found.iter().map(|(l, _, m, _)| (*l, m.as_str())).collect()
}

#[test]
fn reads_environment_reports_each_read_once_per_scope_function() {
    let found = reports(
        "env_token_shapes",
        "forbid = [\"reads-environment\"]\nimport-probes = \"report\"\n",
        &[("pkg/generate.py", GENERATE)],
    );
    let says = |f: &str, what: &str| {
        format!("`{f}` reads the environment ({what}), which effect boundary `env` forbids")
    };
    let expected = [
        (12, says("by_getenv", "calls `os.getenv`")),
        (16, says("by_environ", "reads `os.environ`")),
        (20, says("by_environ_item", "reads `os.environ`")),
        (24, says("by_version_info", "reads `sys.version_info`")),
        (28, says("by_platform", "reads `sys.platform`")),
        (32, says("by_os_name", "reads `os.name`")),
        (36, says("by_platform_system", "calls `platform.system`")),
        (40, says("by_find_spec", "calls `importlib.util.find_spec`")),
        (
            44,
            says("by_import_module", "calls `importlib.import_module`"),
        ),
        (
            48,
            says("by_metadata", "calls `importlib.metadata.version`"),
        ),
        (53, says("by_guarded_import", "probes the import of `yaml`")),
    ];
    let expected: Vec<(u64, &str)> = expected.iter().map(|(l, m)| (*l, m.as_str())).collect();
    assert_eq!(lines_and_messages(&found), expected);
    assert!(
        found
            .iter()
            .all(|(_, target, _, sites)| target == "env:reads-environment" && *sites == 1)
    );
}

#[test]
fn import_probes_are_reported_only_when_asked() {
    let found = reports(
        "env_token_no_probes",
        "forbid = [\"reads-environment\"]\n",
        &[("pkg/generate.py", GENERATE)],
    );
    assert_eq!(found.len(), 10);
    assert!(found.iter().all(|(line, ..)| *line != 53));
}

#[test]
fn an_excluded_catalog_entry_is_never_reported() {
    let found = reports(
        "env_token_excluded",
        "forbid = [\"reads-environment\"]\n\n[[tool.heki.ecosystem]]\nsymbol = \"os.getenv\"\nexcluded = true\n\n[[tool.heki.ecosystem]]\nsymbol = \"sys.platform\"\nexcluded = true\n",
        &[("pkg/generate.py", GENERATE)],
    );
    let lines: Vec<u64> = found.iter().map(|(line, ..)| *line).collect();
    assert_eq!(lines, [16, 20, 24, 32, 36, 40, 44, 48]);
}

#[test]
fn external_targets_match_calls_and_module_reads_by_their_full_names() {
    let found = reports(
        "env_explicit_targets",
        "forbid = [\"calls\", \"reads-targets\"]\nread-targets = [\"sys.version_info\", \"sys.platform\", \"os.name\", \"os.environ\"]\ncall-targets = [\"os.getenv\", \"importlib.util.find_spec\", \"platform.*\", \"importlib.import_module\", \"importlib.metadata.version\"]\n",
        &[("pkg/generate.py", GENERATE)],
    );
    let expected = [
        (12, "env:calls:os.getenv", "`by_getenv` calls `os.getenv`"),
        (
            16,
            "env:reads:os.environ",
            "`by_environ` reads `os.environ` (module_attr)",
        ),
        (
            20,
            "env:reads:os.environ",
            "`by_environ_item` reads `os.environ` (module_attr)",
        ),
        (
            24,
            "env:reads:sys.version_info",
            "`by_version_info` reads `sys.version_info` (module_attr)",
        ),
        (
            28,
            "env:reads:sys.platform",
            "`by_platform` reads `sys.platform` (module_attr)",
        ),
        (
            32,
            "env:reads:os.name",
            "`by_os_name` reads `os.name` (module_attr)",
        ),
        (
            36,
            "env:calls:platform.*",
            "`by_platform_system` calls `platform.*`",
        ),
        (
            40,
            "env:calls:importlib.util.find_spec",
            "`by_find_spec` calls `importlib.util.find_spec`",
        ),
        (
            44,
            "env:calls:importlib.import_module",
            "`by_import_module` calls `importlib.import_module`",
        ),
        (
            48,
            "env:calls:importlib.metadata.version",
            "`by_metadata` calls `importlib.metadata.version`",
        ),
    ];
    let got: Vec<(u64, &str, &str)> = found
        .iter()
        .map(|(l, t, m, _)| (*l, t.as_str(), m.as_str()))
        .collect();
    let want: Vec<(u64, &str, String)> = expected
        .iter()
        .map(|(l, t, m)| (*l, *t, format!("{m}, which effect boundary `env` forbids")))
        .collect();
    let want: Vec<(u64, &str, &str)> = want.iter().map(|(l, t, m)| (*l, *t, m.as_str())).collect();
    assert_eq!(got, want);
}

#[test]
fn entries_naming_project_functions_by_a_suffix_name_nothing_outside() {
    // `getenv`, `*.getenv`, and `*.platform` name project functions or attributes by a
    // suffix, `os:getenv` names project definitions by a module part, and `os.*.getenv` and
    // `version_info.major` name no external symbol by its full name.
    let found = reports(
        "env_suffix_entries",
        "forbid = [\"calls\", \"reads-targets\"]\nread-targets = [\"*.platform\", \"version_info.major\"]\ncall-targets = [\"getenv\", \"*.getenv\", \"os.*.getenv\", \"*.find_spec\", \"os:getenv\", \"importlib.util:find_spec\"]\n",
        &[("pkg/generate.py", GENERATE)],
    );
    assert_eq!(found, Vec::new());
}

#[test]
fn without_a_boundary_on_the_environment_nothing_changes() {
    for boundary in [
        "forbid = [\"blocking\"]\n",
        "forbid = [\"calls\"]\ncall-targets = [\"pkg.generate.pure\"]\n",
    ] {
        let found = reports(
            &format!("env_unrelated_{}", boundary.len()),
            boundary,
            &[("pkg/generate.py", GENERATE)],
        );
        assert_eq!(found, Vec::new(), "{boundary}");
    }
}

#[test]
fn reads_through_aliases_imported_names_and_helpers() {
    let generate = r#"import importlib.util as u
import sys
from os import environ
from sys import platform

from pkg import helpers


def by_imported_environ() -> str | None:
    return environ.get("X")


def by_module_alias() -> bool:
    return u.find_spec("yaml") is not None


def by_imported_platform() -> bool:
    return platform == "darwin"


def by_version_part() -> int:
    return sys.version_info.major


def through_helper() -> bool:
    return helpers.is_windows() and helpers.is_mac()


def local_import() -> str:
    from os import getenv

    return getenv("HOME") or ""


def by_class_body() -> type:
    class Settings:
        home = sys.platform

    return Settings


def unknown(module: object) -> object:
    return getattr(sys, "platform"), module


def by_class_copy() -> type:
    class Settings:
        platform = platform

    return Settings
"#;
    let helpers = r#"import sys


def is_windows() -> bool:
    return sys.platform == "win32"


def is_mac() -> bool:
    return sys.platform == "darwin"
"#;
    let found = reports(
        "env_aliases",
        "forbid = [\"reads-environment\"]\n",
        &[("pkg/generate.py", generate), ("pkg/helpers.py", helpers)],
    );
    let says = |f: &str, what: &str| {
        format!("`{f}` reads the environment ({what}), which effect boundary `env` forbids")
    };
    let expected = [
        (10, says("by_imported_environ", "reads `os.environ`"), 1),
        (
            14,
            says("by_module_alias", "calls `importlib.util.find_spec`"),
            1,
        ),
        (18, says("by_imported_platform", "reads `sys.platform`"), 1),
        (22, says("by_version_part", "reads `sys.version_info`"), 1),
        (
            26,
            says(
                "through_helper",
                "reads `sys.platform` via `pkg.helpers.is_windows`",
            ),
            2,
        ),
        (32, says("local_import", "calls `os.getenv`"), 1),
        (37, says("by_class_body", "reads `sys.platform`"), 1),
        // The class body reads the module's `platform` before it binds its own.
        (48, says("by_class_copy", "reads `sys.platform`"), 1),
    ];
    let got: Vec<(u64, &str, u64)> = found
        .iter()
        .map(|(l, _, m, sites)| (*l, m.as_str(), *sites))
        .collect();
    let want: Vec<(u64, &str, u64)> = expected
        .iter()
        .map(|(l, m, sites)| (*l, m.as_str(), *sites))
        .collect();
    assert_eq!(got, want);
}

#[test]
fn a_call_through_a_helper_names_the_helper() {
    let generate = r#"from pkg import helpers


def settings() -> str:
    return helpers.home()
"#;
    let helpers = r#"import os


def home() -> str:
    return os.getenv("HOME") or ""
"#;
    let tree = [("pkg/generate.py", generate), ("pkg/helpers.py", helpers)];
    let found = reports(
        "env_call_helper",
        "forbid = [\"reads-environment\"]\n",
        &tree,
    );
    assert_eq!(
        lines_and_messages(&found),
        [(
            5,
            "`settings` reads the environment (calls `os.getenv` via `pkg.helpers.home`), which effect boundary `env` forbids"
        )]
    );
    // An allowed callee's calls are allowed.
    let allowed = reports(
        "env_call_allowed",
        "forbid = [\"reads-environment\"]\nallowed-callees = [\"os.getenv\"]\n",
        &tree,
    );
    assert_eq!(allowed, Vec::new());
}

#[test]
fn import_probes_need_an_except_clause_naming_an_import_error() {
    let generate = r#"def plain() -> bool:
    try:
        import yaml  # noqa: F401
    except ImportError:
        return False
    return True


def among_others() -> bool:
    try:
        from yaml import CSafeLoader  # noqa: F401
    except (OSError, ModuleNotFoundError):
        return False
    return True


def dotted() -> bool:
    try:
        import black.mode  # noqa: F401
    except ValueError:
        return False
    except ImportError:
        return False
    return True


def nested() -> bool:
    try:
        try:
            import ryaml  # noqa: F401
        except ValueError:
            return False
    except ImportError:
        return False
    return True


def broad() -> bool:
    try:
        import tomllib  # noqa: F401
    except Exception:
        return False
    return True


def cleanup() -> bool:
    try:
        import json  # noqa: F401
    finally:
        print("done")
    return True


def guarded(lock: object) -> bool:
    try:
        with lock:  # type: ignore[attr-defined]
            import csv  # noqa: F401
    except ImportError:
        return False
    return True


def guarded_then_tested(lock: object, flag: bool) -> bool:
    try:
        with lock:  # type: ignore[attr-defined]
            import csv  # noqa: F401
        if flag:
            return True
    except ImportError:
        return False
    return False


def unguarded() -> object:
    import pickle

    return pickle
"#;
    let found = reports(
        "env_probes",
        "forbid = [\"reads-environment\"]\nimport-probes = \"report\"\n",
        &[("pkg/generate.py", generate)],
    );
    let probes = |f: &str, module: &str| {
        format!(
            "`{f}` reads the environment (probes the import of `{module}`), which effect boundary `env` forbids"
        )
    };
    let expected = [
        (3, probes("plain", "yaml")),
        (11, probes("among_others", "yaml.CSafeLoader")),
        (19, probes("dotted", "black.mode")),
        (30, probes("nested", "ryaml")),
    ];
    let expected: Vec<(u64, &str)> = expected.iter().map(|(l, m)| (*l, m.as_str())).collect();
    assert_eq!(lines_and_messages(&found), expected);
}

/// Read targets whose globs each compile but are too many to match together in one set.
fn many_globs() -> String {
    let globs: Vec<String> = (0..20_000)
        .map(|i| format!("\"*X{i}?[ab]*{{c,d}}*._*\""))
        .collect();
    globs.join(", ")
}

#[test]
fn import_probes_without_reads_environment_is_a_configuration_error() {
    for (boundary, error) in [
        (
            "forbid = [\"blocking\"]\nimport-probes = \"report\"\n",
            "effect-boundaries `env` sets `import-probes = \"report\"` but does not forbid `reads-environment`",
        ),
        (
            "forbid = [\"reads-environment\"]\nimport-probes = \"always\"\n",
            "unknown `import-probes` value `always` in effect-boundaries `env` (expected `allow` or `report`)",
        ),
        (
            "forbid = [\"reads-targets\"]\nread-targets = [\"sys.[x\"]\n",
            "invalid glob `sys.[x` in effect-boundaries.read-targets",
        ),
        (
            "forbid = [\"calls\"]\ncall-targets = [\"os.[g\"]\n",
            "invalid glob `os.[g` in effect-boundaries.call-targets",
        ),
        (
            &format!(
                "forbid = [\"reads-targets\"]\nread-targets = [{}]\n",
                many_globs()
            ),
            "the globs of effect-boundaries.read-targets are too many to match together",
        ),
    ] {
        let p = Project::new(&format!("env_config_error_{}", boundary.len()));
        p.write(
            "pyproject.toml",
            &format!(
                "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"EFF201\"]\n\n[[tool.heki.effect-boundaries]]\nname = \"env\"\nscopes = [\"pkg.*\"]\n{boundary}"
            ),
        )
        .write("pkg/__init__.py", "")
        .write("pkg/generate.py", GENERATE);
        let (code, stdout, stderr) = p.check(&[]);
        assert_eq!(code, 2, "{stdout}{stderr}");
        assert!(stderr.contains(error), "{stderr}");
        // A boundary whose rule does not run is not validated, as for the other contracts.
        let (code, stdout, stderr) = p.check(&["--select", "MUT201"]);
        assert_eq!(code, 0, "{stdout}{stderr}");
    }
}

/// The project's own modules, which an import names in `IMPORTS`: a package, its modules, a
/// directory without `__init__.py` inside it, and a package whose `__init__` lets installed
/// distributions add modules to it, and so to `pkg`, which holds it.
const OWN_MODULES: &[(&str, &str)] = &[
    ("pkg/names.py", "CONFIG = \"pkg.config\"\n"),
    ("pkg/config.py", "VALUE = 1\n"),
    ("pkg/parser/__init__.py", ""),
    ("pkg/parser/base.py", "VALUE = 1\n"),
    ("pkg/ns/mod.py", "VALUE = 1\n"),
    (
        "pkg/plugins/__init__.py",
        "from pkgutil import extend_path\n\n__path__ = extend_path(__path__, __name__)\n",
    ),
    ("pkg/generate/steps.py", "VALUE = 1\n"),
];

const IMPORTS: &str = r#"import importlib
import importlib.util
from importlib import import_module

from pkg.names import CONFIG

PARSER = "pkg.parser"
OUTSIDE = "yaml"
PACKAGE = "pkg"


def whole() -> object:
    return import_module("pkg.config")


def package() -> object:
    return importlib.import_module(name="pkg.parser")


def constants() -> tuple[object, object]:
    return import_module(PARSER), import_module(CONFIG)


def under_a_package(name: str) -> tuple[object, ...]:
    return (
        import_module(f"pkg.parser.{name}"),
        import_module("pkg.parser." + name),
        import_module("pkg.parser.%s" % name),
        import_module("pkg.parser.{}".format(name)),
        import_module(f"pkg.ns.{name}"),
    )


def either(flag: bool) -> object:
    return import_module("pkg.config" if flag else "pkg.parser.base")


def relative(name: str) -> tuple[object, ...]:
    return (
        import_module(".steps", __name__),
        import_module(".steps", package=__package__),
        import_module("..config", "pkg.parser"),
        import_module(f"..parser.{name}", __package__),
        import_module(".config", PACKAGE),
    )


def found() -> tuple[object, object]:
    return importlib.util.find_spec("pkg.config"), importlib.util.find_spec(".config", "pkg")


def outside() -> object:
    return import_module(OUTSIDE)


def missing() -> object:
    return import_module("pkg.missing")


def one_outside(flag: bool) -> object:
    return import_module("pkg.config" if flag else "yaml")


def no_dot(name: str) -> object:
    return import_module("pkg" + name)


def unknown(name: str) -> object:
    return import_module(name)


def case_changed() -> object:
    return import_module("pkg.config".upper())


def namespace() -> object:
    return import_module("pkg.ns")


def extended(name: str) -> object:
    return import_module(f"pkg.plugins.{name}")


def beyond() -> object:
    return import_module("...config", "pkg.parser")


def no_package() -> object:
    return import_module(".config")


def unknown_package(package: str) -> object:
    return import_module(".config", package)


def dots_only(name: str) -> object:
    return import_module(f".{name}", "pkg.parser")


def package_head(name: str) -> object:
    return import_module(".config", f"pkg.{name}")


def probing() -> object:
    try:
        return import_module("pkg.config")
    except ImportError:
        return None


def under_the_top_package(name: str) -> object:
    return import_module(f"pkg.{name}")
"#;

/// The tree of `OWN_MODULES` with `IMPORTS` in `pkg/generate/__init__.py`, and `extra`.
fn importing_tree<'t>(extra: &[(&'t str, &'t str)]) -> Vec<(&'t str, &'t str)> {
    let mut tree = OWN_MODULES.to_vec();
    tree.push(("pkg/generate/__init__.py", IMPORTS));
    tree.extend_from_slice(extra);
    tree
}

#[test]
fn importing_the_projects_own_modules_reads_no_environment() {
    let loader = r#"from importlib import import_module


def sibling() -> object:
    return import_module(".steps", __package__)
"#;
    // The module binds `__package__` itself: what it holds is not known.
    let rebinding = r#"from importlib import import_module

__package__ = str(__name__)


def sibling() -> object:
    return import_module(".steps", __package__)
"#;
    let found = reports(
        "env_own_modules",
        "forbid = [\"reads-environment\"]\n",
        &importing_tree(&[
            ("pkg/generate/loader.py", loader),
            ("pkg/generate/rebinding.py", rebinding),
        ]),
    );
    let reported: Vec<(u64, String)> = found
        .iter()
        .map(|(line, _, message, _)| {
            let calls = message
                .ends_with("reads the environment (calls `importlib.import_module`), which effect boundary `env` forbids");
            assert!(calls, "{message}");
            (*line, message.split('`').nth(1).unwrap().to_owned())
        })
        .collect();
    let expected = [
        (53, "outside"),
        (57, "missing"),
        (61, "one_outside"),
        (65, "no_dot"),
        (69, "unknown"),
        (73, "case_changed"),
        (77, "namespace"),
        (81, "extended"),
        (85, "beyond"),
        (89, "no_package"),
        (93, "unknown_package"),
        (97, "dots_only"),
        (101, "package_head"),
        (106, "probing"),
        (112, "under_the_top_package"),
        (7, "sibling"),
    ];
    let expected: Vec<(u64, String)> = expected
        .iter()
        .map(|(line, f)| (*line, (*f).to_owned()))
        .collect();
    assert_eq!(reported, expected);
}

#[test]
fn calls_of_importlib_stay_calls_for_call_targets() {
    let found = reports(
        "env_own_modules_calls",
        "forbid = [\"calls\", \"reads-environment\"]\ncall-targets = [\"importlib.import_module\"]\n",
        &importing_tree(&[]),
    );
    let whole: Vec<(&str, &str)> = found
        .iter()
        .filter(|(line, ..)| *line == 13)
        .map(|(_, target, message, _)| (target.as_str(), message.as_str()))
        .collect();
    assert_eq!(
        whole,
        [(
            "env:calls:importlib.import_module",
            "`whole` calls `importlib.import_module`, which effect boundary `env` forbids"
        )]
    );
}

#[test]
fn a_package_heki_cannot_read_may_hold_modules_of_other_distributions() {
    let p = Project::new("env_own_modules_unread");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"EFF201\"]\n\n[[tool.heki.effect-boundaries]]\nname = \"env\"\nscopes = [\"pkg.generate.*\"]\nforbid = [\"reads-environment\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write("pkg/legacy/__init__.py", "# -*- coding: cp1252 -*-\nVALUE = 1\n")
    .write("pkg/legacy/mod.py", "VALUE = 1\n")
    .write(
        "pkg/generate.py",
        "from importlib import import_module\n\n\ndef legacy(name: str) -> object:\n    return import_module(f\"pkg.legacy.{name}\")\n\n\ndef whole() -> object:\n    return import_module(\"pkg.legacy\")\n\n\ndef below() -> object:\n    return import_module(\"pkg.legacy.mod\")\n",
    );
    let (code, stdout, stderr) = p.check(&["--output-format", "concise"]);
    assert_eq!(code, 2, "{stdout}{stderr}");
    assert!(stdout.contains("pkg/generate.py:5:12: EFF201"), "{stdout}");
    assert!(!stdout.contains("pkg/generate.py:9:"), "{stdout}");
    // Its `__path__` may lead anywhere, also to another `mod`.
    assert!(stdout.contains("pkg/generate.py:13:12: EFF201"), "{stdout}");
}

/// A project whose `reads-environment` boundary checks `probes.py`.
const PROBES_CONFIG: &str = "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"EFF201\"]\n\n[[tool.heki.effect-boundaries]]\nname = \"env\"\nscopes = [\"probes.*\"]\nforbid = [\"reads-environment\"]\n";

/// `probes.py`: one function per probe `(function, name, reported)`, importing `name`, or a name
/// under it when it ends with a dot (`f"pkg.parser.{name}"`).
fn probes_source(probes: &[(String, String, bool)]) -> String {
    let mut out = String::from("from importlib import import_module\n");
    for (function, name, _) in probes {
        if name.ends_with('.') {
            out.push_str(&format!(
                "\n\ndef {function}(name: str) -> object:\n    return import_module(f\"{name}{{name}}\")\n"
            ));
        } else {
            out.push_str(&format!(
                "\n\ndef {function}() -> object:\n    return import_module(\"{name}\")\n"
            ));
        }
    }
    out
}

/// Writes `probes.py` and checks `p` with `args`: the probes reported and the probes expected
/// to be, both sorted.
fn reported_probes(
    p: &Project,
    probes: &[(String, String, bool)],
    args: &[&str],
) -> (Vec<String>, Vec<String>) {
    p.write("probes.py", &probes_source(probes));
    let mut all = vec!["--output-format", "json"];
    all.extend_from_slice(args);
    let (code, stdout, stderr) = p.check(&all);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut reported: Vec<String> = diags
        .iter()
        .map(|d| {
            let message = d["message"].as_str().unwrap();
            assert!(
                message.ends_with("reads the environment (calls `importlib.import_module`), which effect boundary `env` forbids"),
                "{message}"
            );
            message.split('`').nth(1).unwrap().to_owned()
        })
        .collect();
    reported.sort();
    let mut expected: Vec<String> = probes
        .iter()
        .filter(|(_, _, reported)| *reported)
        .map(|(function, ..)| function.clone())
        .collect();
    expected.sort();
    (reported, expected)
}

fn probe(function: &str, name: &str, reported: bool) -> (String, String, bool) {
    (function.to_owned(), name.to_owned(), reported)
}

#[test]
fn a_subpackage_changing_its_path_takes_names_under_its_parent_outside_the_project() {
    let p = Project::new("env_path_subpackage");
    p.write("pyproject.toml", PROBES_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/plugins/__init__.py", "__path__ = [\"/opt/plugins\"]\n")
        .write("pkg/parser/__init__.py", "")
        .write("clean/__init__.py", "");
    let probes = [
        // `name` may be `plugins.thirdparty`, found in `/opt/plugins`.
        probe("plugin", "pkg.", true),
        probe("sibling", "pkg.parser.", false),
        probe("unrelated", "clean.", false),
    ];
    let (reported, expected) = reported_probes(&p, &probes, &[]);
    assert_eq!(reported, expected);
}

/// What the `__init__.py` of a package under `pkg` does with its `__path__`: whether a name under
/// the package (`f"pkg.<package>.{name}"`) and its module `mod` (`"pkg.<package>.mod"`) are
/// imported from outside the project. Directories after the package's own leave its own modules
/// found first.
const OWN_PATH_SHAPES: &[(&str, &str, bool, bool)] = &[
    ("plain", "", false, false),
    ("reader", READING_PATH, false, false),
    (
        "appended",
        "__path__.append(\"/opt/plugins\")\n",
        true,
        false,
    ),
    (
        "extended",
        "__path__.extend([\"/opt/plugins\"])\n",
        true,
        false,
    ),
    ("added", "__path__ += [\"/opt/plugins\"]\n", true, false),
    (
        "concatenated",
        "__path__ = __path__ + [\"/opt/plugins\"]\n",
        true,
        false,
    ),
    (
        "concatenated_in_a_loop",
        "for directory in [\"/opt/plugins\", \"/opt/more\"]:\n    __path__ = __path__ + [directory]\n",
        true,
        false,
    ),
    (
        "concatenated_after_adding",
        "def setup():\n    global __path__\n    paths = __path__\n    paths += [\"/opt/plugins\"]\n    __path__ = paths + [\"/opt/more\"]\n",
        true,
        false,
    ),
    (
        "concatenated_to_a_parameter",
        "def setup(paths):\n    global __path__\n    __path__ = paths + [\"/opt/plugins\"]\n",
        true,
        true,
    ),
    // `paths` may be another list, which `__path__` then starts with.
    (
        "concatenated_after_a_choice",
        "def setup(flag):\n    global __path__\n    paths = __path__ if flag else [\"/opt/plugins\"]\n    __path__ = paths + [\"/opt/more\"]\n",
        true,
        true,
    ),
    (
        "extend_path",
        "from pkgutil import extend_path\n\n__path__ = extend_path(__path__, __name__)\n",
        true,
        false,
    ),
    (
        "extend_path_of_pkgutil",
        "import pkgutil\n\n__path__ = pkgutil.extend_path(__path__, __name__)\n",
        true,
        false,
    ),
    (
        "extend_path_imported",
        "__path__ = __import__(\"pkgutil\").extend_path(__path__, __name__)\n",
        true,
        false,
    ),
    (
        "declared",
        "__import__(\"pkg_resources\").declare_namespace(__name__)\n",
        true,
        false,
    ),
    (
        "declared_by_name",
        "import pkg_resources\n\npkg_resources.declare_namespace(\"pkg.declared_by_name\")\n",
        true,
        false,
    ),
    ("rebound", "__path__ = [\"/opt/plugins\"]\n", true, true),
    (
        "prepended",
        "__path__ = [\"/opt/plugins\"] + __path__\n",
        true,
        true,
    ),
    (
        "inserted",
        "__path__.insert(0, \"/opt/plugins\")\n",
        true,
        true,
    ),
    ("sliced", "__path__[:0] = [\"/opt/plugins\"]\n", true, true),
    ("deleted", "del __path__\n", true, true),
    (
        "in_a_function",
        "def setup():\n    __path__.insert(0, \"/opt/plugins\")\n",
        true,
        true,
    ),
    (
        "rebound_in_a_function",
        "def setup():\n    global __path__\n    __path__ = [\"/opt/plugins\"]\n",
        true,
        true,
    ),
    (
        "aliased",
        "def setup(flag):\n    paths = __path__ if flag else []\n    paths.insert(0, \"/opt/plugins\")\n",
        true,
        true,
    ),
    (
        "returned",
        "def paths():\n    return __path__\n",
        true,
        true,
    ),
    (
        "handed",
        "from pkg.registry import register\n\nregister(__path__)\n",
        true,
        true,
    ),
    (
        "handed_to_a_method",
        "from pkg.registry import Registry\n\nRegistry().add(__path__)\n",
        true,
        true,
    ),
    ("kept_in_a_global", "PATHS = __path__\n", true, true),
];

/// A package `__init__.py` that only reads its `__path__`.
const READING_PATH: &str = r#"import os
import pkgutil
from importlib import import_module

for _, name, _ in pkgutil.iter_modules(__path__):
    import_module(f"{__name__}.{name}")
walked = list(pkgutil.walk_packages(__path__, __name__ + "."))
first = __path__[0]
count = len(__path__)
for directory in __path__:
    os.listdir(directory)
found = any(os.path.isdir(d) for d in __path__)
where = __path__.index(first)
if __path__:
    names = [d for d in __path__]
"#;

/// The packages under `pkg` whose `__path__` another module changes, through its imports
/// (`CHANGING_PATHS`), as `OWN_PATH_SHAPES`.
const OTHERS_PATH_SHAPES: &[(&str, bool, bool)] = &[
    ("grown", true, false),
    ("shared", true, true),
    ("replaced", true, true),
    ("removed", true, true),
    ("listed", false, false),
    // Both packages then share one list.
    ("lent", true, true),
    ("borrower", true, true),
];

const CHANGING_PATHS: &str = r#"import pkgutil

import pkg.borrower
import pkg.grown
import pkg.lent
import pkg.listed
import pkg.removed
import pkg.replaced as replaced
from pkg.shared import __path__ as shared_path

pkg.grown.__path__.append("/opt/plugins")
shared_path.insert(0, "/opt/plugins")
replaced.__path__ = ["/opt/plugins"]
for info in pkgutil.iter_modules(pkg.listed.__path__):
    pass
del pkg.removed.__path__
pkg.borrower.__path__ = pkg.lent.__path__


def add_more():
    shared_path.append("/opt/more")


def rebind(module):
    module.__path__ = []


def lend(module):
    module.__path__ = pkg.lent.__path__
"#;

#[test]
fn a_package_whose_path_may_lead_outside_the_project_holds_modules_of_other_distributions() {
    let p = Project::new("env_path_shapes");
    p.write("pyproject.toml", PROBES_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/boot.py", CHANGING_PATHS)
        .write(
            "pkg/registry.py",
            "class Registry:\n    def add(self, paths):\n        return paths\n\n\ndef register(paths):\n    return paths\n",
        );
    let mut probes = vec![probe("under_pkg", "pkg.", true)];
    let shapes = OWN_PATH_SHAPES
        .iter()
        .map(|(package, init, under, module)| (*package, *init, *under, *module))
        .chain(
            OTHERS_PATH_SHAPES
                .iter()
                .map(|(package, under, module)| (*package, "", *under, *module)),
        );
    for (package, init, under, module) in shapes {
        p.write(&format!("pkg/{package}/__init__.py"), init)
            .write(&format!("pkg/{package}/mod.py"), "VALUE = 1\n");
        probes.push(probe(
            &format!("under_{package}"),
            &format!("pkg.{package}."),
            under,
        ));
        probes.push(probe(
            &format!("module_{package}"),
            &format!("pkg.{package}.mod"),
            module,
        ));
    }
    // Before 3.12 a comprehension is a function of its own, passed the path to iterate.
    for args in [&[][..], &["--target-version", "py311"]] {
        let (reported, expected) = reported_probes(&p, &probes, args);
        assert_eq!(reported, expected, "{args:?}");
    }
}

#[test]
fn the_packages_along_a_name_decide_where_it_is_found() {
    let p = Project::new("env_path_along");
    p.write("pyproject.toml", PROBES_CONFIG)
        .write("top/__init__.py", "__path__.append(\"/opt/plugins\")\n")
        .write("top/sub/__init__.py", "")
        .write("top/sub/mod.py", "VALUE = 1\n")
        .write("top/portion/mod.py", "VALUE = 1\n")
        .write("moved/__init__.py", "__path__ = [\"/opt/plugins\"]\n")
        .write("moved/sub/__init__.py", "")
        .write("moved/sub/mod.py", "VALUE = 1\n")
        .write("spread/sub/__init__.py", "")
        .write("spread/sub/mod.py", "VALUE = 1\n")
        .write("plain/__init__.py", "")
        .write("plain/mod.py", "VALUE = 1\n");
    let probes = [
        // `top/sub/__init__.py` is found before `/opt/plugins/sub`.
        probe("under_a_package_after", "top.sub.", false),
        probe("in_a_package_after", "top.sub.mod", false),
        // `top/portion` is one portion of a namespace package `/opt/plugins/portion` joins.
        probe("in_a_portion", "top.portion.mod", false),
        probe("under_a_portion", "top.portion.", true),
        probe("under_a_moved_package", "moved.sub.", true),
        probe("in_a_moved_package", "moved.sub.mod", true),
        // A namespace package has a portion in every directory of `sys.path`.
        probe("under_a_namespace", "spread.", true),
        probe("under_a_package_of_a_namespace", "spread.sub.", false),
        probe("in_a_package_of_a_namespace", "spread.sub.mod", false),
        probe("under_a_module", "plain.mod.", true),
    ];
    let (reported, expected) = reported_probes(&p, &probes, &[]);
    assert_eq!(reported, expected);
}
