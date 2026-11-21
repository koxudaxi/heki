//! `queue.Queue.put` waits only while the queue is full, which only a queue its `maxsize`
//! bounds can be: `EFF101` reports a `put` only on a queue known to be bounded. heki follows
//! the queue from its construction into locals, `self` attributes the class family stores,
//! module globals, and parameters whose callers bind known queues; a queue it cannot follow is
//! not known to be bounded.

mod common;

use common::Project;
use serde_json::Value;

/// `(path, line, qualname)` of every `EFF101` report of `heki check` on `files`, sorted.
fn eff101(name: &str, files: &[(&str, &str)]) -> Vec<(String, u64, String)> {
    let p = Project::new(name);
    for (rel, text) in files {
        p.write(rel, text);
    }
    let (code, stdout, stderr) = p.check(&["--output-format", "json", "--select", "EFF101"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<(String, u64, String)> = diags
        .iter()
        .filter(|d| d["rule_code"] == "EFF101")
        .map(|d| {
            assert!(d["target"].as_str().unwrap().ends_with(".put"), "{d:#}");
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["qualname"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

fn r(path: &str, line: u64, qualname: &str) -> (String, u64, String) {
    (path.into(), line, qualname.into())
}

const LOCALS: &str = r#"import queue
from queue import LifoQueue, PriorityQueue, Queue
from typing import Final

LIMIT: Final[int] = 30


async def tp_bounded(item):
    q = queue.Queue(maxsize=2)
    q.put(item)
    lifo = LifoQueue(LIMIT)
    lifo.put(item)
    pri = PriorityQueue(LIMIT - 29)
    pri.put(item, timeout=5)


async def tp_bounded_on_one_path(item, big: bool):
    q = Queue(10) if big else Queue()
    q.put(item)


async def tn_unbounded(item):
    q = queue.Queue()
    q.put(item)
    lifo = LifoQueue(0)
    lifo.put(item)
    pri = PriorityQueue(maxsize=-1)
    pri.put(item)
    none = Queue(None)
    none.put(item)


async def tn_returns_at_once(item):
    q = queue.Queue(maxsize=2)
    q.put(item, block=False)
    q.put(item, False)
    q.put(item, timeout=0)
    q.put(item, True, 0.0)
    q.put_nowait(item)


async def tn_unknown_maxsize(item, size: int, options):
    q = queue.Queue(maxsize=size)
    q.put(item)
    sized = queue.Queue(*options)
    sized.put(item)
    q2 = make()
    q2.put(item)
    q3 = queue.Queue(maxsize=options.size)
    q3.put(item)
    q4 = FACTORY()
    q4.put(item)


def make() -> queue.Queue:
    return queue.Queue(1)


class QueueFactory:
    def __call__(self) -> queue.Queue:
        return queue.Queue(maxsize=1)


FACTORY = QueueFactory()


async def tp_carried_by_a_loop(items):
    q = queue.Queue(maxsize=2)
    for item in items:
        held = q
        q = held
        q.put(item)


async def tn_unknown_path(item, given: queue.Queue | None):
    q = given or queue.Queue(maxsize=2)
    q.put(item)
"#;

/// A queue built in the function: a positive `maxsize` (a literal or a module constant) bounds
/// it on the paths it reaches, also around a loop; none, zero, a negative one, or `None` leaves
/// it unbounded, and a `maxsize` or a queue heki cannot see (an attribute of an untyped object,
/// what a callable object returns) leaves it unknown. A `put` that returns at once
/// (`block=False`, a zero timeout, `put_nowait`) never waits.
#[test]
fn a_put_waits_only_on_a_queue_its_function_bounds() {
    assert_eq!(
        eff101("queue_locals", &[("app/locals.py", LOCALS)]),
        vec![
            r("app/locals.py", 10, "tp_bounded"),
            r("app/locals.py", 12, "tp_bounded"),
            r("app/locals.py", 14, "tp_bounded"),
            r("app/locals.py", 19, "tp_bounded_on_one_path"),
            r("app/locals.py", 72, "tp_carried_by_a_loop"),
        ]
    );
}

// textual: a module-level `Final` constant bounds the queue a writer thread stores.
const WRITER_THREAD: &str = r#"import threading
from queue import Queue
from typing import Final

MAX_QUEUED_WRITES: Final[int] = 30


class WriterThread(threading.Thread):
    def __init__(self) -> None:
        super().__init__()
        self._queue: Queue[str | None] = Queue(MAX_QUEUED_WRITES)

    def write(self, text: str) -> None:
        self._queue.put(text)

    def stop(self) -> None:
        self._queue.put(None)
"#;

// OpenChronicle: a class constant bounds the queue `__init__` stores.
const CAPTURE_RUNNER: &str = r#"import queue
from typing import Any


class CaptureRunner:
    _MAX_PENDING = 16
    _SENTINEL = object()

    def __init__(self) -> None:
        self._queue: queue.Queue[Any] = queue.Queue(maxsize=self._MAX_PENDING)

    def stop_worker(self) -> None:
        self._queue.put(self._SENTINEL, timeout=1.0)
"#;

// wenyi: `__init__`'s `capacity` bounds the queue; every construction leaves its positive
// default.
const REPORTER: &str = r#"import queue
from typing import TextIO


class Reporter:
    """Bounded producer."""

    def __init__(self, stream: TextIO, *, capacity: int = 4096):
        self.stream = stream
        self.queue: queue.Queue[str | None] = queue.Queue(maxsize=capacity)

    def protocol(self, value: str) -> None:
        self.queue.put(value, timeout=2)


def main(stream: TextIO) -> Reporter:
    return Reporter(stream)
"#;

// pymongo: a bounded queue built in one class, handed to another class's `__init__`, which
// stores it; the `put` is in a method of the second class.
const TELEMETRY: &str = r#"import queue
from typing import Any


class SdamTelemetry:
    def __init__(self, topology_id, events: queue.Queue[Any] | None) -> None:
        self._topology_id = topology_id
        self._events = events

    def enqueue(self, fn, args) -> None:
        if self._events is not None:
            self._events.put((fn, args))

    def topology_opened(self) -> None:
        self.enqueue(print, (self._topology_id,))
"#;
const TOPOLOGY: &str = r#"import queue
from typing import Any

from app.telemetry import SdamTelemetry


class TopologyBase:
    def __init__(self, topology_id, publish: bool) -> None:
        self._events: queue.Queue[Any] | None = None
        if publish:
            self._events = queue.Queue(maxsize=100)
        self._sdam = SdamTelemetry(topology_id, self._events)
        self._sdam.topology_opened()
"#;

const BOUNDED_USERS: &str = r#"from app.capture import CaptureRunner
from app.reporter import Reporter
from app.topology import TopologyBase
from app.writer import WriterThread


async def tp_writer(w: WriterThread) -> None:
    w.write("x")


async def tp_runner(r: CaptureRunner) -> None:
    r.stop_worker()


async def tp_reporter(r: Reporter) -> None:
    r.protocol("ready")


async def tp_topology(topology_id) -> None:
    TopologyBase(topology_id, True)
"#;

/// The bounded queues of the corpus: a `maxsize` from a module constant, a class constant, a
/// constructor parameter whose every call leaves its positive default, and a bounded queue
/// handed to another class's constructor, which stores it.
#[test]
fn a_put_waits_on_a_queue_the_class_family_stores_bounded() {
    assert_eq!(
        eff101(
            "queue_attributes_bounded",
            &[
                ("app/__init__.py", ""),
                ("app/writer.py", WRITER_THREAD),
                ("app/capture.py", CAPTURE_RUNNER),
                ("app/reporter.py", REPORTER),
                ("app/telemetry.py", TELEMETRY),
                ("app/topology.py", TOPOLOGY),
                ("app/users.py", BOUNDED_USERS),
            ]
        ),
        vec![
            r("app/users.py", 8, "tp_writer"),
            r("app/users.py", 12, "tp_runner"),
            r("app/users.py", 16, "tp_reporter"),
            r("app/users.py", 20, "tp_topology"),
        ]
    );
}

// playwright, 3dmodel, transformers' serving thread: an unbounded queue in an attribute.
const ENDPOINT: &str = r#"import asyncio
import queue
from queue import Queue


class RemoteEndpoint:
    def __init__(self) -> None:
        self._jobs: "queue.Queue" = queue.Queue()
        self._outbox: "asyncio.Queue" = asyncio.Queue()

    async def _handler(self, client, message) -> None:
        if message:
            self._jobs.put((client, message))
        self._jobs.put((client, None))
        await self._outbox.put(message)


class StaWorker:
    def __init__(self) -> None:
        self._jobs = queue.Queue()

    def close(self) -> None:
        self._jobs.put(None)


class InferenceThread:
    def __init__(self) -> None:
        self._queue: Queue = Queue()

    def submit(self, job) -> None:
        self._queue.put(job)
"#;

// protoprobe: an unbounded queue handed to a thread through an annotated parameter.
const POLLER: &str = r#"import queue
import threading


class PollerThread(threading.Thread):
    def __init__(self, interval_s: float, out_queue: queue.Queue) -> None:
        super().__init__(daemon=True)
        self.interval_s = interval_s
        self.out_queue = out_queue

    async def _run_async(self) -> None:
        self.out_queue.put(("conn_status", "connecting"))


class App:
    def __init__(self) -> None:
        self.ui_queue: queue.Queue = queue.Queue()
        self.poller = PollerThread(1.0, self.ui_queue)
"#;

// agentj: a module global built unbounded on first use.
const DISKQ: &str = r#"import queue

_q: "queue.Queue | None" = None


def _submit(job) -> None:
    global _q
    if _q is None:
        _q = queue.Queue()
    _q.put(job)


def write(path, data) -> None:
    _submit((path, data))
"#;

// transformers: the `maxsize` comes from a configuration object (its default is 0).
const BATCHING: &str = r#"import queue


class BatchingConfig:
    def __init__(self, max_queue_size: int = 0) -> None:
        self.max_queue_size = max_queue_size


class BatchingManager:
    def __init__(self, config: BatchingConfig) -> None:
        self.input_queue: queue.Queue = queue.Queue(maxsize=config.max_queue_size)

    def add_request(self, request) -> None:
        self.input_queue.put(request, block=True, timeout=10)
"#;

const UNBOUNDED_USERS: &str = r#"from app.batching import BatchingManager
from app.diskq import write
from app.endpoint import InferenceThread, StaWorker


async def tn_sta(worker: StaWorker) -> None:
    worker.close()


async def tn_inference(thread: InferenceThread) -> None:
    thread.submit(1)


async def tn_diskq() -> None:
    write("p", b"")


async def tn_batching(manager: BatchingManager) -> None:
    manager.add_request(1)
"#;

/// The unbounded and unknown queues of the corpus: built without a `maxsize`, handed over
/// unbounded through an annotated parameter, a module global built unbounded on first use, and
/// a `maxsize` read from a configuration object.
#[test]
fn a_put_on_an_unbounded_or_unknown_queue_does_not_wait() {
    assert_eq!(
        eff101(
            "queue_attributes_unbounded",
            &[
                ("app/__init__.py", ""),
                ("app/endpoint.py", ENDPOINT),
                ("app/poller.py", POLLER),
                ("app/diskq.py", DISKQ),
                ("app/batching.py", BATCHING),
                ("app/users.py", UNBOUNDED_USERS),
            ]
        ),
        Vec::new()
    );
}

const FAMILY: &str = r#"import queue


class Base:
    def __init__(self) -> None:
        self.q: queue.Queue = queue.Queue(maxsize=4)

    def push(self, item) -> None:
        self.q.put(item)


class Resetting(Base):
    def reset(self) -> None:
        self.q = make()


class Plain:
    def __init__(self) -> None:
        self.q: queue.Queue = queue.Queue(maxsize=4)

    def push(self, item) -> None:
        self.q.put(item)


class Shrinking(Plain):
    def shrink(self) -> None:
        self.q = queue.Queue()


def make() -> queue.Queue:
    return queue.Queue()


class Either:
    def __init__(self, q: queue.Queue | None = None) -> None:
        self.q = q or queue.Queue(maxsize=10)

    def push(self, item) -> None:
        self.q.put(item)


def build() -> None:
    Either()
    Either(queue.Queue())


EVENTS = queue.Queue(maxsize=8)
LOG = queue.Queue()


def send(q: queue.Queue, item) -> None:
    q.put(item)
"#;
const FAMILY_USERS: &str = r#"import queue

from app.family import EVENTS, LOG, Base, Either, Plain, send


async def tn_reset_by_a_subclass(b: Base) -> None:
    b.push(1)


async def tp_unbounded_in_a_subclass(p: Plain) -> None:
    p.push(1)


async def tp_bounded_default(e: Either) -> None:
    e.push(1)


async def tp_imported_global() -> None:
    EVENTS.put(1)


async def tn_imported_global() -> None:
    LOG.put(1)


async def tp_helper_bounded() -> None:
    send(queue.Queue(3), 1)


async def tn_helper_unbounded() -> None:
    send(queue.Queue(), 1)


async def tn_parameter_without_callers(q: queue.Queue) -> None:
    q.put(1)


async def drain(q: queue.Queue) -> None:
    q.put(1)


async def tn_awaits_the_boundary() -> None:
    await drain(queue.Queue(1))
"#;

/// Every store the class family makes counts: a subclass storing a queue heki cannot see makes
/// the attribute unknown, one storing an unbounded queue leaves it bounded on the other paths.
/// A global holds what its module binds it to. A helper's parameter is decided at each call; an
/// `async def`'s by the join of its callers, and a parameter without project callers is not
/// known.
#[test]
fn every_store_and_every_caller_decides() {
    assert_eq!(
        eff101(
            "queue_stores_and_callers",
            &[
                ("app/__init__.py", ""),
                ("app/family.py", FAMILY),
                ("app/users.py", FAMILY_USERS),
            ]
        ),
        vec![
            r("app/users.py", 11, "tp_unbounded_in_a_subclass"),
            r("app/users.py", 15, "tp_bounded_default"),
            r("app/users.py", 19, "tp_imported_global"),
            r("app/users.py", 27, "tp_helper_bounded"),
            r("app/users.py", 39, "drain"),
        ]
    );
}

const BOUNDARY: &str = r#"[project]
name = "t"

[[tool.heki.effect-boundaries]]
name = "no-waits"
scopes = ["app.render.*"]
forbid = ["blocking-sync"]
"#;
const RENDER: &str = r#"import queue

BOUNDED = queue.Queue(maxsize=1)
UNBOUNDED = queue.Queue()


def tp_render_bounded(item) -> None:
    BOUNDED.put(item)


def tn_render_unbounded(item) -> None:
    UNBOUNDED.put(item)
"#;

/// An effect boundary forbidding synchronous waits counts a `put` as `EFF101` does.
#[test]
fn effect_boundaries_count_only_waits_on_bounded_queues() {
    let p = Project::new("queue_effect_boundary");
    p.write("pyproject.toml", BOUNDARY);
    p.write("app/__init__.py", "");
    p.write("app/render.py", RENDER);
    let (code, stdout, stderr) = p.check(&["--output-format", "json", "--select", "EFF201"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let reported: Vec<(&str, u64)> = diags
        .iter()
        .map(|d| (d["qualname"].as_str().unwrap(), d["line"].as_u64().unwrap()))
        .collect();
    assert_eq!(reported, vec![("tp_render_bounded", 8)]);
}

const POOLS: &str = r#"import queue


class Pool:
    def __init__(self, size: int = 8) -> None:
        self.jobs: queue.Queue = queue.Queue(maxsize=size)

    def submit(self, job) -> None:
        self.jobs.put(job)


class Sink:
    def __init__(self, size: int = 8) -> None:
        self.jobs: queue.Queue = queue.Queue(maxsize=size)

    def submit(self, job) -> None:
        self.jobs.put(job)


class Spool:
    def __init__(self, size: int = 8) -> None:
        self.jobs: queue.Queue = queue.Queue(maxsize=size)

    def submit(self, job) -> None:
        self.jobs.put(job)


class Capped:
    _LIMIT = 4

    def __init__(self) -> None:
        limit = self._LIMIT
        self.jobs: queue.Queue = queue.Queue(maxsize=limit)

    def submit(self, job) -> None:
        self.jobs.put(job)


def build(n: int) -> None:
    Pool(0)
    Pool()
    Sink(0)
    Spool(n)
"#;
const HELPERS: &str = r#"import queue
import sys


def send(q: queue.Queue, item) -> None:
    q.put(item)


def relay(q: queue.Queue, item) -> None:
    send(q, item)


def emit(item, out: queue.Queue | None = None) -> None:
    if out is not None:
        out.put(item)


if sys.version_info >= (3, 13):

    def forward(q: queue.Queue, item) -> None:
        q.put(item)

else:

    def forward(q: queue.Queue, item) -> None:
        q.put(item, True)


EVENTS = queue.Queue(maxsize=8)


class Outbox:
    def __init__(self) -> None:
        self.events: queue.Queue = EVENTS

    def post(self, item) -> None:
        self.events.put(item)


def make_handler():
    class Handler:
        EVENTS.put("ready")

    return Handler
"#;
const JOBS: &str = r#"import queue

JOBS = queue.Queue()


def make() -> queue.Queue:
    return queue.Queue()


SHARED: queue.Queue = make()


def push(item) -> None:
    JOBS.put(item)


def share(item) -> None:
    SHARED.put(item)
"#;
const SETUP: &str = r#"import queue

from app import jobs


def configure() -> None:
    jobs.JOBS = queue.Queue(maxsize=1)
"#;
const MORE_USERS: &str = r#"import queue

from app import helpers
from app.helpers import Outbox, emit, forward, make_handler, relay, send
from app.jobs import push, share
from app.pools import Capped, Pool, Sink, Spool


async def tp_pool(p: Pool) -> None:
    p.submit(1)


async def tn_sink(s: Sink) -> None:
    s.submit(1)


async def tn_spool(s: Spool) -> None:
    s.submit(1)


async def tp_capped(c: Capped) -> None:
    c.submit(1)


async def tp_attribute_of_a_typed_object(p: Pool) -> None:
    p.jobs.put(1)


async def tp_relay_bounded() -> None:
    relay(queue.Queue(2), 1)


async def tn_relay_unbounded() -> None:
    relay(queue.Queue(), 1)


async def tn_helper_unpacked(args) -> None:
    send(*args)


async def tn_emit_default() -> None:
    emit(1)


async def tp_emit_bounded() -> None:
    emit(1, queue.Queue(1))


async def tp_forward() -> None:
    forward(queue.Queue(1), 1)


async def tp_module_attribute() -> None:
    helpers.EVENTS.put(1)


async def tp_class_built_in_a_function() -> None:
    make_handler()


async def tn_rebound_elsewhere() -> None:
    push(1)


async def tn_global_from_a_call() -> None:
    share(1)


async def forwarding(q: queue.Queue) -> None:
    send(q, 1)


async def tn_awaits_a_forwarding_boundary() -> None:
    await forwarding(queue.Queue(1))


async def tn_forwarding_without_callers(q: queue.Queue) -> None:
    send(q, 1)


async def tp_outbox(o: Outbox) -> None:
    o.post(1)
"#;

/// Constructor arguments decide the `maxsize` a class stores (a zero one leaves the queue
/// unbounded, an argument heki cannot see leaves it unknown), and a class constant does through
/// a local copy. A helper waits only for the calls handing it a bounded queue, also through
/// another helper and in each of its conditional definitions; a call unpacking its arguments and
/// a parameter left to its `None` default hand none, and an `async def` handing its parameter on
/// waits only when its callers bind a bounded queue. A module global is followed where a class
/// stores it and where it is read as a module's attribute; one another module rebinds, or that a
/// call returns, is not known.
#[test]
fn constructor_arguments_helpers_and_module_globals_decide() {
    assert_eq!(
        eff101(
            "queue_arguments_and_globals",
            &[
                ("app/__init__.py", ""),
                ("app/pools.py", POOLS),
                ("app/helpers.py", HELPERS),
                ("app/jobs.py", JOBS),
                ("app/setup.py", SETUP),
                ("app/users.py", MORE_USERS),
            ]
        ),
        vec![
            r("app/users.py", 10, "tp_pool"),
            r("app/users.py", 22, "tp_capped"),
            r("app/users.py", 26, "tp_attribute_of_a_typed_object"),
            r("app/users.py", 30, "tp_relay_bounded"),
            r("app/users.py", 46, "tp_emit_bounded"),
            r("app/users.py", 50, "tp_forward"),
            r("app/users.py", 54, "tp_module_attribute"),
            r("app/users.py", 58, "tp_class_built_in_a_function"),
            r("app/users.py", 70, "forwarding"),
            r("app/users.py", 82, "tp_outbox"),
        ]
    );
}
