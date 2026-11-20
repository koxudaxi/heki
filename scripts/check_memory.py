#!/usr/bin/env python3
"""Peak-memory gate: run heki on a generated project and fail when its peak RSS exceeds a budget.

The project is generated deterministically and shaped after what dominates heki's memory on
real code bases (measured on datamodel-code-generator, SQLAlchemy, and pydantic):

- typed field writes reached through long call chains across modules: every caller keeps the
  write sites of its callees with their chains (`EffectSummary::typed_field_write_sites`);
- a blocking call below `async def` entry points (execution-effect chains);
- many small modules and classes: every file's check reads the declarations of the project,
  which the language-server engine remembers per file (`--watch` runs that engine).

Each mode runs three times and the lowest peak counts. The peak is the physical footprint on
macOS (`/usr/bin/time -l`; unlike the resident set size it does not drop when the system
compresses memory) and the resident set size on Linux (`wait4`). The allocators differ, so each
platform has its own budgets.

Usage:
    python3 scripts/check_memory.py --heki target/release/heki
    python3 scripts/check_memory.py --heki target/release/heki --report   # print, never fail
"""

from __future__ import annotations

import argparse
import os
import re
import subprocess
import sys
import tempfile
from pathlib import Path

# Call-chain layers, functions per layer, and small modules of classes.
LAYERS = 24
STEPS = 6
SHAPE_MODULES = 600

# Peak budgets in MiB per platform and mode: the lowest of three runs plus about 8 %. Lower a
# budget when a change makes heki smaller; raise one only with the measurement that justifies it.
BUDGETS_MIB = {
    # Peak footprint, measured on macOS (14 cores): `check` 163, `watch` 167.
    "darwin": {"check": 176, "watch": 181},
    # Peak resident set size; not measured yet: calibrate from the first CI run.
    "linux": {"check": 230, "watch": 240},
}
RUNS = 3

MODES = {
    "check": (["check", "--output-format", "json", "."], {}),
    "watch": (["check", "--watch", "--output-format", "json", "."], {"HEKI_WATCH_MAX_RUNS": "0"}),
}


def layer_module(i: int) -> str:
    lines = ["import time", "", "from pkg.models import Model, Record"]
    if i > 0:
        names = ", ".join(f"step{j} as lower{j}" for j in range(STEPS))
        lines.append(f"from pkg.layer{i - 1} import {names}")
    lines.append("")
    for j in range(STEPS):
        lines += [
            "",
            f"def step{j}(a: Model, b: Record) -> None:",
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
                f"    lower{j}(a, b)",
                f"    lower{(j + 1) % STEPS}(a, b)",
            ]
        lines.append("")
    return "\n".join(lines)


def entry_module() -> str:
    names = ", ".join(f"step{j}" for j in range(STEPS))
    lines = [
        "from pkg.models import Model, Record",
        f"from pkg.layer{LAYERS - 1} import {names}",
        "",
    ]
    for j in range(STEPS):
        lines += [
            "",
            f"async def handle{j}(a: Model, b: Record) -> None:",
            f"    step{j}(a, b)",
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
        "pkg/__init__.py": "",
        "pkg/models.py": MODELS,
        "pkg/entry.py": entry_module(),
        "pkg/shapes/__init__.py": "",
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


def peak_mib(heki: str, mode: str, root: Path) -> float:
    """The peak of one run (see the module documentation)."""
    args, extra_env = MODES[mode]
    env = {**os.environ, **extra_env}
    darwin = sys.platform == "darwin"
    cmd = ["/usr/bin/time", "-l", heki, *args] if darwin else [heki, *args]
    proc = subprocess.Popen(
        cmd, cwd=root, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE
    )
    assert proc.stderr is not None
    stderr = proc.stderr.read().decode()
    proc.stderr.close()
    # `wait4` reaps the child and reports its own resource usage.
    _, status, usage = os.wait4(proc.pid, 0)
    proc.returncode = os.waitstatus_to_exitcode(status)
    # 0: clean, 1: diagnostics (the generated project has some); anything else is a failure.
    if proc.returncode not in (0, 1):
        sys.exit(f"heki {' '.join(args)} exited with {proc.returncode}:\n{stderr}")
    if darwin:
        found = re.search(r"(\d+)\s+peak memory footprint", stderr)
        if not found:
            sys.exit(f"`/usr/bin/time -l` printed no peak memory footprint:\n{stderr}")
        return int(found.group(1)) / (1024 * 1024)
    # `ru_maxrss` is in KiB on Linux.
    return usage.ru_maxrss / 1024


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--heki", required=True, help="path to the heki binary (release build)")
    parser.add_argument("--report", action="store_true", help="print the peaks, never fail")
    args = parser.parse_args()
    heki = str(Path(args.heki).resolve())
    budgets = BUDGETS_MIB["darwin" if sys.platform == "darwin" else "linux"]
    what = "footprint" if sys.platform == "darwin" else "RSS"
    failed = []
    with tempfile.TemporaryDirectory(prefix="heki-memory-gate-") as tmp:
        root = Path(tmp)
        lines = generate(root)
        print(f"generated project: {lines} lines", flush=True)
        for mode, budget in budgets.items():
            peak = min(peak_mib(heki, mode, root) for _ in range(RUNS))
            verdict = "ok" if peak <= budget else "OVER BUDGET"
            print(
                f"{mode:<6} peak {what} {peak:7.1f} MiB  budget {budget:4d} MiB  {verdict}",
                flush=True,
            )
            if peak > budget:
                failed.append(mode)
    if failed and not args.report:
        print(
            f"peak memory over budget ({', '.join(failed)}): find what grew "
            "(docs/development.md, Memory) before raising a budget",
            file=sys.stderr,
        )
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
