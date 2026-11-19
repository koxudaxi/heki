"""heki frontend syntax tour (Python 3.11+ syntax).

Every statement and expression form that `heki_semantic` scopes and `heki_ir` lowers. Driven
end to end through the real `heki` binary by `crates/heki/tests/e2e_frontend.rs`, and checked
against CPython `symtable` by `tests/symtable_verify/verify_symtable.py`.
"""

from __future__ import generator_stop

import builtins
import ctypes
import gc
import gc as mygc
import inspect
import inspect as insp
import os.path
import sys
import time
import typing
from collections import abc as cabc
from os.path import *
from typing import TYPE_CHECKING

from . import sibling
from ..pkg.mod import thing as renamed_thing

if typing.TYPE_CHECKING:
    from decimal import Decimal
elif sys.platform == "win32":
    import nt
else:
    import posix

if TYPE_CHECKING:
    import fractions

COUNTER = 0
__not_mangled = 2
MODULE_ROWS = [row * 2 for row in range(3)]
MODULE_GEN = ((seen := v) + (seen := v) for v in range(3))
MODULE_WALRUS = [last_row := v for v in range(3)]


def parameters(a, /, b=COUNTER, *args: int, c, d=len("x"), **kwargs: str) -> None:
    return None


def trace(fn):
    return fn


@trace
@trace
def decorated():
    return decorated


def elif_without_else(value):
    if value == 1:
        return "one"
    elif value == 2:
        return "two"
    return "many"


def first_or_none(items):
    for item in items:
        return item
    return None


def control_flow(items, flag, other):
    total = 0
    for item in items:
        if item is None:
            continue
        elif item == 1:
            total += 1
        elif item <= 2:
            total -= 1
        else:
            break
    else:
        total = -1
    while flag:
        flag = flag - 1
        if flag > 10:
            break
    else:
        total = total * 2
    while other:
        return total
    return total


def dead_code(xs):
    for x in xs:
        continue
        print("after continue")
    while xs:
        break
        xs = None
    return 1
    print("after return")


def raises(err, cause):
    if err:
        raise ValueError("bad") from cause
    assert cause, "cause is required"
    assert err
    raise


def try_forms(fn, log):
    try:
        value = fn()
    except (KeyError, IndexError) as exc:
        log(exc)
        value = None
    except ValueError:
        value = 0
    except:
        raise
    else:
        log(value)
    finally:
        log("done")
    try:
        fn()
    finally:
        log("cleanup")
    try:
        fn()
    except Exception:
        pass
    else:
        return value
    try:
        fn()
    except* OSError as group:
        log(group)
    return None


def with_forms(path, lock):
    with open(path) as fh, lock:
        data = fh.read()
    with open(path) as (first, second):
        pass
    with open(path) as [head, *tail]:
        pass
    return data, first, second, head, tail


async def async_forms(session, urls, fut):
    async with session.get(urls[0]) as resp, session.lock():
        body = await resp.text()
    async for chunk in session.stream():
        await fut
        yield chunk
    fetched = [await session.get(u) async for u in urls]
    pending = (u async for u in session.stream() if u)
    yield body, fetched, pending


def generators(source):
    yield
    yield 1
    received = yield from source
    return received


def comprehensions(rows, cols, key_of):
    pairs = [(r, c) for r in rows if r for c in cols if c != r]
    unique = {r for r in rows if r}
    index = {key_of(r): r for r in rows}
    kept = {key_of(r): r for r in rows if r}
    nested = [[c for c in r] for r in rows]
    lazy = (r * c for r in rows if r for c in cols if c)
    twice = ((hit := r) + (hit := r) for r in rows)
    walrus = [last := r for r in rows]
    return pairs, unique, index, kept, nested, lazy, twice, walrus, last


def comprehension_target_shadows_global(rows):
    print(row)
    return [row for row in rows]


def comprehension_target_also_local(rows):
    row = None
    return [row for row in rows], row


