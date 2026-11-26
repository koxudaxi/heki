"""Recording fakes for the catalog calls that the exact claims name.

`install(recorder)` replaces the library calls a generated program can make with fakes that run
offline and report every call to `recorder.fake(...)`. Each fake behaves as the ecosystem catalog
describes the library (assumption R3): `time.sleep(0)` returns at once, an in-memory SQLite
connection runs its statements without waiting, `requests.get` always goes to the network.

Every event carries:

- `api`: the catalog symbol of the call (`time.sleep`, `sqlite3.Cursor.execute`);
- `kind`: `sleep`, `net`, `subprocess`, `db_stmt` (a statement sent to a database), `db_other`
  (a database call that sends no statement), `thread` (a wait on another thread), `realloc`
  (a call that copies its inputs into a new array or frame), or `lock` (a `threading.Lock` or
  `asyncio.Lock` taken, given back, or waited for);
- `waits`: whether this call waited on the calling thread for input or output as `EFF101` means
  it (a nonzero sleep, the network, a child process, a database file); a wait on another thread
  is not such a wait;
- `detail`: the argument that decided `waits`, for the report; for a lock, `[identity of the
  lock, what happened]` (`taken`, `released`, `busy`: a non-blocking or timed attempt that failed,
  or `self`: a blocking attempt on a lock the run holds);
- `inputs` / `output`: the arrays or frames a reallocation read and the one it returned.
"""

from __future__ import annotations

import _thread
import asyncio as _asyncio
import ipaddress
import queue as _queue
import socket as _socket
import sqlite3 as _sqlite3
import subprocess as _subprocess
import sys
import threading as _threading
import time as _time
import types
import urllib.request as _urllib_request

RECORDER = None
# The number of rows a query returns, set per run by the driver.
ROWS = 3


def _record(api, kind, waits, detail=None, inputs=(), output=None):
    if RECORDER is not None:
        RECORDER.fake(api, kind, waits, detail, inputs, output)


# ---------------------------------------------------------------------------------------------
# time
# ---------------------------------------------------------------------------------------------
def fake_sleep(secs):
    if secs < 0:
        raise ValueError("sleep length must be non-negative")
    _record("time.sleep", "sleep", secs != 0, repr(secs))


# ---------------------------------------------------------------------------------------------
# subprocess
# ---------------------------------------------------------------------------------------------
def _subprocess_fake(name):
    def run(args, *rest, **kwargs):
        _record(f"subprocess.{name}", "subprocess", True, repr(args))
        if name == "run":
            return _subprocess.CompletedProcess(args, 0, stdout=b"", stderr=b"")
        if name == "check_output":
            return b""
        return 0

    run.__name__ = name
    return run


# ---------------------------------------------------------------------------------------------
# socket
# ---------------------------------------------------------------------------------------------
def _resolves_offline(host):
    if host is None or host == "localhost":
        return True
    try:
        ipaddress.ip_address(host)
    except ValueError:
        return False
    return True


def fake_getaddrinfo(host, port, family=0, type=0, proto=0, flags=0):
    offline = _resolves_offline(host) or bool(flags & _socket.AI_NUMERICHOST)
    _record("socket.getaddrinfo", "net", not offline, repr(host))
    return [(_socket.AF_INET, _socket.SOCK_STREAM, 6, "", ("127.0.0.1", port or 0))]


def fake_gethostbyname(host):
    _record("socket.gethostbyname", "net", not _resolves_offline(host), repr(host))
    return "127.0.0.1"


def fake_getfqdn(name=""):
    _record("socket.getfqdn", "net", True, repr(name))
    return "localhost"


# ---------------------------------------------------------------------------------------------
# urllib.request
# ---------------------------------------------------------------------------------------------
class FakeResponse:
    def __init__(self, url, status=200):
        self.url = url
        self.status_code = status
        self.status = status
        self.text = f"body of {url}"
        self.content = self.text.encode()

    def json(self):
        return {"url": self.url}

    def read(self):
        return self.content

    def raise_for_status(self):
        return None

    def close(self):
        return None

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False


def fake_urlopen(url, data=None, timeout=None, **kwargs):
    text = url if isinstance(url, str) else getattr(url, "full_url", str(url))
    _record("urllib.request.urlopen", "net", not text.startswith("data:"), repr(text))
    if "fail" in text:
        raise OSError(f"cannot reach {text}")
    return FakeResponse(text)


# ---------------------------------------------------------------------------------------------
# requests (a module of its own: the real package is not needed)
# ---------------------------------------------------------------------------------------------
class RequestException(OSError):
    pass


class ConnectionError_(RequestException):
    pass


