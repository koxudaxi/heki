//! Which functions a call runs, end to end through the real binary: methods found along
//! Python's MRO (C3 linearization), constructing a named class (its own `__init__`, not a
//! subclass's) and calling a method through it, class methods called through their class,
//! calling an instance (`__call__`), constructions through a module, the targets of `with`
//! statements, module-level names bound twice, the parameters `*` arguments bind, project
//! methods named like container mutators, augmented assignments, and what calling and awaiting
//! an `async def` give.
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
targets = ["Model.items", "Holder.stats", "Holder.tally"]
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

/// A module-level name is what its module binds last: a definition after an import of the same
/// name shadows it (also for the modules importing the name), and an import after a definition
/// shadows the definition. A name bound on different paths keeps its import.
const SHADOWS: &str = r#"import time
from time import sleep

import requests
from pkg.slow import helper


def sleep(delay):
    pass


def helper():
    return None


def fetch(url):
    return None


from requests import get as fetch


class Session:
    def __init__(self):
        time.sleep(1)


from requests import Session

try:
    from requests import post
except ImportError:
    def post(url):
        return None


async def local(url):
    sleep(1)
    helper()
    fetch(url)  # EFF101
"#;

const SLOW: &str = r#"import time


def helper():
    time.sleep(1)
"#;

const SHADOW_USERS: &str = r#"from pkg.shadows import Session, fetch, helper, post, sleep


async def handler(url):
    sleep(1)
    helper()
    fetch(url)  # EFF101
    post(url)  # EFF101
    Session()
"#;

#[test]
fn a_module_name_is_what_its_module_binds_last() {
    check_marks(
        "shadowed_imports",
        &[
            ("pkg/shadows.py", SHADOWS),
            ("pkg/slow.py", SLOW),
            ("pkg/users.py", SHADOW_USERS),
        ],
        &["EFF101"],
    );
}

/// An argument unpacked with `*` binds the items of a display of known length one by one; past
/// one of unknown length (also one unpacked inside the display), the parameter an argument binds
/// is not known.
const UNPACKED: &str = r#"from pkg.model import Model


def fill(ignored, xs):
    xs.append(1)


def empty_pack(m: Model):
    fill(*(), None, m.items)  # MUT201


def one_item_pack(m: Model):
    fill(*(None,), m.items)  # MUT201


def whole_pack(m: Model):
    fill(*[None, m.items])  # MUT201


def first_slot(m: Model, rest):
    fill(m.items, *rest)


def nested_pack(m: Model, rest):
    fill(*(None, *rest), m.items)


def plain(m: Model):
    fill(m.items, None)
"#;

#[test]
fn unpacked_arguments_bind_the_items_of_known_displays() {
    check_marks(
        "unpacked_arguments",
        &[("pkg/model.py", MODEL), ("pkg/unpacked.py", UNPACKED)],
        &["MUT201"],
    );
}

/// A method named like a container mutator changes its receiver in place only when the
/// receiver's class does not define it: a project method's own summary decides.
const HOLDERS: &str = r#"class Stats:
    def clear(self):
        return None


class Tally(list):
    pass


class Counter:
    def __init__(self):
        self.seen = []

    def clear(self):
        self.seen.clear()


class Holder:
    def __init__(self):
        self.stats = Stats()
        self.tally = Tally()
        self.counter = Counter()


def reset(h: Holder):
    h.stats.clear()
    h.tally.clear()  # MUT201
    h.counter.clear()
"#;

#[test]
fn a_project_method_named_like_a_container_mutator_decides_itself() {
    check_marks(
        "named_mutators",
        &[("pkg/holders.py", HOLDERS)],
        &["MUT201"],
    );
}

/// An augmented assignment changes a built-in container in place (`items += ys` extends the list
/// a local name holds); on a copy or an immutable value it binds a new object.
const AUGMENTED: &str = r#"from pkg.model import Model


def extend_alias(m: Model):
    items = m.items
    items += [1]  # MUT201


def through_attribute(m: Model):
    m.items += [1]  # MUT201


def copied(m: Model):
    items = list(m.items)
    items += [1]


def counted(m: Model):
    n = len(m.items)
    n += 1
    return n
"#;

#[test]
fn augmented_assignments_change_containers_in_place() {
    check_marks(
        "augmented_assignments",
        &[("pkg/model.py", MODEL), ("pkg/augmented.py", AUGMENTED)],
        &["MUT201"],
    );
}

/// A mixin's method calling a method on its own `self` runs the implementation of the class
/// the receiver is: on a receiver of known class, only the implementations of classes related to
/// it count (an async client sharing the mixin with a sync one runs the async one).
const MIXINS: &str = r#"import time


class Commands:
    def masters(self):
        return self.execute_command("MASTERS")

    def relay(self):
        return self.masters()


class SyncClient(Commands):
    def execute_command(self, *args):
        time.sleep(1)


class SyncPipeline(SyncClient):
    pass


