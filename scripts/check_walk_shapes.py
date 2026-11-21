#!/usr/bin/env python3
"""Analysis-cost gate: run heki on generated shapes at the largest size it still models, and fail
when one costs more than 1 s or 1 GiB beyond an empty module (docs/development.md, "Analysis
cost").

Each family is a source shape that grows with n: a chain of calls each handed the previous
result, conditional merges of call results, one iterator consumed by many loops, phi diamonds,
`isinstance` tests followed by `continue`, copies a loop carries, and so on. A family that lives
in one function is placed three ways (a function, a method, and a module body) and grown to the
largest n whose function the IR budget still models: n doubles, then bisects, with
`heki assumptions --json` telling a modeled function from an unmodeled one, so a change to the
builder moves the size with it. A family spread over a project (a wide class hierarchy, a chain
of private helpers) has a fixed size.

Each shape is checked with every rule selected and one contract of each kind naming the classes
and functions the shapes define, through `heki check` and through the `--watch` engine. The cost
is the wall time and the peak (the heap of the memory gate's counting build, or what the system
reports, see `check_memory.py`) minus those of an empty module; the time limit is scaled to the
machine by fixed linear shapes of calls and of loops.

`heki check --walk-steps` gives the steps each owner's walks took per function. They must grow
at most 2.5 times from n / 2 to n (quadratic growth inside the budget shows without timing
noise), and no budget may be spent on a family shaped like real code.

`KNOWN_STEPS` lists, the same way, the shapes whose steps are known to grow faster than linearly
or to spend a budget.

`KNOWN_OVER` lists the shapes known to cost more than the limits; the list only shrinks: a shape
listed there that costs less than a quarter of the limits fails the gate until it is removed. The
band between keeps a shape near the limit from failing on one machine and passing on another
(shapes scale differently across machines than the calibration shape does).

Usage:
    python3 scripts/check_walk_shapes.py --heki target/release/heki
    python3 scripts/check_walk_shapes.py --heki target/release/heki --only call_chain --report
"""

from __future__ import annotations

import argparse
import contextlib
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from check_memory import HEAP_LINE, MIB, execute  # noqa: E402

LIMIT_SECONDS = 1.0
LIMIT_MIB = 1024.0
# The seconds are those of the machine the limits were measured on. Fixed linear shapes, one of
# calls and one of loops, take these seconds there (beyond an empty module), and the time limit
# scales by the largest factor by which the machine running the gate is slower on one of them
# (never below 1): machines differ unevenly on calls and on loops, and the gated shapes do both.
REFERENCES = (
    ("with_statements", "function", 3104, 0.26),
    ("alias_loops", "function", 2000, 0.11),
)
# A run past this is over the limits whatever it would have taken.
TIMEOUT_SECONDS = 10.0
# The steps of an owner may grow this much from n / 2 to n.
MAX_STEP_GROWTH = 2.5
FIRST_N = 64
# Sizes past this are not tried: the source would pass megabytes before the IR budget.
MAX_N = 1 << 15

PLACED = ("function", "method", "module")

# Shapes known to cost more than the limits, with the walk that costs: (family, placement).
KNOWN_OVER: dict[tuple[str, str], str] = {
    ("call_chain", "module"): "MUT202 looks up each call's site in a scan of the function's call "
    "sites (`mut_rules::site_call`)",
    **{
        ("returned_merge_chain", p): "the roots of each merged value are listed again per merge, "
        "with the call sites through them (summary `iterations`)"
        for p in PLACED
    },
    **{
        ("one_iterator_many_loops", p): "MUT106 walks the blocks between every pair of loops "
        "consuming the iterator"
        for p in PLACED
    },
    **{
        ("doubled_zips", p): "the size of a `zip` asks both arguments without keeping answers: "
        "2^16 questions per loop (`FnIndex::iter_bound_depth`)"
        for p in PLACED
    },
    **{
        ("lists_extending_each_other", p): "building the IR overflows the stack: the display "
        "question recurses once per list extending the next (`FnIndex::display_at`)"
        for p in ("function", "method")
    },
    ("lists_extending_each_other", "module"): "building the IR of a module body walks the "
    "module-level names again per name (3 s; milliseconds in a function), and EFF107",
    ("copy_chain_in_loop", "module"): "building the IR of a module body walks module-level copy "
    "chains again per name (2.8 s; 0.02 s in a function)",
    ("alias_loops", "module"): "the OPT rules' module context walks the module's blocks once per "
    "block",
    ("reverse_chain_in_loop", "module"): "dense alias passes take one pass per link of a chain "
    "laid against block order (`grown_vars`, `aliases`)",
    ("wide_hierarchy", "project"): "each method call walks the whole subclass family of its "
    "receiver's class (`ProjectDecls::definitions`)",
    ("callers_of_one_helper", "project"): "MUT201 walks the helper's callers again per caller",
}

