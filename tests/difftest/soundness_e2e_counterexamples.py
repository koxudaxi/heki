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
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17]
    hits = []
    for p in (allowed.pop() and range(n)):
        if p in allowed:
            hits.append(p)
    return hits


def old_changed_in_iterable(n):
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17]
    hits = []
    allowed_set = set(allowed)
    for p in (allowed.pop() and range(n)):
        if p in allowed_set:
            hits.append(p)
    return hits


def cx_rebuilt_from_rebound(n):
    keys = [100]
    hits = 0
    for p in (keys := [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17]):
        if p in set(keys) and p < n:
            hits += 1
    return hits


def old_rebuilt_from_rebound(n):
    keys = [100]
    hits = 0
    keys_set = set(keys)
    for p in (keys := [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17]):
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
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17]
    hits = []
    for p in range(n):
        if p in allowed:
            hits.append(p)
    return hits


def old_local_set(n, set=lambda xs: ()):
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17]
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


# ------------------------------------------------------------------------------
# OPT101 (default `--fix`): builtins that keep the accumulator or hand it to other code
# ------------------------------------------------------------------------------
def cx_kept_by_pop_default(items):
    acc = []
    snapshot = {}.pop("missing", acc)
    for v in items:
        acc = acc + [v]
    return snapshot


def old_kept_by_pop_default(items):
    acc = []
    snapshot = {}.pop("missing", acc)
    for v in items:
        acc.append(v)
    return snapshot


class Keeps:
    def __eq__(self, other):
        CALLS.append(other)
        return True


def cx_compared_by_remove(items):
    acc = []
    [Keeps()].remove(acc)
    for v in items:
        acc = acc + [v]
    return len(acc)


def old_compared_by_remove(items):
    acc = []
    [Keeps()].remove(acc)
    for v in items:
        acc.append(v)
    return len(acc)


def cx_function_kept_by_map(items):
    acc = []
    m = map(acc, items)
    for v in items:
        acc = acc + [v]
    return m.__reduce__()[1][0]


def old_function_kept_by_map(items):
    acc = []
    m = map(acc, items)
    for v in items:
        acc.append(v)
    return m.__reduce__()[1][0]


# ------------------------------------------------------------------------------
# OPT110: a loop over an iterator that keeps a list yields what the iterator computes
# ------------------------------------------------------------------------------
def cx_map_over_ints(n):
    keys = list(range(17))
    out = []
    for p in map(lambda _: [], range(n)):
        out.append(p in keys)
    return out


def old_map_over_ints(n):
    keys = list(range(17))
    out = []
    keys_set = set(keys)
    for p in map(lambda _: [], range(n)):
        out.append(p in keys_set)
    return out


def cx_generator_over_ints(n):
    keys = list(range(17))
    out = []
    for p in ([] for _ in range(n)):
        out.append(p in keys)
    return out


def old_generator_over_ints(n):
    keys = list(range(17))
    out = []
    keys_set = set(keys)
    for p in ([] for _ in range(n)):
        out.append(p in keys_set)
    return out


# ------------------------------------------------------------------------------
# OPT110 / OPT130: a variable that may also hold an object from outside the function
# ------------------------------------------------------------------------------
def cx_ints_or_parameter(flag, arg):
    keys = list(range(17))
    source = [0] if flag else arg
    out = []
    for p in source:
        out.append(p in keys)
    return out


def old_ints_or_parameter(flag, arg):
    keys = list(range(17))
    source = [0] if flag else arg
    out = []
    keys_set = set(keys)
    for p in source:
        out.append(p in keys_set)
    return out


def cx_range_or_parameter(flag, arg):
    keys = list(range(17))
    source = arg if flag else range(3)
    out = []
    for p in source:
        out.append(p in keys)
    return out


def old_range_or_parameter(flag, arg):
    keys = list(range(17))
    source = arg if flag else range(3)
    out = []
    keys_set = set(keys)
    for p in source:
        out.append(p in keys_set)
    return out


def cx_tuple_or_element(flag, n):
    pairs = [([],), (1,)]
    keys = (0,) if flag else pairs[0]
    hits = 0
    for i in range(n):
        if i in set(keys):
            hits += 1
    return hits


def old_tuple_or_element(flag, n):
    pairs = [([],), (1,)]
    keys = (0,) if flag else pairs[0]
    hits = 0
    keys_set = set(keys)
    for i in range(n):
        if i in keys_set:
            hits += 1
    return hits


# ------------------------------------------------------------------------------
# OPT110: elements that other code or an earlier iteration may have replaced
# ------------------------------------------------------------------------------
def replace_first(xs):
    xs[0] = []


def cx_elements_after_escape(n):
    keys = list(range(17))
    source = [0]
    replace_first(source)
    out = []
    for p in source:
        out.append(p in keys)
    return out


