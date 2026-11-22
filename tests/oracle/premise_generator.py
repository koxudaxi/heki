"""Generates programs from the premise table (`crates/heki_rules/src/premises.rs`).

For an exact rule with premises P1..Pk (`heki rule --all --output-format json` prints them), a
program keeps every premise established, or all but one: the stripped premise, made false by
construction on an input the program runs. Every other statement of the program is the base
shape, so a report on it is about that one premise:

- with no premise stripped, heki is expected to report the claim exactly (a base program it does
  not report is counted, not a refutation);
- with premise P stripped, an exact report of the rule is a false claim whatever the run shows
  (certification step 1, premise stripping); the runs also check it as usual.

`generate(seed, table)` picks the rule, the premise, and the way to strip it from the seed.
`STRIPS` names, for each rule, the ways this module strips each premise; a premise of the table
that `STRIPS` does not know makes `generate` raise, so a new premise needs its shapes here.
"""

from __future__ import annotations

import random

from generator import CONTROLS, HEKI_TOML, Project, Scenario, _common_import, _module_text, block


def _base_scenario(rng):
    sc = Scenario(rng, 0)
    sc.imports("import contextlib")
    sc.dispatch = None
    return sc


def _raising_guard(sc):
    """A load that raises on some input, inside a `try` whose handler returns."""
    ns = sc.ns
    load, raised = sc.pick(
        [
            ("cfg.value", "ValueError"),
            (f"TABLE_{ns}[key]", "KeyError"),
            ("int(text)", "ValueError"),
            (f"check_{ns}(n)", "ValueError"),
            ("10 // n", "ZeroDivisionError"),
            ("cfg.ping", "ValueError"),
        ]
    )
    return block("try:", [f"v = {load}"]) + block(f"except {raised}:", ["return None"])


# ---------------------------------------------------------------------------------------------
# EFF101 and EFF103: helpers down to a library call
# ---------------------------------------------------------------------------------------------
def _every_return_strip(sc, call_lines):
    """The call on some paths only: an early return, a handler that returns, a suppressing
    `with`, a loop that may run no time, or a branch."""
    way = sc.pick(["early", "handler", "suppressed", "loop", "branch"])
    if way == "early":
        return block("if flag:", ["return None"]) + call_lines
    if way == "handler":
        return _raising_guard(sc) + call_lines
    if way == "suppressed":
        return block(f"with Quiet_{sc.ns}():", ["v = int(text)", *call_lines])
    if way == "loop":
        return block("for _ in range(n):", call_lines)
    return block("if flag:", call_lines)


def _chain(sc, leaf, strip, params, args, prefix=""):
    """Helpers one or two deep down to `leaf` (statements), one of them stripped of "Every
    return" or "Every implementation"; returns the call of the first, from the entry module."""
    depth = sc.rng.randint(1, 2)
    # The entry passes the receiver of a dispatched hop: it is the first hop.
    stripped_level = 0 if strip == "Every implementation" else sc.rng.randrange(depth)
    call = None
    for level in reversed(range(depth)):
        name = sc.name(prefix + "step")
        inner = list(leaf) if call is None else [f"v = {call}"]
        if strip == "Every return" and level == stripped_level:
            inner = _every_return_strip(sc, inner)
        if strip == "Every implementation" and level == stripped_level:
            name, call = _dispatched_hop(sc, name, inner, params, args)
            continue
        sc.define(block(f"def {name}({params}):", ["v = None", *inner, "return v"]))
        call = f"{name}({args})"
    return call