# Shapes known to take steps that grow faster than linearly or to spend a budget, with the walk:
# (family, placement). The list only shrinks: a listed shape that does neither fails the gate.
KNOWN_STEPS: dict[tuple[str, str], str] = {
    **{
        ("isinstance_continue", p): "the path facts of a loop header with many `continue`s "
        "(`Extractor::path_facts`, module bindings) grow quadratically and spend their budget"
        for p in PLACED
    },
}

HELPERS = '''import requests


class Base:
    LIMIT = 3

    def m(self):
        return self.LIMIT

    def reset(self):
        self.cache = []


def g(x):
    return x


def use(x):
    return x


def load():
    return [1, 2, 3]


'''

CONTRACTS = '''[[write-owners]]
name = "shape-writes"
targets = ["Base.cache", "Shapes.cache"]
allowed-writers = ["Base.*"]

[[read-owners]]
name = "shape-reads"
targets = ["Base.LIMIT"]
allowed-readers = ["Base.*"]
unresolved = "report"

[[call-owners]]
name = "shape-calls"
targets = ["Base.m", "g"]
allowed-callers = ["Base.*"]
unresolved = "report"

[[effect-boundaries]]
name = "shape-boundary"
scopes = ["*.shape", "shape", "entry"]
forbid = ["writes-targets", "blocking"]
write-targets = ["Base.*"]

[[readonly-returns]]
name = "shape-returns"
producers = ["load"]

[[invalidation]]
name = "shape-invalidation"
targets = ["Base.cache"]
invalidators = ["Base.reset"]
'''


@dataclass(frozen=True)
class Body:
    """The statements of a shape inside one function: its parameters and its lines, the last of
    which may be a `return`."""

    params: tuple[str, ...]
    lines: list[str]


def call_chain(n: int) -> Body:
    """Each call is handed the previous call's result."""
    lines = ["v0 = p", *(f"v{i} = g(v{i - 1})" for i in range(1, n + 1)), f"return v{n}"]
    return Body(("p", "session"), lines)


def returned_merge_chain(n: int) -> Body:
    """`x_i = g(x_{i-1}) if flag else x_{i-1}`, the last returned."""
    lines = [f"x{i} = g(x{i - 1}) if flag else x{i - 1}" for i in range(1, n + 1)]
    return Body(("x0", "flag"), [*lines, f"return x{n}"])


def one_iterator_many_loops(n: int) -> Body:
    """One iterator consumed by many loops after one another."""
    lines = ["it = iter(xs)"]
    for j in range(n):
        lines += ["for _ in it:", f"    out.append({j})"]
    return Body(("xs", "out"), lines)


def phi_diamonds(n: int) -> Body:
    """`if`/`else` merges each joining two values computed from the previous merge."""
    lines = ["v = xs"]
    for i in range(n):
        lines += [f"if c[{i}]:", "    v = v[:3]", "else:", "    v = v[1:4]"]
    return Body(("c", "xs"), [*lines, "for _ in v:", "    xs.append(1)"])


def isinstance_continue(n: int) -> Body:
    """A loop header's phi is its own input once per `continue`."""
    lines = ["acc = []", "while pending:", "    cur = pending.pop()"]
    for i in range(n):
        lines += [f"    if isinstance(cur, C{i}):", "        acc = acc + [cur]", "        continue"]
    return Body(("pending", "cur"), [*lines, "    acc.append(cur)", "return acc"])


def copy_chain_in_loop(n: int) -> Body:
    """A loop body copies one list along a chain, then calls a method on the last copy."""
    lines = ["for it in items:", "    y0 = xs"]
    lines += [f"    y{i} = y{i - 1}" for i in range(1, n + 1)]
    return Body(("items", "xs"), [*lines, f"    y{n}.remove(it)"])


def reverse_chain_in_loop(n: int) -> Body:
    """A loop carries a chain of copies laid against the order of the blocks."""
    lines = ["acc = []", *(f"x{i} = None" for i in range(1, n + 1)), "x0 = acc", "for it in items:"]
    lines += [f"    x{i} = x{i - 1}" for i in range(n, 0, -1)]
    lines += [f"    x{n}.append(it)", "    for _ in x0:", "        pass"]
    return Body(("items",), [*lines, "return acc"])


