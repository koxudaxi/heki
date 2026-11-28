#!/usr/bin/env python3
"""End-to-end cases for the queue rules `OPT120` / `OPT121` (Milestone M8).

`crates/heki/tests/e2e_soundness.rs` pins every function's diagnostics and checks on CPython that
`heki check --fix --include-size-dependent` keeps this script's output unchanged. The module
imports `deque` from `collections`, so the rewrites spell it `deque(...)`.
"""
from __future__ import annotations

import io
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


def extended_by_builtins(n):
    """`list.extend` and `+=` ask their argument for its length, which runs no code for a
    builtin container, a string, or a `range`."""
    q = list(range(n))
    out = []
    while q:
        cur = q.pop(0)
        out.append(cur)
        if cur == 0:
            q.extend([-1, -2])
            q.extend((-3,))
            q.extend(range(1, 3))
            q.extend("ab")
            q.extend({"k": 1})
            q.extend({-4})
            q.extend(b"\x05")
            q.extend(list("cd"))
            q += [-5]
            q += (-6,)
    return out


def caught_errors(n):
    """A deque raises the exception types a list raises, with other messages, which no fix
    keeps for a builtin exception."""
    q = list(range(n))
    out = []
    for _ in range(n + 1):
        try:
            out.append(q.pop(0))
        except IndexError as e:
            out.append(type(e).__name__)
        try:
            q.remove(-1)
        except ValueError as e:
            out.append(type(e).__name__)
    for attempt in range(5):
        try:
            if attempt == 0:
                out.append(q[0])
            elif attempt == 1:
                out.append(q[-1])
            elif attempt == 2:
                q[0] = 1
            elif attempt == 3:
                del q[-1]
            else:
                out.append(q.pop())
        except IndexError as e:
            out.append(type(e).__name__)
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


def formatted_items(items):
    """The replacement fields format an item and the queue's length, not the queue."""
    q = list(items)
    out = []
    while q:
        x = q.pop(0)
        out.append(f"{x!r} left: {len(q):>{len(out) + 1}}")
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


def formatted_queue(items):
    """An f-string, `str()`, and code heki cannot see all show the queue's text."""
    q = list(items)
    out = []
    while q:
        out.append(q.pop(0))
        out.append(f"left: {q}")
    return out


def stringified_queue(items):
    q = list(items)
    out = []
    while q:
        out.append(q.pop(0))
        out.append("left: " + str(q))
    return out


def printed_queue(items, sink):
    q = list(items)
    out = []
    while q:
        out.append(q.pop(0))
        print("left:", q, file=sink)
    return out, sink.getvalue()


def front_insert_twice(n):
    """Two `insert(0, ...)` sites: each is rewritten on its own pass."""
    q = []
    for i in range(n):
        q.insert(0, i)
        q.insert(0, -i)
    return list(q)


# ==============================================================================
# Variables the queue reaches: a type checker gives each one type
# ==============================================================================
def alias_bound_elsewhere(items, stop):
    """`s = q` stores the queue in `s`, which `s = [...]` types as a list."""
    s = [a for a in items]
    q = []
    for x in reversed(s):
        if x == stop:
            s = q
            break
        q.insert(0, x)
    return list(s)


def queue_bound_again(items):
    """The second `q = [...]` stays a list."""
    q = list(items)
    out = []
    while q:
        out.append(q.pop(0))
    q = [len(out)]
    return out, q


def alias_in_parameter(n, last=None):
    """`last = q` stores the queue in a parameter."""
    q = []
    for i in range(n):
        q.insert(0, i)
    last = q
    return list(last)


def alias_bound_to_argument(n, first):
    """`s = first` binds `s` to the caller's object too."""
    s = first
    q = []
    for i in range(n):
        q.insert(0, i)
    if n > 1:
        s = q
    return list(s)


def alias_of_queue_only(items, stop):
    """`s` is bound to the queue alone: the deque is the one object it holds."""
    q = []
    for x in reversed(items):
        if x == stop:
            s = q
            break
        q.insert(0, x)
    else:
        s = q
    return list(s)


def alias_maybe_unbound(items, stop):
    """`s` holds the queue or nothing (`UnboundLocalError` when `stop` is not an item)."""
    q = []
    for x in items:
        if x == stop:
            s = q
        q.insert(0, x)
    return list(s)


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


def unpacked_queue(items):
    """`*q` makes the list: there is no display to turn into a deque."""
    first, *q = [0, *items]
    out = [first]
    while q:
        out.append(q.pop(0))
    return out


def matched_queue(items):
    """A star pattern makes the list: there is no display to turn into a deque."""
    match [0, *items]:
        case [first, *q]:
            out = [first]
            while q:
                out.append(q.pop(0))
            return out


def fill_front_with_comment(n):
    q = list()
    for i in range(n):
        q.insert(0, i  # keep this comment
                 )
    return list(q)


def drain_then_iterate(n, c):
    """After draining, an iterator of the queue (through a conditional) is only consumed."""
    q = list(range(n))
    out = []
    while q:
        out.append(q.pop(0))
    it = iter(q) if c else iter(out)
    return out, list(it)


def drain_then_sum_reversed(n):
    """A generator expression receives an iterator of the queue: not modelled."""
    q = list(range(n))
    out = []
    while q:
        out.append(q.pop(0))
    return out, sum(x for x in reversed(q))


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
            run(fill_front_with_comment, n),
            run(drain_then_iterate, n, n % 2),
            run(drain_then_sum_reversed, n),
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
            run(extended_by_builtins, n),
            run(caught_errors, n),
            run(alias_after_site, n),
            run(formatted_items, items),
            run(middle_index, n),
            run(sorted_in_loop, n),
            run(passed_to_zip, n),
            run(self_extend, n),
            run(compared, n),
            run(pop_middle, n),
            run(formatted_queue, items),
            run(stringified_queue, items),
            run(printed_queue, items, io.StringIO()),
            run(front_insert_twice, n),
            run(alias_bound_elsewhere, items, n // 2),
            run(queue_bound_again, items),
            run(alias_in_parameter, n),
            run(alias_bound_to_argument, n, items),
            run(alias_of_queue_only, items, n // 2),
            run(alias_maybe_unbound, items, n // 2),
            run(dict_pop, n),
            run(dict_del, n),
            run(bounded, n),
            run(drained_small, n),
            run(rebuilt_per_iteration, n),
            run(drain_and_test, items),
            run(annotated_queue, items),
            run(unpacked_queue, items),
            run(matched_queue, items),
        ]
    lines.append(type(deque()).__name__)
    print("\n".join(lines))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
