# Async and Execution Effects

heki finds blocking calls in async code, database queries and clients in loops, and lock and task hazards, keeps scopes free of an effect with `effect-boundaries`, and reports functions that gained an effect with `heki snapshot`.

An effect is what a call does besides returning a value: it waits on the network, sends a query, takes a lock, writes a field. heki follows effects through helper calls and across files, so a report names the function that does the work and the chain of calls that leads to it. [Rules](rules/index.md) has one page per rule, and [Contracts](configuration.md#contracts) has every contract key.

## 🔍 What runs without configuration

Three rules are exact and run by default, and two more are exact by default and are described with the problems they belong to. Their reports show the call chain, and an error fails `heki check`.

| Rule | Reports | Tier |
| --- | --- | --- |
| [`EFF101`](rules/async-blocking-call.md) | a call in an `async def` that always runs a network, database, subprocess, or sleep call that waits on the event loop, however many helpers below | exact, default |
| [`EFF102`](rules/loop-unpooled-client.md) | a loop over data whose every iteration makes an HTTP, database, or RPC client, or a sessionless request | exact, default |
| [`EFF103`](rules/loop-db-query.md) | a loop over data whose every iteration sends a database statement | exact, default |

`EFF102` and `EFF103` also have heuristic reports, where a premise of the claim is open (a loop that can make a client or run a query on every iteration). They appear with `--heuristic`.

`EFF103` knows Django's model managers from the ecosystem catalog. `Model.objects`, read on a model class or on `cls`, is the `Manager` Django adds, or the manager the model's body declares (`objects = StreamManager()`, a project subclass of `models.Manager`). Its query methods (`get`, `count`, `create`, `get_or_create`, `update`, ...) send a statement on every call, so calling one per item of a loop over data is an exact, default `EFF103` report; `filter`, `exclude`, and `all` only build a `QuerySet`. A method the manager adds is followed as project code. Some managers keep the report heuristic (with `--heuristic`) or unknown:

- a manager that binds a name its query methods use (`get_queryset`, `_queryset_class`, `__init__`) runs its own code under them: heuristic. So is a model's default `objects` while the project defines such a manager, since heki takes a `Manager` to be possibly an instance of any project subclass;
- a project `QuerySet` subclass (`models.QuerySet` is the catalog's `QuerySet`) that binds a name a query method looks up on the queryset (`get`'s `filter` and `_fetch_all`, `acount`'s `count`) runs its own code under that method: a call of it on a `QuerySet` is heuristic, since heki takes a `QuerySet` to be possibly an instance of any project subclass. A subclass that only adds methods keeps every row, on its own instances too;
- `create` and `acreate` save a new instance of the model: while a project model binds `save` (or a method `save` reaches, `save_base`, `_save_table`, ...) otherwise than as a method calling `super().save(...)` on every path, they are heuristic. `get_or_create` and `update_or_create` query by `get` first on every call, and `QuerySet.delete` never calls the model's `delete`, so a model's overrides leave them exact;
- a manager `from_queryset()` or `as_manager()` builds, one of a class outside the project, and related managers (`question.choice_set`, a many-to-many field read on an instance) are unknown: they add no effect.

`EFF109` (a lock taken again while it is held) is under [Exclusive use and lock safety](#a-lock-taken-again-while-it-is-held), and `EFF116` (a generator that nothing iterates) is on [State Ownership and Mutation](ownership.md#a-generator-that-is-never-iterated). Both are exact and run by default.

Two rules run by default only when you pass `--heuristic` (or set `lint.heuristic = true`), because they can be wrong about the code. They are about locks and cancellation: `EFF117` is under [Exclusive use and lock safety](#exclusive-use-and-lock-safety), and `EFF118` under [Resource and cancellation lifetimes](#resource-and-cancellation-lifetimes).

The knowledge base names the calls that wait. Besides `requests` and `httpx.Client`, it lists the synchronous clients of Xinference (`xinference_client.RESTfulClient` and the handles of its models) and of Supabase and PostgREST (`supabase.Client`, `postgrest.SyncPostgrestClient`): building a table query, including through `.not_`, sends nothing, and `execute()` waits for the server, so `EFF101` and `EFF103` report it there. Tier: exact, default ([`EFF101`](rules/async-blocking-call.md), [`EFF103`](rules/loop-db-query.md)).

`DATA101` (exact, default, [`loop-carried-concat`](rules/loop-carried-concat.md)) also reports an array grown through `numpy.r_[...]` or `numpy.c_[...]` on every iteration (`arr = np.r_[arr, chunk]`), also when a helper returns that concatenation (`arr = grow(arr, chunk)`), as it does for `numpy.hstack`.

A function handed to a thread or process pool runs once per item for these rules:
`executor.submit(f, ...)` in a loop counts as a call of `f` on every iteration, and
`executor.map(f, items)` as one call of `f` per item, for the `concurrent.futures` executors;
`loop.run_in_executor(executor, f, ...)` in a loop is the same. `EFF102` then reports a client `f`
makes per item, and `EFF103` a query `f` sends per item. Through a pool both reports are heuristic
(with `--heuristic`), since the pool runs `f` after `submit` returns. A worker in another process
(`ProcessPoolExecutor`, `anyio.to_process.run_sync`) runs on copies of its arguments, so what it
changes in them does not reach the caller, and rules that follow changes to the caller's objects
do not follow it there.

The first page of the docs shows [`EFF101` two helpers below an `async def` and `EFF103` one helper below a loop](index.md), with their output.

The chain a blocking-call report shows (`EFF101`, both tiers, and `EFF105`) is one real path to the call, chosen in this order: a path the call's arguments allow (a helper that dispatches on `isinstance(item, Response)` shows the branch an argument of known class takes), then a path on the normal flow before one through cleanup code (an `except` handler or a `finally` body), then the shortest. Which path is shown never decides whether the report exists or its tier. Output folds many reports into one (`(+N more)`) only when they reach the same blocking call through the same helper: sites that call one API directly still fold together, and sites that reach different blocking calls through different helpers stay separate groups.

## 🔧 Rules to switch on

These rules are off by default. Name one in `extend-select` or with `--extend-select EFF105`. A prefix such as `EFF` or `ALL` never enables them.

```toml
# heki.toml
[lint]
extend-select = ["EFF105", "EFF107"]
```

| Rule | Reports | Tier |
| --- | --- | --- |
| [`EFF105`](rules/async-blocking-fs.md) | a call in an `async def` that always runs blocking local file I/O on the event loop | exact, opt-in |
| [`EFF107`](rules/unretained-async-task.md) | an `asyncio.create_task` result bound to a local variable that is never awaited, returned, or stored; a call whose result is discarded is not reported | heuristic, opt-in |
| [`EFF112`](rules/set-order-output.md) | the iteration order of a set of strings, which changes between runs, reaching a file, a stream, or a serializer | heuristic, experimental, opt-in |
| [`EFF114`](rules/recursion-error-as-value.md) | an `except` around a recursion that takes the `RecursionError` and goes on, so the result depends on the stack depth | heuristic, experimental, opt-in |
| [`EFF115`](rules/async-def-as-sync-callback.md) | an `async def` handed to a library call that does not await it (a thread target, an executor function, a signal handler), so the body never runs | heuristic, experimental, opt-in |

Experimental rules can change or go away between releases. [Experimental Features](experimental.md) says why each one is marked.

## 🔒 Exclusive use and lock safety

Some objects allow one user at a time: a lock, a stream reader, a database connection. These rules report code that breaks that. The opt-in ones (`EFF106` and `EFF110`) are off until you name them in `extend-select`, for example `--extend-select EFF106`. A prefix such as `EFF` or `ALL` never enables them.

```toml
# heki.toml
[lint]
extend-select = ["EFF106", "EFF110"]
```

### An exclusive object used by tasks started together

A stream reader, an asyncpg connection, or a WebSocket runs one operation at a time. `EFF110` reports tasks started together (`asyncio.gather`, a task group) that all use the same one, so the second call raises. Tier: heuristic, experimental, opt-in ([`EFF110`](rules/exclusive-object-fanout.md)).

```python
import asyncio


class FrameClient:
    def __init__(self, reader: asyncio.StreamReader) -> None:
        self._reader = reader

    async def read_frame(self) -> bytes:
        return await self._reader.readexactly(4)

    async def read_frames(self, count: int) -> list[bytes]:
        return await asyncio.gather(*(self.read_frame() for _ in range(count)))  # EFF110
```

heki reports: `FrameClient.read_frames` starts a task for every item, each using `self._reader` (`asyncio.gather`); `asyncio.StreamReader` allows one operation at a time, so the second task's call raises. Read the frames one after another instead.

### A lock taken again while it is held

`threading.Lock` and `asyncio.Lock` are not reentrant. A method that holds the lock and calls another method that takes it waits for itself. `EFF109` reports the second take, directly or through a call. Tier: exact, default; its heuristic reports, where a helper takes the lock on some paths only, appear with `--heuristic` ([`EFF109`](rules/lock-reacquired-while-held.md)).

```python
# app/m.py
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
            return self.size()  # EFF109
```

heki reports: `Registry.add` calls `app.m.Registry.size`, which takes `self._lock` while `Registry.add` holds it (taken at line 14); `threading.Lock` is not reentrant, so this waits forever. Move the body of `size` into a helper that does not take the lock, and call that one.

### A write without the lock its other writes hold

When one method changes a field holding a lock, every method that changes it needs the same lock. `EFF117` reports a write that holds none while another write of the same state holds a thread lock, and a thread heki resolves can run one of them. An update can be lost. Tier: heuristic, runs with `--heuristic` ([`EFF117`](rules/unlocked-write-of-guarded-state.md)).

```python
import threading


class Stats:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self.received = 0

    def start(self) -> None:
        threading.Thread(target=self._read).start()

    def _read(self) -> None:
        with self._lock:
            self.received += 1

    def reset(self) -> None:
        self.received = 0  # EFF117
```

heki reports: `Stats.reset` assigns `self.received` where heki finds no lock held, while `Stats._read` assigns it holding `self._lock`; `Stats._read` can run on another thread, so the two writes can interleave and one can lose the other's update. Take the lock in `reset` too.

### I/O while holding a lock

A lock held across a network call or an `await` makes every other user of the lock wait for the slow call. `EFF106` reports an `await` or blocking network I/O while a `threading` lock or a database transaction is held. Tier: heuristic, opt-in ([`EFF106`](rules/lock-or-tx-held-across-io.md)).

```python
# app/sync.py
import threading

import requests

_lock = threading.Lock()


def refresh(url: str) -> str:
    with _lock:
        return requests.get(url, timeout=5).text
```

```console
$ heki check
app/sync.py:10:16: EFF106 (lock-or-tx-held-across-io) [error, heuristic] `refresh` calls `requests.get` (blocking network I/O) while holding the synchronous `threading.Lock` taken at line 9
   |
10 |         return requests.get(url, timeout=5).text
   |                ^
  = related: app/sync.py:10:16: `refresh` calls `requests.get`
  = help: make the request before taking the lock and hold it only for the update

Found 1 diagnostic(s) in 2 file(s).
...
```

Make the request before taking the lock and hold it only for the update.

## ⏳ Resource and cancellation lifetimes

Work around a resource can end early. A cancelled task stops at its next checkpoint, and cleanup that runs after the cancellation can be stopped the same way, so the resource is never released. Each rule in this part reports one way the end of a scope leaves a resource in the wrong state. The rules are listed as entries of their own, so a new one is one more entry.

### Cleanup that stops at a cancellation checkpoint

A `finally` body, an `except` that takes the cancellation, or an `__aexit__` that awaits a call reaching an AnyIO or Trio cancellation checkpoint is cut short in a cancelled scope: the await raises the cancellation again and the rest of the cleanup does not run. `EFF118` reports it when no shield is on the way. A shield is a `CancelScope` with `shield=True` by the public name (`anyio.CancelScope`) or by the path that defines it (`anyio._core._tasks.CancelScope`). Tier: heuristic, runs with `--heuristic` ([`EFF118`](rules/unshielded-cleanup-checkpoint.md)).

```python
# app/m.py
import anyio


async def delete_group(name: str) -> None:
    await anyio.to_thread.run_sync(delete_container_group, name)


async def run(name: str) -> None:
    try:
        await watch(name)
    finally:
        await delete_group(name)  # EFF118
        log_deleted(name)
```

heki reports: `run` awaits `app.m.delete_group` in a `finally` body, which reaches the cancellation checkpoint `anyio.to_thread.run_sync` 2 calls down with no shield on the way: in a cancelled scope the await raises the cancellation and the rest of the cleanup does not run. Shield the cleanup:

```python
    finally:
        with anyio.CancelScope(shield=True):
            await delete_group(name)
        log_deleted(name)
```

## 📜 Contracts for execution effects

`effect-boundaries` marks code that must stay free of an effect: a render function that makes no blocking call, a decision pass that writes no model state, a generator that does not read the environment. Any function in scope that reaches the effect, directly or through calls, is reported as [`EFF201`](rules/effect-boundary-violation.md) (heuristic, contract).

```toml
# heki.toml
[[effect-boundaries]]
name = "pure-render"
scopes = ["app.render.*"]
forbid = ["blocking"]
message = "render from data the caller already loaded"
fail = true
```

`render_page` in `app/render.py` calls `load_title`, which calls `requests.get`:

```console
$ heki check
app/render.py:5:19: EFF201 (effect-boundary-violation) [error, heuristic] `render_page` reaches blocking_net (`requests.get` via `app.loader.load_title`), which effect boundary `pure-render` forbids
  |
5 |     return f"<h1>{load_title(page_id)}</h1>"
  |                   ^
  = related: app/render.py:5:19: `render_page` calls `app.loader.load_title`
  = related: app/loader.py:5:12: `load_title` calls `requests.get`
  = help: render from data the caller already loaded

Found 1 diagnostic(s) in 3 file(s).
```

The run exits with code 1. `forbid` takes tokens such as `blocking`, `blocking-net`, `blocking-db`, `sleep`, `mutates-params`, `writes-targets`, `reads-targets`, `calls`, and `reads-environment`. [`effect-boundaries`](configuration.md#effect-boundaries) lists them all with their extra keys.

A `scopes` or `allowed-callees` entry naming a method (`Class.method`) names that method only. With `include-overrides = true` in the boundary (or in its [defaults table](configuration.md#defaults-for-the-tables-of-a-kind)) it also names the overrides in the project subclasses of `Class`, nested scopes included, so a subclass written later is in scope or allowed with no change of the configuration. The key is off by default for that reason.

## 📸 Public API effect changes across pull requests

A change in what a public function does is worth a review even when no rule fires: it starts changing an argument, or doing network I/O. `heki snapshot` saves the effect summaries of the project, and `MUT103` ([`summary-effect-regression`](rules/summary-effect-regression.md), heuristic, opt-in) reports each function that gained an effect since.

```console
$ heki snapshot --write    # writes .heki-summary-snapshot.json
```

Commit the file. Later, `total` starts to sort its argument, and so does `checkout`, which calls it:

```console
$ heki check --select MUT103 --fail-on warning
app/billing.py:1:1: MUT103 (summary-effect-regression) [error, heuristic] `total` gained side effects since the summary snapshot: mutates parameter `prices`
  |
1 | def total(prices: list[int]) -> int:
  | ^
  = help: if the change is intended, refresh the snapshot with `heki snapshot --write`

app/billing.py:6:1: MUT103 (summary-effect-regression) [error, heuristic] `checkout` gained side effects since the summary snapshot: mutates parameter `prices`
  |
6 | def checkout(prices: list[int]) -> int:
  | ^
  = help: if the change is intended, refresh the snapshot with `heki snapshot --write`

Found 2 diagnostic(s) in 2 file(s).
```

The run exits with code 1. When the new effect is intended, refresh the snapshot in the same pull request, so the diff of the JSON file shows reviewers what changed. A pull request can also be compared with a snapshot of its base branch, with `heki check --snapshot PATH`. [Effect snapshots](adoption.md#effect-snapshots-mut103) has both workflows.

## 👉 Next steps

- [Configuration: effect-boundaries](configuration.md#effect-boundaries) lists every key and `forbid` token.
- [Adopting heki in a code base](adoption.md) shows how to start with a baseline, and has the [blocking and database-write contracts of a web service](adoption.md#an-async-web-or-database-service) to copy.
- `heki inspect app.render.render_page` (experimental) shows what one function reaches, with the call chains. See [Coding agents](coding-agents.md#what-a-function-does-heki-inspect-experimental).
- [State Ownership and Mutation](ownership.md) covers the contracts about who may change, read, or call a thing, and the rules for borrowed, shared, and single-use values.
