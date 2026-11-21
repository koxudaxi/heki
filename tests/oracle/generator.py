"""Generates small multi-file Python projects from a grammar of the shapes the exact claims'
premises talk about.

`generate(seed)` is deterministic. A project is a package `app/` holding one to three scenarios,
each a family of shapes around one exact claim:

- `eff101`: an `async def` calling helpers N deep that end in a library call that may wait;
- `eff103`: a loop over an input whose body reaches a database call;
- `data101`: a loop growing an array or a frame;
- `mut110`: a read-only parameter handed to helpers that may change it in place;
- `mut114`: a change of what a cached function returned;
- `opt`: an accumulator rewritten by an `OPT` rule, with the aliases, iterators, handlers, and
  uses that decide whether the rewrite is safe;
- `opt113`: nested loops joining two collections on a key.

Every scenario draws the statements around its claim from one shared grammar: early returns,
`raise`, loads that may raise (a property whose getter raises, a subscript, `int(text)`, a
helper that raises) with `try` / `except` around them or around the call, `with` blocks whose
manager suppresses or not, loops with `break` and `continue`, branches, functions handed to
builtins (`map`, `sorted(key=)`), dispatch over subclasses, properties, `functools` caches,
decorators that may skip the body, functions defined twice, and module constants against
parameters. Scenario `i` lives in `app/c{i}.py` (shared controls), `app/s{i}.py` (the entry
point), and `app/h{i}.py` (helpers).

The entry points take the same control parameters: `flag`, `n`, `text`, `key`, and `cfg`, so
that each input decides which of the loads raise and which branches run. The inputs cover none
raising, each control raising alone, and random mixes.

The generator keeps every program inside the assumptions of `docs/precision.md`: no function or
class is replaced at run time (R1), every input matches its annotation (R2), the fakes follow the
catalog (R3), and a library call's deciding argument is a literal, a module constant, a
parameter, or a computed value that waits (R6): `pick_delay()` returns a nonzero sleep.
"""

from __future__ import annotations

import json
import random

CONTROLS = "flag, n, text, key, cfg"
HEKI_TOML = """\
target-version = "py312"

[lint]
select = [{rules}]
"""


def ind(lines, depth=1):
    pad = "    " * depth
    return [pad + line if line else line for line in lines]


def block(header, body):
    return [header] + ind(body or ["pass"])


