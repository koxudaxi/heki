//! `EFF109` (`lock-reacquired-while-held`) end to end: a non-reentrant lock taken again while it
//! is held, by the holder itself or by the calls it makes on its thread or task (helpers on
//! `self`, awaited coroutines, a module global through a chain of helpers and back into the
//! holder, a property getter, a generator iterated there, a closure, a lock passed on as an
//! argument, another module), the shapes that hold no lock or take it elsewhere, the
//! re-entrance guards (also in a function on the way to the lock) and constant arguments that
//! keep a helper from taking it, a name bound to another lock before it is taken again, the
//! catalog entries a configuration adds, the opt-in selection, and `heki rule`.

mod common;

use common::Project;
use serde_json::Value;

fn diagnostics(p: &Project, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["--output-format", "json"];
    all.extend_from_slice(args);
    let (code, stdout, stderr) = p.check(&all);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

/// `(path, line, qualname, target)` of every `EFF109` diagnostic.
fn found(diags: &[Value]) -> Vec<(String, u64, String, String)> {
    diags
        .iter()
        .filter(|d| d["rule_code"] == "EFF109")
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["qualname"].as_str().unwrap().to_owned(),
                d["target"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn d(path: &str, line: u64, qualname: &str, target: &str) -> (String, u64, String, String) {
    (path.into(), line, qualname.into(), target.into())
}

fn at<'v>(diags: &'v [Value], path: &str, line: u64) -> &'v Value {
    diags
        .iter()
        .find(|d| d["path"] == path && d["line"] == line)
        .unwrap_or_else(|| panic!("no diagnostic at {path}:{line}: {diags:#?}"))
}

/// `(path, line, message)` of a diagnostic's related locations.
fn related(diag: &Value) -> Vec<(String, u64, String)> {
    diag["related"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["path"].as_str().unwrap().to_owned(),
                r["line"].as_u64().unwrap(),
                r["message"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

const SELECT: &[&str] = &["--extend-select", "EFF109"];

/// A helper taking the lock its caller holds: on `self` (also on an error path and two helpers
/// deep), awaited in a coroutine, as a property, as a generator, as the key a builtin calls,
/// and the lock passed on.
const REGISTRY: &str = r#"import asyncio
import threading


class Registry:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._items: dict[str, int] = {}

    def size(self) -> int:
        with self._lock:
            return len(self._items)

    def add(self, key: str) -> int:
        with self._lock:
            self._items[key] = 1
            return self.size()

    def stop(self, silent: bool = False) -> None:
        with self._lock:
            self._items.clear()

    def write(self, key: str) -> None:
        with self._lock:
            try:
                self._items[key] += 1
            except KeyError:
                self.stop(silent=True)

    def _record(self, key: str, count: int) -> None:
        if count > 3:
            self.jail(key)

    def jail(self, key: str) -> None:
        with self._lock:
            self._items.pop(key, None)

    def check(self, key: str) -> None:
        with self._lock:
            self._record(key, 5)

    @property
    def total(self) -> int:
        with self._lock:
            return sum(self._items.values())

    def report(self) -> int:
        with self._lock:
            return self.total

    def keys(self):
        with self._lock:
            yield from list(self._items)

    def dump(self) -> list[str]:
        with self._lock:
            return [k for k in self.keys()]

    def clear_with(self, lock: threading.Lock) -> None:
        with lock:
            self._items.clear()

    def reset(self) -> None:
        with self._lock:
            clear_under(self._lock)


def clear_under(lock: threading.Lock) -> None:
    with lock:
        pass


class AsyncRegistry:
    def __init__(self) -> None:
        self._lock = asyncio.Lock()
        self._items: dict[str, int] = {}

    async def update_status(self, key: str) -> bool:
        async with self._lock:
            self._items[key] = 2
            return True

    async def heartbeat(self, key: str) -> bool:
        async with self._lock:
            if self._items.get(key) == 0:
                await self.update_status(key)
            return True


class Ranked:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._scores: dict[str, int] = {}

    def _rank(self, key: str) -> int:
        with self._lock:
            return self._scores.get(key, 0)

    def order(self, keys: list[str]) -> list[str]:
        with self._lock:
            return sorted(keys, key=self._rank)
"#;

#[test]
fn eff109_reports_a_helper_taking_the_lock_its_caller_holds() {
    let p = Project::new("eff109_helpers");
    p.write("pkg/__init__.py", "")
        .write("pkg/registry.py", REGISTRY);
    let diags = diagnostics(&p, SELECT);
    assert_eq!(
        found(&diags),
        vec![
            d(
                "pkg/registry.py",
                17,
                "Registry.add",
                "self._lock->pkg.registry.Registry.size"
            ),
            d(
                "pkg/registry.py",
                28,
                "Registry.write",
                "self._lock->pkg.registry.Registry.stop"
            ),
            d(
                "pkg/registry.py",
                40,
                "Registry.check",
                "self._lock->pkg.registry.Registry._record"
            ),
            d(
                "pkg/registry.py",
                49,
                "Registry.report",
                "self._lock->pkg.registry.Registry.total"
            ),
            d(
                "pkg/registry.py",
                57,
                "Registry.dump",
                "self._lock->pkg.registry.Registry.keys"
            ),
            d(
                "pkg/registry.py",
                65,
                "Registry.reset",
                "self._lock->pkg.registry.clear_under"
            ),
            d(
                "pkg/registry.py",
                86,
                "AsyncRegistry.heartbeat",
                "self._lock->pkg.registry.AsyncRegistry.update_status"
            ),
            d(
                "pkg/registry.py",
                101,
                "Ranked.order",
                "self._lock->pkg.registry.Ranked._rank"
            ),
        ]
    );
    let add = at(&diags, "pkg/registry.py", 17);
    assert_eq!(
        add["message"],
        "`Registry.add` calls `pkg.registry.Registry.size`, which takes `self._lock` while \
         `Registry.add` holds it (taken at line 15); `threading.Lock` is not reentrant, so this \
         waits forever"
    );
    assert_eq!(
        add["help"],
        "call a variant that expects the lock to be held, or make the call after releasing the \
         lock"
    );
    assert_eq!(add["severity"], "error");
    assert!(add["fix"].is_null());
    let check = at(&diags, "pkg/registry.py", 40);
    assert_eq!(
        related(check),
        vec![
            (
                "pkg/registry.py".into(),
                39,
                "`Registry.check` takes `self._lock` here".into()
            ),
            (
                "pkg/registry.py".into(),
                40,
                "`Registry.check` calls `pkg.registry.Registry._record`".into()
            ),
            (
                "pkg/registry.py".into(),
                32,
                "`Registry._record` calls `pkg.registry.Registry.jail`".into()
            ),
            (
                "pkg/registry.py".into(),
                35,
                "`Registry.jail` takes the lock again here".into()
            ),
        ]
    );
    let heartbeat = at(&diags, "pkg/registry.py", 86);
    assert!(
        heartbeat["message"].as_str().unwrap().ends_with(
            "(taken at line 84); `asyncio.Lock` is not reentrant, so this waits forever"
        ),
        "{heartbeat:#?}"
    );
}

/// A module-global lock: taken with `acquire()` / `release()`, re-entered through a chain of
/// helpers back into the function holding it, and from another module that imports it.
const MAPPER: &str = r#"import threading

_COMPILE_MUTEX = threading.Lock()


class Mapper:
    def __init__(self, props: "list[Property]") -> None:
        self.props = props

    def compile(self) -> None:
        _COMPILE_MUTEX.acquire()
        try:
            for prop in self.props:
                self._initialize(prop)
        finally:
            _COMPILE_MUTEX.release()

    def _initialize(self, prop: "Property") -> None:
        prop.mapper.compile()


class Property:
    def __init__(self, mapper: Mapper) -> None:
        self.mapper = mapper


def configure_all(mappers):
    _COMPILE_MUTEX.acquire()
    _COMPILE_MUTEX.release()
    for m in mappers:
        m.compile()
"#;

const MAPPER_USER: &str = r#"from pkg.mapper import _COMPILE_MUTEX, Mapper


def rebuild(m: Mapper) -> None:
    with _COMPILE_MUTEX:
        m.compile()
"#;

#[test]
fn eff109_follows_a_global_lock_through_helpers_and_modules() {
    let p = Project::new("eff109_global");
    p.write("pkg/__init__.py", "")
        .write("pkg/mapper.py", MAPPER)
        .write("pkg/user.py", MAPPER_USER);
    let diags = diagnostics(&p, SELECT);
    assert_eq!(
        found(&diags),
        vec![
            d(
                "pkg/mapper.py",
                14,
                "Mapper.compile",
                "pkg.mapper._COMPILE_MUTEX->pkg.mapper.Mapper._initialize"
            ),
            d(
                "pkg/user.py",
                6,
                "rebuild",
                "pkg.mapper._COMPILE_MUTEX->pkg.mapper.Mapper.compile"
            ),
        ]
    );
}

/// The direct cases: a `with` inside a `with` over the same lock, an `acquire()` while a `with`
/// holds it, a closure the enclosing function calls while it holds their shared lock, and a
/// recursive call.
const DIRECT: &str = r#"import threading


class Plain:
    def __init__(self) -> None:
        self._lock = threading.Lock()

    def nested(self) -> None:
        with self._lock:
            with self._lock:
                pass

    def again(self) -> None:
        with self._lock:
            self._lock.acquire()

    def walk(self, depth: int) -> None:
        with self._lock:
            if depth:
                self.walk(depth - 1)


def make():
    lock = threading.Lock()

    def inner():
        with lock:
            return 1

    with lock:
        inner()
    return inner
"#;

#[test]
fn eff109_reports_direct_retakes_recursion_and_closures() {
    let p = Project::new("eff109_direct");
    p.write("direct.py", DIRECT);
    assert_eq!(
        found(&diagnostics(&p, SELECT)),
        vec![
            d("direct.py", 10, "Plain.nested", "self._lock->again"),
            d("direct.py", 15, "Plain.again", "self._lock->again"),
            d(
                "direct.py",
                20,
                "Plain.walk",
                "self._lock->direct.Plain.walk"
            ),
            d("direct.py", 31, "make", "lock->direct.make.<locals>.inner"),
        ]
    );
}

/// None of these takes a held lock again: reentrant and semaphore locks, calls after the lock is
/// released, coroutines and generators created but not run there, functions run on another
/// thread, locks of other objects, non-blocking or timed acquisitions, and closures only made.
const NEGATIVES: &str = r#"import asyncio
import threading
from concurrent.futures import ThreadPoolExecutor


class Reentrant:
    def __init__(self) -> None:
        self._lock = threading.RLock()
        self._cond = threading.Condition()
        self._sem = threading.Semaphore(2)

    def size(self) -> int:
        with self._lock:
            return 1

    def add(self) -> int:
        with self._lock:
            return self.size()

    def wait_one(self) -> None:
        with self._cond:
            self.notify_one()

    def notify_one(self) -> None:
        with self._cond:
            self._cond.notify()

    def limited(self) -> None:
        with self._sem:
            self.limited_inner()

    def limited_inner(self) -> None:
        with self._sem:
            pass


class Plain:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._pool = ThreadPoolExecutor()

    def size(self) -> int:
        with self._lock:
            return 1

    def after(self) -> int:
        with self._lock:
            n = 1
        return n + self.size()

    def released(self) -> int:
        self._lock.acquire()
        self._lock.release()
        return self.size()

    def threaded(self) -> None:
        with self._lock:
            threading.Thread(target=self.size).start()
            self._pool.submit(self.size)

    async def offloaded(self) -> None:
        with self._lock:
            await asyncio.to_thread(self.size)

    def other(self, peer: "Plain") -> int:
        with self._lock:
            return peer.size()

    def tries(self) -> bool:
        with self._lock:
            return self.try_size()

    def try_size(self) -> bool:
        if self._lock.acquire(blocking=False):
            self._lock.release()
            return True
        return False

    def timed(self) -> bool:
        with self._lock:
            return self.timed_size()

    def timed_size(self) -> bool:
        got = self._lock.acquire(timeout=1)
        if got:
            self._lock.release()
        return got

    def later(self):
        with self._lock:
            return lambda: self.size()

    def make_gen(self):
        with self._lock:
            return self.gen()

    def gen(self):
        with self._lock:
            yield 1


class Tasks:
    def __init__(self) -> None:
        self._lock = asyncio.Lock()

    async def size(self) -> int:
        async with self._lock:
            return 1

    async def spawn(self) -> None:
        async with self._lock:
            asyncio.create_task(self.size())

    async def acquire_unawaited(self) -> None:
        async with self._lock:
            self.helper()

    def helper(self) -> None:
        self._lock.acquire()
"#;

#[test]
fn eff109_leaves_other_locks_released_locks_and_deferred_work() {
    let p = Project::new("eff109_negatives");
    p.write("neg.py", NEGATIVES);
    assert_eq!(found(&diagnostics(&p, SELECT)), vec![]);
}

/// A helper that would take the lock but does not on this call: a constant argument switches off
/// its branch, or it first tests a flag of the lock's owner that the holder's calls set (the code
/// tracks whether it holds the lock). A flag nothing on the holder's side writes is no such
/// guard.
const GUARDED: &str = r#"import asyncio


class Session:
    def __init__(self) -> None:
        self._send_lock = asyncio.Lock()
        self._start_lock = asyncio.Lock()
        self._start_task = None
        self.closed = False
        self.ready = False

    async def send(self, frame: bytes) -> None:
        pass

    async def emit(self, frame: bytes) -> None:
        async with self._send_lock:
            try:
                await self.send(frame)
            except OSError:
                await self.close_stream(notify=False)

    async def close_stream(self, notify: bool) -> None:
        if notify:
            await self.emit(b"close")

    async def write(self, frame: bytes) -> None:
        async with self._send_lock:
            try:
                await self.send(frame)
            except OSError:
                await self.shutdown()

    async def shutdown(self) -> None:
        self.closed = True
        await self.say_bye()

    async def say_bye(self) -> None:
        if self.closed:
            return
        async with self._send_lock:
            await self.send(b"bye")

    async def start(self) -> None:
        async with self._start_lock:
            await self._start_unlocked()

    def _in_own_start(self) -> bool:
        return self._start_task is asyncio.current_task()

    async def _start_unlocked(self) -> None:
        self._start_task = asyncio.current_task()
        try:
            await self.reset()
        finally:
            self._start_task = None

    async def reset(self) -> None:
        if self._in_own_start():
            raise RuntimeError("stopped while starting")
        async with self._start_lock:
            await self._start_unlocked()

    async def flush(self) -> None:
        async with self._send_lock:
            await self.when_ready()

    async def when_ready(self) -> None:
        if not self.ready:
            return
        async with self._send_lock:
            pass
"#;

#[test]
fn eff109_skips_helpers_a_guard_keeps_from_the_lock() {
    let p = Project::new("eff109_guarded");
    p.write("guarded.py", GUARDED);
    assert_eq!(
        found(&diagnostics(&p, SELECT)),
        vec![d(
            "guarded.py",
            65,
            "Session.flush",
            "self._send_lock->guarded.Session.when_ready"
        )]
    );
}

#[test]
fn eff109_takes_lock_classes_from_the_configuration() {
    let p = Project::new("eff109_config");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"EFF109\"]\n\n\
         [[tool.heki.ecosystem]]\nsymbol = \"vendor.sync.Mutex\"\nnon-reentrant-lock = true\n\n\
         [[tool.heki.ecosystem]]\nsymbol = \"threading.Lock\"\nnon-reentrant-lock = false\n",
    )
    .write(
        "app.py",
        r#"import threading

from vendor.sync import Mutex


class Store:
    def __init__(self) -> None:
        self._mutex = Mutex()
        self._lock = threading.Lock()

    def get(self) -> int:
        with self._mutex:
            return 1

    def put(self) -> int:
        with self._mutex:
            return self.get()

    def peek(self) -> int:
        with self._lock:
            return 1

    def poke(self) -> int:
        with self._lock:
            return self.peek()
"#,
    );
    assert_eq!(
        found(&diagnostics(&p, &[])),
        vec![d("app.py", 17, "Store.put", "self._mutex->app.Store.get")]
    );
}

#[test]
fn eff109_selection_and_rule_page() {
    let p = Project::new("eff109_selection");
    p.write("pkg/__init__.py", "")
        .write("pkg/registry.py", REGISTRY);
    assert!(found(&diagnostics(&p, &[])).is_empty());
    assert!(found(&diagnostics(&p, &["--select", "EFF"])).is_empty());
    let named = diagnostics(&p, &["--select", "lock-reacquired-while-held"]);
    assert_eq!(found(&named).len(), 8);
    assert_eq!(named[0]["rule_slug"], "lock-reacquired-while-held");
    let (code, doc, _) = p.heki(&["rule", "EFF109"]);
    assert_eq!(code, 0);
    assert!(
        doc.starts_with("# lock-reacquired-while-held (EFF109)\n\n"),
        "{doc}"
    );
    let sections = [
        "\n## What it does\n",
        "\n## Why is this bad?\n",
        "\n## Example\n",
        "\nUse instead:\n",
        "\n## Fix safety\nNo automatic fix.\n",
        "\n## Options\n",
    ];
    let positions: Vec<usize> = sections
        .iter()
        .map(|s| doc.find(s).unwrap_or_else(|| panic!("{s}: {doc}")))
        .collect();
    assert!(positions.is_sorted(), "{doc}");
}

/// Every way the two sides name one lock: the lock passed on as itself, a module attribute, a
/// variable two closures down, a closure made in a
/// `finally` body; acquisitions by `acquire(True)`, `acquire(blocking=True)`, and an awaited
/// `acquire()`; a class body a function runs, and a function defined twice; the shortest of two
/// chains to the lock.
const NAMES: &str = r#"import asyncio
import sys
import threading

from pkg import mapper

LOCK = threading.Lock()


def take_it(lock: threading.Lock) -> None:
    with lock:
        pass


def hold_it(lock: threading.Lock) -> None:
    with lock:
        take_it(lock)


def rebuild(m: mapper.Mapper) -> None:
    with mapper._COMPILE_MUTEX:
        m.compile()


def outer():
    lock = threading.Lock()

    def middle():
        def inner():
            with lock:
                return 1

        with lock:
            return inner()

    return middle


def in_finally():
    try:
        pass
    finally:
        lock = threading.Lock()

        def inner():
            with lock:
                return 1

        with lock:
            inner()


def deep() -> None:
    middle_step()


def middle_step() -> None:
    shallow()


def shallow() -> None:
    with LOCK:
        pass


def both() -> None:
    deep()
    shallow()


def hold_both() -> None:
    with LOCK:
        both()


def build():
    class Local:
        with LOCK:
            size = 1

    return Local


def hold_build() -> None:
    with LOCK:
        build()


if sys.version_info >= (3, 12):

    def twice() -> None:
        with LOCK:
            pass

else:

    def twice() -> None:
        with LOCK:
            pass


def hold_twice() -> None:
    with LOCK:
        twice()


def by_flag() -> None:
    LOCK.acquire(True)
    shallow()
    LOCK.release()
    LOCK.acquire(blocking=True)
    shallow()
    LOCK.release()


class Waiter:
    def __init__(self) -> None:
        self._lock = asyncio.Lock()

    async def step(self) -> None:
        async with self._lock:
            pass

    async def run(self) -> None:
        await self._lock.acquire()
        try:
            await self.step()
        finally:
            self._lock.release()
"#;

#[test]
fn eff109_names_one_lock_every_way() {
    let p = Project::new("eff109_names");
    p.write("pkg/__init__.py", "")
        .write("pkg/mapper.py", MAPPER)
        .write("pkg/names.py", NAMES);
    let diags = diagnostics(&p, SELECT);
    let names: Vec<_> = found(&diags)
        .into_iter()
        .filter(|f| f.0 == "pkg/names.py")
        .collect();
    assert_eq!(
        names,
        vec![
            d("pkg/names.py", 17, "hold_it", "lock->pkg.names.take_it"),
            d(
                "pkg/names.py",
                22,
                "rebuild",
                "pkg.mapper._COMPILE_MUTEX->pkg.mapper.Mapper.compile"
            ),
            d(
                "pkg/names.py",
                34,
                "outer.<locals>.middle",
                "lock->pkg.names.outer.<locals>.middle.<locals>.inner"
            ),
            d(
                "pkg/names.py",
                50,
                "in_finally",
                "lock->pkg.names.in_finally.<locals>.inner"
            ),
            d(
                "pkg/names.py",
                73,
                "hold_both",
                "pkg.names.LOCK->pkg.names.both"
            ),
            d(
                "pkg/names.py",
                86,
                "hold_build",
                "pkg.names.LOCK->pkg.names.build"
            ),
            d(
                "pkg/names.py",
                104,
                "hold_twice",
                "pkg.names.LOCK->pkg.names.twice"
            ),
            d(
                "pkg/names.py",
                109,
                "by_flag",
                "pkg.names.LOCK->pkg.names.shallow"
            ),
            d(
                "pkg/names.py",
                112,
                "by_flag",
                "pkg.names.LOCK->pkg.names.shallow"
            ),
            d(
                "pkg/names.py",
                127,
                "Waiter.run",
                "self._lock->pkg.names.Waiter.step"
            ),
        ]
    );
    // The shorter chain, through `shallow`, names the report.
    let held = at(&diags, "pkg/names.py", 73);
    let lines: Vec<u64> = related(held).iter().map(|r| r.1).collect();
    assert_eq!(lines, vec![72, 73, 68, 62], "{held:#?}");
}

/// None of these is the held lock taken again: a lock a call hands on below an attribute, a
/// local lock handed to a helper, a closure's variable bound to different things, a helper
/// testing more of the lock owner's state than heki keeps, a test (through a copy and a
/// comparison) of state the holder changes, and an acquisition only a dead branch reaches.
const UNNAMED: &str = r#"import threading


class Child:
    def __init__(self) -> None:
        self._lock = threading.Lock()


def take_child(obj: Child) -> None:
    with obj._lock:
        pass


def take_it(lock: threading.Lock) -> None:
    with lock:
        pass


class Parent:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self.child = Child()
        self.count = 0
        self.f0 = self.f1 = self.f2 = self.f3 = self.f4 = self.f5 = False
        self.f6 = self.f7 = self.f8 = self.f9 = self.f10 = self.f11 = False
        self.f12 = self.f13 = self.f14 = self.f15 = self.f16 = False

    def hold_child(self) -> None:
        with self.child._lock:
            take_child(self.child)

    def local(self) -> None:
        lock = threading.Lock()
        with lock:
            take_it(lock)

    def busy(self) -> None:
        if self.f0:
            return
        if self.f1:
            return
        if self.f2:
            return
        if self.f3:
            return
        if self.f4:
            return
        if self.f5:
            return
        if self.f6:
            return
        if self.f7:
            return
        if self.f8:
            return
        if self.f9:
            return
        if self.f10:
            return
        if self.f11:
            return
        if self.f12:
            return
        if self.f13:
            return
        if self.f14:
            return
        if self.f15:
            return
        if self.f16:
            return
        with self._lock:
            pass

    def hold_busy(self) -> None:
        with self._lock:
            self.busy()

    def bounded(self) -> None:
        full = self.count > 3
        if full:
            return
        with self._lock:
            pass

    def hold_bounded(self) -> None:
        with self._lock:
            self.count = 4
            self.bounded()

    def never(self) -> None:
        if False:
            with self._lock:
                pass
        with self.child._lock:
            pass

    def hold_never(self) -> None:
        with self._lock:
            self.never()
            if False:
                with self._lock:
                    pass


def rebound():
    lock = threading.RLock()
    lock = threading.Lock()

    def inner():
        with lock:
            return 1

    with lock:
        inner()
"#;

#[test]
fn eff109_leaves_locks_it_cannot_name_or_guarded_helpers() {
    let p = Project::new("eff109_unnamed");
    p.write("unnamed.py", UNNAMED);
    assert_eq!(found(&diagnostics(&p, SELECT)), vec![]);
}

/// Helpers that take the lock after branches the rule walks past: a test of a method it cannot
/// resolve, two tests of one value, an `if` with an acquisition on each side, a `try` before
/// the acquisition, and a closure's variable bound to a lock on each binding (`make_lock` has
/// no return annotation, and its one return is a `threading.Lock()` call); and, not reported, a
/// timed `acquire(True, 1)` and a `locked()` test.
const BRANCHES: &str = r#"import threading


def make_lock():
    return threading.Lock()


class Branchy:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self.done = False

    def probe(self) -> None:
        if self.missing():
            return
        done = self.done
        if done:
            return
        if not done:
            with self._lock:
                pass

    def either(self, flag: bool) -> None:
        if flag:
            with self._lock:
                pass
        else:
            with self._lock:
                pass

    def after_try(self) -> None:
        try:
            value = 1
        except ValueError:
            value = 0
        with self._lock:
            print(value)

    def timed(self) -> bool:
        if self._lock.locked():
            return False
        return self._lock.acquire(True, 1)

    def run(self) -> None:
        with self._lock:
            self.probe()
            self.either(True)
            self.after_try()
            self.timed()


def factory_cell():
    lock = make_lock()
    lock = threading.Lock()

    def inner():
        with lock:
            return 1

    with lock:
        inner()
"#;

#[test]
fn eff109_walks_past_branches_to_the_helper_acquisitions() {
    let p = Project::new("eff109_branches");
    p.write("branches.py", BRANCHES);
    assert_eq!(
        found(&diagnostics(&p, SELECT)),
        vec![
            d(
                "branches.py",
                46,
                "Branchy.run",
                "self._lock->branches.Branchy.probe"
            ),
            d(
                "branches.py",
                47,
                "Branchy.run",
                "self._lock->branches.Branchy.either"
            ),
            d(
                "branches.py",
                48,
                "Branchy.run",
                "self._lock->branches.Branchy.after_try"
            ),
            d(
                "branches.py",
                61,
                "factory_cell",
                "lock->branches.factory_cell.<locals>.inner"
            ),
        ]
    );
}

/// Code that takes a lock again on purpose, not reported: it gives the lock back first inside
/// the `with` (`release()`), the helper it calls tests a module global it sets first (a lazy
/// initialization under the lock), or another thread gives the lock back as a signal (some
/// function releases it without holding it). A tested global the holder does not set is no
/// such guard.
const ON_PURPOSE: &str = r#"import threading

_LIB = None
_LIB_LOCK = threading.Lock()
_READY = False


class LoadError(RuntimeError):
    def __init__(self, what: str) -> None:
        name = load().name if _LIB else what
        super().__init__(name)


def load():
    global _LIB
    if _LIB is not None:
        return _LIB
    with _LIB_LOCK:
        if _LIB is not None:
            return _LIB
        found = find()
        if found is None:
            raise LoadError("not found")
        _LIB = found
        return _LIB


def find():
    return None


def ready_load():
    if not _READY:
        return None
    with _LIB_LOCK:
        return 1


def hold_ready():
    with _LIB_LOCK:
        return ready_load()


class Queue:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._items: list[int] = []

    def wait_for_item(self) -> int:
        with self._lock:
            while not self._items:
                self._lock.release()
                self._lock.acquire()
            return self._items.pop()


class Solver:
    def __init__(self) -> None:
        self._started = threading.Lock()

    def _run(self) -> None:
        self._started.release()

    def start(self) -> threading.Thread:
        self._started.acquire()
        t = threading.Thread(target=self._run, daemon=True)
        t.start()
        self._started.acquire()
        self._started.release()
        return t
"#;

#[test]
fn eff109_leaves_locks_taken_again_on_purpose() {
    let p = Project::new("eff109_on_purpose");
    p.write("purpose.py", ON_PURPOSE);
    assert_eq!(
        found(&diagnostics(&p, SELECT)),
        vec![d(
            "purpose.py",
            41,
            "hold_ready",
            "purpose._LIB_LOCK->purpose.ready_load"
        )]
    );
}

/// A name bound to another object between the two acquisitions, so that the inner one takes
/// another lock, not reported: by the holder (an attribute of `self`, also through an alias or
/// `del`, an attribute of a module, a closure variable), by a function the holder calls first
/// (an attribute, a module global, `del` of one, a `nonlocal` variable), by the helper taking the
/// lock, or by a function on the way to it. Binding another attribute does not count.
const REBOUND_LOCKS: &str = r#"import threading

LOCK = threading.Lock()


def take() -> None:
    with LOCK:
        pass


def renew() -> None:
    global LOCK
    LOCK = threading.Lock()
"#;

const REBOUND: &str = r#"import threading

from pkg import locks
from pkg.locks import renew, take

_LOCK = threading.Lock()


def take_own() -> None:
    with _LOCK:
        pass


def drop_own() -> None:
    global _LOCK
    del _LOCK


def module_store() -> None:
    with locks.LOCK:
        locks.LOCK = threading.Lock()
        take()


def through_renew() -> None:
    with locks.LOCK:
        renew()
        take()


def through_del() -> None:
    with _LOCK:
        drop_own()
        take_own()


def still_held() -> None:
    with locks.LOCK:
        take()


class Pool:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._other = threading.Lock()

    def size(self) -> int:
        with self._lock:
            return 1

    def _reset(self) -> None:
        self._lock = threading.Lock()

    def _reset_other(self) -> None:
        self._other = threading.Lock()

    def fresh_size(self) -> int:
        self._lock = threading.Lock()
        with self._lock:
            return 1

    def reset_then_size(self) -> int:
        self._reset()
        return self.size()

    def by_alias(self) -> int:
        with self._lock:
            me = self
            me._lock = threading.Lock()
            return self.size()

    def by_del(self) -> int:
        with self._lock:
            del self._lock
            return self.size()

    def by_call(self) -> int:
        with self._lock:
            self._reset()
            return self.size()

    def by_callee(self) -> int:
        with self._lock:
            return self.fresh_size()

    def by_wrapper(self) -> int:
        with self._lock:
            return self.reset_then_size()

    def other_reset(self) -> int:
        with self._lock:
            self._reset_other()
            return self.size()


def cells() -> None:
    lock = threading.Lock()

    def take_cell() -> None:
        with lock:
            pass

    def renew_cell() -> None:
        nonlocal lock
        lock = threading.Lock()

    with lock:
        renew_cell()
        take_cell()
    with lock:
        lock = threading.Lock()
        take_cell()
    with lock:
        take_cell()


def own_store() -> None:
    global _LOCK
    with _LOCK:
        _LOCK = threading.Lock()
        take_own()


def locals_only(pool: Pool) -> None:
    other = Pool()
    other.size_hint = 1
    scratch = [pool]
    del scratch
    if False:
        take_own()
    with _LOCK:
        take_own()
"#;

#[test]
fn eff109_leaves_a_name_bound_to_another_lock() {
    let p = Project::new("eff109_rebound");
    p.write("pkg/__init__.py", "")
        .write("pkg/locks.py", REBOUND_LOCKS)
        .write("pkg/rebound.py", REBOUND);
    assert_eq!(
        found(&diagnostics(&p, SELECT)),
        vec![
            d(
                "pkg/rebound.py",
                39,
                "still_held",
                "pkg.locks.LOCK->pkg.locks.take"
            ),
            d(
                "pkg/rebound.py",
                93,
                "Pool.other_reset",
                "self._lock->pkg.rebound.Pool.size"
            ),
            d(
                "pkg/rebound.py",
                114,
                "cells",
                "lock->pkg.rebound.cells.<locals>.take_cell"
            ),
            d(
                "pkg/rebound.py",
                132,
                "locals_only",
                "pkg.rebound._LOCK->pkg.rebound.take_own"
            ),
        ]
    );
}

/// The tests of state in a function on the way from the holder to the acquisition: a flag the
/// holder sets, one or two calls deep, a re-entrance flag the wrapper tests and sets itself, a
/// method reading what the holder sets, and a module global the holder sets, not reported. A
/// flag the holder does not change, and the holder's own test before the call, keep the report.
const WRAPPED: &str = r#"import threading

_READY = False
_GLOBAL_LOCK = threading.Lock()


def take_global() -> None:
    with _GLOBAL_LOCK:
        pass


def when_ready() -> None:
    if not _READY:
        return
    take_global()


def start() -> None:
    global _READY
    with _GLOBAL_LOCK:
        _READY = True
        when_ready()


def start_unset() -> None:
    with _GLOBAL_LOCK:
        when_ready()


class Stream:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._closed = False
        self._stopping = False
        self._busy = False
        self._owner = None

    def take(self) -> None:
        with self._lock:
            pass

    def wrapper(self) -> None:
        if self._closed:
            return
        self.take()

    def deeper(self) -> None:
        self.wrapper()

    def close(self) -> None:
        with self._lock:
            self._closed = True
            self.wrapper()

    def close_deeper(self) -> None:
        with self._lock:
            self._closed = True
            self.deeper()

    def stop(self) -> None:
        if self._stopping:
            return
        self._stopping = True
        self._do_stop()

    def _do_stop(self) -> None:
        with self._lock:
            self._cleanup()

    def _cleanup(self) -> None:
        self.stop()

    def _in_own_call(self) -> bool:
        return self._owner is threading.current_thread()

    def checked(self) -> None:
        if self._in_own_call():
            return
        self.take()

    def run(self) -> None:
        with self._lock:
            self._owner = threading.current_thread()
            self.checked()

    def flush(self) -> None:
        with self._lock:
            self.wrapper()

    def flush_busy(self) -> None:
        with self._lock:
            self._busy = True
            self.wrapper()

    def drain(self) -> None:
        with self._lock:
            if not self._closed:
                self._closed = True
                self.take()
"#;

#[test]
fn eff109_reads_the_tests_of_functions_on_the_way() {
    let p = Project::new("eff109_wrapped");
    p.write("wrapped.py", WRAPPED);
    assert_eq!(
        found(&diagnostics(&p, SELECT)),
        vec![
            d(
                "wrapped.py",
                27,
                "start_unset",
                "wrapped._GLOBAL_LOCK->wrapped.when_ready"
            ),
            d(
                "wrapped.py",
                88,
                "Stream.flush",
                "self._lock->wrapped.Stream.wrapper"
            ),
            d(
                "wrapped.py",
                93,
                "Stream.flush_busy",
                "self._lock->wrapped.Stream.wrapper"
            ),
            d(
                "wrapped.py",
                99,
                "Stream.drain",
                "self._lock->wrapped.Stream.take"
            ),
        ]
    );
}

/// `(line, certainty)` of every `EFF109` diagnostic of `path`.
fn tiers(diags: &[Value], path: &str) -> Vec<(u64, String)> {
    diags
        .iter()
        .filter(|d| d["rule_code"] == "EFF109" && d["path"] == path)
        .map(|d| {
            let line = d["line"].as_u64().unwrap();
            (line, d["certainty"].as_str().unwrap().to_owned())
        })
        .collect()
}

fn tier(line: u64, certainty: &str) -> (u64, String) {
    (line, certainty.to_owned())
}

/// The exact tier: a retake of the function's own lock, a helper (also awaited, also two
/// calls deep) taking it on every return, against a helper with a branch around the lock, a
/// method a subclass overrides without it, a decorated helper, and a lock class a project class
/// derives from.
const TIERS: &str = r#"import asyncio
import functools
import threading


def logged(fn):
    @functools.wraps(fn)
    def inner(*args, **kwargs):
        return fn(*args, **kwargs)
    return inner


class Store:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self.items: list[int] = []

    def size(self) -> int:
        with self._lock:
            return len(self.items)

    def outer_size(self) -> int:
        return self.size() + 1

    def maybe_size(self, flag: bool) -> int:
        if flag:
            return 0
        with self._lock:
            return len(self.items)

    @logged
    def wrapped_size(self) -> int:
        with self._lock:
            return len(self.items)

    def again(self) -> None:
        with self._lock:
            with self._lock:
                pass

    def add(self, flag: bool) -> int:
        with self._lock:
            self.items.append(1)
            a = self.size()
            b = self.outer_size()
            c = self.maybe_size(flag)
            d = self.wrapped_size()
            return a + b + c + d


class Base:
    def __init__(self) -> None:
        self._lock = threading.Lock()

    def count(self) -> int:
        with self._lock:
            return 1

    def run(self) -> int:
        with self._lock:
            return self.count()


class Quiet(Base):
    def count(self) -> int:
        return 0


class Agents:
    def __init__(self) -> None:
        self._lock = asyncio.Lock()

    async def update(self) -> None:
        async with self._lock:
            pass

    async def beat(self) -> None:
        async with self._lock:
            await self.update()
"#;

#[test]
fn eff109_is_exact_only_where_every_premise_is_established() {
    let p = Project::new("eff109_tiers");
    p.write("tiers.py", TIERS);
    let diags = diagnostics(&p, SELECT);
    assert_eq!(
        tiers(&diags, "tiers.py"),
        vec![
            tier(38, "exact"),
            tier(44, "exact"),
            tier(45, "exact"),
            tier(46, "heuristic"),
            tier(47, "heuristic"),
            tier(61, "heuristic"),
            tier(79, "exact"),
        ],
        "{diags:#?}"
    );
    // The exact report through two calls shows the way it takes the lock on every return.
    let deep = at(&diags, "tiers.py", 45);
    assert_eq!(
        related(deep),
        vec![
            (
                "tiers.py".into(),
                42,
                "`Store.add` takes `self._lock` here".into()
            ),
            (
                "tiers.py".into(),
                45,
                "`Store.add` calls `tiers.Store.outer_size`".into()
            ),
            (
                "tiers.py".into(),
                23,
                "`Store.outer_size` calls `tiers.Store.size`".into()
            ),
            (
                "tiers.py".into(),
                19,
                "`Store.size` takes the lock again here".into()
            ),
        ],
        "{deep:#?}"
    );
}

/// A project class deriving from `asyncio.Lock` binds `acquire`, so under R2 a value of
/// `asyncio.Lock` may take it otherwise: no report on such a lock is exact.
#[test]
fn eff109_is_heuristic_for_a_lock_class_the_project_derives_from() {
    let p = Project::new("eff109_derived_lock");
    p.write(
        "board.py",
        r#"import asyncio


class Traced(asyncio.Lock):
    async def acquire(self) -> bool:
        return await super().acquire()


class Board:
    def __init__(self) -> None:
        self._lock = asyncio.Lock()

    async def update(self) -> None:
        async with self._lock:
            pass

    async def beat(self) -> None:
        async with self._lock:
            await self.update()
"#,
    );
    let diags = diagnostics(&p, SELECT);
    assert_eq!(
        tiers(&diags, "board.py"),
        vec![tier(19, "heuristic")],
        "{diags:#?}"
    );
}

/// A helper in another module that gives its caller's lock back before taking it again does not
/// wait for itself, and a call after it no longer holds the lock: neither is reported.
#[test]
fn eff109_leaves_a_lock_a_helper_gives_back() {
    let p = Project::new("eff109_gives_back");
    p.write("locks.py", "import threading\n\nLOCK = threading.Lock()\n");
    p.write(
        "store.py",
        r#"from helpers import pause
from locks import LOCK


def size() -> int:
    with LOCK:
        return 1


def add() -> int:
    with LOCK:
        pause()
        return size()
"#,
    );
    p.write(
        "helpers.py",
        r#"from locks import LOCK


def pause() -> None:
    LOCK.release()
    LOCK.acquire()
"#,
    );
    let diags = diagnostics(&p, SELECT);
    assert_eq!(found(&diags), vec![], "{diags:#?}");
}

/// `(qualname, certainty)` of every `EFF109` diagnostic.
fn qualname_tiers(diags: &[Value]) -> Vec<(String, String)> {
    diags
        .iter()
        .filter(|d| d["rule_code"] == "EFF109")
        .map(|d| {
            let qualname = d["qualname"].as_str().unwrap().to_owned();
            (qualname, d["certainty"].as_str().unwrap().to_owned())
        })
        .collect()
}

/// The counterexamples `tests/difftest/lock_retaken_counterexamples.py` runs on CPython, one or
/// more per premise, each returning without waiting on the input the script runs: those of
/// Non-reentrant (a project class derived from `asyncio.Lock`), Every implementation (a
/// decorator that may skip the body, a subclass's method), and Every return are heuristic;
/// those of Held and Same lock are not reported. The base shape, which waits for itself, is
/// exact.
#[test]
fn eff109_counterexamples_of_each_premise_are_not_exact() {
    let p = Project::new("eff109_counterexamples");
    let source = std::fs::read_to_string(
        common::repo_root().join("tests/difftest/lock_retaken_counterexamples.py"),
    )
    .unwrap();
    p.write("cases.py", &source);
    let diags = diagnostics(&p, SELECT);
    let expected = [
        ("Store.add", "exact"),
        ("Store.add_maybe", "heuristic"),
        ("Store.add_skipped", "heuristic"),
        ("Base.run", "heuristic"),
        ("Board.beat", "heuristic"),
    ];
    assert_eq!(
        qualname_tiers(&diags),
        expected.map(|(q, c)| (q.to_owned(), c.to_owned())),
        "{diags:#?}"
    );
}

/// An abstract method among the targets never runs: the report through it is exact when the
/// implementations take the lock on every return.
#[test]
fn eff109_skips_an_abstract_method_among_the_targets() {
    let p = Project::new("eff109_abstract_target");
    p.write(
        "counter.py",
        r#"import abc
import threading


class Base(abc.ABC):
    def __init__(self) -> None:
        self._lock = threading.Lock()

    @abc.abstractmethod
    def count(self) -> int: ...

    def run(self) -> int:
        with self._lock:
            return self.count()


class Counter(Base):
    def count(self) -> int:
        with self._lock:
            return 1
"#,
    );
    let diags = diagnostics(&p, SELECT);
    assert_eq!(
        tiers(&diags, "counter.py"),
        vec![tier(14, "exact")],
        "{diags:#?}"
    );
}
