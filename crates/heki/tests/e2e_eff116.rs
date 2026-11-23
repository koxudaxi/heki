//! `EFF116` (`generator-never-iterated`) end to end: a call of a generator function whose
//! generator nothing iterates, as a statement (a method on `self`, a class method, a static
//! method, a nested function calling itself, an async generator, a function of another module,
//! in a module body), read only by truth tests (`if`, `while`, `assert`, a conditional
//! expression, `not`, `bool()`), and handed to library calls that drop what they call returns
//! (threads, timers, processes, event loop callbacks, `atexit`, signal handlers, exit stacks) or
//! test it for truth (`filter` and three `itertools` functions), directly, as a bound method,
//! and through `functools.partial`; the wording of a method with a plain implementation and of a
//! name bound to two definitions; and the shapes left silent: a generator iterated, stepped,
//! returned, yielded, delegated to, stored, bound, or passed on, a decorated generator function,
//! an awaited call, a shadowed `bool`, a plain function, and the calls that hand the result back
//! (executors, offloading APIs, `weakref.finalize`); the opt-in selection, and `heki rule`.
//! Every line marked `# EFF116` in the sources is reported, and no other.

mod common;

use common::Project;
use serde_json::Value;

const ROWS: &str = r#"def scan(text: str):
    for line in text.splitlines():
        yield line.strip()


async def stream(n: int):
    for i in range(n):
        yield i


def plain(text: str) -> list[str]:
    return text.splitlines()
"#;

/// The shapes of the corpus, rewritten: a method that fills a cache as it yields, called as a
/// statement before the cache is read; a fallback behind `not` of a generator; a yield-style
/// test helper called and dropped; an async generator method called in a test; a nested
/// generator calling itself.
const STATEMENTS: &str = r#"from pkg.rows import scan, stream


class Code:
    def __init__(self, raw: list[bytes]) -> None:
        self.raw = raw
        self.cache: list[str] | None = None

    def instructions(self):
        if self.cache is None:
            self.cache = []
            for item in self.raw:
                self.cache.append(item.decode())
        yield from self.cache

    def instruction(self, i: int) -> str:
        self.instructions()  # EFF116
        assert self.cache is not None
        return self.cache[i]


class Channel:
    @classmethod
    def embed_urls(cls, page: str):
        for line in page.splitlines():
            if line.startswith("http"):
                yield line

    @staticmethod
    def parts(page: str):
        yield page

    def urls(self, page: str) -> list[str]:
        if not Channel.embed_urls(page):  # EFF116
            return [page]
        Channel.parts(page)  # EFF116
        return list(Channel.embed_urls(page))


def check_roundtrip(path: str):
    data = open(path).read()
    yield data == data.strip()


def test_roundtrip() -> None:
    check_roundtrip("a.txt")  # EFF116


class Spider:
    async def parse(self, page: str):
        yield page


async def test_parse() -> None:
    Spider().parse("p")  # EFF116
    stream(3)  # EFF116
    async for item in Spider().parse("q"):
        print(item)


def visit_binary(node):
    def visit(n):
        if n is not None:
            yield n
            visit(n.left)  # EFF116
            yield from visit(n.right)

    return list(visit(node))


def lines(text: str) -> int:
    scan(text)  # EFF116
    return len(text)


scan("module")  # EFF116
"#;

const TESTS: &str = r#"from pkg.rows import scan


def truth(text: str) -> bool:
    if scan(text):  # EFF116
        pass
    while scan(text):  # EFF116
        break
    assert scan(text)  # EFF116
    first = 1 if scan(text) else 0  # EFF116
    negated = not scan(text)  # EFF116
    tested = bool(scan(text))  # EFF116
    return negated or tested or first == 1
"#;

const HANDOFFS: &str = r#"import asyncio
import atexit
import contextlib
import functools
import itertools
import multiprocessing
import signal
import threading

from pkg.rows import scan


class Exporter:
    def drain(self):
        yield 1

    def start(self) -> None:
        threading.Thread(target=self.drain).start()  # EFF116
        atexit.register(functools.partial(self.drain))  # EFF116


def runners(items: list[str]) -> list[str]:
    threading.Thread(target=scan, args=("x",)).start()  # EFF116
    threading.Timer(1.0, scan).start()  # EFF116
    multiprocessing.Process(target=scan).start()  # EFF116
    atexit.register(scan, "x")  # EFF116
    signal.signal(signal.SIGTERM, scan)  # EFF116
    with contextlib.ExitStack() as stack:
        stack.callback(scan, "x")  # EFF116
    kept = list(filter(scan, items))  # EFF116
    dropped = list(itertools.filterfalse(scan, items))  # EFF116
    head = list(itertools.takewhile(scan, items))  # EFF116
    tail = list(itertools.dropwhile(scan, items))  # EFF116
    return kept + dropped + head + tail


