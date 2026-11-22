#!/usr/bin/env python3
"""Analysis-cost gate: run heki on generated shapes at the largest size it still models, and fail
when one costs more than 1 s or 1 GiB beyond an empty module (docs/development.md, "Analysis
cost").

Each family is a source shape that grows with n: a chain of calls each handed the previous
result, conditional merges of call results, one iterator consumed by many loops, phi diamonds,
`isinstance` tests followed by `continue`, copies a loop carries, and so on. A family that lives
in one function is placed three ways (a function, a method, and a module body) and grown to the
largest n whose function the IR budget still models: n doubles, then bisects, with
`heki assumptions --json` telling a modeled function from an unmodeled one, so a change to the
builder moves the size with it. A family spread over a project (a wide class hierarchy, a chain
of private helpers, a chain of container subclasses) has a fixed size, and so has a family in one
function whose cost already shows at a smaller size, where the walks it does not gate stay cheap.

Each shape is checked with every rule selected and one contract of each kind naming the classes
and functions the shapes define (a family may add contracts of its own, such as thousands of
argument lists), through `heki check` and through the `--watch` engine; a family of the facts only
`heki recommend` reads also runs through `heki dump-recommend-facts`. The cost
is the wall time and the peak (the heap of the memory gate's counting build, or what the system
reports, see `check_memory.py`) minus those of an empty module; the time limit is scaled to the
machine by fixed linear shapes of calls and of loops, timed on a fixed release of heki (from
PyPI, through pip), not on the binary under test.

`heki check --walk-steps` gives the steps each owner's walks took per function. They must grow
at most 2.5 times from n / 2 to n (quadratic growth inside the budget shows without timing
noise), and no budget may be spent on a family shaped like real code.

`KNOWN_STEPS` lists, the same way, the shapes whose steps are known to grow faster than linearly
or to spend a budget.

`KNOWN_OVER` lists the shapes known to cost more than the limits; the list only shrinks: a shape
listed there that costs less than a quarter of the limits fails the gate until it is removed. The
band between keeps a shape near the limit from failing on one machine and passing on another
(shapes scale differently across machines than the calibration shape does).

A family marked `open_premises` runs `heki dump-open-premises` instead of `heki check`: the
premises its heuristic reports leave open are explained only there.

`--shard k/n` runs every n-th shape, starting at the k-th (each placement of a family is a shape),
after the same calibration: CI runs the shapes in parallel jobs.

Usage:
    python3 scripts/check_walk_shapes.py --heki target/release/heki
    python3 scripts/check_walk_shapes.py --heki target/release/heki --only call_chain --report
    python3 scripts/check_walk_shapes.py --heki target/release/heki --shard 1/2
"""

from __future__ import annotations

import argparse
import contextlib
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
import zipfile
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_memory import HEAP_LINE, MIB, execute  # noqa: E402

LIMIT_SECONDS = 1.0
LIMIT_MIB = 1024.0
# The seconds are those of the machine the limits were measured on (an Apple M4 Max). Fixed
# linear shapes, one of calls and one of loops (`reference_source`), take these seconds there on
# the release `REFERENCE_HEKI` (beyond an empty module), and the time limit scales by the largest
# factor by which the machine running the gate is slower on one of them (never below 1): machines
# differ unevenly on calls and on loops, and the gated shapes do both. The references never run
# on the binary under test, whose changes would move every limit: a reference made cheaper
# tightens them all, one made dearer loosens them all. Another `REFERENCE_HEKI` or
# `reference_source` needs these seconds measured again on that machine (the gate prints them).
REFERENCE_HEKI = "0.2.4"
REFERENCES = (("calls", 3104, 0.225), ("loops", 2000, 0.105))
# A run past this is over the limits whatever it would have taken.
TIMEOUT_SECONDS = 10.0
# The steps of an owner may grow this much from n / 2 to n.
MAX_STEP_GROWTH = 2.5
FIRST_N = 64
# Sizes past this are not tried: the source would pass megabytes before the IR budget.
MAX_N = 1 << 15

PLACED = ("function", "method", "module")

# Shapes known to cost more than the limits, with the walk that costs: (family, placement).
KNOWN_OVER: dict[tuple[str, str], str] = {
    **{
        ("returned_merge_chain", p): "the summary keeps a site per call a merged argument may be "
        "the result of (`iterations`), until the iteration facts pass their budget"
        for p in PLACED
    },
    **{
        ("one_iterator_many_loops", p): "MUT106 walks the blocks between every pair of loops "
        "consuming the iterator"
        for p in PLACED
    },
    **{
        ("doubled_zips", p): "the size of a `zip` asks both arguments without keeping answers: "
        "2^16 questions per loop (`FnIndex::iter_bound_depth`)"
        for p in PLACED
    },
    ("wide_hierarchy", "project"): "each method call walks the whole subclass family of its "
    "receiver's class (`ProjectDecls::definitions`)",
    ("callers_of_one_helper", "project"): "MUT201 walks the helper's callers again per caller",
}

# Shapes known to take steps that grow faster than linearly or to spend a budget, with the walk:
# (family, placement). The list only shrinks: a listed shape that does neither fails the gate.
KNOWN_STEPS: dict[tuple[str, str], str] = {
    **{
        ("isinstance_continue", p): "the path facts of a loop header with many `continue`s "
        "(`Extractor::path_facts`, module bindings) grow quadratically and spend their budget"
        for p in PLACED
    },
    **{
        ("returned_merge_chain", p): "each argument may be the result of every call before it: "
        "the roots of the merged values (`consumption::IterFlow`) grow quadratically and spend "
        "their budget"
        for p in PLACED
    },
}

HELPERS = '''import requests


class Base:
    LIMIT = 3

    def m(self):
        return self.LIMIT

    def reset(self):
        self.cache = []


def g(x):
    return x


def use(x):
    return x


def load():
    return [1, 2, 3]


'''

CONTRACTS = '''[[write-owners]]
name = "shape-writes"
targets = ["Base.cache", "Shapes.cache"]
allowed-writers = ["Base.*"]

[[read-owners]]
name = "shape-reads"
targets = ["Base.LIMIT"]
allowed-readers = ["Base.*"]
unresolved = "report"

[[call-owners]]
name = "shape-calls"
targets = ["Base.m", "g"]
allowed-callers = ["Base.*"]
unresolved = "report"
references = "reads"

[[effect-boundaries]]
name = "shape-boundary"
scopes = ["*.shape", "shape", "entry"]
forbid = ["writes-targets", "blocking"]
write-targets = ["Base.*"]

[[readonly-returns]]
name = "shape-returns"
producers = ["load"]

[[invalidation]]
name = "shape-invalidation"
targets = ["Base.cache"]
invalidators = ["Base.reset"]
'''


@dataclass(frozen=True)
class Body:
    """The statements of a shape inside one function: its parameters and its lines, the last of
    which may be a `return`."""

    params: tuple[str, ...]
    lines: list[str]


def call_chain(n: int) -> Body:
    """Each call is handed the previous call's result."""
    lines = ["v0 = p", *(f"v{i} = g(v{i - 1})" for i in range(1, n + 1)), f"return v{n}"]
    return Body(("p", "session"), lines)


