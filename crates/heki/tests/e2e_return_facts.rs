//! What every `return` of a function gives, when it is narrower than the return annotation
//! (`ReturnFact::ty`), types its calls: a factory annotated `-> sqlite3.Connection` that opens a
//! file gives a connection on a file, one that opens a database in memory one in memory, and a
//! factory annotated with a base class that returns a subclass gives the subclass. A function
//! without a return annotation whose every return is a call giving an instance of one class (a
//! construction of a library class, of a class of its module or of another module) gives an
//! instance of that class, unless another module rebinds the name it calls. A module constant
//! that names a file (`ModuleDecls::file_constants`) names it wherever it is read, unless another
//! module rebinds it.

mod common;

use common::Project;
use serde_json::Value;

/// `(line, code with its tier, message)` of every `EFF101` report in `pkg/app.py`.
fn reports(p: &Project) -> Vec<(u64, String, String)> {
    let (code, stdout, stderr) = p.check(&["--select", "EFF101", "--output-format", "json"]);
    assert!(code == 0 || code == 1, "exit {code}: {stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<(u64, String, String)> = diags
        .iter()
        .filter(|d| d["path"] == "pkg/app.py")
        .map(|d| {
            (
                d["line"].as_u64().unwrap(),
                common::tiered_code(d).to_owned(),
                d["message"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

const DB: &str = r#"import sqlite3
from pathlib import Path

DATA_DIR = Path(__file__).resolve().parent
DB_PATH = DATA_DIR / "app.db"
MEMORY = ":memory:"
MOVED = Path(__file__).parent / "moved.db"


def connect() -> sqlite3.Connection:
    conn = sqlite3.connect(DB_PATH, check_same_thread=False)
    conn.row_factory = sqlite3.Row
    return conn


def connect_memory() -> sqlite3.Connection:
    return sqlite3.connect(MEMORY)


def connect_to(path: str) -> sqlite3.Connection:
    return sqlite3.connect(path)


def connect_either(flag: bool) -> sqlite3.Connection:
    if flag:
        return sqlite3.connect(DB_PATH)
    return sqlite3.connect(MEMORY)


class Fetcher:
    def fetch(self) -> object:
        return None


class Remote(Fetcher):
    def fetch(self) -> object:
        import requests

        return requests.get("https://example.com/", timeout=5)


def fetcher() -> Fetcher:
    return Remote()


def connect_moved() -> sqlite3.Connection:
    return sqlite3.connect(MOVED)
"#;

const APP: &str = r#"import sqlite3

from pkg import db
from pkg.db import DB_PATH, MOVED

conn = db.connect()


async def tp_global_from_factory() -> object:
    return conn.execute("SELECT 1").fetchone()


async def tp_factory_call() -> object:
    return db.connect().execute("SELECT 1")


async def tn_memory_factory() -> object:
    return db.connect_memory().execute("SELECT 1")


async def th_path_factory(path: str) -> object:
    return db.connect_to(path).execute("SELECT 1")


async def th_either_factory() -> object:
    return db.connect_either(True).execute("SELECT 1")


async def tp_imported_path_constant() -> object:
    return sqlite3.connect(DB_PATH).execute("SELECT 1")


async def th_rebound_path_constant() -> object:
    return sqlite3.connect(MOVED).execute("SELECT 1")


async def tp_subclass_factory() -> object:
    return db.fetcher().fetch()


async def tp_module_attribute_path() -> object:
    return sqlite3.connect(db.DB_PATH).execute("SELECT 1")


async def th_rebound_factory_constant() -> object:
    return db.connect_moved().execute("SELECT 1")
"#;

const ELSEWHERE: &str = r#"from pkg import db


def use_memory() -> None:
    db.MOVED = ":memory:"
"#;

#[test]
fn a_factory_gives_what_every_return_gives_and_a_path_constant_names_its_file() {
    let p = Project::new("return_facts");
    p.write("pkg/__init__.py", "")
        .write("pkg/db.py", DB)
        .write("pkg/app.py", APP)
        .write("pkg/elsewhere.py", ELSEWHERE);
    let runs = |line: u64, f: &str, call: &str| {
        (
            line,
            "EFF101".to_owned(),
            format!("`{f}` runs the waiting call `{call}` on the event loop"),
        )
    };
    let reaches = |line: u64, f: &str, call: &str| {
        (
            line,
            "EFF101 [heuristic]".to_owned(),
            format!("`{f}` reaches the blocking call `{call}` on the event loop"),
        )
    };
    let mut expected =
        vec![
        runs(10, "tp_global_from_factory", "sqlite3.Connection.execute"),
        runs(10, "tp_global_from_factory", "sqlite3.Cursor.fetchone"),
        runs(14, "tp_factory_call", "sqlite3.Connection.execute"),
        (
            14,
            "EFF101".to_owned(),
            "`pkg.db.connect` always runs the waiting call `sqlite3.connect` on the event loop \
             (1 call down, on every path)"
                .to_owned(),
        ),
        // The path is the caller's argument.
        reaches(22, "th_path_factory", "sqlite3.Connection.execute"),
        reaches(22, "th_path_factory", "sqlite3.connect"),
        // One return opens a file, the other a database in memory.
        reaches(26, "th_either_factory", "sqlite3.Connection.execute"),
        reaches(26, "th_either_factory", "sqlite3.connect"),
        runs(30, "tp_imported_path_constant", "sqlite3.Connection.execute"),
        runs(30, "tp_imported_path_constant", "sqlite3.connect"),
        // Another module binds the constant to an in-memory name.
        reaches(34, "th_rebound_path_constant", "sqlite3.Connection.execute"),
        reaches(34, "th_rebound_path_constant", "sqlite3.connect"),
        (
            38,
            "EFF101".to_owned(),
            "`pkg.db.Remote.fetch` always runs the waiting call `requests.get` on the event loop \
             (1 call down, on every path)"
                .to_owned(),
        ),
        runs(42, "tp_module_attribute_path", "sqlite3.Connection.execute"),
        runs(42, "tp_module_attribute_path", "sqlite3.connect"),
        // The factory opens the constant another module binds to an in-memory name.
        reaches(
            46,
            "th_rebound_factory_constant",
            "sqlite3.Connection.execute",
        ),
        reaches(46, "th_rebound_factory_constant", "sqlite3.connect"),
    ];
    expected.sort();
    assert_eq!(reports(&p), expected);
}

const FS_BOUNDARY: &str = r#"[project]
name = "returns"
version = "0"
requires-python = ">=3.11"

[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "fs"
scopes = ["pkg.probe:*", "pkg.app:*"]
forbid = ["blocking-fs"]
"#;

const PROBE: &str = r#"from pathlib import Path


class Store:
    def save(self) -> None:
        Path("x").write_text("x")


def _make(name):
    return Path(name)


def _store():
    return Store()


def library_class(name: str) -> bool:
    return _make(name).exists()


def project_class() -> None:
    _store().save()
"#;

const MODELS: &str = r#"from pathlib import Path


class Other:
    def save(self) -> None:
        Path("y").write_text("y")
"#;

const FACTORIES: &str = r#"from pathlib import Path
from typing import Any

from pkg import models
from pkg.models import Other


class Swapped:
    def save(self) -> None:
        Path("z").write_text("z")


def other():
    return Other()


def other_by_module():
    return models.Other()


def either_path(flag: bool, name: str):
    if flag:
        return Path(name)
    return Path("default")


def bound_path(name):
    path = Path(name)
    return path


async def async_path(name):
    return Path(name)


def any_path(name) -> Any:
    return Path(name)


def mixed(flag: bool, name: str):
    if flag:
        return Path(name)
    return Other()


def maybe(flag: bool, name: str):
    if flag:
        return Path(name)
    return None


def swapped():
    return Swapped()
"#;

const SWAPPING: &str = r#"from pkg import factories


class Quiet:
    def save(self) -> None:
        pass


def swap() -> None:
    factories.Swapped = Quiet
"#;

const CALLERS: &str = r#"from pkg import factories, probe
from pkg.factories import bound_path


def tp_imported_function(name: str) -> bool:
    return probe._make(name).exists()


def tp_class_of_another_module() -> None:
    factories.other().save()


def tp_class_by_module_attribute() -> None:
    factories.other_by_module().save()


def tp_every_return_the_same_class(name: str) -> bool:
    return factories.either_path(True, name).exists()


def tp_local_copy(name: str) -> bool:
    return bound_path(name).exists()


async def tp_awaited(name: str) -> bool:
    return (await factories.async_path(name)).exists()


def tp_annotated_any(name: str) -> bool:
    return factories.any_path(name).exists()


def tn_two_classes(name: str) -> bool:
    return factories.mixed(True, name).exists()


def tn_maybe_none(name: str) -> bool:
    return factories.maybe(True, name).exists()


def tn_rebound_class() -> None:
    factories.swapped().save()
"#;

#[test]
fn a_function_without_a_return_annotation_gives_the_class_every_returned_call_gives() {
    let p = Project::new("return_facts_called");
    p.write("pyproject.toml", FS_BOUNDARY)
        .write("pkg/__init__.py", "")
        .write("pkg/probe.py", PROBE)
        .write("pkg/models.py", MODELS)
        .write("pkg/factories.py", FACTORIES)
        .write("pkg/swapping.py", SWAPPING)
        .write("pkg/app.py", CALLERS);
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut reports: Vec<(String, u64, String)> = diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["message"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    reports.sort();
    let reaches = |path: &str, line: u64, f: &str, via: &str| {
        (
            path.to_owned(),
            line,
            format!("`{f}` reaches blocking_fs (`{via}`), which effect boundary `fs` forbids"),
        )
    };
    let other_save = "pathlib.Path.write_text` via `pkg.models.Other.save";
    let mut expected = vec![
        reaches(
            "pkg/app.py",
            6,
            "tp_imported_function",
            "pathlib.Path.exists",
        ),
        reaches("pkg/app.py", 10, "tp_class_of_another_module", other_save),
        reaches("pkg/app.py", 14, "tp_class_by_module_attribute", other_save),
        reaches(
            "pkg/app.py",
            18,
            "tp_every_return_the_same_class",
            "pathlib.Path.exists",
        ),
        reaches("pkg/app.py", 22, "tp_local_copy", "pathlib.Path.exists"),
        reaches("pkg/app.py", 26, "tp_awaited", "pathlib.Path.exists"),
        reaches("pkg/app.py", 30, "tp_annotated_any", "pathlib.Path.exists"),
        reaches("pkg/probe.py", 6, "Store.save", "pathlib.Path.write_text"),
        reaches("pkg/probe.py", 18, "library_class", "pathlib.Path.exists"),
    ];
    expected.sort();
    assert_eq!(reports, expected);
    // `Store.save` is reported in the boundary itself; its caller reaches the same call.
    let (code, stdout, stderr) = p.heki(&[
        "inspect",
        "--output-format",
        "json",
        "pkg.probe.project_class",
    ]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    let inspected: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        inspected["purity"]["because"],
        serde_json::json!(["effects"])
    );
    let effects: Vec<&str> = inspected["effects"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["effect"].as_str().unwrap())
        .collect();
    assert_eq!(effects, ["blocking_fs"]);
}