ConnectionError_.__name__ = "ConnectionError"
ConnectionError_.__qualname__ = "ConnectionError"


class HTTPError(RequestException):
    pass


def _request(api, method, url, **kwargs):
    _record(api, "net", True, repr(url))
    if "fail" in str(url):
        raise ConnectionError_(f"cannot reach {url}")
    return FakeResponse(url)


def _requests_function(method):
    def call(url, *args, **kwargs):
        return _request(f"requests.{method}", method, url, **kwargs)

    call.__name__ = method
    return call


class FakeSession:
    def _call(self, method, url, **kwargs):
        return _request(f"requests.Session.{method}", method, url, **kwargs)

    def get(self, url, **kwargs):
        return self._call("get", url, **kwargs)

    def post(self, url, **kwargs):
        return self._call("post", url, **kwargs)

    def put(self, url, **kwargs):
        return self._call("put", url, **kwargs)

    def delete(self, url, **kwargs):
        return self._call("delete", url, **kwargs)

    def close(self):
        return None

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False


def _requests_module():
    module = types.ModuleType("requests")
    for method in ("get", "post", "put", "delete", "patch", "head", "options"):
        setattr(module, method, _requests_function(method))

    def request(method, url, **kwargs):
        return _request("requests.request", method, url, **kwargs)

    module.request = request
    module.Session = FakeSession
    module.session = FakeSession
    module.Response = FakeResponse
    module.RequestException = RequestException
    module.ConnectionError = ConnectionError_
    module.HTTPError = HTTPError
    exceptions = types.ModuleType("requests.exceptions")
    exceptions.RequestException = RequestException
    exceptions.ConnectionError = ConnectionError_
    exceptions.HTTPError = HTTPError
    module.exceptions = exceptions
    return module, exceptions


# ---------------------------------------------------------------------------------------------
# sqlite3 (the real module keeps its exception classes; `connect` returns a fake connection)
# ---------------------------------------------------------------------------------------------
def _in_memory(database):
    text = str(database)
    return text == ":memory:" or text.startswith("file::memory:") or "mode=memory" in text


def _check_statement(sql, params):
    if "bad" in str(sql) or any(p == "bad" for p in _flat(params)):
        raise _sqlite3.OperationalError(f"near {sql!r}: syntax error")


def _flat(params):
    if isinstance(params, (list, tuple)):
        for p in params:
            if isinstance(p, (list, tuple)):
                yield from p
            else:
                yield p
    elif isinstance(params, dict):
        yield from params.values()


class FakeCursor:
    def __init__(self, connection):
        self.connection = connection
        self._rows = []
        self.rowcount = -1

    def _send(self, method, sql, params=()):
        _record(
            f"{self.connection.api_prefix}.Cursor.{method}",
            "db_stmt",
            self.connection.on_file,
            self.connection.detail,
        )
        _check_statement(sql, params)
        self._rows = [(i, f"row{i}") for i in range(ROWS)]
        self.rowcount = len(self._rows)
        return self

    def execute(self, sql, params=()):
        return self._send("execute", sql, params)

    def executemany(self, sql, seq):
        return self._send("executemany", sql, list(seq))

    def executescript(self, script):
        return self._send("executescript", script)

    def fetchone(self):
        _record(
            f"{self.connection.api_prefix}.Cursor.fetchone",
            "db_other",
            self.connection.on_file,
            self.connection.detail,
        )
        return self._rows.pop(0) if self._rows else None

    def fetchall(self):
        _record(
            f"{self.connection.api_prefix}.Cursor.fetchall",
            "db_other",
            self.connection.on_file,
            self.connection.detail,
        )
        rows, self._rows = self._rows, []
        return rows

    def __iter__(self):
        rows, self._rows = self._rows, []
        return iter(rows)

    def close(self):
        return None


class FakeConnection:
    api_prefix = "sqlite3"

    def __init__(self, database):
        self.database = database
        self.on_file = not _in_memory(database)
        self.detail = repr(database)

    def cursor(self):
        return FakeCursor(self)

    def _send(self, method, sql, params=()):
        _record(f"{self.api_prefix}.Connection.{method}", "db_stmt", self.on_file, self.detail)
        cursor = FakeCursor(self)
        _check_statement(sql, params)
        cursor._rows = [(i, f"row{i}") for i in range(ROWS)]
        return cursor

    def execute(self, sql, params=()):
        return self._send("execute", sql, params)

    def executemany(self, sql, seq):
        return self._send("executemany", sql, list(seq))

    def executescript(self, script):
        return self._send("executescript", script)

    def commit(self):
        _record(f"{self.api_prefix}.Connection.commit", "db_other", False, self.detail)

    def rollback(self):
        _record(f"{self.api_prefix}.Connection.rollback", "db_other", False, self.detail)

    def close(self):
        return None

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        return False