def match_forms(command, point_cls):
    match command:
        case None:
            return "none"
        case True | False as flag:
            return flag
        case 0 | -1:
            return "small"
        case cabc.Sized.__name__:
            return "dotted value"
        case "go" if command:
            return "go"
        case [first, *rest]:
            return first, rest
        case [_, _, *_]:
            return "long"
        case {"op": op, "args": [*args], **extra}:
            return op, args, extra
        case {"op": _}:
            return "op"
        case point_cls(x=0, y=py) as origin_like:
            return origin_like, py
        case point_cls(px, py2):
            return px, py2
        case cabc.Mapping():
            return "mapping"
        case _:
            return "other"
    match command:
        case [*_]:
            pass


def delete_forms(obj, mapping, seq, flag):
    temp = 1
    other = 2
    del temp
    del obj.attr, mapping["k"]
    del (other, seq[0]), [seq[1]]
    del seq[1:3]
    if flag:
        del flag
    return obj


def assignment_forms(pairs, obj, data):
    a, b = pairs
    [c, d] = pairs
    e, *f = pairs
    [*g, h] = pairs
    (i, j), k = pairs
    obj.attr = data[0] = x = 1
    obj.count += 1
    data[0] += 1
    data["key"] -= 2
    x *= 3
    x /= 4
    x //= 5
    x %= 6
    x **= 7
    x <<= 1
    x >>= 1
    x &= 1
    x |= 2
    x ^= 3
    x @= obj
    return a, b, c, d, e, f, g, h, i, j, k, x


def expression_forms(a, b, seq, obj, width, precision):
    complex_num = 3j
    ellipsis = ...
    neg_int = -1
    neg_float = -1.5
    huge_neg = -123456789012345678901234567890
    big = 123456789012345678901234567890
    flags = +a, ~a, -a, not a
    either = a or b
    both = a and b and seq
    chained = a < b <= seq > obj >= width != precision
    identity = a is not b, a in seq, b not in seq
    choice = a if b else seq
    arithmetic = a / b, a ** b, a & b, a ^ b, a << b, a >> b, a @ b, a // b, a % b, a | b
    sliced = seq[1:2], seq[::2], seq[a:], seq[:b], seq[1:-1:1]
    formatted = f"{a!r:>{width}.{precision}}" "tail" f"{b=}"
    raw = b"bytes" + rb"raw"
    displays = [a, *seq], (a, *seq), {a, *seq}, {"k": a, **obj}, {}
    call = obj.method(a, *seq, key=b, **obj)
    plain_call = len(seq, *seq, key=b, **obj)
    callee_expr = (lambda: a)()
    attr = obj.attr.sub
    lam = lambda x, y=a, *rest, z=b, **kw: x + y
    no_params = lambda: a
    return (
        complex_num,
        ellipsis,
        neg_int,
        neg_float,
        huge_neg,
        big,
        flags,
        either,
        both,
        chained,
        identity,
        choice,
        arithmetic,
        sliced,
        formatted,
        raw,
        displays,
        call,
        plain_call,
        callee_expr,
        attr,
        lam,
        no_params,
    )


def closures():
    counter = 0

    def bump():
        nonlocal counter
        nonlocal counter
        counter += 1
        return counter

    def reads_later():
        return defined_later

    defined_later = 1

    def middle():
        def innermost():
            return counter

        return innermost

    return bump, reads_later, middle


def global_forms():
    global COUNTER, FRESH_GLOBAL
    global COUNTER
    COUNTER = COUNTER + 1
    FRESH_GLOBAL = COUNTER
    return FRESH_GLOBAL


def enclosing_global():
    global COUNTER, ANOTHER_FRESH
    COUNTER = 0
    ANOTHER_FRESH = 1

    def inner():
        return COUNTER, ANOTHER_FRESH

    return inner


def a1_hazards(frame, coro, agen, gen, cls, ns):
    eval("1 + 1")
    builtins.exec("pass")
    locals()
    vars()
    vars(ns)
    sys._getframe(1)
    inspect.currentframe()
    insp.currentframe()
    gc.get_objects()
    mygc.get_objects()
    gc.get_referrers(ns)
    ctypes.pythonapi.Py_IncRef(ns)
    frame.f_back._getframe()
    frame.f_globals["x"] = 1
    frame.f_locals += 1
    del frame.f_locals
    return coro.cr_frame, agen.ag_frame, gen.gi_frame, cls.__subclasses__()


