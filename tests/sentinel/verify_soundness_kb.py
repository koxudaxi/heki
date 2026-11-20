#!/usr/bin/env python3
"""Layer 1 sentinel: verify `kb/soundness/*.toml` against the running CPython (3.11-3.15).

Checks (docs/ANALYSIS_AND_KB_SPEC.md section 4.1):
  1. No advisory (Layer 2) field and no unknown field appears in a Layer 1 file.
  2. Every `retained_in_return` API returns a derived reference that observes a later mutation of
     the retained argument, and the KB marks exactly the argument positions the scenario retains.
  3. Every `return_exact_type` API satisfies `type(result) is expected_type`, also for adversarial
     inputs (subclass instances, dunders returning subclasses).
Supporting checks:
  - every symbol resolves on this interpreter;
  - every API returning a fresh mutable container does not alias its argument;
  - the reason `builtins.str` / `builtins.bytes` carry no `return_exact_type` still holds.

Usage: python3.15 tests/sentinel/verify_soundness_kb.py   (exit status 0 = no failure)
"""
from __future__ import annotations

import builtins
import importlib
import sys
import tomllib
import warnings
from collections.abc import Callable
from pathlib import Path
from typing import Any

KB_DIR = Path(__file__).resolve().parents[2] / "kb" / "soundness"

ALLOWED_FIELDS = {"symbol", "return_exact_type", "param_effects", "verified_max_python"}
REQUIRED_FIELDS = {"symbol", "param_effects", "verified_max_python"}
# Layer 2 (advisory) fields: they must never appear in Layer 1 (ADR-005).
ADVISORY_FIELDS = {
    "effects",
    "async_safe",
    "offloads_blocking_arg",
    "excluded",
    "copy_sanitizer",
    "opaque_to_layer3",
}
PARAM_EFFECTS = {"read_only", "mutated", "retained_in_return", "escaped"}


# --------------------------------------------------------------------------- adversarial types


class ListSub(list):
    pass


class SetSub(set):
    pass


class DictSub(dict):
    pass


class TupleSub(tuple):
    pass


class IntSub(int):
    pass


class FloatSub(float):
    pass


class StrSub(str):
    pass


class BytesSub(bytes):
    pass


class LenReturnsBool:
    def __len__(self) -> bool:
        return True


class BoolDunder:
    def __bool__(self) -> bool:
        return False


class IntDunderSub:
    def __int__(self) -> int:
        return IntSub(7)


class IndexDunderSub:
    def __index__(self) -> int:
        return IntSub(8)


class FloatDunderSub:
    def __float__(self) -> float:
        return FloatSub(1.5)


class StrDunderSub:
    def __str__(self) -> str:
        return StrSub("s")


class BytesDunderSub:
    def __bytes__(self) -> bytes:
        return BytesSub(b"b")


# --------------------------------------------------------------------------- check 2 scenarios
# Each scenario receives the resolved KB callable, builds a derived reference, mutates the
# argument(s) at the declared positions, and asserts the reference observes the mutation.


def _iter(fn: Callable[..., Any]) -> None:
    x = [1, 2]
    it = fn(x)
    x.append(3)
    assert list(it) == [1, 2, 3], "iterator must see the appended element"


def _enumerate(fn: Callable[..., Any]) -> None:
    x = [1, 2]
    e = fn(x)
    x.append(3)
    assert list(e) == [(0, 1), (1, 2), (2, 3)]


def _zip(fn: Callable[..., Any]) -> None:
    a, b = [1, 2], [3, 4]
    z = fn(a, b)
    a.append(5)
    b.append(6)
    assert list(z) == [(1, 3), (2, 4), (5, 6)], "zip retains every iterable argument"


def _map(fn: Callable[..., Any]) -> None:
    x = [1, 2]
    m = fn(str, x)
    x.append(3)
    assert list(m) == ["1", "2", "3"]


def _filter(fn: Callable[..., Any]) -> None:
    x = [1, 2]
    f = fn(None, x)
    x.append(3)
    assert list(f) == [1, 2, 3]


def _reversed(fn: Callable[..., Any]) -> None:
    x = [1, 2, 3]
    r = fn(x)
    x[0] = 9
    assert list(r) == [3, 2, 9], "reversed reads the live sequence"