def fake_sqlite_connect(database, *args, **kwargs):
    _record("sqlite3.connect", "db_other", not _in_memory(database), repr(database))
    return FakeConnection(database)


# ---------------------------------------------------------------------------------------------
# psycopg2 (a module of its own): every connection is a network database
# ---------------------------------------------------------------------------------------------
class FakePgConnection(FakeConnection):
    api_prefix = "psycopg2.extensions"

    def __init__(self, dsn):
        super().__init__(dsn)
        self.on_file = True


def _psycopg2_module():
    module = types.ModuleType("psycopg2")

    def connect(dsn="", **kwargs):
        _record("psycopg2.connect", "db_other", True, repr(dsn))
        return FakePgConnection(dsn)

    module.connect = connect
    module.Error = _sqlite3.Error
    module.OperationalError = _sqlite3.OperationalError
    extensions = types.ModuleType("psycopg2.extensions")
    extensions.connection = FakePgConnection
    extensions.cursor = FakeCursor
    module.extensions = extensions
    module.__path__ = []
    return module


# ---------------------------------------------------------------------------------------------
# queue: waits on another thread; an empty queue raises instead of blocking the run forever
# ---------------------------------------------------------------------------------------------
class FakeQueue(_queue.Queue):
    def get(self, block=True, timeout=None):
        _record("queue.Queue.get", "thread", False, repr(block))
        return super().get(block=False)

    def put(self, item, block=True, timeout=None):
        _record("queue.Queue.put", "thread", False, repr(block))
        return super().put(item, block=False)


# ---------------------------------------------------------------------------------------------
# threading.Lock and asyncio.Lock: the runs have one thread and no event loop, so a lock that is
# held is held by the thread or task asking for it. A blocking attempt on it waits for itself
# (assumption R3: the locks are not reentrant): a thread waits for a short timeout, a task
# cannot wait without a loop; then the attempt raises `WaitsForItself` instead of blocking the
# run forever.
# ---------------------------------------------------------------------------------------------
LOCK_WAIT = 0.05


class WaitsForItself(BaseException):
    """A blocking attempt on a lock the run holds (`EFF109`'s claim). A `BaseException`, so that
    the program's handlers of `Exception` do not take it for one of its own."""


def _lock_event(api, lock, what):
    _record(api, "lock", False, [id(lock), what])


class FakeLock:
    __slots__ = ("_lock", "__weakref__")

    def __init__(self):
        self._lock = _thread.allocate_lock()

    def acquire(self, blocking=True, timeout=-1):
        if self._lock.acquire(False):
            _lock_event("threading.Lock.acquire", self, "taken")
            return True
        if not blocking or timeout >= 0:
            _lock_event("threading.Lock.acquire", self, "busy")
            return False
        _lock_event("threading.Lock.acquire", self, "self")
        if self._lock.acquire(True, LOCK_WAIT):
            _lock_event("threading.Lock.acquire", self, "taken")
            return True
        raise WaitsForItself()

    __enter__ = acquire

    def release(self):
        self._lock.release()
        _lock_event("threading.Lock.release", self, "released")

    def __exit__(self, *exc):
        self.release()

    def locked(self):
        return self._lock.locked()


class FakeAsyncLock(_asyncio.Lock):
    async def acquire(self):
        if self.locked():
            _lock_event("asyncio.Lock.acquire", self, "self")
            raise WaitsForItself()
        # Free: the base class takes it without a loop.
        await super().acquire()
        _lock_event("asyncio.Lock.acquire", self, "taken")
        return True

    def release(self):
        super().release()
        _lock_event("asyncio.Lock.release", self, "released")


# ---------------------------------------------------------------------------------------------
# numpy and pandas (modules of their own): arrays and frames as plain lists of values
# ---------------------------------------------------------------------------------------------
class FakeArray:
    __slots__ = ("data", "__weakref__")

    def __init__(self, data=()):
        self.data = list(data)

    def __len__(self):
        return len(self.data)

    def __iter__(self):
        return iter(self.data)

    def __getitem__(self, key):
        if isinstance(key, slice):
            return FakeArray(self.data[key])
        return self.data[key]

    def __repr__(self):
        return f"array({self.data!r})"

    def __eq__(self, other):
        return isinstance(other, FakeArray) and self.data == other.data

    __hash__ = None

    def _map(self, other, op):
        if isinstance(other, FakeArray):
            return FakeArray(op(a, b) for a, b in zip(self.data, other.data, strict=False))
        return FakeArray(op(a, other) for a in self.data)

    def __add__(self, other):
        return self._map(other, lambda a, b: a + b)

    def __sub__(self, other):
        return self._map(other, lambda a, b: a - b)

    def __mul__(self, other):
        return self._map(other, lambda a, b: a * b)

    @property
    def shape(self):
        return (len(self.data),)

    @property
    def size(self):
        return len(self.data)

    def tolist(self):
        return list(self.data)

    def sum(self):
        return sum(self.data)


