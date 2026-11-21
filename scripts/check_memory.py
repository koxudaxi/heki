#!/usr/bin/env python3
"""Memory gate: run heki on a generated project and fail when its peak memory exceeds a budget.

The project is generated deterministically and shaped after what dominates heki's memory on
real code bases:

- typed field writes reached through long chains of private helpers across modules: with an
  ownership contract, every caller keeps the write sites its private callees hand over, with
  their chains (`EffectSummary::typed_field_write_sites`), and the write facts of every callee;
- a blocking call below `async def` entry points (execution-effect chains);
- many small modules and classes: every file's check reads the declarations of the project,
  which the language-server engine remembers per file (`--watch` runs that engine);
- functions whose IR copies multiply: `finally` bodies nested deep, each `try` returning
  early (each copy of a `finally` body copies the ones inside it once per way out), and a
  loop after a `return` that no path enters. Past its budget a function is left unmodeled,
  so none of them costs more than its budget.

Two measures:

- `heap`, the gate of every change: the peak of the heap bytes heki asks for, counted by the
  allocator of a `count-alloc` build, which also runs its parallel work on the main thread
  alone. No allocator cache, memory pressure, or thread timing moves it, so every run reads the
  same bytes and one run per mode suffices. The total of the bytes allocated is printed with it.
- `rss`, at release time: the peak memory of a release build as the system sees it, which also
  holds the allocator's fragmentation, its mappings, and the code. Each mode runs three times
  and the lowest peak counts: the physical footprint on macOS (`/usr/bin/time -l`; unlike the
  resident set size it does not drop when the system compresses memory) and the resident set
  size on Linux (`wait4`). The allocators differ, so each platform has its own budgets.

Usage:
    cargo build --release -p heki --features count-alloc
    python3 scripts/check_memory.py --heki target/release/heki
    cargo build --release -p heki
    python3 scripts/check_memory.py --heki target/release/heki --measure rss
    python3 scripts/check_memory.py --heki target/release/heki --report   # print, never fail
"""

from __future__ import annotations

import argparse
import contextlib
import os
import re
import signal
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

# Call-chain layers, functions per layer, and small modules of classes.
LAYERS = 24
STEPS = 6
SHAPE_MODULES = 600
# `finally` bodies nested this deep pass a function's IR budget.
COPIES_DEPTH = 16

# Heap budgets in MiB per mode: the peak on the CI runner at calibration plus 5 %. A change
# of a budget needs the owner's decision (docs/development.md, Memory).
HEAP_BUDGETS_MIB = {"check": 56.7, "watch": 51.8, "owners": 70.6}

# RSS budgets in MiB per platform and mode, a coarse check at release time: the lowest of three
# runs at calibration plus about 10 %.
RSS_BUDGETS_MIB = {
    # Peak physical footprint.
    "darwin": {"check": 78, "watch": 88, "owners": 96},
    # Peak resident set size on the CI runner.
    "linux": {"check": 83, "watch": 93, "owners": 101},
}
RSS_RUNS = 3

MIB = 1024 * 1024
HEAP_LINE = re.compile(r"^heki heap: peak (\d+) bytes, total (\d+) bytes$", re.MULTILINE)

# `--heuristic`: every rule that is not opt-in runs, the heuristic ones too.
MODES = {
    "check": (["check", "--heuristic", "--output-format", "json", "."], {}),
    "watch": (
        ["check", "--heuristic", "--watch", "--output-format", "json", "."],
        {"HEKI_WATCH_MAX_RUNS": "0"},
    ),
    "owners": (
        ["check", "--heuristic", "--output-format", "json", "--config", "owners.toml", "."],
        {},
    ),
}


def layer_module(i: int) -> str:
    lines = ["import time", "", "from pkg.models import Model, Record"]
    if i > 0:
        names = ", ".join(f"_step{j} as _lower{j}" for j in range(STEPS))
        lines.append(f"from pkg.layer{i - 1} import {names}")
    lines.append("")
    for j in range(STEPS):
        lines += [
            "",
            f"def _step{j}(a: Model, b: Record) -> None:",
            f"    a.items.append({j})",
            f"    b.size = {j}",
            f'    a.names["k{j}"] = b',
            "    for n in range(3):",
            "        b.tags.add(n)",
        ]
        if i == 0:
            lines.append("    time.sleep(0.01)")
        else:
            lines += [
                f"    _lower{j}(a, b)",
                f"    _lower{(j + 1) % STEPS}(a, b)",
            ]
        lines.append("")
    return "\n".join(lines)