def _dispatched_hop(sc, name, inner, params, args):
    """A hop whose implementations do not all make the next call: a subclass, a union member,
    a protocol's implementer, or a mixin's subclass without it, a cache that skips the body on a
    hit, or a decorator that skips it."""
    way = sc.pick(["subclass", "union", "protocol", "mixin", "cached", "decorated"])
    body = ["v = None", *inner, "return v"]
    quiet = ["return None"]
    if way == "cached":
        sc.imports("import functools")
        sc.define(["@functools.lru_cache(maxsize=None)"] + block(f"def {name}({params}):", body))
        return name, f"{name}({args})"
    if way == "decorated":
        deco = sc.name("skipping")
        sc.define(
            block(
                f"def {deco}(fn):",
                block(
                    "def inner(*a, **k):", block("if a and a[0]:", ["return None"]) + ["return fn(*a, **k)"]
                )
                + ["return inner"],
            )
        )
        sc.define([f"@{deco}"] + block(f"def {name}({params}):", body))
        return name, f"{name}({args})"
    klass, other = sc.name("Impl"), sc.name("Other")
    if way == "subclass":
        sc.define(block(f"class {klass}:", block(f"def run(self, {params}):", body)))
        sc.define(block(f"class {other}({klass}):", block(f"def run(self, {params}):", quiet)))
        receiver = klass
    elif way == "union":
        sc.define(block(f"class {klass}:", block(f"def run(self, {params}):", body)))
        sc.define(block(f"class {other}:", block(f"def run(self, {params}):", quiet)))
        receiver = f"{klass} | {other}"
    elif way == "protocol":
        proto = sc.name("Runner")
        sc.imports("from typing import Protocol")
        sc.define(block(f"class {proto}(Protocol):", block(f"def run(self, {params}):", ["..."])))
        sc.define(block(f"class {klass}:", block(f"def run(self, {params}):", body)))
        sc.define(block(f"class {other}:", block(f"def run(self, {params}):", quiet)))
        receiver = proto
    else:
        mixin = sc.name("Mixin")
        sc.define(block(f"class {mixin}:", block(f"def run(self, {params}):", [f"return self.leaf({args})"])))
        sc.define(block(f"class {klass}({mixin}):", block(f"def leaf(self, {params}):", body)))
        sc.define(block(f"class {other}({mixin}):", block(f"def leaf(self, {params}):", quiet)))
        receiver = mixin
    sc.dispatch = (receiver, [klass, other])
    return name, f"obj.run({args})"


def _waiting_leaf(sc, strip):
    ns = sc.ns
    if strip != "Waits":
        lines, imports = sc.pick(
            [
                (["v = time.sleep(0.01)"], ["import time"]),
                ([f"v = requests.get(URL_{ns})"], ["import requests"]),
                (['v = subprocess.run(["true"], check=False)'], ["import subprocess"]),
                ([f'v = sqlite3.connect(DB_FILE_{ns}).execute("SELECT 1")'], ["import sqlite3"]),
                ([f"v = urllib.request.urlopen(URL_{ns})"], ["import urllib.request"]),
                ([f"v = socket.getaddrinfo(HOST_{ns}, 80)"], ["import socket"]),
            ]
        )
        for imp in imports:
            sc.imports(imp)
        return lines
    const = sc.name("NONE_WAIT")
    lines, imports, consts = sc.pick(
        [
            (["v = time.sleep(0)"], ["import time"], []),
            ([f"v = time.sleep({const})"], ["import time"], [f"{const} = 0"]),
            # The module's constants as its IR computes them: from other constants, and after a
            # star import that cannot rebind them.
            (
                [f"v = time.sleep({const})"],
                ["import time"],
                [f"{const}_BASE = 3", f"{const} = {const}_BASE - 3"],
            ),
            ([f"v = time.sleep({const})"], ["import time", f"from {sc.common} import *"], [f"{const} = 0"]),
            ([f"v = urllib.request.urlopen({const})"], ["import urllib.request"], [f'{const} = "data:,x"']),
            ([f"v = socket.getaddrinfo({const}, 80)"], ["import socket"], [f'{const} = "127.0.0.1"']),
            ([f"v = socket.gethostbyname({const})"], ["import socket"], [f'{const} = "localhost"']),
            (['v = sqlite3.connect(":memory:").execute("SELECT 1")'], ["import sqlite3"], []),
            ([f"Q_{ns}.put(1)", f"v = Q_{ns}.get()"], ["import queue"], [f"Q_{ns} = queue.Queue()"]),
        ]
    )
    for imp in imports:
        sc.imports(imp)
    if consts:
        sc.define(consts)
    return lines


