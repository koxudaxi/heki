#!/usr/bin/env python3
"""End-to-end cases for the hoisting rules `OPT110`, `OPT111`, and `OPT130` (Milestone M8).

`crates/heki/tests/e2e_soundness.rs` pins every function's diagnostics and checks on CPython that
`heki check --fix --include-size-dependent` keeps this script's output unchanged.
"""
from __future__ import annotations

NINE = [1, 2, 3, 4, 5, 6, 7, 8, 9]


# ==============================================================================
# OPT110: `e in lst`
# ==============================================================================
def tuple_container(n):
    """A tuple bound outside the loop is invariant whatever happens in it."""
    allowed = (1, 2, 3, 4, 5, 6, 7, 8, 9)
    hits = []
    for p in range(n):
        if p in allowed:
            hits.append(p)
    return hits


def while_loop(n):
    """No `for` statement to hoist before: proven safe, rewritten by hand."""
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    hits = []
    i = 0
    while i < n:
        if i in allowed:
            hits.append(i)
        i += 1
    return hits


def comprehension(n):
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    return [p for p in range(n) if p in allowed]


def split_for_header(n):
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    hits = []
    for \
            p in range(n):
        if p in allowed:
            hits.append(p)
    return hits


def rebound_in_loop(n):
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    hits = []
    for p in range(n):
        if p in allowed:
            hits.append(p)
        allowed = [p]
    return hits


def changed_twice_in_loop(n):
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    hits = []
    for p in range(n):
        if p in allowed:
            hits.append(p)
        allowed.append(p)
        allowed.append(-p)
    return hits


def changed_in_for_header(n):
    """`allowed.pop()` runs after a hoisted `set(allowed)` would be built."""
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]
    hits = []
    for p in range(allowed.pop() + n):
        if p in allowed:
            hits.append(p)
    return hits


def changed_before_and_after(n):
    allowed = [1, 2, 3, 4, 5, 6, 7, 8]
    allowed.append(9)
    hits = []
    for p in range(n):
        if p in allowed:
            hits.append(p)
    allowed.append(10)
    return hits, allowed


def parameter_container(n, allowed):
    """Nothing shows a linear scan: not reported."""
    hits = []
    for p in range(n):
        if p in allowed:
            hits.append(p)
    return hits


def literal_container(n):
    """A display inside the comparison is not a candidate."""
    hits = []
    for p in range(n):
        if p in [1, 2, 3, 4, 5, 6, 7, 8, 9]:
            hits.append(p)
    return hits


def string_container(n):
    """`str` membership is a substring test."""
    text = "0123456789"
    return [p for p in range(n) if str(p) in text]


def unknown_elements(items, n):
    allowed = list(items)
    hits = []
    for p in range(n):
        if p in allowed:
            hits.append(p)
    return hits


def unbound_in_dead_code(n, c):
    """`allowed` is unbound on a path that cannot run (after `return`)."""
    if c:
        allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    else:
        return []
        pass
    hits = []
    for p in range(n):
        if p in allowed:
            hits.append(p)
    return hits


def call_probe(words):
    allowed = ["a", "b", "c", "d", "e", "f", "g", "h", "i"]
    return [w for w in words if str(w) in allowed]


def method_probe(words):
    allowed = ["a", "b", "c", "d", "e", "f", "g", "h", "i"]
    hits = []
    for w in words:
        if w.lower() in allowed:
            hits.append(w)
    return hits


def generator_probe(words):
    allowed = ["a", "b", "c", "d", "e", "f", "g", "h", "i"]
    hits = []
    for w in words:
        if (c for c in w) in allowed:
            hits.append(w)
    return hits


def attribute_probe(points):
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    hits = []
    for pt in points:
        if pt.real in allowed:
            hits.append(pt)
    return hits


def mixed_probe(n):
    """`p` is an `int` on one path and a `str` on the other."""
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9, "a"]
    hits = []
    for i in range(n):
        p = i if i % 2 else "a"
        if p in allowed:
            hits.append(p)
    return hits