def _memoryview(fn: Callable[..., Any]) -> None:
    buf = bytearray(b"ab")
    view = fn(buf)
    buf[0] = ord("z")
    assert view.tobytes() == b"zb", "memoryview shares the buffer"


def _dict_keys(fn: Callable[..., Any]) -> None:
    d = {"a": 1}
    view = fn(d)
    d["b"] = 2
    assert list(view) == ["a", "b"]


def _dict_values(fn: Callable[..., Any]) -> None:
    d = {"a": 1}
    view = fn(d)
    d["a"] = 5
    d["b"] = 2
    assert list(view) == [5, 2]


def _dict_items(fn: Callable[..., Any]) -> None:
    d = {"a": 1}
    view = fn(d)
    d["a"] = 5
    d["b"] = 2
    assert list(view) == [("a", 5), ("b", 2)]


# symbol -> (argument positions the scenario shows are retained, scenario)
RETAINED_SCENARIOS: dict[str, tuple[set[int], Callable[[Callable[..., Any]], None]]] = {
    "builtins.iter": ({0}, _iter),
    "builtins.enumerate": ({0}, _enumerate),
    # Variadic: the last declared effect covers the remaining iterables (scenario checks two).
    "builtins.zip": ({0}, _zip),
    "builtins.map": ({1}, _map),
    "builtins.filter": ({1}, _filter),
    "builtins.reversed": ({0}, _reversed),
    "builtins.memoryview": ({0}, _memoryview),
    "builtins.dict.keys": ({0}, _dict_keys),
    "builtins.dict.values": ({0}, _dict_values),
    "builtins.dict.items": ({0}, _dict_items),
}

# --------------------------------------------------------------------------- check 3 cases
# symbol -> argument tuples; every call must return exactly the KB's `return_exact_type`.

EXACT_TYPE_CASES: dict[str, list[tuple[Any, ...]]] = {
    "builtins.len": [([1, 2],), (ListSub([1]),), ("abc",), (LenReturnsBool(),)],
    "builtins.bool": [(0,), ([1],), (IntSub(1),), (BoolDunder(),)],
    "builtins.int": [
        (),
        ("12",),
        ("ff", 16),
        (3.7,),
        (True,),
        (IntSub(4),),
        (IntDunderSub(),),
        (IndexDunderSub(),),
    ],
    "builtins.float": [
        (),
        ("1.5",),
        (2,),
        (True,),
        (FloatSub(2.5),),
        (FloatDunderSub(),),
        (IndexDunderSub(),),
    ],
    "builtins.list": [(), ((1, 2),), (ListSub([1]),), ({"a": 1},), (iter([1]),)],
    "builtins.set": [(), ([1, 1],), (SetSub({1}),), (frozenset({1}),)],
    "builtins.dict": [(), ({"a": 1},), (DictSub(a=1),), ([("a", 1)],)],
    "builtins.tuple": [(), ([1],), ((1, 2),), (TupleSub((1,)),)],
    "builtins.list.copy": [([1],), (ListSub([1]),)],
    "builtins.set.copy": [({1},), (SetSub({1}),)],
    "builtins.dict.copy": [({"a": 1},), (DictSub(a=1),)],
}

# --------------------------------------------------------------------------- supporting checks
# symbol -> (fresh mutable argument factory, mutation of that argument)
FRESH_CONTAINER_CASES: dict[str, tuple[Callable[[], Any], Callable[[Any], None]]] = {
    "builtins.list": (lambda: [1, 2], lambda a: a.append(3)),
    "builtins.set": (lambda: {1, 2}, lambda a: a.add(3)),
    "builtins.dict": (lambda: {"a": 1}, lambda a: a.update(b=2)),
    "builtins.list.copy": (lambda: [1, 2], lambda a: a.append(3)),
    "builtins.set.copy": (lambda: {1, 2}, lambda a: a.add(3)),
    "builtins.dict.copy": (lambda: {"a": 1}, lambda a: a.update(b=2)),
}
MUTABLE_EXACT_TYPES = {"list", "set", "dict"}

