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
import re

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
# EFF101, EFF105, EFF102, and EFF103: helpers down to a library call
# ---------------------------------------------------------------------------------------------
EVERY_RETURN_WAYS = ["early", "handler", "suppressed", "loop", "branch"]


def _every_return_strip(sc, call_lines, ways=EVERY_RETURN_WAYS):
    """The call on some paths only: an early return, a handler that returns, a suppressing
    `with`, a loop that may run no time, a branch, or (where `ways` has it) the body of a class
    defined in a branch."""
    way = sc.pick(ways)
    if way == "class-body":
        return block("if flag:", block(f"class {sc.name('Defined')}:", call_lines))
    if way == "early":
        return block("if flag:", ["return None"]) + call_lines
    if way == "handler":
        return _raising_guard(sc) + call_lines
    if way == "suppressed":
        return block(f"with Quiet_{sc.ns}():", ["v = int(text)", *call_lines])
    if way == "loop":
        return block("for _ in range(n):", call_lines)
    return block("if flag:", call_lines)


def _chain(sc, leaf, strip, params, args, prefix="", every_return_ways=EVERY_RETURN_WAYS):
    """Helpers one or two deep down to `leaf` (statements), one of them stripped of "Every
    return" (in one of `every_return_ways`) or "Every implementation"; returns the call of the
    first, from the entry module."""
    depth = sc.rng.randint(1, 2)
    # The entry passes the receiver of a dispatched hop: it is the first hop.
    stripped_level = 0 if strip == "Every implementation" else sc.rng.randrange(depth)
    call = None
    for level in reversed(range(depth)):
        name = sc.name(prefix + "step")
        inner = list(leaf) if call is None else [f"v = {call}"]
        if strip == "Every return" and level == stripped_level:
            inner = _every_return_strip(sc, inner, every_return_ways)
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


def _file_system_leaf(sc, strip):
    sc.imports("import os")
    if strip == "Touches the file system":
        # No I/O on POSIX: the catalog marks it as a call that can work without the file system.
        return ['v = os.path.isjunction("data_s0.txt")']
    lines, imports = sc.pick(
        [
            (['v = os.path.isfile("data_s0.txt")'], []),
            (['v = os.path.getsize("data_s0.txt")'], []),
            (['v = pathlib.Path("data_s0.txt").read_text()'], ["import pathlib"]),
        ]
    )
    for imp in imports:
        sc.imports(imp)
    return lines


def eff105(sc, strip):
    params = sc.params
    sc.dispatch = None
    leaf = _file_system_leaf(sc, strip)
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


def eff102(sc, strip):
    params = sc.params
    sc.dispatch = None
    if strip == "Allocates":
        # The caller passes the URL, and every input passes a `data:` URL, which opens nothing.
        sc.imports("import urllib.request")
        statement = "v = urllib.request.urlopen(u)"
    else:
        sc.imports("import requests")
        statement = sc.pick([f"v = requests.get(URL_{sc.ns})", "v = requests.Session()"])
    call = _chain(sc, [statement], strip, f"u, x, {params}", f"u, x, {CONTROLS}", prefix="c")
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
    entry = sc.name("fetch")
    signature = f"u: str, items: list[str], {params}"
    if sc.dispatch:
        signature = f"u: str, items: list[str], obj: {sc.dispatch[0]}, {params}"
    sc.define(
        block(
            f"def {entry}({signature}):",
            ["v = None", "out = []"]
            + block(f"for x in {origin}:", body + ["out.append(x)"])
            + ["return out"],
        )
    )
    url = "data:,x" if strip == "Allocates" else "http://example.com/u"
    inputs = []
    for k, controls in enumerate(sc.control_inputs(8)):
        base = [url, list(ITEMS[k % len(ITEMS)])]
        if sc.dispatch:
            for klass in sc.dispatch[1]:
                inputs.append({"args": [*base, {"$obj": [f"{sc.main_module}:{klass}", [], {}]}, *controls]})
        else:
            inputs.append({"args": [*base, *controls]})
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
# MUT101 and MUT111: a write that leaves a cached property stale
# ---------------------------------------------------------------------------------------------
def _kept_value_classes(sc, strip):
    """The classes of a kept-value program: `(holder, builds)`. The holder's `@cached_property`
    `features` reads an attribute (`raw`, a dict it rebinds, or `items`, a list it changes in
    place); its method `replace` writes it, then runs code that keeps the value; `builds` are the
    classes the entry may construct. Each stripped premise is made false by construction on a
    class the entry constructs."""
    sc.imports("import functools")
    holder, quiet = sc.name("Holder"), sc.name("Quiet")
    in_place = sc.chance(0.5 if strip == "Plain write" else 0.3)
    store_runs_code = strip == "Plain write" and not in_place
    if in_place:
        expr = '",".join(self.items)'
        write = ["self.items.append(text)"]
        if strip == "Plain write":
            # A project subclass of `list` whose `append` changes nothing (R2: a value annotated
            # `list` may be one).
            sc.define(block(f"class {quiet}(list):", block("def append(self, item):", ["return None"])))
            init = [f"self.items: list[str] = {quiet}() if flag else []"]
        else:
            init = ["self.items: list[str] = []"]
    else:
        expr = 'self.raw.get("k", "")'
        write = ["self.raw = raw"]
        init = []
    helpers = []
    if strip == "Reads on every path":
        if sc.chance(0.5):
            read = f'return {expr} if self.flag else ""'
        else:
            read = 'return self._look() if self.flag else ""'
            helpers = [""] + block("def _look(self) -> str:", [f"return {expr}"])
    elif strip == "Every implementation":
        read = "return self.hook()"
        helpers = [""] + block("def hook(self) -> str:", [f"return {expr}"])
    elif sc.chance(0.5):
        read = "return self._look()"
        helpers = [""] + block("def _look(self) -> str:", [f"return {expr}"])
    else:
        read = f"return {expr}"
    stored = ["self._raw = raw"] if store_runs_code else ["self.raw = raw"]
    body = block(
        "def __init__(self, raw: dict[str, str], flag: bool) -> None:",
        [*stored, "self.flag = flag", *init],
    )
    if store_runs_code:
        # Storing `raw` runs the property's setter, which stores a copy elsewhere.
        body += [""] + ["@property"] + block("def raw(self) -> dict[str, str]:", ["return self._raw"])
        body += [""] + ["@raw.setter"] + block("def raw(self, value: dict[str, str]) -> None:", ["self._raw = dict(value)"])
    body += [""] + ["@functools.cached_property"] + block("def features(self) -> str:", [read])
    body += helpers
    kept_strip = strip == "Kept to the return"
    params = "self, raw: dict[str, str], text: str" + (", hook" if kept_strip else "")
    after = ["hook(self)"] if kept_strip else []
    body += [""] + block(f"def replace({params}) -> None:", [*write, *after, "return None"])
    sc.define(block(f"class {holder}:", body))
    builds = [holder]
    if strip == "Cached property":
        plain = sc.name("Plain")
        sc.define(block(f"class {plain}({holder}):", ["@property"] + block("def features(self) -> str:", ['return "plain"'])))
        builds.append(plain)
    elif strip == "Every implementation":
        other = sc.name("Other")
        sc.define(block(f"class {other}({holder}):", block("def hook(self) -> str:", ['return "other"'])))
        builds.append(other)
    return holder, builds