def empty_probe():
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    hits = []
    for p in list():
        if p in allowed:
            hits.append(p)
    return hits


def deleted_container(n):
    """`del allowed` before the test: the value is not tracked (raises `NameError`)."""
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    hits = []
    for p in range(n):
        print(allowed)
        del allowed
        if p in allowed:
            hits.append(p)
    return hits


def stored_flag(n):
    """The comparison result is stored, not branched on: a plain `OPT110`."""
    seen = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    flags = []
    for v in range(n):
        missing = v not in seen
        flags.append(missing)
    return flags


def rebound_in_outer_loop(sizes, c):
    """`allowed` changes between outer iterations, never during the inner loop: the set is
    built before the inner `for`, once per outer iteration."""
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    hits = []
    for size in sizes:
        for p in range(size):
            if p in allowed:
                hits.append(p)
        if c:
            allowed = [2, 3, 4, 5, 6, 7, 8, 9, 10]
    return hits


def flag_tested_elsewhere(n):
    """The block branches on another test: still a plain `OPT110`."""
    seen = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    flags = []
    for v in range(n):
        missing = v not in seen
        if v % 2:
            flags.append(missing)
    return flags


def write_through_iterator(n):
    """A write through an iterator of `allowed` counts as a change of `allowed`."""
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    it = iter(allowed)
    hits = []
    for p in range(n):
        if p in allowed:
            hits.append(p)
        it[0] = p
    return hits


# ==============================================================================
# OPT111: `if v not in seen: seen.append(v)`
# ==============================================================================
def dedup_rebound(n):
    seen = []
    for v in range(n):
        if v not in seen:
            seen.append(v)
        seen = list(seen)
    return seen


def dedup_other_changes(n):
    seen = []
    for v in range(n):
        if v not in seen:
            seen.append(v)
        if len(seen) > 5:
            seen.remove(v)
            seen.pop()
    return seen


def dedup_live_alias(n):
    seen = []
    alias = seen
    for v in range(n):
        w = v % 3
        if w not in seen:
            seen.append(w)
    return seen, alias


def dedup_parameter(n, seen):
    for v in range(n):
        if v not in seen:
            seen.append(v)
    return seen


def dedup_result_used(n):
    seen = []
    log = []
    for v in range(n):
        w = v % 4
        if w not in seen:
            log.append(seen.append(w))
    return seen, log


def dedup_while(n):
    seen = []
    v = 0
    while v < n:
        w = v % 4
        if w not in seen:
            seen.append(w)
        v += 1
    return seen


def dedup_few():
    seen = []
    for v in range(5):
        if v not in seen:
            seen.append(v)
    return seen


def dedup_set_append(items):
    """`set.append` does not exist: not a list, not a candidate."""
    seen = set()
    for v in items:
        if v not in seen:
            seen.append(v)
    return seen


def dedup_in_for_header(items):
    """`seen.pop()` in the `for` header runs after a hoisted `set(seen)` would be built."""
    seen = [1, 2, 3]
    for v in range(seen.pop() + len(items)):
        if v not in seen:
            seen.append(v)
    return seen


# ==============================================================================
# OPT130: `e in set(keys)`
# ==============================================================================
def rebuild_list(n):
    keys = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    return [p for p in range(n) if p in list(keys)]


def rebuild_tuple_of_str(n):
    letters = "abcdefghij"
    hits = 0
    for i in range(n):
        if "abcdefghij"[i % 10] in tuple(letters):
            hits += 1
    return hits


def rebuild_set_of_set(n):
    keys = {1, 2, 3, 4, 5, 6, 7, 8, 9}
    hits = 0
    for p in range(n):
        if p in set(keys):
            hits += 1
    return hits


def rebuild_set_of_copy(n):
    keys = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    hits = 0
    for p in range(n):
        if p in set(keys.copy()):
            hits += 1
    return hits


def rebuild_empty(n):
    hits = 0
    for p in range(n):
        if p in set():
            hits += 1
    return hits


def rebuild_small(n):
    keys = [1, 2, 3]
    hits = 0
    for p in range(n):
        if p in set(keys):
            hits += 1
    return hits


