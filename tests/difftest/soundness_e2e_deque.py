#!/usr/bin/env python3
"""End-to-end cases for the queue rules `OPT120` / `OPT121` (Milestone M8).

`crates/heki/tests/e2e_soundness.rs` pins every function's diagnostics and checks on CPython that
`heki check --fix --include-size-dependent` keeps this script's output unchanged. The module
imports `deque` from `collections`, so the rewrites spell it `deque(...)`.
"""
from __future__ import annotations

from collections import deque


# ==============================================================================
# Rewritten forms and allocation sites
# ==============================================================================
def drain_del(items):
    q = list(items)
    out = []
    while q:
        out.append(q[0])
        del q[0]
    return out


def drain_slice(items):
    q = list(items)
    out = []
    while q:
        out.append(q[0])
        q = q[1:]
    return out


def drain_slice_after_change(items):
    """`q.pop()` before the slice may empty the list."""
    q = list(items)
    out = []
    while q:
        q.pop()
        out.append(len(q))
        q = q[1:]
    return out


def drain_slice_for(items, n):
    """In a `for` loop nothing proves `q` non-empty at the slice."""
    q = list(items)
    for _ in range(n):
        q = q[1:]
    return len(q)


def fill_front(n):
    q = list()
    for i in range(n):
        q.insert(0, i)
    return list(q)


def fill_front_parenthesized(n):
    q = []
    for i in range(n):
        (q).insert(0, i)
    return list(q)


def pop_parenthesized(n):
    q = list(range(n))
    out = []
    while q:
        out.append((q).pop(0))
    return out


def drain_then_alias(items):
    """`last = q` after the loop is an alias no block entry state holds."""
    q = list(items)
    out = []
    while q:
        out.append(q.pop(0))
    last = q
    return out, len(last)


def drain_del_parenthesized(items):
    q = list(items)
    out = []
    while q:
        out.append(q[0])
        del (q)[0]
    return out


def drain_slice_parenthesized(items):
    q = list(items)
    out = []
    while q:
        out.append(q[0])
        q = (q)[1:]
    return out


def copied_queue(items):
    """`nums[:]` is a fresh list, but not one `deque(...)` can replace."""
    nums = [3, 1, 2, *items]
    q = nums[:]
    out = list(items)
    while q:
        out.append(q.pop(0))
    return out


def parameter_queue(q):
    while q:
        q.pop(0)
    return q


# ==============================================================================
# Uses that have a deque equivalent
# ==============================================================================
def deque_uses(n):
    q = [0]
    seen = []
    while q:
        if len(seen) >= 4 * n:
            break
        cur = q.pop(0)
        seen.append(cur)
        if not q:
            q.append(cur + 1)
        if cur in q:
            continue
        q += [cur + 2]
        q.extend([cur + 3])
        q[-1] = q[-1] + 1
        q[0] = q[0]
        if len(q) > 3:
            del q[-1]
        seen.append((bool(q), list(q), tuple(q), sorted(set(q))))
    return seen


def unknown_list_methods(n):
    """`list.count` / `list.index` are not in the Layer 1 KB: unknown code sees `q`."""
    q = [1, 2, 3, *range(n)]
    out = []
    while q:
        out.append((q.count(n), q.index(q[0])))
        q.pop(0)
    return out


def deque_more_uses(n):
    q = [5, 4, 3]
    out = []
    for i in range(n):
        q.insert(0, i)
        q.remove(i)
        q.reverse()
        out.append(q.pop())
        for x in iter(q):
            out.append(x)
        for k, x in enumerate(reversed(q)):
            out.append(k + x)
        q.append(i)
    q.clear()
    return out


def alias_after_site(n):
    q = [1, 2, 3]
    out = []
    while q:
        out.append(q.pop(0))
        view = q
        out.append(len(view))
        if len(out) < n:
            q.append(n)
    return out