def eff101(sc, strip):
    params = sc.params
    sc.dispatch = None
    leaf = _waiting_leaf(sc, strip)
    call = _chain(sc, leaf, strip, params, CONTROLS)
    entry = sc.name("handle")
    signature = params
    if sc.dispatch:
        signature = f"obj: {sc.dispatch[0]}, {params}"
    sc.define(block(f"async def {entry}({signature}):", [f"return {call}"]))
    _add_inputs(sc, entry, lambda k: [])


def _add_inputs(sc, entry, extra):
    inputs = []
    for k, controls in enumerate(sc.control_inputs(8)):
        args = [*extra(k), *controls]
        if getattr(sc, "dispatch", None):
            for klass in sc.dispatch[1]:
                inputs.append({"args": [{"$obj": [f"{sc.main_module}:{klass}", [], {}]}, *args]})
        else:
            inputs.append({"args": args})
    sc.entries.append({"module": sc.main_module, "func": entry, "inputs": inputs})


ITEMS = [["1", "2", "3"], ["1", "skip", "2"], ["x", "1"], [], ["4"], ["5", "6"], ["0", "1"], ["7"]]


def eff103(sc, strip):
    params = sc.params
    sc.dispatch = None
    sc.imports("import sqlite3")
    statement = "v = db.fetchone()" if strip == "Sends" else f"v = db.execute(SQL_{sc.ns}, (x, n))"
    call = _chain(
        sc, [statement], strip, f"db: sqlite3.Cursor, x, {params}", f"db, x, {CONTROLS}", prefix="q"
    )
    body = [f"v = {call}"]
    if strip == "Every iteration":
        way = sc.pick(["skip", "handler", "branch"])
        if way == "skip":
            body = block('if x == "skip":', ["continue"]) + body
        elif way == "handler":
            body = block("try:", ["v = int(x)"]) + block("except ValueError:", ["continue"]) + body
        else:
            body = block("if flag:", body)
    origin = (
        sc.pick(["range(3)", '["1", "2"]'])
        if strip == "Origin"
        else sc.pick(["items", "sorted(items)", "list(items)"])
    )
    entry = sc.name("process")
    signature = f"db: sqlite3.Cursor, items: list[str], {params}"
    if sc.dispatch:
        signature = f"db: sqlite3.Cursor, items: list[str], obj: {sc.dispatch[0]}, {params}"
    sc.define(
        block(
            f"def {entry}({signature}):",
            ["v = None", "out = []"]
            + block(f"for x in {origin}:", body + ["out.append(x)"])
            + ["return out"],
        )
    )
    inputs = []
    for k, controls in enumerate(sc.control_inputs(8)):
        base = [{"$cursor": "data.db"}, list(ITEMS[k % len(ITEMS)])]
        if sc.dispatch:
            for klass in sc.dispatch[1]:
                inputs.append(
                    {"args": [*base, {"$obj": [f"{sc.main_module}:{klass}", [], {}]}, *controls], "rows": 1}
                )
        else:
            inputs.append({"args": [*base, *controls], "rows": 1})
    sc.entries.append({"module": sc.main_module, "func": entry, "inputs": inputs})