class AsyncClient(Commands):
    async def execute_command(self, *args):
        return args


class AsyncCommands(Commands):
    async def amasters(self):
        return self.masters()


async def on_async(c: AsyncClient):
    await c.masters()
    await c.relay()


async def on_sync(c: SyncClient):
    c.masters()  # EFF101
    c.relay()  # EFF101


async def on_sync_subclass(c: SyncPipeline):
    c.masters()  # EFF101


async def on_any(c: Commands):
    c.masters()  # EFF101


class Sentinel:
    def __init__(self):
        self.clients = [AsyncClient()]

    async def each(self):
        for c in self.clients:
            await c.masters()
"#;

#[test]
fn a_mixin_method_runs_the_implementations_of_its_receivers_class() {
    check_marks("mixins", &[("pkg/mixins.py", MIXINS)], &["EFF101"]);
}

/// An effect a method reaches through a subclass's implementation on its own `self` and also
/// through a plain call runs whatever `self` is; of two chains through one implementation, the
/// shorter one is kept.
const HOOKS: &str = r#"import time


def pause():
    time.sleep(1)


class Base:
    def run(self):
        self.flush()
        pause()

    def run_short_first(self):
        self.flush()
        self.slow_path()

    def run_long_first(self):
        self.slow_path()
        self.flush()

    def slow_path(self):
        self.flush()


class FileSink(Base):
    def flush(self):
        time.sleep(1)


class Quiet(Base):
    def flush(self):
        pass


async def on_quiet(q: Quiet):
    q.run()  # EFF101
    q.run_short_first()
    q.run_long_first()


async def on_file(f: FileSink):
    f.run_short_first()  # EFF101
    f.run_long_first()  # EFF101
"#;

