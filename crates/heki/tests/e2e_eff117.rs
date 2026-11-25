//! `EFF117` (`unlocked-write-of-guarded-state`) end to end: shared state a writer changes holding
//! a thread lock, changed elsewhere holding none, where a thread heki resolves runs one of the
//! writes. The harm shapes (a lost reset, a read-modify-write, a lost invalidation, a counter),
//! the thread entries, and the shapes that are not reported: a helper taking the lock (released
//! by the caller, or returning holding it), a nested scope under the lock, a caller holding it,
//! a mutator name on an object that is no container, construction, `threading.local`, an
//! after-fork handler, test code, code no thread runs; and the selection.

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

/// `(path, line, qualname, target)` of every `EFF117` diagnostic, sorted.
fn found(diags: &[Value]) -> Vec<(String, u64, String, String)> {
    let mut out: Vec<_> = diags
        .iter()
        .filter(|d| d["rule_code"] == "EFF117")
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["qualname"].as_str().unwrap().to_owned(),
                d["target"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

fn d(path: &str, line: u64, qualname: &str, target: &str) -> (String, u64, String, String) {
    (path.into(), line, qualname.into(), target.into())
}

const SELECT: &[&str] = &["--select", "EFF117"];

/// A reader thread counting under the lock: a reset without it (a lost reset), a failure count
/// read and written back without it while the thread pops it under the lock.
const STATS: &str = r#"import threading


class Stats:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self.received = 0
        self.failures: dict[str, int] = {}
        self._load()

    def _load(self) -> None:
        self.received = 0

    def start(self) -> None:
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self) -> None:
        with self._lock:
            self.received += 1
            self.failures.pop("serial", None)

    def reset(self) -> None:
        self.received = 0

    def record(self, channel: str) -> None:
        self.failures[channel] = self.failures.get(channel, 0) + 1
"#;

/// A catalog rebuilt under a module lock on a worker thread, invalidated without the lock.
const CATALOG: &str = r#"import asyncio
import threading

_cat_lock = threading.Lock()
_cat: dict[str, float] = {"mtime": 0.0}
_version = 0


def _rebuild() -> None:
    global _version
    with _cat_lock:
        _cat["mtime"] = 1.0
        _version += 1


async def refresh() -> None:
    await asyncio.to_thread(_rebuild)


def exclude(name: str) -> None:
    global _version
    _cat["mtime"] = 0
    _version = 0


def _grab() -> None:
    _cat_lock.acquire()


def _drop() -> None:
    _cat_lock.release()


def invalidate_locked() -> None:
    _grab()
    _cat["mtime"] = 0
    _drop()
"#;

/// A `threading.Thread` subclass counting bytes without the lock a reset holds, and a module
/// list a thread pool's task appends to without the lock.
const SERIAL: &str = r#"import threading
from concurrent.futures import ThreadPoolExecutor


class Reader(threading.Thread):
    def __init__(self) -> None:
        super().__init__()
        self.stats_lock = threading.Lock()
        self.rx_bytes = 0

    def run(self) -> None:
        while True:
            data = b"x"
            self.rx_bytes += len(data)

    def reset_stats(self) -> None:
        with self.stats_lock:
            self.rx_bytes = 0


_seen_lock = threading.Lock()
_seen: list[str] = []


def add(line: str) -> None:
    with _seen_lock:
        _seen.append(line)


def _job(line: str) -> None:
    _seen.append(line)


def submit_all(pool: ThreadPoolExecutor, lines: list[str]) -> None:
    for line in lines:
        pool.submit(_job, line)
"#;

#[test]
fn reports_the_harm_shapes_with_the_locked_write() {
    let p = Project::new("eff117_harm");
    p.write("pkg/__init__.py", "")
        .write("pkg/stats.py", STATS)
        .write("pkg/catalog.py", CATALOG)
        .write("pkg/serial.py", SERIAL);
    let diags = diagnostics(&p, SELECT);
    assert_eq!(
        found(&diags),
        vec![
            d(
                "pkg/catalog.py",
                22,
                "exclude",
                "pkg.catalog._cat->pkg.catalog._cat_lock"
            ),
            d(
                "pkg/catalog.py",
                23,
                "exclude",
                "pkg.catalog._version->pkg.catalog._cat_lock"
            ),
            d(
                "pkg/serial.py",
                14,
                "Reader.run",
                "self.rx_bytes->self.stats_lock"
            ),
            d(
                "pkg/serial.py",
                31,
                "_job",
                "pkg.serial._seen->pkg.serial._seen_lock"
            ),
            d(
                "pkg/stats.py",
                23,
                "Stats.reset",
                "self.received->self._lock"
            ),
            d(
                "pkg/stats.py",
                26,
                "Stats.record",
                "self.failures->self._lock"
            ),
        ]
    );
    let reset = diags
        .iter()
        .find(|d| d["qualname"] == "Stats.reset")
        .unwrap();
    assert_eq!(
        reset["message"],
        "`Stats.reset` assigns `self.received` where heki finds no lock held, while \
         `Stats._read` assigns it holding `self._lock`; `Stats._read` can run on another \
         thread, so the two writes can interleave and one can lose the other's update"
    );
    assert_eq!(reset["related"][0]["line"], 19);
    assert_eq!(
        reset["related"][0]["message"],
        "`Stats._read` assigns `self.received` here holding `self._lock`"
    );
    assert_eq!(reset["certainty"], "heuristic");
    let job = diags.iter().find(|d| d["qualname"] == "_job").unwrap();
    assert!(
        job["message"]
            .as_str()
            .unwrap()
            .contains("`_job` can run on another thread"),
        "{job:#?}"
    );
}

/// The shapes whose writes hold a lock, run while the object is built, or are not shared.
const HELD: &str = r#"import os
import threading


class Meter:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._mem = Client()
        self.calls = 0
        self.seen: set[str] = set()
        with self._lock:
            self.pending: list[int] = []
        os.register_at_fork(after_in_child=self._after_fork)

    def start(self) -> None:
        threading.Thread(target=self._loop).start()
        list(filter(self._ok, ["a"]))

    def _ok(self, key: str) -> bool:
        return bool(key)

    def _loop(self) -> None:
        with self._lock:
            self.calls += 1
            self.seen.add("x")
            self._mem = Client()
            if self._lock.locked():
                self.seen.discard("y")

    def _acquire_state(self, wait: float | None) -> bool:
        if wait is None:
            return self._lock.acquire()
        return self._lock.acquire(timeout=wait)

    def local_lock(self) -> None:
        with threading.Lock():
            self.calls = 3

    @staticmethod
    def touch(m: "Meter") -> None:
        m.calls = 1

    def _take(self) -> None:
        self._lock.acquire()

    def _give(self) -> None:
        self._lock.release()

    def flush(self) -> None:
        if not self._acquire_state(1.0):
            return
        try:
            self.calls = 0
            self._take_batch_locked()
        finally:
            self._lock.release()

    def _take_batch_locked(self) -> None:
        self.calls = 0

    def swap(self) -> None:
        self._take()
        self.calls = 2
        self._give()

    def rerun(self) -> None:
        with self._lock:
            any(self._request_locked() for _ in range(2))
            any(self.seen.add(k) for k in ["b"])
            list(map(lambda k: self._mark(k), ["a"]))

    def _request_locked(self) -> bool:
        self.calls += 1
        return True

    def _mark(self, key: str) -> None:
        self.seen.add(key)

    def remember(self, text: str) -> None:
        self._mem.add(text)

    def pend(self) -> None:
        self.pending.clear()

    def _after_fork(self) -> None:
        self.calls = 0


class Client:
    def add(self, text: str) -> None:
        print(text)


class PerThread(threading.local):
    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.depth = 0

    def enter(self) -> None:
        with self.lock:
            self.depth += 1

    def leave(self) -> None:
        self.depth -= 1


_tls = threading.local()
_tls_lock = threading.Lock()


def _worker() -> None:
    with _tls_lock:
        _tls.value = 1
    holder = PerThread()
    holder.enter()


def clear() -> None:
    _tls.value = 0


def poke(m: Meter) -> None:
    m._take()
    m.calls = 5
    m._give()


def run_all(m: Meter) -> None:
    lock = threading.Lock()

    def work() -> None:
        with lock:
            m.calls = 1

    threading.Thread(target=work).start()


def start() -> None:
    threading.Thread(target=_worker).start()
"#;

/// Locked and unlocked writes no thread heki resolves runs, and a test writing locked state.
const NO_THREAD: &str = r#"import threading


class Cache:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self.items: dict[str, int] = {}

    def put(self, key: str) -> None:
        with self._lock:
            self.items[key] = 1

    def drop(self) -> None:
        self.items.clear()
"#;

const TEST: &str = r#"from pkg.stats import Stats


class FakeStats(Stats):
    def poke(self) -> None:
        self.received = 9
"#;

#[test]
fn leaves_held_setup_unshared_and_unthreaded_writes() {
    let p = Project::new("eff117_held");
    p.write("pkg/__init__.py", "")
        .write("pkg/meter.py", HELD)
        .write("pkg/cache.py", NO_THREAD)
        .write("pkg/stats.py", STATS)
        .write("tests/test_stats.py", TEST);
    let diags = diagnostics(&p, SELECT);
    // `remember` calls `add` on a client; `pend` empties a list guarded only in `__init__`,
    // a constructor's lock making nothing guarded.
    let shown: Vec<_> = found(&diags)
        .into_iter()
        .filter(|(path, ..)| path != "pkg/stats.py")
        .collect();
    assert_eq!(shown, vec![], "{diags:#?}");
}

#[test]
fn runs_only_when_selected_or_heuristic() {
    let p = Project::new("eff117_selection");
    p.write("pkg/__init__.py", "").write("pkg/stats.py", STATS);
    assert_eq!(found(&diagnostics(&p, &["--no-heuristic"])), vec![]);
    assert_eq!(found(&diagnostics(&p, &[])).len(), 2);
    let plain = Project::new("eff117_no_locks");
    plain.write("m.py", "def f(xs: list[int]) -> None:\n    xs.append(1)\n");
    assert_eq!(found(&diagnostics(&plain, SELECT)), vec![]);
    let (code, stdout, _) = p.heki(&["rule", "EFF117"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("## What a report states"), "{stdout}");
    assert!(stdout.contains("This rule is heuristic"), "{stdout}");
}

/// A class lock taken through `cls`, a declared global deleted, a lock released later that is not
/// the one held, a branch that never ends, and a helper taking a lock of another object.
const SHAPES: &str = r#"import threading


_reg_lock = threading.Lock()


class Registry:
    count = 0

    @classmethod
    def bump(cls) -> None:
        with _reg_lock:
            cls.count += 1

    @classmethod
    def reset(cls) -> None:
        cls.count = 0


_slot_lock = threading.Lock()
_slot = 0


def _fill() -> None:
    global _slot
    with _slot_lock:
        _slot = 1


def drop() -> None:
    global _slot
    del _slot


class Pair:
    def __init__(self) -> None:
        self._a = threading.Lock()
        self._b = threading.Lock()
        self.n = 0
        self.m = 0

    def start(self) -> None:
        threading.Thread(target=self._loop).start()
        threading.Thread(target=self._spin).start()

    def _loop(self) -> None:
        with self._a:
            self.n += 1
        self._b.release()

    def clear(self) -> None:
        self.n = 0

    def _spin(self) -> None:
        if self.m:
            while True:
                pass
        self._a.acquire()
        self.m += 1
        self._a.release()

    def zero(self) -> None:
        self.m = 0

    def _take(self) -> None:
        self._a.acquire()

    def _give(self) -> None:
        self._a.release()

    def merge(self, other: "Pair") -> None:
        other._take()
        self.m = 3
        other._give()


def borrow(one: Pair, other: Pair) -> None:
    other._take()
    other.n = 2
    other._give()


def run() -> None:
    threading.Thread(target=_fill).start()
    threading.Thread(target=Registry.bump).start()
"#;

#[test]
fn reports_writes_in_the_remaining_lock_shapes() {
    let p = Project::new("eff117_shapes");
    p.write("pkg/__init__.py", "")
        .write("pkg/shapes.py", SHAPES);
    let diags = diagnostics(&p, SELECT);
    assert_eq!(
        found(&diags),
        vec![
            d(
                "pkg/shapes.py",
                17,
                "Registry.reset",
                "pkg.shapes.Registry.count->pkg.shapes._reg_lock"
            ),
            d(
                "pkg/shapes.py",
                32,
                "drop",
                "pkg.shapes._slot->pkg.shapes._slot_lock"
            ),
            d("pkg/shapes.py", 52, "Pair.clear", "self.n->self._a"),
            d("pkg/shapes.py", 63, "Pair.zero", "self.m->self._a"),
        ],
        "{diags:#?}"
    );
}
