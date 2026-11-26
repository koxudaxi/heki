#!/usr/bin/env python3
"""Counterexamples of `EFF109`'s exact claim, one or more per premise. Each `add_*` (and
`Base.run`, `Board.beat`) holds a lock and makes a call that may take it again; in each
counterexample one premise is false on the input `main()` runs, so the call returns without
waiting. Run on CPython, `main()` shows the base shapes wait for themselves (a thread still
blocked after a timeout, a task the timeout cancels) and each counterexample returns.
`crates/heki/tests/e2e_eff109.rs` asserts that heki reports the base shapes exact, the
counterexamples of Non-reentrant, Every implementation, and Every return heuristic, and those of
Held and Same lock not at all.

Usage: python3.15 tests/difftest/lock_retaken_counterexamples.py   (exit status 0 = all hold)
"""

from __future__ import annotations

import asyncio
import sys
import threading

WAIT = 0.5


def skipping(fn):
    def inner(self, flag):
        if flag:
            return 0
        return fn(self, flag)

    return inner


class Store:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self.items: list[int] = []

    def size(self) -> int:
        with self._lock:
            return len(self.items)

    def add(self) -> int:
        with self._lock:
            self.items.append(1)
            return self.size()

    # Every return: the helper returns without the lock when the flag is set.
    def maybe_size(self, flag: bool) -> int:
        if flag:
            return 0
        with self._lock:
            return len(self.items)

    def add_maybe(self, flag: bool) -> int:
        with self._lock:
            return self.maybe_size(flag)

    # Every implementation: a decorator that may skip the body.
    @skipping
    def skipped_size(self, flag: bool) -> int:
        with self._lock:
            return len(self.items)

    def add_skipped(self, flag: bool) -> int:
        with self._lock:
            return self.skipped_size(flag)

    # Held: the lock is taken on one path only, or a helper gives it back.
    def add_once(self, flag: bool) -> int:
        if flag:
            self._lock.acquire()
        return self.size()

    # Same lock: the helper takes another object's lock.
    def add_other(self, other: Store) -> int:
        with self._lock:
            return other.size()


# Held: a helper gives the lock back. A class of its own: a lock some function releases
# without holding it is a handoff, which no retake of it is reported for.
class Handoff:
    def __init__(self) -> None:
        self._lock = threading.Lock()

    def size(self) -> int:
        with self._lock:
            return 0

    def unlock(self) -> None:
        self._lock.release()

    def add_unlocked(self) -> int:
        self._lock.acquire()
        self.unlock()
        return self.size()


# Same lock: the helper binds another lock to the name before taking it.
class Rebound:
    def __init__(self) -> None:
        self._lock = threading.Lock()

    def fresh_size(self) -> int:
        self._lock = threading.Lock()
        with self._lock:
            return 0

    def add_fresh(self) -> int:
        with self._lock:
            return self.fresh_size()


# Every implementation: a subclass overrides the helper without the lock.
class Base:
    def __init__(self) -> None:
        self._lock = threading.Lock()

    def count(self) -> int:
        with self._lock:
            return 1

    def run(self) -> int:
        with self._lock:
            return self.count()


class Quiet(Base):
    def count(self) -> int:
        return 0


# Non-reentrant: a project class derived from the lock class takes it again without waiting.
class Reentrant(asyncio.Lock):
    def __init__(self) -> None:
        super().__init__()
        self._owner: object = None
        self._depth = 0

    async def acquire(self) -> bool:
        task = asyncio.current_task()
        if self._owner is task:
            self._depth += 1
            return True
        await super().acquire()
        self._owner, self._depth = task, 1
        return True

    def release(self) -> None:
        self._depth -= 1
        if self._depth == 0:
            self._owner = None
            super().release()


class Board:
    def __init__(self, lock: asyncio.Lock) -> None:
        self._lock = lock
        self.beats = 0

    async def update(self) -> None:
        async with self._lock:
            self.beats += 1

    async def beat(self) -> None:
        async with self._lock:
            await self.update()


def waits(run) -> bool:
    """Whether `run` is still blocked after the timeout (a daemon thread, left behind)."""
    t = threading.Thread(target=run, daemon=True)
    t.start()
    t.join(WAIT)
    return t.is_alive()


def waits_async(make) -> bool:
    """Whether the coroutine `make()` returns is still waiting when the timeout cancels it."""

    async def bounded() -> None:
        await asyncio.wait_for(make(), WAIT)

    try:
        asyncio.run(bounded())
    except TimeoutError:
        return True
    return False


def main() -> int:
    failures = []
    checks = [
        ("base: a helper taking the held lock", True, lambda: Store().add()),
        ("base: the method of the class itself", True, lambda: Base().run()),
        ("Every return: the helper returned first", False, lambda: Store().add_maybe(True)),
        ("Every implementation: the decorator skipped the body", False, lambda: Store().add_skipped(True)),
        ("Every implementation: the subclass's method", False, lambda: Quiet().run()),
        ("Held: taken on one path only", False, lambda: Store().add_once(False)),
        ("Held: a helper gave it back", False, lambda: Handoff().add_unlocked()),
        ("Same lock: the name bound to another lock", False, lambda: Rebound().add_fresh()),
        ("Same lock: another object's lock", False, lambda: Store().add_other(Store())),
    ]
    for name, expected, run in checks:
        if waits(run) != expected:
            failures.append(f"{name}: {'returned' if expected else 'waited'}")
    if not waits_async(lambda: Board(asyncio.Lock()).beat()):
        failures.append("base: the asyncio lock returned")
    if waits_async(lambda: Board(Reentrant()).beat()):
        failures.append("Non-reentrant: the derived lock waited")
    for failure in failures:
        print(f"FAIL {failure}")
    total = len(checks) + 2
    print(f"{total - len(failures)} of {total} counterexamples hold")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
