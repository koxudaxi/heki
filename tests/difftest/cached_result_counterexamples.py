#!/usr/bin/env python3
"""Counterexamples of `MUT114`'s exact claim through a call whose targets are not every
implementation it can run. The project's only definition of the method each `add_*` function
calls is cached by `functools.lru_cache`; the value the call is made on may be of a class whose
method heki does not see, which caches nothing: an object of a class outside the project that
has the protocol's method, or an instance of a project class that takes the method from a base
made at run time. Run on CPython, `main()` shows that each such call hands out a new list, so the
line changes no list a cache keeps, while the same change through the cached class changes the
kept one. `crates/heki/tests/e2e_mut114.rs` asserts that heki reports the first two heuristic,
with "Every implementation" open, and the third exact.

Usage: python3.15 tests/difftest/cached_result_counterexamples.py   (exit status 0 = all hold)
"""

from __future__ import annotations

import functools
import sys
import types
from typing import Protocol


class Repo(Protocol):
    def items(self, k: str) -> list[str]: ...


class CachedRepo:
    @functools.lru_cache(maxsize=None)
    def items(self, k: str) -> list[str]:
        return [k, "id"]


def add(repo: Repo, k: str) -> list[str]:
    xs = repo.items(k)
    xs.append("x")
    return xs


Made = type("Made", (), {"rows": lambda self, k: [k, "id"]})


class Listed(Made):
    pass


class CachedListed(Listed):
    @functools.lru_cache(maxsize=None)
    def rows(self, k: str) -> list[str]:
        return [k, "id"]


def add_listed(table: Listed, k: str) -> list[str]:
    xs = table.rows(k)
    xs.append("x")
    return xs


def add_cached(repo: CachedRepo, k: str) -> list[str]:
    xs = repo.items(k)
    xs.append("x")
    return xs


def changes_new_lists(add_one, repo) -> bool:
    first, second = add_one(repo, "k"), add_one(repo, "k")
    return first is not second and second == ["k", "id", "x"]


def main() -> int:
    failures = []
    # An object of a class outside the project with the protocol's method.
    if not changes_new_lists(add, types.SimpleNamespace(items=lambda k: [k, "id"])):
        failures.append("protocol: the change reached a kept list")
    if not changes_new_lists(add_listed, Listed()):
        failures.append("base made at run time: the change reached a kept list")
    cached = CachedRepo()
    add_cached(cached, "k")
    if cached.items("k") != ["k", "id", "x"]:
        failures.append("cached class: the change missed the kept list")
    for failure in failures:
        print(f"FAIL {failure}")
    print(f"{3 - len(failures)} of 3 counterexamples hold")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
