#!/usr/bin/env python3
"""Layer 2 sentinel for the standard library: verify the rows of `kb/ecosystem/stdlib.toml` that
type a value against the running CPython (3.11-3.15).

The receiver typing reads these rows as the class of what a call or an attribute read gives
(docs/ANALYSIS_AND_KB_SPEC.md section 4.2). Each is checked on instances this script builds
(`RECEIVERS`), of a subclass too where the class may have one:
  1. `[[attributes]]`: reading the attribute gives an instance of exactly the row's `type`.
  2. `return_type`: calling the method gives an instance of exactly that class; `"Self"`, of the
     class of the receiver (the class it is called on, for a class method).
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
import sys
import tomllib
from collections.abc import Callable
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


# Instances of each class whose rows are checked, by the symbol the rows name it with.
RECEIVERS: dict[str, Callable[[], list[Any]]] = {
    "typing.NamedTuple": named_tuples,
    "contextvars.ContextVar": context_variables,
    "contextvars.Token": tokens,
}

# The arguments a method is called with, when it takes some.
ARGUMENTS: dict[str, tuple[Any, ...]] = {
    "typing.NamedTuple._make": ([3, 4],),
    "contextvars.ContextVar.set": (object(),),
}


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
    module, _, name = path.rpartition(".")
    obj: Any = __import__(module, fromlist=[name])
    return getattr(obj, name)


def check_attribute(symbol: str, expected: str) -> list[str]:
    owner, _, attr = symbol.rpartition(".")
    make = RECEIVERS.get(owner)
    if make is None:
        return [f"{symbol}: no receivers for {owner}"]
    cls = resolve(expected)
    return [
        f"{symbol}: on {type(r).__qualname__} gives {type(v).__qualname__}, not {expected}"
        for r in make()
        if type(v := getattr(r, attr)) is not cls
    ]


def check_return(symbol: str, entry: dict) -> list[str]:
    owner, _, method = symbol.rpartition(".")
    make = RECEIVERS.get(owner)
    returns = entry.get("return_type")
    if make is None:
        return [f"{symbol}: no receivers for {owner}"] if returns == "Self" else []
    failures = []
    args = ARGUMENTS.get(symbol, ())
    for receiver in make():
        # A class method returns an instance of the class it is called on.
        callers = [receiver, type(receiver)] if returns == "Self" else [receiver]
        for caller in callers:
            result = getattr(caller, method)(*args)
            on = caller if isinstance(caller, type) else type(caller)
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
        if entry.get("return_type") or (entry.get("copy_sanitizer") and owner in RECEIVERS):
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
