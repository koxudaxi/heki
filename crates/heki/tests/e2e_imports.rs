//! Import facts end to end (`heki inspect --imports`, ADR-022): what each import site loads and
//! when it runs, what loading a module loads while it loads, the kinds of code, and the errors.
//! `tests/import_verify/verify_imports.py` checks the same facts against CPython.

mod common;

use common::Project;
use serde_json::{Value, json};

/// `heki inspect --imports <query> --output-format json <extra>`: exit code, the JSON object,
/// stderr.
fn imports(p: &Project, query: &str, extra: &[&str]) -> (i32, Value, String) {
    let mut args = vec!["inspect", "--imports", query, "--output-format", "json"];
    args.extend_from_slice(extra);
    let (code, out, err) = p.heki(&args);
    let v = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}\n{err}"));
    (code, v, err)
}

/// The modules of the report.
fn modules(report: &Value) -> &Vec<Value> {
    report["modules"].as_array().unwrap()
}

fn module<'r>(report: &'r Value, name: &str) -> &'r Value {
    modules(report)
        .iter()
        .find(|m| m["module"] == name)
        .unwrap_or_else(|| panic!("no module {name}: {report:#}"))
}

/// Each site of `name` in one line: `line form[ name] timing[ in function][ guarded][ always]
/// -> target[ | may load][ ! missing]`.
fn sites(report: &Value, name: &str) -> Vec<String> {
    let text = |v: &Value| v.as_str().map(str::to_owned);
    module(report, name)["imports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| {
            let mut line = format!("{} {}", s["line"], s["form"].as_str().unwrap());
            if let Some(n) = text(&s["name"]) {
                line += &format!(" {n}");
            }
            line += &format!(" {}", text(&s["timing"]).unwrap_or("open".into()));
            if s["function"] != "<module>" {
                line += &format!(" in {}", s["function"].as_str().unwrap());
            }
            if s["guarded"] == true {
                line += " guarded";
            }
            if s["unconditional"] == true {
                line += " always";
            }
            line += &format!(" -> {}", text(&s["target"]).unwrap_or("-".into()));
            if let Some(may) = s["may_load"].as_array() {
                let may: Vec<&str> = may.iter().map(|m| m.as_str().unwrap()).collect();
                line += &format!(" | {}", may.join(" "));
            }
            if let Some(missing) = text(&s["missing"]) {
                line += &format!(" ! {missing}");
            }
            line
        })
        .collect()
}

/// `from pkg import name` of a name the package binds nowhere, with no module `pkg.name` among
/// the project's files: Python imports the submodule, which it finds outside them (a compiled
/// extension module) or not at all; the package loads either way.
#[test]
fn a_name_the_package_binds_nowhere_may_be_a_submodule_outside_the_files() {
    let p = Project::new("imports_unbound_name");
    p.write("ext/__init__.py", "VALUE = 1\n").write(
        "ext/user.py",
        "from ext import _speedups\nfrom ext import VALUE\n",
    );
    let (code, report, err) = imports(&p, "ext.user", &[]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        sites(&report, "ext.user"),
        strings(&[
            "1 from _speedups body always -> ext | ext._speedups",
            "2 from VALUE body always -> ext",
        ])
    );
}

/// `(project modules, other modules)` loading `name` loads while it loads.
fn loads(report: &Value, name: &str) -> (Vec<String>, Vec<String>) {
    let loads = &module(report, name)["loads"];
    let names = |v: &Value| -> Vec<String> {
        (v.as_array().unwrap().iter())
            .map(|n| n.as_str().unwrap().to_owned())
            .collect()
    };
    (names(&loads["project"]), names(&loads["external"]))
}

fn strings(names: &[&str]) -> Vec<String> {
    names.iter().map(|n| (*n).to_owned()).collect()
}

const APP_UTIL: &str = r#"import os.path
from os import path
from typing import TYPE_CHECKING
import importlib
if TYPE_CHECKING:
    import collections
try:
    import yaml
except ImportError:
    yaml = None
if os.name == "nt":
    import winreg


def load(n, flag):
    import csv
    importlib.import_module("json" if flag else "pickle")
    return importlib.import_module(n)


try:
    class Guarded:
        import decimal
        found = importlib.import_module("glob")
except ImportError:
    pass