def alias_loops(n: int) -> Body:
    """Each loop iterates a new alias of one small display."""
    lines = ["a0 = [1, 2, 3]"]
    for i in range(1, n + 1):
        lines += [f"a{i} = a{i - 1}", f"for _ in a{i}:", "    xs.append(1)"]
    return Body(("xs",), lines)


def doubled_constant_slices(n: int) -> Body:
    """Integer constants doubling through `+`, each loop slicing by the last."""
    lines = ["c0 = 1", *(f"c{i} = c{i - 1} + c{i - 1}" for i in range(1, 17))]
    for j in range(n):
        lines += ["for _ in xs[:c16]:", f"    out.append({j})"]
    return Body(("xs", "out"), lines)


def doubled_zips(n: int) -> Body:
    """Each `zip` takes the previous one twice; many loops ask the size of the last."""
    lines = ["z0 = xs", *(f"z{i} = zip(z{i - 1}, z{i - 1})" for i in range(1, 17))]
    for j in range(n):
        lines += ["for _ in z16:", f"    out.append({j})"]
    return Body(("xs", "out"), lines)


def lists_extending_each_other(n: int) -> Body:
    """Lists each extended by the next, the last by the first."""
    lines = [f"a{i} = []" for i in range(n)]
    lines += [f"a{i}.extend(a{(i + 1) % n})" for i in range(n)]
    return Body(("src",), [*lines, "for _ in a0:", "    src.append(1)"])


def item_reads(n: int) -> Body:
    """Many item reads of one parameter."""
    return Body(("xs", "out"), [f"out.append(xs[{i}])" for i in range(n)])


def merged_flag_loop(n: int) -> Body:
    """A loop-carried flag merged many times from two inputs of its previous value."""
    lines = ["t = p is None", "for x in xs:"]
    lines += [f"    t = (not t) if c[{i}] else t" for i in range(n)]
    return Body(("xs", "c", "p"), [*lines, "if t:", "    requests.get(p)"])


def lazy_init_sites(n: int) -> Body:
    """One lazy initialization in a loop, many calls in its region."""
    lines = ["for it in items:", "    if self._s is None:", "        self._s = requests.Session()"]
    lines += [f"        self._s.get(it{i % 7})" for i in range(n)]
    return Body(("items",), [*lines, "    it.x = 1"])


def is_tests(n: int) -> Body:
    """Many `is` tests, each followed by a conditional expression on the tested parameter."""
    lines = []
    for i in range(n):
        lines += ["if p is None:", f"    x{i} = g(p) if p is None else p"]
    return Body(("p",), [*lines, "return p"])


def match_pass_through(n: int) -> Body:
    """Many `match` statements whose cases slice or pass the value on."""
    lines = ["v = xs"]
    for i in range(n):
        lines += ["match c:", f"    case {i}:", "        v = v[:3]", "    case _:", "        pass"]
    return Body(("c", "xs"), [*lines, "for _ in v:", "    xs.append(1)"])


def with_statements(n: int) -> Body:
    """Many `with` statements one after another."""
    lines = []
    for i in range(n):
        lines += [f"with open(p) as f{i}:", f"    use(f{i})"]
    return Body(("p",), lines)


def attribute_chain(n: int) -> Body:
    """Names each bound to an attribute of the one before."""
    lines = ["a0 = p", *(f"a{i} = a{i - 1}.attr" for i in range(1, n + 1))]
    return Body(("p",), [*lines, f"return a{n}"])


def path_aliases(n: int) -> Body:
    """Merges each joining the previous alias of a package's `__path__` and a new read of it,
    each alias read."""
    lines = ["p0 = requests.__path__"]
    for i in range(1, n + 1):
        lines += [f"p{i} = p{i - 1} if flag else requests.__path__", f"use(p{i}.count(x))"]
    return Body(("flag", "x"), lines)


def wide_hierarchy(n: int) -> str:
    """Many subclasses overriding one method, and as many functions calling it on the base."""
    lines = []
    for i in range(n):
        lines += [f"class C{i}(Base):", "    def m(self):", "        self.cache = []"]
        lines += ["        return super().m()", ""]
    for i in range(n):
        lines += [f"def use{i}(obj: Base):", "    obj.m()", "    return obj.LIMIT", ""]
    return "\n".join(lines)