def _values(obj):
    if isinstance(obj, FakeArray):
        return list(obj.data)
    if isinstance(obj, FakeFrame):
        return list(obj.rows)
    if isinstance(obj, (list, tuple)):
        out = []
        for item in obj:
            out.extend(_values(item) if isinstance(item, (FakeArray, list, tuple)) else [item])
        return out
    return [obj]


def _parts(seq):
    if isinstance(seq, (list, tuple)):
        return list(seq)
    return [seq]


def _realloc(api, parts, result):
    _record(api, "realloc", False, None, tuple(parts), result)
    return result


def _numpy_module():
    module = types.ModuleType("numpy")
    module.ndarray = FakeArray

    def array(data=(), dtype=None):
        return FakeArray(_values(data))

    def empty(n=0, dtype=None):
        return FakeArray([0.0] * (n if isinstance(n, int) else 0))

    def append(arr, values, axis=None):
        return _realloc("numpy.append", [arr, values], FakeArray(_values(arr) + _values(values)))

    def insert(arr, index, values, axis=None):
        data = _values(arr)
        data[index:index] = _values(values)
        return _realloc("numpy.insert", [arr, values], FakeArray(data))

    def _stack(name):
        def stack(seq, axis=0, out=None):
            parts = _parts(seq)
            data = []
            for part in parts:
                data.extend(_values(part))
            if out is not None:
                if len(out.data) != len(data):
                    raise ValueError("Output array has wrong dimensionality")
                out.data[:] = data
                _record(f"numpy.{name}", "realloc", False, "out=", tuple(parts), out)
                return out
            return _realloc(f"numpy.{name}", parts, FakeArray(data))

        stack.__name__ = name
        return stack

    module.array = array
    module.asarray = array
    module.empty = empty
    module.zeros = empty
    module.append = append
    module.insert = insert
    for name in ("concatenate", "vstack", "hstack", "dstack", "column_stack"):
        setattr(module, name, _stack(name))
    return module


class FakeFrame:
    __slots__ = ("rows", "__weakref__")

    def __init__(self, data=None, **kwargs):
        if isinstance(data, FakeFrame):
            self.rows = list(data.rows)
        elif data is None:
            self.rows = []
        else:
            self.rows = list(data) if isinstance(data, (list, tuple)) else [data]

    def __len__(self):
        return len(self.rows)

    def __repr__(self):
        return f"DataFrame({self.rows!r})"

    def __eq__(self, other):
        return isinstance(other, FakeFrame) and self.rows == other.rows

    __hash__ = None

    @property
    def empty(self):
        return not self.rows


def _pandas_module():
    module = types.ModuleType("pandas")
    module.DataFrame = FakeFrame

    def concat(objs, axis=0, ignore_index=False, **kwargs):
        parts = list(objs)
        rows = []
        for part in parts:
            rows.extend(part.rows if isinstance(part, FakeFrame) else [part])
        return _realloc("pandas.concat", parts, FakeFrame(rows))

    module.concat = concat
    return module


# ---------------------------------------------------------------------------------------------
# installation
# ---------------------------------------------------------------------------------------------
def install(recorder):
    global RECORDER
    RECORDER = recorder
    _time.sleep = fake_sleep
    for name in ("run", "call", "check_call", "check_output"):
        setattr(_subprocess, name, _subprocess_fake(name))
    _socket.getaddrinfo = fake_getaddrinfo
    _socket.gethostbyname = fake_gethostbyname
    _socket.getfqdn = fake_getfqdn
    _urllib_request.urlopen = fake_urlopen
    _sqlite3.connect = fake_sqlite_connect
    _queue.Queue = FakeQueue
    _threading.Lock = FakeLock
    _asyncio.Lock = FakeAsyncLock
    requests, exceptions = _requests_module()
    sys.modules["requests"] = requests
    sys.modules["requests.exceptions"] = exceptions
    psycopg2 = _psycopg2_module()
    sys.modules["psycopg2"] = psycopg2
    sys.modules["psycopg2.extensions"] = psycopg2.extensions
    sys.modules["numpy"] = _numpy_module()
    sys.modules["pandas"] = _pandas_module()