def returned_merge_chain(n: int) -> Body:
    """`x_i = g(x_{i-1}) if flag else x_{i-1}`, the last returned."""
    lines = [f"x{i} = g(x{i - 1}) if flag else x{i - 1}" for i in range(1, n + 1)]
    return Body(("x0", "flag"), [*lines, f"return x{n}"])


def one_iterator_many_loops(n: int) -> Body:
    """One iterator consumed by many loops after one another."""
    lines = ["it = iter(xs)"]
    for j in range(n):
        lines += ["for _ in it:", f"    out.append({j})"]
    return Body(("xs", "out"), lines)


def phi_diamonds(n: int) -> Body:
    """`if`/`else` merges each joining two values computed from the previous merge."""
    lines = ["v = xs"]
    for i in range(n):
        lines += [f"if c[{i}]:", "    v = v[:3]", "else:", "    v = v[1:4]"]
    return Body(("c", "xs"), [*lines, "for _ in v:", "    xs.append(1)"])


def isinstance_continue(n: int) -> Body:
    """A loop header's phi is its own input once per `continue`."""
    lines = ["acc = []", "while pending:", "    cur = pending.pop()"]
    for i in range(n):
        lines += [f"    if isinstance(cur, C{i}):", "        acc = acc + [cur]", "        continue"]
    return Body(("pending", "cur"), [*lines, "    acc.append(cur)", "return acc"])


def copy_chain_in_loop(n: int) -> Body:
    """A loop body copies one list along a chain, then calls a method on the last copy."""
    lines = ["for it in items:", "    y0 = xs"]
    lines += [f"    y{i} = y{i - 1}" for i in range(1, n + 1)]
    return Body(("items", "xs"), [*lines, f"    y{n}.remove(it)"])


def unpacked_display_copies(n: int) -> Body:
    """One dict display copied along a chain, each copy unpacked with `**` into a call."""
    lines = ['f0 = {"x": p}', *(f"f{i} = f{i - 1}" for i in range(1, n + 1))]
    return Body(("p",), [*lines, *(f"g(**f{i})" for i in range(n + 1))])


def wide_display_unpacked(n: int) -> Body:
    """A dict display of n entries unpacked with `**` by n calls."""
    entries = ", ".join(f'"x{i}": p' for i in range(n))
    return Body(("p",), [f"f = {{{entries}}}", *("g(**f)" for _ in range(n))])


def reverse_chain_in_loop(n: int) -> Body:
    """A loop carries a chain of copies laid against the order of the blocks."""
    lines = ["acc = []", *(f"x{i} = None" for i in range(1, n + 1)), "x0 = acc", "for it in items:"]
    lines += [f"    x{i} = x{i - 1}" for i in range(n, 0, -1)]
    lines += [f"    x{n}.append(it)", "    for _ in x0:", "        pass"]
    return Body(("items",), [*lines, "return acc"])


def alias_loops(n: int) -> Body:
    """Each loop iterates a new alias of one small display."""
    lines = ["a0 = [1, 2, 3]"]
    for i in range(1, n + 1):
        lines += [f"a{i} = a{i - 1}", f"for _ in a{i}:", "    xs.append(1)"]
    return Body(("xs",), lines)


def paced_loops(n: int) -> Body:
    """Loops after one another, each paced by a sleep on its way back."""
    lines = []
    for j in range(n):
        lines += ["while True:", f"    if out.poll({j}):", "        break", "    clock.sleep(1)"]
    return Body(("clock", "out"), lines)


def doubled_constant_slices(n: int) -> Body:
    """Integer constants doubling through `+`, each loop slicing by the last."""
    lines = ["c0 = 1", *(f"c{i} = c{i - 1} + c{i - 1}" for i in range(1, 17))]
    for j in range(n):
        lines += ["for _ in xs[:c16]:", f"    out.append({j})"]
    return Body(("xs", "out"), lines)


def doubled_zips(n: int) -> Body:
    """Each `zip` takes the previous one twice; many loops ask the size of the last."""
    lines = ["z0 = xs", *(f"z{i} = zip(z{i - 1}, z{i - 1})" for i in range(1, 17))]
    for j in range(n):
        lines += ["for _ in z16:", f"    out.append({j})"]
    return Body(("xs", "out"), lines)


def lists_extending_each_other(n: int) -> Body:
    """Lists each extended by the next, the last by the first."""
    lines = [f"a{i} = []" for i in range(n)]
    lines += [f"a{i}.extend(a{(i + 1) % n})" for i in range(n)]
    return Body(("src",), [*lines, "for _ in a0:", "    src.append(1)"])


def item_reads(n: int) -> Body:
    """Many item reads of one parameter."""
    return Body(("xs", "out"), [f"out.append(xs[{i}])" for i in range(n)])


def merged_flag_loop(n: int) -> Body:
    """A loop-carried flag merged many times from two inputs of its previous value."""
    lines = ["t = p is None", "for x in xs:"]
    lines += [f"    t = (not t) if c[{i}] else t" for i in range(n)]
    return Body(("xs", "c", "p"), [*lines, "if t:", "    requests.get(p)"])


def lazy_init_sites(n: int) -> Body:
    """One lazy initialization in a loop, many calls in its region."""
    lines = ["for it in items:", "    if self._s is None:", "        self._s = requests.Session()"]
    lines += [f"        self._s.get(it{i % 7})" for i in range(n)]
    return Body(("items",), [*lines, "    it.x = 1"])


def self_item_reads(n: int) -> Body:
    """Items of constant keys of one attribute of `self`, each read on both sides of a branch: the
    reads of each key on every path are asked together."""
    lines = ["total = 0"]
    for i in range(n):
        lines += ["if flag:", f'    total += self.d["k{i}"]', "else:", f'    total -= self.d["k{i}"]']
    return Body(("flag",), [*lines, "return total"])


def writes_then_library_calls(n: int) -> Body:
    """Writes of `self` attributes, each followed by library calls on values of known and unknown
    classes: each call's catalog entry is read once."""
    lines = []
    for i in range(n):
        lines += [f"self.a{i % 50} = len(xs) + {i}", f"out.append(str(xs[{i % 7}]))", f"seen = isinstance(out, list)"]
    return Body(("xs", "out"), [*lines, "return seen"])


def is_tests(n: int) -> Body:
    """Many `is` tests, each followed by a conditional expression on the tested parameter."""
    lines = []
    for i in range(n):
        lines += ["if p is None:", f"    x{i} = g(p) if p is None else p"]
    return Body(("p",), [*lines, "return p"])


def alias_identity_tests(n: int) -> Body:
    """A chain of names each bound to the one before, from an attribute load, each compared by
    identity: every comparison asks what its operands copy."""
    lines = ["v0 = p.attr"] + [f"v{i} = v{i - 1}" for i in range(1, n)]
    lines += [f"x{i} = v{i} is p" for i in range(n)]
    return Body(("p",), [*lines, "return p"])