for n in ("a", "b"):
    importlib.import_module("app." + n)
PLUGIN = importlib.import_module(".parse", __package__)
dyn = importlib.import_module(os.environ["X"])
old = __import__("fractions")
rel = __import__("parse", globals(), None, [], 1)
zero = __import__("shlex", level=0)
from .missing import gone
from ... import nothing
from app import parse as parse_function
from app import util as self_module
from app import opened
from os.path import *
import moved.sub
EITHER = importlib.import_module("moved.sub" if dyn else "json")
try:
    pass
finally:
    class Twice:
        import string
"#;

/// An application package with every kind of import site, a cycle, a package whose `__path__`
/// points elsewhere, a file heki cannot parse, a typing stub, tests, and scripts.
fn app() -> Project {
    let p = Project::new("imports_app");
    p.write("pyproject.toml", "[project]\nname = \"app\"\n")
        .write(
            "app/__init__.py",
            "from .parse import parse\nfrom . import util\nfrom .helpers import tool\n\n\ndef opened():\n    pass\n",
        )
        .write(
            "app/parse.py",
            "import json\n\n\ndef parse(x):\n    return json.loads(x)\n",
        )
        .write("app/helpers.py", "def tool():\n    pass\n")
        .write("app/opened.py", "VALUE = 1\n")
        .write("app/util.py", APP_UTIL)
        .write("app/cyc_a.py", "from . import cyc_b\n")
        .write("app/cyc_b.py", "from . import cyc_a\nimport app.parse\n")
        .write("app/broken.py", "def broken(:\n")
        .write("app/stub_only.pyi", "import json\n")
        .write("app/parse.pyi", "def parse(x: str) -> object: ...\n")
        .write("moved/__init__.py", "__path__ = [\"/elsewhere\"]\n")
        .write("moved/sub.py", "")
        .write("tests/test_app.py", "import app\n")
        .write("scripts/run.py", "import app.util\n")
        .write("conftest.py", "import pytest\n");
    p
}

#[test]
fn import_sites_say_what_they_load_and_when_they_run() {
    let p = app();
    let (code, report, err) = imports(&p, "*", &[]);
    // `app/broken.py` cannot be parsed.
    assert_eq!(code, 2, "{err}");
    assert!(err.contains("app/broken.py"), "{err}");
    assert_eq!(
        sites(&report, "app.util"),
        strings(&[
            "1 import body always -> os.path",
            "2 from path body always -> os | os.path",
            "3 from TYPE_CHECKING body always -> typing | typing.TYPE_CHECKING",
            "4 import body always -> importlib",
            "6 import type-checking -> collections",
            "8 import body guarded always -> yaml",
            "12 import body -> winreg",
            "16 import function in load always -> csv",
            "17 by_name function in load always -> - | json pickle",
            "18 by_name function in load always -> -",
            "23 import body guarded -> decimal",
            "24 by_name body guarded -> glob",
            "28 by_name body -> - | app.*",
            "29 by_name body always -> app.parse",
            "30 by_name body always -> -",
            "31 by_name body always -> fractions",
            "32 by_name body always -> -",
            "33 by_name body always -> shlex",
            "34 from gone body always -> - ! app.missing",
            "35 from nothing body always -> - ! ",
            "36 from parse body always -> app | app.parse",
            "37 from util body always -> app.util",
            "38 from opened body always -> app | app.opened",
            "39 star body always -> os.path",
            "40 import body always -> -",
            "41 by_name body always -> - | json",
            "46 import body -> string",
        ])
    );
    assert_eq!(
        sites(&report, "app"),
        strings(&[
            "1 from parse body always -> app.parse",
            "2 from util body always -> app.util",
            "3 from tool body always -> app.helpers",
        ])
    );
    let util = module(&report, "app.util");
    assert_eq!(util["imports"][0]["group"], "stdlib");
    assert_eq!(util["imports"][5]["group"], "third-party");
    assert_eq!(util["imports"][20]["group"], "project");
    // What loading a module loads: its `body` imports that run on every path through it with
    // no `ImportError` handler, with the parent packages of what they load, through the
    // project's modules. Neither `yaml` (guarded), `winreg` (on one branch), `collections`
    // (never runs), `csv` (in a function), `decimal` (in a class guarded around its statement),
    // nor `string` (in a class body, where heki does not tell the paths) is one.
    let app_loads = (
        strings(&["app.helpers", "app.parse", "app.util"]),
        strings(&[
            "fractions",
            "importlib",
            "json",
            "os",
            "os.path",
            "shlex",
            "typing",
        ]),
    );
    assert_eq!(loads(&report, "app"), app_loads);
    // A cycle: each module loads the other, and the package of the other first.
    let (project, external) = loads(&report, "app.cyc_a");
    assert_eq!(
        project,
        strings(&["app", "app.cyc_b", "app.helpers", "app.parse", "app.util"])
    );
    assert_eq!(external, app_loads.1);
    assert_eq!(
        sites(&report, "app.cyc_b"),
        strings(&[
            "1 from cyc_a body always -> app.cyc_a",
            "2 import body always -> app.parse",
        ])
    );
    // No facts: a file heki cannot parse, and a stub, whose imports never run.
    for name in ["app.broken", "app.stub_only"] {
        assert_eq!(module(&report, name)["imports"], Value::Null, "{name}");
    }
    let kinds: Vec<(&str, &str)> = modules(&report)
        .iter()
        .map(|m| (m["module"].as_str().unwrap(), m["kind"].as_str().unwrap()))
        .collect();
    assert_eq!(
        kinds,
        vec![
            ("app", "package"),
            ("app.broken", "package"),
            ("app.cyc_a", "package"),
            ("app.cyc_b", "package"),
            ("app.helpers", "package"),
            ("app.opened", "package"),
            ("app.parse", "package"),
            ("app.stub_only", "package"),
            ("app.util", "package"),
            ("conftest", "development"),
            ("moved", "open"),
            ("moved.sub", "open"),
            ("scripts.run", "open"),
            ("tests.test_app", "development"),
        ]
    );
}

