#!/usr/bin/env python3
"""Layer 2 sentinel for the standard library: verify the rows of `kb/ecosystem/stdlib.toml` that
type a value against the running CPython (3.11-3.15).

The receiver typing reads these rows as the class of what a call or an attribute read gives
(docs/ANALYSIS_AND_KB_SPEC.md section 4.2). Each is checked on instances this script builds
(`RECEIVERS`), of a subclass too where the class may have one:
  1. `[[attributes]]`: reading the attribute gives an instance of exactly the row's `type`;
     `"Self"`, of the class of the receiver; `"C[Self]"`, a `C` whose items are of that class.
  2. `return_type`: calling the method gives an instance of exactly that class; `"Self"`, of the
     class of the receiver (the class it is called on, for a class method); `"C[Self]"`, a `C`
     (any iterator for a class named with `()`) whose items are of that class. `pathlib.Path`
     is the class of a path of this system (`PosixPath` or `WindowsPath`).
  3. `copy_sanitizer` on a method: the call gives a new object of the receiver's class holding the
     receiver's items (`"shallow"`).
  4. `returns_held`: the method gives the very object its receiver holds, and with `default`, the
     default it is passed where the receiver holds none.
Every row of kinds 1, 4, and `"Self"` has receivers here, or the check fails; a row of kinds 2 and
3 is checked where its class has them.

Usage: python3.15 tests/sentinel/verify_stdlib_kb.py   (exit status 0 = no failure)
"""

from __future__ import annotations

import asyncio
import concurrent.futures
import contextvars
import importlib.machinery
import importlib.util
import inspect
import sys
import tempfile
import tomllib
from collections.abc import Callable, Sequence
from pathlib import Path
from typing import Any, NamedTuple

KB = Path(__file__).resolve().parents[2] / "kb" / "ecosystem" / "stdlib.toml"


class Point(NamedTuple):
    x: int
    y: int = 0


class SubPoint(Point):
    pass


def named_tuples() -> list[Any]:
    return [Point(1), SubPoint(1, 2)]


def context_variables() -> list[Any]:
    return [contextvars.ContextVar("sentinel")]


def tokens() -> list[Any]:
    return [contextvars.ContextVar("sentinel").set(1)]


def paths() -> list[Any]:
    """A directory holding one file, kept for the life of the script."""
    folder = Path(tempfile.mkdtemp(prefix="heki-sentinel-"))
    (folder / "f.txt").write_text("x")
    return [folder]


def files(*modes: str) -> Callable[[], list[Any]]:
    """New files holding two lines, opened in each of `modes` (`"r+b0"`: with `buffering=0`)."""

    def make() -> list[Any]:
        folder = Path(tempfile.mkdtemp(prefix="heki-sentinel-"))
        opened = []
        for i, mode in enumerate(modes):
            path = folder / f"f{i}.txt"
            path.write_text("a\nb\n")
            unbuffered = mode.endswith("0")
            opened.append(open(path, mode.removesuffix("0"), buffering=0 if unbuffered else -1))
        return opened

    return make


# Instances of each class whose rows are checked, by the symbol the rows name it with.
RECEIVERS: dict[str, Callable[[], list[Any]]] = {
    "typing.NamedTuple": named_tuples,
    "contextvars.ContextVar": context_variables,
    "contextvars.Token": tokens,
    "pathlib.Path": paths,
    "io.TextIOWrapper": files("r+"),
    "io.BufferedReader": files("rb"),
    "io.BufferedWriter": files("ab"),
    "io.BufferedRandom": files("r+b"),
    "io.FileIO": files("r+b0"),
    # A file object of a mode heki does not know: one of each class whose members all work.
    "io.open()": files("r+", "r+b", "r+b0"),
}

# Calls of the functions of a module whose `return_type` is checked, by symbol.
FUNCTIONS: dict[str, Callable[[], Any]] = {
    "importlib.util.module_from_spec": lambda: importlib.util.module_from_spec(
        importlib.machinery.ModuleSpec("sentinel", None)
    ),
}

# The arguments a method is called with, when it takes some.
ARGUMENTS: dict[str, tuple[Any, ...]] = {
    "typing.NamedTuple._make": ([3, 4],),
    "contextvars.ContextVar.set": (object(),),
    "pathlib.Path.joinpath": ("x",),
    "pathlib.Path.with_name": ("n.py",),
    "pathlib.Path.with_stem": ("s",),
    "pathlib.Path.with_suffix": (".py",),
    "pathlib.Path.relative_to": ("/",),
    "pathlib.Path.__truediv__": ("x",),
    "pathlib.Path.__rtruediv__": ("x",),
    "pathlib.Path.glob": ("*",),
    "pathlib.Path.rglob": ("*",),
    "io.TextIOWrapper.write": ("x",),
    "io.BufferedWriter.write": (b"x",),
    "io.BufferedRandom.write": (b"x",),
    "io.TextIOWrapper.seek": (0,),
    "io.BufferedReader.seek": (0,),
    "io.BufferedWriter.seek": (0,),
    "io.BufferedRandom.seek": (0,),
    "io.FileIO.seek": (0,),
    "io.open().seek": (0,),
}


# The first release that has a method, where the interpreter running the script may be older.
SINCE: dict[str, tuple[int, int]] = {"pathlib.Path.walk": (3, 12)}


def holding_context_variable(held: object) -> tuple[Any, Any]:
    full = contextvars.ContextVar("full")
    full.set(held)
    return full, contextvars.ContextVar("empty")


def holding_future(held: object) -> tuple[Any, None]:
    future: concurrent.futures.Future[object] = concurrent.futures.Future()
    future.set_result(held)
    return future, None