def old_elements_after_escape(n):
    keys = list(range(17))
    source = [0]
    replace_first(source)
    out = []
    keys_set = set(keys)
    for p in source:
        out.append(p in keys_set)
    return out


def cx_list_from_earlier_iteration(n):
    keys = list(range(17))
    previous = []
    out = []
    for i in range(n):
        current = [0]
        for p in previous:
            out.append(p in keys)
        current.append([])
        previous = current
    return out


def old_list_from_earlier_iteration(n):
    keys = list(range(17))
    previous = []
    out = []
    for i in range(n):
        current = [0]
        keys_set = set(keys)
        for p in previous:
            out.append(p in keys_set)
        current.append([])
        previous = current
    return out


# ------------------------------------------------------------------------------
# OPT110: a subscript with a `slice` object is a new list, not an element
# ------------------------------------------------------------------------------
def cx_subscript_with_slice_object(n):
    keys = list(range(17))
    s = slice(0, 1)
    p = keys[s]
    out = []
    for _ in range(n):
        out.append(p in keys)
    return out


def old_subscript_with_slice_object(n):
    keys = list(range(17))
    s = slice(0, 1)
    p = keys[s]
    out = []
    keys_set = set(keys)
    for _ in range(n):
        out.append(p in keys_set)
    return out


# ------------------------------------------------------------------------------
# OPT102 (default `--fix`): insertions that run key code, observed through the order or a
# handler
# ------------------------------------------------------------------------------
class RaisingKey:
    def __hash__(self):
        return 0

    def __eq__(self, other):
        raise ValueError("compared")


def cx_partly_updated_in_handler():
    d = {RaisingKey(): 0}
    other = {1: 1, 0: 2}
    try:
        for _ in range(1):
            d = {**d, **other}
    except ValueError:
        return 1 in d
    return None


def old_partly_updated_in_handler():
    d = {RaisingKey(): 0}
    other = {1: 1, 0: 2}
    try:
        for _ in range(1):
            d.update(other)
    except ValueError:
        return 1 in d
    return None


def cx_set_partly_updated_in_handler():
    s = {RaisingKey()}
    other = {8, 1, 0}
    try:
        for _ in range(1):
            s = s | other
    except ValueError:
        return 1 in s
    return None


def old_set_partly_updated_in_handler():
    s = {RaisingKey()}
    other = {8, 1, 0}
    try:
        for _ in range(1):
            s |= other
    except ValueError:
        return 1 in s
    return None


def later_entries():
    raise KeyError("later")


def cx_later_entry_before_collision():
    d = {RaisingKey(): 0}
    try:
        for _ in range(1):
            d = {**d, **{0: 1}, **later_entries()}
    except Exception as e:  # noqa: BLE001 - the exception type is the observable
        return type(e).__name__
    return None


def old_later_entry_before_collision():
    d = {RaisingKey(): 0}
    try:
        for _ in range(1):
            d.update({**{0: 1}, **later_entries()})
    except Exception as e:  # noqa: BLE001
        return type(e).__name__
    return None


class LoggedKey:
    def __hash__(self):
        return 0

    def __eq__(self, other):
        CALLS.append("eq")
        return False


def cx_entry_reads_what_eq_changes():
    d = {LoggedKey(): 0}
    part = {LoggedKey(): 0}
    for _ in range(1):
        d = {**d, **part, 1: len(CALLS)}
    return d[1]


def old_entry_reads_what_eq_changes():
    d = {LoggedKey(): 0}
    part = {LoggedKey(): 0}
    for _ in range(1):
        d.update({**part, 1: len(CALLS)})
    return d[1]


# ------------------------------------------------------------------------------
# OPT130: what the hoisted constructor shares, expands, or receives
# ------------------------------------------------------------------------------
def cx_rebuilt_container_changed(n):
    keys = list(range(17))
    hits = 0
    for _ in range(n):
        if 0 in (tmp := list(keys)):
            hits += 1
            tmp.clear()
    return hits


def old_rebuilt_container_changed(n):
    keys = list(range(17))
    hits = 0
    keys_list = list(keys)
    for _ in range(n):
        if 0 in (tmp := keys_list):
            hits += 1
            tmp.clear()
    return hits


def cx_rebuild_with_unpacked_argument(packed, items):
    args = tuple(packed)
    hits = 0
    for x in items:
        if x in list(*args):
            hits += 1
    return hits


def old_rebuild_with_unpacked_argument(packed, items):
    args = tuple(packed)
    hits = 0
    args_list = list(*args)
    for x in items:
        if x in args_list:
            hits += 1
    return hits


def cx_rebuild_copy_with_argument(n):
    keys = list(range(17))
    hit = None
    for i in range(n):
        hit = i in set(keys.copy(1))
    return hit