def match_pass_through(n: int) -> Body:
    """Many `match` statements whose cases slice or pass the value on."""
    lines = ["v = xs"]
    for i in range(n):
        lines += ["match c:", f"    case {i}:", "        v = v[:3]", "    case _:", "        pass"]
    return Body(("c", "xs"), [*lines, "for _ in v:", "    xs.append(1)"])


def with_statements(n: int) -> Body:
    """Many `with` statements one after another."""
    lines = []
    for i in range(n):
        lines += [f"with open(p) as f{i}:", f"    use(f{i})"]
    return Body(("p",), lines)


def element_change_chain(n: int) -> Body:
    """Names each bound to an item of the one before, each changed in place."""
    lines = ["e0 = p"]
    for i in range(1, n + 1):
        lines += [f"e{i} = e{i - 1}['k']", f"e{i}.append({i})"]
    return Body(("p",), lines)


def copied_item_changes(n: int) -> Body:
    """Shallow copies of one parameter, each followed by a change of one of its items."""
    lines = []
    for i in range(n):
        lines += [f"d{i} = dict(p)", f"d{i}['k'].append({i})"]
    return Body(("p",), lines)


def copy_chain(n: int) -> Body:
    """Each name a shallow copy of the one before, an item of the last changed."""
    lines = ["c0 = p", *(f"c{i} = dict(c{i - 1})" for i in range(1, n + 1))]
    return Body(("p",), [*lines, f"c{n}['k'].append(1)"])


def copy_rebound_branches(n: int) -> Body:
    """A loop rebinding an item of a shallow copy on one branch per test, then changing it."""
    lines = ["c = dict(p)", "for x in xs:"]
    for i in range(n):
        lines += [f"    if x == {i}:", "        c['k'] = []"]
    return Body(("p", "xs"), [*lines, "    c['k'].append(x)"])


def attribute_chain(n: int) -> Body:
    """Names each bound to an attribute of the one before."""
    lines = ["a0 = p", *(f"a{i} = a{i - 1}.attr" for i in range(1, n + 1))]
    return Body(("p",), [*lines, f"return a{n}"])


# The declared type of the cell families' names: a wide container, so that joining the values bound
# to a cell costs more than the rest of the work per binding, and the families show a quadratic
# join at a size where the walks they do not gate stay cheap.
CELL_TYPE = f"dict[str, list[tuple[{', '.join(['Base'] * 16)}]]]"
# The size of the cell families.
CELL_BINDINGS = 1024


def cell_reads(n: int) -> Body:
    """Many assignments to one name of a declared container type, each followed by a generator
    expression reading it: a closure cell bound many times and read by as many scopes."""
    lines = [f"x: {CELL_TYPE} = g(0)"]
    for i in range(n):
        lines += [f"x = g({i})", "out.append(sum(1 for _ in xs if x))"]
    return Body(("xs", "out"), lines)


def cell_reads_of_several_names(n: int) -> Body:
    """Assignments to four names in turn, each followed by a generator expression reading all
    four."""
    lines = [f"x{k}: {CELL_TYPE} = g({k})" for k in range(4)]
    for i in range(n):
        lines += [f"x{i % 4} = g({i})", "out.append(sum(1 for _ in xs if x0 and x1 and x2 and x3))"]
    return Body(("xs", "out"), lines)


def guarded_accesses_in_generator_expressions(n: int) -> Body:
    """Generator expressions each reading a guarded attribute and calling a guarded function: the
    generator expressions of a function share one qualname, so each of their call sites runs the
    accesses of all of them."""
    lines = ["b = Base()"]
    lines += ["out.append(sum(1 for _ in xs if b.LIMIT and g(b)))" for _ in range(n)]
    return Body(("xs", "out"), lines)


def read_only_value_handed_to_calls(n: int) -> Body:
    """The value a `readonly-returns` producer returns, handed to many project calls."""
    return Body(("p",), ["v = load()", *("use(v)" for _ in range(n))])


def calls_between_invalidators(n: int) -> Body:
    """Project calls and calls of an invalidator alternating in one block: the contracts announced
    after each call, and the call after it, are looked up instead of scanned per call."""
    lines = ["self.cache = []"]
    for _ in range(n):
        lines += ["g(p)", "self.reset()"]
    return Body(("p",), lines)


def calls_under_branches(n: int) -> Body:
    """Project calls each under its own branch: the next call after each one is found on the
    post-dominator chain once per block instead of once per call."""
    lines = ["v = load()"]
    for _ in range(n):
        lines += ["if p:", "    use(v)"]
    return Body(("p",), lines)


def passed_iterables(n: int) -> Body:
    """Generator expressions of two clauses, each over a value bound just before it: child scopes
    each receiving their iterable from the function creating them, sized where the IR's loop
    bounds are refined and again by `OPT113`, which sizes every loop of a function of two."""
    lines = []
    for i in range(n):
        lines += [f"rows = xs[{i}]", f"out[{i}] = (1 for _ in rows for _ in (1, 2))"]
    return Body(("xs", "out"), lines)


def path_aliases(n: int) -> Body:
    """Merges each joining the previous alias of a package's `__path__` and a new read of it,
    each alias read."""
    lines = ["p0 = requests.__path__"]
    for i in range(1, n + 1):
        lines += [f"p{i} = p{i - 1} if flag else requests.__path__", f"use(p{i}.count(x))"]
    return Body(("flag", "x"), lines)


def merged_package_paths(n: int) -> Body:
    """Merges each joining the paths of the packages before them and a new package's path."""
    lines = ["v0 = requests.p0.__path__"]
    lines += [f"v{i} = v{i - 1} if flag else requests.p{i}.__path__" for i in range(1, n + 1)]
    return Body(("flag",), [*lines, f"return len(v{n})"])


def package_paths_merged_in_functions(n: int) -> str:
    """Functions each merging the paths of `n` packages, the first one's reaching every merge."""
    lines = []
    for f in range(16):
        lines += [f"def merge{f}(flag):", f"    v0 = requests.f{f}_0.__path__"]
        lines += [f"    v{i} = v{i - 1} if flag else requests.f{f}_{i}.__path__" for i in range(1, n + 1)]
        lines += [f"    return len(v{n})", ""]
    return "\n".join(lines)


def wide_hierarchy(n: int) -> str:
    """Many subclasses overriding one method, and as many functions calling it on the base."""
    lines = []
    for i in range(n):
        lines += [f"class C{i}(Base):", "    def m(self):", "        self.cache = []"]
        lines += ["        return super().m()", ""]
    for i in range(n):
        lines += [f"def use{i}(obj: Base):", "    obj.m()", "    return obj.LIMIT", ""]
    return "\n".join(lines)


def self_helper_chain(n: int) -> str:
    """A `@cached_property` reading an attribute through a chain of `self` helpers, each reading
    it on every path and calling the next, and a method writing the attribute."""
    lines = ["import functools", "", "class Chain:", "    def __init__(self):", "        self.a = 0", ""]
    lines += ["    @functools.cached_property", "    def p(self):", "        return self._h0()", ""]
    for i in range(n):
        lines += [f"    def _h{i}(self):", f"        return self.a + self._h{i + 1}()", ""]
    lines += [f"    def _h{n}(self):", "        return self.a", "", "    def bump(self):", "        self.a += 1", ""]
    return "\n".join(lines)