class Scenario:
    def __init__(self, rng, index):
        self.rng = rng
        self.i = index
        self.ns = f"s{index}"
        self.common = f"app.c{index}"
        self.main_module = f"app.s{index}"
        self.helper_module = f"app.h{index}"
        self.main_defs = []
        self.helper_defs = []
        self.main_imports = set()
        self.helper_imports = set()
        self.entries = []
        self.counter = 0
        self.extra_files = {}
        # Project subclasses of the builtin containers, defined only where a scenario passes
        # them: their presence alone changes what heki may assume about every annotation.
        self.frozen = False
        self.rows = False
        # Typed controls let heki know the class of `cfg`, so its R7 rules for attribute loads
        # decide; untyped ones leave every load open.
        self.params = (
            f"flag: bool, n: int, text: str, key: str, cfg: Cfg_{self.ns}" if rng.random() < 0.5 else CONTROLS
        )

    def name(self, stem):
        self.counter += 1
        return f"{stem}_{self.ns}_{self.counter}"

    def noise(self, most=2):
        """How many extra statements to put before a call: usually none or one, so that most
        programs keep every premise and a few break one."""
        return min(most, self.rng.choice([0, 0, 0, 1, 1, 2]))

    def pick(self, options):
        return self.rng.choice(options)

    def chance(self, p):
        return self.rng.random() < p

    def define(self, lines, helper=False):
        (self.helper_defs if helper else self.main_defs).append(lines)

    def imports(self, line, helper=False):
        (self.helper_imports if helper else self.main_imports).add(line)

    def ref(self, name, helper_side, defined_in_helper):
        """How code in one module refers to a function defined in this or the other module."""
        if helper_side == defined_in_helper:
            return name
        if defined_in_helper:
            self.imports(f"from app import h{self.i}")
            return f"h{self.i}.{name}"
        self.imports(f"from app.s{self.i} import {name}", helper=True)
        return name

    # -- controls ---------------------------------------------------------------------------
    def common_source(self):
        ns = self.ns
        return "\n".join(
            [
                "import time",
                "",
                f"LIMIT_{ns} = 3",
                f"DEBUG_{ns} = False",
                f'TABLE_{ns} = {{"a": 1, "b": 2}}',
                f"DELAY_{ns} = {self.pick(['0.25', '1'])}",
                f'URL_{ns} = "http://example.com/{ns}"',
                f'DATA_URL_{ns} = "data:,{ns}"',
                f'HOST_{ns} = "{self.pick(["example.com", "db.internal"])}"',
                f'DB_FILE_{ns} = "{ns}.db"',
                f'SQL_{ns} = "UPDATE t SET v = ? WHERE k = ?"',
                "",
                "",
                f"class Cfg_{ns}:",
                "    def __init__(self, bad):",
                "        self.bad = bad",
                "        if not bad:",
                "            self.extra = 1",
                "",
                "    @property",
                "    def value(self):",
                "        if self.bad:",
                '            raise ValueError("bad config")',
                "        return 1",
                "",
                "    def ping(self):",
                "        return 1",
                "",
                "    def __getattribute__(self, name):",
                '        if name == "ping" and object.__getattribute__(self, "bad"):',
                '            raise ValueError("bad ping")',
                "        return object.__getattribute__(self, name)",
                "",
                "",
                f"def check_{ns}(n):",
                "    if n < 0:",
                '        raise ValueError("negative")',
                "    return n",
                "",
                "",
                f"def pick_delay_{ns}():",
                "    return 0.5",
                "",
                "",
                f"class Plain_{ns}:",
                "    def __enter__(self):",
                "        return self",
                "",
                "    def __exit__(self, *exc):",
                "        return False",
                "",
                "",
                f"class Quiet_{ns}:",
                "    def __enter__(self):",
                "        return self",
                "",
                "    def __exit__(self, *exc):",
                "        return True",
                "",
                "",
            ]
            + self.container_subclasses()
        )

    def container_subclasses(self):
        ns = self.ns
        lines = []
        if self.frozen:
            lines += [
                "# Project subclasses of the builtin containers whose changing methods change nothing:",
                "# values of the annotated types under assumption R2.",
                f"class Frozen_{ns}(list):",
                "    def append(self, item):",
                "        return None",
                "",
                "    def extend(self, items):",
                "        return None",
                "",
                "    def insert(self, index, item):",
                "        return None",
                "",
                "    def sort(self, *args, **kwargs):",
                "        return None",
                "",
                "    def reverse(self):",
                "        return None",
                "",
                "    def clear(self):",
                "        return None",
                "",
                "    def pop(self, *args):",
                "        return None",
                "",
                "    def __setitem__(self, index, value):",
                "        return None",
                "",
                "    def __delitem__(self, index):",
                "        return None",
                "",
                "    def __iadd__(self, other):",
                "        return self",
                "",
                "",
                f"class FrozenDict_{ns}(dict):",
                "    def __setitem__(self, key, value):",
                "        return None",
                "",
                "    def update(self, *args, **kwargs):",
                "        return None",
                "",
                "    def pop(self, *args):",
                "        return None",
                "",
                "    def setdefault(self, key, default=None):",
                "        return default",
                "",
                "    def clear(self):",
                "        return None",
                "",
                "    def __ior__(self, other):",
                "        return self",
                "",
                "",
                f"class FrozenSet_{ns}(set):",
                "    def add(self, item):",
                "        return None",
                "",
                "    def discard(self, item):",
                "        return None",
                "",
                "    def update(self, *args):",
                "        return None",
                "",
                "    def __ior__(self, other):",
                "        return self",
                "",
                "",
            ]
        if self.rows:
            lines += [
                f"class Rows_{ns}(list):",
                "    def __iter__(self):",
                "        for item in list.__iter__(self):",
                "            yield item",
                "",
                "",
            ]
        return lines

    def common_names(self):
        ns = self.ns
        return (
            [
                f"LIMIT_{ns}",
                f"DEBUG_{ns}",
                f"TABLE_{ns}",
                f"DELAY_{ns}",
                f"URL_{ns}",
                f"DATA_URL_{ns}",
                f"HOST_{ns}",
                f"DB_FILE_{ns}",
                f"SQL_{ns}",
                f"Cfg_{ns}",
                f"check_{ns}",
                f"pick_delay_{ns}",
                f"Plain_{ns}",
                f"Quiet_{ns}",
            ]
            + ([f"Frozen_{ns}", f"FrozenDict_{ns}", f"FrozenSet_{ns}"] if self.frozen else [])
            + ([f"Rows_{ns}"] if self.rows else [])
        )

    def control_inputs(self, count):
        """Values of the control parameters: none raising, each raising alone, then mixes."""
        base = {"flag": False, "n": 2, "text": "7", "key": "a", "bad": False}
        alternatives = {
            "flag": [True],
            "n": [0, -1, 5],
            "text": ["x"],
            "key": ["zz", "fail"],
            "bad": [True],
        }
        out = [dict(base)]
        for name, values in alternatives.items():
            for value in values:
                one = dict(base)
                one[name] = value
                out.append(one)
        while len(out) < count:
            mix = {name: self.rng.choice([base[name], *alternatives[name]]) for name in base}
            out.append(mix)
        specs = []
        for values in out[:count] if len(out) > count else out:
            # One object per value, shared by the runs, so that a cache keyed on it can hit.
            cfg = {
                "$shared": [
                    f"cfg-{values['bad']}",
                    {"$obj": [f"{self.common}:Cfg_{self.ns}", [values["bad"]], {}]},
                ]
            }
            specs.append([values["flag"], values["n"], values["text"], values["key"], cfg])
        return specs

    # -- shared grammar -----------------------------------------------------------------------
    def risky(self):
        """An expression that may raise, with its exception class, or one that cannot (R7)."""
        ns = self.ns
        return self.pick(
            [
                ("cfg.value", "ValueError"),
                (f"TABLE_{ns}[key]", "KeyError"),
                ("int(text)", "ValueError"),
                ("10 // n", "ZeroDivisionError"),
                (f"check_{ns}(n)", "ValueError"),
                ("cfg.extra", "AttributeError"),
                ("cfg.ping", "ValueError"),
                ("n", None),
                (f"LIMIT_{ns}", None),
                ("[n, 1]", None),
                ("(n, flag)", None),
            ]
        )

    def handler_class(self, raised):
        if raised is None or self.chance(0.2):
            return self.pick(["ValueError", "KeyError", "Exception", "(ValueError, KeyError)"])
        return raised

    def prelude(self, in_loop=False):
        """A statement before the call that a premise has to look through."""
        ns = self.ns
        expr, raised = self.risky()
        options = [
            lambda: block("if flag:", ["return None"]) if not in_loop else block("if flag:", ["continue"]),
            lambda: block("if n == 0:", ["return None" if not in_loop else "continue"]),
            lambda: block(f"if DEBUG_{ns}:", ["return None" if not in_loop else "continue"]),
            lambda: block("if n < 0:", ['raise ValueError("negative")']),
            lambda: [f"v = {expr}"],
            lambda: (
                block("try:", [f"v = {expr}"])
                + block(
                    f"except {self.handler_class(raised)}:",
                    [
                        self.pick(
                            ["return None", "pass", "v = 0", "raise"]
                            + (["continue", "break"] if in_loop else [])
                        )
                    ],
                )
            ),
            lambda: block(f"for item in range(LIMIT_{ns}):", block("if item == n:", ["break"])),
            lambda: block(f"with Plain_{ns}():", [f"v = {expr}"]),
            lambda: block(f"with Quiet_{ns}():", [f"v = {expr}"]),
            lambda: block("with contextlib.suppress(ValueError):", [f"v = {expr}"]),
            lambda: [f"v = LIMIT_{ns}"],
            lambda: block("if flag:", [f"v = {expr}"]) + block("else:", ["v = n"]),
        ]
        if in_loop:
            options.append(lambda: block('if x == "stop":', ["break"]))
            options.append(lambda: block('if x == "skip":', ["continue"]))
        return self.pick(options)()

    def wrap(self, stmt, in_loop=False):
        """The call statement, possibly inside a `try`, a `with`, a branch, or a loop."""
        ns = self.ns
        expr, raised = self.risky()
        choice = self.pick(
            ["plain"] * 12
            + [
                "try",
                "try",
                "with_plain",
                "with_quiet",
                "suppress",
                "if",
                "ifelse",
                "for",
                "while",
                "after_risky",
                "finally",
            ]
        )
        if choice == "plain":
            return stmt
        if choice == "try":
            handler = self.pick(["return None", "pass", "raise"] + (["continue"] if in_loop else []))
            return block("try:", stmt) + block(f"except {self.handler_class(raised)}:", [handler])
        if choice == "with_plain":
            return block(f"with Plain_{ns}():", stmt)
        if choice == "with_quiet":
            return block(f"with Quiet_{ns}():", [f"v = {expr}", *stmt])
        if choice == "suppress":
            return block("with contextlib.suppress(ValueError):", [f"v = {expr}", *stmt])
        if choice == "if":
            return block("if flag:", stmt)
        if choice == "ifelse":
            return block("if flag:", stmt) + block("else:", stmt)
        if choice == "for":
            return block(
                f"for _ in {self.pick(['range(n)', '[1]', '[1, 2]', 'range(LIMIT_' + ns + ')'])}:", stmt
            )
        if choice == "while":
            return block("while True:", stmt + ["break"])
        if choice == "finally":
            return block("try:", [f"v = {expr}"]) + block("finally:", stmt)
        return block("try:", [f"v = {expr}", *stmt]) + block(
            f"except {self.handler_class(raised)}:",
            [self.pick(["return None", "pass"] + (["continue"] if in_loop else []))],
        )


# ---------------------------------------------------------------------------------------------
# Call chains: helpers N deep, reached through the call forms the claims talk about
# ---------------------------------------------------------------------------------------------
def build_chain(sc, depth, leaf_lines, params, args, leaf_imports=(), leaf_consts=()):
    """Defines `depth` helpers, the last running `leaf_lines`; returns how the caller in the main
    module calls the first one (an expression)."""
    nxt_call = None
    nxt_in_helper = False
    for _level in range(depth):
        # Built from the deepest helper up. A helper module never imports the entry module
        # back: a hop there calls hops there.
        in_helper = sc.chance(0.5) and (nxt_call is None or nxt_in_helper)
        name = sc.name("step")
        for imp in leaf_imports:
            sc.imports(imp, helper=in_helper)
        sc.imports("import contextlib", helper=in_helper)
        sc.imports(_common_import(sc), helper=in_helper)
        if nxt_call is None:
            inner = list(leaf_lines)
            if leaf_consts:
                sc.define(list(leaf_consts), helper=in_helper)
        else:
            call = nxt_call(in_helper)
            inner = [sc.pick([f"v = {call}", f"return {call}", call])]
        body = ["v = None"]
        for _ in range(sc.noise()):
            body += sc.prelude()
        body += sc.wrap(inner)
        body += ["return v"]
        form = sc.pick(["plain"] * 12 + ["cached", "decorated", "twice", "static", "property", "builtin"])
        lines, call_of = _define_hop(sc, name, params, args, body, form, in_helper)
        sc.define(lines, helper=in_helper)
        nxt_call, nxt_in_helper = call_of, in_helper
    return nxt_call(False) if nxt_call is not None else None