def rebuild_display(n):
    hits = 0
    for p in range(n):
        if p in set([1, 2, 3, 4, 5, 6, 7, 8, 9]):
            hits += 1
    return hits


def rebuild_frozenset(n):
    keys = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    hits = 0
    for p in range(n):
        if p in frozenset(keys):
            hits += 1
    return hits


def rebuild_keywords(n):
    hits = 0
    for p in range(n):
        if p in dict(a=1):
            hits += 1
    return hits


def rebuild_choice(n, c, other):
    keys = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    hits = 0
    for p in range(n):
        if p in set(keys if c else other):
            hits += 1
    return hits


def rebuild_unknown_call(n, load):
    hits = 0
    for p in range(n):
        if p in set(load(n)):
            hits += 1
    return hits


def rebuild_popped(groups):
    stack = [[1, 2], [3], [4, 5, 6]]
    hits = 0
    for g in groups:
        if g in set(stack.pop()):
            hits += 1
    return hits


def rebuild_dict_copy(n):
    table = {1: "a", 2: "b", 3: "c"}
    hits = 0
    for p in range(n):
        if p in list(table.copy()):
            hits += 1
    return hits


def rebuild_unknown_elements(n, items):
    pairs = tuple(items)
    hits = 0
    for p in range(n):
        if p in set(pairs):
            hits += 1
    return hits


def rebuild_parameter(n, keys):
    hits = 0
    for p in range(n):
        if p in set(keys):
            hits += 1
    return hits


def rebuild_rebound_key(n):
    keys = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    hits = 0
    for p in range(n):
        if p in set(keys):
            hits += 1
        keys = [p, p + 1]
    return hits


def rebuild_dict(n):
    table = {1: "a", 2: "b", **{p: "c" for p in range(n)}}
    hits = 0
    for p in range(n):
        if p in dict(table):
            hits += 1
    return hits


def rebuild_number(n):
    """`list(5)` raises `TypeError`: hoisting would raise even for a zero-trip loop."""
    five = 5
    hits = 0
    for p in range(n):
        if p in list(five):
            hits += 1
    return hits


def bytes_display(n):
    """A display of 3 `bytes` constants: a scan costs no more than hashing."""
    allowed = [b"1", b"2", b"3"]
    hits = 0
    for p in range(n):
        if str(p).encode() in allowed:
            hits += 1
    return hits


def unhashable_elements(n):
    """A list of dicts: no `set` can hold its elements."""
    table = [{"k": i} for i in range(9)]
    hits = 0
    for p in range(n):
        if {"k": p} in table:
            hits += 1
    return hits


def probes_from_a_list(n):
    """Iterating a list of `int`s yields `int`s: hash-consistent probes."""
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    probes = list(range(n))
    hits = []
    for p in probes:
        if p in allowed:
            hits.append(p)
    return hits


def probes_from_either_list(n, c):
    """Both lists a branch may pick hold only `int`s."""
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    probes = list(range(n)) if c else list(range(n, 2 * n))
    hits = []
    for p in probes:
        if p in allowed:
            hits.append(p)
    return hits


def probes_measured_first(n):
    """`len` only reads the list: its elements stay known."""
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    probes = list(range(n))
    hits = [len(probes)]
    for p in probes:
        if p in allowed:
            hits.append(p)
    return hits


def probe_by_index(n):
    """`keys[True]` reads one `int` element."""
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    keys = list(range(n + 2))
    probe = keys[True]
    hits = []
    for _ in range(n):
        if probe in allowed:
            hits.append(probe)
    return hits


def lookup_in_a_bounded_loop(n):
    """One iteration: building a set costs more than the scan it saves."""
    allowed = list(range(n + 100))
    hits = []
    for i in range(1):
        if i in allowed:
            hits.append(i)
    return hits


def rebuild_in_a_bounded_loop(n):
    keys = list(range(n + 100))
    hits = 0
    for i in range(1):
        if i in set(keys):
            hits += 1
    return hits