def callers_of_one_helper(n: int) -> str:
    """One private helper writing an attribute, called by many functions."""
    lines = ["def _write(obj: Base):", "    obj.cache = []", ""]
    for i in range(n):
        lines += [f"def call{i}(obj: Base):", "    _write(obj)", "    return obj.m()", ""]
    return "\n".join(lines)


def reports_sharing_a_chain(n: int) -> str:
    """Many `async def`s each reaching a sleep through one chain of helpers, none of which makes
    the next call on every return: each report's open premises are explained along the chain."""
    depth = 100
    lines = ["import time", "", "def helper0(flag):", "    if flag:", "        return None"]
    lines += ["    time.sleep(1)", ""]
    for i in range(1, depth):
        lines += [f"def helper{i}(flag):", f"    helper{i - 1}(flag)", ""]
    for j in range(n):
        lines += [f"async def handler{j}(flag):", f"    helper{depth - 1}(flag)", ""]
    return "\n".join(lines)


def functions_each_calling(n: int) -> str:
    """Many functions each making one project call."""
    lines = []
    for i in range(n):
        lines += [f"def call{i}(x):", "    return g(x)", ""]
    return "\n".join(lines)


def helper_chain(n: int) -> str:
    """Private helpers each calling the next twice, the last mutating its parameter."""
    lines = []
    for i in range(n):
        lines += [f"def _h{i}(d):", f"    _h{i + 1}(d)", f"    _h{i + 1}(d)", ""]
    lines += [f"def _h{n}(d):", "    d.append(1)", "", "def entry(d):", "    _h0(d)", ""]
    return "\n".join(lines)


def sentinel_subclasses(n: int) -> str:
    """A dataclass whose field defaults to an `object()` sentinel, with many subclasses, each
    tested against the sentinel and deep-copied in functions of its own, and the base
    deep-copied as often."""
    lines = ["import copy", "from dataclasses import dataclass", "", "MISSING = object()", ""]
    lines += ["@dataclass", "class Spec:", "    value: object = MISSING", ""]
    for i in range(n):
        lines += [f"class Spec{i}(Spec):", "    pass", ""]
        lines += [f"def given{i}(s: Spec{i}):", "    return s.value is not MISSING", ""]
        lines += [f"def isolated{i}(s: Spec{i}):", "    return copy.deepcopy(s)", ""]
        lines += [f"def isolated_base{i}(s: Spec):", "    return copy.deepcopy(s)", ""]
    return "\n".join(lines)


def sentinels_per_class(n: int) -> str:
    """Many `object()` sentinels, each the default of a field of its own dataclass, tested
    against it and deep-copied in functions of its own."""
    lines = ["import copy", "from dataclasses import dataclass", ""]
    for i in range(n):
        lines += [f"MISSING{i} = object()", "", "@dataclass", f"class Spec{i}:"]
        lines += [f"    value: object = MISSING{i}", ""]
        lines += [f"def given{i}(s: Spec{i}):", f"    return s.value is not MISSING{i}", ""]
        lines += [f"def isolated{i}(s: Spec{i}):", "    return copy.deepcopy(s)", ""]
    return "\n".join(lines)


def sentinel_attribute_chain(n: int) -> str:
    """Classes each holding the next in a typed attribute and each deep-copied, the last holding a
    tested `object()` sentinel."""
    lines = ["import copy", "", "MISSING = object()", ""]
    for i in range(n):
        last = i + 1 == n
        params = "self" if last else f'self, following: "C{i + 1}"'
        store = "self.mark = MISSING" if last else "self.next = following"
        lines += [f"class C{i}:", f"    def __init__({params}):", f"        {store}", ""]
        lines += [f"def isolated{i}(c: C{i}):", "    return copy.deepcopy(c)", ""]
    lines += [f"def marked(c: C{n - 1}):", "    return c.mark is MISSING", ""]
    return "\n".join(lines)


def sentinel_attribute_ring(n: int) -> str:
    """Classes in a ring, each holding the next in a typed attribute, the last the first, and each
    deep-copied; one holds a tested `object()` sentinel."""
    lines = ["import copy", "", "MISSING = object()", ""]
    for i in range(n):
        lines += [f"class C{i}:", f'    def __init__(self, following: "C{(i + 1) % n}"):']
        lines += ["        self.next = following"]
        if i == n // 2:
            lines += ["        self.mark = MISSING"]
        lines += [""]
        lines += [f"def isolated{i}(c: C{i}):", "    return copy.deepcopy(c)", ""]
    lines += [f"def marked(c: C{n // 2}):", "    return c.mark is MISSING", ""]
    return "\n".join(lines)


def initialization_steps(n: int) -> str:
    """A class whose initializer calls n methods, each running one public helper that writes,
    reads, and calls on its own `self` n times."""
    lines = ["class Model(Base):", "    def __init__(self):", "        self.items = []"]
    lines += [f"        self.step{i}()" for i in range(n)]
    lines += ["", "    def fill(self):"]
    for _ in range(n):
        lines += ["        self.items.append(self.LIMIT)", "        self.m()"]
    for i in range(n):
        lines += ["", f"    def step{i}(self):", "        self.fill()"]
    return "\n".join(lines) + "\n"


def step_items(n: int) -> str:
    """A `write-owners` contract on the attribute `initialization_steps` fills, which no
    `invalidation` contract names."""
    return '\n[[write-owners]]\nname = "shape-steps"\ntargets = ["Model.items"]\nallowed-writers = ["Base.*"]\n'


# A function returning a set of strings, for the set-order families.
SET_ORDER_HELPERS = """import json
from pathlib import Path


def names(table: dict[str, int]) -> set[str]:
    return set(table)


"""


def order_returned_up(n: int) -> str:
    """Functions each returning what the one below returns, the last a list in a set's order,
    written at the top."""
    lines = ["def f0(table: dict[str, int]) -> list[str]:", "    return [k for k in names(table)]", ""]
    for i in range(1, n):
        lines += [f"def f{i}(table: dict[str, int]) -> list[str]:", f"    return f{i - 1}(table)", ""]
    lines += ["def top(table: dict[str, int], path: Path) -> None:"]
    lines += [f"    path.write_text(json.dumps(f{n - 1}(table)))", ""]
    return SET_ORDER_HELPERS + "\n".join(lines)


def order_handed_down(n: int) -> str:
    """A value in a set's order handed down helpers to the one that writes it."""
    lines = ["def h0(path: Path, data: object) -> None:", "    path.write_text(json.dumps(data))", ""]
    for i in range(1, n):
        lines += [f"def h{i}(path: Path, data: object) -> None:", f"    h{i - 1}(path, data)", ""]
    lines += ["def top(table: dict[str, int], path: Path) -> None:"]
    lines += [f"    h{n - 1}(path, [k for k in names(table)])", ""]
    return SET_ORDER_HELPERS + "\n".join(lines)