def _kept_value_entry(sc, strip, holder, builds, reader=None):
    """The entry of a kept-value program: it constructs one of `builds`, computes the cached
    value, runs the writer, then the reader (`MUT111`) and the property again. With a reader it
    reads the property by a name it builds, so that its own reads are not claims of their own."""
    entry = sc.name("use")
    pick = "cls = builds[n % len(builds)]" if len(builds) > 1 else f"cls = {holder}"
    kept_strip = strip == "Kept to the return"
    args = "other, text, hook" if kept_strip else "other, text"
    prop = '"".join(("feat", "ures"))'
    get = f"getattr(obj, {prop})" if reader is not None else "obj.features"
    lines = [
        f"builds = [{', '.join(builds)}]",
        pick,
        "obj = cls(raw, flag)",
        f"first = {get}",
        f"obj.replace({args})",
    ]
    if reader is not None:
        lines.append(f"seen = {reader}(obj)")
    lines.append(f"return first, {get}")
    extra = "hook, " if kept_strip else ""
    sc.define(block(f"def {entry}(raw, other, {extra}{sc.params}):", lines))
    popper = None
    if kept_strip:
        popper = sc.name("forget")
        sc.define(block(f"def {popper}(obj):", ['obj.__dict__.pop("features", None)']))

    def values(k):
        raw = {"$dict": [["k", ["a", "b", "c"][k % 3]]]}
        other = {"$dict": [["k", ["x", "y"][k % 2]]]}
        out = [raw, other]
        if popper is not None:
            out.append({"$ref": f"{sc.main_module}:{popper}"})
        return out

    _add_inputs(sc, entry, values)


def mut101(sc, strip):
    holder, builds = _kept_value_classes(sc, strip)
    _kept_value_entry(sc, strip, holder, builds)