def _common_import(sc):
    return f"from {sc.common} import " + ", ".join(sc.common_names())


def _define_hop(sc, name, params, args, body, form, in_helper):
    """The text of a helper and a function giving the expression that calls it from a module."""

    def ref(from_helper):
        return sc.ref(name, from_helper, in_helper)

    if form == "cached":
        sc.imports("import functools", helper=in_helper)
        lines = ["@functools.lru_cache(maxsize=None)"] + block(f"def {name}({params}):", body)
        return lines, lambda h: f"{ref(h)}({args})"
    if form == "decorated":
        deco = sc.name("logged")
        lines = block(
            f"def {deco}(fn):",
            block("def inner(*a, **k):", block("if a and a[0]:", ["return None"]) + ["return fn(*a, **k)"])
            + ["return inner"],
        )
        lines += ["", "", f"@{deco}"] + block(f"def {name}({params}):", body)
        return lines, lambda h: f"{ref(h)}({args})"
    if form == "twice":
        first = block(f"def {name}({params}):", ["return None"])
        lines = first + ["", ""] + block(f"def {name}({params}):", body)
        return lines, lambda h: f"{ref(h)}({args})"
    if form == "static":
        klass = sc.name("Steps")
        kind = sc.pick(["staticmethod", "classmethod"])
        sig = params if kind == "staticmethod" else f"cls, {params}"
        lines = block(f"class {klass}:", [f"@{kind}"] + block(f"def run({sig}):", body))
        return lines, lambda h: f"{sc.ref(klass, h, in_helper)}.run({args})"
    if form == "property":
        klass = sc.name("Holder")
        names = [p.strip().split(":")[0] for p in params.split(",")]
        init = block(f"def __init__(self, {params}):", [f"self.{p} = {p}" for p in names])
        getter = ["@property"] + block("def data(self):", [f"{p} = self.{p}" for p in names] + body)
        lines = block(f"class {klass}:", init + [""] + getter)
        return lines, lambda h: f"{sc.ref(klass, h, in_helper)}({args}).data"
    if form == "builtin":
        lines = block(f"def {name}({params}):", body)
        style = sc.pick(["map", "sorted", "lambda_map"])
        if style == "map":
            return lines, lambda h: f"list(map(lambda _v: {ref(h)}({args}), [1]))[0]"
        if style == "sorted":
            return lines, lambda h: f"sorted([1, 2], key=lambda _v: ({ref(h)}({args}), 0)[1])"
        return lines, lambda h: f"[{ref(h)}({args}) for _v in [1]][0]"
    lines = block(f"def {name}({params}):", body)
    return lines, lambda h: f"{ref(h)}({args})"


# ---------------------------------------------------------------------------------------------
# EFF101
# ---------------------------------------------------------------------------------------------
def eff101_leaf(sc):
    """A library call that may wait, with its imports and the module constants it reads; those
    are defined in the module of the call, where heki can evaluate them. A constant imported from
    another module always holds a value that waits (R6)."""
    ns = sc.ns
    pause, page, addr, dbname = sc.name("PAUSE"), sc.name("PAGE"), sc.name("ADDR"), sc.name("DBNAME")
    options = [
        (["time.sleep(0.01)"], ["import time"], []),
        (["time.sleep(0)"], ["import time"], []),
        ([f"time.sleep({pause})"], ["import time"], [f"{pause} = {sc.pick(['0', '0.0', '0.2'])}"]),
        ([f"time.sleep(DELAY_{ns})"], ["import time"], []),
        (["time.sleep(n)"], ["import time"], []),
        ([f"time.sleep(pick_delay_{ns}())"], ["import time"], []),
        ([f"requests.get(URL_{ns})"], ["import requests"], []),
        ([f'requests.post(URL_{ns} + "/" + key, json={{"n": n}})'], ["import requests"], []),
        ([f'sqlite3.connect(DB_FILE_{ns}).execute("SELECT 1")'], ["import sqlite3"], []),
        (['sqlite3.connect(":memory:").execute("SELECT 1")'], ["import sqlite3"], []),
        (
            [f'sqlite3.connect({dbname}).execute("SELECT 1")'],
            ["import sqlite3"],
            [f'{dbname} = "{sc.pick([":memory:", "file::memory:", "file:x?mode=memory", "local.db"])}"'],
        ),
        ([f"sqlite3.connect(DB_FILE_{ns}).commit()"], ["import sqlite3"], []),
        (['subprocess.run(["true"], check=False)'], ["import subprocess"], []),
        ([f"socket.getaddrinfo(HOST_{ns}, 80)"], ["import socket"], []),
        (
            [sc.pick([f"socket.getaddrinfo({addr}, 80)", f"socket.gethostbyname({addr})"])],
            ["import socket"],
            [f'{addr} = "{sc.pick(["127.0.0.1", "::1", "localhost", "example.org"])}"'],
        ),
        (["socket.gethostbyname(key)"], ["import socket"], []),
        ([f"urllib.request.urlopen(URL_{ns})"], ["import urllib.request"], []),
        (
            [f"urllib.request.urlopen({page})"],
            ["import urllib.request"],
            [f'{page} = "{sc.pick(["data:,x", "http://example.com/p"])}"'],
        ),
        ([f"Q_{ns}.put(1)", f"Q_{ns}.get()"], ["import queue"], []),
    ]
    lines, imports, consts = sc.pick(options)
    if len(lines) == 1:
        lines = [f"v = {lines[0]}"]
    return lines, imports, consts


def _waiting_body(sc, name, params):
    """A method `name` that reaches a library call through helpers, wrapped like any call."""
    leaf, imports, consts = eff101_leaf(sc)
    for imp in imports:
        sc.imports(imp)
    chain = build_chain(sc, sc.rng.randint(0, 2), leaf, params, CONTROLS, imports, consts)
    if chain:
        inner = [f"v = {chain}"]
    else:
        inner = leaf
        if consts:
            sc.define(list(consts))
    return block(f"def {name}(self, {params}):", ["v = None", *sc.wrap(inner), "return v"])


