//! Which functions a call runs, end to end through the real binary: methods found along
//! Python's MRO (C3 linearization), constructing a named class (its own `__init__`, not a
//! subclass's) and calling a method through it, class methods called through their class,
//! calling an instance (`__call__`), constructions through a module, and the targets of `with`
//! statements.
//!
//! Every fixture marks the lines that must be reported with the rule code in a comment
//! (`# EFF101`); every other line must not be.

mod common;

use common::Project;
use serde_json::Value;

const CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
extend-select = ["MUT108"]

[[tool.heki.write-owners]]
name = "model"
targets = ["Model.items"]
"#;

/// `(path, line, rule)` of every diagnostic of `rules`.
fn reported(p: &Project, rules: &[&str]) -> Vec<(String, u64, String)> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<(String, u64, String)> = diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
            )
        })
        .filter(|(_, _, r)| rules.contains(&r.as_str()))
        .collect();
    out.sort();
    out
}

/// The `(path, line, rule)` marks of `files`: a line ending in `# <RULE>` for each of `rules`.
fn marked(files: &[(&str, &str)], rules: &[&str]) -> Vec<(String, u64, String)> {
    let mut out = Vec::new();
    for (path, text) in files {
        for (i, line) in text.lines().enumerate() {
            for rule in rules {
                if line.trim_end().ends_with(&format!("# {rule}")) {
                    out.push(((*path).to_owned(), i as u64 + 1, (*rule).to_owned()));
                }
            }
        }
    }
    out.sort();
    out
}

fn check_marks(name: &str, files: &[(&str, &str)], rules: &[&str]) {
    let p = Project::new(name);
    p.write("pyproject.toml", CONFIG);
    p.write("pkg/__init__.py", "");
    for (path, text) in files {
        p.write(path, text);
    }
    assert_eq!(reported(&p, rules), marked(files, rules));
}

/// `Leaf(Left, Right)` finds `Right.read` before `Root.read`, which `Left` inherits.
const DIAMOND: &str = r#"import time


class Root:
    def read(self):
        return 1


class Left(Root):
    pass


class Right(Root):
    def read(self):
        time.sleep(1)


class Leaf(Left, Right):
    pass


class Plain(Left):
    pass


async def leaf(x: Leaf):
    x.read()  # EFF101


async def plain(x: Plain):
    x.read()
"#;

#[test]
fn methods_are_found_along_the_c3_linearization() {
    check_marks("mro_c3", &[("pkg/diamond.py", DIAMOND)], &["EFF101"]);
}

/// A named class constructs itself and finds its own methods; `cls` in a class method may be
/// any subclass.
const NAMED: &str = r#"import time


class Base:
    def read(self):
        return 1

    @classmethod
    def make(cls):
        return cls()


class Slow(Base):
    def __init__(self):
        time.sleep(1)

    def read(self):
        time.sleep(1)


async def construct_base():
    Base()


async def construct_slow():
    Slow()  # EFF101


async def through_base(obj):
    Base.read(obj)


async def through_slow(obj):
    Slow.read(obj)  # EFF101


async def through_cls():
    Base.make()  # EFF101
"#;

#[test]
fn a_named_class_constructs_itself_and_finds_its_own_methods() {
    check_marks("named_class", &[("pkg/named.py", NAMED)], &["EFF101"]);
}

/// A class method called through its class binds the class to `cls`: the first argument is
/// its second parameter. A static method and a plain function called through the class bind
/// none.
const CLASS_CALLS: &str = r#"from pkg.model import Model


class Store:
    @classmethod
    def drain(cls, xs):
        xs.clear()

    @classmethod
    def drain_through(cls, xs):
        cls.drain(xs)

    @classmethod
    def keep(cls, xs, ys):
        ys.clear()

    @staticmethod
    def wipe(xs):
        xs.clear()

    def own(self, xs):
        xs.clear()


def through_class(m: Model, other: list):
    Store.drain(m.items)  # MUT201
    Store.drain_through(m.items)  # MUT201
    Store.keep(m.items, other)
    Store.wipe(m.items)  # MUT201
    Store.own(Store(), m.items)  # MUT201
    Store.own(m.items, other)
"#;

const MODEL: &str = r#"class Model:
    def __init__(self):
        self.items = []
"#;

#[test]
fn class_methods_called_through_their_class_bind_the_class() {
    check_marks(
        "class_calls",
        &[("pkg/model.py", MODEL), ("pkg/store.py", CLASS_CALLS)],
        &["MUT201"],
    );
}

/// Calling an instance runs its `__call__` with the instance as `self`, and returns what that
/// returns; constructing a class returns the new instance.
const CALLABLE: &str = r#"import functools


@functools.cache
def shared():
    return []


class Provider:
    def __call__(self):
        return shared()


class Holder(Provider):
    def __init__(self):
        self.items = []


def via(p: Provider):
    return p()


def use(p: Provider):
    via(Provider()).clear()  # MUT202
    p().clear()  # MUT202
    Holder().items.clear()
"#;

#[test]
fn calling_an_instance_runs_its_call_method_on_it() {
    check_marks("callable", &[("pkg/callable.py", CALLABLE)], &["MUT202"]);
}

/// Constructing a frozen dataclass through its module makes a new object, as importing it does.
const RECORDS: &str = r#"from dataclasses import dataclass


@dataclass(frozen=True)
class Record:
    items: list
"#;

const USES: &str = r#"from pkg import records
from pkg.records import Record


def through_module():
    r = records.Record([])
    r.items.append(1)
    return r


def imported():
    r = Record([])
    r.items.append(1)
    return r


def given(r: records.Record):
    r.items.append(1)  # MUT108
"#;

#[test]
fn constructing_through_a_module_makes_a_new_object() {
    check_marks(
        "qualified_construction",
        &[("pkg/records.py", RECORDS), ("pkg/uses.py", USES)],
        &["MUT108"],
    );
}

/// The target of `with m as r` is what `m.__enter__()` returns (`await m.__aenter__()` for
/// `async with`): the declared return type, else the manager itself.
const MANAGERS: &str = r#"import time


class Resource:
    def read(self):
        time.sleep(1)


class Manager:
    def __enter__(self) -> Resource:
        return Resource()

    def __exit__(self, *exc):
        return None

    def read(self):
        return None


class AsyncManager:
    async def __aenter__(self) -> Resource:
        return Resource()

    async def __aexit__(self, *exc):
        return None

    def read(self):
        return None


class Itself:
    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return None

    def read(self):
        time.sleep(1)


async def entered():
    with Manager() as r:
        r.read()  # EFF101


async def entered_async():
    async with AsyncManager() as r:
        r.read()  # EFF101


async def manager_itself(m: Manager):
    m.read()
    with Itself() as i:
        i.read()  # EFF101
"#;

#[test]
fn a_with_target_is_what_the_manager_enters() {
    check_marks(
        "with_targets",
        &[("pkg/managers.py", MANAGERS)],
        &["EFF101"],
    );
}