def mut111(sc, strip):
    shared = None if strip in ("Reads the property", "Runs the writer") else strip
    holder, builds = _kept_value_classes(sc, shared)
    reader = sc.name("check")
    if strip == "Reads the property":
        # The property is read by a name the code does not show.
        sc.define(block(f"def {reader}(obj: {holder}) -> bool:", ['return bool(getattr(obj, "".join(("feat", "ures"))))']))
    elif strip == "Runs the writer":
        sibling = sc.name("Sibling")
        sc.define(block(f"class {sibling}({holder}):", block("def replace(self, raw, text) -> None:", ["return None"])))
        builds = [*builds, sibling]
        sc.define(block(f"def {reader}(obj: {holder}) -> bool:", ['return obj.features.startswith("x")']))
    else:
        sc.define(block(f"def {reader}(obj: {holder}) -> bool:", ['return obj.features.startswith("x")']))
    _kept_value_entry(sc, shared, holder, builds, reader)


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
    annotation = " -> list[str]"
    change = 'result.append("x")'
    if strip == "Container kind":
        # Neither the construction nor an annotation names a builtin container.
        sc.frozen = True
        build = f"Frozen_{sc.ns}({build})"
        annotation = ""
    if strip == "Changed in place":
        # No change in place, or one a project subclass replaces where only the return
        # annotation names the kind (R2 admits the subclass).
        if sc.pick(["rebind", "subclass"]) == "rebind":
            change = 'result = result + ["x"]'
        else:
            sc.frozen = True
            build = f"Frozen_{sc.ns}({build})"
    body = ['name = "fixed"', f"return {build}"] if strip == "Keyed" else [f"return {build}"]
    if strip == "Every implementation":
        _cached_through_a_protocol(sc, deco, annotation, body, change)
        return
    sc.define([deco] + block(f"def {cached}({params}){annotation}:", body))
    call = f"{cached}({'' if strip == 'Keyed' else 'name'})"
    lines = [f"result = {call}"]
    if strip == "Same object":
        lines = [sc.pick([f"result = list({call})", f"result = {call} if not flag else []"])]
    entry = sc.name("use")
    sc.define(block(f"def {entry}(name, {sc.params}):", lines + [change, "return result"]))
    _add_inputs(sc, entry, lambda k: [["a", "b"][k % 2]])


def _cached_through_a_protocol(sc, deco, annotation, body, change):
    """The cached function is the project's one implementation of a protocol's `split`, called on
    a value typed by the protocol; a `str` has the method too, and its `split` hands out a new
    list on every call, so on those inputs the line changes no list a cache keeps."""
    sc.imports("from typing import Protocol")
    proto, klass = sc.name("Splits"), sc.name("Cached")
    sc.define(block(f"class {proto}(Protocol):", block("def split(self, name):", ["..."])))
    sc.define(block(f"class {klass}:", [deco] + block(f"def split(self, name){annotation}:", body)))
    entry = sc.name("use")
    lines = ["result = source.split(name)", change, "return result"]
    sc.define(block(f"def {entry}(name, source: {proto}, {sc.params}):", lines))

    def values(k):
        source = "a,b" if k % 2 else {"$obj": [f"{sc.main_module}:{klass}", [], {}]}
        return [["a", "b"][k % 2], source]

    _add_inputs(sc, entry, values)


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
# MUT118: an `object()` sentinel an instance holds, tested by identity, deep-copied
# ---------------------------------------------------------------------------------------------
def _sentinel_holder(sc, klass, sentinel, strip):
    """A class whose instances hold `sentinel` in `value` (a dataclass field default, a
    parameter default stored on `self`, or a store in `__init__`); with "Held by the class"
    stripped, one that only has it as its class attribute. Returns the class's source lines and
    whether its constructor takes `value`."""
    if strip == "Held by the class":
        way = sc.pick(["class-attribute", "not-in-init"])
        if way == "class-attribute":
            body = [f"value = {sentinel}"] + block("def __init__(self, name):", ["self.name = name"])
            return block(f"class {klass}:", body), False
        sc.imports("from dataclasses import dataclass, field")
        body = ["name: str", f"value: object = field(default={sentinel}, init=False)"]
        return ["@dataclass"] + block(f"class {klass}:", body), False
    way = sc.pick(["dataclass", "parameter", "store"])
    if way == "dataclass":
        sc.imports("from dataclasses import dataclass")
        return ["@dataclass"] + block(f"class {klass}:", ["name: str", f"value: object = {sentinel}"]), True
    if way == "parameter":
        init = block(f"def __init__(self, name, value={sentinel}):", ["self.name = name", "self.value = value"])
        return block(f"class {klass}:", init), True
    init = block("def __init__(self, name):", ["self.name = name", f"self.value = {sentinel}"])
    return block(f"class {klass}:", init), False


