#!/usr/bin/env python3
"""End-to-end cases for the `heki_soundness` abstract interpreter (Milestone M8).

Every function holds an `OPT` candidate next to one IR construct the interpreter models (unary
operators, subscripts, attributes, closures, `with`, `await`, `yield from`, ...); the verdict
depends on how that construct is modelled. `crates/heki/tests/e2e_soundness.rs` pins each
function's diagnostics (rule, fix or blocker) and checks on CPython that `heki check --fix
--include-size-dependent` keeps this script's output unchanged.
"""
from __future__ import annotations

import asyncio

SCALE = 3


# ==============================================================================
# Constants, unary and binary operators
# ==============================================================================
def scaled(n):
    """`Const` float; `int * float` is a fresh `float`."""
    factor = 1.5
    x = []
    for i in range(n):
        x = x + [i * factor]
    return x


def unary_mix(n):
    """`-i`, `~i`, `not i`, `-flag`, `+f` on exact operands stay exact builtins."""
    flag = True
    f = 0.5
    x = []
    for i in range(n):
        x = x + [-i, ~i, not i, -flag, +f, -f]
    return x


def unary_unknown(items):
    """`not p` / `-p` on an unknown value may run `__bool__` / `__neg__`."""
    x = []
    for p in items:
        if not p:
            continue
        x = x + [-p]
    return x


def binops(n):
    """Exact operands: concatenations, set difference, `int | int`, `dict | dict`."""
    t = (1,) + (2,)
    s = "a" + "b"
    b = b"x" + b"y"
    diff = {1, 2, 3} - {2}
    bits = 6 | 1
    merged = {"a": 1} | {"b": 2}
    x = []
    for i in range(n):
        x = x + [t, s, b, diff, bits, merged]
    return len(x)


def binops_unknown_results(n):
    """`list + tuple`, `list * int`, `set | list`, `int / int`, `int ** int`, `int << int` are
    not fresh builtins the analysis knows."""
    out = []
    for i in range(n):
        try:
            a = [i] + (i,)
        except TypeError:
            a = None
        try:
            c = {i} | [i]
        except TypeError:
            c = None
        out = out + [a, [i] * 2, c, i / 2, i ** 2, i << 1]
    return out


def compares(items, limit):
    """`is` / `is not` always give a `bool`; `==` on two exact containers is not plain."""
    x = []
    marker = []
    for v in items:
        if v is None or v is marker:
            continue
        if [v] == [limit]:
            continue
        x = x + [v]
    return x


# ==============================================================================
# Subscripts
# ==============================================================================
def vowels_seen(n):
    """`list("...")` holds `str` elements; indexing an exact `str` gives a `str` probe."""
    vowels = list("aeiouAEIOU")
    text = "heki checks effects and aliases"
    out = []
    for i in range(n):
        ch = text[i % len(text)]
        if ch in vowels:
            out.append(ch)
    return out


def byte_codes(n):
    """`list(b"...")` holds `int` elements; indexing exact `bytes` gives an `int` probe."""
    codes = list(b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09")
    data = b"\x03\x0a\x05\x10"
    out = []
    for i in range(n):
        c = data[i % 4]
        if c in codes:
            out.append(c)
    return out


def mixed_tuple_probe(n):
    """`pair[k]` of a tuple with `int` and `str` elements is an `int` or a `str`."""
    pair = (1, "a")
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9, "a"]
    out = []
    for i in range(n):
        e = pair[i % 2]
        if e in allowed:
            out.append(e)
    return out


def slices_and_items(n):
    """Slices of exact lists / tuples / `str` / `bytes`; items of a list, tuple, and dict."""
    nums = [1, 2, 3, 4]
    pair = (5, 6)
    text = "abcdef"
    data = b"\x01\x02\x03"
    table = {"k": 7}
    x = []
    for i in range(n):
        x = x + [nums[1:], pair[0:1], text[:2], data[1:], nums[0], pair[1], table["k"]]
    return x


def unknown_subscripts(rows):
    """Subscripts of unknown objects may run `__getitem__` / `__setitem__` / `__delitem__`."""
    x = []
    for row in rows:
        first = row[0]
        row[0] = first
        x = x + [first]
    return x


def subscript_writes(n, sink):
    """`lst[0] = v` changes an exact list; `d[k] = v` stores into a dict; `del` on both."""
    allowed = [0, 1, 2, 3, 4, 5, 6, 7, 8]
    allowed[0] = 9
    table = {"a": 1, "b": 2}
    table["c"] = 3
    del table["a"]
    del allowed[1]
    hits = []
    for p in range(n):
        if p in allowed:
            hits.append(p)
    sink[0] = table
    del sink[0]
    return hits


# ==============================================================================
# Attributes, closures, globals
# ==============================================================================
class Tally:
    def __init__(self):
        self.base = 0
        self.tmp = 1

    def collect(self, items):
        """Attribute stores, loads, and deletes on `self` do not touch the local list."""
        out = []
        for i in items:
            out = out + [i]
            self.last = i
        del self.tmp
        self.total = len(out)
        return out + [self.base]