#[test]
fn the_text_report_lists_each_site_and_what_the_module_loads() {
    let p = app();
    let (code, out, _) = p.heki(&["inspect", "--imports", "app.[bch]*", "."]);
    assert_eq!(code, 2);
    assert_eq!(
        out,
        "\
app.broken (app/broken.py, package code)
  imports: unknown (a typing stub, whose imports never run, or a file heki could not read)
  loads while it loads, at least:
    project modules: none
    other modules: none

app.cyc_a (app/cyc_a.py, package code)
  imports:
    1:15  from-import of `cyc_b`: app.cyc_b (project)  [body, relative, unconditional]
  loads while it loads, at least:
    project modules: app, app.cyc_b, app.helpers, app.parse, app.util
    other modules: fractions, importlib, json, os, os.path, shlex, typing

app.cyc_b (app/cyc_b.py, package code)
  imports:
    1:15  from-import of `cyc_a`: app.cyc_a (project)  [body, relative, unconditional]
    2:8  import: app.parse (project)  [body, unconditional]
  loads while it loads, at least:
    project modules: app, app.cyc_a, app.helpers, app.parse, app.util
    other modules: fractions, importlib, json, os, os.path, shlex, typing

app.helpers (app/helpers.py, package code)
  imports: none
  loads while it loads, at least:
    project modules: none
    other modules: none
"
    );
    let (_, out, _) = p.heki(&["inspect", "--imports", "app.util", "."]);
    for line in [
        "    6:12  import: collections (stdlib)  [type-checking]",
        "    16:12  import: csv (stdlib)  [function, in `load`, unconditional]",
        "    18:12  import by name: unknown  [function, in `load`, unconditional]",
        "    17:5  import by name: one of the modules below  [function, in `load`, unconditional]\n      may load: json, pickle",
        "    23:16  import: decimal (stdlib)  [body, guarded]",
        "    34:22  from-import of `gone`: app.missing, which the project does not have  [body, relative, unconditional]",
        "    35:17  from-import of `nothing`: nothing: climbs past the top-level package  [body, relative, unconditional]",
        "    39:1  star import: os.path (stdlib)  [body, unconditional]",
    ] {
        assert!(out.contains(line), "{line}\n{out}");
    }
}