def entry_module() -> str:
    names = ", ".join(f"_step{j}" for j in range(STEPS))
    lines = [
        "from pkg.models import Model, Record",
        f"from pkg.layer{LAYERS - 1} import {names}",
        "",
    ]
    for j in range(STEPS):
        lines += [
            "",
            f"async def handle{j}(a: Model, b: Record) -> None:",
            f"    _step{j}(a, b)",
            "",
        ]
    return "\n".join(lines)


def shape_module(k: int) -> str:
    lines = []
    if k > 0:
        lines += [f"from pkg.shapes.s{k - 1} import Shape{k - 1}a", ""]
    for c in "abc":
        lines += [
            "",
            f"class Shape{k}{c}:",
            "    def __init__(self, n):",
            "        self.n = n",
            "        self.parts = []",
            "",
            "    def grow(self, x):",
            "        self.parts.append(x)",
            "        return len(self.parts)",
            "",
        ]
    if k > 0:
        lines += [
            "",
            f"def link{k}(s: Shape{k - 1}a):",
            f"    s.grow({k})",
            f"    return Shape{k}a(s.n)",
            "",
        ]
    return "\n".join(lines)


def copies_module(depth: int) -> str:
    lines = ["def nested(xs):"]
    pad = "    "
    for i in range(depth):
        lines += [f"{pad}try:", f"{pad}    if xs:", f"{pad}        return {i}", f"{pad}finally:"]
        pad += "    "
    lines += [f"{pad}for x in xs:", f"{pad}    print(x)", "", ""]
    lines += ["def dead(xs):", "    return xs", "    for x in xs:", "        print(x)"]
    lines += ["    print(xs)"]
    return "\n".join(lines) + "\n"


# The `owners` mode's configuration: a write-owner contract on the fields the layers write.
OWNERS = """\
target-version = "py312"

[[write-owners]]
name = "models"
targets = ["pkg.models.Model.items", "pkg.models.Record.tags"]
allowed-writers = ["pkg.entry.*"]
"""

MODELS = """\
class Model:
    def __init__(self):
        self.items = []
        self.names = {}


class Record:
    def __init__(self):
        self.size = 0
        self.tags = set()
"""


def generate(root: Path) -> int:
    """Write the project under `root`; returns its number of lines."""
    files = {
        "pyproject.toml": '[project]\nname = "memory-gate"\nrequires-python = ">=3.12"\n',
        "owners.toml": OWNERS,
        "pkg/__init__.py": "",
        "pkg/models.py": MODELS,
        "pkg/entry.py": entry_module(),
        "pkg/shapes/__init__.py": "",
        "pkg/copies.py": copies_module(COPIES_DEPTH),
    }
    for i in range(LAYERS):
        files[f"pkg/layer{i}.py"] = layer_module(i)
    for k in range(SHAPE_MODULES):
        files[f"pkg/shapes/s{k}.py"] = shape_module(k)
    lines = 0
    for rel, text in files.items():
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text)
        if rel.endswith(".py"):
            lines += text.count("\n")
    return lines


def execute(
    heki: str,
    args: list[str],
    root: Path,
    extra_env: dict[str, str],
    timed: bool,
    timeout: float | None = None,
) -> tuple[float, int, str, os.struct_rusage]:
    """One run of heki in `root` (with `/usr/bin/time -l` when `timed`): its wall time in
    seconds, its exit code (`-9` when it ran past `timeout` and was stopped), its stderr, and its
    resource usage."""
    env = {**os.environ, **extra_env}
    cmd = ["/usr/bin/time", "-l", heki, *args] if timed else [heki, *args]
    start = time.monotonic()
    # Its own process group, so that a run past `timeout` is stopped with `time`'s child.
    proc = subprocess.Popen(
        cmd,
        cwd=root,
        env=env,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )

    def stop() -> None:
        # The group may have ended between the deadline and the timer's cancellation.
        with contextlib.suppress(ProcessLookupError):
            os.killpg(proc.pid, signal.SIGKILL)

    killer = threading.Timer(timeout, stop) if timeout is not None else None
    if killer is not None:
        killer.start()
    assert proc.stderr is not None
    stderr = proc.stderr.read().decode()
    proc.stderr.close()
    # `wait4` reaps the child and reports its own resource usage.
    _, status, usage = os.wait4(proc.pid, 0)
    seconds = time.monotonic() - start
    if killer is not None:
        killer.cancel()
    return seconds, os.waitstatus_to_exitcode(status), stderr, usage