def callers_of_one_helper(n: int) -> str:
    """One private helper writing an attribute, called by many functions."""
    lines = ["def _write(obj: Base):", "    obj.cache = []", ""]
    for i in range(n):
        lines += [f"def call{i}(obj: Base):", "    _write(obj)", "    return obj.m()", ""]
    return "\n".join(lines)


def helper_chain(n: int) -> str:
    """Private helpers each calling the next twice, the last mutating its parameter."""
    lines = []
    for i in range(n):
        lines += [f"def _h{i}(d):", f"    _h{i + 1}(d)", f"    _h{i + 1}(d)", ""]
    lines += [f"def _h{n}(d):", "    d.append(1)", "", "def entry(d):", "    _h0(d)", ""]
    return "\n".join(lines)


@dataclass(frozen=True)
class Family:
    name: str
    body: Callable[[int], Body] | None = None
    project: Callable[[int], str] | None = None
    placements: tuple[str, ...] = PLACED
    # The size of a project family.
    size: int = 0
    # Adversarial: its budgets may be spent.
    real_code: bool = True


FAMILIES = [
    Family("call_chain", call_chain),
    Family("returned_merge_chain", returned_merge_chain),
    Family("one_iterator_many_loops", one_iterator_many_loops),
    Family("phi_diamonds", phi_diamonds),
    Family("isinstance_continue", isinstance_continue),
    Family("copy_chain_in_loop", copy_chain_in_loop),
    Family("reverse_chain_in_loop", reverse_chain_in_loop),
    Family("alias_loops", alias_loops),
    Family("doubled_constant_slices", doubled_constant_slices, real_code=False),
    Family("doubled_zips", doubled_zips, real_code=False),
    Family("lists_extending_each_other", lists_extending_each_other),
    Family("item_reads", item_reads),
    Family("merged_flag_loop", merged_flag_loop),
    Family("is_tests", is_tests),
    Family("match_pass_through", match_pass_through),
    Family("with_statements", with_statements),
    Family("attribute_chain", attribute_chain),
    Family("path_aliases", path_aliases),
    Family("lazy_init_sites", lazy_init_sites, placements=("method",)),
    Family("wide_hierarchy", project=wide_hierarchy, placements=("project",), size=4000),
    Family("helper_chain", project=helper_chain, placements=("project",), size=2000),
    Family("callers_of_one_helper", project=callers_of_one_helper, placements=("project",), size=4000),
]


def source(family: Family, placement: str, n: int) -> str:
    if family.project is not None:
        return HELPERS + family.project(n)
    assert family.body is not None
    body = family.body(n)
    if placement == "module":
        lines = [f"{p} = load()" for p in body.params]
        lines += [("result = " + line[7:]) if line.startswith("return ") else line for line in body.lines]
        return HELPERS + "\n".join(lines) + "\n"
    if placement == "method":
        head = f"class Shapes(Base):\n    def shape(self, {', '.join(body.params)}):\n"
        indent = " " * 8
    else:
        head = f"def shape({', '.join(body.params)}):\n"
        indent = " " * 4
    return HELPERS + head + "".join(indent + line + "\n" for line in body.lines)


def peak_mib(stderr: str, usage: os.struct_rusage) -> float:
    """The peak of a run: the heap a counting build reports (the memory gate's build), else the
    physical footprint `/usr/bin/time -l` reports on macOS, else the resident set size (0 for a
    run that was stopped before reporting)."""
    for pattern in (HEAP_LINE, re.compile(r"(\d+)\s+peak memory footprint")):
        found = pattern.search(stderr)
        if found:
            return int(found.group(1)) / MIB
    # `ru_maxrss` is in KiB on Linux; macOS runs report their footprint above.
    return usage.ru_maxrss / 1024 if sys.platform != "darwin" else 0.0