def mut118(sc, strip):
    ns = sc.ns
    sc.imports("import copy")
    sentinel = f"MISSING_{ns}"
    if strip == "Object sentinel":
        if sc.chance(0.5):
            marker = sc.name("Marker")
            keep = block("def __deepcopy__(self, memo):", ["return self"]) + block("def __copy__(self):", ["return self"])
            sc.define(block(f"class {marker}:", keep))
            sc.define([f"{sentinel} = {marker}()"])
        else:
            # Bound twice: no one object the name holds.
            sc.define([f"{sentinel} = object()", f"{sentinel} = object()"])
    else:
        sc.define([f"{sentinel} = object()"])
    klass = sc.name("Spec")
    lines, takes_value = _sentinel_holder(sc, klass, sentinel, strip)
    if strip == "Deep-copied" and sc.chance(0.5):
        # A copy hook of the class keeps the instance, sentinel and all.
        lines = lines + ["    " + line for line in block("def __deepcopy__(self, memo):", ["return self"])]
    sc.define(lines)
    tester = sc.name("given")
    test = f"item.value is not {sentinel}"
    if strip == "Tested by identity":
        if sc.chance(0.5):
            test = f"item.value != {sentinel}"
        else:
            other = sc.name("Other")
            sc.define(block(f"class {other}:", block("def __init__(self):", [f"self.value = {sentinel}"])))
            test = f"other.value is not {sentinel}"
            sc.define(block(f"def {tester}(item: {klass}, other: {other} = {other}()) -> bool:", [f"return {test}"]))
    if not (strip == "Tested by identity" and "other" in test):
        sc.define(block(f"def {tester}(item: {klass}) -> bool:", [f"return {test}"]))
    copied = "copy.deepcopy(item)"
    if strip == "Deep-copied" and not any("__deepcopy__" in line for line in lines):
        copied = "copy.deepcopy(item, {})"
    entry = sc.name("isolated")
    sc.define(
        block(
            f"def {entry}(item: {klass}, {sc.params}):",
            [f"copied = {copied}", f"return {tester}(item), {tester}(copied)"],
        )
    )

    def value(k):
        kwargs = {"value": k} if takes_value and k % 2 else {}
        return [{"$obj": [f"{sc.main_module}:{klass}", [f"n{k}"], kwargs]}]

    _add_inputs(sc, entry, value)


# ---------------------------------------------------------------------------------------------
# MUT119: an in-place change of a new builtin container that nothing else holds
# ---------------------------------------------------------------------------------------------
# Per kind: what makes a new one where it is changed (a display or a builtin maker, of the
# entry's parameters `xs: list[int]`, `opts: dict[str, int]`, and `n`), copies of the annotated
# parameters, one held by the module, and the changes as `(text with {x} for the object, form,
# whether the method returns a value)`. The inputs keep `xs` non-empty and "a" in `opts`, so that
# every change runs without an exception.
MUT119_KINDS = {
    "list": (
        ["[3, 1, 2]", "list(xs)", "sorted(xs)"],
        ["xs.copy()", "xs[:]"],
        "[5, 6]",
        [
            ("{x}.sort()", "method", False),
            ("{x}.reverse()", "method", False),
            ("{x}.append(n)", "method", False),
            ("{x}.extend(xs)", "method", False),
            ("{x}.insert(0, n)", "method", False),
            ("{x}.clear()", "method", False),
            ("{x}.pop()", "method", True),
            ("{x}[0] = n", "store", False),
            ("del {x}[0]", "delete", False),
        ],
    ),
    "dict": (
        ['{"a": 1, "b": 2}', "dict(opts)", "dict(a=1, b=n)"],
        ["opts.copy()"],
        '{"a": 0}',
        [
            ("{x}.update(c=n)", "method", False),
            ("{x}.clear()", "method", False),
            ('{x}.setdefault("c", n)', "method", True),
            ('{x}.pop("a", None)', "method", True),
            ('{x}["c"] = n', "store", False),
            ('del {x}["a"]', "delete", False),
        ],
    ),
    "set": (
        ["{1, 2, 3}", "set(xs)"],
        [],
        "{5, 6}",
        [
            ("{x}.add(n)", "method", False),
            ("{x}.discard(1)", "method", False),
            ("{x}.update(xs)", "method", False),
            ("{x}.clear()", "method", False),
            ("{x}.pop()", "method", True),
        ],
    ),
    "bytearray": (
        ['bytearray(b"abc")', "bytearray(xs)"],
        [],
        'bytearray(b"q")',
        [
            ("{x}.append(65)", "method", False),
            ('{x}.extend(b"z")', "method", False),
            ("{x}.reverse()", "method", False),
            ("{x}.pop()", "method", True),
            ("{x}[0] = 66", "store", False),
            ("del {x}[0]", "delete", False),
        ],
    ),
    "deque": (
        ["collections.deque(xs)", "collections.deque([1, 2])"],
        [],
        "collections.deque([5])",
        [
            ("{x}.append(n)", "method", False),
            ("{x}.appendleft(n)", "method", False),
            ("{x}.extend(xs)", "method", False),
            ("{x}.clear()", "method", False),
            ("{x}.popleft()", "method", True),
            ("{x}[0] = n", "store", False),
            ("del {x}[0]", "delete", False),
        ],
    ),
}
# The builtin maker a module can bind to a function of its own, per kind.
MUT119_MAKERS = {"list": "sorted", "dict": "dict", "set": "set", "bytearray": "bytearray"}
# How each premise is stripped.
MUT119_WAYS = {
    # What makes the object outlives the call: a function stores it first, a getter returns its
    # field, one path returns module state, the parameter's class copies as itself, or the module
    # binds the maker's name to a function returning module state.
    "New object": ["stored", "field", "path", "copy-self", "shadowed"],
    # A name holds the object: bound first, or bound in the changed expression.
    "Unnamed": ["bound", "walrus"],
    # The change does more than its builtin type's: its result is read, the parameter's class
    # replaces the copy and the change, or an augmented item assignment changes the item in place.
    "Changed in place": ["read", "override", "augmented"],
}


