//! `EFF110` (`exclusive-object-fanout`) end to end: an object that allows one operation at a
//! time used by tasks a function starts together (`asyncio.gather` of coroutines, of a
//! generator expression or a comprehension, `asyncio.wait` over tasks, `asyncio.TaskGroup`, an
//! anyio task group), through helpers and keyword arguments, for each catalogued class
//! (`asyncio.StreamReader`, asyncpg connections, websockets connections, aiohttp WebSocket
//! responses), SQLAlchemy's `AsyncSession` only where the fan-out can be its first operation;
//! the shapes the tasks do not share an object in (stdout and stderr, a reader and a writer, a
//! lock around the read, a handle per task, a task started again only once it is done); the
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

/// `(path, line, qualname, target)` of every `EFF110` diagnostic.
fn found(diags: &[Value]) -> Vec<(String, u64, String, String)> {
    diags
        .iter()
        .filter(|d| d["rule_code"] == "EFF110")
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

const SELECT: &[&str] = &["--extend-select", "EFF110"];

/// A stream reader two tasks read at once: two coroutines of a helper (one reading through
/// another helper), a generator expression and a comprehension starting one per item, tasks
/// `asyncio.wait` waits on, a task group's tasks, keyword arguments, and the reads themselves.
const STREAMS: &str = r#"import asyncio


async def read_line(reader: asyncio.StreamReader) -> bytes:
    return await reader.readline()


async def read_header(stream: asyncio.StreamReader) -> bytes:
    return await read_line(stream)


async def both(reader: asyncio.StreamReader) -> list[bytes]:
    return await asyncio.gather(read_line(reader), read_header(reader))


async def each(reader: asyncio.StreamReader, count: int) -> list[bytes]:
    return await asyncio.gather(*(read_line(reader) for _ in range(count)))


async def listed(reader: asyncio.StreamReader, count: int) -> list[bytes]:
    return await asyncio.gather(*[read_line(reader) for _ in range(count)])


async def waited(reader: asyncio.StreamReader) -> None:
    first = asyncio.create_task(read_line(reader))
    await asyncio.wait({first, asyncio.create_task(read_header(stream=reader))})


async def grouped(reader: asyncio.StreamReader, count: int) -> None:
    async with asyncio.TaskGroup() as tg:
        for _ in range(count):
            tg.create_task(read_line(reader))


async def direct(reader: asyncio.StreamReader) -> list[bytes]:
    return await asyncio.gather(reader.readline(), reader.readline())


class FrameClient:
    def __init__(self, reader: asyncio.StreamReader) -> None:
        self._reader = reader

    async def read_frame(self) -> bytes:
        return await self._reader.readexactly(4)

    async def read_frames(self, count: int) -> list[bytes]:
        return await asyncio.gather(*(self.read_frame() for _ in range(count)))

    async def read_frames_in_turn(self, count: int) -> list[bytes]:
        return [await self.read_frame() for _ in range(count)]
"#;

#[test]
fn eff110_reports_tasks_started_together_on_one_reader() {
    let p = Project::new("eff110_streams");
    p.write("streams.py", STREAMS);
    for target in ["py311", "py312"] {
        let diags = diagnostics(
            &p,
            &["--extend-select", "EFF110", "--target-version", target],
        );
        assert_eq!(
            found(&diags),
            vec![
                d(
                    "streams.py",
                    13,
                    "both",
                    "reader->asyncio.StreamReader.readline"
                ),
                d(
                    "streams.py",
                    17,
                    "each",
                    "reader->asyncio.StreamReader.readline"
                ),
                d(
                    "streams.py",
                    21,
                    "listed",
                    "reader->asyncio.StreamReader.readline"
                ),
                d(
                    "streams.py",
                    26,
                    "waited",
                    "reader->asyncio.StreamReader.readline"
                ),
                d(
                    "streams.py",
                    30,
                    "grouped",
                    "reader->asyncio.StreamReader.readline"
                ),
                d(
                    "streams.py",
                    36,
                    "direct",
                    "reader->asyncio.StreamReader.readline"
                ),
                d(
                    "streams.py",
                    47,
                    "FrameClient.read_frames",
                    "self._reader->asyncio.StreamReader.readexactly"
                ),
            ],
            "{target}"
        );
        let both = at(&diags, "streams.py", 13);
        assert_eq!(
            both["message"],
            "`both` starts tasks together that both use `reader` (`asyncio.gather`); \
             `asyncio.StreamReader` allows one operation at a time, so the second task's call \
             raises"
        );
        assert_eq!(
            related(both),
            vec![
                (
                    "streams.py".into(),
                    13,
                    "`both` calls `streams.read_line`".into()
                ),
                (
                    "streams.py".into(),
                    5,
                    "`read_line` calls `asyncio.StreamReader.readline` here".into()
                ),
                (
                    "streams.py".into(),
                    13,
                    "`both` calls `streams.read_header`".into()
                ),
                (
                    "streams.py".into(),
                    9,
                    "`read_header` calls `streams.read_line`".into()
                ),
                (
                    "streams.py".into(),
                    5,
                    "`read_line` calls `asyncio.StreamReader.readline` here".into()
                ),
            ]
        );
        assert!(
            at(&diags, "streams.py", 17)["message"]
                .as_str()
                .unwrap()
                .starts_with("`each` starts a task for every item, each using `reader`"),
        );
    }
}

/// The other catalogued classes, typed by their factories and annotations, and an anyio task
/// group starting a function twice with the same connection.
const CLASSES: &str = r#"import asyncio

import aiohttp
import anyio
import asyncpg
import websockets


async def count(conn: asyncpg.Connection, query: str) -> int:
    return await conn.fetchval(query)


async def counts(dsn: str, queries: list[str]) -> list[int]:
    conn = await asyncpg.connect(dsn)
    return await asyncio.gather(*[count(conn, q) for q in queries])


async def next_message(ws: websockets.ClientConnection) -> str:
    return await ws.recv()


async def two_messages(url: str) -> list[str]:
    async with websockets.connect(url) as ws:
        return await asyncio.gather(next_message(ws), next_message(ws))


async def receive(ws: aiohttp.ClientWebSocketResponse) -> object:
    return await ws.receive()


async def listen(session: aiohttp.ClientSession, url: str) -> None:
    async with session.ws_connect(url) as ws:
        async with anyio.create_task_group() as tg:
            tg.start_soon(receive, ws)
            tg.start_soon(receive, ws)
"#;

#[test]
fn eff110_knows_the_catalogued_classes() {
    let p = Project::new("eff110_classes");
    p.write("classes.py", CLASSES);
    assert_eq!(
        found(&diagnostics(&p, SELECT)),
        vec![
            d(
                "classes.py",
                15,
                "counts",
                "conn->asyncpg.Connection.fetchval"
            ),
            d(
                "classes.py",
                24,
                "two_messages",
                "ws->websockets.ClientConnection.recv"
            ),
            d(
                "classes.py",
                33,
                "listen",
                "ws->aiohttp.ClientWebSocketResponse.receive"
            ),
        ]
    );
}

/// An `AsyncSession` raises only while it holds no connection: reported where the fan-out can
/// be its first operation since it was made or committed, not after an operation on every
/// path.
const SESSIONS: &str = r#"import asyncio

from sqlalchemy.ext.asyncio import AsyncSession, async_sessionmaker

Session = async_sessionmaker()


async def load_user(session: AsyncSession, uid: int) -> object:
    return await session.get(object, uid)


async def load_items(session: AsyncSession, uid: int) -> object:
    return (await session.execute("select")).all()


async def first(uid: int) -> object:
    async with Session() as session:
        return await asyncio.gather(load_user(session, uid), load_items(session, uid))


async def after_query(uid: int) -> object:
    async with Session() as session:
        await session.execute("select 1")
        return await asyncio.gather(load_user(session, uid), load_items(session, uid))


async def after_helper(session: AsyncSession, uid: int) -> object:
    await load_user(session, uid)
    return await asyncio.gather(load_user(session, uid), load_items(session, uid))


async def after_commit(uid: int) -> object:
    async with Session() as session:
        await session.execute("select 1")
        await session.commit()
        return await asyncio.gather(load_user(session, uid), load_items(session, uid))


async def given(session: AsyncSession, uid: int) -> object:
    return await asyncio.gather(load_user(session, uid), load_items(session, uid))
"#;

#[test]
fn eff110_reports_an_async_session_only_where_it_may_be_unbound() {
    let p = Project::new("eff110_sessions");
    p.write("sessions.py", SESSIONS);
    let target = "session->sqlalchemy.ext.asyncio.AsyncSession.get";
    assert_eq!(
        found(&diagnostics(&p, SELECT)),
        vec![
            d("sessions.py", 18, "first", target),
            d("sessions.py", 36, "after_commit", target),
            d("sessions.py", 40, "given", target),
        ]
    );
}

/// None of these shares one object between tasks started together: stdout and stderr of a
/// process, each task's own reader and writer, reads under a lock, a handle per task, a task
/// started again only once the last one is done, a session per task, and a task that binds
/// the attribute it reads to a new reader first.
const SEPARATE: &str = r#"import asyncio

from sqlalchemy.ext.asyncio import AsyncSession, async_sessionmaker

Session = async_sessionmaker()


async def drain(stream: asyncio.StreamReader) -> bytes:
    return await stream.read()


async def run(cmd: str) -> list[bytes]:
    proc = await asyncio.create_subprocess_shell(cmd)
    return await asyncio.gather(drain(proc.stdout), drain(proc.stderr))


async def pump(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
    writer.write(await reader.read(1024))


async def relay(
    client_reader: asyncio.StreamReader,
    client_writer: asyncio.StreamWriter,
    backend_reader: asyncio.StreamReader,
    backend_writer: asyncio.StreamWriter,
) -> None:
    await asyncio.gather(
        pump(client_reader, backend_writer), pump(backend_reader, client_writer)
    )


class Locked:
    def __init__(self, reader: asyncio.StreamReader) -> None:
        self.lock = asyncio.Lock()
        self.reader = reader

    async def _read(self) -> bytes:
        return await self.reader.readline()

    async def guarded(self) -> bytes:
        async with self.lock:
            return await self.reader.readline()

    async def guarded_call(self) -> bytes:
        async with self.lock:
            return await self._read()

    async def two(self) -> list[bytes]:
        return await asyncio.gather(self.guarded(), self.guarded_call())


async def each(streams: list[asyncio.StreamReader]) -> list[bytes]:
    return await asyncio.gather(*(drain(s) for s in streams))


async def grouped(streams: list[asyncio.StreamReader]) -> None:
    async with asyncio.TaskGroup() as tg:
        for s in streams:
            tg.create_task(drain(s))


async def rearmed(reader: asyncio.StreamReader, stop: asyncio.Event) -> None:
    read_task = asyncio.create_task(reader.readline())
    stop_task = asyncio.create_task(stop.wait())
    while True:
        done, _ = await asyncio.wait({read_task, stop_task})
        if stop_task in done:
            return
        read_task = asyncio.create_task(reader.readline())


async def per_task(uids: list[int]) -> list[object]:
    async def one(uid: int) -> object:
        async with Session() as session:
            return await session.get(object, uid)

    return await asyncio.gather(*(one(u) for u in uids))


async def alone(reader: asyncio.StreamReader) -> list[bytes]:
    return await asyncio.gather(drain(reader))


class Reconnecting:
    def __init__(self, reader: asyncio.StreamReader) -> None:
        self.reader = reader

    async def fresh_line(self, host: str) -> bytes:
        self.reader, _ = await asyncio.open_connection(host, 80)
        return await self.reader.readline()

    async def both(self) -> list[bytes]:
        return await asyncio.gather(self.fresh_line("a"), self.fresh_line("b"))
"#;

#[test]
fn eff110_leaves_tasks_that_share_nothing() {
    let p = Project::new("eff110_separate");
    p.write("separate.py", SEPARATE);
    for target in ["py311", "py312"] {
        assert_eq!(
            found(&diagnostics(
                &p,
                &["--extend-select", "EFF110", "--target-version", target]
            )),
            vec![],
            "{target}"
        );
    }
}

/// An attribute of a local variable is one object for both tasks.
#[test]
fn eff110_reports_one_attribute_of_a_local_used_twice() {
    let p = Project::new("eff110_local_attr");
    p.write(
        "proc.py",
        r#"import asyncio


async def drain(stream: asyncio.StreamReader) -> bytes:
    return await stream.read()


async def run(cmd: str) -> list[bytes]:
    proc = await asyncio.create_subprocess_shell(cmd)
    return await asyncio.gather(drain(proc.stdout), drain(proc.stdout))
"#,
    );
    assert_eq!(
        found(&diagnostics(&p, SELECT)),
        vec![d(
            "proc.py",
            10,
            "run",
            "proc.stdout->asyncio.StreamReader.read"
        )]
    );
}

#[test]
fn eff110_takes_methods_from_the_configuration() {
    let p = Project::new("eff110_config");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"EFF110\"]\n\n\
         [[tool.heki.ecosystem]]\nsymbol = \"vendor.wire.Channel.recv\"\none-at-a-time = \"always\"\n",
    )
    .write(
        "app.py",
        r#"import asyncio

from vendor.wire import Channel


async def take(channel: Channel) -> bytes:
    return await channel.recv()


async def two(channel: Channel) -> list[bytes]:
    return await asyncio.gather(take(channel), take(channel))
"#,
    );
    assert_eq!(
        found(&diagnostics(&p, &[])),
        vec![d("app.py", 11, "two", "channel->vendor.wire.Channel.recv")]
    );
    // A pool raising only while unbound, and the call giving its connection back.
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"EFF110\"]\n\n\
         [[tool.heki.ecosystem]]\nsymbol = \"vendor.wire.Channel.recv\"\none-at-a-time = \"when_unbound\"\n\n\
         [[tool.heki.ecosystem]]\nsymbol = \"vendor.wire.Channel.reset\"\none-at-a-time = \"unbinds\"\n",
    );
    assert_eq!(
        found(&diagnostics(&p, &[])),
        vec![d("app.py", 11, "two", "channel->vendor.wire.Channel.recv")]
    );
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.ecosystem]]\nsymbol = \"vendor.wire.Channel.recv\"\n\
         one-at-a-time = \"sometimes\"\n",
    );
    let (code, _, stderr) = p.check(&[]);
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("invalid one-at-a-time `sometimes`"),
        "{stderr}"
    );
}