def _dispatch(sc, params):
    """Classes a receiver can be, by how the receiver is typed: a base class and its subclasses
    (also abstract), a union of unrelated classes, a protocol and classes that only match it, or
    a mixin whose method calls a method each subclass defines. Returns the classes defined, the
    receiver's annotation, and the classes whose instances the inputs pass."""
    style = sc.pick(["base", "base", "abstract", "union", "protocol", "mixin"])
    impls = [sc.name("Impl") for _ in range(sc.rng.randint(1, 3))]
    if style in ("base", "abstract"):
        base = sc.name("Base")
        if style == "abstract":
            sc.imports("import abc")
            head = block(
                f"class {base}(abc.ABC):",
                ["@abc.abstractmethod"] + block(f"def run(self, {params}):", ["raise NotImplementedError"]),
            )
        elif sc.chance(0.4):
            head = block(f"class {base}:", ["kind = 0"])
        else:
            head = block(f"class {base}:", _waiting_body(sc, "run", params))
        sc.define(head)
        for impl in impls:
            overrides = style == "abstract" or sc.chance(0.85)
            sc.define(
                block(
                    f"class {impl}({base}):", _waiting_body(sc, "run", params) if overrides else ["kind = 1"]
                )
            )
        objects = impls + ([] if style == "abstract" else [base])
        return base, objects
    if style == "union":
        for impl in impls:
            sc.define(block(f"class {impl}:", _waiting_body(sc, "run", params)))
        return " | ".join(impls), impls
    if style == "protocol":
        proto = sc.name("Runner")
        sc.imports("from typing import Protocol")
        sc.define(block(f"class {proto}(Protocol):", block(f"def run(self, {params}):", ["..."])))
        for impl in impls:
            sc.define(block(f"class {impl}:", _waiting_body(sc, "run", params)))
        return proto, impls
    mixin = sc.name("Mixin")
    sc.define(block(f"class {mixin}:", block(f"def run(self, {params}):", [f"return self.leaf({CONTROLS})"])))
    for impl in impls:
        sc.define(block(f"class {impl}({mixin}):", _waiting_body(sc, "leaf", params)))
    return mixin, impls


def gen_eff101(sc):
    ns = sc.ns
    params = sc.params
    entry = sc.name("handle")
    if sc.chance(0.35):
        annotation, objects = _dispatch(sc, params)
        call = f"obj.run({CONTROLS})"
        signature = f"obj: {annotation}, {params}"
    else:
        leaf, imports, consts = eff101_leaf(sc)
        for imp in imports:
            sc.imports(imp)
        depth = sc.rng.randint(0, 3)
        call = build_chain(sc, depth, leaf, params, CONTROLS, imports, consts)
        if call is None and consts:
            sc.define(list(consts))
        signature = params
        objects = None
    sc.imports("import contextlib")
    sc.imports(_common_import(sc))
    sc.imports("import queue")
    body = ["v = None"]
    for _ in range(sc.noise()):
        body += sc.prelude()
    if call is None:
        stmt = leaf
    else:
        stmt = [sc.pick([f"v = {call}", f"return {call}", call])]
    body += sc.wrap(stmt)
    body += ["return v"]
    sc.define([f"Q_{ns} = queue.Queue()", "", ""] + block(f"async def {entry}({signature}):", body))
    inputs = []
    for controls in sc.control_inputs(8):
        if objects:
            for klass in objects:
                inputs.append({"args": [{"$obj": [f"{sc.main_module}:{klass}", [], {}]}, *controls]})
        else:
            inputs.append({"args": controls})
    sc.entries.append({"module": sc.main_module, "func": entry, "inputs": inputs[:24]})


# ---------------------------------------------------------------------------------------------
# EFF103
# ---------------------------------------------------------------------------------------------
def gen_eff103(sc):
    ns = sc.ns
    entry = sc.name("process")
    db = sc.pick(["cursor", "conn", "local_file", "local_memory", "pg"])
    if db == "cursor":
        sc.imports("import sqlite3")
        db_param, db_value, prep = "db: sqlite3.Cursor", {"$cursor": "data.db"}, []
    elif db == "conn":
        sc.imports("import sqlite3")
        db_param, db_value, prep = "db: sqlite3.Connection", {"$conn": "data.db"}, []
    elif db == "pg":
        sc.imports("import psycopg2.extensions")
        sc.imports("import psycopg2")
        db_param, db_value, prep = "db: psycopg2.extensions.cursor", {"$pgcursor": "dbname=x"}, []
    else:
        sc.imports("import sqlite3")
        target = f"DB_FILE_{ns}" if db == "local_file" else '":memory:"'
        db_param, db_value, prep = "unused", None, [f"db = sqlite3.connect({target})"]
    statement = sc.pick(
        [
            f"db.execute(SQL_{ns}, (x, n))",
            f"db.execute(SQL_{ns}, (x, n))",
            f"db.execute(SQL_{ns}, (x, n))",
            f"db.executemany(SQL_{ns}, [(x, n)])",
            "db.commit()" if db not in ("cursor", "pg") else "db.fetchone()",
        ]
    )
    leaf = [f"v = {statement}"]
    chain_params = f"db, x, {sc.params}"
    depth = sc.rng.choice([0, 0, 1, 1, 2])
    call = build_chain(sc, depth, leaf, chain_params, f"db, x, {CONTROLS}", ["import sqlite3"])
    origin, target, extra_params, extra_values = _origin(sc)
    body = []
    for _ in range(sc.noise()):
        body += sc.prelude(in_loop=True)
    stmt = [f"v = {call}"] if call else leaf
    if sc.chance(0.15):
        stmt = block(f"for _j in {sc.pick(['range(2)', '[1]', 'range(n)', 'items'])}:", stmt)
    body += sc.wrap(stmt, in_loop=True)
    body += ["out.append(x)"]
    if sc.chance(0.2):
        body += block('if x == "stop":', ["break"])
    sc.imports("import contextlib")
    sc.imports(_common_import(sc))
    sig = ", ".join(p for p in [db_param, "items: list[str]", *extra_params, sc.params] if p)
    fn = ["v = None", "out = []", *prep] + block(f"for {target} in {origin}:", body) + ["return out"]
    sc.define(block(f"def {entry}({sig}):", fn))
    inputs = []
    item_lists = [
        ["1", "2", "3"],
        ["1", "skip", "2"],
        ["x", "1"],
        ["1", "bad", "2"],
        ["stop", "1"],
        [],
        ["4"],
    ]
    for k, controls in enumerate(sc.control_inputs(8)):
        items = item_lists[k % len(item_lists)]
        args = [db_value, list(items), *[value(items) for value in extra_values], *controls]
        inputs.append({"args": args, "rows": 1 + k % 3})
    sc.entries.append({"module": sc.main_module, "func": entry, "inputs": inputs})


def _origin(sc):
    """What a loop steps through: (expression, target, extra parameters, their values)."""
    ns = sc.ns
    choice = sc.pick(
        ["param"] * 4
        + [
            "sorted",
            "list",
            "slice",
            "enumerate",
            "zip",
            "attr",
            "dict_items",
            "rows",
            "lines",
            "range",
            "display",
            "map",
        ]
    )
    if choice == "param":
        return "items", "x", [], []
    if choice == "sorted":
        return "sorted(items)", "x", [], []
    if choice == "list":
        return "list(items)", "x", [], []
    if choice == "slice":
        return "items[1:]", "x", [], []
    if choice == "enumerate":
        return "enumerate(items)", "i, x", [], []
    if choice == "zip":
        return "zip(items, items)", "x, _y", [], []
    if choice == "attr":
        klass = sc.name("Box")
        sc.define(block(f"class {klass}:", block("def __init__(self, items):", ["self.items = items"])))
        return (
            "box.items",
            "x",
            [f"box: {klass}"],
            [lambda items: {"$obj": [f"{sc.main_module}:{klass}", [list(items)], {}]}],
        )
    if choice == "dict_items":
        return (
            "mapping.items()",
            "x, _y",
            ["mapping: dict[str, int]"],
            [lambda items: {"$dict": [[i, 1] for i in items]}],
        )
    if choice == "rows":
        sc.imports("import sqlite3")
        return (
            'src.execute("SELECT k FROM t").fetchall()',
            "x",
            ["src: sqlite3.Cursor"],
            [lambda items: {"$cursor": ":memory:"}],
        )
    if choice == "lines":
        return "open(path)", "x", ["path: str"], [lambda items: {"$path": f"data_{ns}.txt"}]
    if choice == "range":
        return "range(3)", "x", [], []
    if choice == "display":
        return '["1", "2"]', "x", [], []
    return "map(str, items)", "x", [], []