def _mut119_owned(made):
    """`made` as a getter computes it from the instance's copies of the parameters."""
    return re.sub(r"\b(xs|opts|n)\b", r"self._\1", made)


def _mut119_made(sc, made, way):
    """Project code making the object: a function (through another one, or not) or the getter of
    a property of an object the entry holds, returning `made`. With "New object" stripped, what
    they return outlives the call: stored first, the getter's field, or module state on one path.
    Returns the statements the entry runs first and the object's expression."""
    ns = sc.ns
    if way == "field" or (way == "stored" and sc.chance(0.5)) or (way is None and sc.chance(0.4)):
        holder, prop = sc.name("Holder"), sc.name("value")
        init = ["self._xs = xs", "self._opts = opts", "self._n = n"]
        if way == "field":
            init.append(f"self._field = {_mut119_owned(made)}")
            getter = ["return self._field"]
        elif way == "stored":
            getter = [f"self._last = {_mut119_owned(made)}", "return self._last"]
        else:
            getter = [f"return {_mut119_owned(made)}"]
        sc.define(
            block(
                f"class {holder}:",
                block("def __init__(self, xs, opts, n):", init) + ["", "@property"] + block(f"def {prop}(self):", getter),
            )
        )
        return [f"holder = {holder}(xs, opts, n)"], f"holder.{prop}"
    make = sc.name("make")
    if way == "stored":
        sc.define([f"KEEP_{ns} = []"])
        body = [f"out = {made}", f"KEEP_{ns}.append(out)", "return out"]
    elif way == "path":
        body = [f"return {made} if n > 1 else SHARED_{ns}"]
    else:
        body = [f"return {made}"]
    sc.define(block(f"def {make}(xs, opts, n):", body))
    if way is None and sc.chance(0.4):
        outer = sc.name("fresh")
        sc.define(block(f"def {outer}(xs, opts, n):", [f"return {make}(xs, opts, n)"]))
        make = outer
    return [], f"{make}(xs, opts, n)"


def mut119(sc, strip):
    ns = sc.ns
    way = sc.pick(MUT119_WAYS[strip]) if strip else None
    if way in ("copy-self", "override", "augmented"):
        kind = sc.pick(["list", "dict"])
    elif way == "shadowed":
        kind = sc.pick(sorted(MUT119_MAKERS))
    else:
        kind = sc.pick(sorted(MUT119_KINDS))
    built, copies, held, changes = MUT119_KINDS[kind]
    if kind == "deque":
        sc.imports("import collections")
    if way in ("path", "shadowed"):
        sc.define([f"SHARED_{ns} = {held}"])
    lines = []
    if way in ("path", "shadowed"):
        # The entry replenishes the module state a previous input may have emptied.
        lines += [f"global SHARED_{ns}", f"SHARED_{ns} = {held}"]
    origin = sc.pick(["built", "built", "project"] + (["copy"] if copies else []))
    if way in ("stored", "field", "path"):
        origin = "project"
    elif way in ("copy-self", "override"):
        origin = "copy"
    elif way == "shadowed":
        origin = "built"
    elif way == "augmented":
        origin = sc.pick(["built", "project"])
    if way == "augmented":
        sc.define([f"ITEMS_{ns} = []"])
        built = [f"[ITEMS_{ns}, 1]" if kind == "list" else f'{{"a": ITEMS_{ns}}}']
    if way == "shadowed":
        built = [b for b in built if b.startswith(MUT119_MAKERS[kind] + "(")]
    if way in ("copy-self", "override"):
        obj = "xs.copy()" if kind == "list" else "opts.copy()"
    elif origin == "copy":
        obj = sc.pick(copies)
    elif origin == "project":
        made_lines, obj = _mut119_made(sc, sc.pick(built), way)
        lines += made_lines
    else:
        obj = sc.pick(built)
    text, form, returns_value = sc.pick(changes)
    if way == "read":
        text, form, returns_value = sc.pick([c for c in changes if c[1] == "method"])
    tail = "return len(xs)"
    if way == "bound":
        lines.append(f"items = {obj}")
        obj, tail = "items", "return len(items)"
    elif way == "walrus":
        obj, tail = f"(items := {obj})", "return len(items)"
    change = text.format(x=obj)
    if way == "augmented":
        change = f"{obj}[{'0' if kind == 'list' else repr('a')}] += [n]"
    elif way == "read":
        change, tail = f"top = {change}" + ("" if returns_value else " or 0"), "return top"
    elif form == "method" and not returns_value and way is None:
        result = sc.pick(["unused", "unused", "bound", "stored"])
        if result == "bound":
            change, tail = f"env = {change}", "return env"
        elif result == "stored":
            lines.append("out = {}")
            change, tail = f"out[key] = {change}", "return out"
    # A loop would run a destructive change twice on a receiver the entry retains.
    retained = way in ("field", "path", "shadowed", "copy-self", "override")
    place = sc.pick(["plain", "plain", "if", "try"] if retained else ["plain", "plain", "if", "for", "try"])
    if place == "if":
        if way in ("walrus", "read"):
            # The change binds the name the tail reads; `flag` may be false.
            lines.append("items = []" if way == "walrus" else "top = 0")
        lines += block("if flag:", [change])
    elif place == "for":
        lines += block("for _ in range(2):", [change])
    elif place == "try":
        lines += block("try:", [change]) + block("except ValueError:", ["return None"])
    else:
        lines.append(change)
    if way in ("copy-self", "override"):
        _mut119_subclass(sc, kind, way, text, form)
    entry = sc.name("lost")
    sc.define(block(f"def {entry}(xs: list[int], opts: dict[str, int], {sc.params}):", lines + [tail]))
    if way == "shadowed":
        # After the entry: its annotations name the builtins.
        sc.define(block(f"def {MUT119_MAKERS[kind]}(*args, **kwargs):", [f"return SHARED_{ns}"]))
    subclass = f"{sc.main_module}:Shared_{ns}" if way in ("copy-self", "override") else None

    def value(k):
        xs = [3, 1, 2][: 1 + k % 3]
        opts = {"$dict": [["a", 1], ["b", k]]}
        if subclass is not None and k % 2:
            if kind == "list":
                xs = {"$obj": [subclass, [xs], {}]}
            else:
                opts = {"$obj": [subclass, [opts], {}]}
        return [xs, opts]

    _add_inputs(sc, entry, value)