def rebuild_tuple_of_a_tuple(n):
    """`tuple(t)` of a tuple returns `t`: no container is built."""
    keys = tuple(range(9))
    hits = 0
    for i in range(n):
        if i in tuple(keys):
            hits += 1
    return hits


def chained_membership(n):
    """`i in allowed in allowed`: the last name is the second comparison's operand."""
    allowed = list(range(9))
    out = []
    for i in range(n):
        out.append(i in allowed in allowed)
    return out


def comprehension_clause_on_its_own_line(n):
    """The `for` clause starts a line, but it is inside the brackets: no statement to hoist
    before."""
    allowed = [1, 2, 3, 4, 5, 6, 7, 8, 9]
    return [p
            for p in range(n) if p in allowed]


def generator_clause_on_its_own_line(n):
    return sum(1
               for p in "abcdefghijkl"[:n]
               if p in set("abcdefghi"))


def two_hoists_in_nested_loops(n):
    """Both rebuilds are hoisted; the inner set must not take the outer one's name."""
    outer = inner = 0
    for i in range(n):
        if "a" in set("abcdefghi"):
            outer += 1
        for j in range(n):
            if "a" in set("jklmnopqr"):
                inner += 1
    return outer, inner


def run(fn, *args):
    try:
        return repr(fn(*args))
    except Exception as e:  # noqa: BLE001 - the transcript records every outcome
        return f"{type(e).__name__}"


def main() -> int:
    lines = []
    for n in [0, 1, 4, 12]:
        items = list(range(n))
        words = ["A", "b", "zz", "C", "e"][: n % 6]
        lines += [
            run(tuple_container, n),
            run(while_loop, n),
            run(comprehension, n),
            run(split_for_header, n),
            run(rebound_in_loop, n),
            run(changed_twice_in_loop, n),
            run(changed_in_for_header, n),
            run(changed_before_and_after, n),
            run(parameter_container, n, NINE),
            run(literal_container, n),
            run(string_container, n),
            run(unknown_elements, items, n),
            run(unbound_in_dead_code, n, n % 2),
            run(call_probe, words),
            run(method_probe, words),
            run(generator_probe, words),
            run(attribute_probe, [complex(i, 1) for i in items]),
            run(mixed_probe, n),
            run(empty_probe),
            run(stored_flag, n),
            run(rebound_in_outer_loop, [n, n + 2], n % 2),
            run(flag_tested_elsewhere, n),
            run(dedup_rebound, n),
            run(dedup_other_changes, n),
            run(dedup_live_alias, n),
            run(dedup_parameter, n, []),
            run(dedup_result_used, n),
            run(dedup_while, n),
            run(dedup_few),
            run(dedup_in_for_header, items),
            run(rebuild_list, n),
            run(rebuild_tuple_of_str, n),
            run(rebuild_set_of_set, n),
            run(rebuild_set_of_copy, n),
            run(rebuild_empty, n),
            run(rebuild_small, n),
            run(rebuild_display, n),
            run(rebuild_frozenset, n),
            run(rebuild_keywords, n),
            run(rebuild_choice, n, n % 2, [n]),
            run(rebuild_unknown_call, n, lambda p: [p]),
            run(rebuild_popped, [3, 5][: n % 3]),
            run(rebuild_dict_copy, n),
            run(rebuild_unknown_elements, n, items),
            run(rebuild_parameter, n, items),
            run(rebuild_rebound_key, n),
            run(rebuild_dict, n),
            run(rebuild_number, n),
            run(bytes_display, n),
            run(unhashable_elements, n),
            run(probes_from_a_list, n),
            run(probes_from_either_list, n, n % 2),
            run(probes_measured_first, n),
            run(probe_by_index, n),
            run(lookup_in_a_bounded_loop, n),
            run(rebuild_in_a_bounded_loop, n),
            run(rebuild_tuple_of_a_tuple, n),
            run(chained_membership, n),
            run(comprehension_clause_on_its_own_line, n),
            run(generator_clause_on_its_own_line, n),
            run(two_hoists_in_nested_loops, n),
        ]
    print("\n".join(lines))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