# ---------------------------------------------------------------------------------------------
# DATA101
# ---------------------------------------------------------------------------------------------
def data101(sc, strip):
    sc.imports("import numpy as np")
    update = "acc = np.append(acc, c)"
    if strip == "Reallocates":
        grow = sc.name("grow")
        sc.define(
            block(
                f"def {grow}(arr, c, flag):", block("if flag:", ["return arr"]) + ["return np.append(arr, c)"]
            )
        )
        update = f"acc = {grow}(acc, c, flag)"
    body = [update]
    if strip == "Loop-carried on every path":
        way = sc.pick(["reset", "other"])
        if way == "reset":
            body = body + block('if c == "0":', ["acc = np.array([])"])
        else:
            body = ["acc = np.append(other, c)"]
    if strip == "Every iteration":
        way = sc.pick(["skip", "branch"])
        body = block('if c == "skip":', ["continue"]) + body if way == "skip" else block("if flag:", body)
    origin = sc.pick(["range(3)", '["1", "2"]']) if strip == "Origin" else "items"
    entry = sc.name("build")
    sc.define(
        block(
            f"def {entry}(items: list[str], {sc.params}):",
            ["acc = np.array([])", "other = np.array([])"]
            + block(f"for c in {origin}:", body)
            + ["return acc"],
        )
    )
    _add_inputs(sc, entry, lambda k: [list(ITEMS[k % len(ITEMS)])])


# ---------------------------------------------------------------------------------------------
# MUT110 and MUT114
# ---------------------------------------------------------------------------------------------
def mut110(sc, strip):
    sc.imports("from collections.abc import Sequence")
    helper = sc.name("mut")
    change = block("if isinstance(xs, list):", ["xs.append(n)"])
    sc.define(block(f"def {helper}(xs, {sc.params}):", change))
    annotation = "list[int]" if strip == "Read-only parameter" else "Sequence[int]"
    passed = "values"
    if strip == "Passed as itself":
        passed = sc.pick(["list(values)", "values if not flag else list(values)"])
    entry = sc.name("entry")
    sc.define(
        block(
            f"def {entry}(values: {annotation}, {sc.params}):",
            [f"{helper}({passed}, {CONTROLS})", "return len(values)"],
        )
    )
    if strip == "Changed at the line":
        sc.frozen = True

    def value(k):
        if strip == "Changed at the line" and k % 2:
            return [{"$obj": [f"{sc.common}:Frozen_{sc.ns}", [[3, 1, 2]], {}]}]
        return [[3, 1, 2]]

    _add_inputs(sc, entry, value)


def mut114(sc, strip):
    sc.imports("import functools")
    cached = sc.name("table")
    deco = "@functools.lru_cache(maxsize=0)" if strip == "Cached" else "@functools.lru_cache(maxsize=None)"
    params = "" if strip == "Keyed" else "name"
    build = '[name, "id"]'
    if strip == "Container kind":
        sc.frozen = True
        build = f"Frozen_{sc.ns}({build})"
    body = ['name = "fixed"', f"return {build}"] if strip == "Keyed" else [f"return {build}"]
    sc.define([deco] + block(f"def {cached}({params}) -> list[str]:", body))
    call = f"{cached}({'' if strip == 'Keyed' else 'name'})"
    lines = [f"result = {call}"]
    if strip == "Same object":
        lines = [sc.pick([f"result = list({call})", f"result = {call} if not flag else []"])]
    change = 'result = result + ["x"]' if strip == "Changed in place" else 'result.append("x")'
    entry = sc.name("use")
    sc.define(block(f"def {entry}(name, {sc.params}):", lines + [change, "return result"]))
    _add_inputs(sc, entry, lambda k: [["a", "b"][k % 2]])


def mut115(sc, strip):
    """A copy of a parameter's dict and a change of the list one of its items holds."""
    ns = sc.ns
    main = sc.main_module
    lines = ["result = options.copy()"]
    if strip == "Borrowed original":
        lines.insert(0, 'options = {"bases": [text]}')
    if strip == "Still shared":
        lines += block("if flag:", ['result["bases"] = []'])
    lines += ['result["bases"].append(text)', 'return len(options["bases"])']
    entry = sc.name("copied")
    sc.define(block(f"def {entry}(options: dict[str, list[str]], {sc.params}):", lines))

    def bases(k):
        if strip == "Changed below the copy" and k % 2:
            return {"$obj": [f"{main}:Silent_{ns}", [["a"]], {}]}
        return ["a", "b"][: 1 + k % 2]

    def value(k):
        options = {"$dict": [["bases", bases(k)]]}
        if strip == "Shallow copy" and k % 2:
            return [{"$obj": [f"{main}:Deep_{ns}", [options], {}]}]
        return [options]

    if strip == "Shallow copy":
        sc.define(
            block(f"class Deep_{ns}(dict):", block("def copy(self):", ["return {k: list(v) for k, v in self.items()}"]))
        )
    if strip == "Changed below the copy":
        sc.define(block(f"class Silent_{ns}(list):", block("def append(self, item):", ["return None"])))
    _add_inputs(sc, entry, value)