# Symbols that must NOT carry `return_exact_type`, with a call returning a non-exact type.
NON_EXACT_WITNESSES: dict[str, tuple[Any, type]] = {
    "builtins.str": (StrDunderSub(), str),
    "builtins.bytes": (BytesDunderSub(), bytes),
}


# --------------------------------------------------------------------------- driver


def resolve(symbol: str) -> Any:
    head, *rest = symbol.split(".")
    obj: Any = importlib.import_module(head)
    for part in rest:
        obj = getattr(obj, part)
    return obj


def _resolves(symbol: str) -> bool:
    try:
        resolve(symbol)
    except (ImportError, AttributeError):
        return False
    return True


def load_kb() -> tuple[list[tuple[str, dict[str, Any]]], list[str]]:
    """Return `(entries with their file name, structural failures)`."""
    entries: list[tuple[str, dict[str, Any]]] = []
    failures: list[str] = []
    files = sorted(KB_DIR.glob("*.toml"))
    if not files:
        failures.append(f"no Layer 1 TOML file found under {KB_DIR}")
    for path in files:
        with path.open("rb") as fh:
            data = tomllib.load(fh)
        for key in sorted(set(data) - {"entries"}):
            failures.append(f"{path.name}: unknown top-level key `{key}`")
        for raw in data.get("entries", []):
            entries.append((path.name, raw))
    return entries, failures


class Report:
    def __init__(self) -> None:
        self.failures: list[str] = []
        self.passes = 0

    def check(self, name: str, problems: list[str]) -> None:
        if problems:
            for problem in problems:
                print(f"  FAIL {name}: {problem}")
            self.failures.extend(problems)
        else:
            print(f"  PASS {name}")
            self.passes += 1


def check_fields(entries: list[tuple[str, dict[str, Any]]]) -> list[str]:
    problems = []
    seen: set[str] = set()
    for file, raw in entries:
        symbol = raw.get("symbol", "<missing symbol>")
        for key in sorted(set(raw) & ADVISORY_FIELDS):
            problems.append(f"{file}: {symbol}: advisory field `{key}` in Layer 1")
        for key in sorted(set(raw) - ALLOWED_FIELDS - ADVISORY_FIELDS):
            problems.append(f"{file}: {symbol}: unknown field `{key}`")
        for key in sorted(REQUIRED_FIELDS - set(raw)):
            problems.append(f"{file}: {symbol}: missing field `{key}`")
        for effect in raw.get("param_effects", []):
            if effect not in PARAM_EFFECTS:
                problems.append(f"{file}: {symbol}: unknown param effect `{effect}`")
        if symbol in seen:
            problems.append(f"{file}: duplicate symbol `{symbol}`")
        seen.add(symbol)
    return problems


def check_tables_match_kb(kb: dict[str, dict[str, Any]]) -> list[str]:
    problems = []
    tables = [RETAINED_SCENARIOS, EXACT_TYPE_CASES, FRESH_CONTAINER_CASES, NON_EXACT_WITNESSES]
    for symbol in sorted(set().union(*tables) - set(kb)):
        problems.append(f"{symbol}: sentinel table entry but the KB has no such entry")
    return problems


def check_resolves(kb: dict[str, dict[str, Any]]) -> list[str]:
    problems = []
    for symbol, raw in kb.items():
        try:
            resolve(symbol)
        except (ImportError, AttributeError) as exc:
            problems.append(f"{symbol}: does not resolve ({exc!r})")
        expected = raw.get("return_exact_type")
        if expected is not None and not isinstance(getattr(builtins, expected, None), type):
            problems.append(f"{symbol}: return_exact_type `{expected}` is not a builtins type")
    return problems


def check_retained(kb: dict[str, dict[str, Any]]) -> list[str]:
    problems = []
    for symbol, raw in kb.items():
        retained = {i for i, e in enumerate(raw["param_effects"]) if e == "retained_in_return"}
        if not retained:
            continue
        if symbol not in RETAINED_SCENARIOS:
            problems.append(f"{symbol}: retained_in_return has no sentinel scenario")
            continue
        positions, scenario = RETAINED_SCENARIOS[symbol]
        if retained != positions:
            problems.append(
                f"{symbol}: KB marks positions {sorted(retained)} retained, "
                f"scenario retains {sorted(positions)}"
            )
        try:
            scenario(resolve(symbol))
        except Exception as exc:  # noqa: BLE001 - report every failure kind
            problems.append(f"{symbol}: {type(exc).__name__}: {exc}")
    return problems