def _mut119_subclass(sc, kind, way, text, form):
    """A project subclass of the copied parameter's type, which the odd inputs pass: its copy is
    the object itself ("New object" stripped), or a new one whose change the subclass replaces
    ("Changed in place" stripped)."""
    ns = sc.ns
    klass = f"Shared_{ns}"
    if way == "copy-self":
        body = block("def copy(self):", ["return self"])
    else:
        sc.define([f"LOG_{ns} = []"])
        member = {"store": "__setitem__", "delete": "__delitem__"}.get(form) or text[4:].split("(")[0]
        body = block("def copy(self):", [f"return {klass}(self)"]) + [""]
        body += block(
            f"def {member}(self, *args, **kwargs):",
            [f'LOG_{ns}.append("{member}")', f"return super().{member}(*args, **kwargs)"],
        )
    sc.define(block(f"class {klass}({kind}):", body))


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


# ---------------------------------------------------------------------------------------------
# EFF109: a lock the entry holds, taken again through helpers
# ---------------------------------------------------------------------------------------------
def _holding(lock, body, way, awaits=False):
    """`body` with `lock` held: in a `with`, or between `acquire()` and a `release()` in
    `finally`."""
    if way == "with":
        return block(f"{'async ' if awaits else ''}with {lock}:", body)
    acquire = f"{'await ' if awaits else ''}{lock}.acquire()"
    return [acquire] + block("try:", body) + block("finally:", [f"{lock}.release()"])


def _held_strip(sc, lock, body, signature, args):
    """The entry's statements, with `lock` not held at the call on some input: taken on one
    path, given back before the call, given back by a helper (a handoff), or held by a generator
    making the call, which the entry has a helper of another module give back while the
    generator is suspended."""
    way = sc.pick(["one-path", "released", "helper", "suspended"])
    if way == "suspended":
        rows = sc.name("rows")
        call = body[0].removeprefix("return ")
        given_back = block("finally:", block(f"if {lock}.locked():", [f"{lock}.release()"]))
        holding = [f"{lock}.acquire()"] + block("try:", ["yield None", f"yield {call}"]) + given_back
        sc.define(block(f"def {rows}({signature}):", holding))
        give = sc.name("give_back")
        sc.define(block(f"def {give}(lock):", ["lock.release()"]), helper=True)
        resume = block("if flag:", [f"{sc.ref(give, False, True)}({lock})"])
        return [f"rows = {rows}({args})", "next(rows)", *resume, "return next(rows)"]
    if way == "one-path":
        release = block("if flag:", [f"{lock}.release()"])
        return block("if flag:", [f"{lock}.acquire()"]) + block("try:", body) + block("finally:", release)
    if way == "released":
        inner = [f"{lock}.release()"] + block("try:", body) + block("finally:", [f"{lock}.acquire()"])
        return block(f"with {lock}:", inner)
    unlock = sc.name("unlock")
    sc.define(block(f"def {unlock}():", [f"{lock}.release()"]))
    return [f"{lock}.acquire()", f"{unlock}()", *body]


def _reentrant_lock(sc):
    """A project class derived from `asyncio.Lock` that takes it again without waiting."""
    klass = sc.name("Reentrant")
    sc.define(
        block(
            f"class {klass}(asyncio.Lock):",
            ["depth = 0", ""]
            + block(
                "async def acquire(self):",
                ["self.depth += 1"] + block("if self.depth > 1:", ["return True"]) + ["return await super().acquire()"],
            )
            + [""]
            + block(
                "def release(self):",
                ["self.depth -= 1"] + block("if self.depth == 0:", ["super().release()"]),
            ),
        )
    )
    return klass