# ---------------------------------------------------------------------------------------------
# DATA101
# ---------------------------------------------------------------------------------------------
def gen_data101(sc):
    entry = sc.name("build")
    frame = sc.chance(0.3)
    sc.imports("import pandas as pd" if frame else "import numpy as np")
    start = "pd.DataFrame()" if frame else sc.pick(["np.array([])", "np.empty(0)", "np.array([0.0])"])
    piece = "pd.DataFrame([c])" if frame else sc.pick(["c", "[c]", "int(c)", "[float(c)]"])
    if frame:
        update = sc.pick(
            [f"pd.concat([acc, {piece}])", f"pd.concat([acc] + [{piece}])", f"pd.concat([other, {piece}])"]
        )
    else:
        update = sc.pick(
            [
                f"np.append(acc, {piece})",
                f"np.concatenate((acc, np.array([{piece}])))",
                f"np.vstack([acc, np.array([{piece}])])",
                f"np.hstack([acc, np.array([{piece}])])",
                f"np.append(other, {piece})",
                f"np.concatenate((acc * 1, np.array([{piece}])))",
                f"np.concatenate((acc, np.array([{piece}])), out=acc)",
            ]
        )
    if sc.chance(0.3):
        grow = sc.name("grow")
        in_helper = sc.chance(0.5)
        sc.imports("import pandas as pd" if frame else "import numpy as np", helper=in_helper)
        sc.imports(_common_import(sc), helper=in_helper)
        helper_update = update.replace("acc", "arr").replace("other", "arr")
        hbody = ["return " + helper_update]
        if sc.chance(0.3):
            hbody = block("if flag:", ["return arr"]) + hbody
        sc.define(block(f"def {grow}(arr, c, flag):", hbody), helper=in_helper)
        update = f"{sc.ref(grow, False, in_helper)}(acc, c, flag)"
    stmt = [f"acc = {update}"]
    body = []
    for _ in range(sc.noise()):
        body += sc.prelude(in_loop=True)
    body += sc.wrap(stmt, in_loop=True)
    if sc.chance(0.2):
        body += block('if c == "0":', [f"acc = {start}"])
    origin, target, extra_params, extra_values = _origin(sc)
    if target != "x":
        target = target.replace("x", "c", 1)
    else:
        target = "c"
    sc.imports("import contextlib")
    sc.imports(_common_import(sc))
    sig = ", ".join(["items: list[str]", *extra_params, sc.params])
    body = [stmt.replace("x ==", "c ==") for stmt in body]
    fn = (
        [f"acc = {start}", f"other = {start}", "v = None"]
        + block(f"for {target} in {origin}:", body)
        + ["return acc"]
    )
    sc.define(block(f"def {entry}({sig}):", fn))
    inputs = []
    item_lists = [["1", "2", "3"], ["1", "skip", "2"], ["x", "1"], ["0", "1"], ["stop", "1"], [], ["4", "5"]]
    for k, controls in enumerate(sc.control_inputs(8)):
        items = item_lists[k % len(item_lists)]
        inputs.append({"args": [list(items), *[value(items) for value in extra_values], *controls]})
    sc.entries.append({"module": sc.main_module, "func": entry, "inputs": inputs})


# ---------------------------------------------------------------------------------------------
# MUT110
# ---------------------------------------------------------------------------------------------
def gen_mut110(sc):
    ns = sc.ns
    mapping = sc.chance(0.3)
    sc.frozen = sc.chance(0.3)
    sc.imports("from collections.abc import Sequence, Mapping")
    sc.imports("from typing import Optional")
    if mapping:
        annotation = sc.pick(["Mapping[str, int]", "Optional[Mapping[str, int]]", '"Mapping[str, int]"'])
        change = sc.pick(
            ['xs["k"] = n', 'xs.update({"k": n})', 'xs.pop("b", None)', 'xs.setdefault("z", n)', "xs.clear()"]
        )
        guard = "isinstance(xs, dict)"
    else:
        annotation = sc.pick(
            ["Sequence[int]", "Optional[Sequence[int]]", '"Sequence[int]"', "Sequence[int] | None"]
        )
        change = sc.pick(
            [
                "xs.append(n)",
                "xs.sort()",
                "xs.extend([n])",
                "xs.insert(0, n)",
                "xs += [n]",
                "xs[0] = n",
                "del xs[0]",
                "xs.reverse()",
                "xs.clear()",
            ]
        )
        guard = "isinstance(xs, list)"
    last = [*block(f"if {guard}:", [change])] if sc.chance(0.8) else [change]
    depth = sc.rng.randint(1, 3)
    names = [sc.name("mut") for _ in range(depth)]
    sides = [False] * depth
    for level in reversed(range(depth)):
        sides[level] = sc.chance(0.5) and (level + 1 == depth or sides[level + 1])
    for level in range(depth):
        name, in_helper = names[level], sides[level]
        sc.imports(_common_import(sc), helper=in_helper)
        sc.imports("import contextlib", helper=in_helper)
        # CPython before 3.14 evaluates parameter annotations when the function is defined.
        sc.imports("from collections.abc import Sequence, Mapping", helper=in_helper)
        sc.imports("from typing import Optional", helper=in_helper)
        body = []
        for _ in range(sc.noise(1)):
            body += sc.prelude()
        if sc.chance(0.15):
            body += [sc.pick(["xs = list(xs)", "xs = dict(xs) if isinstance(xs, dict) else list(xs)"])]
        elif sc.chance(0.15):
            body += block("if flag:", ["xs = list(xs) if not isinstance(xs, dict) else dict(xs)"])
        if level + 1 < depth:
            nxt = sc.ref(names[level + 1], in_helper, sides[level + 1])
            inner = [f"{nxt}(xs, {CONTROLS})"]
            if sc.chance(0.2):
                inner.append(f"{nxt}([] if not isinstance(xs, dict) else {{}}, {CONTROLS})")
        else:
            inner = last
        body += sc.wrap(inner)
        param = sc.pick(
            ["xs", "xs", "xs", "xs: list[int]" if not mapping else "xs: dict[str, int]", f"xs: {annotation}"]
        )
        generator = level + 1 == depth and sc.chance(0.1)
        if generator:
            body += ["yield n"]
        sc.define(block(f"def {name}({param}, {sc.params}):", body or ["pass"]), helper=in_helper)
    entry = sc.name("entry")
    first = sc.ref(names[0], False, sides[0])
    passed = sc.pick(
        [
            "values",
            "values",
            "alias",
            "list(values) if values is not None else []",
            "alias if flag else values",
        ]
    )
    pre = ["alias = values"]
    body = list(pre)
    for _ in range(sc.noise(1)):
        body += sc.prelude()
    call = f"{first}({passed}, {CONTROLS})"
    if depth and sc.chance(0.1):
        call = f"list({call})"
    body += sc.wrap([call])
    body += ["return None if values is None else len(values)"]
    sc.imports("import contextlib")
    sc.imports(_common_import(sc))
    sc.define(block(f"def {entry}(values: {annotation}, {sc.params}):", body))
    inputs = []
    for k, controls in enumerate(sc.control_inputs(8)):
        if mapping:
            value = {"$dict": [["b", 2], ["a", 1]]} if k % 4 else None
            if sc.frozen and k % 3 == 2:
                value = {"$obj": [f"{sc.common}:FrozenDict_{ns}", [{"$dict": [["b", 2]]}], {}]}
        else:
            value = [3, 1, 2] if k % 5 else {"$tuple": [3, 1, 2]}
            if sc.frozen and k % 3 == 2:
                value = {"$obj": [f"{sc.common}:Frozen_{ns}", [[3, 1, 2]], {}]}
        inputs.append({"args": [value, *controls]})
    sc.entries.append({"module": sc.main_module, "func": entry, "inputs": inputs})