# ---------------------------------------------------------------------------------------------
# The proven OPT tier: one pattern per rule, and the contexts that keep or break each premise
# ---------------------------------------------------------------------------------------------
# rule: (construction, statement in the loop, the loop, what the function returns, a change in
# the loop that makes the rewrite change what the program does)
OPT_PATTERNS = {
    "OPT101": ("x = []", "x = x + [i]", "for i in items:", "len(x)", None),
    "OPT102": ("x = {}", "x = {**x, repr(i): n}", "for i in items:", "len(x)", None),
    "OPT103": ("x = ()", "x = x + (i,)", "for i in items:", "len(x)", None),
    "OPT110": (
        "x = list(range(n * 7))",
        "hits += 1 if len(str(i)) in x else 0",
        "for i in items:",
        "hits",
        "x.append(len(str(i)) + 1)",
    ),
    "OPT111": (
        "x = []",
        "k = len(str(i))\nif k not in x:\n    x.append(k)",
        "for i in items:",
        "len(x)",
        "x.clear()",
    ),
    "OPT120": ("x = list(items)", "hits += x.pop(0)", "while x:", "hits", None),
    "OPT121": ("x = []", "x.insert(0, i)", "for i in items:", "len(x)", None),
    "OPT130": (
        "x = list(range(n * 7))",
        "hits += 1 if len(str(i)) in set(x) else 0",
        "for i in items:",
        "hits",
        "x.append(len(str(i)) + 1)",
    ),
}
# The rewrite of the accumulations changes the list in place, so an alias made before the loop
# sees it change; a deque's text differs from the list's.
OPT_UNSAFE_RESULT = {
    "OPT101": ["y = x", "(x, y)"],
    "OPT102": ["y = x", "(x, y)"],
    "OPT103": ["y = x", "(x, y)"],
    "OPT120": [None, 'f"{hits} {x}"'],
    "OPT121": [None, "(len(x), str(x))"],
}


def opt(sc, strip, rule):
    construction, statement, loop, result, unsafe_change = OPT_PATTERNS[rule]
    stmt = statement.split("\n")
    init = "x = seed" if strip == "Builtin type by construction" else construction
    if strip == "Builtin type by construction" and rule == "OPT130":
        # The constructor's call is the value: a project function named like the builtin builds
        # nothing (what a builtin constructor builds the container from is an obligation of the
        # rewrite, not this premise).
        init = construction
        sc.define(block("def set(values):", ["return values"]))
    before = []
    body = list(stmt)
    if strip == "Loop-carried":
        # The result no longer feeds the next iteration: it goes to another name.
        body = [statement.replace("x = ", "y = ", 1)]
        result = "(len(x), len(y) if items else 0)"
    if strip == "Rewrite proven safe":
        if unsafe_change is not None:
            body = body + block("if i == 2:", [unsafe_change])
        else:
            alias, result = OPT_UNSAFE_RESULT[rule]
            before = [alias] if alias else []
    lines = [init, "hits = 0", *before]
    if strip == "In a loop":
        lines += [line.replace("i)", "n)").replace("(i", "(n").replace(" i]", " n]") for line in stmt]
    else:
        lines += block(loop, body + (["i = len(x)"] if loop == "while x:" else []))
    entry = sc.name("opt")
    sc.define(block(f"def {entry}(items, other, seed, {sc.params}):", lines + [f"return {result}"]))
    _add_inputs(
        sc,
        entry,
        lambda k: [[1, 2, 2, 3][: k % 4 + 1], [1, 2, 5], [7] if rule != "OPT102" else {"$dict": [["a", 1]]}],
    )