/// From Python 3.15 on, a `lazy` import, and one a module's `__lazy_modules__` lists, load when
/// the name is first used: in the module body, that is while the module loads.
#[test]
fn lazy_imports_load_while_the_module_loads_only_when_its_body_reads_them() {
    let p = Project::new("imports_lazy");
    p.write(
        "lazy_app.py",
        "\
lazy import json
lazy from decimal import Decimal
import csv
__lazy_modules__ = [\"fractions\", \"textwrap\"]
import fractions
import textwrap
if csv:
    WRAPPED = textwrap.dedent(\"x\")
VALUE = json.dumps({})
try:
    pass
finally:
    import fractions
",
    );
    let (code, report, err) = imports(&p, "lazy_app", &["--target-version", "py315"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        sites(&report, "lazy_app"),
        strings(&[
            "1 import body always -> json",
            "2 from Decimal lazy always -> decimal | decimal.Decimal",
            "3 import body always -> csv",
            "5 import lazy always -> fractions",
            "6 import body -> textwrap",
            "13 import open always -> fractions",
        ])
    );
    let (_, text, _) = p.heki(&[
        "inspect",
        "--imports",
        "lazy_app",
        "--target-version",
        "py315",
        ".",
    ]);
    assert!(
        text.contains("    13:12  import: fractions (stdlib)  [timing unknown, unconditional]"),
        "{text}"
    );
    let (_, older, _) = imports(&p, "lazy_app", &["--target-version", "py314"]);
    assert_eq!(
        sites(&older, "lazy_app")[3..],
        strings(&[
            "5 import body always -> fractions",
            "6 import body always -> textwrap",
            "13 import body always -> fractions",
        ])
    );
}

/// A read of a lazily imported name loads the module of the binding it sees: of a name bound
/// twice, the later one; of a name bound on each branch, or rebound in a `try` body or by a
/// star import, maybe the lazy one; of a name rebound before any read, none. A read after a
/// `with` statement also sees the bindings its body held where it may have raised, since the
/// manager may suppress the exception. A read heki cannot place among several bindings (in a
/// class body) leaves them open.
#[test]
fn a_lazy_read_loads_the_import_whose_binding_it_sees() {
    let p = Project::new("imports_lazy_bindings");
    p.write(
        "lazy_names.py",
        "\
import sys
lazy import fractions as dep
lazy import decimal as dep
NUMBER = dep.Decimal
if sys.version_info >= (3, 15):
    lazy import tomllib as toml
else:
    lazy import plistlib as toml
LOADS = toml.loads
lazy import shlex
shlex = None
SPLIT = shlex
lazy import json
lazy import csv as json
class Table:
    dumps = json.dumps
lazy import heapq
class Queue:
    push = heapq.heappush
lazy import pickle as serializer
try:
    serializer = make_serializer()
except Exception:
    pass
DUMP = serializer.dumps
",
    )
    .write(
        "lazy_star.py",
        "lazy import string as join\nfrom os.path import *\nJOINED = join\n",
    )
    .write(
        "lazy_with.py",
        "\
import contextlib
lazy import fractions as dep
with contextlib.suppress(NameError):
    lazy import decimal as dep
    probe()
    dep = None
VALUE = dep
",
    );
    let (code, report, err) = imports(&p, "lazy_names", &["--target-version", "py315"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        sites(&report, "lazy_names"),
        strings(&[
            "1 import body always -> sys",
            "2 import lazy always -> fractions",
            "3 import body always -> decimal",
            "6 import body -> tomllib",
            "8 import body -> plistlib",
            "10 import lazy always -> shlex",
            "13 import open always -> json",
            "14 import open always -> csv",
            "17 import body -> heapq",
            "20 import body -> pickle",
        ])
    );
    assert_eq!(
        loads(&report, "lazy_names"),
        (vec![], strings(&["decimal", "sys"]))
    );
    let (_, report, _) = imports(&p, "lazy_star", &["--target-version", "py315"]);
    assert_eq!(
        sites(&report, "lazy_star"),
        strings(&["1 import body -> string", "2 star body always -> os.path"])
    );
    let (_, report, _) = imports(&p, "lazy_with", &["--target-version", "py315"]);
    assert_eq!(
        sites(&report, "lazy_with"),
        strings(&[
            "1 import body always -> contextlib",
            "2 import body -> fractions",
            "4 import body -> decimal",
        ])
    );
}

/// A lazy import loads at the first read of its binding, so an `ImportError` handler around
/// that read takes its failure: it is guarded when every read that may load it is.
#[test]
fn a_lazy_import_is_guarded_by_the_handler_around_the_read_loading_it() {
    let p = Project::new("imports_lazy_guarded");
    p.write(
        "optional.py",
        "\
lazy import zipfile
lazy import tarfile
try:
    zipfile.ZipFile
except ImportError:
    pass
try:
    tarfile.open
except ImportError:
    pass
ARCHIVE = tarfile.TarFile
",
    );
    let (code, report, err) = imports(&p, "optional", &["--target-version", "py315"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        sites(&report, "optional"),
        strings(&[
            "1 import body guarded always -> zipfile",
            "2 import body always -> tarfile",
        ])
    );
    assert_eq!(loads(&report, "optional"), (vec![], strings(&["tarfile"])));
}

/// A module body past its IR budget is left unmodeled, star imports on a branch included.
#[test]
fn a_star_import_in_a_module_body_past_its_budget_is_not_placed() {
    let names: Vec<String> = (0..200).map(|i| format!("a{i}")).collect();
    let mut lines = vec![
        "import os".to_owned(),
        "if os.name:".to_owned(),
        "    from os.path import *".to_owned(),
    ];
    let small = lines.join("\n") + "\n";
    lines.push(format!("{} = 0", names.join(" = ")));
    lines.extend(std::iter::repeat_n(
        "if os.name:\n    pass".to_owned(),
        1000,
    ));
    lines.push(format!("print({})", names.join(", ")));
    let p = Project::new("imports_star_past_budget");
    p.write("large.py", &(lines.join("\n") + "\n"))
        .write("small.py", &small);
    let (code, report, err) = imports(&p, "*", &[]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(module(&report, "large")["imports"], json!([]));
    assert_eq!(
        sites(&report, "small"),
        strings(&["1 import body always -> os", "3 star body -> os.path"])
    );
}

/// A by-name import whose module the code does not fix, and the from-import of a package's
/// name bound before its submodule is imported, are open; so is a module whose `__path__`
/// may lead the import elsewhere.
#[test]
fn imports_heki_cannot_decide_load_only_what_they_certainly_load() {
    let p = app();
    let (_, report, _) = imports(&p, "app.util", &[]);
    let util = &modules(&report)[0];
    let last = &util["imports"][24];
    assert_eq!(
        (&last["line"], &last["target"], &last.get("may_load")),
        (&json!(40), &Value::Null, &None)
    );
    let (project, _) = loads(&report, "app.util");
    assert_eq!(project, strings(&["app", "app.helpers", "app.parse"]));
}

#[test]
fn a_src_layout_has_package_and_development_code() {
    let p = Project::new("imports_src");
    p.write("pyproject.toml", "[project]\nname = \"acme\"\n")
        .write("src/acme/__init__.py", "from acme.core import engine\n")
        .write("src/acme/core/__init__.py", "")
        .write("src/acme/core/engine.py", "from ..settings import DEBUG\n")
        .write("src/acme/settings.py", "DEBUG = False\n")
        .write("tools/release.py", "import acme\n")
        .write("tests/conftest.py", "");
    let (code, report, err) = imports(&p, "*", &[]);
    assert_eq!(code, 0, "{err}");
    let kinds: Vec<(&str, &str, &str)> = modules(&report)
        .iter()
        .map(|m| {
            (
                m["module"].as_str().unwrap(),
                m["path"].as_str().unwrap(),
                m["kind"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            ("acme", "src/acme/__init__.py", "package"),
            ("acme.core", "src/acme/core/__init__.py", "package"),
            ("acme.core.engine", "src/acme/core/engine.py", "package"),
            ("acme.settings", "src/acme/settings.py", "package"),
            ("tests.conftest", "tests/conftest.py", "development"),
            ("tools.release", "tools/release.py", "development"),
        ]
    );
    assert_eq!(
        sites(&report, "acme.core.engine"),
        strings(&["1 from DEBUG body always -> acme.settings"])
    );
    assert_eq!(
        loads(&report, "tools.release").0,
        strings(&["acme", "acme.core", "acme.core.engine", "acme.settings"])
    );
}

#[test]
fn a_query_naming_no_module_fails() {
    let p = Project::new("imports_errors");
    p.write("pkg/__init__.py", "");
    let (code, v, err) = imports(&p, "nothing.here", &[]);
    assert_eq!(code, 2);
    assert_eq!(v["error"]["kind"], "unknown");
    assert!(
        err.contains("no module of the project is named `nothing.here`"),
        "{err}"
    );
    // As text, the error is all the output.
    let (code, out, err) = p.heki(&["inspect", "--imports", "nothing.here"]);
    assert_eq!(code, 2);
    assert!(out.is_empty(), "{out}");
    assert!(
        err.contains("no module of the project is named `nothing.here`"),
        "{err}"
    );
    let (code, v, _) = imports(&p, "a:b:c", &[]);
    assert_eq!(code, 2);
    assert_eq!(v["error"]["kind"], "invalid");
    let (code, _, err) = p.heki(&["inspect", "--imports", "--callers", "pkg"]);
    assert_eq!(code, 2);
    assert!(err.contains("cannot be used with"), "{err}");
}

/// A project root that is a package names its modules top-level: a relative import of the
/// root package itself names no module heki knows.
#[test]
fn a_relative_import_of_the_root_package_is_open() {
    let p = Project::new("imports_root_package");
    p.write("__init__.py", "")
        .write("a.py", "from . import b\nfrom .b import VALUE\n")
        .write("b.py", "VALUE = 1\n");
    let (code, report, err) = imports(&p, "a", &[]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        sites(&report, "a"),
        strings(&["1 from b body always -> -", "2 from VALUE body always -> b"])
    );
}

/// `heki inspect --imports '*'` walks what every module of a long chain loads: the walks
/// share one budget, and past it the rest say so.
#[test]
fn modules_past_the_budget_of_the_walks_load_what_heki_cannot_tell() {
    let p = Project::new("imports_long_chain");
    for i in 0..300 {
        p.write(&format!("m{i}.py"), &format!("import m{}\n", i + 1));
    }
    p.write("m300.py", "");
    let (code, out, err) = p.heki(&["inspect", "--imports", "*", "."]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("m0 (m0.py, open code)"), "{out}");
    assert!(
        out.contains("  loads while it loads: unknown (past the walks' budget)\n"),
        "{out}"
    );
}

/// How each site names its modules: by the full name, relative to its own module (leading dots,
/// or a relative name anchored at the module's own `__package__`; one anchored at a package the
/// code spells is no such name), or both, for a by-name import given both kinds of names; nothing
/// for a site whose modules heki cannot name.
#[test]
fn each_site_says_how_it_names_its_modules() {
    let p = Project::new("imports_spelling");
    p.write("pkg/__init__.py", "").write("pkg/b.py", "").write(
        "pkg/a.py",
        "import importlib\nfrom . import b\n\n\ndef load(flag: bool) -> object:\n    importlib.import_module(\".b\", __package__)\n    importlib.import_module(\".b\", \"pkg\")\n    __import__(\"b\", globals(), None, [], 1)\n    return importlib.import_module(\".b\" if flag else \"json\", __package__)\n",
    );
    let (code, report, err) = imports(&p, "pkg.a", &[]);
    assert_eq!(code, 0, "{err}");
    let mut spellings: Vec<(u64, &str)> = (module(&report, "pkg.a")["imports"].as_array().unwrap())
        .iter()
        .map(|s| {
            (
                s["line"].as_u64().unwrap(),
                s["spelling"].as_str().unwrap_or("-"),
            )
        })
        .collect();
    spellings.sort_unstable();
    assert_eq!(
        spellings,
        [
            (1, "absolute"),
            (2, "relative"),
            (6, "relative"),
            (7, "absolute"),
            (8, "-"),
            (9, "mixed")
        ]
    );
    let (_, out, _) = p.heki(&["inspect", "--imports", "pkg.a", "."]);
    for line in [
        "    1:8  import: importlib (stdlib)  [body, unconditional]",
        "    2:15  from-import of `b`: pkg.b (project)  [body, relative, unconditional]",
        "    8:5  import by name: unknown  [function, in `load`, unconditional]",
        "    9:12  import by name: one of the modules below  [function, relative and absolute names, in `load`, unconditional]",
    ] {
        assert!(out.contains(line), "{line}\n{out}");
    }
}