def _eff109_async(sc, strip, lock, way):
    """Coroutines one or two deep under the entry; with Non-reentrant stripped, the lock is of a
    project class derived from `asyncio.Lock`."""
    sc.imports("import asyncio")
    made = f"{_reentrant_lock(sc)}()" if strip == "Non-reentrant" else "asyncio.Lock()"
    sc.define([f"{lock}: asyncio.Lock = {made}"])
    call = None
    for _ in range(sc.rng.randint(1, 2)):
        name = sc.name("astep")
        inner = _holding(lock, ["v = 1"], sc.pick(["with", "acquire"]), True) if call is None else [f"v = await {call}"]
        sc.define(block(f"async def {name}({sc.params}):", ["v = None", *inner, "return v"]))
        call = f"{name}({CONTROLS})"
    entry = sc.name("handle")
    sc.define(block(f"async def {entry}({sc.params}):", _holding(lock, [f"return await {call}"], way, True)))
    _add_inputs(sc, entry, lambda k: [])


def eff109(sc, strip):
    sc.dispatch = None
    ns = sc.ns
    way = sc.pick(["with", "acquire"])
    if strip == "Non-reentrant" or (strip is None and sc.chance(0.3)):
        _eff109_async(sc, strip, f"LOCK_{ns}", way)
        return
    sc.imports("import threading")
    same_lock_way = sc.pick(["rebound", "argument"]) if strip == "Same lock" else None
    # The lock is a module global, or an attribute of the entry's parameter that the entry
    # passes on to the helpers (with Same lock stripped: another object's).
    by_argument = same_lock_way == "argument" or (strip in (None, "Every return") and sc.chance(0.4))
    params, args = sc.params, CONTROLS
    if by_argument:
        klass = sc.name("Store")
        sc.define(block(f"class {klass}:", block("def __init__(self):", ["self.lock = threading.Lock()"])))
        lock, taken = "obj.lock", "box.lock"
        params, args = f"box: {klass}, {params}", f"box, {args}"
    else:
        lock = taken = f"LOCK_{ns}"
        sc.define([f"{lock} = threading.Lock()"])
    leaf = _holding(taken, ["v = 1"], sc.pick(["with", "acquire"]))
    if same_lock_way == "rebound":
        leaf = [f"global {lock}", f"{lock} = threading.Lock()", *leaf]
    ways = [*EVERY_RETURN_WAYS, "class-body"]
    call = _chain(sc, leaf, strip, params, args, prefix="lock", every_return_ways=ways)
    if by_argument:
        passed = "obj"
        if same_lock_way == "argument":
            passed = sc.name("OTHER")
            sc.define([f"{passed} = {klass}()"])
        call = call.replace("(box, ", f"({passed}, ", 1)
        sc.dispatch = (klass, [klass])
    body = [f"return {call}"]
    entry = sc.name("handle")
    signature, entry_args = sc.params, CONTROLS
    if sc.dispatch:
        signature, entry_args = f"obj: {sc.dispatch[0]}, {sc.params}", f"obj, {CONTROLS}"
    if strip == "Held":
        lines = _held_strip(sc, lock, body, signature, entry_args)
    else:
        lines = _holding(lock, body, way)
    sc.define(block(f"def {entry}({signature}):", lines))
    _add_inputs(sc, entry, lambda k: [])


# ---------------------------------------------------------------------------------------------
# EFF116: a generator nothing iterates
# ---------------------------------------------------------------------------------------------
def _generator_function(sc, strip, body):
    """A generator function with `body` and the call of it, from the entry: a module function
    or a method of the entry's receiver; with Generator function stripped, a name bound to a
    plain definition too (the one the run binds), or a decorator that iterates the generator."""
    params = sc.params
    name = sc.name("rows")
    defs = block(f"def {name}({params}):", body)
    if strip == "Generator function":
        way = sc.pick(["bound-twice", "decorated"])
        if way == "bound-twice":
            sc.imports("import os")
            plain = block(f"def {name}({params}):", [line for line in body if not line.startswith("yield")])
            sc.define(block(f"if not os.environ.get(\"HEKI_ORACLE_{sc.ns}\"):", plain) + block("else:", defs))
            return f"{name}({CONTROLS})"
        deco = sc.name("drains")
        sc.define(block(f"def {deco}(fn):", block("def inner(*a, **k):", ["return list(fn(*a, **k))"]) + ["return inner"]))
        sc.define([f"@{deco}"] + defs)
        return f"{name}({CONTROLS})"
    if sc.chance(0.5):
        sc.define(defs)
        return f"{name}({CONTROLS})"
    klass = sc.name("Rows")
    sc.define(block(f"class {klass}:", block(f"def rows(self, {params}):", body)))
    sc.dispatch = (klass, [klass])
    return f"obj.rows({CONTROLS})"