def _opt_builder(rule):
    def build(sc, strip):
        opt(sc, strip, rule)

    return build


# ---------------------------------------------------------------------------------------------
# OPT113
# ---------------------------------------------------------------------------------------------
def opt113(sc, strip):
    sc.imports("from dataclasses import dataclass")
    left, right = sc.name("Left"), sc.name("Right")
    sc.define(["@dataclass"] + block(f"class {left}:", ["key: int", "other: int = 0"]))
    make = right
    if strip == "Equality of keys":
        way = sc.pick(["property", "mixin", "getattr"])
        if way == "property":
            sc.define(
                ["@dataclass"]
                + block(
                    f"class {right}:",
                    ["raw: int", "payload: int = 0", ""]
                    + ["@property"]
                    + block("def key(self):", ["return self.raw"]),
                )
            )
        elif way == "getattr":
            sc.define(
                ["@dataclass"]
                + block(
                    f"class {right}:",
                    ["raw: int", "payload: int = 0", ""]
                    + block("def __getattr__(self, name):", ["return self.raw"]),
                )
            )
        else:
            mixin, mixed = sc.name("KeyMixin"), sc.name("Mixed")
            sc.define(["@dataclass"] + block(f"class {right}:", ["key: int", "payload: int = 0"]))
            # A setter, so that the dataclass initializer of the class it is mixed into can store
            # the key; reading it still runs the getter.
            sc.define(
                block(
                    f"class {mixin}:",
                    ["@property"]
                    + block("def key(self):", ["return self._key + 0"])
                    + ["", "@key.setter"]
                    + block("def key(self, value):", ["self._key = value"]),
                )
            )
            sc.define(block(f"class {mixed}({mixin}, {right}):", ["pass"]))
            make = mixed
    else:
        sc.define(["@dataclass"] + block(f"class {right}:", ["key: int", "payload: int = 0"]))
    inner_iter = "rights"
    if strip == "Steps through inputs":
        load = sc.name("load")
        sc.define(block(f"def {load}(rows):", ["return list(rows)"]))
        inner_iter = f"{load}(rights)"
    test = "a.key == b.key"
    inner = block(f"if {test}:", ["out.append((a.other, b.payload))"])
    outer_pre = []
    if strip == "Idle on a mismatch":
        way = sc.pick(["before", "else", "and"])
        if way == "before":
            inner = ["seen += 1"] + inner
        elif way == "else":
            inner = inner + block("else:", ["seen += 1"])
        else:
            inner = block(f"if flag and {test}:", ["out.append((a.other, b.payload))"])
    if strip == "Same collection, same keys":
        way = sc.pick(["grow", "rekey"])
        if way == "grow":
            outer_pre = block("if flag and rights:", ["rights.append(rights[0])"])
        else:
            inner = block(f"if {test}:", ["out.append((a.other, b.payload))", "b.key = -1"])
    body = (
        ["out = []", "seen = 0"]
        + block("for a in lefts:", outer_pre + block(f"for b in {inner_iter}:", inner))
        + ["return (len(out), seen)"]
    )
    entry = sc.name("join")
    sc.define(block(f"def {entry}(lefts: list[{left}], rights: list[{right}], {sc.params}):", body))
    shapes = [([1, 2, 3], [2, 3, 4]), ([5], [1, 2]), ([1, 1], [1, 2, 1]), ([3, 4], [4, 9])]

    def lists(k):
        lk, rk = shapes[k % len(shapes)]
        return [
            [{"$obj": [f"{sc.main_module}:{left}", [x], {}]} for x in lk],
            [{"$obj": [f"{sc.main_module}:{make}", [x, x * 10], {}]} for x in rk],
        ]

    _add_inputs(sc, entry, lists)