def loop_bounds(items, fn, worker, attempts):
    for _ in (1, 2, 3):
        pass
    for _ in {1, 2}:
        pass
    for _ in (1, 2, 3, 4, 5, 6, 7, 8, 9):
        pass
    for _ in [1, 2]:
        pass
    for _ in range(fn(), 3):
        pass
    for _ in range(0, 10, 2):
        pass
    for _ in range(2, 6):
        pass
    for _ in range(0, 100):
        pass
    for _ in range(-2, +3):
        pass
    for _ in range(+3):
        pass
    for _ in range(~3):
        pass
    for _ in range(1.5):
        pass
    for _ in range(fn()):
        pass
    for _ in fn(3):
        pass
    for attempt in attempts:
        try:
            return fn(attempt)
        except OSError:
            time.sleep(1)
    for retry_count in items:
        try:
            if fn(retry_count):
                return retry_count
        except OSError:
            continue
    for item in items:
        try:
            with worker:
                fn(item)
        except OSError:
            pass
    for item in items:
        try:
            with worker:
                return item
        except OSError:
            pass
    for host, port in items:
        try:
            return fn(host, port)
        except OSError:
            time.sleep(1)
    for item in items:
        try:
            if fn(item):
                fn()
            elif item:
                break
        except OSError:
            pass
    for item in items:
        try:
            if fn(item):
                fn()
        except OSError:
            pass
    while worker.running:
        fn()
    while not worker.stopped:
        fn()
    while not shutdown_requested:
        fn()
    while not worker.ready():
        fn()
    while True:
        try:
            fn()
        except OSError:
            time.sleep(1)
    while items:
        try:
            fn()
            break
        except OSError:
            time.sleep(1)


def sleep_detection(fn, flag):
    while fn():
        try:
            fn()
            break
        except OSError:
            if flag:
                time.sleep(1)
    while fn():
        try:
            fn()
            break
        except OSError:
            try:
                time.sleep(1)
            except OSError:
                pass
    while fn():
        try:
            fn()
            break
        except OSError:
            try:
                fn()
            except OSError:
                pass
            sleep(1)
    while fn():
        try:
            fn()
            break
        except OSError:
            "a docstring-like expression"
            (lambda: 0)()
            fn()
    while fn():
        try:
            fn()
            break
        except OSError:
            if flag:
                fn()
            elif fn():
                time.sleep(1)
    while fn():
        try:
            fn()
            break
        except OSError:
            if flag:
                fn()


async def async_retry(fn):
    while True:
        try:
            await fn()
            break
        except OSError:
            await asyncio_sleep_alias.sleep(1)


def nested_while_with_boolop(read, write):
    while chunk := read():
        while len(chunk) < 8 and (more := read()):
            chunk += more
        write(chunk)


def closure_loop(source, flag):
    counter = 0

    def inner():
        nonlocal counter
        limit = 10
        for item in source:
            counter = item + limit
            received = yield from source
            yield
            if flag:
                del received
            del item
        return counter

    return inner


class Base:
    pass


def make_base():
    return Base


def decorate(cls):
    return cls


class Meta(type):
    pass


@decorate
class ClassForms(Base, cabc.Sized, typing.Generic[typing.AnyStr], make_base(), metaclass=Meta):
    size = 3
    names = [size for _ in range(3)]
    squares = [n * n for n in range(4)]
    __private = 1

    def method(self):
        return self.__private, __class_level_name

    @property
    def prop(self):
        return self.size

    @staticmethod
    def static(value):
        return value

    class Nested:
        __inner = 0

        def method(self):
            return self.__inner

    # Class-level annotations come last: CPython 3.14+ opens the class `__annotate__` scope at
    # the first one, heki after the class body (scope order only).
    field: int = 2
    (other_field): int = 3


class _:
    __kept = 1


def nested_class_factory():
    class Local:
        def method(self):
            return Local

    return Local


# Module-level annotations come last for the same reason (see `ClassForms`).
ANNOTATED: int = 0
UNSET: "int"
(PARENTHESIZED): int = 1
holder = ClassForms
holder.attr: int = 2