# ---------------------------------------------------------------------------------------------
# MUT114
# ---------------------------------------------------------------------------------------------
def gen_mut114(sc):
    ns = sc.ns
    kind = sc.pick(["list", "dict", "set"])
    deco_import, deco = sc.pick(
        [
            ("import functools", "@functools.lru_cache(maxsize=None)"),
            ("import functools", "@functools.lru_cache(maxsize=32)"),
            ("import functools", "@functools.cache"),
            ("from functools import lru_cache", "@lru_cache"),
            ("from functools import lru_cache as memo", "@memo(maxsize=8)"),
            ("import functools", "@functools.lru_cache(maxsize=0)"),
            ("", f"@cache_{ns}"),
        ]
    )
    if deco_import:
        sc.imports(deco_import)
    else:
        sc.define(block(f"def cache_{ns}(fn):", ["return fn"]))
    cached = sc.name("table")
    build = {
        "list": sc.pick(['[name, "id"]', 'sorted([name, "id"])', 'name.split(",")', 'list((name, "id"))']),
        "dict": sc.pick(['{"name": name}', "dict(name=name)", "{k: 1 for k in name}"]),
        "set": sc.pick(['{name, "id"}', "set(name)"]),
    }[kind]
    annotation = sc.pick([f"-> {kind}", f"-> {kind}[str]" if kind != "dict" else "-> dict[str, object]", ""])
    if annotation and sc.chance(0.3):
        sc.frozen = True
        frozen = {"list": f"Frozen_{ns}", "dict": f"FrozenDict_{ns}", "set": f"FrozenSet_{ns}"}[kind]
        build = f"{frozen}({build})"
    keyed = sc.chance(0.85)
    params = "name" if keyed else ""
    body = [f"return {build}"] if keyed else ['name = "fixed"', f"return {build}"]
    if sc.chance(0.15):
        body = block("if not name:", ["return None"]) + body if keyed else body
    owner = sc.pick(["function", "function", "static"])
    if owner == "static":
        klass = sc.name("Tables")
        sc.define(
            block(
                f"class {klass}:",
                ["@staticmethod", deco]
                + block(f"def get({params}) {annotation}:".replace(") :", "):"), body),
            )
        )
        call = f"{klass}.get({'name' if keyed else ''})"
    else:
        sc.define([deco] + block(f"def {cached}({params}) {annotation}:".replace(") :", "):"), body))
        call = f"{cached}({'name' if keyed else ''})"
    mutation = {
        "list": sc.pick(
            [
                'result.append("x")',
                'result.extend(["x"])',
                'result += ["x"]',
                'result[0] = "x"',
                "result.pop()",
                "result.sort()",
            ]
        ),
        "dict": sc.pick(
            [
                'result["x"] = 1',
                "result.update(x=1)",
                'result.pop("name", None)',
                'result |= {"x": 1}',
                'result.setdefault("x", 1)',
            ]
        ),
        "set": sc.pick(
            ['result.add("x")', 'result.discard("id")', 'result |= {"x"}', 'result.update({"x"})']
        ),
    }[kind]
    if sc.chance(0.1):
        mutation = sc.pick(['result.append("x")', 'result["x"] = 1', 'result.add("x")'])
    source = sc.pick([call, call, call, f"{kind}({call})", f"{call} if flag else {kind}()"])
    entry = sc.name("use")
    body = [f"result = {source}"]
    for _ in range(sc.noise(1)):
        body += sc.prelude()
    if sc.chance(0.15):
        body += block("if flag:", [f"result = {kind}()"])
    body += sc.wrap([mutation])
    body += ["return result"]
    sc.imports("import contextlib")
    sc.imports(_common_import(sc))
    sc.define(block(f"def {entry}(name, {sc.params}):", body))
    inputs = []
    for controls in sc.control_inputs(8):
        inputs.append({"args": [sc.pick(["a", "b", "a"]), *controls]})
    sc.entries.append({"module": sc.main_module, "func": entry, "inputs": inputs})


# ---------------------------------------------------------------------------------------------
# OPT rewrites
# ---------------------------------------------------------------------------------------------
# Per rule: (setup, pattern lines, results the function may return).
OPT_PATTERNS = {
    "OPT101": [
        ("x = []", ["x = x + [i]"]),
        ("x = []", ["x = x + [i, i]"]),
        ("x = []", ["x = x + [probe(i)]"]),
        ("x = [0]", ["x = x + [i]"]),
    ],
    "OPT102d": [
        ("x = {}", ["x = {**x, i: n}"]),
        ("x = {}", ["x = {**x, **{i: 1}}"]),
        ("x = {}", ["x = x | {i: 1}"]),
        ("x = {}", ["x = {**x, repr(i): probe(i)}"]),
    ],
    "OPT102s": [("x = set()", ["x = x | {i}"]), ("x = set()", ["x = x | {repr(i)}"])],
    "OPT103b": [('x = b""', ["x = x + str(i).encode()"]), ('x = b""', ['x += b"-"'])],
    "OPT103t": [("x = ()", ["x = x + (i,)"])],
    "OPT110": [
        ("x = [str(v) for v in other]", ["hits += 1 if str(i) in x else 0"]),
        ("x = list(range(n * 7))", ["hits += 1 if len(repr(i)) in x else 0"]),
        ("x = list(other)", ["hits += 1 if i in x else 0"]),
    ],
    "OPT111": [
        ("x = []", ["k = str(i)"] + block("if k not in x:", ["x.append(k)"])),
        ("x = []", block("if i not in x:", ["x.append(i)"])),
    ],
    "OPT120": [
        ("x = list(items)", ["first = x.pop(0)"]),
        ("x = [str(v) for v in items]", ["del x[0]"]),
        ("x = list(items)", ["x = x[1:]"]),
    ],
    "OPT121": [("x = []", ["x.insert(0, i)"]), ("x = []", ["x.insert(0, str(i))"])],
    "OPT130": [
        ("x = [str(v) for v in other]", ["hits += 1 if str(i) in set(x) else 0"]),
        ("x = list(other)", ["hits += 1 if i in set(x) else 0"]),
        ("x = list(other)", ["hits += 1 if i in list(x) else 0"]),
    ],
}
OPT_RESULTS = {
    "OPT101": ["x", "len(x)", "(x, hits)", "sorted(x, key=repr)"],
    "OPT102d": ["x", "len(x)", "list(x)"],
    "OPT102s": ["len(x)", "sorted(x, key=repr)", "x", "list(x)", "repr(i) in x if items else None"],
    "OPT103b": ["x", "len(x)"],
    "OPT103t": ["x", "len(x)"],
    "OPT110": ["hits", "(hits, len(x))"],
    "OPT111": ["len(x)", "x", "(len(x), x[:1])"],
    "OPT120": ["len(x)", "hits", "x"],
    "OPT121": ["len(x)", "x[0] if x else None", "list(x)"],
    "OPT130": ["hits", "(hits, len(x))"],
}