# ==============================================================================
# Uses without one
# ==============================================================================
def middle_index(n):
    q = [1, 2, 3, 4, *range(n)]
    out = []
    while len(q) > 1:
        out.append(q[1])
        q.pop(0)
    return out


def sorted_in_loop(n):
    q = [3, 1, 2, *range(n)]
    while q:
        q.sort(key=abs)
        q.pop(0)
    return n


def passed_to_zip(n):
    q = [1, 2, 3, *range(n)]
    out = []
    while q:
        out.append(list(zip(q, q)))
        q.pop(0)
    return out


def self_extend(n):
    q = [1, 2]
    while len(q) < n:
        q.extend(q)
        q.pop(0)
    return len(q)


def compared(n):
    q = [1, 2, *range(n)]
    while q != []:
        q.pop(0)
    return n


def pop_middle(n):
    q = list(range(n))
    while q:
        q.pop(0)
        if len(q) > 2:
            q.pop(1)
    return len(q)


def front_insert_twice(n):
    """Two `insert(0, ...)` sites: each is rewritten on its own pass."""
    q = []
    for i in range(n):
        q.insert(0, i)
        q.insert(0, -i)
    return list(q)


# ==============================================================================
# Not candidates
# ==============================================================================
def dict_pop(n):
    pending = {0: "a", 1: "b"}
    out = []
    for i in range(n):
        if 0 in pending:
            out.append(pending.pop(0))
    return out


def dict_del(n):
    counts = {0: 1}
    for i in range(n):
        if 0 in counts:
            del counts[0]
    return counts


def bounded(n):
    q = [1, 2, 3]
    for i in range(3):
        q.pop(0)
    return q


# ==============================================================================
# Profitability: sizes the code fixes, lists rebuilt per iteration, truth tests
# ==============================================================================
def drained_small(n):
    """A display the loop only drains never holds more than its 3 items."""
    q = [1, 2, 3]
    out = []
    while q:
        out.append(q.pop(0))
    return out[:n]


def rebuilt_per_iteration(n):
    """One front insert into a list built in the same iteration."""
    out = []
    for i in range(n):
        q = [i, i + 1]
        q.insert(0, -i)
        out.append(q)
    return out


def drain_and_test(items):
    """`q and not q[0]` is a list or a `bool`: its truth test runs no user code."""
    q = list(items)
    while q and not q[0]:
        del q[0]
    return len(q)


def annotated_queue(items):
    """The variable's `list[int]` annotation becomes `deque[int]`."""
    q: list[int] = list(items)
    out = []
    while q:
        out.append(q.pop(0))
    return out


def run(fn, *args):
    try:
        return repr(fn(*args))
    except Exception as e:  # noqa: BLE001 - the transcript records every outcome
        return f"{type(e).__name__}"


def main() -> int:
    lines = []
    for n in [0, 1, 4, 9]:
        items = list(range(n))
        lines += [
            run(drain_del, items),
            run(drain_slice, items),
            run(drain_slice_after_change, items),
            run(drain_slice_for, items, n),
            run(fill_front, n),
            run(fill_front_parenthesized, n),
            run(pop_parenthesized, n),
            run(drain_then_alias, items),
            run(drain_del_parenthesized, items),
            run(drain_slice_parenthesized, items),
            run(copied_queue, items),
            run(parameter_queue, list(items)),
            run(deque_uses, n),
            run(unknown_list_methods, n),
            run(deque_more_uses, n),
            run(alias_after_site, n),
            run(middle_index, n),
            run(sorted_in_loop, n),
            run(passed_to_zip, n),
            run(self_extend, n),
            run(compared, n),
            run(pop_middle, n),
            run(front_insert_twice, n),
            run(dict_pop, n),
            run(dict_del, n),
            run(bounded, n),
            run(drained_small, n),
            run(rebuilt_per_iteration, n),
            run(drain_and_test, items),
            run(annotated_queue, items),
        ]
    lines.append(type(deque()).__name__)
    print("\n".join(lines))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