#[test]
fn an_effect_through_self_and_a_plain_call_runs_for_every_receiver() {
    check_marks("self_hooks", &[("pkg/hooks.py", HOOKS)], &["EFF101"]);
    let p = Project::new("self_hooks_chains");
    p.write("pkg/__init__.py", "").write("pkg/hooks.py", HOOKS);
    let (_, stdout, _) = p.check(&["--select", "EFF101", "--output-format", "json"]);
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    // `(line of the report, line of the hop into `FileSink.flush`)`.
    let hops: Vec<(u64, u64)> = diags
        .iter()
        .filter(|d| d["qualname"] == "on_file")
        .map(|d| {
            (
                d["line"].as_u64().unwrap(),
                d["related"][1]["line"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(hops, [(42, 14), (43, 19)], "{diags:#?}");
}

/// A class whose bases C3 cannot linearize keeps the depth-first, left-to-right order: a base
/// order its subclass contradicts (Python rejects the class statement, as the test expects),
/// and bases that name the class itself (a class extending an imported class of its own name
/// with a mixin).
const UNORDERED: &str = r#"import time

import pytest

from pkg.parsers import Parser


class Mixin:
    def extra(self):
        time.sleep(1)


class Parser(Parser, Mixin):
    pass


async def on_parser(p: Parser):
    p.extra()  # EFF101


class X:
    def read(self):
        time.sleep(1)


class Y:
    def read(self):
        pass


class A(X, Y):
    pass


class B(Y, X):
    pass


def test_rejects_an_inconsistent_order():
    with pytest.raises(TypeError):

        class Both(A, B):
            async def run(self):
                self.read()  # EFF101
"#;

#[test]
fn bases_c3_cannot_linearize_keep_the_depth_first_order() {
    check_marks(
        "unordered_bases",
        &[
            (
                "pkg/parsers.py",
                "class Parser:\n    def parse(self):\n        return None\n",
            ),
            ("pkg/unordered.py", UNORDERED),
        ],
        &["EFF101"],
    );
}

/// A class method's `cls` stays any subclass of the class also where one path rebinds it to the
/// class itself: constructing it runs any subclass's `__init__`. The class's own name constructs
/// exactly that class.
const CLASS_FACTORIES: &str = r#"import time


class Model:
    def __init__(self):
        pass

    @classmethod
    def build(cls, base):
        if base:
            cls = Model
        return cls()

    @classmethod
    def plain(cls):
        return Model()


class Slow(Model):
    def __init__(self):
        time.sleep(1)


async def make():
    Slow.build(False)  # EFF101
    Slow.plain()
"#;

#[test]
fn a_class_method_constructs_any_subclass_through_cls() {
    check_marks(
        "class_factories",
        &[("pkg/factories.py", CLASS_FACTORIES)],
        &["EFF101"],
    );
}

/// The call chain of a report runs for the receiver's class: an effect a hook reaches only in a
/// subclass's override is not the path shown for a receiver of another subclass.
#[test]
fn a_chain_runs_for_the_receivers_class() {
    let p = Project::new("receiver_chains");
    p.write("pyproject.toml", CONFIG);
    p.write("pkg/__init__.py", "");
    p.write(
        "pkg/hooks.py",
        r#"import time


def pause():
    time.sleep(1)


class Base:
    def run(self):
        self.flush()
        pause()


class FileSink(Base):
    def flush(self):
        time.sleep(1)


class Quiet(Base):
    def flush(self):
        pass


async def on_quiet(q: Quiet):
    q.run()


async def on_file(f: FileSink):
    f.run()
"#,
    );
    let (_, stdout, _) = p.check(&["--output-format", "json"]);
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let chain = |line: u64| -> Vec<String> {
        let d = diags
            .iter()
            .find(|d| d["line"] == line && d["rule_code"] == "EFF101")
            .unwrap_or_else(|| panic!("no EFF101 at {line}: {diags:#?}"));
        d["related"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["message"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(
        chain(25),
        vec![
            "`on_quiet` calls `pkg.hooks.Base.run`",
            "`Base.run` calls `pkg.hooks.pause`",
            "`pause` calls `time.sleep`",
        ]
    );
    assert_eq!(chain(29).len(), 3);
}

/// A class statement evaluates its bases before it binds its own name: `class Parser(Parser)`
/// derives from the `Parser` the module imported, whose methods it inherits. A base the module
/// rebound before the class statement is the new binding.
const SHADOWING_BASES: &str = r#"import time

from pkg.parser_base import Parser


class Mixin:
    def extra(self):
        time.sleep(1)


class Parser(Parser, Mixin):
    def own(self):
        return 2


async def through_both(p: Parser):
    p.extra()  # EFF101
    p.parse()  # EFF101
"#;

/// A base reached through a module the class's own name shadows.
const SHADOWING_MODULE: &str = r#"from pkg import parser_base


class parser_base(parser_base.Parser):
    pass


async def through_module(p: parser_base):
    p.parse()  # EFF101
"#;

/// Classes deriving from each other (Python rejects the second statement): the MRO walks
/// each class once.
const CYCLE: &str = r#"import time


class Mixin:
    def read(self):
        time.sleep(1)


class A(B, Mixin):
    pass


class B(A):
    pass


async def on_cycle(a: A):
    a.read()  # EFF101
"#;

const REBOUND_FIRST: &str = r#"from pkg.parser_base import Parser


class Parser:
    def parse(self):
        return 1


class Child(Parser):
    pass


async def through_local(c: Child):
    c.parse()
"#;

const PARSER_BASE: &str = r#"import time


class Parser:
    def parse(self):
        time.sleep(1)
"#;

#[test]
fn a_class_derives_from_the_import_its_own_name_shadows() {
    check_marks(
        "shadowing_bases",
        &[
            ("pkg/parser_base.py", PARSER_BASE),
            ("pkg/shadowing.py", SHADOWING_BASES),
            ("pkg/shadowing_module.py", SHADOWING_MODULE),
            ("pkg/rebound.py", REBOUND_FIRST),
            ("pkg/cycle.py", CYCLE),
        ],
        &["EFF101"],
    );
}

/// Calling an `async def` gives a coroutine, and awaiting it gives what the function's
/// annotation says it returns: the awaited store's method runs, a coroutine has no `save`, and
/// awaiting either of two coroutines gives either result.
const AWAITED: &str = r#"import time


class Store:
    def save(self):
        time.sleep(1)


async def open_store() -> Store:
    return Store()


class Pool:
    async def acquire(self) -> Store:
        return Store()


class Cache:
    def save(self):
        time.sleep(1)


async def open_cache() -> Cache:
    return Cache()


async def awaited(pool: Pool):
    store = await open_store()
    store.save()  # EFF101
    other = await pool.acquire()
    other.save()  # EFF101
    pending = open_store()
    ready = await pending
    ready.save()  # EFF101


async def either(flag):
    pending = open_store() if flag else open_cache()
    obj = await pending
    obj.save()  # EFF101


async def never_awaited(pool: Pool):
    store = open_store()
    store.save()
    other = pool.acquire()
    other.save()
"#;

/// A project coroutine and a catalogued awaitable (typed by what awaiting it gives) merged
/// before the `await`: the awaited connection runs a query per item either way.
const MIXED_AWAITED: &str = r#"import psycopg


async def connect() -> psycopg.AsyncConnection:
    return await psycopg.AsyncConnection.connect("dbname=t")


async def load(ids, flag):
    pending = connect() if flag else psycopg.AsyncConnection.connect("dbname=t")
    conn = await pending
    for i in ids:
        await conn.execute("SELECT 1")  # EFF103


async def each_awaited(ids, flag):
    conn = await connect() if flag else await psycopg.AsyncConnection.connect("dbname=t")
    for i in ids:
        await conn.execute("SELECT 1")  # EFF103
"#;

#[test]
fn a_call_of_an_async_def_is_a_coroutine_of_what_awaiting_it_gives() {
    check_marks(
        "awaited",
        &[
            ("pkg/awaited.py", AWAITED),
            ("pkg/mixed_awaited.py", MIXED_AWAITED),
        ],
        &["EFF101", "EFF103"],
    );
}