#[test]
fn eff110_selection_and_rule_page() {
    let p = Project::new("eff110_selection");
    p.write("streams.py", STREAMS);
    assert!(found(&diagnostics(&p, &[])).is_empty());
    assert!(found(&diagnostics(&p, &["--select", "EFF"])).is_empty());
    let named = diagnostics(&p, &["--select", "exclusive-object-fanout"]);
    assert_eq!(found(&named).len(), 7);
    assert_eq!(named[0]["rule_slug"], "exclusive-object-fanout");
    let (code, doc, _) = p.heki(&["rule", "EFF110"]);
    assert_eq!(code, 0);
    assert!(
        doc.starts_with("# exclusive-object-fanout (EFF110)\n\n"),
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

/// More ways to start and to name: a module global (also read by a generator expression), an
/// attribute of `self` a generator expression reads, `async for`, bound methods an anyio task
/// group starts, two paths of different depth to one read, and tasks appended to a list in a
/// loop are reported. Not reported: an attribute of a global object (two names deep), tasks
/// handed in or stored, a coroutine from a plain function, arguments after `*` unpacking, a
/// session used before the fan-out through a method, a fan-out no run reaches, a process's
/// stdout and stderr appended under tests, and a list gathered in the loop that fills it.
const SHAPES: &str = r#"import asyncio

import anyio
from sqlalchemy.ext.asyncio import AsyncSession

_READER: asyncio.StreamReader = asyncio.StreamReader()


class Client:
    def __init__(self, reader: asyncio.StreamReader) -> None:
        self.reader = reader
        self.task_a = asyncio.ensure_future(self.line())
        self.task_b = asyncio.ensure_future(self.line())

    async def line(self) -> bytes:
        return await self.reader.readline()

    async def per_item(self) -> list[bytes]:
        return await asyncio.gather(*(drain(self.reader) for _ in range(3)))

    async def soon(self) -> None:
        async with anyio.create_task_group() as tg:
            tg.start_soon(self.line)
            tg.start_soon(self.line)

    async def stored(self) -> list[bytes]:
        return await asyncio.gather(self.task_a, self.task_b)

    async def prepare(self, session: AsyncSession) -> None:
        await session.execute("select 1")

    async def prepared(self, session: AsyncSession) -> object:
        await self.prepare(session)
        return await asyncio.gather(load(session), load(session))


_CLIENT = Client(_READER)


async def drain(stream: asyncio.StreamReader) -> bytes:
    return await stream.read()


async def read_global() -> bytes:
    return await _READER.readline()


async def lines(reader: asyncio.StreamReader) -> None:
    async for line in reader:
        print(line)


async def mid(reader: asyncio.StreamReader) -> bytes:
    return await reader.readline()


async def deep(reader: asyncio.StreamReader) -> bytes:
    return await mid(reader)


async def both_paths(reader: asyncio.StreamReader) -> bytes:
    await deep(reader)
    return await mid(reader)


async def via(client: Client) -> bytes:
    return await client.reader.readline()


async def load(session: AsyncSession) -> object:
    return await session.get(object, 1)


def make(reader: asyncio.StreamReader):
    return drain(reader)


async def globals_together() -> list[bytes]:
    return await asyncio.gather(read_global(), read_global())


async def globals_per_item() -> list[bytes]:
    return await asyncio.gather(*(drain(_READER) for _ in range(3)))


async def iterated(reader: asyncio.StreamReader) -> None:
    await asyncio.gather(lines(reader), lines(reader))


async def two_paths(reader: asyncio.StreamReader) -> None:
    await asyncio.gather(both_paths(reader), both_paths(reader))


async def through_global_client() -> None:
    await asyncio.gather(via(_CLIENT), via(_CLIENT))


async def given_tasks(tasks: set[asyncio.Task]) -> None:
    await asyncio.wait(tasks)


async def with_sleep(reader: asyncio.StreamReader) -> None:
    await asyncio.gather(asyncio.sleep(1), drain(reader))


async def handed(callback, pending) -> None:
    async with anyio.create_task_group() as tg:
        tg.start_soon(callback)
        tg.start_soon(callback)
    async with asyncio.TaskGroup() as group:
        group.create_task(pending)
        group.create_task(pending)


async def made(reader: asyncio.StreamReader) -> None:
    await asyncio.gather(make(reader), make(reader))


async def unpacked(streams: list[asyncio.StreamReader]) -> None:
    await asyncio.gather(drain(*streams), drain(*streams))


async def never(reader: asyncio.StreamReader) -> None:
    if False:
        await asyncio.gather(drain(reader), drain(reader))


async def piped(proc: asyncio.subprocess.Process) -> None:
    tasks = []
    if proc.stdout:
        tasks.append(drain(proc.stdout))
    if proc.stderr:
        tasks.append(drain(proc.stderr))
    await asyncio.gather(*tasks)


async def looped_appends(reader: asyncio.StreamReader, n: int) -> None:
    tasks = []
    for _ in range(n):
        tasks.append(drain(reader))
    await asyncio.gather(*tasks)


async def batch_per_round(readers: list[asyncio.StreamReader], reader: asyncio.StreamReader) -> None:
    for _ in readers:
        tasks = []
        tasks.append(drain(reader))
        await asyncio.gather(*tasks)


async def early(reader: asyncio.StreamReader, lock: asyncio.Lock) -> bytes:
    async with lock:
        pass
    return b""
    return await drain(reader)


if _READER.at_eof():
    async def twice(reader: asyncio.StreamReader) -> bytes:
        return await reader.readline()
else:
    async def twice(reader: asyncio.StreamReader) -> bytes:
        return await reader.read()


async def defined_twice(reader: asyncio.StreamReader) -> None:
    await asyncio.gather(twice(reader), twice(reader))


async def two_operations(session: AsyncSession) -> None:
    await session.get(object, 1)
    await session.execute("select 1")
"#;

#[test]
fn eff110_follows_more_ways_to_start_and_name_tasks() {
    let p = Project::new("eff110_shapes");
    p.write("shapes.py", SHAPES);
    let read = "asyncio.StreamReader.read";
    let readline = "asyncio.StreamReader.readline";
    assert_eq!(
        found(&diagnostics(&p, SELECT)),
        vec![
            d(
                "shapes.py",
                19,
                "Client.per_item",
                &format!("self.reader->{read}")
            ),
            d(
                "shapes.py",
                22,
                "Client.soon",
                &format!("self.reader->{readline}")
            ),
            d(
                "shapes.py",
                79,
                "globals_together",
                &format!("shapes._READER->{readline}")
            ),
            d(
                "shapes.py",
                83,
                "globals_per_item",
                &format!("shapes._READER->{read}")
            ),
            d(
                "shapes.py",
                87,
                "iterated",
                "reader->asyncio.StreamReader.__anext__"
            ),
            d("shapes.py", 91, "two_paths", &format!("reader->{readline}")),
            d(
                "shapes.py",
                141,
                "looped_appends",
                &format!("reader->{read}")
            ),
            d(
                "shapes.py",
                167,
                "defined_twice",
                &format!("reader->{readline}")
            ),
        ]
    );
}