def _generator_dispatch(sc, body):
    """A method call whose implementations are not all generator functions: a subclass, a
    union member, or a protocol's implementer with a plain method."""
    params = sc.params
    klass, other = sc.name("Impl"), sc.name("Other")
    plain = [line for line in body if not line.startswith("yield")]
    way = sc.pick(["subclass", "union", "protocol"])
    sc.define(block(f"class {klass}:", block(f"def run(self, {params}):", body)))
    base = f"({klass})" if way == "subclass" else ""
    sc.define(block(f"class {other}{base}:", block(f"def run(self, {params}):", plain)))
    receiver = klass if way == "subclass" else f"{klass} | {other}"
    if way == "protocol":
        receiver = sc.name("Runner")
        sc.imports("from typing import Protocol")
        sc.define(block(f"class {receiver}(Protocol):", block(f"def run(self, {params}):", ["..."])))
    sc.dispatch = (receiver, [klass, other])
    return f"obj.run({CONTROLS})"


def _unread_generator(sc, call):
    """The call's generator, never iterated: a statement, or read only by a truth test."""
    return sc.pick(
        [
            [call],
            block(f"if not {call}:", ["return 1"]),
            [f"bool({call})"],
            block(f"while {call}:", ["break"]),
            [f"assert {call}"],
        ]
    )


def _iterated_generator(sc, call):
    """The call's generator, iterated: by a loop, a builtin, `next()` on a name, the caller
    (returned), or a delegating generator."""
    return sc.pick(
        [
            block(f"for _ in {call}:", ["pass"]),
            [f"list({call})"],
            [f"rows = {call}", "next(rows, None)"],
            [f"return {call}"],
            [f"yield from {call}"],
        ]
    )


def eff116(sc, strip):
    sc.dispatch = None
    log = f"LOG_{sc.ns}"
    sc.define([f"{log} = []"])
    body = [f"{log}.append(n)", "yield flag"]
    if strip == "Every implementation":
        call = _generator_dispatch(sc, body)
    else:
        call = _generator_function(sc, strip, body)
    uses = _iterated_generator(sc, call) if strip == "Never iterated" else _unread_generator(sc, call)
    entry = sc.name("handle")
    signature = sc.params
    if sc.dispatch:
        signature = f"obj: {sc.dispatch[0]}, {sc.params}"
    sc.define(block(f"def {entry}({signature}):", [*uses, "return None"]))
    _add_inputs(sc, entry, lambda k: [])


BUILDERS = {
    "EFF101": eff101,
    "EFF105": eff105,
    "EFF102": eff102,
    "EFF103": eff103,
    "EFF109": eff109,
    "EFF116": eff116,
    "DATA101": data101,
    "MUT101": mut101,
    "MUT110": mut110,
    "MUT111": mut111,
    "MUT114": mut114,
    "MUT115": mut115,
    "MUT118": mut118,
    "MUT119": mut119,
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
    "EFF105": ["Every return", "Every implementation", "Touches the file system"],
    "EFF102": ["Origin", "Every iteration", "Every return", "Every implementation", "Allocates"],
    "EFF103": ["Origin", "Every iteration", "Every return", "Every implementation", "Sends"],
    "EFF109": ["Non-reentrant", "Held", "Same lock", "Every implementation", "Every return"],
    "EFF116": ["Generator function", "Every implementation", "Never iterated"],
    "DATA101": ["Loop-carried on every path", "Every iteration", "Origin", "Reallocates"],
    "MUT101": [
        "Cached property",
        "Reads on every path",
        "Every implementation",
        "Plain write",
        "Kept to the return",
    ],
    "MUT110": ["Read-only parameter", "Passed as itself", "Changed at the line"],
    "MUT111": [
        "Cached property",
        "Reads on every path",
        "Every implementation",
        "Plain write",
        "Kept to the return",
        "Reads the property",
        "Runs the writer",
    ],
    "MUT114": ["Cached", "Keyed", "Container kind", "Same object", "Every implementation", "Changed in place"],
    "MUT115": ["Shallow copy", "Borrowed original", "Still shared", "Changed below the copy"],
    "MUT118": ["Object sentinel", "Held by the class", "Tested by identity", "Deep-copied"],
    "MUT119": ["New object", "Unnamed", "Changed in place"],
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
# (`heki dump-open-premises`). With a premise stripped, such a report has to name it among them.
OPEN_PREMISE_RULES = frozenset({"EFF101", "EFF105", "EFF102", "EFF103", "DATA101", "MUT110", "MUT114", *OPT_PATTERNS, "OPT113"})


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
    if sc.helper_defs:
        files["app/h0.py"] = _module_text(sc.helper_imports, sc.helper_defs)
    files["heki.toml"] = HEKI_TOML.format(rules=f'"{code}"')
    project = Project(seed, files, sc.entries, [f"table:{code}"])
    project.premise = {"rule": code, "stripped": strip, "premises": premises}
    return project