def holding_asyncio_future(held: object) -> tuple[Any, None]:
    loop = asyncio.new_event_loop()
    try:
        future = loop.create_future()
        future.set_result(held)
    finally:
        loop.close()
    return future, None


# A receiver holding a value, and one holding none (`None`: it cannot), by class.
HOLDERS: dict[str, Callable[[object], tuple[Any, Any]]] = {
    "contextvars.ContextVar": holding_context_variable,
    "concurrent.futures.Future": holding_future,
    "asyncio.Future": holding_asyncio_future,
}


def resolve(path: str) -> Any:
    if path == "pathlib.Path":
        return type(Path())
    module, _, name = path.rpartition(".")
    obj: Any = __import__(module, fromlist=[name])
    return getattr(obj, name)


def check_attribute(symbol: str, expected: str) -> list[str]:
    owner, _, attr = symbol.rpartition(".")
    make = RECEIVERS.get(owner)
    if make is None:
        return [f"{symbol}: no receivers for {owner}"]
    failures = []
    for r in make():
        value = getattr(r, attr)
        if expected == "Self":
            if type(value) is not type(r):
                failures.append(f"{symbol}: on {type(r).__qualname__} gives {type(value).__qualname__}")
        elif expected.endswith("[Self]"):
            container = resolve(expected.removesuffix("[Self]").replace("typing.", "collections.abc."))
            if not isinstance(value, container) or any(type(i) is not type(r) for i in value):
                failures.append(f"{symbol}: on {type(r).__qualname__}, not {expected}")
        elif type(value) is not resolve(expected):
            failures.append(
                f"{symbol}: on {type(r).__qualname__} gives {type(value).__qualname__}, not {expected}"
            )
    return failures


def check_return(symbol: str, entry: dict) -> list[str]:
    owner, _, method = symbol.rpartition(".")
    make = RECEIVERS.get(owner)
    returns = entry.get("return_type")
    if make is None:
        return [f"{symbol}: no receivers for {owner}"] if returns == "Self" else []
    failures = []
    args = ARGUMENTS.get(symbol, ())
    if sys.version_info < SINCE.get(symbol, (0, 0)):
        return failures
    for receiver in make():
        # A class method returns an instance of the class it is called on.
        classmethod_too = inspect.ismethod(getattr(type(receiver), method, None))
        callers = [receiver, type(receiver)] if returns == "Self" and classmethod_too else [receiver]
        for caller in callers:
            result = getattr(caller, method)(*args)
            on = caller if isinstance(caller, type) else type(caller)
            if returns is not None and returns.endswith("[Self]"):
                container = returns.removesuffix("[Self]")
                items = list(result)
                if (
                    not items
                    or any(type(i) is not on for i in items)
                    or (not container.endswith("()") and not isinstance(result, resolve(
                        container.replace("typing.", "collections.abc.")
                    )))
                ):
                    failures.append(f"{symbol}: on {on.__qualname__}, not {returns}")
                continue
            if returns is not None and returns.endswith("()"):
                continue  # The generator of a call is a class of heki's, named after the call.
            if returns == "Self":
                expected = on
            elif returns is not None:
                expected = resolve(returns)
            else:
                expected = type(receiver)
            if type(result) is not expected:
                failures.append(
                    f"{symbol}: on {on.__qualname__} returns {type(result).__qualname__}"
                )
            if entry.get("copy_sanitizer") == "shallow" and (
                result is receiver or any(a is not b for a, b in zip(result, receiver))
            ):
                failures.append(f"{symbol}: no shallow copy of a {on.__qualname__}")
    return failures


def check_function(symbol: str, expected: str, call: Callable[[], Any]) -> list[str]:
    result = call()
    if type(result) is not resolve(expected):
        return [f"{symbol}: gives {type(result).__qualname__}, not {expected}"]
    return []


def check_held(symbol: str, held_row: dict) -> list[str]:
    owner, _, method = symbol.rpartition(".")
    make = HOLDERS.get(owner)
    if make is None:
        return [f"{symbol}: no holders for {owner}"]
    held, default = object(), object()
    full, empty = make(held)
    failures = []
    if getattr(full, method)() is not held:
        failures.append(f"{symbol}: does not return the value held")
    at = held_row.get("default")
    if at is not None:
        args: list[object] = [None] * at + [default]
        if getattr(full, method)(*args) is not held:
            failures.append(f"{symbol}: returns the default although a value is held")
        if empty is None or getattr(empty, method)(*args) is not default:
            failures.append(f"{symbol}: does not return the default where none is held")
    return failures


def checked(symbol: str, check: Callable[[], list[str]]) -> list[str]:
    try:
        return check()
    except Exception as e:  # A row the interpreter contradicts may make the call raise.
        return [f"{symbol}: {e!r}"]


def main() -> int:
    data = tomllib.loads(KB.read_text())
    failures: list[str] = []
    for row in data.get("attributes", []):
        failures += checked(row["symbol"], lambda: check_attribute(row["symbol"], row["type"]))
    entries = 0
    for entry in data.get("entries", []):
        symbol = entry["symbol"]
        owner = symbol.rpartition(".")[0]
        if symbol in FUNCTIONS:
            failures += checked(
                symbol, lambda: check_function(symbol, entry["return_type"], FUNCTIONS[symbol])
            )
            entries += 1
        elif entry.get("return_type") or (entry.get("copy_sanitizer") and owner in RECEIVERS):
            failures += checked(symbol, lambda: check_return(symbol, entry))
            entries += owner in RECEIVERS
        if "returns_held" in entry:
            failures += checked(symbol, lambda: check_held(symbol, entry["returns_held"]))
            entries += 1
    print(
        f"Python {sys.version.split()[0]}: {len(data.get('attributes', []))} attribute rows, "
        f"{entries} typed entries checked"
    )
    for f in failures:
        print(f"  {f}")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