def gen_opt(sc):
    ns = sc.ns
    rule = sc.pick(list(OPT_PATTERNS))
    init, pattern = sc.pick(OPT_PATTERNS[rule])
    pattern = list(pattern)
    entry = sc.name("opt")
    probe = sc.name("probe")
    sink = sc.name("sink")
    store = f"KEPT_{ns}"
    sc.define([f"{store} = []"])
    sc.define(block(f"def {probe}(i):", ["return i"]))
    sc.define(block(f"def {sink}(value):", [f"{store}.append(value)", "return len(value)"]))
    pattern = [line.replace("probe(", f"{probe}(") for line in pattern]
    before, inside_before, inside_after, after = [], [], [], []
    result = sc.pick(OPT_RESULTS[rule])
    contexts = sc.rng.sample(
        [
            "alias",
            "alias_in_loop",
            "iterate_self",
            "iterator",
            "handler",
            "closure",
            "sink",
            "store",
            "set_order",
            "deque_uses",
            "mutate_other",
            "raising_element",
            "annotation",
            "eval_order",
            "while_loop",
            "nested",
            "none",
            "none",
            "none",
        ],
        k=sc.rng.randint(1, 3),
    )
    loop_iter = "items"
    loop_kind = "for"
    for ctx in contexts:
        if ctx == "alias":
            before.append("y = x")
            result = "(x, y)" if "hits" not in result else result
        elif ctx == "alias_in_loop":
            inside_after.append("y = x")
        elif ctx == "iterate_self" and rule in ("OPT101", "OPT102s", "OPT103t"):
            init = {
                "OPT101": "x = list(items)",
                "OPT102s": "x = set(map(repr, items))",
                "OPT103t": "x = tuple(items)",
            }[rule]
            loop_iter = "x"
        elif ctx == "iterator":
            before.append("it = iter(x) if x is not None else iter(())")
            after.append("rest = list(it)")
            result = "(x, rest)"
        elif ctx == "handler":
            inside_before.append("try:")
            inside_before.append("    check = int(i) if isinstance(i, str) else i")
            inside_before.append("except ValueError:")
            inside_before.append('    return ("failed", x)')
        elif ctx == "closure":
            before += block("def peek():", ["return len(x) if x is not None else 0"])
            inside_after.append("seen_len = peek()")
        elif ctx == "sink":
            inside_after.append(f"{sink}(x) if x is not None else None")
            result = f"({result}, [repr(k) for k in {store}])"
        elif ctx == "store":
            inside_after.append("cfg.saved = x")
        elif ctx == "set_order":
            result = "list(x) if x is not None else []"
        elif ctx == "deque_uses" and rule in ("OPT120", "OPT121"):
            after.append(
                sc.pick(
                    [
                        "peek2 = x[1] if len(x) > 1 else None",
                        'text_x = f"{x}"',
                        "sorted_x = sorted(x, key=repr)",
                        "same = x == [1]",
                    ]
                )
            )
            result = sc.pick(["(len(x), " + after[-1].split(" = ")[0] + ")", "x"])
        elif ctx == "mutate_other" and rule in ("OPT110", "OPT130"):
            inside_after += block("if i == 3 and 3 not in other:", ["other.append(3)"])
        elif ctx == "raising_element":
            inside_before.append("v = 10 // (i if isinstance(i, int) else 1) if flag else 0")
        elif ctx == "annotation" and rule in ("OPT101", "OPT111", "OPT120", "OPT121"):
            init = (
                init.replace("x = ", "x: list[int] = ")
                if sc.chance(0.7)
                else init.replace("x = ", "x: Sequence[int] = ")
            )
            sc.imports("from collections.abc import Sequence")
        elif ctx == "eval_order" and rule == "OPT101":
            pattern = [f"x = x + [{sink}(x)]"]
            result = f"({result}, [repr(k) for k in {store}])"
        elif ctx == "while_loop" and rule in ("OPT120",):
            loop_kind = "while"
        elif ctx == "nested":
            loop_kind = "nested"
    if rule == "OPT120" and loop_kind != "while" and sc.chance(0.6):
        loop_kind = "while"
    body_inner = inside_before + pattern + inside_after
    if rule == "OPT120" and loop_kind == "while":
        loop = block("while x:", body_inner + ["i = len(x)"])
    elif loop_kind == "nested":
        loop = block(f"for row in [{loop_iter}]:", block("for i in row:", body_inner))
    else:
        loop = block(f"for i in {loop_iter}:", body_inner)
    fn = (
        [f"{store}.clear()", "hits = 0", "seen_len = 0", "rest = None", init]
        + before
        + loop
        + after
        + [f"return {result}"]
    )
    sc.imports(_common_import(sc))
    sc.define(block(f"def {entry}(items, other, {sc.params}):", fn))
    weird = sc.name("Weird")
    sc.define(
        block(
            f"class {weird}:",
            block("def __eq__(self, other):", ["return True"])
            + [""]
            + block("def __hash__(self):", ["return 1"])
            + [""]
            + block("def __repr__(self):", ['return "W"']),
        )
    )
    item_lists = [
        [1, 2, 2, 3],
        [3, 1, 3],
        [],
        ["a", "b", "a"],
        [[1], [2]],
        [1, {"$obj": [f"{sc.main_module}:{weird}", [], {}]}],
        list(range(20)),
        ["1", "x", "2"],
    ]
    inputs = []
    for k, controls in enumerate(sc.control_inputs(8)):
        items = item_lists[k % len(item_lists)]
        other = item_lists[(k * 3 + 1) % len(item_lists)]
        inputs.append({"args": [items, other, *controls]})
    sc.entries.append({"module": sc.main_module, "func": entry, "inputs": inputs})


