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
                d["rule_code"].as_str().unwrap().to_owned(),
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
"#;

#[test]
fn sqlalchemy_sync_apis_block_and_async_ones_only_count_queries() {
    assert_eq!(
        check("catalog_sqlalchemy", "app/db.py", SQLALCHEMY, RULES),
        vec![
            // `Engine.connect()` / `begin()` connect; `begin()` types its connection.
            d(
                29,
                "EFF101",
                "tp_engine",
                "sqlalchemy.engine.Engine.connect"
            ),
            d(
                30,
                "EFF101",
                "tp_engine",
                "sqlalchemy.engine.Connection.execute"
            ),
            d(31, "EFF101", "tp_engine", "sqlalchemy.engine.Engine.begin"),
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
                "EFF103",
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
                "EFF103",
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
    cur = sqlite3.connect(path).execute("SELECT 1")
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
            d(26, "EFF101", "tp_psycopg", "psycopg.Connection.commit"),
            d(
                27,
                "EFF101",
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
            d(41, "EFF101", "tp_sqlite", "sqlite3.Cursor.fetchone"),
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
                "EFF101",
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
