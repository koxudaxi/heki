#!/usr/bin/env python3
"""Counterexamples found in Milestone M8: shapes heki used to prove safe (Milestone M6/M7) whose
rewrite changes behaviour on CPython. Each `cx_*` function is the original; `old_*` is the exact
text `heki check --fix` (`--include-size-dependent` for the size-dependent rules) produced for
it. `main()` asserts that every old rewrite diverges from its original, and
`crates/heki/tests/e2e_soundness.rs` asserts that heki now reports each original as `Blocked` or
`Disproved` and rewrites none of them.
"""
from __future__ import annotations

import collections


# ------------------------------------------------------------------------------
# OPT102 (default `--fix`): the display stores the accumulator inside the new dict
# ------------------------------------------------------------------------------
def cx_dict_stores_itself(items):
    d = {}
    for k in items:
        d = {**d, "prev": d}
    return d


def old_dict_stores_itself(items):
    d = {}
    for k in items:
        d["prev"] = d
    return d


def cx_dict_stores_its_iterator(items):
    d = {"a": 1}
    for k in items:
        d = {**d, k: iter(d)}
    return d


def old_dict_stores_its_iterator(items):
    d = {"a": 1}
    for k in items:
        d.update({k: iter(d)})
    return d


def depth(d):
    n = 0
    while isinstance(d, dict) and "prev" in d:
        d = d["prev"]
        n += 1
        if n > 10:
            return "cycle"
    return n


def drain_iterators(d):
    return {k: (v if isinstance(v, int) else list(v)) for k, v in d.items()}


# ------------------------------------------------------------------------------
# OPT110 / OPT111 / OPT130: the `for` iterable runs after a hoisted statement
# ------------------------------------------------------------------------------
def cx_rebound_in_iterable(n):
    allowed = [100]
    hits = []
    for p in (allowed := list(range(20))):
        if p in allowed and p < n:
            hits.append(p)
    return hits


def old_rebound_in_iterable(n):
    allowed = [100]
    hits = []
    allowed_set = set(allowed)
    for p in (allowed := list(range(20))):
        if p in allowed_set and p < n:
            hits.append(p)
    return hits


def cx_changed_in_iterable(n):
    """The original check only looked at the preheader block; `and` puts `pop` before it."""
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
    hits = []
    for p in (allowed.pop() and range(n)):
        if p in allowed:
            hits.append(p)
    return hits


def old_changed_in_iterable(n):
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
    hits = []
    allowed_set = set(allowed)
    for p in (allowed.pop() and range(n)):
        if p in allowed_set:
            hits.append(p)
    return hits


def cx_rebuilt_from_rebound(n):
    keys = [100]
    hits = 0
    for p in (keys := [1, 2, 3, 4, 5, 6, 7, 8, 9]):
        if p in set(keys) and p < n:
            hits += 1
    return hits


def old_rebuilt_from_rebound(n):
    keys = [100]
    hits = 0
    keys_set = set(keys)
    for p in (keys := [1, 2, 3, 4, 5, 6, 7, 8, 9]):
        if p in keys_set and p < n:
            hits += 1
    return hits


def cx_dedup_rebound_in_iterable(items):
    seen = [5]
    for v in (seen := []) or items:
        if v not in seen:
            seen.append(v)
    return seen


def old_dedup_rebound_in_iterable(items):
    seen = [5]
    seen_set = set(seen)
    for v in (seen := []) or items:
        if v not in seen_set:
            seen.append(v); seen_set.add(v)
    return seen


# ------------------------------------------------------------------------------
# OPT130: what the hoisted constructor runs even when the loop runs zero times
# ------------------------------------------------------------------------------
def cx_nested_builtin_raises(items, tag):
    limit = f"{tag}"
    hits = 0
    for p in items:
        if p in set(range(int(limit))):
            hits += 1
    return hits


def old_nested_builtin_raises(items, tag):
    limit = f"{tag}"
    hits = 0
    hoisted = set(range(int(limit)))
    for p in items:
        if p in hoisted:
            hits += 1
    return hits


CALLS = []


class Loud:
    def __init__(self, i):
        self.i = i

    def __repr__(self):
        CALLS.append(self.i)
        return f"L{self.i}"


def cx_nested_builtin_runs_user_code(items):
    objs = [Loud(1), Loud(2)]
    hits = 0
    for p in items:
        if p in tuple(list(map(repr, objs))):
            hits += 1
    return hits


def old_nested_builtin_runs_user_code(items):
    objs = [Loud(1), Loud(2)]
    hits = 0
    hoisted = tuple(list(map(repr, objs)))
    for p in items:
        if p in hoisted:
            hits += 1
    return hits


def cx_list_of_number(items):
    five = 5
    hits = 0
    for p in items:
        if p in list(five):
            hits += 1
    return hits


def old_list_of_number(items):
    five = 5
    hits = 0
    five_list = list(five)
    for p in items:
        if p in five_list:
            hits += 1
    return hits


class Key:
    def __init__(self, k):
        self.k = k

    def __hash__(self):
        return 0

    def __eq__(self, other):
        CALLS.append("eq")
        return isinstance(other, Key) and self.k == other.k


def cx_set_of_dict_keys(items):
    d = {Key(1): 1, Key(2): 2, **{Key(p): p for p in items}}
    hits = 0
    for p in items:
        if p in set(d):
            hits += 1
    return hits


def old_set_of_dict_keys(items):
    d = {Key(1): 1, Key(2): 2, **{Key(p): p for p in items}}
    hits = 0
    d_set = set(d)
    for p in items:
        if p in d_set:
            hits += 1
    return hits