# ---------------------------------------------------------------------------------------------
# OPT113
# ---------------------------------------------------------------------------------------------
def gen_opt113(sc):
    ns = sc.ns
    left, right = sc.name("Left"), sc.name("Right")
    style = sc.pick(
        [
            "dataclass",
            "dataclass",
            "plain",
            "slots",
            "property",
            "getattr",
            "namedtuple",
            "dicts",
            "tuples",
            "subclass_property",
        ]
    )
    entry = sc.name("join")
    if style in ("dataclass", "property", "getattr", "subclass_property"):
        sc.imports("from dataclasses import dataclass")
        sc.define(
            ["@dataclass"] + block(f"class {left}:", ["key: int", "other: int = 0", "matched: object = None"])
        )
        if style == "property":
            sc.define(
                ["@dataclass"]
                + block(
                    f"class {right}:",
                    ["raw: int", "payload: int = 0", ""]
                    + ["@property"]
                    + block("def key(self):", ["return self.raw"]),
                )
            )
            make_right = lambda k: {"$obj": [f"{sc.main_module}:{right}", [k, k * 10], {}]}
        elif style == "getattr":
            sc.define(
                ["@dataclass"]
                + block(
                    f"class {right}:",
                    ["raw: int", "payload: int = 0", ""]
                    + block("def __getattr__(self, name):", ["return self.raw"]),
                )
            )
            make_right = lambda k: {"$obj": [f"{sc.main_module}:{right}", [k, k * 10], {}]}
        else:
            sc.define(["@dataclass"] + block(f"class {right}:", ["key: int", "payload: int = 0"]))
            make_right = lambda k: {"$obj": [f"{sc.main_module}:{right}", [k, k * 10], {}]}
            if style == "subclass_property":
                sub = sc.name("Shadow")
                sc.define(
                    block(
                        f"class {sub}({right}):",
                        ["@property"]
                        + block("def key(self):", ["return self._key + 0"])
                        + ["", "@key.setter"]
                        + block("def key(self, value):", ["self._key = value"]),
                    )
                )
                base_make = make_right
                make_right = lambda k, base_make=base_make, sub=sub: (
                    {"$obj": [f"{sc.main_module}:{sub}", [k, k * 10], {}]} if k % 2 else base_make(k)
                )
        make_left = lambda k: {"$obj": [f"{sc.main_module}:{left}", [k], {}]}
        key_left, key_right = "a.key", "b.key"
        ann_left, ann_right = f"list[{left}]", f"list[{right}]"
    elif style in ("plain", "slots"):
        slots = ['__slots__ = ("key", "payload", "other", "matched")'] if style == "slots" else []
        init = block(
            "def __init__(self, key, payload=0):",
            ["self.key = key", "self.payload = payload", "self.other = 0", "self.matched = None"],
        )
        sc.define(block(f"class {left}:", slots + init))
        sc.define(block(f"class {right}:", slots + init))
        make_left = lambda k: {"$obj": [f"{sc.main_module}:{left}", [k], {}]}
        make_right = lambda k: {"$obj": [f"{sc.main_module}:{right}", [k, k * 10], {}]}
        key_left, key_right = "a.key", "b.key"
        ann_left, ann_right = f"list[{left}]", f"list[{right}]"
    elif style == "namedtuple":
        sc.imports("from typing import NamedTuple")
        sc.define(block(f"class {left}(NamedTuple):", ["key: int", "other: int = 0"]))
        sc.define(block(f"class {right}(NamedTuple):", ["key: int", "payload: int = 0"]))
        make_left = lambda k: {"$obj": [f"{sc.main_module}:{left}", [k], {}]}
        make_right = lambda k: {"$obj": [f"{sc.main_module}:{right}", [k, k * 10], {}]}
        key_left, key_right = "a.key", "b.key"
        ann_left, ann_right = f"list[{left}]", f"list[{right}]"
    elif style == "dicts":
        make_left = lambda k: {"$dict": [["k", k], ["other", 0]]}
        make_right = lambda k: {"$dict": [["k", k], ["payload", k * 10]]}
        key_left, key_right = 'a["k"]', 'b["k"]'
        ann_left, ann_right = "list[dict]", "list[dict]"
    else:
        make_left = lambda k: {"$tuple": [k, 0]}
        make_right = lambda k: {"$tuple": [k, k * 10]}
        key_left, key_right = "a[0]", "b[0]"
        ann_left, ann_right = "list[tuple]", "list[tuple]"
    sc.rows = sc.chance(0.25)
    inner_iter = sc.pick(
        ["rights"] * 4
        + [
            "sorted(rights, key=repr)",
            "rights[:]",
            f"gen_{ns}(rights)",
            "list(rights)",
            "tuple(rights)",
            "iter(rights)",
        ]
    )
    if inner_iter.startswith("gen_"):
        sc.define(block(f"def gen_{ns}(rows):", block("for row in rows:", ["yield row"])))
    test = f"{key_left} == {key_right}"
    variant = sc.pick(
        ["append"] * 3
        + ["break", "and_first", "else", "before", "after", "change_key", "grow", "count_after"]
    )
    pre, post, orelse, outer_pre = [], [], [], []
    action = ["out.append((a, b))"]
    if variant == "break":
        action = ["out.append(b)", "break"]
    elif variant == "and_first":
        test = f"flag and {test}"
    elif variant == "else":
        orelse = ["out.append(None)"]
    elif variant == "before":
        pre = ["seen += 1"]
    elif variant == "after":
        post = ["seen += 1"]
    elif variant == "change_key":
        if style in ("dataclass", "plain", "slots", "dicts"):
            action = ["out.append(b)", f"{key_right} = -1" if style != "dicts" else 'b["k"] = -1']
    elif variant == "grow":
        outer_pre = block("if flag and rights:", ["rights.append(rights[0])"])
    elif variant == "count_after":
        post = block("if seen > 100:", ["break"])
    inner_body = pre + block(f"if {test}:", action) + (block("else:", orelse) if orelse else []) + post
    if sc.chance(0.15) and not pre and not post and not orelse and variant in ("append",):
        loop = outer_pre + [f"out.extend([(a, b) for b in {inner_iter} if {test}])"]
        fn = ["out = []", "seen = 0"] + block("for a in lefts:", loop) + ["return (len(out), seen)"]
    else:
        fn = (
            ["out = []", "seen = 0"]
            + block("for a in lefts:", outer_pre + block(f"for b in {inner_iter}:", inner_body))
            + ["return (len(out), seen)"]
        )
    sc.imports(_common_import(sc))
    sc.define(block(f"def {entry}(lefts: {ann_left}, rights: {ann_right}, {sc.params}):", fn))
    inputs = []
    shapes = [([1, 2, 3], [2, 3, 4]), ([5], [1, 2]), ([1, 1], [1, 2, 1]), ([], [1]), ([3, 4], [])]
    for k, controls in enumerate(sc.control_inputs(6)):
        lk, rk = shapes[k % len(shapes)]
        rights = [make_right(x) for x in rk]
        if sc.rows and k % 3 == 2:
            rights = {"$obj": [f"{sc.common}:Rows_{ns}", [rights], {}]}
        inputs.append({"args": [[make_left(x) for x in lk], rights, *controls]})
    sc.entries.append({"module": sc.main_module, "func": entry, "inputs": inputs})


FAMILIES = {
    "eff101": (gen_eff101, 4),
    "eff103": (gen_eff103, 3),
    "data101": (gen_data101, 2),
    "mut110": (gen_mut110, 2),
    "mut114": (gen_mut114, 2),
    "opt": (gen_opt, 3),
    "opt113": (gen_opt113, 2),
}


class Project:
    def __init__(self, seed, files, entries, families):
        self.seed = seed
        self.files = files
        self.entries = entries
        self.families = families

    def write(self, root):
        import os

        for path, text in self.files.items():
            full = os.path.join(root, path)
            os.makedirs(os.path.dirname(full), exist_ok=True)
            with open(full, "w", encoding="utf-8") as f:
                f.write(text)
        with open(os.path.join(root, "oracle.json"), "w", encoding="utf-8") as f:
            json.dump({"seed": self.seed, "entries": self.entries, "families": self.families}, f, indent=1)


def _module_text(imports, defs):
    lines = sorted(i for i in imports if i.startswith("import ")) + sorted(
        i for i in imports if i.startswith("from ")
    )
    out = "\n".join(lines) + ("\n\n\n" if lines else "")
    out += "\n\n\n".join("\n".join(d) for d in defs)
    return out.rstrip() + "\n"


def generate(seed, families=None, rules=None):
    rng = random.Random(seed)
    names = list(families or FAMILIES)
    weights = [FAMILIES[n][1] for n in names]
    count = rng.choice([1, 1, 2, 2, 3])
    picked = [rng.choices(names, weights)[0] for _ in range(count)]
    files = {"app/__init__.py": ""}
    entries = []
    for index, family in enumerate(picked):
        sc = Scenario(rng, index)
        FAMILIES[family][0](sc)
        files[f"app/c{index}.py"] = sc.common_source()
        files[f"app/s{index}.py"] = _module_text(sc.main_imports, sc.main_defs)
        if sc.helper_defs:
            files[f"app/h{index}.py"] = _module_text(sc.helper_imports, sc.helper_defs)
        files[f"data_{sc.ns}.txt"] = "1\n2\nskip\n"
        entries.extend(sc.entries)
    from plan import EXACT_RULES

    selected = rules or EXACT_RULES
    files["heki.toml"] = HEKI_TOML.format(rules=", ".join(f'"{r}"' for r in selected))
    return Project(seed, files, entries, picked)