def comprehensions_over_one_set(n: int) -> str:
    """Comprehensions over one set, each appended to one list handed to a writer."""
    lines = ["def write(path: Path, parts: list[object]) -> None:", "    path.write_text(json.dumps(parts))", ""]
    lines += ["def top(table: dict[str, int], path: Path) -> None:", "    found = names(table)", "    parts = []"]
    lines += ["    parts.append([k for k in found])"] * n
    lines += ["    write(path, parts)", ""]
    return SET_ORDER_HELPERS + "\n".join(lines)


def set_or_sorted_branches(n: int) -> str:
    """A value chosen among a set and many sorted lists by a chain of branches, then iterated and
    handed to a writer."""
    lines = ["def write(path: Path, data: object) -> None:", "    path.write_text(json.dumps(data))", ""]
    lines += ["def top(table: dict[str, int], path: Path, k: int) -> None:", "    found = names(table)"]
    for i in range(n):
        lines += [f"    if k == {i}:", "        found = sorted(table)"]
    lines += ["    write(path, [x for x in found])", ""]
    return SET_ORDER_HELPERS + "\n".join(lines)


def stores_after_reads(n: int) -> str:
    """A loop over a set storing each element as a key of one mapping, each store after a read of
    another mapping by the same key and of the stored mapping by another key."""
    lines = ["def filled(table: dict[str, int], other: dict[str, int], k: str) -> dict:"]
    lines += ["    fields = {}", "    for name in names(table):"]
    for i in range(n):
        lines += [f"        a{i} = {{}}", f"        x{i} = a{i}[name] if other else fields[k]"]
        lines += ["        fields[name] = other"]
    lines += ["    return fields", "", "def top(table: dict[str, int], other: dict[str, int], path: Path) -> None:"]
    lines += ['    path.write_text(json.dumps(filled(table, other, "k")))', ""]
    return SET_ORDER_HELPERS + "\n".join(lines)


def try_statements_into_a_cycle(n: int) -> str:
    """One function of many `try` statements, each calling into one recursion and taking its
    `RecursionError` in a clause that completes."""
    lines = ["def rec(x):", "    return rec(x)", "", "def shape(x):"]
    for _ in range(n):
        lines += ["    try:", "        rec(x)", "    except Exception:", "        pass"]
    return "\n".join(lines) + "\n"


def cycle_of_functions(n: int) -> str:
    """A call cycle through many functions, each calling the next inside a `try`."""
    lines = []
    for i in range(n):
        lines += [f"def f{i}(x):", "    try:", f"        return f{(i + 1) % n}(x)"]
        lines += ["    except Exception:", "        return None", ""]
    return "\n".join(lines)


def lattice_above_a_cycle(n: int) -> str:
    """A lattice of calls n levels deep, two functions a level, each calling both of the level
    below inside a `try`, above a function calling itself."""
    lines = ["def bottom(x):", "    return bottom(x)", ""]
    for level in range(n):
        below = ["bottom"] if level == 0 else [f"a{level - 1}", f"b{level - 1}"]
        for side in ("a", "b"):
            lines += [f"def {side}{level}(x):", "    try:"]
            lines += [f"        {callee}(x)" for callee in below]
            lines += ["    except Exception:", "        return None", ""]
    return "\n".join(lines)


# The functions `argument_marks` defines, each named by an argument list of `marked_targets`.
MARKED_FUNCTIONS = 1000


