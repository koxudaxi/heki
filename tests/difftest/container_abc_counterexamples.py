#!/usr/bin/env python3
"""Counterexamples of the classes a value of a container ABC may be (`MUT101`'s "Kept to the
return", `MUT111` behind it). Each module under `container_abcs/` is one project: a writer that
iterates a value annotated with an ABC after leaving a cached property stale, and a class the
value may be whose iteration removes the cached value. Run on CPython, each `main()` shows the
value is an instance of the ABC and that the cached value is gone when the writer returns, so
the case may not be taken as established. `crates/heki/tests/e2e_mut101_exact.rs` asserts heki
reports each writer and reader heuristic, and exact once the class is gone (but for the
structural ABC, which never establishes its classes).

Each module runs in an interpreter of its own: a `register` or a `__subclasshook__` changes what
every later subclass check of the ABC answers.

Usage: python3.15 tests/difftest/container_abc_counterexamples.py   (exit status 0 = all hold)
"""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

CASES = Path(__file__).resolve().parent / "container_abcs"


def main() -> int:
    failures = []
    paths = sorted(CASES.glob("*.py"))
    for path in paths:
        run = subprocess.run(
            [sys.executable, "-I", str(path)], capture_output=True, text=True, check=False
        )
        if run.returncode != 0:
            failures.append(f"{path.name}: {run.stderr.strip()}")
    for failure in failures:
        print(f"FAIL {failure}")
    print(f"{len(paths) - len(failures)} of {len(paths)} counterexamples hold")
    return 1 if failures or not paths else 0


if __name__ == "__main__":
    sys.exit(main())