def check_exact_types(kb: dict[str, dict[str, Any]]) -> list[str]:
    problems = []
    for symbol, raw in kb.items():
        expected_name = raw.get("return_exact_type")
        if expected_name is None:
            continue
        cases = EXACT_TYPE_CASES.get(symbol)
        if not cases:
            problems.append(f"{symbol}: return_exact_type has no sentinel cases")
            continue
        expected = getattr(builtins, expected_name)
        fn = resolve(symbol)
        for args in cases:
            try:
                with warnings.catch_warnings():
                    # `__int__` / `__float__` returning a subclass is deprecated but still accepted.
                    warnings.simplefilter("ignore", DeprecationWarning)
                    result = fn(*args)
            except Exception as exc:  # noqa: BLE001
                problems.append(f"{symbol}{args!r}: raised {type(exc).__name__}: {exc}")
                continue
            if type(result) is not expected:
                problems.append(
                    f"{symbol}{args!r}: type(result) is {type(result).__qualname__}, "
                    f"expected exactly {expected_name}"
                )
    return problems


def check_fresh_containers(kb: dict[str, dict[str, Any]]) -> list[str]:
    problems = []
    for symbol, raw in kb.items():
        if raw.get("return_exact_type") not in MUTABLE_EXACT_TYPES:
            continue
        if symbol not in FRESH_CONTAINER_CASES:
            problems.append(f"{symbol}: fresh mutable container has no sentinel case")
            continue
        make_arg, mutate = FRESH_CONTAINER_CASES[symbol]
        arg = make_arg()
        result = resolve(symbol)(arg)
        snapshot = type(result)(result)
        mutate(arg)
        if result is arg or result != snapshot:
            problems.append(f"{symbol}: result aliases its argument")
    return problems


def check_non_exact_witnesses(kb: dict[str, dict[str, Any]]) -> list[str]:
    problems = []
    for symbol, (arg, base) in NON_EXACT_WITNESSES.items():
        raw = kb.get(symbol)
        if raw is None:
            problems.append(f"{symbol}: KB has no such entry")
            continue
        if raw.get("return_exact_type") is not None:
            problems.append(f"{symbol}: must not carry return_exact_type (see witness)")
        result = resolve(symbol)(arg)
        if type(result) is base:
            problems.append(
                f"{symbol}: witness now returns exactly {base.__name__}; the KB comment is stale "
                "and return_exact_type may be reconsidered"
            )
    return problems


def main() -> int:
    print(f"verify_soundness_kb: CPython {sys.version.split()[0]} ({KB_DIR})")
    report = Report()
    entries, structural = load_kb()
    report.check("layer1-structure", structural)
    report.check("no-advisory-or-unknown-fields", check_fields(entries))
    kb = {raw["symbol"]: raw for _, raw in entries if "symbol" in raw and "param_effects" in raw}

    current = sys.version_info[:2]
    unverified = [
        s
        for s, raw in kb.items()
        if (raw["verified_max_python"]["major"], raw["verified_max_python"]["minor"]) < current
    ]
    if unverified:
        print(
            f"  NOTE {len(unverified)} entries have verified_max_python < {current[0]}.{current[1]} "
            "(heki drops them for this target); checked anyway"
        )

    report.check("sentinel-tables-match-kb", check_tables_match_kb(kb))
    report.check("symbols-resolve", check_resolves(kb))
    # Unresolvable symbols are reported above; the dynamic checks run on the rest.
    live = {s: raw for s, raw in kb.items() if _resolves(s)}
    report.check("retained-in-return", check_retained(live))
    report.check("return-exact-type", check_exact_types(live))
    report.check("fresh-containers-do-not-alias", check_fresh_containers(live))
    report.check("non-exact-witnesses", check_non_exact_witnesses(live))
    print(
        f"{len(kb)} entries, {report.passes} checks passed, {len(report.failures)} failures."
    )
    return 1 if report.failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