def bound_method_alias(items):
    """`acc.copy` keeps the old list alive inside a bound method."""
    acc = []
    snapshot = None
    for i in items:
        snapshot = acc.copy
        acc = acc + [i]
    return acc, snapshot() if snapshot else None


def free_variables(items):
    """A nested function reads (`items`) and rebinds (`nonlocal`) enclosing variables."""
    last = None

    def collect():
        nonlocal last
        out = []
        for i in items:
            out = out + [i]
            last = out
        return out

    return collect(), last


CACHE = None


def stored_in_global(items):
    """`global CACHE; CACHE = x` makes the list reachable from outside the frame."""
    global CACHE
    x = []
    CACHE = x
    for i in items:
        x = x + [i]
    return x


def scaled_by_global(n):
    """A module global and an import inside the function are values from outside."""
    import math

    x = []
    for i in range(n):
        x = x + [math.floor(i * SCALE)]
    return x


# ==============================================================================
# Deletion, unbound names, copies
# ==============================================================================
def deleted_alias(items):
    """`del tmp` drops an alias of the list before the loop."""
    x = []
    tmp = x
    del tmp
    for i in items:
        x = x + [i]
    return x


def maybe_unbound(items, c):
    """`seed` may be unbound: reading it is a `possibly unbound name`."""
    if c:
        seed = 1
    x = []
    for i in items:
        x = x + [i, seed]
    return x


def long_copy_chain(n):
    """A probe copied through ten names keeps its exact type."""
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    hits = []
    for i in range(n):
        a1 = i
        a2 = a1
        a3 = a2
        a4 = a3
        a5 = a4
        a6 = a5
        a7 = a6
        a8 = a7
        a9 = a8
        a10 = a9
        if a10 in allowed:
            hits.append(a10)
    return hits


def long_copy_chain_mixed(n):
    """Past eight copies the element kinds of a mixed iterator are no longer followed."""
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9, "a"]
    hits = []
    mixed = list(range(n))
    mixed.append("a")
    for e in mixed:
        b1 = e
        b2 = b1
        b3 = b2
        b4 = b3
        b5 = b4
        b6 = b5
        b7 = b6
        b8 = b7
        b9 = b8
        b10 = b9
        if b10 in allowed:
            hits.append(b10)
    return hits * n


# ==============================================================================
# Iterators, generators, escapes through derived references
# ==============================================================================
def inner_loop_over_list(n):
    """The inner `for` re-creates its iterator over `grid` on every outer iteration."""
    grid = [1, 2, 3]
    x = []
    for i in range(n):
        for v in grid:
            x = x + [v * i]
    return x


def escaped_iterator(items, sink):
    """An iterator of the list escapes: the list may be read through it."""
    x = []
    it = iter(x)
    sink(it)
    for i in items:
        x = x + [i]
    return x


def generator_after_loop(items):
    """A generator expression built after the loop captures `k` and reads `x`."""
    x = []
    k = 2
    for i in items:
        x = x + [i]
    return sum(v * k for v in x)


def generator_over_param(items):
    """A generator expression over an unknown iterable."""
    x = []
    for i in items:
        x = x + [i]
    return x, list(v for v in items)


def generator_before_loop(items):
    """S24 through a generator: it iterates the old list lazily and is used after the loop."""
    x = [0]
    g = (v for v in x)
    for i in items:
        x = x + [i]
    return list(g)


def delegating(items, inner):
    """`yield from` hands control to code heki cannot see."""
    x = []
    for i in items:
        x = x + [i]
    yield from inner
    yield x


async def awaiting(items, fetch):
    """`await` runs other code; the local list stays private."""
    x = []
    for i in items:
        v = await fetch(i)
        x = x + [v]
    return x


class Ctx:
    def __init__(self):
        self.log = []

    def __enter__(self):
        self.log.append("enter")
        return self

    def __exit__(self, *exc):
        self.log.append("exit")
        return False


def within_context(items):
    """`with` enters and exits a manager heki cannot see."""
    ctx = Ctx()
    x = []
    with ctx as c:
        for i in items:
            x = x + [i]
    return x, c.log


class AsyncCtx:
    async def __aenter__(self):
        return self

    async def __aexit__(self, *exc):
        return False


async def within_async_context(items):
    x = []
    async with AsyncCtx():
        for i in items:
            x = x + [i]
    return x


# ==============================================================================
# Calls and methods
# ==============================================================================
def known_calls(n):
    """`float()` / `bool()` / `int()` give exact results; keyword arguments escape."""
    x = []
    for i in range(n):
        x = x + [float(i), bool(i), int("10", base=2), dict(k=i)]
    return x


def methods(n):
    """`.copy()` returns a fresh exact container; `.sort(key=...)` passes the key on;
    `.index()` is unknown to the Layer 1 KB."""
    base = [3, 1, 2]
    x = []
    for i in range(n):
        c = base.copy()
        c.sort(key=abs)
        x = x + [c, c.index(1), {1}.copy(), {"a": i}.copy()]
    return x