async def callbacks() -> None:
    loop = asyncio.get_running_loop()
    loop.call_soon(scan, "x")  # EFF116
    loop.call_later(1.0, scan, "x")  # EFF116
"#;

/// Generators that something iterates, returns, keeps, or hands on, generator functions under
/// decorators, and calls that are no generator function's or hand the result back.
const SILENT: &str = r#"import asyncio
import contextlib
import threading
import weakref
from concurrent.futures import ThreadPoolExecutor

import pytest

from pkg.rows import plain, scan, stream


def keep(fn):
    return fn


@contextlib.contextmanager
def opened(path: str):
    yield path


@pytest.fixture
def server():
    yield 1


@keep
def kept():
    yield 1


class Holder:
    def __init__(self) -> None:
        self.rows = scan("x")


def bool(value) -> bool:
    return value is not None


def consumers(text: str, sink: list):
    for line in scan(text):
        print(line)
    rows = list(scan(text))
    first = next(scan(text), None)
    unpacked = [*scan(text)]
    sink.append(scan(text))
    held = scan(text)
    either = scan(text) or None
    yield scan(text)
    yield from scan(text)
    opened("p")
    kept()
    plain(text)
    shadowed = bool(scan(text))
    gen_expr = (line for line in text)
    return rows, first, unpacked, held, either, shadowed, gen_expr


def returned(text: str):
    return scan(text)


async def awaited(text: str) -> None:
    async for item in stream(3):
        print(item)
    await asyncio.to_thread(scan, text)
    loop = asyncio.get_running_loop()
    await loop.run_in_executor(None, scan, text)


def hands_back(text: str, items: list[str]) -> None:
    with ThreadPoolExecutor() as pool:
        future = pool.submit(scan, text)
        for line in future.result():
            print(line)
        for rows in pool.map(scan, items):
            print(list(rows))
    weakref.finalize(items, scan, text)
    threading.Thread(target=plain, args=(text,)).start()
"#;

/// The reports worded as possible: a method whose call may run a plain implementation, and a
/// name bound to two definitions.
const POSSIBLE: &str = r#"import sys


class Source:
    def rows(self):
        yield 1


class Cached(Source):
    def rows(self):
        return [1]


if sys.version_info >= (3, 12):
    def chunks():
        return []
else:
    def chunks():
        yield 1


def chunks():
    yield 1


def run(source: Source) -> None:
    source.rows()  # EFF116
    chunks()  # EFF116
"#;

const SOURCES: &[(&str, &str)] = &[
    ("pkg/rows.py", ROWS),
    ("pkg/statements.py", STATEMENTS),
    ("pkg/tests.py", TESTS),
    ("pkg/handoffs.py", HANDOFFS),
    ("pkg/silent.py", SILENT),
    ("pkg/possible.py", POSSIBLE),
];

fn project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("pkg/__init__.py", "");
    for (rel, text) in SOURCES {
        p.write(rel, text);
    }
    p
}

fn diagnostics(p: &Project, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["--output-format", "json"];
    all.extend_from_slice(args);
    let (code, stdout, stderr) = p.check(&all);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

fn eff116(diags: &[Value]) -> Vec<&Value> {
    diags
        .iter()
        .filter(|d| d["rule_code"] == "EFF116")
        .collect()
}

/// `(path, line)` of every `EFF116` diagnostic.
fn found(diags: &[Value]) -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = eff116(diags)
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
            )
        })
        .collect();
    out.sort();
    out
}

/// `(path, line)` of every line marked `# EFF116` in the sources.
fn marked() -> Vec<(String, u64)> {
    let mut out: Vec<(String, u64)> = SOURCES
        .iter()
        .flat_map(|(rel, text)| {
            text.lines()
                .zip(1u64..)
                .filter(|(line, _)| line.ends_with("# EFF116"))
                .map(|(_, n)| ((*rel).to_owned(), n))
        })
        .collect();
    out.sort();
    out
}

fn report_at<'d>(diags: &'d [Value], path: &str, line: u64) -> &'d Value {
    eff116(diags)
        .into_iter()
        .find(|d| d["path"] == path && d["line"] == line)
        .unwrap_or_else(|| panic!("no EFF116 at {path}:{line}"))
}