BUILDERS = {
    "EFF101": eff101,
    "EFF103": eff103,
    "DATA101": data101,
    "MUT110": mut110,
    "MUT114": mut114,
    "MUT115": mut115,
    **{rule: _opt_builder(rule) for rule in OPT_PATTERNS},
    "OPT113": opt113,
}

# The premises each builder strips. A table premise of a rule here that the builder does not
# strip is an error of `generate`: the generator has to grow with the table.
#
# Stripping makes the premise false by construction on an input the program runs, so an exact
# report of the rule is a false claim, except for the premises in `RELEVANCE`: those decide
# whether the rule reports at all (a cache without a key still shares its one result; a join
# over a call's result is still a join), so an exact report with one stripped is a departure
# from the rule page, counted apart.
STRIPS = {
    "EFF101": ["Every return", "Every implementation", "Waits"],
    "EFF103": ["Origin", "Every iteration", "Every return", "Every implementation", "Sends"],
    "DATA101": ["Loop-carried on every path", "Every iteration", "Origin", "Reallocates"],
    "MUT110": ["Read-only parameter", "Passed as itself", "Changed at the line"],
    "MUT114": ["Cached", "Keyed", "Container kind", "Same object", "Changed in place"],
    "MUT115": ["Shallow copy", "Borrowed original", "Still shared", "Changed below the copy"],
    **{
        rule: ["Builtin type by construction", "Loop-carried", "In a loop", "Rewrite proven safe"]
        for rule in ("OPT101", "OPT102", "OPT103")
    },
    **{
        rule: ["Builtin type by construction", "In a loop", "Rewrite proven safe"]
        for rule in ("OPT110", "OPT111", "OPT120", "OPT121", "OPT130")
    },
    "OPT113": [
        "Equality of keys",
        "Steps through inputs",
        "Idle on a mismatch",
        "Same collection, same keys",
    ],
}


RELEVANCE = {("MUT114", "Keyed"), ("OPT113", "Steps through inputs")}

# The exact rules whose heuristic reports record the premises they leave open
# (`heki dump-open-premises`), each with the code those reports carry: its heuristic side
# (`heki_rules::premises::exact_side_of`), or its own code when its heuristic tier keeps it. With a
# premise stripped, such a report has to name it among them.
OPEN_PREMISE_CODES = {
    "EFF101": "EFF111",
    "EFF103": "EFF113",
    **{rule: rule for rule in OPT_PATTERNS},
    "OPT113": "OPT113",
}


class TableError(Exception):
    pass


def check_table(table):
    """The rules of `table` (rule code to premise names) the builders cover, with every premise
    stripped; raises when the table has a premise a builder does not strip."""
    missing = []
    for code, builder_premises in STRIPS.items():
        names = table.get(code)
        if names is None:
            continue
        unknown = [p for p in names if p not in builder_premises]
        if unknown:
            missing.append(f"{code}: {unknown}")
    if missing:
        raise TableError("the premise table has premises the generator cannot strip: " + "; ".join(missing))
    covered = [code for code in STRIPS if code in table]
    if not covered:
        raise TableError("the heki prints no premise table (`heki rule --output-format json`)")
    return covered


def generate(seed, table):
    """A project for one rule of `table`: every premise established, or one stripped."""
    rules = check_table(table)
    rng = random.Random(seed)
    code = rng.choice(rules)
    premises = table[code]
    strip = rng.choice([None, *premises])
    sc = _base_scenario(rng)
    BUILDERS[code](sc, strip)
    # After the builder: it may have asked for the container subclasses.
    sc.imports(_common_import(sc))
    files = {
        "app/__init__.py": "",
        "app/c0.py": sc.common_source(),
        "app/s0.py": _module_text(sc.main_imports, sc.main_defs),
        "data_s0.txt": "1\n2\nskip\n",
    }
    files["heki.toml"] = HEKI_TOML.format(rules=f'"{code}"')
    project = Project(seed, files, sc.entries, [f"table:{code}"])
    project.premise = {"rule": code, "stripped": strip, "premises": premises}
    return project
