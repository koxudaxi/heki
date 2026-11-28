//! The Layer 2 ecosystem catalog end to end, one library family per test: the synchronous APIs
//! reached from an `async def` (directly and through a helper) and the queries and clients
//! created in loops are reported; the async counterparts (awaited directly or not), clients
//! built outside loops, offloaded calls, and APIs that do no I/O are not. The libraries are not
//! installed: heki resolves imports by name.

mod common;

use common::Project;
use serde_json::Value;

/// The family-2 rules the catalog feeds (the opt-in ones are selected where a test needs them).
const RULES: &str = "EFF101,EFF102,EFF103,DATA101";

/// `(line, rule, qualname, target)` of every diagnostic of `heki check` on the one-file project
/// `rel` holding `source`.
fn check(name: &str, rel: &str, source: &str, rules: &str) -> Vec<(u64, String, String, String)> {
    let p = Project::new(name);
    p.write(rel, source);
    let (code, stdout, stderr) = p.check(&["--output-format", "json", "--select", rules]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    diags
        .iter()
        .map(|d| {
            assert_eq!(d["path"], rel, "{d:#}");
            (
                d["line"].as_u64().unwrap(),
                common::tiered_code(d).to_owned(),
                d["qualname"].as_str().unwrap().to_owned(),
                d["target"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// [`check`] of `source` at `rel` with the `[tool.heki]` table `config` in `pyproject.toml`.
fn check_configured(
    name: &str,
    config: &str,
    rel: &str,
    source: &str,
    rules: &str,
) -> Vec<(u64, String, String, String)> {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!("[project]\nname = \"t\"\n\n{config}"),
    );
    p.write(rel, source);
    let (code, stdout, stderr) = p.check(&["--output-format", "json", "--select", rules]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    diags
        .iter()
        .map(|d| {
            (
                d["line"].as_u64().unwrap(),
                common::tiered_code(d).to_owned(),
                d["qualname"].as_str().unwrap().to_owned(),
                d["target"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn d(line: u64, rule: &str, qualname: &str, target: &str) -> (u64, String, String, String) {
    (line, rule.into(), qualname.into(), target.into())
}

const HTTP: &str = r#"import asyncio

import aiohttp
import httpx
import requests
import urllib3

http = urllib3.PoolManager()


def download(url):
    return requests.get(url)


def fetch(url):
    return httpx.get(url)


async def tp_requests(url):
    download(url)
    with requests.Session() as session:
        session.options(url)


async def tp_httpx(url):
    fetch(url)
    with httpx.Client() as client:
        client.head(url)
    with httpx.stream("GET", url) as response:
        response.read()


async def tp_urllib3(url):
    http.request("GET", url)
    urllib3.request("GET", url)
    urllib3.HTTPConnectionPool("example.com").urlopen("GET", "/")


def tp_clients_in_loops(urls):
    for u in urls:
        requests.session().get(u)
        httpx.Client().get(u)
        urllib3.PoolManager().request("GET", u)


async def tp_async_clients_in_loops(urls):
    for u in urls:
        async with httpx.AsyncClient() as client:
            await client.get(u)
        async with aiohttp.ClientSession() as session:
            await session.get(u)
        async with aiohttp.request("GET", u) as resp:
            await resp.text()


async def tn_httpx_async(url, urls):
    async with httpx.AsyncClient() as client:
        await client.get(url)
        await client.head(url)
        await asyncio.wait_for(client.post(url), 5)
        await asyncio.gather(*[client.get(u) for u in urls])
        async with client.stream("GET", url) as response:
            await response.aread()
        for u in urls:
            await client.get(u)


async def tn_aiohttp(url):
    async with aiohttp.ClientSession() as session:
        async with session.get(url) as resp:
            await resp.text()
        async with session.ws_connect(url) as ws:
            await ws.receive()
        await session.post(url)


def tn_clients_reused(urls):
    with requests.Session() as session, httpx.Client() as client:
        for u in urls:
            session.get(u)
            client.get(u)
"#;

#[test]
fn http_clients_block_only_through_their_sync_apis() {
    assert_eq!(
        check("catalog_http", "app/http.py", HTTP, RULES),
        vec![
            d(20, "EFF101", "tp_requests", "requests.get"),
            d(22, "EFF101", "tp_requests", "requests.Session.options"),
            d(26, "EFF101", "tp_httpx", "httpx.get"),
            d(28, "EFF101", "tp_httpx", "httpx.Client.head"),
            d(29, "EFF101", "tp_httpx", "httpx.stream"),
            d(34, "EFF101", "tp_urllib3", "urllib3.PoolManager.request"),
            d(35, "EFF101", "tp_urllib3", "urllib3.request"),
            d(
                36,
                "EFF101",
                "tp_urllib3",
                "urllib3.HTTPConnectionPool.urlopen"
            ),
            d(41, "EFF102", "tp_clients_in_loops", "requests.session"),
            d(42, "EFF102", "tp_clients_in_loops", "httpx.Client"),
            d(43, "EFF102", "tp_clients_in_loops", "urllib3.PoolManager"),
            d(
                48,
                "EFF102",
                "tp_async_clients_in_loops",
                "httpx.AsyncClient"
            ),
            d(
                50,
                "EFF102",
                "tp_async_clients_in_loops",
                "aiohttp.ClientSession"
            ),
            d(52, "EFF102", "tp_async_clients_in_loops", "aiohttp.request"),
        ]
    );
}

const OFFLOAD: &str = r#"import asyncio
import functools

import anyio
import anyio.to_process
import anyio.to_thread
import fastapi.concurrency
import requests
import trio
from starlette.concurrency import run_in_threadpool


def fetch(url):
    return requests.get(url)


async def tn_offloaded(url):
    loop = asyncio.get_running_loop()
    await asyncio.to_thread(fetch, url)
    await asyncio.to_thread(requests.get, url)
    await loop.run_in_executor(None, fetch, url)
    await loop.run_in_executor(None, functools.partial(fetch, url))
    await anyio.to_thread.run_sync(fetch, url)
    await anyio.to_process.run_sync(fetch, url)
    await run_in_threadpool(fetch, url)
    await fastapi.concurrency.run_in_threadpool(fetch, url)
    await trio.to_thread.run_sync(fetch, url)


async def in_thread(fn, *args):
    return await anyio.to_thread.run_sync(fn, *args)


async def tn_through_a_wrapper(url):
    return await in_thread(fetch, url)


async def tp_not_offloaded(url):
    return fetch(url)
"#;

#[test]
fn offload_wrappers_run_the_callable_off_the_event_loop() {
    assert_eq!(
        check("catalog_offload", "app/offload.py", OFFLOAD, RULES),
        // Every offload API (and a project wrapper around one) takes the callable off the
        // loop: only the direct call is reported.
        vec![d(39, "EFF101", "tp_not_offloaded", "requests.get")]
    );
}

const SQLALCHEMY: &str = r#"import asyncio

import requests
import sqlalchemy
from sqlalchemy import create_engine, select, text
from sqlalchemy.ext.asyncio import AsyncSession, async_sessionmaker, create_async_engine
from sqlalchemy.orm import Session, sessionmaker
from sqlmodel import Session as ModelSession
from sqlmodel import create_engine as create_model_engine

engine = create_engine("postgresql://db")
SessionLocal = sessionmaker(bind=engine)
async_engine = create_async_engine("postgresql+asyncpg://db")
AsyncSessionLocal = async_sessionmaker(async_engine, expire_on_commit=False)
LegacyAsyncSession = sessionmaker(async_engine, class_=AsyncSession)


def load(session: Session, model):
    return session.scalars(select(model)).all()


async def tn_engines_connect_lazily(url):
    sqlalchemy.create_engine(url)
    create_async_engine(url)
    create_model_engine(url)


async def tp_engine(stmt):
    with engine.connect() as conn:
        conn.execute(stmt)
    with engine.begin() as conn:
        conn.exec_driver_sql("SELECT 1")


async def tp_session(session: Session, model):
    load(session, model)
    with ModelSession(engine) as model_session:
        model_session.exec(select(model))


def tp_queries_in_loops(session: Session, model, ids):
    for i in ids:
        session.get(model, i)
        session.refresh(i)


async def tn_async_session(model, stmt):
    async with AsyncSessionLocal() as session:
        await session.execute(stmt)
        await session.get(model, 1)
        await asyncio.wait_for(session.scalars(stmt), 5)
        await session.commit()
    async with AsyncSessionLocal.begin() as session:
        await session.execute(stmt)
    async with async_engine.connect() as conn:
        await conn.execute(stmt)
        async with conn.stream(stmt) as result:
            await result.fetchall()
    async with async_engine.begin() as conn:
        await conn.execute(stmt)
    await AsyncSession(async_engine).refresh(model)


async def tn_sessionmaker_with_async_class(stmt):
    # `sessionmaker()` sessions stay untyped: `class_=AsyncSession` makes them async.
    async with LegacyAsyncSession() as session:
        await asyncio.wait_for(session.execute(stmt), 5)


async def tp_async_queries_and_engines_in_loops(model, ids, urls):
    async with AsyncSessionLocal() as session:
        for i in ids:
            await session.get(model, i)
    async with async_engine.connect() as conn:
        for i in ids:
            await conn.execute(text("SELECT 1"), {"i": i})
    for u in urls:
        create_async_engine(u)


def tp_transactions_held_across_io(url):
    with SessionLocal.begin() as session:
        requests.post(url)
    with engine.begin() as conn:
        requests.post(url)


async def tp_savepoints(conn: sqlalchemy.engine.Connection, session: Session, model):
    with conn.begin_nested():
        pass
    with session.begin_nested():
        session.add(model)


async def tn_begin_sends_nothing(session: Session):
    with session.begin():
        pass
"#;

#[test]
fn sqlalchemy_sync_apis_block_and_async_ones_only_count_queries() {
    assert_eq!(
        check("catalog_sqlalchemy", "app/db.py", SQLALCHEMY, RULES),
        vec![
            // `Engine.connect()` / `begin()` connect; `begin()` types its connection.
            d(
                29,
                "EFF101 [heuristic]",
                "tp_engine",
                "sqlalchemy.engine.Engine.connect"
            ),
            d(
                30,
                "EFF101",
                "tp_engine",
                "sqlalchemy.engine.Connection.execute"
            ),
            d(
                31,
                "EFF101 [heuristic]",
                "tp_engine",
                "sqlalchemy.engine.Engine.begin"
            ),
            d(
                32,
                "EFF101",
                "tp_engine",
                "sqlalchemy.engine.Connection.exec_driver_sql"
            ),
            // Through a helper, and a SQLModel session.
            d(36, "EFF101", "tp_session", "sqlalchemy.orm.Session.scalars"),
            d(38, "EFF101", "tp_session", "sqlmodel.Session.exec"),
            d(
                43,
                "EFF103 [heuristic]",
                "tp_queries_in_loops",
                "sqlalchemy.orm.Session.get"
            ),
            d(
                44,
                "EFF103",
                "tp_queries_in_loops",
                "sqlalchemy.orm.Session.refresh"
            ),
            // Awaited async queries still count in loops, and so do new engines.
            d(
                73,
                "EFF103 [heuristic]",
                "tp_async_queries_and_engines_in_loops",
                "sqlalchemy.ext.asyncio.AsyncSession.get"
            ),
            d(
                76,
                "EFF103",
                "tp_async_queries_and_engines_in_loops",
                "sqlalchemy.ext.asyncio.AsyncConnection.execute"
            ),
            d(
                78,
                "EFF102",
                "tp_async_queries_and_engines_in_loops",
                "sqlalchemy.ext.asyncio.create_async_engine"
            ),
            // `SAVEPOINT` is sent at the call; the session flushes its pending changes first.
            d(
                89,
                "EFF101",
                "tp_savepoints",
                "sqlalchemy.engine.Connection.begin_nested"
            ),
            d(
                91,
                "EFF101",
                "tp_savepoints",
                "sqlalchemy.orm.Session.begin_nested"
            ),
        ]
    );
    // `EFF106` (opt-in): a sync transaction from `sessionmaker.begin()` / `Engine.begin()`.
    assert_eq!(
        check("catalog_sqlalchemy_tx", "app/db.py", SQLALCHEMY, "EFF106"),
        vec![
            d(
                83,
                "EFF106",
                "tp_transactions_held_across_io",
                "sqlalchemy.orm.sessionmaker.begin->requests.post"
            ),
            d(
                85,
                "EFF106",
                "tp_transactions_held_across_io",
                "sqlalchemy.engine.Engine.begin->requests.post"
            ),
        ]
    );
}

const DRIVERS: &str = r#"import asyncio
import sqlite3

import psycopg
import psycopg2
import pymysql
from psycopg_pool import AsyncConnectionPool, ConnectionPool

pool = ConnectionPool("dbname=app")
async_pool = AsyncConnectionPool("dbname=app")


def run(conn: psycopg.Connection, sql):
    conn.execute(sql)


def count(conn: sqlite3.Connection):
    return conn.execute("SELECT count(*) FROM t").fetchone()


async def tp_psycopg(dsn, conn: psycopg.Connection):
    run(conn, "SELECT 1")
    with psycopg.connect(dsn) as fresh:
        cur = fresh.cursor()
        cur.executemany("INSERT INTO t VALUES (%s)", [(1,)])
        fresh.commit()
    with pool.connection() as pooled:
        pooled.execute("SELECT 1")


async def tp_psycopg2_and_pymysql(dsn):
    pg = psycopg2.connect(dsn)
    pg.cursor().execute("SELECT 1")
    my = pymysql.connect(host="db")
    with my.cursor() as cur:
        cur.execute("SELECT 1")
    my.commit()


async def tp_sqlite(path, conn: sqlite3.Connection):
    count(conn)
    cur = sqlite3.connect("app.db").execute("SELECT 1")
    cur.fetchall()


def tp_queries_in_loops(conn: psycopg.Connection, my: pymysql.connections.Connection, lite: sqlite3.Connection, ids):
    cur = conn.cursor()
    with my.cursor() as my_cur:
        for i in ids:
            cur.execute("SELECT * FROM t WHERE id = %s", (i,))
            my_cur.execute("SELECT * FROM t WHERE id = %s", (i,))
            lite.execute("SELECT * FROM t WHERE id = ?", (i,)).fetchone()


def tn_fetch_loops(cur: sqlite3.Cursor):
    cur.execute("SELECT * FROM t")
    while True:
        row = cur.fetchone()
        if row is None:
            break


async def tn_client_side_rows(conn: psycopg.Connection):
    cur = conn.cursor()
    await asyncio.to_thread(cur.execute, "SELECT 1")
    return cur.fetchall()


async def tn_async_psycopg(dsn):
    async with await psycopg.AsyncConnection.connect(dsn) as conn:
        await conn.execute("SELECT 1")
        cur = conn.cursor()
        await cur.execute("SELECT 2")
        await cur.fetchall()
        async with conn.transaction():
            await conn.execute("SELECT 3")
    async with async_pool.connection() as pooled:
        await pooled.execute("SELECT 4")


async def tp_async_queries_and_connections_in_loops(dsn, ids):
    conn = await psycopg.AsyncConnection.connect(dsn)
    for i in ids:
        await conn.execute("SELECT * FROM t WHERE id = %s", (i,))
    for d in ids:
        await psycopg.AsyncConnection.connect(d)


def tp_pools_in_loops(dsns):
    for dsn in dsns:
        with ConnectionPool(dsn) as per_dsn:
            per_dsn.check()


async def tp_async_pools_in_loops(dsns):
    for dsn in dsns:
        async with AsyncConnectionPool(dsn) as per_dsn:
            await per_dsn.check()


def tp_cursors_from_execute_in_loops(conn: psycopg.Connection, ids):
    cur = conn.execute("SELECT 1")
    for i in ids:
        cur.execute("SELECT %s", (i,))
        cur.execute("SELECT %s", (i,)).execute("SELECT 2")


async def tp_async_cursors_from_execute_in_loops(conn: psycopg.AsyncConnection, ids):
    cur = await conn.execute("SELECT 1")
    for i in ids:
        await cur.execute("SELECT %s", (i,))


async def tp_named_cursors_fetch_from_the_server(conn: psycopg.Connection, name):
    with conn.cursor("export") as cur:
        cur.execute("SELECT 1")
    named = conn.cursor(name="export")
    named.fetchall()
    for row in named:
        pass
    conn.cursor(name).fetchall()


async def tn_async_named_cursors(conn: psycopg.AsyncConnection):
    cur = conn.cursor(name="export")
    await cur.execute("SELECT 1")
    await cur.fetchall()
"#;

#[test]
fn dbapi_drivers_block_on_connect_and_statements_but_not_on_row_reads() {
    assert_eq!(
        check("catalog_drivers", "app/sql.py", DRIVERS, RULES),
        vec![
            // psycopg: through a helper, the connection, its statements, and a pool.
            d(22, "EFF101", "tp_psycopg", "psycopg.Connection.execute"),
            d(23, "EFF101", "tp_psycopg", "psycopg.connect"),
            d(25, "EFF101", "tp_psycopg", "psycopg.Cursor.executemany"),
            d(
                26,
                "EFF101 [heuristic]",
                "tp_psycopg",
                "psycopg.Connection.commit"
            ),
            d(
                27,
                "EFF101 [heuristic]",
                "tp_psycopg",
                "psycopg_pool.ConnectionPool.connection"
            ),
            d(28, "EFF101", "tp_psycopg", "psycopg.Connection.execute"),
            d(32, "EFF101", "tp_psycopg2_and_pymysql", "psycopg2.connect"),
            d(
                33,
                "EFF101",
                "tp_psycopg2_and_pymysql",
                "psycopg2.extensions.cursor.execute"
            ),
            d(34, "EFF101", "tp_psycopg2_and_pymysql", "pymysql.connect"),
            d(
                36,
                "EFF101",
                "tp_psycopg2_and_pymysql",
                "pymysql.cursors.Cursor.execute"
            ),
            d(
                37,
                "EFF101",
                "tp_psycopg2_and_pymysql",
                "pymysql.connections.Connection.commit"
            ),
            // sqlite3 reads the database file when it steps a statement (`fetch*`).
            d(
                41,
                "EFF101 [heuristic]",
                "tp_sqlite",
                "sqlite3.Cursor.fetchone"
            ),
            d(42, "EFF101", "tp_sqlite", "sqlite3.Connection.execute"),
            d(42, "EFF101", "tp_sqlite", "sqlite3.connect"),
            d(43, "EFF101", "tp_sqlite", "sqlite3.Cursor.fetchall"),
            // One query per iteration; the `fetchone()` of the executed statement is not one.
            d(
                50,
                "EFF103",
                "tp_queries_in_loops",
                "psycopg.Cursor.execute"
            ),
            d(
                51,
                "EFF103",
                "tp_queries_in_loops",
                "pymysql.cursors.Cursor.execute"
            ),
            d(
                52,
                "EFF103",
                "tp_queries_in_loops",
                "sqlite3.Connection.execute"
            ),
            d(
                84,
                "EFF103",
                "tp_async_queries_and_connections_in_loops",
                "psycopg.AsyncConnection.execute"
            ),
            d(
                86,
                "EFF102",
                "tp_async_queries_and_connections_in_loops",
                "psycopg.AsyncConnection.connect"
            ),
            // A pool per iteration (the module-level pools are built once).
            d(
                91,
                "EFF102",
                "tp_pools_in_loops",
                "psycopg_pool.ConnectionPool"
            ),
            d(
                97,
                "EFF102",
                "tp_async_pools_in_loops",
                "psycopg_pool.AsyncConnectionPool"
            ),
            // `execute()` returns the cursor holding the result.
            d(
                104,
                "EFF103",
                "tp_cursors_from_execute_in_loops",
                "psycopg.Cursor.execute"
            ),
            // Both statements of the chain.
            d(
                105,
                "EFF103",
                "tp_cursors_from_execute_in_loops",
                "psycopg.Cursor.execute"
            ),
            d(
                105,
                "EFF103",
                "tp_cursors_from_execute_in_loops",
                "psycopg.Cursor.execute"
            ),
            d(
                111,
                "EFF103",
                "tp_async_cursors_from_execute_in_loops",
                "psycopg.AsyncCursor.execute"
            ),
            // A named cursor is a server-side one: its fetches block, an unknown name does
            // not make one.
            d(
                116,
                "EFF101",
                "tp_named_cursors_fetch_from_the_server",
                "psycopg.ServerCursor.execute"
            ),
            d(
                118,
                "EFF101",
                "tp_named_cursors_fetch_from_the_server",
                "psycopg.ServerCursor.fetchall"
            ),
            d(
                119,
                "EFF101 [heuristic]",
                "tp_named_cursors_fetch_from_the_server",
                "psycopg.ServerCursor.__iter__"
            ),
        ]
    );
}

const KEY_VALUE: &str = r#"import asyncio

import pymongo
import redis
import redis.asyncio as aioredis
from pymongo import AsyncMongoClient, MongoClient
from pymongo.collection import Collection


def cached(r: redis.Redis, key):
    return r.get(key)


def lookup(coll: Collection, key):
    return coll.find_one({"_id": key})


def build(r: redis.Redis):
    return r.pipeline()


async def tn_clients_connect_lazily(url):
    r = redis.Redis()
    redis.from_url(url)
    MongoClient(url)
    pipe = build(r)
    await asyncio.to_thread(r.get, "k")
    await asyncio.to_thread(pipe.execute)


async def tp_redis(r: redis.Redis, url):
    cached(r, "k")
    redis.from_url(url).set("k", 1)
    pipe = r.pipeline()
    pipe.set("a", 1)
    pipe.execute()


async def tp_mongo(coll: Collection, url):
    lookup(coll, 1)
    users = MongoClient(url).get_database("app").get_collection("users")
    users.insert_many([{"a": 1}])
    pymongo.MongoClient(url).get_default_database().get_collection("x").count_documents({})


def tp_queries_and_clients_in_loops(r: redis.Redis, coll: Collection, keys, urls):
    for k in keys:
        r.hgetall(k)
        coll.update_many({"_id": k}, {"$set": {"a": 1}})
    for u in urls:
        redis.Redis.from_url(u)
        MongoClient(u)


async def tn_async_clients(url):
    r = aioredis.from_url(url)
    await r.get("k")
    await asyncio.gather(r.get("a"), r.get("b"))
    async with r.pipeline() as pipe:
        pipe.set("a", 1)
        await pipe.execute()
    await aioredis.Redis.from_url(url).publish("c", "m")
    coll = AsyncMongoClient(url).get_database("app").get_collection("users")
    await coll.find_one({})
    await asyncio.wait_for(coll.insert_one({}), 5)
    async for doc in coll.find({}):
        pass


async def tp_async_queries_and_clients_in_loops(url, keys):
    r = aioredis.Redis.from_url(url)
    coll = AsyncMongoClient(url).get_database("app").get_collection("users")
    for k in keys:
        await r.get(k)
        await coll.find_one({"_id": k})
        aioredis.Redis()


def tn_scan_cursor_loop(r: redis.Redis):
    # `SCAN` is iterated with its cursor by design: no N+1 query.
    cursor = 0
    while True:
        cursor, keys = r.scan(cursor)
        if cursor == 0:
            break


async def tp_scan_blocks(r: redis.Redis):
    return r.scan(0)


async def tp_pipeline_chains(r: redis.Redis):
    r.pipeline().set("k", "v").incr("n").execute()


async def tp_watching_pipeline(r: redis.Redis):
    with r.pipeline() as pipe:
        pipe.watch("k")
        pipe.multi()
        pipe.set("k", "v")
        pipe.execute()


async def tn_async_pipelines(r: aioredis.Redis):
    await r.pipeline().set("k", "v").execute()
    async with r.pipeline() as pipe:
        await pipe.watch("k")
        pipe.multi()
        pipe.set("k", "v")
        await pipe.execute()
"#;

#[test]
fn redis_and_pymongo_block_on_commands_not_on_clients_or_pipelines() {
    assert_eq!(
        check("catalog_kv", "app/kv.py", KEY_VALUE, RULES),
        vec![
            d(32, "EFF101", "tp_redis", "redis.Redis.get"),
            d(33, "EFF101", "tp_redis", "redis.Redis.set"),
            // The pipeline sends its buffered commands in `execute()` only.
            d(36, "EFF101", "tp_redis", "redis.client.Pipeline.execute"),
            d(
                40,
                "EFF101",
                "tp_mongo",
                "pymongo.collection.Collection.find_one"
            ),
            // `MongoClient` -> `Database` -> `Collection` factories.
            d(
                42,
                "EFF101",
                "tp_mongo",
                "pymongo.collection.Collection.insert_many"
            ),
            d(
                43,
                "EFF101",
                "tp_mongo",
                "pymongo.collection.Collection.count_documents"
            ),
            d(
                48,
                "EFF103",
                "tp_queries_and_clients_in_loops",
                "redis.Redis.hgetall"
            ),
            d(
                49,
                "EFF103",
                "tp_queries_and_clients_in_loops",
                "pymongo.collection.Collection.update_many"
            ),
            d(
                51,
                "EFF102",
                "tp_queries_and_clients_in_loops",
                "redis.Redis.from_url"
            ),
            d(
                52,
                "EFF102",
                "tp_queries_and_clients_in_loops",
                "pymongo.MongoClient"
            ),
            d(
                74,
                "EFF103",
                "tp_async_queries_and_clients_in_loops",
                "redis.asyncio.Redis.get"
            ),
            d(
                75,
                "EFF103",
                "tp_async_queries_and_clients_in_loops",
                "pymongo.asynchronous.collection.AsyncCollection.find_one"
            ),
            d(
                76,
                "EFF102",
                "tp_async_queries_and_clients_in_loops",
                "redis.asyncio.Redis"
            ),
            // `SCAN` blocks, though its cursor loop is no N+1 query.
            d(89, "EFF101", "tp_scan_blocks", "redis.Redis.scan"),
            // Buffered commands return the pipeline; `WATCH` is sent at once.
            d(
                93,
                "EFF101",
                "tp_pipeline_chains",
                "redis.client.Pipeline.execute"
            ),
            d(
                98,
                "EFF101",
                "tp_watching_pipeline",
                "redis.client.Pipeline.watch"
            ),
            d(
                101,
                "EFF101",
                "tp_watching_pipeline",
                "redis.client.Pipeline.execute"
            ),
        ]
    );
}

const CLOUD_AND_DJANGO: &str = r#"import asyncio

import boto3
from django.db import connection
from django.db.models.query import QuerySet


def read(key):
    return boto3.client("s3").get_object(Bucket="b", Key=key)


def raw(sql):
    with connection.cursor() as cursor:
        cursor.execute(sql)


async def tp_boto3(key):
    read(key)
    boto3.Session().client("sqs").send_message(QueueUrl="q", MessageBody="m")
    boto3.resource("dynamodb").Table("t").get_item(Key={"id": key})
    boto3.resource("s3").Bucket("b").upload_file("f", key)


async def tn_boto3_local_and_offloaded(key):
    s3 = boto3.client("s3")
    s3.generate_presigned_url("get_object", Params={"Bucket": "b", "Key": key})
    await asyncio.to_thread(s3.get_object, Bucket="b", Key=key)


def tp_boto3_in_loops(keys):
    table = boto3.resource("dynamodb").Table("t")
    s3 = boto3.client("s3")
    for k in keys:
        boto3.client("s3")
        table.get_item(Key={"id": k})
        s3.get_object(Bucket="b", Key=k)


async def tp_django(qs: QuerySet, sql):
    raw(sql)
    qs.filter(active=True).first()


def tp_django_in_loops(qs: QuerySet, sql, ids):
    with connection.cursor() as cursor:
        for i in ids:
            cursor.execute(sql, [i])
            qs.get(pk=i)


async def tn_django_lazy_querysets(qs: QuerySet, ids):
    async for obj in qs.filter(active=True):
        pass
    await qs.all().afirst()
    await qs.exclude(active=False).order_by("pk").acount()
    for i in ids:
        qs.filter(pk=i)


async def tp_django_async_queries_in_loops(qs: QuerySet, ids):
    for i in ids:
        await qs.aget(pk=i)


def tn_dynamodb_pagination(name):
    # `query` / `scan` page through results with `LastEvaluatedKey` by design.
    table = boto3.resource("dynamodb").Table(name)
    client = boto3.client("dynamodb")
    start = None
    while True:
        page = table.query(ExclusiveStartKey=start)
        client.scan(TableName=name, ExclusiveStartKey=start)
        start = page.get("LastEvaluatedKey")
        if not start:
            break


def tp_sessions_in_loops(profiles):
    for name in profiles:
        # A session only reads configuration; its clients are what is allocated.
        print(boto3.Session(profile_name=name).region_name)
        boto3.Session(profile_name=name).client("s3")
"#;

#[test]
fn boto3_and_django_block_on_their_calls_and_lazy_querysets_do_not() {
    assert_eq!(
        check("catalog_cloud", "app/cloud.py", CLOUD_AND_DJANGO, RULES),
        vec![
            d(18, "EFF101", "tp_boto3", "boto3.client.get_object"),
            d(19, "EFF101", "tp_boto3", "boto3.client.send_message"),
            d(20, "EFF101", "tp_boto3", "boto3.resource.Table.get_item"),
            d(
                21,
                "EFF101",
                "tp_boto3",
                "boto3.resource.Bucket.upload_file"
            ),
            d(34, "EFF102", "tp_boto3_in_loops", "boto3.client"),
            // A DynamoDB read per item; an S3 download per key is no database query.
            d(
                35,
                "EFF103",
                "tp_boto3_in_loops",
                "boto3.resource.Table.get_item"
            ),
            d(
                40,
                "EFF101",
                "tp_django",
                "django.db.backends.utils.CursorWrapper.execute"
            ),
            // Reported at the call that runs the query, not at the lazy `filter()`.
            d(
                41,
                "EFF101 [heuristic]",
                "tp_django",
                "django.db.models.query.QuerySet.first"
            ),
            d(
                47,
                "EFF103",
                "tp_django_in_loops",
                "django.db.backends.utils.CursorWrapper.execute"
            ),
            d(
                48,
                "EFF103",
                "tp_django_in_loops",
                "django.db.models.query.QuerySet.get"
            ),
            d(
                62,
                "EFF103",
                "tp_django_async_queries_in_loops",
                "django.db.models.query.QuerySet.aget"
            ),
            d(82, "EFF102", "tp_sessions_in_loops", "boto3.Session.client"),
        ]
    );
}

const BULK: &str = r#"from django.db.models.query import QuerySet
from pymongo.asynchronous.collection import AsyncCollection
from pymongo.collection import Collection


def tn_bulk_apis_per_batch(qs: QuerySet, coll: Collection, batches):
    for batch in batches:
        qs.bulk_create(batch)
        qs.in_bulk(batch)
        coll.insert_many(batch)
        coll.bulk_write(batch)


async def tn_async_bulk_apis_per_batch(qs: QuerySet, coll: AsyncCollection, batches):
    for batch in batches:
        await qs.abulk_create(batch)
        await qs.ain_bulk(batch)
        await coll.insert_many(batch)
        await coll.bulk_write(batch)


async def tp_bulk_apis_block(qs: QuerySet, coll: Collection, rows):
    qs.bulk_create(rows)
    coll.insert_many(rows)
"#;

/// The bulk APIs send many rows in one statement: a loop over batches is no N+1, but the
/// synchronous ones still block.
#[test]
fn bulk_apis_per_batch_are_no_n_plus_one() {
    assert_eq!(
        check("catalog_bulk", "app/bulk.py", BULK, RULES),
        vec![
            d(
                23,
                "EFF101",
                "tp_bulk_apis_block",
                "django.db.models.query.QuerySet.bulk_create"
            ),
            d(
                24,
                "EFF101",
                "tp_bulk_apis_block",
                "pymongo.collection.Collection.insert_many"
            ),
        ]
    );
}

const DATA: &str = r#"import numpy as np
import pandas as pd


def load(path):
    return pd.read_csv(path)


async def tp_pandas_io(path, con):
    load(path)
    pd.read_parquet(path)
    pd.read_sql("SELECT 1", con)
    pd.DataFrame({"a": [1]}).to_sql("t", con)


def tp_queries_in_loops(con, ids):
    for i in ids:
        pd.read_sql_query("SELECT * FROM t WHERE id = %s", con, params=[i])


def tp_self_accumulation(parts, rows, cols):
    df = pd.DataFrame()
    for part in parts:
        df = pd.concat([df, part])
    arr = np.zeros((0, 3))
    for row in rows:
        arr = np.vstack([arr, row])
    grid = np.zeros((3, 0))
    for col in cols:
        grid = np.column_stack([grid, col])
    flat = np.zeros(0)
    for row in rows:
        flat = np.append(flat, row)
    return df, arr, grid, flat


def tn_concatenated_once(parts):
    chunks = []
    for part in parts:
        chunks.append(part)
    return pd.concat(chunks), np.vstack(chunks)
"#;

#[test]
fn pandas_io_and_quadratic_concatenation() {
    assert_eq!(
        check(
            "catalog_data",
            "app/frames.py",
            DATA,
            "EFF101,EFF103,EFF105,DATA101"
        ),
        vec![
            // File readers are `EFF105` (opt-in); SQL readers and writers block on the database.
            d(10, "EFF105", "tp_pandas_io", "pandas.read_csv"),
            d(11, "EFF105", "tp_pandas_io", "pandas.read_parquet"),
            d(12, "EFF101", "tp_pandas_io", "pandas.read_sql"),
            d(13, "EFF101", "tp_pandas_io", "pandas.DataFrame.to_sql"),
            d(18, "EFF103", "tp_queries_in_loops", "pandas.read_sql_query"),
            d(24, "DATA101", "tp_self_accumulation", "df"),
            d(27, "DATA101", "tp_self_accumulation", "arr"),
            d(30, "DATA101", "tp_self_accumulation", "grid"),
            d(33, "DATA101", "tp_self_accumulation", "flat"),
        ]
    );
}

const METADATA: &str = r#"import asyncio
import os
import socket
from pathlib import Path

from starlette.staticfiles import StaticFiles


async def tp_metadata(p: str):
    os.stat(p)
    os.path.exists(p)
    os.path.isfile(p)
    os.path.isdir(p)
    return os.path.realpath(p)


async def tp_paths(p: Path):
    p.exists()
    p.stat()
    p.is_file()
    p.is_dir()
    return p.resolve()


async def tp_lookups(host: str):
    socket.getaddrinfo(host, 443)
    socket.gethostbyname(host)
    return socket.getfqdn(host)


async def tn_offloaded(p: str, host: str):
    await asyncio.to_thread(os.stat, p)
    loop = asyncio.get_running_loop()
    return await loop.getaddrinfo(host, 443)


def tn_sync(p: str):
    return os.path.exists(p)


class Frontend(StaticFiles):
    def _exists(self, path: str) -> bool:
        full_path, stat_result = self.lookup_path(path)
        return stat_result is not None

    async def get_response(self, path, scope):
        if self._exists(path):
            return await super().get_response(path, scope)
        return None
"#;

/// File metadata (`os.stat`, the `os.path` and `pathlib.Path` tests, `realpath` / `resolve`)
/// blocks on the file system (`EFF105`, opt-in), host name lookups on the network (`EFF101`);
/// offloaded, in a synchronous function, or through the event loop's own `getaddrinfo` they
/// block nothing. Starlette's `StaticFiles.lookup_path` resolves and stats the path, so a
/// subclass calling it on the event loop blocks.
#[test]
fn file_metadata_and_name_lookups_block() {
    assert_eq!(
        check("cat_metadata", "app.py", METADATA, "EFF101,EFF105"),
        vec![
            d(10, "EFF105", "tp_metadata", "os.stat"),
            d(11, "EFF105", "tp_metadata", "os.path.exists"),
            d(12, "EFF105", "tp_metadata", "os.path.isfile"),
            d(13, "EFF105", "tp_metadata", "os.path.isdir"),
            d(14, "EFF105", "tp_metadata", "os.path.realpath"),
            d(18, "EFF105", "tp_paths", "pathlib.Path.exists"),
            d(19, "EFF105", "tp_paths", "pathlib.Path.stat"),
            d(20, "EFF105", "tp_paths", "pathlib.Path.is_file"),
            d(21, "EFF105", "tp_paths", "pathlib.Path.is_dir"),
            d(22, "EFF105", "tp_paths", "pathlib.Path.resolve"),
            d(26, "EFF101 [heuristic]", "tp_lookups", "socket.getaddrinfo"),
            d(
                27,
                "EFF101 [heuristic]",
                "tp_lookups",
                "socket.gethostbyname"
            ),
            d(28, "EFF101", "tp_lookups", "socket.getfqdn"),
            d(
                47,
                "EFF105",
                "Frontend.get_response",
                "starlette.staticfiles.StaticFiles.lookup_path"
            ),
        ]
    );
}

const FILES: &str = r#"import io
from pathlib import Path


async def tp_text(p: str):
    f = open(p)
    f.readable()
    f.fileno()
    text = f.read()
    f.close()
    return text.upper()


async def tp_binary_read(p: str):
    f = open(p, "rb")
    f.seekable()
    data = f.read()
    f.close()
    return data.hex()


async def tp_binary_write(p: str, data: bytes):
    f = open(p, mode="ab")
    f.writable()
    f.write(data)
    f.flush()
    f.close()


async def tp_unbuffered(p: str):
    f = io.open(p, "rb", 0)
    f.isatty()
    f.tell()
    f.readline()


async def tp_unknown_mode(p: str, mode: str, size: int, options: dict):
    f = open(p, mode)
    f.fileno()
    f.write("x")
    g = open(p, "wb", buffering=size)
    g.tell()
    h = open(p, "rb", **options)
    h.flush()


async def tp_with(p: Path, q: str):
    with p.open("r+b") as f:
        f.readable()
        for line in f:
            f.seek(0)
    with open(q, "w") as out:
        out.writelines(["a"])


def tn_sync(p: str):
    with open(p) as f:
        return f.read()
"#;

/// `open`, `io.open` and `Path.open` give the file class their mode and buffering select
/// (text, binary read, binary write, unbuffered binary, binary read and write), and a mode or
/// a buffering heki does not know gives the members every file class has (`io.open()`). The
/// members that read, write, flush, seek, or close wait on the file system (`EFF105`, opt-in)
/// as `open` does; those that only ask the kernel (`readable`, `fileno`, `isatty`, `tell` of a
/// binary file) do not. `with` binds the file itself.
#[test]
fn file_objects_wait_on_the_file_system_by_the_mode_they_are_opened_in() {
    assert_eq!(
        check("cat_files", "app/files.py", FILES, "EFF101,EFF105"),
        vec![
            d(6, "EFF105", "tp_text", "builtins.open"),
            d(9, "EFF105", "tp_text", "io.TextIOWrapper.read"),
            d(10, "EFF105", "tp_text", "io.TextIOWrapper.close"),
            d(15, "EFF105", "tp_binary_read", "builtins.open"),
            d(17, "EFF105", "tp_binary_read", "io.BufferedReader.read"),
            d(18, "EFF105", "tp_binary_read", "io.BufferedReader.close"),
            d(23, "EFF105", "tp_binary_write", "builtins.open"),
            d(25, "EFF105", "tp_binary_write", "io.BufferedWriter.write"),
            d(26, "EFF105", "tp_binary_write", "io.BufferedWriter.flush"),
            d(27, "EFF105", "tp_binary_write", "io.BufferedWriter.close"),
            d(31, "EFF105", "tp_unbuffered", "io.open"),
            d(34, "EFF105", "tp_unbuffered", "io.FileIO.readline"),
            d(38, "EFF105", "tp_unknown_mode", "builtins.open"),
            d(40, "EFF105", "tp_unknown_mode", "io.open().write"),
            d(41, "EFF105", "tp_unknown_mode", "builtins.open"),
            d(42, "EFF105", "tp_unknown_mode", "io.open().tell"),
            d(43, "EFF105", "tp_unknown_mode", "builtins.open"),
            d(44, "EFF105", "tp_unknown_mode", "io.open().flush"),
            d(48, "EFF105", "tp_with", "pathlib.Path.open"),
            d(50, "EFF105", "tp_with", "io.BufferedRandom.__iter__"),
            d(51, "EFF105", "tp_with", "io.BufferedRandom.seek"),
            d(52, "EFF105", "tp_with", "builtins.open"),
            d(53, "EFF105", "tp_with", "io.TextIOWrapper.writelines"),
        ]
    );
}

/// A binary read returns `None` from a non-blocking file descriptor, and `open` sets
/// `O_NONBLOCK` on no path: only a path argument gives the buffered class, whose `read` is
/// `bytes`. An integer or unknown file gives the members every file class has.
#[test]
fn a_binary_read_of_a_file_that_may_be_a_descriptor_has_no_class() {
    const SRC: &str = r#"from pathlib import Path


async def from_path():
    f = open("x.bin", "rb")
    return f.read()


async def from_path_object(p: Path):
    f = open(p, "rb")
    return f.read()


async def from_descriptor(fd: int):
    f = open(fd, "rb")
    return f.read()


async def from_unknown(x):
    f = open(x, "rb")
    return f.read()


"#;
    assert_eq!(
        check("cat_files_fd", "app/fd.py", SRC, "EFF101,EFF105"),
        vec![
            d(5, "EFF105", "from_path", "builtins.open"),
            d(6, "EFF105", "from_path", "io.BufferedReader.read"),
            d(10, "EFF105", "from_path_object", "builtins.open"),
            d(11, "EFF105", "from_path_object", "io.BufferedReader.read"),
            d(15, "EFF105", "from_descriptor", "builtins.open"),
            d(16, "EFF105", "from_descriptor", "io.open().read"),
            d(20, "EFF105", "from_unknown", "builtins.open"),
            d(21, "EFF105", "from_unknown", "io.open().read"),
        ]
    );
}

/// The calls on those file objects, and on what `read` returns, are all resolved: `heki
/// recommend` counts no call it cannot see.
#[test]
fn file_object_calls_are_resolved() {
    let p = Project::new("cat_files_resolved");
    p.write("app/files.py", FILES);
    let (code, out, err) = p.heki(&["dump-recommend-facts"]);
    assert_eq!(code, 0, "{out}\n{err}");
    let facts: Value = serde_json::from_str(&out).unwrap();
    let spots = &facts["blind_spots"];
    assert_eq!(spots["unresolved"], serde_json::json!([]), "{spots:#}");
    assert_eq!(spots["totals"]["unresolved"]["calls"], 0, "{spots:#}");
}

const RESULTS: &str = r#"import json
import logging
import os
import re
import shutil
import subprocess
import threading
from importlib import import_module
from pathlib import Path
from urllib.parse import urlparse


def processes():
    done = subprocess.run(["true"])
    done.check_returncode()
    return done.returncode.bit_length()


def stats(p: Path):
    os.stat("x").st_mtime.is_integer()
    os.lstat("x").st_size.bit_length()
    p.stat().st_mode.bit_length()
    p.lstat().st_mtime_ns.bit_length()


def texts(p: Path):
    os.getcwd().upper()
    os.getcwdb().upper()
    json.dumps({}).upper()
    p.read_text().upper()
    p.read_bytes().upper()


def paths(p: Path, s: str, b: bytes):
    os.path.abspath(p).upper()
    os.path.realpath(s).upper()
    os.path.relpath(b).upper()
    os.path.expanduser("~").upper()
    shutil.copy(p, s).upper()
    shutil.copy2(b, b).upper()
    shutil.copytree(s, p).exists()


def unknown_paths(x, p: Path):
    os.path.abspath(x).upper()
    shutil.copy(x, p).upper()


def objects(name: str):
    logging.getLogger(name).info("x")
    threading.current_thread().is_alive()
    threading.main_thread().is_alive()
    re.compile("a").findall("a").copy()
    urlparse("http://h").geturl().upper()
    urlparse(b"http://h").geturl().upper()


def unknown_objects(x):
    urlparse(x).geturl()
    import_module(x).run()
"#;

/// The calls on what these standard library functions return are resolved, by the class of
/// their result: a path function's follows the kind of path it is given, and `urlparse`'s the
/// kind of URL. An argument heki knows nothing of gives the join of the declared results of
/// the overloads: none for a path function's, of a type variable (a destination path object
/// may give a `str` or itself to `shutil.copy`), and both result classes for `urlparse`'s, of
/// which the catalog lists `geturl` only for `ParseResult`. A module `import_module` returns
/// says nothing of the names it binds.
#[test]
fn calls_on_standard_library_results_are_resolved() {
    let p = Project::new("cat_stdlib_results");
    p.write("app/results.py", RESULTS);
    let (code, out, err) = p.heki(&["dump-recommend-facts"]);
    assert_eq!(code, 0, "{out}\n{err}");
    let facts: Value = serde_json::from_str(&out).unwrap();
    let unresolved: Vec<(String, String, Value)> = facts["blind_spots"]["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .map(|u| {
            (
                u["function"].as_str().unwrap().to_owned(),
                u["reason"].as_str().unwrap().to_owned(),
                u["names"].clone(),
            )
        })
        .collect();
    let at = |function: &str, reason: &str, names: &[&str]| {
        (
            format!("app.results.{function}"),
            reason.to_owned(),
            serde_json::json!(names),
        )
    };
    assert_eq!(
        unresolved,
        vec![
            at("unknown_objects", "unknown-member", &["geturl", "run"]),
            at("unknown_paths", "call-result", &["upper"]),
        ]
    );
}

#[test]
fn a_numeric_host_is_parsed_not_resolved() {
    // `AI_NUMERICHOST` (alone, through a local, or among other flags) never asks a resolver.
    assert_eq!(
        check(
            "cat_numeric_host",
            "app.py",
            r#"import socket


def parse(ip, port):
    flags = socket.AI_NUMERICHOST
    return socket.getaddrinfo(ip, port, flags=flags)


def parse_stream(ip):
    return socket.getaddrinfo(ip, 0, socket.AF_UNSPEC, socket.SOCK_STREAM, 0, socket.AI_NUMERICHOST)


def parse_passive(ip):
    return socket.getaddrinfo(ip, 0, flags=socket.AI_PASSIVE | socket.AI_NUMERICHOST)


def resolve(host):
    return socket.getaddrinfo(host, 0, flags=socket.AI_PASSIVE)


async def handler(host, ip):
    parse(ip, 0)
    parse_stream(ip)
    parse_passive(ip)
    resolve(host)
"#,
            "EFF101",
        ),
        vec![d(25, "EFF101 [heuristic]", "handler", "socket.getaddrinfo")]
    );
}

#[test]
fn a_project_subclass_overriding_a_catalogued_method_leaves_its_calls_unestablished() {
    // A value annotated `httpx.Client` may be a project subclass (R2): one that overrides `get`
    // runs no request, so a helper calling `get` has not established it, and neither has its
    // caller. `requests.Session` has no such subclass.
    assert_eq!(
        check(
            "cat_overridden_catalogued_method",
            "app.py",
            r#"import httpx
import requests


class CachedClient(httpx.Client):
    def get(self, url):
        return url


def fetch(c: httpx.Client, url):
    return c.get(url)


def fetch_session(s: requests.Session, url):
    return s.get(url)


async def tn_overridden(c: httpx.Client):
    return fetch(c, "https://example.com")


async def tp_session(s: requests.Session):
    return fetch_session(s, "https://example.com")
"#,
            "EFF101",
        ),
        vec![
            d(
                19,
                "EFF101 [heuristic]",
                "tn_overridden",
                "httpx.Client.get"
            ),
            d(23, "EFF101", "tp_session", "requests.Session.get"),
        ]
    );
}

/// `sqlite3.connect` waits on a file only where heki establishes that its argument names one: a
/// literal or a module constant without an in-memory form, or a path built from such parts and
/// `__file__`. Every other way of opening an in-memory database (a module constant, a partial, a
/// default, an environment lookup, an f-string, `%`, or `.format()` URI, `*args`, a phi, a
/// container, a constructor argument) is not established; a path from a module value that is no
/// string constant is not either.
#[test]
fn only_a_named_file_makes_a_sqlite_connection_wait() {
    let p = Project::new("cat_sqlite_names_a_file");
    for (rel, source) in [
        ("shapes.py", MEMORY_SHAPES),
        ("shapes2.py", MEMORY_SHAPES_2),
        ("shapes3.py", MEMORY_SHAPES_3),
        ("shapes4.py", MEMORY_SHAPES_4),
        ("ondisk.py", FILE_SHAPES),
    ] {
        p.write(rel, source);
    }
    let (code, stdout, stderr) = p.check(&["--output-format", "json", "--select", "EFF101"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    // The exact reports: a heuristic one does not establish that the connection waits.
    let mut got: Vec<(String, u64)> = diags
        .iter()
        .filter(|d| d["certainty"] == "exact")
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
            )
        })
        .collect();
    got.dedup();
    let file = |line: u64| ("ondisk.py".to_owned(), line);
    assert_eq!(
        got,
        vec![file(10), file(14), file(18), file(22), file(26), file(30)]
    );
}

const MEMORY_SHAPES: &str = r#"import contextlib
import functools
import os
import sqlite3

DB = ":memory:"


async def through_closing() -> None:
    with contextlib.closing(sqlite3.connect(":memory:")) as conn:
        conn.execute("SELECT 1")


async def through_with() -> None:
    with sqlite3.connect(":memory:") as conn:
        conn.execute("SELECT 1")


async def through_cursor() -> None:
    cur = sqlite3.connect(":memory:").cursor()
    cur.execute("SELECT 1")
    cur.fetchall()


async def through_phi(path: str | None) -> None:
    conn = sqlite3.connect(path) if path else sqlite3.connect(":memory:")
    conn.execute("SELECT 1")


async def through_module_constant() -> None:
    conn = sqlite3.connect(DB)
    conn.execute("SELECT 1")


async def through_partial() -> None:
    opener = functools.partial(sqlite3.connect, ":memory:")
    opener().execute("SELECT 1")


def open_default(path: str = ":memory:") -> sqlite3.Connection:
    return sqlite3.connect(path)


async def through_default() -> None:
    open_default().execute("SELECT 1")


class Holder:
    shared = sqlite3.connect(":memory:")

    def __init__(self) -> None:
        self.conns = {"a": sqlite3.connect(":memory:")}
        self.items = [sqlite3.connect(":memory:")]

    def by_class_attribute(self) -> None:
        self.shared.execute("SELECT 1")

    def by_dict(self) -> None:
        self.conns["a"].execute("SELECT 1")

    def by_list(self) -> None:
        self.items[0].execute("SELECT 1")


async def holders() -> None:
    h = Holder()
    h.by_class_attribute()
    h.by_dict()
    h.by_list()


def unannotated_return():
    return sqlite3.connect(":memory:")


async def through_unannotated_return() -> None:
    unannotated_return().execute("SELECT 1")


class Repo:
    def __init__(self, conn) -> None:
        self.conn = conn

    def get(self) -> None:
        self.conn.execute("SELECT 1")


async def through_constructor_argument() -> None:
    Repo(sqlite3.connect(":memory:")).get()


async def through_uri() -> None:
    conn = sqlite3.connect("file:memdb1?mode=memory&cache=shared", uri=True)
    conn.execute("SELECT 1")


async def through_env_default() -> None:
    conn = sqlite3.connect(os.environ.get("DB", ":memory:"))
    conn.execute("SELECT 1")


async def through_keyword() -> None:
    conn = sqlite3.connect(database=":memory:")
    conn.execute("SELECT 1")


async def through_backup() -> None:
    src = sqlite3.connect(":memory:")
    dst = sqlite3.connect(":memory:")
    src.backup(dst)
    dst.execute("SELECT 1")
"#;

const MEMORY_SHAPES_2: &str = r#"import sqlite3


async def through_fstring_uri(name: str) -> None:
    conn = sqlite3.connect(f"file:{name}?mode=memory&cache=shared", uri=True)
    conn.execute("SELECT 1")


async def through_concat_uri(name: str) -> None:
    conn = sqlite3.connect("file:" + name + "?mode=memory", uri=True)
    conn.execute("SELECT 1")


async def through_local_constant() -> None:
    path = ":memory:"
    conn = sqlite3.connect(path)
    conn.execute("SELECT 1")


async def through_star(args: tuple[str]) -> None:
    conn = sqlite3.connect(*args)
    conn.execute("SELECT 1")
"#;

const MEMORY_SHAPES_3: &str = r#"import sqlite3


async def through_percent(alias: str) -> None:
    conn = sqlite3.connect("file:memorydb_%s?mode=memory&cache=shared" % alias, uri=True)
    conn.execute("SELECT 1")


async def through_format(alias: str) -> None:
    conn = sqlite3.connect("file:memorydb_{}?mode=memory&cache=shared".format(alias), uri=True)
    conn.execute("SELECT 1")


async def through_constant_fstring() -> None:
    conn = sqlite3.connect(f"file:rope-{1}:?mode=memory&cache=shared", uri=True)
    conn.execute("SELECT 1")


class Data:
    def __init__(self, name: str) -> None:
        self._filename = f"file:coverage-{name}?mode=memory&cache=shared"

    def read(self) -> None:
        sqlite3.connect(self._filename, uri=True).execute("SELECT 1")


async def through_attribute_fstring() -> None:
    Data("x").read()
"#;

const MEMORY_SHAPES_4: &str = r#"import sqlite3


class Store:
    def __init__(self, path: str = ":memory:") -> None:
        self.conn = sqlite3.connect(f"{path}")

    def get(self) -> None:
        self.conn.execute("SELECT 1")


async def fstring_default() -> None:
    Store().get()


def open_fmt(path: str = ":memory:"):
    return sqlite3.connect("{}".format(path))


async def format_default() -> None:
    open_fmt().execute("SELECT 1")
"#;

const FILE_SHAPES: &str = r#"import os
import sqlite3
from pathlib import Path

DB = "app.db"
HERE = os.path.dirname(__file__)


async def tp_literal() -> None:
    sqlite3.connect("app.db").execute("SELECT 1")


async def tp_constant() -> None:
    sqlite3.connect(DB).execute("SELECT 1")


async def tp_path() -> None:
    sqlite3.connect(Path(__file__).parent / "data.db").execute("SELECT 1")


async def tp_joined() -> None:
    sqlite3.connect(os.path.join(HERE, "data.db")).execute("SELECT 1")


async def tp_suffixed() -> None:
    sqlite3.connect(Path(__file__).with_suffix(".db")).execute("SELECT 1")


async def tp_joined_to_a_literal() -> None:
    sqlite3.connect(os.path.join("data", "app.db")).execute("SELECT 1")


def join(*parts: str) -> str:
    return ":memory:"


async def tn_project_function_named_like_a_path_function() -> None:
    sqlite3.connect(join("data", "app.db")).execute("SELECT 1")


async def tn_path_method_on_a_parameter(base: Path) -> None:
    sqlite3.connect(base.with_suffix(".db")).execute("SELECT 1")
"#;

/// A connection whose opening heki does not see, a value only an annotation types, may be in
/// memory: its waits are not established, whatever opens it (here, in memory, through annotated
/// attributes and an unannotated parameter). A helper's annotated parameter is decided at each
/// call by the connection passed there: one opened in memory does not wait, as the same calls
/// written inline do not, and so is a factory's annotated return by the connection every return
/// gives. Its statements are sent either way, so a loop over data still sends one per item.
#[test]
fn connections_only_annotations_name_may_be_in_memory() {
    let p = Project::new("cat_annotated_connections");
    p.write("repro.py", REPRO_ANNOTATED)
        .write("repro2.py", REPRO_UNANNOTATED);
    let (code, stdout, stderr) = p.check(&["--output-format", "json", "--select", "EFF101,EFF103"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let got: Vec<(String, u64, String)> = diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                common::tiered_code(d).to_owned(),
            )
        })
        .collect();
    let at = |path: &str, line: u64, code: &str| (path.to_owned(), line, code.to_owned());
    assert_eq!(
        got,
        vec![
            at("repro.py", 27, "EFF101 [heuristic]"),
            at("repro.py", 34, "EFF103"),
        ]
    );
}

const REPRO_ANNOTATED: &str = r#"import sqlite3


def count_rows(db: sqlite3.Connection) -> int:
    return db.execute("SELECT count(*) FROM t").fetchone()[0]


def make_table(db: sqlite3.Connection) -> None:
    db.execute("CREATE TABLE t(x)")


async def annotated_parameter() -> int:
    conn = sqlite3.connect(":memory:")
    make_table(conn)
    return count_rows(conn)


class Cache:
    def __init__(self) -> None:
        self.conn: sqlite3.Connection = sqlite3.connect(":memory:")

    def get(self) -> None:
        self.conn.execute("SELECT 1")


async def annotated_attribute() -> None:
    Cache().get()


async def loop_over(items: list[str]) -> None:
    conn = sqlite3.connect(":memory:")
    make_table(conn)
    for item in items:
        conn.execute("INSERT INTO t VALUES (?)", (item,))
"#;

const REPRO_UNANNOTATED: &str = r#"import sqlite3


class Plain:
    def __init__(self) -> None:
        self.conn = sqlite3.connect(":memory:")

    def get(self) -> None:
        self.conn.execute("SELECT 1")


async def unannotated_attribute() -> None:
    Plain().get()


def helper(db):
    db.execute("SELECT 1")


async def unannotated_parameter() -> None:
    helper(sqlite3.connect(":memory:"))


def opened() -> sqlite3.Connection:
    return sqlite3.connect(":memory:")


async def through_annotated_return() -> None:
    opened().execute("SELECT 1")
"#;

/// A local or a global annotated with the library class of the connection or the cursor it is
/// assigned keeps what heki knows of the value: opened on a file (every wait established) or in
/// memory (none), also once an `isinstance` test of that class passes. An annotation naming
/// another class, or typing a value heki does not know, is what types it, as before; a statement
/// sent per item is established either way.
#[test]
fn an_annotation_agreeing_with_a_connection_keeps_where_it_was_opened() {
    assert_eq!(
        check("cat_sqlite_annotated", "lite.py", ANNOTATED_SQLITE, RULES),
        vec![
            d(8, "EFF101", "tp_plain", "sqlite3.Connection.execute"),
            d(9, "EFF101", "tp_plain", "sqlite3.Cursor.fetchone"),
            d(
                13,
                "EFF101",
                "tp_annotated_cursor",
                "sqlite3.Connection.execute"
            ),
            d(
                14,
                "EFF101",
                "tp_annotated_cursor",
                "sqlite3.Cursor.fetchone"
            ),
            d(18, "EFF101", "tp_annotated_connection", "sqlite3.connect"),
            d(
                19,
                "EFF101",
                "tp_annotated_connection",
                "sqlite3.Connection.execute"
            ),
            d(
                23,
                "EFF101",
                "tp_annotated_global",
                "sqlite3.Connection.execute"
            ),
            d(27, "EFF101", "tp_isinstance", "sqlite3.Connection.execute"),
            d(29, "EFF101", "tp_isinstance", "sqlite3.Cursor.fetchone"),
            d(
                39,
                "EFF101",
                "heuristic_disagreeing",
                "sqlite3.Connection.execute"
            ),
            d(
                40,
                "EFF101 [heuristic]",
                "heuristic_disagreeing",
                "sqlite3.Connection.execute"
            ),
            d(
                45,
                "EFF101 [heuristic]",
                "heuristic_unknown_value",
                "sqlite3.Cursor.fetchone"
            ),
            d(51, "EFF103", "tp_query_per_id", "sqlite3.Cursor.execute"),
            d(
                57,
                "EFF103",
                "tp_query_per_row",
                "sqlite3.Connection.execute"
            ),
            d(
                63,
                "EFF103",
                "tp_held_query_per_id",
                "sqlite3.Connection.execute"
            ),
        ]
    );
}

const ANNOTATED_SQLITE: &str = r#"import sqlite3

conn = sqlite3.connect("app.db")
shared: sqlite3.Connection = sqlite3.connect("app.db")


async def tp_plain() -> object:
    cur = conn.execute("SELECT 1")
    return cur.fetchone()


async def tp_annotated_cursor() -> object:
    cur: sqlite3.Cursor = conn.execute("SELECT 1")
    return cur.fetchone()


async def tp_annotated_connection() -> object:
    c: sqlite3.Connection = sqlite3.connect("app.db")
    return c.execute("SELECT 1")


async def tp_annotated_global() -> object:
    return shared.execute("SELECT 1")


async def tp_isinstance() -> object:
    cur = conn.execute("SELECT 1")
    if isinstance(cur, sqlite3.Cursor):
        return cur.fetchone()
    return None


async def tn_annotated_in_memory() -> object:
    mem: sqlite3.Connection = sqlite3.connect(":memory:")
    return mem.execute("SELECT 1")


async def heuristic_disagreeing() -> object:
    cur: sqlite3.Connection = conn.execute("SELECT 1")
    return cur.execute("SELECT 2")


async def heuristic_unknown_value(make) -> object:
    cur: sqlite3.Cursor = make()
    return cur.fetchone()


def tp_query_per_id(ids: list[int]) -> None:
    cur: sqlite3.Cursor = conn.cursor()
    for i in ids:
        cur.execute("SELECT * FROM t WHERE id = ?", (i,))


def tp_query_per_row() -> None:
    rows: sqlite3.Cursor = conn.execute("SELECT id FROM t")
    for (i,) in rows:
        conn.execute("SELECT * FROM u WHERE id = ?", (i,))


def tp_held_query_per_id(ids: list[int]) -> None:
    execute = conn.execute
    for i in ids:
        execute("SELECT * FROM t WHERE id = ?", (i,))
"#;

#[test]
fn in_memory_databases_and_connection_options_do_not_wait() {
    // An in-memory database is opened, and its statements run, without waiting; a connection
    // opened from a parameter (here through a literal default) or only named by an annotation
    // may be one, so its waits are not established. A pragma that sets an option of the connection does not wait either; one
    // that changes the database file does.
    assert_eq!(
        check(
            "cat_in_memory_sqlite",
            "app.py",
            r#"import sqlite3


class History:
    def __init__(self, path=":memory:"):
        self.connection = sqlite3.connect(str(path))
        self.connection.execute("CREATE TABLE t (x)")


async def tn_through_a_default():
    History()


async def tn_direct():
    sqlite3.connect(":memory:")


async def tn_uri():
    sqlite3.connect("file::memory:?cache=shared", uri=True)
    sqlite3.connect("file:cache?mode=memory&cache=shared", uri=True)


async def tp_file():
    sqlite3.connect("app.db")


async def tn_connection_options(conn: sqlite3.Connection):
    conn.execute("PRAGMA busy_timeout = 5000")
    conn.execute("pragma foreign_keys=ON")


async def tp_journal(conn: sqlite3.Connection):
    conn.execute("PRAGMA journal_mode=WAL")


def open_named(name):
    return sqlite3.connect(f"{name}.db").execute("SELECT 1")


def open_joined(name):
    return sqlite3.connect(name + ".db").execute("SELECT 1")


async def tn_paths_from_parameters():
    open_named("app")
    open_joined("app")


async def tn_option_reads_and_values(conn: sqlite3.Connection, ms):
    conn.execute("PRAGMA foreign_keys")
    conn.execute(f"PRAGMA busy_timeout = {ms}")


async def tp_pragma_named_at_run_time(conn: sqlite3.Connection, name):
    conn.execute(f"PRAGMA {name}")
"#,
            "EFF101",
        ),
        vec![
            d(
                11,
                "EFF101 [heuristic]",
                "tn_through_a_default",
                "sqlite3.Connection.execute"
            ),
            d(24, "EFF101", "tp_file", "sqlite3.connect"),
            d(
                33,
                "EFF101 [heuristic]",
                "tp_journal",
                "sqlite3.Connection.execute"
            ),
            d(
                // An f-string names no file heki can establish (here one ending in `.db`).
                45,
                "EFF101 [heuristic]",
                "tn_paths_from_parameters",
                "sqlite3.Connection.execute"
            ),
            d(
                46,
                "EFF101 [heuristic]",
                "tn_paths_from_parameters",
                "sqlite3.Connection.execute"
            ),
            d(
                55,
                "EFF101 [heuristic]",
                "tp_pragma_named_at_run_time",
                "sqlite3.Connection.execute"
            ),
        ]
    );
}

#[test]
fn waits_that_their_arguments_end_at_once_are_not_reported() {
    // No sleep, a polled child, a numeric or local host, a `data:` URL: none of them waits.
    // An argument heki cannot evaluate is one the call waits on.
    assert_eq!(
        check(
            "cat_waits_end_at_once",
            "app.py",
            r#"import socket
import subprocess
import time
from urllib.request import urlopen


async def tn_no_sleep():
    time.sleep(0)
    time.sleep(0.0)


async def tn_polled(child: subprocess.Popen):
    child.wait(timeout=0)


async def tn_local_hosts():
    socket.getaddrinfo("127.0.0.1", 80)
    socket.getaddrinfo(host="::1", port=80)
    socket.getaddrinfo("localhost", 80)
    socket.gethostbyname("10.0.0.1")


async def tn_data_url():
    return urlopen("data:,hello").read()


async def tp_sleep(seconds):
    time.sleep(seconds)


async def tp_lookup():
    socket.getaddrinfo("example.com", 80)


async def tp_fetch():
    return urlopen("https://example.com").read()
"#,
            "EFF101",
        ),
        vec![
            d(28, "EFF101 [heuristic]", "tp_sleep", "time.sleep"),
            d(32, "EFF101", "tp_lookup", "socket.getaddrinfo"),
            d(36, "EFF101", "tp_fetch", "urllib.request.urlopen"),
        ]
    );
}

const OVERRIDES: &str = r#"
[[tool.heki.ecosystem]]
symbol = "sqlite3.connect"
effects = ["blocking_db"]

[[tool.heki.ecosystem]]
symbol = "vendor.fetch"
async-safe = true
effects = ["blocking_net", "db_query"]

[[tool.heki.ecosystem]]
symbol = "vendor.db.open"
return-type = "sqlite3.Connection"

[[tool.heki.ecosystem]]
symbol = "vendor.locks.Gate"
sync-lock-or-tx = true
"#;

const OVERRIDDEN: &str = r#"import asyncio
import sqlite3

import requests
import vendor
import vendor.db
import vendor.locks


def tp_factory_keeps_its_return_type(ids):
    conn = sqlite3.connect("app.db")
    for i in ids:
        conn.execute("SELECT ?", (i,))


def tn_override_drops_the_allocation(paths):
    for p in paths:
        sqlite3.connect(p)


async def tn_async_safe_api_never_blocks():
    await asyncio.gather(vendor.fetch())


async def tp_async_safe_api_keeps_its_queries(ids):
    for i in ids:
        await vendor.fetch(i)


def tp_declared_factory(ids):
    conn = vendor.db.open()
    for i in ids:
        conn.execute("SELECT ?", (i,))


def tp_declared_lock(url):
    with vendor.locks.Gate():
        requests.get(url)
"#;

/// An override sets the keys it lists and keeps the built-in entry's others; `return-type` and
/// `sync-lock-or-tx` declare factories and locks; an async-safe API never blocks the event loop
/// but still counts its queries in loops.
#[test]
fn overrides_set_only_the_keys_they_list() {
    assert_eq!(
        check_configured(
            "catalog_overrides",
            OVERRIDES,
            "app/overridden.py",
            OVERRIDDEN,
            "EFF101,EFF102,EFF103,EFF106"
        ),
        vec![
            d(
                13,
                "EFF103",
                "tp_factory_keeps_its_return_type",
                "sqlite3.Connection.execute"
            ),
            d(
                27,
                "EFF103",
                "tp_async_safe_api_keeps_its_queries",
                "vendor.fetch"
            ),
            d(
                33,
                "EFF103",
                "tp_declared_factory",
                "sqlite3.Connection.execute"
            ),
            d(
                38,
                "EFF106",
                "tp_declared_lock",
                "vendor.locks.Gate->requests.get"
            ),
        ]
    );
}

const SANITIZERS: &str = r#"
[[tool.heki.ecosystem]]
symbol = "vendor.detach"
copy-sanitizer = "deep"

[[tool.heki.ecosystem]]
symbol = "vendor.shallow_deepcopy"
copy-sanitizer = true
"#;

const SANITIZED: &str = r#"import functools

import vendor


@functools.cache
def cached():
    return {"items": []}


def tn_deep_sanitizer():
    vendor.detach(cached())["items"].append(1)


def tp_shallow_sanitizer_keeps_the_items_shared():
    vendor.shallow_deepcopy(cached())["items"].append(1)


def tn_shallow_sanitizer_copies_the_top_level():
    vendor.shallow_deepcopy(cached())["new"] = 1
"#;

/// A declared copy sanitizer copies as deeply as its entry says, whatever its name.
#[test]
fn copy_sanitizers_copy_as_deeply_as_declared() {
    assert_eq!(
        check_configured(
            "catalog_sanitizers",
            SANITIZERS,
            "app/sanitized.py",
            SANITIZED,
            "MUT114"
        ),
        vec![d(
            16,
            "MUT114",
            "tp_shallow_sanitizer_keeps_the_items_shared",
            "app.sanitized.cached"
        )]
    );
}

const MODEL_COPIES: &str = r#"import functools

from pydantic import BaseModel


class Settings(BaseModel):
    items: list[int] = []


@functools.cache
def cached() -> Settings:
    return Settings()


def tn_deep_copy():
    copied = cached().model_copy(deep=True)
    copied.items.clear()


def tp_update_values_are_kept_by_reference():
    shared = cached()
    copied = shared.model_copy(deep=True, update={"items": shared.items})
    copied.items.clear()


def tp_returned_update_values_are_kept_by_reference():
    shared = cached()
    return shared.model_copy(update={"name": "x"}, deep=True)


def tp_caller_of_the_update_copy():
    tp_returned_update_values_are_kept_by_reference().items.clear()
"#;

/// `model_copy(deep=True)` copies deeply, but the values of `update` are assigned to the copy
/// by reference after copying: with `update` it is a shallow copy.
#[test]
fn model_copy_with_update_is_a_shallow_copy() {
    assert_eq!(
        check(
            "catalog_model_copy",
            "app/models.py",
            MODEL_COPIES,
            "MUT114"
        ),
        vec![
            d(
                23,
                "MUT114",
                "tp_update_values_are_kept_by_reference",
                "app.models.cached"
            ),
            d(
                32,
                "MUT114",
                "tp_caller_of_the_update_copy",
                "app.models.cached"
            ),
        ]
    );
}

const PANDAS_SOURCES: &str = r#"import io
from io import BytesIO

import pandas as pd


async def tp_downloads(url):
    pd.read_csv("https://example.org/rows.csv")
    pd.read_parquet(path="s3://bucket/rows.parquet")


async def tp_local_files(path):
    pd.read_csv("rows.csv")
    pd.read_excel(path)


async def tn_buffers(text, raw):
    pd.read_csv(io.StringIO("x\n1\n"))
    pd.read_json(path_or_buf=io.StringIO(text))
    pd.read_parquet(BytesIO(raw))


async def tp_unpacked_source(args):
    pd.read_csv(*args)


async def tp_path_or_buffer(source: str | io.StringIO):
    pd.read_csv(source)
"#;

/// A pandas reader's source decides its I/O: a URL is downloaded (`EFF101`), a path is read
/// from the disk (`EFF105`), and an in-memory buffer is parsed without I/O. A source that may be
/// a path or a buffer may be read from the disk.
#[test]
fn pandas_readers_do_the_io_of_their_source() {
    assert_eq!(
        check(
            "catalog_pandas_sources",
            "app/sources.py",
            PANDAS_SOURCES,
            "EFF101,EFF105"
        ),
        vec![
            d(8, "EFF101", "tp_downloads", "pandas.read_csv"),
            d(9, "EFF101", "tp_downloads", "pandas.read_parquet"),
            d(13, "EFF105", "tp_local_files", "pandas.read_csv"),
            d(14, "EFF105", "tp_local_files", "pandas.read_excel"),
            d(24, "EFF105", "tp_unpacked_source", "pandas.read_csv"),
            d(28, "EFF105", "tp_path_or_buffer", "pandas.read_csv"),
        ]
    );
}

const CONNECTION_OPTIONS: &str = r#"import pymongo
import pymysql


async def tp_srv_uri_resolves_in_the_constructor():
    pymongo.MongoClient("mongodb+srv://cluster.example/")
    pymongo.MongoClient(host="mongodb+srv://cluster.example/", connect=True)


async def tn_deferred_connections(uri):
    pymongo.MongoClient("mongodb+srv://cluster.example/", connect=False)
    pymongo.MongoClient("mongodb://db.example/")
    pymongo.MongoClient(uri)
    pymysql.connect(host="db", defer_connect=True)


async def tp_connections(options):
    pymysql.connect(host="db")
    pymysql.connect(**options)
    conn = pymysql.connect(host="db", defer_connect=True)
    conn.connect()
"#;

/// Connection options decide whether a client constructor does I/O: a `mongodb+srv://` URI is
/// resolved at once unless `connect=False`; `defer_connect=True` leaves the socket closed until
/// `connect()`.
#[test]
fn connection_options_decide_where_the_connection_is_made() {
    assert_eq!(
        check(
            "catalog_connection_options",
            "app/connect.py",
            CONNECTION_OPTIONS,
            "EFF101"
        ),
        vec![
            d(
                6,
                "EFF101",
                "tp_srv_uri_resolves_in_the_constructor",
                "pymongo.MongoClient"
            ),
            d(
                7,
                "EFF101",
                "tp_srv_uri_resolves_in_the_constructor",
                "pymongo.MongoClient"
            ),
            d(18, "EFF101", "tp_connections", "pymysql.connect"),
            d(19, "EFF101", "tp_connections", "pymysql.connect"),
            d(
                21,
                "EFF101",
                "tp_connections",
                "pymysql.connections.Connection.connect"
            ),
        ]
    );
}

const SYNC_WAITS: &str = r#"import concurrent.futures
import queue
import threading
from concurrent.futures import ThreadPoolExecutor


def drain(q: queue.Queue):
    return q.get()


async def tp_waits(event: threading.Event, lock: threading.Lock, q: queue.Queue, worker: threading.Thread):
    event.wait()
    lock.acquire()
    drain(q)
    worker.join(5)
    worker.join(None)


async def tp_futures(pool: ThreadPoolExecutor, job):
    future = pool.submit(job)
    future.result()
    concurrent.futures.wait([future])


async def tn_waits_that_return_at_once(event: threading.Event, lock: threading.Lock, q: queue.Queue, pool: ThreadPoolExecutor):
    event.wait(0)
    event.wait(timeout=0.0)
    lock.acquire(blocking=False)
    lock.acquire(False)
    q.get(block=False)
    q.get_nowait()
    q.put(1, False)
    pool.shutdown(wait=False)
"#;

/// Waiting on a thread, a lock, an event, a queue, or a future blocks the event loop (also with
/// no timeout, `None`), unless the call passes a zero timeout or does not block.
#[test]
fn synchronous_waits_block_unless_they_return_at_once() {
    assert_eq!(
        check("catalog_sync_waits", "app/waits.py", SYNC_WAITS, "EFF101"),
        vec![
            d(12, "EFF101 [heuristic]", "tp_waits", "threading.Event.wait"),
            d(
                13,
                "EFF101 [heuristic]",
                "tp_waits",
                "threading.Lock.acquire"
            ),
            d(14, "EFF101 [heuristic]", "tp_waits", "queue.Queue.get"),
            d(
                15,
                "EFF101 [heuristic]",
                "tp_waits",
                "threading.Thread.join"
            ),
            d(
                16,
                "EFF101 [heuristic]",
                "tp_waits",
                "threading.Thread.join"
            ),
            d(
                21,
                "EFF101 [heuristic]",
                "tp_futures",
                "concurrent.futures.Future.result"
            ),
            d(
                22,
                "EFF101 [heuristic]",
                "tp_futures",
                "concurrent.futures.wait"
            ),
        ]
    );
}

const CONSUMED_QUERIES: &str = r#"from sqlalchemy.orm import Query, Session


async def tp_consuming_calls(q: Query, session: Session, model):
    list(q)
    sorted(session.query(model))
    rows = []
    rows.extend(q)


async def tn_lazy_query(q: Query):
    return q.filter(True)
"#;

/// A consuming call iterates a catalog-typed iterable as a `for` loop does: `list(query)` runs
/// the query.
#[test]
fn consuming_calls_iterate_catalog_iterables() {
    assert_eq!(
        check(
            "catalog_consumed_queries",
            "app/consumed.py",
            CONSUMED_QUERIES,
            "EFF101"
        ),
        vec![
            d(
                5,
                "EFF101",
                "tp_consuming_calls",
                "sqlalchemy.orm.Query.__iter__"
            ),
            d(
                6,
                "EFF101",
                "tp_consuming_calls",
                "sqlalchemy.orm.Query.__iter__"
            ),
            d(
                8,
                "EFF101",
                "tp_consuming_calls",
                "sqlalchemy.orm.Query.__iter__"
            ),
        ]
    );
}

const LAZY_ITERATORS: &str = r#"import asyncio
import os

from pymongo.asynchronous.collection import AsyncCollection
from pymongo.collection import Collection


async def tn_iterated_on_a_thread(coll: Collection, path):
    await asyncio.to_thread(list, coll.find({}))
    await asyncio.to_thread(list, coll.find({}).sort("x").limit(5))
    await asyncio.to_thread(list, os.walk(path))


async def tp_iterated_on_the_loop(coll: Collection, path):
    for doc in coll.find({}):
        pass
    list(coll.find({}).sort("x"))
    coll.find({}).to_list()
    for entry in os.walk(path):
        pass


async def tn_async_cursors_collected(coll: AsyncCollection, ids):
    cursors = []
    for i in ids:
        cursors.append(coll.find({"_id": i}))
    return cursors


async def tp_async_cursors_run_per_item(coll: AsyncCollection, ids):
    for i in ids:
        async for doc in coll.find({"_id": i}):
            pass
        await coll.find({"_id": i}).sort("x").to_list()


async def tp_exhausted_cursor_reused(coll: AsyncCollection, ids):
    cursor = coll.find({})
    for i in ids:
        await cursor.to_list()
"#;

/// A cursor or a generator does its I/O while it is iterated, not when it is made: one handed
/// to a thread runs there, one iterated on the event loop blocks it, and an async cursor is a
/// query per iteration of the loop around its iteration. A cursor that is already exhausted
/// returns nothing without a query, so a consumption in a loop is a possible query, never an
/// exact one, even where the cursor is made in the loop.
#[test]
fn lazy_iterators_do_their_io_when_iterated() {
    assert_eq!(
        check(
            "catalog_lazy_iterators",
            "app/lazy.py",
            LAZY_ITERATORS,
            "EFF101,EFF103,EFF105"
        ),
        vec![
            d(
                15,
                "EFF101 [heuristic]",
                "tp_iterated_on_the_loop",
                "pymongo.cursor.Cursor.__iter__"
            ),
            d(
                17,
                "EFF101 [heuristic]",
                "tp_iterated_on_the_loop",
                "pymongo.cursor.Cursor.__iter__"
            ),
            d(
                18,
                "EFF101 [heuristic]",
                "tp_iterated_on_the_loop",
                "pymongo.cursor.Cursor.to_list"
            ),
            d(
                19,
                "EFF105",
                "tp_iterated_on_the_loop",
                "os.walk().__iter__"
            ),
            d(
                32,
                "EFF103 [heuristic]",
                "tp_async_cursors_run_per_item",
                "pymongo.asynchronous.cursor.AsyncCursor.__aiter__"
            ),
            d(
                34,
                "EFF103 [heuristic]",
                "tp_async_cursors_run_per_item",
                "pymongo.asynchronous.cursor.AsyncCursor.to_list"
            ),
            d(
                40,
                "EFF103 [heuristic]",
                "tp_exhausted_cursor_reused",
                "pymongo.asynchronous.cursor.AsyncCursor.to_list"
            ),
        ]
    );
}

/// `(path, line, rule, qualname, target)` of every diagnostic of `heki check` on a project of
/// `files` (`pyproject.toml` included).
fn check_files(
    name: &str,
    files: &[(&str, &str)],
    rules: &str,
) -> Vec<(String, u64, String, String, String)> {
    let p = Project::new(name);
    for (rel, text) in files {
        p.write(rel, text);
    }
    let (code, stdout, stderr) = p.check(&["--output-format", "json", "--select", rules]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                common::tiered_code(d).to_owned(),
                d["qualname"].as_str().unwrap().to_owned(),
                d["target"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

const DECLARED_PROJECT_FUNCTIONS: &str = r#"[project]
name = "t"

[[tool.heki.ecosystem]]
symbol = "pkg.db.run_query"
effects = ["blocking_db"]

[[tool.heki.ecosystem]]
symbol = "pkg.log.emit"
excluded = true

[[tool.heki.ecosystem]]
symbol = "pkg.net.fetch"
async-safe = true

[[tool.heki.ecosystem]]
symbol = "pkg.db.open_db"
return-type = "sqlite3.Connection"

[[tool.heki.ecosystem]]
symbol = "pkg.log.Sink.write"
excluded = true

[[tool.heki.ecosystem]]
symbol = "pkg.net.pages"
async-safe = true
"#;

const DECLARED_DB: &str = r#"def run_query(cur, sql):
    return cur.execute(sql)


def open_db(factory):
    return factory()
"#;

const DECLARED_LOG: &str = r#"import time


def emit(message):
    time.sleep(0.1)


class Sink:
    def write(self):
        self.flush()


class FileSink(Sink):
    def flush(self):
        time.sleep(0.1)
"#;

const DECLARED_NET: &str = r#"import requests


def fetch(url):
    return requests.get(url)


def _pages(url):
    yield requests.get(url)


def pages(url):
    return _pages(url)
"#;

const DECLARED_APP: &str = r#"from pkg.db import open_db, run_query
from pkg.log import FileSink, emit
from pkg.net import fetch, pages


async def tp_declared_effects(cur):
    return run_query(cur, "SELECT 1")


async def tn_excluded_and_async_safe(url):
    emit("x")
    fetch(url)


def tp_async_safe_keeps_its_allocations(urls):
    for u in urls:
        fetch(u)


def tp_declared_result(factory, ids):
    conn = open_db(factory)
    for i in ids:
        conn.execute("SELECT ?", (i,))


async def tn_declared_method_and_iterated_result(sink: FileSink, url):
    sink.write()
    for page in pages(url):
        print(page)
"#;

/// An ecosystem entry naming a project function applies when its source is checked too: its
/// effects add to the body's, `excluded` / `async-safe` keep the body's (blocking) effects from
/// its callers (also those a subclass's implementation on `self` adds, and those iterating its
/// result runs), and `return-type` types its result.
#[test]
fn declarations_of_project_functions_apply_to_their_callers() {
    let files = [
        ("pyproject.toml", DECLARED_PROJECT_FUNCTIONS),
        ("pkg/__init__.py", ""),
        ("pkg/db.py", DECLARED_DB),
        ("pkg/log.py", DECLARED_LOG),
        ("pkg/net.py", DECLARED_NET),
        ("pkg/app.py", DECLARED_APP),
    ];
    let app = |line, rule: &str, qualname: &str, target: &str| {
        (
            "pkg/app.py".to_owned(),
            line,
            rule.to_owned(),
            qualname.to_owned(),
            target.to_owned(),
        )
    };
    assert_eq!(
        check_files("catalog_declared_project", &files, RULES),
        vec![
            app(
                7,
                "EFF101 [heuristic]",
                "tp_declared_effects",
                "pkg.db.run_query"
            ),
            app(
                17,
                "EFF102",
                "tp_async_safe_keeps_its_allocations",
                "requests.get"
            ),
            app(
                23,
                "EFF103",
                "tp_declared_result",
                "sqlite3.Connection.execute"
            ),
        ]
    );
}

const WORKER_CALLBACKS: &str = r#"import asyncio
import functools

from asgiref.sync import sync_to_async
from sqlalchemy.orm import Session


@functools.cache
def cached():
    return [1, 2]


def clear(xs):
    xs.clear()


def load_one(session: Session, i):
    session.execute("SELECT 1")


@sync_to_async
def load_one_offloaded(session: Session, i):
    session.execute("SELECT 1")


async def tp_worker_mutates_a_shared_list():
    await asyncio.to_thread(clear, cached())


async def tp_queries_per_item_on_a_worker(session: Session, ids):
    loop = asyncio.get_running_loop()
    for i in ids:
        await load_one_offloaded(session, i)
        await asyncio.to_thread(load_one, session, i)
        await loop.run_in_executor(None, load_one, session, i)


async def tn_worker_does_not_block_the_loop(session: Session):
    await load_one_offloaded(session, 1)
    await asyncio.to_thread(load_one, session, 1)


class Cleaner:
    def wipe(self, xs):
        xs.clear()


async def tp_worker_method_mutates_a_shared_list(c: Cleaner):
    await asyncio.to_thread(c.wipe, cached())


async def tn_unpacked_job(job):
    await asyncio.to_thread(*job)
"#;

/// A callable an offloading API runs on a worker thread (a function or a bound method) does
/// everything it does there: its writes to shared objects and its queries per item count, its
/// blocking calls do not block the event loop. A callable unpacked with `*` is not known.
#[test]
fn offloaded_callables_run_on_a_worker() {
    assert_eq!(
        check(
            "catalog_worker_callbacks",
            "app/workers.py",
            WORKER_CALLBACKS,
            "EFF101,EFF103,MUT114"
        ),
        vec![
            d(
                27,
                "MUT114",
                "tp_worker_mutates_a_shared_list",
                "app.workers.cached"
            ),
            d(
                33,
                "EFF103 [heuristic]",
                "tp_queries_per_item_on_a_worker",
                "sqlalchemy.orm.Session.execute"
            ),
            d(
                34,
                "EFF103 [heuristic]",
                "tp_queries_per_item_on_a_worker",
                "sqlalchemy.orm.Session.execute"
            ),
            d(
                35,
                "EFF103 [heuristic]",
                "tp_queries_per_item_on_a_worker",
                "sqlalchemy.orm.Session.execute"
            ),
            d(
                49,
                "MUT114",
                "tp_worker_method_mutates_a_shared_list",
                "app.workers.cached"
            ),
        ]
    );
}

const OUT_BUFFERS: &str = r#"import numpy as np


def tn_preallocated_buffer(pairs):
    out = np.empty(2)
    for a, b in pairs:
        out = np.concatenate(([a], [b]), out=out)
    return out


def tp_accumulated_into_a_buffer(rows):
    acc = np.empty(0)
    for row in rows:
        acc = np.concatenate((acc, row), out=None)
    return acc
"#;

/// `out=` only names the buffer a concatenation writes into: the result rebuilt from the data it
/// combines is what grows.
#[test]
fn concatenation_into_a_supplied_buffer_does_not_accumulate() {
    assert_eq!(
        check(
            "catalog_out_buffers",
            "app/buffers.py",
            OUT_BUFFERS,
            "DATA101"
        ),
        vec![d(14, "DATA101", "tp_accumulated_into_a_buffer", "acc")]
    );
}

const SYNC_FACADES: &str = r#"import requests
from sqlalchemy.ext.asyncio import AsyncConnection, AsyncSession
from sqlalchemy.orm import Session


def fetch(sync_session):
    return requests.get("https://example.com")


def load(sync_session: Session):
    return sync_session.execute("SELECT 1")


async def tp_blocking_call_in_run_sync(session: AsyncSession):
    return await session.run_sync(fetch)


async def tn_database_io_through_the_facade(session: AsyncSession, conn: AsyncConnection):
    await session.run_sync(load)
    await conn.run_sync(lambda sync_conn: sync_conn.execute("SELECT 1"))


async def tp_query_per_item_through_the_facade(session: AsyncSession, ids):
    for i in ids:
        await session.run_sync(load)


async def tn_unpacked_facade_job(session: AsyncSession, job):
    await session.run_sync(*job)
"#;

/// SQLAlchemy's `run_sync(fn)` runs `fn` on the event loop's thread with a synchronous facade:
/// its database I/O through the facade is awaited for it (still a query per call), its other
/// blocking calls block the loop. A callable unpacked with `*` is not known.
#[test]
fn run_sync_callables_run_on_the_event_loop() {
    assert_eq!(
        check(
            "catalog_sync_facades",
            "app/facades.py",
            SYNC_FACADES,
            "EFF101,EFF103"
        ),
        vec![
            d(
                15,
                "EFF101 [heuristic]",
                "tp_blocking_call_in_run_sync",
                "requests.get"
            ),
            d(
                25,
                "EFF103 [heuristic]",
                "tp_query_per_item_through_the_facade",
                "sqlalchemy.orm.Session.execute"
            ),
        ]
    );
}

const FORWARDED_FLAGS: &str = r#"from concurrent.futures import ThreadPoolExecutor


class Client:
    def __init__(self):
        self._executor = ThreadPoolExecutor()

    def close(self, wait=True):
        self._executor.shutdown(wait=wait)

    async def aclose(self):
        self.close(wait=False)

    async def aclose_and_wait(self):
        self.close()


class Configured:
    def __init__(self, wait):
        self._executor = ThreadPoolExecutor()
        self.wait = wait

    def close(self):
        self._executor.shutdown(wait=self.wait)

    async def aclose(self):
        self.close()
"#;

/// A flag a helper passes on from its own parameter (`shutdown(wait=wait)`) is decided at the
/// helper's call: `close(wait=False)` does not wait. One read from an attribute of a parameter
/// (`wait=self.wait`) is no caller's to decide.
#[test]
fn forwarded_flags_are_decided_by_the_caller() {
    assert_eq!(
        check(
            "catalog_forwarded_flags",
            "app/client.py",
            FORWARDED_FLAGS,
            "EFF101"
        ),
        vec![
            d(
                15,
                "EFF101 [heuristic]",
                "Client.aclose_and_wait",
                "concurrent.futures.ThreadPoolExecutor.shutdown"
            ),
            d(
                27,
                "EFF101 [heuristic]",
                "Configured.aclose",
                "concurrent.futures.ThreadPoolExecutor.shutdown"
            ),
        ]
    );
}

const SETTINGS: &str = r#"QUIET = 0
SHARED = 0
"#;

const REBIND: &str = r#"import app.settings


def configure():
    app.settings.SHARED = 5
"#;

const CONSTANT_ARGUMENTS: &str = r#"import socket
import time
from urllib.request import urlopen

from app.settings import QUIET, SHARED

PAUSE = 0
NO_DELAY = 0.0
PAGE = "data:,x"
HOST = "localhost"
LOOPBACK = "127.0.0.1"
V6 = "::1"
WAIT = 1
REMOTE = "example.com"
TWICE = 0
TWICE = 1


def nap():
    time.sleep(PAUSE)


async def tn_module_constants():
    nap()
    time.sleep(NO_DELAY)
    urlopen(PAGE).read()
    socket.getaddrinfo(HOST, 80)
    socket.gethostbyname(LOOPBACK)
    socket.getaddrinfo(V6, 80)


async def tn_imported_constant():
    time.sleep(QUIET)


async def tp_waiting_constant():
    time.sleep(WAIT)


async def tp_remote_constant():
    socket.getaddrinfo(REMOTE, 80)


async def tp_bound_twice():
    time.sleep(TWICE)


async def tp_rebound_elsewhere():
    time.sleep(SHARED)
"#;

/// A catalog case reads a module constant bound once to a literal as it reads the literal: a
/// sleep of no time, a `data:` URL, and a local or numeric host end at once, also through a
/// helper and from another module. A constant that waits, one bound twice, and one another
/// module rebinds are values the call waits on.
#[test]
fn catalog_argument_cases_read_module_constants_bound_once() {
    let p = Project::new("cat_constant_arguments");
    p.write("app/__init__.py", "")
        .write("app/settings.py", SETTINGS)
        .write("app/rebind.py", REBIND)
        .write("app/use.py", CONSTANT_ARGUMENTS);
    let (code, stdout, stderr) = p.check(&["--output-format", "json", "--select", "EFF101"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let got: Vec<(String, u64, String, String)> = diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                common::tiered_code(d).to_owned(),
                d["qualname"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    let at = |line: u64, qualname: &str| {
        (
            "app/use.py".to_owned(),
            line,
            "EFF101".to_owned(),
            qualname.to_owned(),
        )
    };
    assert_eq!(
        got,
        vec![
            at(37, "tp_waiting_constant"),
            at(41, "tp_remote_constant"),
            at(45, "tp_bound_twice"),
            at(49, "tp_rebound_elsewhere"),
        ]
    );
}

/// The constants a catalog case reads are those of the module's IR (`heki_ir::ModuleConstants`),
/// the facts' own view of a name bound once: an integer the code computes from other constants, a
/// negative literal, and a name the module binds after its last star import hold their value. A
/// name bound before the star import may be rebound by it, also one with an underscore, which
/// the imported module's `__all__` may list.
#[test]
fn catalog_argument_cases_read_the_constants_of_the_module_ir() {
    let p = Project::new("cat_ir_constants");
    p.write("app/__init__.py", "")
        .write(
            "app/extra.py",
            "__all__ = [\"LATE\", \"_HIDDEN\"]\nLATE = 1\n_HIDDEN = 1\n",
        )
        .write(
            "app/use.py",
            r#"import time

_HIDDEN = 0

from app.extra import *

BASE = 2
PAUSE = BASE - 2
NEGATIVE = -1
LATE = 0


async def tn_computed():
    time.sleep(PAUSE)


async def tn_after_star():
    time.sleep(LATE)


async def tp_negative():
    time.sleep(NEGATIVE)


async def tp_rebound_by_the_star_import():
    time.sleep(_HIDDEN)
"#,
        );
    let (code, stdout, stderr) = p.check(&["--output-format", "json", "--select", "EFF101"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let got: Vec<&str> = diags
        .iter()
        .map(|d| d["qualname"].as_str().unwrap())
        .collect();
    // `time.sleep(-1)` raises `ValueError` without waiting, which no catalog case says: it counts
    // as a wait.
    assert_eq!(
        got,
        vec!["tp_negative", "tp_rebound_by_the_star_import"],
        "{stdout}"
    );
}

/// A module constant imported by a module that binds the name again: in a function declaring it
/// `global`, in a second module-level store, or by deleting it. The import no longer decides
/// the value, so the call waits on it; another module binding a global of the same name of its
/// own changes nothing.
#[test]
fn catalog_argument_cases_read_no_imported_constant_the_module_binds_again() {
    let sleeps = |head: &str| {
        format!(
            "import time\n\nfrom app.settings import QUIET\n{head}\n\nasync def handler():\n    time.sleep(QUIET)\n"
        )
    };
    let p = Project::new("cat_rebound_imports");
    p.write("app/__init__.py", "")
        .write("app/settings.py", SETTINGS)
        .write(
            "app/unrelated.py",
            "QUIET = 3\n\n\ndef louder():\n    global QUIET\n    QUIET = 4\n",
        )
        .write(
            "app/flips.py",
            &sleeps("\n\ndef louder():\n    global QUIET\n    QUIET = 1\n"),
        )
        .write(
            "app/twice.py",
            &sleeps("import os\n\nif os.environ.get(\"X\"):\n    QUIET = 1\n"),
        )
        .write(
            "app/deleted.py",
            &sleeps("\n\ndef drop():\n    global QUIET\n    del QUIET\n"),
        )
        .write("app/kept.py", &sleeps(""));
    let (code, stdout, stderr) = p.check(&["--output-format", "json", "--select", "EFF101"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let got: Vec<(String, String)> = diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                common::tiered_code(d).to_owned(),
            )
        })
        .collect();
    let at = |path: &str| (path.to_owned(), "EFF101".to_owned());
    assert_eq!(
        got,
        vec![at("app/deleted.py"), at("app/flips.py"), at("app/twice.py")]
    );
}

/// A catalog case reads a module constant through the module object (`settings.QUIET`) and
/// through a function's own import as it reads the name: a sleep of no time is no wait, one
/// another module rebinds is, and a database name read as an attribute names its file.
#[test]
fn catalog_argument_cases_read_module_constants_as_attributes_and_local_imports() {
    let p = Project::new("cat_constant_read_forms");
    p.write("app/__init__.py", "")
        .write("app/settings.py", SETTINGS)
        .write("app/rebind.py", REBIND)
        .write("app/names.py", "DB = \"data.db\"\nMEMORY = \":memory:\"\n")
        .write(
            "app/use.py",
            "import sqlite3\nimport time\n\nfrom app import names, settings\n\n\nasync def by_attribute():\n    time.sleep(settings.QUIET)\n\n\nasync def by_local_import():\n    from app.settings import QUIET\n    time.sleep(QUIET)\n\n\nasync def rebound_elsewhere():\n    time.sleep(settings.SHARED)\n\n\nasync def file_database():\n    sqlite3.connect(names.DB)\n\n\nasync def memory_database():\n    sqlite3.connect(names.MEMORY)\n",
        );
    let (code, stdout, stderr) = p.check(&["--output-format", "json", "--select", "EFF101"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let got: Vec<(u64, String)> = diags
        .iter()
        .map(|d| {
            (
                d["line"].as_u64().unwrap(),
                d["qualname"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (17, "rebound_elsewhere".to_owned()),
            (21, "file_database".to_owned()),
        ]
    );
}

const ASYNC_DRIVERS_AND_DUCKDB: &str = r#"import asyncio

import asyncpg
import duckdb
import redis
import redis.asyncio as aioredis
from asyncpg.pool import Pool
from motor.motor_asyncio import AsyncIOMotorClient, AsyncIOMotorCollection


async def tp_asyncpg_in_loops(dsn, ids):
    for i in ids:
        conn = await asyncpg.connect(dsn)
        await conn.fetchrow("SELECT $1", i)
    pool = await asyncpg.create_pool(dsn)
    async with pool.acquire() as conn:
        for i in ids:
            await conn.execute("DELETE FROM t WHERE id = $1", i)
    for i in ids:
        await pool.fetchval("SELECT $1", i)


async def tp_rows_of_a_query(pool: Pool):
    for row in await pool.fetch("SELECT id FROM t"):
        await pool.execute("DELETE FROM u WHERE t_id = $1", row["id"])


async def tn_asyncpg(dsn, rows):
    async with asyncpg.create_pool(dsn) as pool:
        await pool.fetch("SELECT 1")
        await asyncio.gather(pool.fetchval("SELECT 1"), pool.fetchval("SELECT 2"))
        async with pool.acquire() as conn:
            await conn.executemany("INSERT INTO t VALUES ($1)", rows)
            await conn.copy_records_to_table("t", records=rows)


async def tp_motor_in_loops(url, ids):
    users = AsyncIOMotorClient(url)["app"]["users"]
    for i in ids:
        await users.find_one({"_id": i})
    for i in ids:
        await users.find({"_id": i}).sort("a").to_list(None)


async def tn_motor(coll: AsyncIOMotorCollection, docs):
    await coll.find_one({})
    await coll.insert_many(docs)
    async for doc in coll.find({}).sort("a"):
        pass
    for d in docs:
        coll.find({"_id": d})
        coll.aggregate([{"$match": d}])


async def tp_redis_commands(r: redis.Redis, keys):
    r.hincrby("h", "n", 1)
    for k in keys:
        await aioredis.Redis().xadd(k, {"a": 1})


def tp_duckdb_in_loops(path, ids):
    con = duckdb.connect(path)
    for i in ids:
        con.execute("SELECT ?", [i])
        duckdb.connect()


async def tn_duckdb_may_be_in_memory(path):
    con = duckdb.connect(path)
    con.execute("SELECT 1")
    duckdb.connect().execute("SELECT 1").fetchall()
"#;

/// asyncpg and motor count their queries (a pool's included) and never block; a sync redis
/// command blocks; a DuckDB statement is a query but, the database possibly in memory, no wait.
#[test]
fn async_drivers_count_their_queries_and_duckdb_waits_on_nothing_known() {
    assert_eq!(
        check(
            "catalog_async_drivers",
            "app/drivers.py",
            ASYNC_DRIVERS_AND_DUCKDB,
            RULES
        ),
        vec![
            d(13, "EFF102", "tp_asyncpg_in_loops", "asyncpg.connect"),
            d(
                14,
                "EFF103",
                "tp_asyncpg_in_loops",
                "asyncpg.Connection.fetchrow"
            ),
            d(
                18,
                "EFF103",
                "tp_asyncpg_in_loops",
                "asyncpg.Connection.execute"
            ),
            d(20, "EFF103", "tp_asyncpg_in_loops", "asyncpg.Pool.fetchval"),
            // The rows of a query are as many as the database holds.
            d(
                25,
                "EFF103",
                "tp_rows_of_a_query",
                "asyncpg.pool.Pool.execute"
            ),
            d(
                40,
                "EFF103",
                "tp_motor_in_loops",
                "motor.motor_asyncio.AsyncIOMotorCollection.find_one"
            ),
            d(
                42,
                "EFF103 [heuristic]",
                "tp_motor_in_loops",
                "motor.motor_asyncio.AsyncIOMotorCursor.to_list"
            ),
            d(56, "EFF101", "tp_redis_commands", "redis.Redis.hincrby"),
            d(58, "EFF102", "tp_redis_commands", "redis.asyncio.Redis"),
            d(
                58,
                "EFF103",
                "tp_redis_commands",
                "redis.asyncio.Redis.xadd"
            ),
            d(
                64,
                "EFF103",
                "tp_duckdb_in_loops",
                "duckdb.DuckDBPyConnection.execute"
            ),
            d(65, "EFF102", "tp_duckdb_in_loops", "duckdb.connect"),
        ]
    );
}

const API_CLIENTS_AND_DATA: &str = r#"import io
import sqlite3

import anthropic
import celery
import httpx
import openai
import polars as pl
import torch
from celery import Celery
from google import genai

app = Celery("proj")


async def tp_celery(task: celery.Task):
    app.send_task("proj.add", args=(1, 2))
    task.delay(1, 2)


def tp_clients_in_loops(prompts):
    for p in prompts:
        openai.OpenAI()
        anthropic.Anthropic()
        genai.Client()


def tn_clients_sharing_a_pool(prompts):
    shared = httpx.Client()
    for p in prompts:
        openai.OpenAI(http_client=shared)
        anthropic.Anthropic(http_client=shared)


async def tn_async_clients_sharing_a_pool(prompts):
    shared = httpx.AsyncClient()
    for p in prompts:
        openai.AsyncOpenAI(http_client=shared)
        anthropic.AsyncAnthropic(http_client=shared)


def tp_clients_with_unknown_http_client(prompts, other):
    for p in prompts:
        openai.OpenAI(http_client=other)


async def tp_async_clients_in_loops(prompts):
    for p in prompts:
        openai.AsyncOpenAI()
        anthropic.AsyncAnthropic()


async def tn_clients_built_once(prompts):
    client = openai.AsyncOpenAI()
    sync = openai.OpenAI()
    for p in prompts:
        pass


async def tp_polars(conn, path):
    pl.read_database("SELECT 1", conn)
    pl.read_csv("https://example.com/a.csv")
    pl.read_parquet(path)
    pl.read_database("SELECT 1", sqlite3.connect("app.db"))


async def tn_polars_in_memory(data: bytes):
    pl.read_csv(io.BytesIO(data))
    pl.read_csv(b"a\n1\n")
    pl.read_ndjson(b"{}")
    pl.read_database("SELECT 1", sqlite3.connect(":memory:"))
    pl.scan_parquet("a.parquet")


def tp_torch_accumulation(batches):
    acc = torch.zeros(0)
    for b in batches:
        acc = torch.cat([acc, b])
    return acc


def tn_torch_concatenated_once(batches):
    out = []
    for b in batches:
        out.append(b)
    return torch.cat(out)


async def tp_torch_io(path, model):
    torch.save(model.state_dict(), path)
    torch.load(path)


async def tn_torch_buffers(model):
    buf = io.BytesIO()
    torch.save(model.state_dict(), buf)
    torch.load(io.BytesIO(buf.getvalue()))
"#;

/// Celery publishes to its broker unless configured otherwise (heuristic reports only); LLM API clients build a connection pool each; polars and
/// torch read and write files, polars downloads URLs and runs queries, and `torch.cat`
/// reallocates its accumulator.
#[test]
fn api_clients_task_queues_and_data_libraries() {
    assert_eq!(
        check(
            "catalog_api_clients",
            "app/apis.py",
            API_CLIENTS_AND_DATA,
            "EFF101,EFF102,EFF103,EFF105,DATA101"
        ),
        vec![
            d(
                17,
                "EFF101 [heuristic]",
                "tp_celery",
                "celery.Celery.send_task"
            ),
            d(18, "EFF101 [heuristic]", "tp_celery", "celery.Task.delay"),
            d(23, "EFF102", "tp_clients_in_loops", "openai.OpenAI"),
            d(24, "EFF102", "tp_clients_in_loops", "anthropic.Anthropic"),
            d(25, "EFF102", "tp_clients_in_loops", "google.genai.Client"),
            d(
                44,
                "EFF102",
                "tp_clients_with_unknown_http_client",
                "openai.OpenAI"
            ),
            d(
                49,
                "EFF102",
                "tp_async_clients_in_loops",
                "openai.AsyncOpenAI"
            ),
            d(
                50,
                "EFF102",
                "tp_async_clients_in_loops",
                "anthropic.AsyncAnthropic"
            ),
            d(
                61,
                "EFF101 [heuristic]",
                "tp_polars",
                "polars.read_database"
            ),
            d(62, "EFF101", "tp_polars", "polars.read_csv"),
            d(63, "EFF105", "tp_polars", "polars.read_parquet"),
            d(64, "EFF101", "tp_polars", "polars.read_database"),
            d(64, "EFF101", "tp_polars", "sqlite3.connect"),
            d(78, "DATA101", "tp_torch_accumulation", "acc"),
            d(90, "EFF105", "tp_torch_io", "torch.save"),
            d(91, "EFF105", "tp_torch_io", "torch.load"),
        ]
    );
}

const LLM_REQUESTS: &str = r#"from anthropic import Anthropic, AsyncAnthropic
from google import genai
from openai import AsyncOpenAI, AzureOpenAI, OpenAI

client = OpenAI()
aclient = AsyncOpenAI()
claude = Anthropic()
gemini = genai.Client()


async def tp_openai(prompt):
    client.chat.completions.create(model="m", messages=[])
    client.responses.create(model="m", input=prompt)
    client.embeddings.create(model="m", input=prompt)
    client.with_options(timeout=5).chat.completions.create(model="m", messages=[])


async def tp_azure(azure: AzureOpenAI):
    azure.chat.completions.create(model="m", messages=[])


async def tp_anthropic():
    claude.messages.create(model="m", max_tokens=1, messages=[])


async def tp_genai():
    gemini.models.generate_content(model="m", contents="x")
    gemini.models.embed_content(model="m", contents="x")


async def tn_async_clients(prompt):
    await aclient.chat.completions.create(model="m", messages=[])
    await aclient.responses.create(model="m", input=prompt)
    await aclient.with_options(timeout=5).embeddings.create(model="m", input=prompt)
    await AsyncAnthropic().messages.create(model="m", max_tokens=1, messages=[])
    await gemini.aio.models.generate_content(model="m", contents="x")


async def tn_untyped(make, wrapped):
    make().chat.completions.create(model="m", messages=[])
    wrapped.chat.completions.create(model="m", messages=[])


async def tn_either(c: OpenAI | AsyncOpenAI):
    c.chat.completions.create(model="m", messages=[])


def tn_sync_caller():
    client.chat.completions.create(model="m", messages=[])
"#;

/// The LLM SDKs' requests are reached through the clients' attributes: the synchronous
/// clients' requests block the event loop, the asynchronous clients' (`AsyncOpenAI`,
/// `AsyncAnthropic`, google-genai's `aio`) never do, and a client of unknown or mixed class
/// types nothing.
#[test]
fn llm_requests_block_through_the_synchronous_clients_attributes() {
    let create = "openai.resources.chat.Completions.create";
    assert_eq!(
        check(
            "catalog_llm_requests",
            "app/llm.py",
            LLM_REQUESTS,
            "EFF101,EFF102"
        ),
        vec![
            d(12, "EFF101", "tp_openai", create),
            d(
                13,
                "EFF101",
                "tp_openai",
                "openai.resources.responses.Responses.create"
            ),
            d(
                14,
                "EFF101",
                "tp_openai",
                "openai.resources.Embeddings.create"
            ),
            d(15, "EFF101", "tp_openai", create),
            d(19, "EFF101", "tp_azure", create),
            d(
                23,
                "EFF101",
                "tp_anthropic",
                "anthropic.resources.Messages.create"
            ),
            d(
                27,
                "EFF101",
                "tp_genai",
                "google.genai.models.Models.generate_content"
            ),
            d(
                28,
                "EFF101",
                "tp_genai",
                "google.genai.models.Models.embed_content"
            ),
        ]
    );
}

/// A project with one effect boundary per effect, over every function of `pkg.probe`.
const PROBE_BOUNDARIES: &str = r#"[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "fs"
scopes = ["pkg.probe:*"]
forbid = ["blocking-fs"]

[[tool.heki.effect-boundaries]]
name = "sub"
scopes = ["pkg.probe:*"]
forbid = ["blocking-subprocess"]

[[tool.heki.effect-boundaries]]
name = "env"
scopes = ["pkg.probe:*"]
forbid = ["reads-environment"]

[[tool.heki.effect-boundaries]]
name = "net"
scopes = ["pkg.probe:*"]
forbid = ["blocking-net"]

[[tool.heki.effect-boundaries]]
name = "sync"
scopes = ["pkg.probe:*"]
forbid = ["blocking-sync"]
"#;

/// `(boundary, function)` of every `EFF201` report of the module `source` at `pkg/probe.py`.
fn probe_reports(name: &str, source: &str) -> Vec<(String, String)> {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!("[project]\nname = \"catalog\"\nversion = \"0\"\n\n{PROBE_BOUNDARIES}"),
    );
    p.write("pkg/__init__.py", "");
    p.write("pkg/probe.py", source);
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut found: Vec<(String, String)> = diags
        .iter()
        .map(|d| {
            let message = d["message"].as_str().unwrap();
            let boundary = ["fs", "sub", "env", "net", "sync"]
                .into_iter()
                .find(|b| message.contains(&format!("`{b}`")))
                .unwrap_or_else(|| panic!("no boundary in {d:#}"));
            (
                boundary.to_owned(),
                d["qualname"]
                    .as_str()
                    .unwrap()
                    .rsplit('.')
                    .next()
                    .unwrap()
                    .to_owned(),
            )
        })
        .collect();
    found.sort();
    found.dedup();
    found
}

fn reported(found: &[(String, String)], boundary: &str) -> Vec<String> {
    found
        .iter()
        .filter(|(b, _)| b == boundary)
        .map(|(_, f)| f.clone())
        .collect()
}

const FILE_SYSTEM_ROWS: &str = r#"import glob
import os
import os.path
import shutil
import tempfile
from pathlib import Path


def f_path_iterdir(p: Path) -> list[Path]:
    return list(p.iterdir())


def f_path_glob(p: Path) -> list[Path]:
    return list(p.glob("*.json"))


def f_path_rglob(p: Path) -> list[Path]:
    return [q for q in p.rglob("*.json")]


def f_path_mkdir(p: Path) -> None:
    p.mkdir(parents=True)


def f_path_touch(p: Path) -> None:
    p.touch()


def f_path_unlink(p: Path) -> None:
    p.unlink()


def f_path_rename(p: Path, q: Path) -> Path:
    return p.rename(q)


def f_path_replace(p: Path, q: Path) -> Path:
    return p.replace(q)


def f_path_symlink_to(p: Path, q: Path) -> None:
    p.symlink_to(q)


def f_path_readlink(p: Path) -> Path:
    return p.readlink()


def f_path_is_symlink(p: Path) -> bool:
    return p.is_symlink()


def f_os_replace(a: str, b: str) -> None:
    os.replace(a, b)


def f_os_open(p: str) -> int:
    return os.open(p, os.O_RDONLY)


def f_os_remove(p: str) -> None:
    os.remove(p)


def f_os_makedirs(p: str) -> None:
    os.makedirs(p, exist_ok=True)


def f_os_chmod(p: str) -> None:
    os.chmod(p, 0o600)


def f_os_symlink(a: str, b: str) -> None:
    os.symlink(a, b)


def f_os_lstat(p: str) -> object:
    return os.lstat(p)


def f_getsize(p: str) -> int:
    return os.path.getsize(p)


def f_getmtime(p: str) -> float:
    return os.path.getmtime(p)


def f_islink(p: str) -> bool:
    return os.path.islink(p)


def f_shutil_move(a: str, b: str) -> object:
    return shutil.move(a, b)


def f_shutil_copyfile(a: str, b: str) -> object:
    return shutil.copyfile(a, b)


def f_shutil_disk_usage(p: str) -> object:
    return shutil.disk_usage(p)


def f_shutil_make_archive(p: str, root: str) -> str:
    return shutil.make_archive(p, "zip", root)


def f_tempfile_mkdtemp() -> str:
    return tempfile.mkdtemp()


def f_tempfile_mkstemp() -> object:
    return tempfile.mkstemp()


def f_tempfile_named() -> None:
    with tempfile.NamedTemporaryFile() as f:
        f.write(b"x")


def f_tempfile_directory() -> None:
    with tempfile.TemporaryDirectory() as d:
        print(d)


def f_glob_glob() -> list[str]:
    return glob.glob("*.py")


def f_glob_iglob() -> list[str]:
    return list(glob.iglob("*.py"))


def g_path_pure(p: Path) -> Path:
    return p / "a" / "b"


def g_path_name(p: Path) -> str:
    return p.with_suffix(".txt").name


def g_path_not_iterated(p: Path) -> object:
    return p.glob("*.json")


def g_path_as_posix(p: Path) -> str:
    return p.as_posix()


def g_os_path_join(a: str, b: str) -> str:
    return os.path.join(os.path.dirname(a), b)


def g_os_path_split(p: str) -> tuple[str, str]:
    return os.path.splitext(os.path.basename(p))


def g_shutil_copyfileobj(src, dst) -> None:
    shutil.copyfileobj(src, dst)
"#;

#[test]
fn file_system_rows_cover_the_modules_not_call_by_call() {
    let found = probe_reports("cat_fs_rows", FILE_SYSTEM_ROWS);
    let fs = reported(&found, "fs");
    let expected: Vec<String> = FILE_SYSTEM_ROWS
        .lines()
        .filter_map(|l| l.strip_prefix("def f_"))
        .map(|l| format!("f_{}", l.split('(').next().unwrap()))
        .collect();
    let mut sorted = expected.clone();
    sorted.sort();
    assert_eq!(fs, sorted);
    assert!(found.iter().all(|(b, _)| b == "fs"), "{found:?}");
}

const PROCESS_ROWS: &str = r#"import os
import subprocess


def f_os_system() -> None:
    os.system("ruff")


def f_os_spawnl_wait() -> int:
    return os.spawnl(os.P_WAIT, "/bin/ls", "ls")


def f_os_spawnvp_wait(argv: list[str]) -> int:
    return os.spawnvp(os.P_WAIT, "ls", argv)


def f_os_waitpid(pid: int) -> object:
    return os.waitpid(pid, 0)


def f_os_wait() -> object:
    return os.wait()


def f_getoutput() -> str:
    return subprocess.getoutput("ls")


def f_getstatusoutput() -> object:
    return subprocess.getstatusoutput("ls")


def f_run(argv: list[str]) -> object:
    return subprocess.run(argv)


def g_os_execvpe(argv: list[str]) -> None:
    os.execvpe("ls", argv, {})


def g_os_execv(argv: list[str]) -> None:
    os.execv("/bin/ls", argv)


def g_os_spawnl_nowait() -> int:
    return os.spawnl(os.P_NOWAIT, "/bin/ls", "ls")


def g_os_spawnv_detach(argv: list[str]) -> int:
    return os.spawnv(os.P_DETACH, "/bin/ls", argv)


def g_os_posix_spawn(argv: list[str]) -> int:
    return os.posix_spawn("/bin/ls", argv, {})


def g_os_popen() -> object:
    return os.popen("ls")


def g_os_fork() -> int:
    return os.fork()


def g_waitpid_nohang(pid: int) -> object:
    return os.waitpid(pid, os.WNOHANG)


def g_popen_spawns_without_waiting(argv: list[str]) -> object:
    return subprocess.Popen(argv)


def g_polled(child: subprocess.Popen) -> object:
    return child.poll()
"#;

#[test]
fn only_calls_that_wait_for_a_child_block() {
    let found = probe_reports("cat_process_rows", PROCESS_ROWS);
    assert_eq!(
        reported(&found, "sub"),
        [
            "f_getoutput",
            "f_getstatusoutput",
            "f_os_spawnl_wait",
            "f_os_spawnvp_wait",
            "f_os_system",
            "f_os_wait",
            "f_os_waitpid",
            "f_run",
        ]
    );
    assert!(found.iter().all(|(b, _)| b == "sub"), "{found:?}");
}

#[test]
fn replacing_or_starting_a_process_is_no_wait_on_the_event_loop() {
    let found = check(
        "cat_exec_async",
        "app.py",
        r#"import os


async def tn_exec(argv):
    os.execvpe("ls", argv, {})


async def tp_system():
    os.system("ruff")
"#,
        "EFF101",
    );
    assert_eq!(found, vec![d(9, "EFF101", "tp_system", "os.system")]);
}

const ENVIRONMENT_ROWS: &str = r#"import os
import os.path
from pathlib import Path


def f_path_cwd() -> Path:
    return Path.cwd()


def f_path_home() -> Path:
    return Path.home()


def f_path_absolute(p: Path) -> Path:
    return p.absolute()


def f_path_expanduser(p: Path) -> Path:
    return p.expanduser()


def f_os_getcwd() -> str:
    return os.getcwd()


def f_os_getcwdb() -> bytes:
    return os.getcwdb()


def f_os_path_abspath(p: str) -> str:
    return os.path.abspath(p)


def f_os_path_relpath(p: str) -> str:
    return os.path.relpath(p)


def f_os_path_expanduser(p: str) -> str:
    return os.path.expanduser(p)


def f_os_path_expandvars(p: str) -> str:
    return os.path.expandvars(p)


def g_normpath(p: str) -> str:
    return os.path.normpath(p)


def g_path_parent(p: Path) -> Path:
    return p.parent
"#;

#[test]
fn the_working_directory_and_the_home_are_environment_reads_not_file_system() {
    let found = probe_reports("cat_environment_rows", ENVIRONMENT_ROWS);
    assert_eq!(
        reported(&found, "env"),
        [
            "f_os_getcwd",
            "f_os_getcwdb",
            "f_os_path_abspath",
            "f_os_path_expanduser",
            "f_os_path_expandvars",
            "f_os_path_relpath",
            "f_path_absolute",
            "f_path_cwd",
            "f_path_expanduser",
            "f_path_home",
        ]
    );
    assert!(found.iter().all(|(b, _)| b == "env"), "{found:?}");
}

const MORE_BLOCKING: &str = r#"import ftplib
import signal

import fsspec
import psutil


def f_sigwait() -> int:
    return signal.sigwait({signal.SIGUSR1})


def g_sigtimedwait_at_once() -> object:
    return signal.sigtimedwait({signal.SIGUSR1}, 0)


def f_ftp(ftp: ftplib.FTP) -> list[str]:
    return ftp.nlst()


def f_ftp_login(host: str) -> None:
    ftplib.FTP_TLS(host).login()


def f_psutil_children(pid: int) -> object:
    return psutil.Process(pid).children()


def f_psutil_memory() -> object:
    return psutil.virtual_memory()


def f_psutil_iter() -> list[object]:
    return [p for p in psutil.process_iter()]


def f_fsspec_open(path: str) -> bytes:
    with fsspec.open(path) as f:
        return f.read()


def f_fsspec_cat(path: str) -> bytes:
    return fsspec.filesystem("s3").cat(path)


def f_fsspec_ls(fs: fsspec.AbstractFileSystem, path: str) -> list[object]:
    return fs.ls(path)


def g_psutil_wait_at_once(p: psutil.Process) -> object:
    return p.wait(0)
"#;

#[test]
fn signal_ftplib_psutil_and_fsspec_have_rows() {
    let found = probe_reports("cat_more_blocking", MORE_BLOCKING);
    assert_eq!(reported(&found, "sync"), ["f_sigwait"]);
    assert_eq!(reported(&found, "net"), ["f_ftp", "f_ftp_login"]);
    assert_eq!(
        reported(&found, "fs"),
        [
            "f_fsspec_cat",
            "f_fsspec_ls",
            "f_fsspec_open",
            "f_psutil_children",
            "f_psutil_iter",
            "f_psutil_memory",
        ]
    );
    assert!(found.iter().all(|(_, f)| !f.starts_with("g_")), "{found:?}");
}

const LAZY_AND_CONNECTING: &str = r#"import ftplib
from pathlib import Path

import fsspec
import urllib3.poolmanager


def f_iterdir_returned(p: Path) -> object:
    return p.iterdir()


def f_ftp_with_host() -> object:
    return ftplib.FTP("ftp.example.com")


def f_ftp_tls_with_host() -> object:
    return ftplib.FTP_TLS("ftp.example.com")


def g_ftp_without_host() -> object:
    return ftplib.FTP()


def g_mlsd_returned(ftp: ftplib.FTP) -> object:
    return ftp.mlsd()


def f_mlsd_consumed(ftp: ftplib.FTP) -> list[object]:
    return list(ftp.mlsd())


def g_fsspec_open_returned(path: str) -> object:
    return fsspec.open(path)


def f_fsspec_open_entered(path: str) -> bytes:
    with fsspec.open(path) as f:
        return f.read()


def f_fsspec_open_method(path: str) -> object:
    return fsspec.open(path).open()


def f_fsspec_ftp() -> object:
    return fsspec.filesystem("ftp")


def g_fsspec_memory() -> object:
    return fsspec.filesystem("memory")


def g_fsspec_walk_returned(fs: fsspec.AbstractFileSystem, path: str) -> object:
    return fs.walk(path)


def f_fsspec_walk_consumed(fs: fsspec.AbstractFileSystem, path: str) -> list[object]:
    return list(fs.walk(path))


def g_pool_from_manager_does_no_io(pm: urllib3.poolmanager.PoolManager) -> object:
    return pm.connection_from_host("example.com")
"#;

#[test]
fn lazy_results_wait_where_they_are_used_and_constructors_that_connect_wait() {
    let found = probe_reports("cat_lazy_and_connecting", LAZY_AND_CONNECTING);
    assert_eq!(
        reported(&found, "fs"),
        [
            "f_fsspec_open_entered",
            "f_fsspec_open_method",
            "f_fsspec_walk_consumed",
            "f_iterdir_returned",
        ]
    );
    assert_eq!(
        reported(&found, "net"),
        [
            "f_fsspec_ftp",
            "f_ftp_tls_with_host",
            "f_ftp_with_host",
            "f_mlsd_consumed",
        ]
    );
    assert!(found.iter().all(|(_, f)| !f.starts_with("g_")), "{found:?}");
}

const CATALOG_IN_ASYNC: &str = r#"import ftplib
import glob
import os
import shutil
import signal
import subprocess
import tempfile
from pathlib import Path

import fsspec
import psutil


async def tp_fs(p: Path, a: str, b: str):
    p.mkdir()
    list(p.iterdir())
    os.replace(a, b)
    shutil.move(a, b)
    tempfile.mkdtemp()
    glob.glob("*")


async def tp_process():
    os.system("ruff")
    subprocess.getoutput("ls")


async def tp_waits(pid: int, ftp: ftplib.FTP):
    signal.sigwait({signal.SIGUSR1})
    psutil.Process(pid).children()
    with fsspec.open("s3://bucket/key"):
        pass
    ftp.nlst()


async def tn_environment(p: str):
    os.getcwd()
    os.path.abspath(p)
    Path.cwd()
    Path.home()


async def tn_pure(p: Path):
    p.with_suffix(".txt")
    os.path.join("a", "b")
"#;

#[test]
fn the_new_rows_block_the_event_loop() {
    let found: Vec<(u64, String, String)> = check(
        "cat_async_rows",
        "app.py",
        CATALOG_IN_ASYNC,
        "EFF101,EFF105",
    )
    .into_iter()
    .map(|(l, r, q, t)| (l, format!("{r} {q}"), t))
    .collect();
    let by_function = |name: &str| -> Vec<&str> {
        found
            .iter()
            .filter(|(_, rq, _)| rq.ends_with(name))
            .map(|(_, _, t)| t.as_str())
            .collect()
    };
    assert_eq!(
        by_function("tp_fs"),
        [
            "pathlib.Path.mkdir",
            "pathlib.Path.iterdir",
            "os.replace",
            "shutil.move",
            "tempfile.mkdtemp",
            "glob.glob",
        ]
    );
    assert_eq!(
        by_function("tp_process"),
        ["os.system", "subprocess.getoutput"]
    );
    assert_eq!(
        by_function("tp_waits"),
        [
            "signal.sigwait",
            "psutil.Process",
            "psutil.Process.children",
            "fsspec.core.OpenFile.__enter__",
            "ftplib.FTP.nlst"
        ]
    );
    assert!(by_function("tn_environment").is_empty(), "{found:?}");
    assert!(by_function("tn_pure").is_empty(), "{found:?}");
}

const URLLIB3_POOLS: &str = r#"import urllib3
from urllib3 import PoolManager, ProxyManager
from urllib3.connectionpool import HTTPConnectionPool, connection_from_url


def via_pm(pm: urllib3.PoolManager, url):
    return pm.connection_from_host("example.org").urlopen("GET", url).data


def via_pm_url(pm: PoolManager, url):
    return pm.connection_from_url(url).request("GET", url).data


def via_pm_context(pm: PoolManager, url, context):
    return pm.connection_from_context(context).urlopen("GET", url)


def via_pm_kwargs(pm: PoolManager, url):
    return pm.connection_from_pool_kwargs({"host": "h"}).request("GET", url)


def via_proxy(pm: ProxyManager, url):
    return pm.connection_from_host("example.org").urlopen("GET", url)


def pool2(url):
    return urllib3.connectionpool.connection_from_url(url).request("GET", url).data


def pool3(url):
    return connection_from_url(url).request("GET", url).data


def pool4(url):
    return HTTPConnectionPool("h").urlopen("GET", url)


def pool_listed(url):
    return urllib3.connection_from_url(url).request("GET", url)


def proxy(url):
    return urllib3.proxy_from_url("http://proxy").request("GET", url)


def pure(pm: PoolManager):
    return pm.connection_from_host("example.org")


async def tp_pools(pm: PoolManager, url):
    pm.connection_from_host("example.org").urlopen("GET", url)
    urllib3.connectionpool.connection_from_url(url).request("GET", url)
"#;

#[test]
fn a_pool_taken_from_a_urllib3_manager_makes_network_calls() {
    let found = probe_reports(
        "cat_urllib3_pools",
        &URLLIB3_POOLS.replace("\n\nasync def tp_pools", "\n\ndef tp_pools"),
    );
    assert_eq!(
        reported(&found, "net"),
        [
            "pool2",
            "pool3",
            "pool4",
            "pool_listed",
            "proxy",
            "tp_pools",
            "via_pm",
            "via_pm_context",
            "via_pm_kwargs",
            "via_pm_url",
            "via_proxy",
        ]
    );
    let in_async = check("cat_urllib3_pools_async", "app.py", URLLIB3_POOLS, "EFF101");
    let targets: Vec<(&str, &str)> = in_async
        .iter()
        .filter(|(_, _, q, _)| q == "tp_pools")
        .map(|(_, r, _, t)| (r.as_str(), t.as_str()))
        .collect();
    assert_eq!(
        targets,
        [
            ("EFF101", "urllib3.HTTPConnectionPool.urlopen"),
            ("EFF101", "urllib3.HTTPConnectionPool.request"),
        ]
    );
}

const XINFERENCE: &str = r#"import asyncio

from xinference_client import AsyncRESTfulClient, RESTfulClient


async def tp_handles(url: str) -> None:
    client = RESTfulClient(url)
    model = client.get_model("chat")
    model.chat("hello")
    client.list_models()


async def tp_by_defining_path(url: str) -> None:
    from xinference_client.client.restful.restful_client import Client

    Client(url).launch_model("m")


def tn_sync_function(url: str) -> None:
    RESTfulClient(url).list_models()


async def tn_offloaded(client: RESTfulClient) -> None:
    await asyncio.to_thread(client.list_models)


async def tn_async_client(url: str) -> None:
    client = AsyncRESTfulClient(url)
    await client.list_models()
"#;

/// The synchronous Xinference client, its constructor, and the handles `get_model` gives wait on
/// the network; the asynchronous client and offloaded calls do not block the loop.
#[test]
fn xinference_client_calls_wait_on_the_network() {
    let found = check("catalog_xinference", "app/models.py", XINFERENCE, "EFF101");
    assert_eq!(
        found,
        vec![
            d(7, "EFF101", "tp_handles", "xinference_client.RESTfulClient"),
            d(
                8,
                "EFF101",
                "tp_handles",
                "xinference_client.RESTfulClient.get_model"
            ),
            d(
                9,
                "EFF101",
                "tp_handles",
                "xinference_client.client.restful.restful_client.RESTfulModelHandle.chat"
            ),
            d(
                10,
                "EFF101",
                "tp_handles",
                "xinference_client.RESTfulClient.list_models"
            ),
            d(
                16,
                "EFF101",
                "tp_by_defining_path",
                "xinference_client.client.restful.restful_client.Client"
            ),
            d(
                16,
                "EFF101",
                "tp_by_defining_path",
                "xinference_client.client.restful.restful_client.Client.launch_model"
            ),
        ]
    );
}

const SUPABASE: &str = r#"import asyncio

from supabase import Client, create_client


async def tp_query(client: Client, user_id: int) -> None:
    client.table("users").select("*").eq("id", user_id).execute()


async def tp_created(url: str, key: str) -> None:
    create_client(url, key).table("users").insert({"id": 1}).execute()


def tp_per_row(client: Client, ids: list[int]) -> None:
    for i in ids:
        client.table("users").update({"seen": True}).eq("id", i).execute()


def tn_one_statement(client: Client, ids: list[int]) -> None:
    client.table("users").select("*").in_("id", ids).execute()


async def tp_negated_filter(client: Client) -> None:
    client.table("users").select("*").not_.is_("name", "null").execute()


async def tn_negated_filter_not_sent(client: Client) -> None:
    client.table("users").select("*").not_.is_("name", "null")


async def tn_built_not_sent(client: Client) -> None:
    client.table("users").select("*").limit(10)


async def tn_offloaded(client: Client) -> None:
    query = client.table("users").select("*")
    await asyncio.to_thread(query.execute)
"#;

/// A table query waits for the PostgREST server when it is executed, not while it is built.
#[test]
fn supabase_queries_wait_when_executed() {
    let found = check(
        "catalog_supabase",
        "app/db.py",
        SUPABASE,
        "EFF101,EFF102,EFF103",
    );
    let execute = "postgrest._sync.request_builder.SyncSelectRequestBuilder.execute";
    assert_eq!(
        found,
        vec![
            d(7, "EFF101", "tp_query", execute),
            d(11, "EFF101", "tp_created", execute),
            d(16, "EFF103", "tp_per_row", execute),
            d(24, "EFF101", "tp_negated_filter", execute),
        ]
    );
}

const NUMPY_R_AND_C: &str = r#"import numpy as np


def tp_r(chunks: list[np.ndarray]) -> np.ndarray:
    arr = np.array([])
    for chunk in chunks:
        arr = np.r_[arr, chunk]
    return arr


def tp_c(columns: list[np.ndarray]) -> np.ndarray:
    grid = np.empty((3, 0))
    for column in columns:
        grid = np.c_[grid, column]
    return grid


def grow(acc: np.ndarray, x: np.ndarray) -> np.ndarray:
    return np.r_[acc, x]


def tp_through_helper(chunks: list[np.ndarray]) -> np.ndarray:
    arr = np.array([])
    for chunk in chunks:
        arr = grow(arr, chunk)
    return arr


def tn_once(chunks: list[np.ndarray]) -> np.ndarray:
    return np.r_[tuple(chunks)]


def tn_not_the_previous_value(chunks: list[np.ndarray]) -> np.ndarray:
    out = np.array([])
    for chunk in chunks:
        out = np.r_[chunk, chunk]
    return out
"#;

/// `numpy.r_[...]` and `numpy.c_[...]` concatenate their items like `numpy.hstack`: rebuilding the
/// accumulated array through them on every iteration is quadratic.
#[test]
fn numpy_r_and_c_concatenations_accumulate() {
    assert_eq!(
        check(
            "catalog_numpy_r_c",
            "app/arrays.py",
            NUMPY_R_AND_C,
            "DATA101"
        ),
        vec![
            d(7, "DATA101", "tp_r", "arr"),
            d(14, "DATA101", "tp_c", "grid"),
            d(25, "DATA101", "tp_through_helper", "arr"),
        ]
    );
}