def unknown_method_on_accumulator(items):
    """`.index()` on the accumulator is unknown code: the list escapes."""
    x = []
    for i in items:
        x = x + [i]
        _ = x.index(i)
    return x


def in_place_updates(n, extra):
    """`+=` / `*=` / `|=` / `&=` / `^=` / `-=` / `|=` change the left operand in place."""
    lst = [1]
    lst += [2]
    lst += extra
    lst *= 2
    s = {1, 2, 3}
    s |= {4}
    s &= {1, 2, 4}
    s ^= {5}
    s -= {1}
    d = {"a": 1}
    d |= {"b": 2}
    allowed = [10, 11, 12, 13, 14, 15, 16, 17, 18]
    hits = []
    for p in range(n):
        if p in allowed:
            hits.append(p)
    return hits, lst, sorted(s), d


# ==============================================================================
# Exception handlers
# ==============================================================================
def handler_names(items):
    """`except ... as e` binds a caught exception; the list is not involved."""
    x = []
    errors = []
    for i in items:
        try:
            v = 10 // i
        except ZeroDivisionError as e:
            errors.append(type(e).__name__)
            continue
        x = x + [v]
    return x, errors


def two_candidates_one_try(items):
    """Two accumulations protected by the same handler."""
    x = []
    y = []
    try:
        for i in items:
            x = x + [i]
            y = y + [i * 2]
    except ValueError:
        return None
    return x, y


def branchy_try(items, flag):
    """A handler entered from several protected blocks."""
    x = []
    try:
        for i in items:
            if flag:
                x = x + [i]
            else:
                x = x + [-i]
    except TypeError:
        pass
    return x


def returned_in_try(items):
    """`return x` inside `try`; the `finally` loop rebinds `x` after the value is taken."""
    x = []
    try:
        size = len(x)
        return x
    finally:
        for i in items:
            x = x + [i + size]


def stale_handler_alias(items, check):
    """The handler may still see `z` bound to the old list."""
    x = []
    z = None
    try:
        for i in items:
            z = x
            x = x + [i]
            check(i)
            z = None
    except ValueError:
        return z
    return x


def many_sites(n, c):
    """Five allocation sites flow into one variable (more than the four tracked)."""
    if c == 1:
        x = []
    elif c == 2:
        x = [0]
    elif c == 3:
        x = [1]
    elif c == 4:
        x = [2]
    else:
        x = [3]
    for i in range(n):
        x = x + [i]
    return x


def run(fn, *args):
    try:
        out = fn(*args)
        if hasattr(out, "__next__"):
            out = list(out)
        if asyncio.iscoroutine(out):
            out = asyncio.run(out)
        return repr(out)
    except Exception as e:  # noqa: BLE001 - the transcript records every outcome
        return f"{type(e).__name__}"


def consumed_before_the_loop(items):
    """`pop`'s default, `map`'s iterable, and `remove`'s receiver are not live in the loop."""
    x = [0]
    {}.pop("missing", x)
    names = list(map(str, x))
    x.remove(0)
    for i in items:
        x = x + [i]
    return x, names


def main() -> int:
    async def fetch(i):
        return i * 10

    lines = []
    for n in [0, 1, 3, 12]:
        items = list(range(n))
        sink = []
        lines += [
            run(scaled, n),
            run(unary_mix, n),
            run(unary_unknown, [0, *items]),
            run(binops, n),
            run(binops_unknown_results, n),
            run(compares, [None, *items], 2),
            run(vowels_seen, n),
            run(byte_codes, n),
            run(mixed_tuple_probe, n),
            run(slices_and_items, n),
            run(unknown_subscripts, [[i] for i in items]),
            run(subscript_writes, n, [None]),
            run(Tally().collect, items),
            run(bound_method_alias, items),
            run(free_variables, items),
            run(stored_in_global, items),
            run(scaled_by_global, n),
            run(deleted_alias, items),
            run(maybe_unbound, items, n % 2),
            run(long_copy_chain, n),
            run(long_copy_chain_mixed, n),
            run(inner_loop_over_list, n),
            run(escaped_iterator, items, sink.append),
            run(generator_after_loop, items),
            run(generator_over_param, items),
            run(generator_before_loop, items),
            run(delegating, items, iter("ab")),
            run(awaiting, items, fetch),
            run(within_context, items),
            run(within_async_context, items),
            run(known_calls, n),
            run(methods, n),
            run(unknown_method_on_accumulator, items),
            run(in_place_updates, n * 2, [n]),
            run(handler_names, items),
            run(two_candidates_one_try, items),
            run(branchy_try, items, n % 2),
            run(returned_in_try, items),
            run(stale_handler_alias, items, int),
            run(many_sites, n, n % 5),
            run(consumed_before_the_loop, items),
        ]
    lines.append(f"CACHE = {CACHE}")
    print("\n".join(lines))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