def run(heki: str, mode: str, root: Path, timed: bool) -> tuple[str, os.struct_rusage]:
    """One run of `mode` (with `/usr/bin/time -l` when `timed`): its stderr and resource usage."""
    args, extra_env = MODES[mode]
    _, code, stderr, usage = execute(heki, args, root, extra_env, timed)
    # 0 or 1: the reports of the generated project, warnings or errors; anything else is a failure.
    if code not in (0, 1):
        sys.exit(f"heki {' '.join(args)} exited with {code}:\n{stderr}")
    return stderr, usage


def heap_peak(heki: str, mode: str, root: Path) -> tuple[int, int]:
    """The peak and the total of the heap bytes of one run of a `count-alloc` build."""
    stderr, _ = run(heki, mode, root, timed=False)
    found = HEAP_LINE.search(stderr)
    if not found:
        sys.exit(
            "heki printed no heap count: the heap measure needs a counting build "
            "(cargo build --release -p heki --features count-alloc)"
        )
    return int(found.group(1)), int(found.group(2))


def rss_peak_mib(heki: str, mode: str, root: Path) -> float:
    """The peak of one run of a release build as the system sees it (module documentation)."""
    darwin = sys.platform == "darwin"
    stderr, usage = run(heki, mode, root, timed=darwin)
    if HEAP_LINE.search(stderr):
        sys.exit("the rss measure needs a release build without `count-alloc`")
    if darwin:
        found = re.search(r"(\d+)\s+peak memory footprint", stderr)
        if not found:
            sys.exit(f"`/usr/bin/time -l` printed no peak memory footprint:\n{stderr}")
        return int(found.group(1)) / MIB
    # `ru_maxrss` is in KiB on Linux.
    return usage.ru_maxrss / 1024


def check_heap(heki: str, root: Path) -> list[str]:
    """Print each mode's heap peak against its budget; return the modes over budget."""
    failed = []
    for mode, budget in HEAP_BUDGETS_MIB.items():
        peak, total = heap_peak(heki, mode, root)
        verdict = "ok" if peak <= budget * MIB else "OVER BUDGET"
        print(
            f"{mode:<6} peak heap {peak:>11} bytes ({peak / MIB:6.2f} MiB)  "
            f"total {total:>12} bytes  budget {budget:5.1f} MiB  {verdict}",
            flush=True,
        )
        if peak > budget * MIB:
            failed.append(mode)
    return failed


def check_rss(heki: str, root: Path) -> list[str]:
    """Print each mode's lowest RSS peak against its budget; return the modes over budget."""
    budgets = RSS_BUDGETS_MIB["darwin" if sys.platform == "darwin" else "linux"]
    what = "footprint" if sys.platform == "darwin" else "RSS"
    failed = []
    for mode, budget in budgets.items():
        peak = min(rss_peak_mib(heki, mode, root) for _ in range(RSS_RUNS))
        verdict = "ok" if peak <= budget else "OVER BUDGET"
        print(
            f"{mode:<6} peak {what} {peak:7.1f} MiB  budget {budget:4d} MiB  {verdict}",
            flush=True,
        )
        if peak > budget:
            failed.append(mode)
    return failed


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--heki", required=True, help="path to the heki binary")
    parser.add_argument(
        "--measure",
        choices=("heap", "rss"),
        default="heap",
        help="heap: a count-alloc build's heap bytes (the gate); rss: a release build's peak",
    )
    parser.add_argument("--report", action="store_true", help="print the peaks, never fail")
    args = parser.parse_args()
    heki = str(Path(args.heki).resolve())
    with tempfile.TemporaryDirectory(prefix="heki-memory-gate-") as tmp:
        root = Path(tmp)
        lines = generate(root)
        print(f"generated project: {lines} lines", flush=True)
        check = check_heap if args.measure == "heap" else check_rss
        failed = check(heki, root)
    if failed and not args.report:
        print(
            f"peak memory over budget ({', '.join(failed)}): find what grew "
            "(docs/development.md, Memory); a budget changes only with the owner's decision",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
