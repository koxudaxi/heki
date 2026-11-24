//! What every `return` of a function gives, when it is narrower than the return annotation
//! (`ReturnFact::ty`), types its calls: a factory annotated `-> sqlite3.Connection` that opens a
//! file gives a connection on a file, one that opens a database in memory one in memory, and a
//! factory annotated with a base class that returns a subclass gives the subclass. A module
//! constant that names a file (`ModuleDecls::file_constants`) names it wherever it is read, unless
//! another module rebinds it.

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
