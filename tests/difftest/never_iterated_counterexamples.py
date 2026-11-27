#!/usr/bin/env python3
"""Counterexamples of `EFF116`'s exact claim, one or more per premise. Each `refresh_*` calls a
function whose body appends to a log; in each counterexample one premise is false on the input
`main()` runs, so the body runs. Run on CPython, `main()` shows that the base shapes leave the log
empty and each counterexample fills it. `crates/heki/tests/e2e_eff116.rs` asserts that heki
reports the base shapes exact, the counterexamples of Generator function and Every
implementation heuristic (a name bound to two definitions, a receiver typed by a protocol), and
the others not at all (a subclass overriding the method with a plain function, or with a
generator function a decorator replaces, a generator that is iterated or kept).

Usage: python3.15 tests/difftest/never_iterated_counterexamples.py   (exit status 0 = all hold)
"""

from __future__ import annotations

import os
import sys
import types
from typing import Protocol


class Loader:
    def __init__(self) -> None:
        self.log: list[str] = []

    def load(self):
        self.log.append("loaded")
        yield 1


def refresh(loader: Loader) -> None:
    loader.load()


def refresh_tested(loader: Loader) -> bool:
    return not loader.load()


# Generator function: the name is bound to two definitions, the plain one on the input run.
if not os.environ.get("HEKI_COUNTEREXAMPLE_GENERATOR"):

    def clean(log: list[str]):
        log.append("cleaned")

else:

    def clean(log: list[str]):
        log.append("cleaned")
        yield


def refresh_bound_twice(log: list[str]) -> None:
    clean(log)


# Every implementation: a receiver typed by a protocol, which an object of a class outside the
# project implements, or a subclass's plain method.
class Rows(Protocol):
    def rows(self): ...


class Table:
    def rows(self):
        yield 1


def refresh_protocol(source: Rows) -> None:
    source.rows()


class Source:
    def __init__(self) -> None:
        self.log: list[str] = []

    def load(self):
        self.log.append("loaded")
        yield 1


class EagerSource(Source):
    def load(self):
        self.log.append("loaded")


def refresh_subclass(source: Source) -> None:
    source.load()


# Generator function: a subclass's generator function, which a decorator replaces with a plain
# function iterating it.
def eager(fn):
    def inner(*args, **kwargs):
        return list(fn(*args, **kwargs))

    return inner


class Feed:
    def __init__(self) -> None:
        self.log: list[str] = []

    def load(self):
        self.log.append("loaded")
        yield 1


class DrainedFeed(Feed):
    @eager
    def load(self):
        self.log.append("loaded")
        yield 1


def refresh_decorated(feed: Feed) -> None:
    feed.load()


# Never iterated: the generator is iterated, or kept and stepped.
def refresh_iterated(loader: Loader) -> None:
    for _ in loader.load():
        pass


def refresh_stepped(loader: Loader) -> None:
    rows = loader.load()
    next(rows)


def main() -> int:
    failures = []
    loader = Loader()
    refresh(loader)
    refresh_tested(loader)
    if loader.log:
        failures.append("base: the body ran")
    log: list[str] = []
    refresh_bound_twice(log)
    eager: list[str] = []
    refresh_protocol(types.SimpleNamespace(rows=lambda: eager.append("loaded")))
    subclass = EagerSource()
    refresh_subclass(subclass)
    drained = DrainedFeed()
    refresh_decorated(drained)
    iterated, stepped = Loader(), Loader()
    refresh_iterated(iterated)
    refresh_stepped(stepped)
    ran = {
        "Generator function: the plain definition": log,
        "Every implementation: an implementer outside the project": eager,
        "Every implementation: the subclass's plain method": subclass.log,
        "Generator function: the subclass's method a decorator replaces": drained.log,
        "Never iterated: a for loop": iterated.log,
        "Never iterated: a name and next()": stepped.log,
    }
    failures.extend(f"{name}: the body did not run" for name, logged in ran.items() if not logged)
    for failure in failures:
        print(f"FAIL {failure}")
    total = len(ran) + 1
    print(f"{total - len(failures)} of {total} counterexamples hold")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