fn message_at<'d>(diags: &'d [Value], path: &str, line: u64) -> &'d str {
    report_at(diags, path, line)["message"].as_str().unwrap()
}

#[test]
fn a_generator_nothing_iterates_is_reported() {
    let p = project("eff116_shapes");
    let diags = diagnostics(&p, &["--select", "EFF116"]);
    assert_eq!(found(&diags), marked());
}

#[test]
fn a_report_names_the_function_and_what_reads_its_generator() {
    let p = project("eff116_messages");
    let diags = diagnostics(&p, &["--select", "EFF116"]);
    assert_eq!(
        message_at(&diags, "pkg/statements.py", 17),
        "the call of `Code.instructions` is a statement, and `Code.instructions` is a generator \
         function: calling it only creates a generator, so the body does not run"
    );
    assert_eq!(
        message_at(&diags, "pkg/statements.py", 34),
        "only a truth test reads what `Channel.embed_urls` returns, and `Channel.embed_urls` is \
         a generator function: a generator is always true, and the body does not run"
    );
    assert_eq!(
        message_at(&diags, "pkg/statements.py", 56),
        "the call of `stream` is a statement, and `stream` is an async generator function: \
         calling it only creates an async generator, so the body does not run"
    );
    assert_eq!(
        message_at(&diags, "pkg/handoffs.py", 23),
        "`threading.Thread` drops what `scan` returns, and `scan` is a generator function: \
         calling it only creates a generator, so the body does not run"
    );
    assert_eq!(
        message_at(&diags, "pkg/handoffs.py", 30),
        "`filter` tests what `scan` returns for truth, and `scan` is a generator function: a \
         generator is always true, and the body does not run"
    );
    assert_eq!(
        message_at(&diags, "pkg/possible.py", 27),
        "the call of `Source.rows` is a statement, and `Source.rows`, one implementation the \
         call may run, is a generator function: calling it only creates a generator, so the \
         body does not run"
    );
    assert_eq!(
        message_at(&diags, "pkg/possible.py", 28),
        "the call of `chunks` is a statement, and `chunks`, one implementation the call may \
         run, is a generator function: calling it only creates a generator, so the body does \
         not run"
    );
    let first = report_at(&diags, "pkg/statements.py", 17);
    assert_eq!(first["qualname"], "Code.instruction");
    assert_eq!(first["target"], "Code.instructions");
    let handed = report_at(&diags, "pkg/handoffs.py", 23);
    assert_eq!(handed["qualname"], "runners");
    assert_eq!(handed["target"], "threading.Thread->scan");
    // An experimental opt-in rule: heuristic until it is certified.
    assert!(eff116(&diags).iter().all(|d| d["certainty"] == "heuristic"));
}

#[test]
fn a_generator_function_that_becomes_a_plain_function_is_no_longer_reported() {
    let p = project("eff116_plain");
    p.write(
        "pkg/rows.py",
        &ROWS.replace("        yield line.strip()", "        print(line.strip())"),
    );
    let diags = diagnostics(&p, &["--select", "EFF116"]);
    let targets: Vec<&str> = eff116(&diags)
        .iter()
        .map(|d| d["target"].as_str().unwrap())
        .collect();
    assert!(!targets.is_empty());
    assert!(
        targets
            .iter()
            .all(|t| *t != "scan" && !t.ends_with("->scan")),
        "{targets:?}"
    );
}

#[test]
fn a_suppression_comment_silences_a_report() {
    let p = Project::new("eff116_noqa");
    p.write("pkg/__init__.py", "").write(
        "pkg/main.py",
        "def gen():\n    yield 1\n\n\ngen()  # heki: ignore[EFF116]\ngen()\n",
    );
    let diags = diagnostics(&p, &["--select", "EFF116"]);
    assert_eq!(found(&diags), vec![("pkg/main.py".to_owned(), 6)]);
}

#[test]
fn the_rule_is_opt_in() {
    let p = project("eff116_opt_in");
    assert!(found(&diagnostics(&p, &[])).is_empty());
    assert!(found(&diagnostics(&p, &["--select", "EFF"])).is_empty());
    assert_eq!(
        found(&diagnostics(&p, &["--select", "generator-never-iterated"])),
        marked()
    );
}

#[test]
fn heki_rule_explains_it() {
    let p = Project::new("eff116_rule");
    let (code, stdout, _) = p.heki(&["rule", "EFF116"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("generator-never-iterated"));
    assert!(stdout.contains("Never iterated"));
    assert!(stdout.contains("experimental"));
}