class Runner:
    def __init__(self, heki: str, root: Path) -> None:
        self.heki = heki
        self.root = root
        codes = json.loads(self.run_text(["rule", "--all", "--output-format", "json"]))
        self.select = ",".join(rule["code"] for rule in codes)
        (root / "pyproject.toml").write_text('[project]\nname = "shapes"\n')
        (root / "shapes.toml").write_text(CONTRACTS)

    def run_text(self, args: list[str]) -> str:
        done = subprocess.run([self.heki, *args], cwd=self.root, capture_output=True, text=True)
        if done.returncode not in (0, 1):
            sys.exit(f"heki {' '.join(args)} exited with {done.returncode}:\n{done.stderr}")
        return done.stdout

    def write(self, text: str) -> None:
        (self.root / "m.py").write_text(text)

    def modeled(self, text: str) -> bool | str:
        """Whether the IR budget models every function of `text`, or why building the IR failed
        (a crash, or a run past `TIMEOUT_SECONDS`, which is stopped with its process group)."""
        self.write(text)
        args = [self.heki, "assumptions", "--json", "m.py"]
        proc = subprocess.Popen(
            args,
            cwd=self.root,
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            start_new_session=True,
        )
        try:
            stdout, _ = proc.communicate(timeout=TIMEOUT_SECONDS)
        except subprocess.TimeoutExpired:
            with contextlib.suppress(ProcessLookupError):
                os.killpg(proc.pid, signal.SIGKILL)
            proc.communicate()
            return f"ran past {TIMEOUT_SECONDS:.0f} s"
        if proc.returncode not in (0, 1):
            return f"crashed (exit code {proc.returncode})"
        return not any(h["kind"] == "unmodeled" for h in json.loads(stdout))

    def largest_modeled(self, family: Family, placement: str) -> tuple[int, str | None]:
        """The largest n the IR budget models, or the size at which building the IR failed and
        why."""
        sizes = [FIRST_N]
        while sizes[-1] * 2 <= MAX_N:
            sizes.append(sizes[-1] * 2)
        low = 0
        for n in sizes:
            modeled = self.modeled(source(family, placement, n))
            if isinstance(modeled, str):
                return n, modeled
            if not modeled:
                break
            low = n
        else:
            return low, None
        if low == 0:
            sys.exit(f"{family.name}/{placement}: unmodeled at n = {FIRST_N}")
        high = low * 2
        while high - low > max(1, low // 64):
            mid = (low + high) // 2
            modeled = self.modeled(source(family, placement, mid))
            if isinstance(modeled, str):
                return mid, modeled
            if modeled:
                low = mid
            else:
                high = mid
        return low, None

    def check(
        self, text: str, watch: bool, steps: Path | None = None, timeout: float = TIMEOUT_SECONDS
    ) -> tuple[float, float, int]:
        self.write(text)
        args = ["check", "--select", self.select, "--config", "shapes.toml"]
        args += ["--output-format", "json", "m.py"]
        env = {}
        if watch:
            args.insert(1, "--watch")
            env["HEKI_WATCH_MAX_RUNS"] = "0"
        if steps is not None:
            args[1:1] = ["--walk-steps", str(steps)]
        darwin = sys.platform == "darwin"
        seconds, code, stderr, usage = execute(self.heki, args, self.root, env, darwin, timeout)
        return seconds, peak_mib(stderr, usage), code

    def steps(self, text: str) -> dict[tuple[str, str], tuple[int, bool]] | None:
        """The steps of each owner and function, `None` when the run failed: only a run that
        ends cleanly (exit code 0 or 1) writes records to trust."""
        path = self.root / "steps.json"
        path.unlink(missing_ok=True)
        _, _, code = self.check(text, watch=False, steps=path)
        if code not in (0, 1) or not path.exists():
            return None
        records = json.loads(path.read_text())
        out: dict[tuple[str, str], tuple[int, bool]] = {}
        for r in records:
            key = (r["owner"], r["function"] or "")
            taken, spent = out.get(key, (0, False))
            out[key] = (taken + r["taken"], spent or r["spent"])
        return out


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--heki", required=True, help="path to the heki binary (release build)")
    parser.add_argument("--only", action="append", default=[], help="run only these families")
    parser.add_argument("--report", action="store_true", help="print the costs, never fail")
    args = parser.parse_args()
    heki = str(Path(args.heki).resolve())
    families = [f for f in FAMILIES if not args.only or f.name in args.only]
    problems: list[str] = []
    with tempfile.TemporaryDirectory(prefix="heki-walk-shapes-") as tmp:
        runner = Runner(heki, Path(tmp))
        # Only runs that end cleanly measure anything: a crash or a stopped run would make the
        # baseline or a reference say nothing, or inflate the limit.
        empty = [runner.check(HELPERS, watch=False) for _ in range(3)]
        empty = [run for run in empty if run[2] in (0, 1)]
        if not empty:
            print("calibration failed: no run of the empty module ended cleanly", file=sys.stderr)
            return 1
        base_seconds = min(run[0] for run in empty)
        base_peak = min(run[1] for run in empty)
        print(f"empty module: {base_seconds:.2f} s, {base_peak:.0f} MiB", flush=True)
        factor = 1.0
        for name, placement, n, seconds in REFERENCES:
            reference = source(next(f for f in FAMILIES if f.name == name), placement, n)
            runs = [runner.check(reference, watch=False) for _ in range(3)]
            runs = [run for run in runs if run[2] in (0, 1)]
            if not runs:
                print(
                    f"calibration failed: no run of {name}/{placement} ended cleanly",
                    file=sys.stderr,
                )
                return 1
            took = min(run[0] for run in runs) - base_seconds
            print(f"reference {name}/{placement} n={n}: {took:.2f} s ({seconds:.2f} s on the "
                  "machine of the limits)", flush=True)
            factor = max(factor, took / seconds)
        limit_seconds = LIMIT_SECONDS * factor
        print(f"time limit on this machine: {limit_seconds:.2f} s", flush=True)
        for family in families:
            for placement in family.placements:
                shape = (family.name, placement)
                if family.project is not None:
                    n, failed = family.size, None
                else:
                    n, failed = runner.largest_modeled(family, placement)
                text = source(family, placement, n)
                listed = shape in KNOWN_OVER
                over = []
                # Whether a listed shape costs clearly less than the limits.
                well_under = failed is None
                if failed is not None:
                    print(f"{family.name}/{placement} n={n}: building the IR {failed}", flush=True)
                    over.append("build")
                for watch in () if failed is not None else (False,) if listed else (False, True):
                    # A listed shape only has to show when it drops under the limits.
                    timeout = base_seconds + 2 * limit_seconds if listed else TIMEOUT_SECONDS
                    seconds, peak, code = runner.check(text, watch, timeout=timeout)
                    cost = (seconds - base_seconds, peak - base_peak)
                    if not listed and (cost[0] > limit_seconds or cost[1] > LIMIT_MIB or code == -9):
                        # Once more, against noise.
                        seconds, peak, code = runner.check(text, watch)
                        cost = (seconds - base_seconds, peak - base_peak)
                    mode = "watch" if watch else "check"
                    if code == -9:
                        spent = f"stopped after {cost[0]:.0f} s"
                    elif code not in (0, 1):
                        spent = f"crashed (exit code {code})"
                    else:
                        spent = f"{cost[0]:.2f} s, {cost[1]:.0f} MiB"
                    print(f"{family.name}/{placement} n={n} {mode}: {spent}", flush=True)
                    if cost[0] > limit_seconds or cost[1] > LIMIT_MIB or code not in (0, 1):
                        over.append(mode)
                    if cost[0] > limit_seconds / 4 or cost[1] > LIMIT_MIB / 4 or code not in (0, 1):
                        well_under = False
                if over and not listed:
                    problems.append(f"{family.name}/{placement} at n={n} costs more than "
                                    f"{limit_seconds:.1f} s or {LIMIT_MIB:.0f} MiB ({', '.join(over)})")
                if listed and well_under:
                    problems.append(f"{family.name}/{placement} costs less than a quarter of the limits: "
                                    "remove it from KNOWN_OVER")
                if family.project is None and not over:
                    steps = step_growth(runner, family, placement, n, text)
                    if shape not in KNOWN_STEPS:
                        problems += steps
                    elif not steps:
                        problems.append(f"{family.name}/{placement} takes linear steps and spends no "
                                        "budget: remove it from KNOWN_STEPS")
    for problem in problems:
        print(problem, file=sys.stderr)
    return 1 if problems and not args.report else 0


def step_growth(runner: Runner, family: Family, placement: str, n: int, text: str) -> list[str]:
    """The owners whose steps grow more than `MAX_STEP_GROWTH` times from n / 2 to n, and the
    budgets spent on a family shaped like real code."""
    full = runner.steps(text)
    half = runner.steps(source(family, placement, n // 2))
    if full is None or half is None:
        return [f"{family.name}/{placement}: the steps could not be counted (a run failed)"]
    out = []
    for key, (taken, spent) in sorted(full.items()):
        before = half.get(key, (0, False))[0]
        if before and taken / before > MAX_STEP_GROWTH:
            out.append(f"{family.name}/{placement}: {key[0]} {key[1]} takes {before} steps at "
                       f"n={n // 2} and {taken} at n={n}")
        if spent and family.real_code:
            out.append(f"{family.name}/{placement}: {key[0]} spends its budget on {key[1]}")
    return out


if __name__ == "__main__":
    sys.exit(main())
