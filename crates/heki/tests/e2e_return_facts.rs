//! What every `return` of a function gives, when it is narrower than the return annotation
//! (`ReturnFact::ty`), types its calls: a factory annotated `-> sqlite3.Connection` that opens a
//! file gives a connection on a file, one that opens a database in memory one in memory, and a
//! factory annotated with a base class that returns a subclass gives the subclass. A function
//! without a return annotation whose every return is a call giving an instance of one class (a
//! construction of a library class, of a class of its module or of another module) gives an
//! instance of that class, unless another module rebinds the name it calls; a method does so for
//! a call on an instance when every definition the call may run agrees, also one returning an
//! attribute of `self` that only the class's own methods bind to one class. A module constant
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

const MEMBER_FACTORY: &str = r#"from pkg import models


def make():
    return models.Other()
"#;

const MEMBER_CALLER: &str = r#"from pkg.member_factory import make


def serve() -> None:
    make().save()
"#;

const MEMBER_REBINDING: &str = r#"from pkg import models


class Quiet:
    def save(self) -> None:
        pass


def swap() -> None:
    models.Other = Quiet
"#;

/// A function returning `models.Other()` gives callers the instance of `pkg.models.Other` only
/// while no module rebinds that member, not only the module `pkg.models` it loads.
#[test]
fn a_rebinding_of_the_member_a_factory_calls_drops_the_class_it_returns() {
    let p = Project::new("return_facts_called_member");
    p.write("pyproject.toml", FS_BOUNDARY)
        .write("pkg/__init__.py", "")
        .write("pkg/models.py", MODELS)
        .write("pkg/member_factory.py", MEMBER_FACTORY)
        .write("pkg/app.py", MEMBER_CALLER);
    let messages = |p: &Project| -> Vec<String> {
        let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
        assert!(code == 0 || code == 1, "exit {code}: {stdout}{stderr}");
        let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
        diags
            .iter()
            .map(|d| d["message"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(
        messages(&p),
        [
            "`serve` reaches blocking_fs (`pathlib.Path.write_text` via `pkg.models.Other.save`), which effect boundary `fs` forbids"
        ]
    );
    p.write("pkg/swapping.py", MEMBER_REBINDING);
    assert_eq!(messages(&p), Vec::<String>::new());
}

const METHOD_BOUNDARY: &str = r#"[project]
name = "returns"
version = "0"
requires-python = ">=3.11"

[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "fs"
scopes = ["pkg.callers:*"]
forbid = ["blocking-fs"]
"#;

const METHOD_MODELS: &str = r#"from pathlib import Path


class Store:
    def save(self) -> None:
        Path("x").write_text("x")


class Database:
    def __init__(self, url: str) -> None:
        self.url = url

    def query(self) -> str:
        return Path(self.url).read_text()

    async def fetch(self) -> str:
        return Path(self.url).read_text()


class Other:
    def save(self) -> None:
        pass

    def query(self) -> str:
        return ""
"#;

const SERVICES: &str = r#"from typing import Union

from pkg.models import Database, Other, Store


class Factory:
    def make(self):
        return Store()


class Held:
    def __init__(self, url: str) -> None:
        self._db = Database(url)

    def db(self):
        return self._db


class Pool:
    def __init__(self, url: str) -> None:
        if url:
            self._db = Database(url)
        else:
            self._db = Database("default")

    async def get_db(self):
        return self._db


class StoredOutside:
    def __init__(self) -> None:
        self._db = Database("x")

    def db(self):
        return self._db


def reset(s: StoredOutside) -> None:
    s._db = Database("y")


class Swapped:
    def __init__(self) -> None:
        self._db = Database("x")

    def swap(self) -> None:
        self._db = Other()

    def db(self):
        return self._db


class Closed:
    def __init__(self) -> None:
        self._db = Database("x")

    def close(self) -> None:
        self._db = None

    def db(self):
        return self._db


class Given:
    def __init__(self, db) -> None:
        self._db = db

    def db(self):
        return self._db


class Overridden:
    def make(self):
        return Store()


class OverridingSub(Overridden):
    def make(self):
        return Other()


class Optional:
    def __init__(self) -> None:
        self._db: Database | None = Database("x")

    def db(self):
        return self._db


class Maybe:
    def __init__(self, url: str) -> None:
        self._db = Database(url) if url else None

    def db(self):
        return self._db


class Public:
    def __init__(self) -> None:
        self.conn = Database("x")

    def get(self):
        return self.conn


class Base:
    def __init__(self) -> None:
        self._db = Database("x")

    def db(self):
        return self._db


class ClosedElsewhere:
    def __init__(self) -> None:
        self._db = Database("x")

    def db(self):
        return self._db


class Unioned:
    def __init__(self) -> None:
        self._db: "Union[Database, None]" = Database("x")

    def db(self):
        return self._db


class Shaped:
    def make(self):
        return Store()


class PropertySub(Shaped):
    @property
    def make(self):
        return Store


class Crowded:
    def make(self):
        return Store()


class Crowded0(Crowded):
    def make(self):
        return Store()


class Crowded1(Crowded):
    def make(self):
        return Store()


class Crowded2(Crowded):
    def make(self):
        return Store()


class Crowded3(Crowded):
    def make(self):
        return Store()


class Crowded4(Crowded):
    def make(self):
        return Store()


class Crowded5(Crowded):
    def make(self):
        return Store()


class Crowded6(Crowded):
    def make(self):
        return Store()


class Crowded7(Crowded):
    def make(self):
        return Store()


class Crowded8(Crowded):
    def make(self):
        return Store()
"#;

const SUBCLASSES: &str = r#"from pkg.models import Database, Other
from pkg.services import Base, Held


class SameSub(Held):
    def __init__(self) -> None:
        super().__init__("sub")
        self._db = Database("sub")


class OtherSub(Base):
    def __init__(self) -> None:
        super().__init__()
        self._db = Other()
"#;

const WIRING: &str = r#"from pkg.services import ClosedElsewhere


def configure(app) -> None:
    app.conn = object()


def close(c: ClosedElsewhere) -> None:
    c._db = None
"#;

const METHOD_CALLERS: &str = r#"from pkg.services import (
    Base,
    Closed,
    ClosedElsewhere,
    Crowded,
    Factory,
    Given,
    Held,
    Maybe,
    Optional,
    Overridden,
    Pool,
    Public,
    Shaped,
    StoredOutside,
    Swapped,
    Unioned,
)


def tp_method_constructs(f: Factory) -> None:
    f.make().save()


def tp_attribute_of_self(h: Held) -> str:
    return h.db().query()


async def tp_awaited_getter(p: Pool) -> str:
    db = await p.get_db()
    return await db.fetch()


def tn_stored_outside_the_class(s: StoredOutside) -> str:
    return s.db().query()


def tn_another_class(s: Swapped) -> str:
    return s.db().query()


def tn_none(c: Closed) -> str:
    return c.db().query()


def tn_unknown(g: Given) -> str:
    return g.db().query()


def tn_overridden(o: Overridden) -> None:
    o.make().save()


def tn_annotated_optional(o: Optional) -> str:
    return o.db().query()


def tn_maybe_none(m: Maybe) -> str:
    return m.db().query()


def tn_written_through_an_untyped_receiver(p: Public) -> str:
    return p.get().query()


def tn_subclass_of_another_file(b: Base) -> str:
    return b.db().query()


def tn_none_from_another_file(c: ClosedElsewhere) -> str:
    return c.db().query()


def tn_annotated_union_with_none(u: Unioned) -> str:
    return u.db().query()


def tn_overridden_by_a_property(s: Shaped) -> None:
    s.make().save()


def tn_past_the_override_limit(c: Crowded) -> None:
    c.make().save()
"#;

/// A method without a return annotation gives a call on an instance the class every definition
/// the call may run returns (`TypeCtx::dispatched_return`): a construction, or an attribute of
/// `self` that only the class's own methods bind, to one class (`ProjectDecls::held_attr`), also
/// awaited of an `async def` getter and with a subclass in another file binding the same class.
/// A store outside the class (also of `None` alone from another file), of another class, of
/// `None`, or of an unknown value, an override
/// returning another class or by a property, more overrides than `CHA_CANDIDATE_LIMIT`, an
/// annotation allowing `None`, a store through a receiver of no known class, and a subclass in
/// another file binding another class leave the call unresolved.
#[test]
fn a_method_gives_the_class_every_definition_the_call_may_run_returns() {
    let p = Project::new("return_facts_methods");
    p.write("pyproject.toml", METHOD_BOUNDARY)
        .write("pkg/__init__.py", "")
        .write("pkg/models.py", METHOD_MODELS)
        .write("pkg/services.py", SERVICES)
        .write("pkg/subclasses.py", SUBCLASSES)
        .write("pkg/wiring.py", WIRING)
        .write("pkg/callers.py", METHOD_CALLERS);
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut reports: Vec<(u64, String)> = diags
        .iter()
        .map(|d| {
            assert_eq!(d["path"], "pkg/callers.py", "{d}");
            (
                d["line"].as_u64().unwrap(),
                d["message"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    reports.sort();
    let reaches = |line: u64, f: &str, via: &str| {
        (
            line,
            format!("`{f}` reaches blocking_fs ({via}), which effect boundary `fs` forbids"),
        )
    };
    assert_eq!(
        reports,
        [
            reaches(
                22,
                "tp_method_constructs",
                "`pathlib.Path.write_text` via `pkg.models.Store.save`"
            ),
            reaches(
                26,
                "tp_attribute_of_self",
                "`pathlib.Path.read_text` via `pkg.models.Database.query`"
            ),
            reaches(
                31,
                "tp_awaited_getter",
                "`pathlib.Path.read_text` via `pkg.models.Database.fetch`"
            ),
        ]
    );
}