def old_rebuild_copy_with_argument(n):
    keys = list(range(17))
    hit = None
    hoisted = set(keys.copy(1))
    for i in range(n):
        hit = i in hoisted
    return hit


# ------------------------------------------------------------------------------
# OPT110: a bare annotation makes `set` a local name of the function
# ------------------------------------------------------------------------------
def cx_set_declared_without_value(n):
    set: object
    keys = list(range(17))
    hit = None
    for i in range(n):
        hit = i in keys
    return hit


def old_set_declared_without_value(n):
    set: object
    keys = list(range(17))
    hit = None
    keys_set = set(keys)
    for i in range(n):
        hit = i in keys_set
    return hit


# ------------------------------------------------------------------------------
# OPT120 / OPT121: what a deque does differently while it is built, extended, or iterated
# ------------------------------------------------------------------------------
def options():
    CALLS.append("options")
    return {}


def cx_deque_keyword_arguments(n):
    q = list(**options())
    for i in range(n):
        q.insert(0, i)
    return list(q)


def old_deque_keyword_arguments(n):
    q = collections.deque()
    for i in range(n):
        q.appendleft(i)
    return list(q)


def cx_extended_by_its_own_iterator(n):
    q = list(range(n))
    q.extend(reversed(q))
    out = []
    while q:
        out.append(q.pop(0))
    return out


def old_extended_by_its_own_iterator(n):
    q = collections.deque(range(n))
    q.extend(reversed(q))
    out = []
    while q:
        out.append(q.popleft())
    return out


def cx_iterator_type_observed(n):
    q = list(range(n))
    while q:
        q.pop(0)
    return type(iter(q)).__name__


def old_iterator_type_observed(n):
    q = collections.deque(range(n))
    while q:
        q.popleft()
    return type(iter(q)).__name__


def cx_stepped_slice(n):
    q = list(range(n))
    steps = 0
    while q:
        q = q[1::2]
        steps += 1
    return steps


def old_stepped_slice(n):
    q = collections.deque(range(n))
    steps = 0
    while q:
        q.popleft()
        steps += 1
    return steps


def cx_iterator_returned(n):
    q = list(range(n))
    while q:
        q.pop(0)
    return iter(q)


def old_iterator_returned(n):
    q = collections.deque(range(n))
    while q:
        q.popleft()
    return iter(q)


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
        (cx_changed_in_iterable, old_changed_in_iterable, lambda f: f(18)),
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
        (cx_kept_by_pop_default, old_kept_by_pop_default, lambda f: f([1, 2])),
        (cx_compared_by_remove, old_compared_by_remove, lambda f: f([1, 2])),
        (cx_function_kept_by_map, old_function_kept_by_map, lambda f: f([1, 2])),
        (cx_map_over_ints, old_map_over_ints, lambda f: f(2)),
        (cx_generator_over_ints, old_generator_over_ints, lambda f: f(2)),
        (cx_ints_or_parameter, old_ints_or_parameter, lambda f: f(False, [[]])),
        (cx_range_or_parameter, old_range_or_parameter, lambda f: f(True, [[]])),
        (cx_tuple_or_element, old_tuple_or_element, lambda f: f(False, 0)),
        (cx_elements_after_escape, old_elements_after_escape, lambda f: f(1)),
        (cx_list_from_earlier_iteration, old_list_from_earlier_iteration, lambda f: f(3)),
        (cx_subscript_with_slice_object, old_subscript_with_slice_object, lambda f: f(2)),
        (cx_partly_updated_in_handler, old_partly_updated_in_handler, lambda f: f()),
        (
            cx_set_partly_updated_in_handler,
            old_set_partly_updated_in_handler,
            lambda f: f(),
        ),
        (cx_later_entry_before_collision, old_later_entry_before_collision, lambda f: f()),
        (cx_entry_reads_what_eq_changes, old_entry_reads_what_eq_changes, lambda f: f()),
        (cx_rebuilt_container_changed, old_rebuilt_container_changed, lambda f: f(5)),
        (
            cx_rebuild_with_unpacked_argument,
            old_rebuild_with_unpacked_argument,
            lambda f: f((iter([1]),), [1, 1]),
        ),
        (cx_rebuild_copy_with_argument, old_rebuild_copy_with_argument, lambda f: f(0)),
        (cx_set_declared_without_value, old_set_declared_without_value, lambda f: f(3)),
        (cx_deque_keyword_arguments, old_deque_keyword_arguments, lambda f: f(3)),
        (cx_extended_by_its_own_iterator, old_extended_by_its_own_iterator, lambda f: f(2)),
        (cx_iterator_type_observed, old_iterator_type_observed, lambda f: f(3)),
        (cx_stepped_slice, old_stepped_slice, lambda f: f(9)),
        (cx_iterator_returned, old_iterator_returned, lambda f: type(f(3)).__name__),
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