def argument_marks(n: int) -> str:
    """Calls passing the constants of argument lists, and as many calls of a function of fifty
    parameters that no list names, a hundred in each function."""
    lines = []
    for i in range(MARKED_FUNCTIONS):
        lines += [f"def mark{i}(p{i}=False, q=None):", f"    return p{i}", ""]
    params = ", ".join(f"b{j}=None" for j in range(50))
    lines += [f"def wide(x, {params}):", "    return x", ""]
    for f in range(n // 100):
        lines.append(f"def call{f}(x):")
        for k in range(100):
            i = (f * 100 + k) % MARKED_FUNCTIONS
            lines += [f"    mark{i}(p{i}=True)", "    wide(x)"]
        lines.append("")
    return "\n".join(lines)


def marked_targets(n: int) -> str:
    """A `call-owners` contract whose n targets each name a parameter of their own."""
    targets = ", ".join(f'"m.mark{i}(p{i}=True)"' for i in range(n))
    return (
        f'\n[[call-owners]]\nname = "shape-marks"\ntargets = [{targets}]\n'
        'forbidden-callers = ["m.call0"]\n'
    )


def blind_calls(n: int) -> Body:
    """Calls heki is blind to, in a loop: library names the catalog lacks, and methods of an
    unannotated parameter."""
    lines = ["for item in items:"]
    for i in range(n):
        lines += [f"    requests.fancy{i}(item)", f"    client.m{i}(item)"]
    return Body(("client", "items"), lines)


def blind_calls_below_async(n: int) -> str:
    """A chain of helpers each calling the next and making blind calls in a loop, below as many
    `async def`s calling its head."""
    lines = []
    for i in range(n):
        lines += [f"def h{i}(client, items):", "    for item in items:"]
        lines += [f"        requests.fancy{i}(item)", f"        client.m{i}(item)"]
        if i + 1 < n:
            lines.append(f"    h{i + 1}(client, items)")
        lines += ["", f"async def entry{i}(client, items):", "    h0(client, items)", ""]
    return "\n".join(lines)


def attributes_of_many_classes(n: int) -> str:
    """Classes each initializing attributes, and as many functions changing and reading them: the
    census contract candidates are drawn from."""
    lines = []
    for i in range(n):
        lines += [f"class C{i}:", "    def __init__(self):", "        self.items = []"]
        lines += ["        self.state = 0", "", f"def put{i}(c: C{i}, x):", "    c.items.append(x)"]
        lines += ["    c.state = x", "", f"def show{i}(c: C{i}):", "    return c.items, c.state", ""]
    return "\n".join(lines)


def container_subclass_chain(n: int) -> str:
    """A chain of subclasses of `list`, each deriving from the one before: the classes a list or
    the chain's first class may be under R2, read by a read-only parameter handed to a helper
    that appends, a cached list changed in place, and a method looked up before a wait."""
    lines = ["import functools", "import time", "from collections.abc import Sequence", ""]
    lines += ["class L0(list):", "    def keep(self):", "        return 0", ""]
    for i in range(1, n):
        lines += [f"class L{i}(L{i - 1}):", "    pass", ""]
    lines += [
        "def _grow(xs):",
        "    if isinstance(xs, list):",
        "        xs.append(1)",
        "",
        "def hand(values: Sequence[int]):",
        "    _grow(values)",
        "",
        "@functools.lru_cache(maxsize=None)",
        "def table(k) -> list[str]:",
        "    return [k]",
        "",
        "def changed(k):",
        "    r = table(k)",
        "    r.append('x')",
        "",
        "def lookup(c: L0):",
        "    try:",
        "        c.keep",
        "        time.sleep(1)",
        "    except ValueError:",
        "        pass",
        "",
        "async def serve(c: L0):",
        "    lookup(c)",
        "",
    ]
    return "\n".join(lines)


@dataclass(frozen=True)
class Family:
    name: str
    body: Callable[[int], Body] | None = None
    project: Callable[[int], str] | None = None
    placements: tuple[str, ...] = PLACED
    # The size of a project family, or of a family in one function whose cost shows at a smaller
    # size than the largest the IR budget models (0: that largest size).
    size: int = 0
    # Adversarial: its budgets may be spent.
    real_code: bool = True
    # Contracts of its own, added to `CONTRACTS` for its runs.
    contracts: Callable[[int], str] | None = None
    # Also run through `heki dump-recommend-facts`: the facts only `heki recommend` reads.
    facts: bool = False
    # Run through `heki dump-open-premises`, which explains the reports' open premises.
    open_premises: bool = False


FAMILIES = [
    Family("call_chain", call_chain),
    Family("returned_merge_chain", returned_merge_chain),
    Family("one_iterator_many_loops", one_iterator_many_loops),
    Family("phi_diamonds", phi_diamonds),
    Family("isinstance_continue", isinstance_continue),
    Family("copy_chain_in_loop", copy_chain_in_loop),
    # A module body's names are globals: no copy of the display reaches a call.
    Family("unpacked_display_copies", unpacked_display_copies, placements=("function", "method")),
    # Every call binds every entry: past the budget, a call passes nothing the display holds.
    Family(
        "wide_display_unpacked",
        wide_display_unpacked,
        placements=("function", "method"),
        real_code=False,
    ),
    Family("reverse_chain_in_loop", reverse_chain_in_loop),
    Family("alias_loops", alias_loops),
    Family("paced_loops", paced_loops),
    Family("doubled_constant_slices", doubled_constant_slices, real_code=False),
    Family("doubled_zips", doubled_zips, real_code=False),
    Family("lists_extending_each_other", lists_extending_each_other),
    Family("item_reads", item_reads),
    Family("merged_flag_loop", merged_flag_loop),
    Family("is_tests", is_tests),
    Family("alias_identity_tests", alias_identity_tests),
    Family("match_pass_through", match_pass_through),
    Family("with_statements", with_statements),
    Family("attribute_chain", attribute_chain),
    Family("element_change_chain", element_change_chain),
    Family("copied_item_changes", copied_item_changes),
    # A module body of thousands of such assignments takes the IR builder past its time limit
    # before any analysis runs.
    Family("copy_chain", copy_chain, placements=("function", "method")),
    Family("copy_rebound_branches", copy_rebound_branches),
    # A module body's names are globals, not closure cells.
    Family("cell_reads", cell_reads, placements=("function", "method"), size=CELL_BINDINGS),
    Family(
        "cell_reads_of_several_names",
        cell_reads_of_several_names,
        placements=("function", "method"),
        size=CELL_BINDINGS,
    ),
    Family("read_only_value_handed_to_calls", read_only_value_handed_to_calls),
    Family("calls_between_invalidators", calls_between_invalidators, placements=("method",)),
    Family("calls_under_branches", calls_under_branches),
    # Every call site is reported, standing for every access: the cost of the reports shows at a
    # size where the walks the family does not gate stay cheap.
    Family(
        "guarded_accesses_in_generator_expressions",
        guarded_accesses_in_generator_expressions,
        size=2000,
    ),
    Family("passed_iterables", passed_iterables),
    Family("path_aliases", path_aliases),
    Family("merged_package_paths", merged_package_paths),
    Family("lazy_init_sites", lazy_init_sites, placements=("method",)),
    # Adversarial: a getter with as many branches as keys asks one walk per key, past the budget.
    Family("self_item_reads", self_item_reads, placements=("method",), real_code=False),
    Family("writes_then_library_calls", writes_then_library_calls, placements=("method",)),
    Family("blind_calls", blind_calls, facts=True),
    Family("wide_hierarchy", project=wide_hierarchy, placements=("project",), size=4000),
    Family("helper_chain", project=helper_chain, placements=("project",), size=2000),
    Family(
        "functions_each_calling", project=functions_each_calling, placements=("project",), size=20000
    ),
    Family("self_helper_chain", project=self_helper_chain, placements=("project",), size=2000),
    Family("order_returned_up", project=order_returned_up, placements=("project",), size=2000),
    Family("order_handed_down", project=order_handed_down, placements=("project",), size=2000),
    Family(
        "comprehensions_over_one_set",
        project=comprehensions_over_one_set,
        placements=("project",),
        size=500,
    ),
    Family(
        "set_or_sorted_branches", project=set_or_sorted_branches, placements=("project",), size=400
    ),
    Family("stores_after_reads", project=stores_after_reads, placements=("project",), size=1000),
    Family("callers_of_one_helper", project=callers_of_one_helper, placements=("project",), size=4000),
    Family(
        "reports_sharing_a_chain",
        project=reports_sharing_a_chain,
        placements=("project",),
        size=4000,
        open_premises=True,
    ),
    Family(
        "initialization_steps",
        project=initialization_steps,
        placements=("project",),
        size=2000,
        contracts=step_items,
    ),
    Family(
        "argument_marks",
        project=argument_marks,
        placements=("project",),
        size=8000,
        contracts=marked_targets,
    ),
    Family(
        "container_subclass_chain", project=container_subclass_chain, placements=("project",), size=1400
    ),
    Family(
        "try_statements_into_a_cycle",
        project=try_statements_into_a_cycle,
        placements=("project",),
        size=2000,
    ),
    Family("cycle_of_functions", project=cycle_of_functions, placements=("project",), size=4000),
    Family("lattice_above_a_cycle", project=lattice_above_a_cycle, placements=("project",), size=30),
    Family(
        "package_paths_merged_in_functions",
        project=package_paths_merged_in_functions,
        placements=("project",),
        size=2000,
    ),
    Family("sentinel_subclasses", project=sentinel_subclasses, placements=("project",), size=2000),
    Family("sentinels_per_class", project=sentinels_per_class, placements=("project",), size=2000),
    Family(
        "sentinel_attribute_chain", project=sentinel_attribute_chain, placements=("project",), size=2000
    ),
    Family("sentinel_attribute_ring", project=sentinel_attribute_ring, placements=("project",), size=2000),
    Family(
        "blind_calls_below_async",
        project=blind_calls_below_async,
        placements=("project",),
        size=2000,
        facts=True,
    ),
    Family(
        "attributes_of_many_classes",
        project=attributes_of_many_classes,
        placements=("project",),
        size=2000,
        facts=True,
    ),
]


def source(family: Family, placement: str, n: int) -> str:
    if family.project is not None:
        return HELPERS + family.project(n)
    assert family.body is not None
    body = family.body(n)
    if placement == "module":
        lines = [f"{p} = load()" for p in body.params]
        lines += [("result = " + line[7:]) if line.startswith("return ") else line for line in body.lines]
        return HELPERS + "\n".join(lines) + "\n"
    if placement == "method":
        head = f"class Shapes(Base):\n    def shape(self, {', '.join(body.params)}):\n"
        indent = " " * 8
    else:
        head = f"def shape({', '.join(body.params)}):\n"
        indent = " " * 4
    return HELPERS + head + "".join(indent + line + "\n" for line in body.lines)


def reference_source(kind: str, n: int) -> str:
    """A reference of `REFERENCES`: n `with` statements calling a function, or n loops over
    aliases of one display. It shares nothing with the gated shapes (helpers, contracts, family
    generators), which change, while the reference seconds hold only for this source."""
    if kind == "calls":
        lines = ["def use(x):", "    return x", "", "", "def shape(p):"]
        for i in range(n):
            lines += [f"    with open(p) as f{i}:", f"        use(f{i})"]
    else:
        lines = ["def shape(xs):", "    a0 = [1, 2, 3]"]
        for i in range(1, n + 1):
            lines += [f"    a{i} = a{i - 1}", f"    for _ in a{i}:", "        xs.append(1)"]
    return "\n".join(lines) + "\n"


def released_heki(root: Path) -> str:
    """The `heki` executable of the release `REFERENCE_HEKI`, from its wheel on PyPI for this
    platform, unpacked under `root`."""
    wheels = root / "wheels"
    pip = [sys.executable, "-m", "pip", "download", "--quiet", "--disable-pip-version-check"]
    pip += ["--no-deps", "--only-binary=:all:", "--dest", str(wheels), f"heki=={REFERENCE_HEKI}"]
    subprocess.run(pip, check=True)
    (wheel,) = wheels.glob("*.whl")
    executable = root / "released-heki"
    with zipfile.ZipFile(wheel) as archive:
        member = next(name for name in archive.namelist() if name.endswith(".data/scripts/heki"))
        executable.write_bytes(archive.read(member))
    executable.chmod(0o755)
    return str(executable)


def peak_mib(stderr: str, usage: os.struct_rusage) -> float:
    """The peak of a run: the heap a counting build reports (the memory gate's build), else the
    physical footprint `/usr/bin/time -l` reports on macOS, else the resident set size (0 for a
    run that was stopped before reporting)."""
    for pattern in (HEAP_LINE, re.compile(r"(\d+)\s+peak memory footprint")):
        found = pattern.search(stderr)
        if found:
            return int(found.group(1)) / MIB
    # `ru_maxrss` is in KiB on Linux; macOS runs report their footprint above.
    return usage.ru_maxrss / 1024 if sys.platform != "darwin" else 0.0


class Runner:
    def __init__(self, heki: str, root: Path) -> None:
        self.heki = heki
        self.root = root
        codes = json.loads(self.run_text(["rule", "--all", "--output-format", "json"]))
        self.select = ",".join(rule["code"] for rule in codes)
        (root / "pyproject.toml").write_text('[project]\nname = "shapes"\n')
        self.contracts("")

    def contracts(self, extra: str) -> None:
        """The contracts of the runs that follow: `CONTRACTS` and `extra`."""
        (self.root / "shapes.toml").write_text(CONTRACTS + extra)

    def run_text(self, args: list[str]) -> str:
        done = subprocess.run([self.heki, *args], cwd=self.root, capture_output=True, text=True)
        if done.returncode not in (0, 1):
            sys.exit(f"heki {' '.join(args)} exited with {done.returncode}:\n{done.stderr}")
        return done.stdout

    def write(self, text: str) -> None:
        (self.root / "m.py").write_text(text)

    def modeled(self, text: str) -> bool | str:
        """Whether the IR budget models every function of `text`, or why building the IR failed
        (a crash, or a run past `TIMEOUT_SECONDS`, which is stopped with its process group)."""
        self.write(text)
        args = [self.heki, "assumptions", "--json", "m.py"]
        proc = subprocess.Popen(
            args,
            cwd=self.root,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            start_new_session=True,
        )
        try:
            stdout, _ = proc.communicate(timeout=TIMEOUT_SECONDS)
        except subprocess.TimeoutExpired:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(proc.pid, signal.SIGKILL)
            proc.communicate()
            return f"ran past {TIMEOUT_SECONDS:.0f} s"
        if proc.returncode not in (0, 1):
            return f"crashed (exit code {proc.returncode})"
        return not any(h["kind"] == "unmodeled" for h in json.loads(stdout))

    def largest_modeled(self, family: Family, placement: str) -> tuple[int, str | None]:
        """The largest n the IR budget models, or the size at which building the IR failed and
        why."""
        sizes = [FIRST_N]
        while sizes[-1] * 2 <= MAX_N:
            sizes.append(sizes[-1] * 2)
        low = 0
        for n in sizes:
            modeled = self.modeled(source(family, placement, n))
            if isinstance(modeled, str):
                return n, modeled
            if not modeled:
                break
            low = n
        else:
            return low, None
        if low == 0:
            sys.exit(f"{family.name}/{placement}: unmodeled at n = {FIRST_N}")
        high = low * 2
        while high - low > max(1, low // 64):
            mid = (low + high) // 2
            modeled = self.modeled(source(family, placement, mid))
            if isinstance(modeled, str):
                return mid, modeled
            if modeled:
                low = mid
            else:
                high = mid
        return low, None

    def check(
        self, text: str, watch: bool, steps: Path | None = None, timeout: float = TIMEOUT_SECONDS
    ) -> tuple[float, float, int]:
        args = ["check", "--select", self.select, "--config", "shapes.toml"]
        args += ["--output-format", "json", "m.py"]
        env = {}
        if watch:
            args.insert(1, "--watch")
            env["HEKI_WATCH_MAX_RUNS"] = "0"
        if steps is not None:
            args[1:1] = ["--walk-steps", str(steps)]
        return self.measure(text, args, env, timeout)

    def run(self, text: str, mode: str, timeout: float = TIMEOUT_SECONDS) -> tuple[float, float, int]:
        """A run of `mode`: `check`, `watch` (the `--watch` engine), `facts` (`heki
        dump-recommend-facts`), or `open-premises` (`heki dump-open-premises`)."""
        dumps = {"facts": "dump-recommend-facts", "open-premises": "dump-open-premises"}
        if mode not in dumps:
            return self.check(text, mode == "watch", timeout=timeout)
        args = [dumps[mode], "--select", self.select, "--config", "shapes.toml", "m.py"]
        return self.measure(text, args, {}, timeout)

    def measure(
        self, text: str, args: list[str], env: dict[str, str], timeout: float
    ) -> tuple[float, float, int]:
        self.write(text)
        darwin = sys.platform == "darwin"
        seconds, code, stderr, usage = execute(self.heki, args, self.root, env, darwin, timeout)
        return seconds, peak_mib(stderr, usage), code

    def steps(self, text: str) -> dict[tuple[str, str], tuple[int, bool]] | None:
        """The steps of each owner and function, `None` when the run failed: only a run that
        ends cleanly (exit code 0 or 1) writes records to trust."""
        path = self.root / "steps.json"
        path.unlink(missing_ok=True)
        _, _, code = self.check(text, watch=False, steps=path)
        if code not in (0, 1) or not path.exists():
            return None
        records = json.loads(path.read_text())
        out: dict[tuple[str, str], tuple[int, bool]] = {}
        for r in records:
            key = (r["owner"], r["function"] or "")
            taken, spent = out.get(key, (0, False))
            out[key] = (taken + r["taken"], spent or r["spent"])
        return out


def shard(text: str) -> tuple[int, int]:
    """`k/n` with 1 <= k <= n."""
    k, _, n = text.partition("/")
    if not (k.isdigit() and n.isdigit() and 1 <= int(k) <= int(n)):
        raise argparse.ArgumentTypeError(f"expected K/N with 1 <= K <= N, got {text!r}")
    return int(k), int(n)


def time_limit(root: Path) -> float | None:
    """`LIMIT_SECONDS` scaled to this machine by `REFERENCES`, run on the release
    `REFERENCE_HEKI` in `root`; `None` when no run of a reference or of its empty module ended
    cleanly (a crash or a stopped run says nothing, or would inflate the limit)."""
    root.mkdir()
    released = Runner(released_heki(root), root)
    args = ["check", "--select", released.select, "--output-format", "json", "m.py"]
    least: dict[str, float] = {}
    for kind, text in [("empty", ""), *((k, reference_source(k, n)) for k, n, _ in REFERENCES)]:
        runs = [released.measure(text, args, {}, TIMEOUT_SECONDS) for _ in range(3)]
        clean = [seconds for seconds, _, code in runs if code in (0, 1)]
        if not clean:
            print(f"calibration failed: no run of the {kind} reference ended cleanly",
                  file=sys.stderr)
            return None
        least[kind] = min(clean)
    factor = 1.0
    for kind, n, seconds in REFERENCES:
        took = least[kind] - least["empty"]
        print(f"reference {kind} n={n} on heki {REFERENCE_HEKI}: {took:.3f} s ({seconds:.3f} s "
              "on the machine of the limits)", flush=True)
        factor = max(factor, took / seconds)
    return LIMIT_SECONDS * factor


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--heki", required=True, help="path to the heki binary (release build)")
    parser.add_argument("--only", action="append", default=[], help="run only these families")
    parser.add_argument("--report", action="store_true", help="print the costs, never fail")
    parser.add_argument("--shard", type=shard, default=(1, 1), metavar="K/N",
                        help="run every N-th shape, starting at the K-th")
    args = parser.parse_args()
    heki = str(Path(args.heki).resolve())
    families = [f for f in FAMILIES if not args.only or f.name in args.only]
    first, step = args.shard
    shapes = [(f, placement) for f in families for placement in f.placements][first - 1 :: step]
    problems: list[str] = []
    with tempfile.TemporaryDirectory(prefix="heki-walk-shapes-") as tmp:
        runner = Runner(heki, Path(tmp))
        # Only runs that end cleanly measure anything: a crash or a stopped run would make the
        # baseline or a reference say nothing, or inflate the limit.
        empty = [runner.check(HELPERS, watch=False) for _ in range(3)]
        empty = [run for run in empty if run[2] in (0, 1)]
        if not empty:
            print("calibration failed: no run of the empty module ended cleanly", file=sys.stderr)
            return 1
        base_seconds = min(run[0] for run in empty)
        base_peak = min(run[1] for run in empty)
        print(f"empty module: {base_seconds:.2f} s, {base_peak:.0f} MiB", flush=True)
        limit_seconds = time_limit(Path(tmp) / "reference")
        if limit_seconds is None:
            return 1
        print(f"time limit on this machine: {limit_seconds:.2f} s", flush=True)
        for family, placement in shapes:
            shape = (family.name, placement)
            runner.contracts("" if family.contracts is None else family.contracts(family.size))
            if family.project is not None:
                n, failed = family.size, None
            elif family.size:
                n = family.size
                modeled = runner.modeled(source(family, placement, n))
                failed = None if modeled is True else modeled or "leaves the function unmodeled"
            else:
                n, failed = runner.largest_modeled(family, placement)
            text = source(family, placement, n)
            listed = shape in KNOWN_OVER
            over = []
            # Whether a listed shape costs clearly less than the limits.
            well_under = failed is None
            if failed is not None:
                print(f"{family.name}/{placement} n={n}: building the IR {failed}", flush=True)
                over.append("build")
            if family.open_premises:
                modes: tuple[str, ...] = ("open-premises",)
            else:
                modes = ("check",) if listed else ("check", "watch")
                modes += ("facts",) * family.facts
            for mode in () if failed is not None else modes:
                # A listed shape only has to show when it drops under the limits.
                timeout = base_seconds + 2 * limit_seconds if listed else TIMEOUT_SECONDS
                seconds, peak, code = runner.run(text, mode, timeout=timeout)
                cost = (seconds - base_seconds, peak - base_peak)
                if not listed and (cost[0] > limit_seconds or cost[1] > LIMIT_MIB or code == -9):
                    # Noise only adds time: the least of three runs, as the empty module and the
                    # references are timed, so that both sides of the comparison are measured
                    # alike.
                    runs = [(seconds, peak, code), runner.run(text, mode), runner.run(text, mode)]
                    seconds, peak, code = min(runs)
                    cost = (seconds - base_seconds, peak - base_peak)
                if code == -9:
                    spent = f"stopped after {cost[0]:.0f} s"
                elif code not in (0, 1):
                    spent = f"crashed (exit code {code})"
                else:
                    spent = f"{cost[0]:.2f} s, {cost[1]:.0f} MiB"
                print(f"{family.name}/{placement} n={n} {mode}: {spent}", flush=True)
                if cost[0] > limit_seconds or cost[1] > LIMIT_MIB or code not in (0, 1):
                    over.append(mode)
                if cost[0] > limit_seconds / 4 or cost[1] > LIMIT_MIB / 4 or code not in (0, 1):
                    well_under = False
            if over and not listed:
                problems.append(f"{family.name}/{placement} at n={n} costs more than "
                                f"{limit_seconds:.1f} s or {LIMIT_MIB:.0f} MiB ({', '.join(over)})")
            if listed and well_under:
                problems.append(f"{family.name}/{placement} costs less than a quarter of the limits: "
                                "remove it from KNOWN_OVER")
            if family.project is None and not over:
                steps = step_growth(runner, family, placement, n, text)
                if shape not in KNOWN_STEPS:
                    problems += steps
                elif not steps:
                    problems.append(f"{family.name}/{placement} takes linear steps and spends no "
                                    "budget: remove it from KNOWN_STEPS")
    for problem in problems:
        print(problem, file=sys.stderr)
    return 1 if problems and not args.report else 0


def step_growth(runner: Runner, family: Family, placement: str, n: int, text: str) -> list[str]:
    """The owners whose steps grow more than `MAX_STEP_GROWTH` times from n / 2 to n, and the
    budgets spent on a family shaped like real code."""
    full = runner.steps(text)
    half = runner.steps(source(family, placement, n // 2))
    if full is None or half is None:
        return [f"{family.name}/{placement}: the steps could not be counted (a run failed)"]
    out = []
    for key, (taken, spent) in sorted(full.items()):
        before = half.get(key, (0, False))[0]
        if before and taken / before > MAX_STEP_GROWTH:
            out.append(f"{family.name}/{placement}: {key[0]} {key[1]} takes {before} steps at "
                       f"n={n // 2} and {taken} at n={n}")
        if spent and family.real_code:
            out.append(f"{family.name}/{placement}: {key[0]} spends its budget on {key[1]}")
    return out


if __name__ == "__main__":
    sys.exit(main())