# ------------------------------------------------------------------------------
# Edits that name something the function rebinds
# ------------------------------------------------------------------------------
def cx_local_set(n, set=lambda xs: ()):
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
    hits = []
    for p in range(n):
        if p in allowed:
            hits.append(p)
    return hits


def old_local_set(n, set=lambda xs: ()):
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
    hits = []
    allowed_set = set(allowed)
    for p in range(n):
        if p in allowed_set:
            hits.append(p)
    return hits


def cx_local_set_dedup(n, set=lambda xs: {0, 1, 2, 3, 4, 5, 6}):
    seen = []
    for v in range(n):
        w = v % 7
        if w not in seen:
            seen.append(w)
    return seen


def old_local_set_dedup(n, set=lambda xs: {0, 1, 2, 3, 4, 5, 6}):
    seen = []
    seen_set = set(seen)
    for v in range(n):
        w = v % 7
        if w not in seen_set:
            seen.append(w); seen_set.add(w)
    return seen


def cx_local_collections(n, collections=None):
    q = [1, 2, 3, *range(n)]
    out = []
    while q:
        out.append(q.pop(0))
    return out[:n]


def old_local_collections(n, collections=None):
    q = collections.deque([1, 2, 3, *range(n)])
    out = []
    while q:
        out.append(q.popleft())
    return out[:n]


def cx_class_in_function_collections(n, collections=None):
    # The method sees the enclosing function's `collections` (not the module's).
    class LocalCx:
        def drain(self):
            q = [1, 2, 3, *range(n)]
            out = []
            while q:
                out.append(q.pop(0))
            return out[:n]

    return LocalCx().drain()


def old_class_in_function_collections(n, collections=None):
    class LocalOld:
        def drain(self):
            q = collections.deque([1, 2, 3, *range(n)])
            out = []
            while q:
                out.append(q.popleft())
            return out[:n]

    return LocalOld().drain()


# ------------------------------------------------------------------------------
# OPT120: list operations a deque does not have (the same way)
# ------------------------------------------------------------------------------
def cx_two_pops(n):
    q = [1, 2, 3, 4, *range(n)]
    out = []
    while q:
        out.append(q.pop(0))
        if q:
            out.append(q.pop(0))
    return out[:n]


def old_two_pops(n):
    """The second site's fix overlapped the first one's and was skipped."""
    q = collections.deque([1, 2, 3, 4, *range(n)])
    out = []
    while q:
        out.append(q.popleft())
        if q:
            out.append(q.pop(0))
    return out[:n]


def cx_iterated_while_popped(n):
    q = [1, 2, 3, 4, *range(n)]
    out = []
    for x in q:
        out.append(x)
        if x < 3:
            q.pop(0)
    return out[:n]


def old_iterated_while_popped(n):
    q = collections.deque([1, 2, 3, 4, *range(n)])
    out = []
    for x in q:
        out.append(x)
        if x < 3:
            q.popleft()
    return out[:n]


def cx_slice_of_empty(n):
    q = [*range(n // 2)]
    for _ in range(n):
        q = q[1:]
    return len(q)


def old_slice_of_empty(n):
    q = collections.deque([*range(n // 2)])
    for _ in range(n):
        q.popleft()
    return len(q)


def run(fn, *args):
    del CALLS[:]
    try:
        out = repr(fn(*args))
    except Exception as e:  # noqa: BLE001 - the transcript records every outcome
        out = type(e).__name__
    return f"{out} calls={CALLS}"


def main() -> int:
    pairs = [
        (cx_dict_stores_itself, old_dict_stores_itself, lambda f: depth(f([1, 2, 3]))),
        (
            cx_dict_stores_its_iterator,
            old_dict_stores_its_iterator,
            lambda f: drain_iterators(f(["b", "c"])),
        ),
        (cx_rebound_in_iterable, old_rebound_in_iterable, lambda f: f(5)),
        (cx_changed_in_iterable, old_changed_in_iterable, lambda f: f(11)),
        (cx_rebuilt_from_rebound, old_rebuilt_from_rebound, lambda f: f(5)),
        (cx_dedup_rebound_in_iterable, old_dedup_rebound_in_iterable, lambda f: f([5, 6])),
        (cx_nested_builtin_raises, old_nested_builtin_raises, lambda f: f([], "x")),
        (cx_nested_builtin_runs_user_code, old_nested_builtin_runs_user_code, lambda f: f([])),
        (cx_list_of_number, old_list_of_number, lambda f: f([])),
        (cx_set_of_dict_keys, old_set_of_dict_keys, lambda f: f([])),
        (cx_local_set, old_local_set, lambda f: f(5)),
        (cx_local_set_dedup, old_local_set_dedup, lambda f: f(20)),
        (cx_local_collections, old_local_collections, lambda f: f(3)),
        (
            cx_class_in_function_collections,
            old_class_in_function_collections,
            lambda f: f(3),
        ),
        (cx_two_pops, old_two_pops, lambda f: f(4)),
        (cx_iterated_while_popped, old_iterated_while_popped, lambda f: f(4)),
        (cx_slice_of_empty, old_slice_of_empty, lambda f: f(5)),
    ]
    lines = []
    for cx, old, call in pairs:
        original, rewritten = run(call, cx), run(call, old)
        assert original != rewritten, cx.__name__
        lines.append(f"{cx.__name__}: {original}")
    lines.append(f"old rewrites diverge: {len(pairs)}/{len(pairs)}")
    print("\n".join(lines))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
