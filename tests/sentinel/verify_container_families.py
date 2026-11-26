#!/usr/bin/env python3
"""Sentinel of the container family table: verify `ContainerFamily::TABLE`
(`crates/heki_effect/src/readonly.rs`) against the running CPython (3.11-3.15).

The table says, for each class an annotation or a base resolves to, which family of `list`,
`dict`, and `set` it belongs to and what it is there; `ProjectDecls::implementers` reads it to list
the project classes a value of a builtin or of a container ABC may be (R2). Checks:
  1. Every row that resolves here is in its family as the row says: a builtin row is the builtin
     (or its `typing` alias), an ABC row the ABC, and every other row a subclass of the read-only
     ABC whose levels the row admits (deriving from the builtin, an instance of the mutable ABC);
     a row whose read methods (those the read-only ABC lists) are Python code is not `Native`.
     A row that does not resolve is one of the rows of other versions (`ROWS_ELSEWHERE`).
  2. The table is complete: every public class of the standard library that a class can derive
     from and that is a subclass of a family's read-only ABC has a row, under each public name a
     module gives it (its own module, or a name in the module's `__all__`).
  3. The ABCs of the rows are nominal: a class with every method of the ABC that neither derives
     from it nor is registered with it is no subclass, while the structural ABCs (`Iterable`,
     `Collection`, `Sized`, ...) accept it, and none of those has a row.
  4. What opens a family: a class registered with an ABC, and its subclasses, are subclasses of
     the ABC; a `__subclasshook__` of a subclass of the ABC makes any class one.

Usage: python3.15 tests/sentinel/verify_container_families.py   (exit status 0 = no failure)
"""

from __future__ import annotations

import collections.abc as cabc
import contextlib
import importlib
import inspect
import io
import pkgutil
import re
import sys
import typing
import warnings
from pathlib import Path

TABLE = Path(__file__).resolve().parents[2] / "crates" / "heki_effect" / "src" / "readonly.rs"
ROW = re.compile(
    r'row!\(\s*"([^"]+)",\s*(List|Dict|Set),\s*'
    r"(Builtin|Derived|Lineage|ReadOnly|Mutable|Abc"
    r"|(?:Native|Implements) \{ mutable: (?:true|false) \}),?\s*\)"
)

# kind -> (builtin, read-only ABC, mutable ABC)
FAMILIES = {
    "List": (list, cabc.Sequence, cabc.MutableSequence),
    "Dict": (dict, cabc.Mapping, cabc.MutableMapping),
    "Set": (set, cabc.Set, cabc.MutableSet),
}

# The methods of each family's read-only ABC (`ContainerKind::read_only_methods`).
READS = {
    "List": [
        "index",
        "count",
        "__getitem__",
        "__len__",
        "__contains__",
        "__iter__",
        "__reversed__",
    ],
    "Dict": [
        "get",
        "keys",
        "items",
        "values",
        "__getitem__",
        "__len__",
        "__contains__",
        "__iter__",
        "__eq__",
        "__ne__",
    ],
    "Set": [
        "isdisjoint",
        "__contains__",
        "__len__",
        "__iter__",
        "__le__",
        "__lt__",
        "__ge__",
        "__gt__",
        "__eq__",
        "__ne__",
        "__and__",
        "__or__",
        "__sub__",
        "__xor__",
    ],
}

# Rows of classes another version has: (row, first version with it, first version without it).
ROWS_ELSEWHERE = {
    "configparser.SafeConfigParser": (None, (3, 12)),
    "distutils.command.bdist.ListCompat": (None, (3, 12)),
    "distutils.dir_util.SkipRepeatAbsolutePaths": (None, (3, 12)),
    "importlib.metadata.DeprecatedList": (None, (3, 12)),
    "importlib.metadata.SelectableGroups": (None, (3, 12)),
    "nntplib.ArticleInfo": (None, (3, 13)),
    "nntplib.GroupInfo": (None, (3, 13)),
    "sndhdr.SndHeaders": (None, (3, 13)),
    "importlib.resources.Resource": (None, (3, 13)),
    "enum.EnumDict": ((3, 13), None),
    "multiprocessing.managers.SetProxy": ((3, 14), None),
    "platform.AndroidVer": ((3, 13), None),
    "platform.IOSVersionInfo": ((3, 13), None),
    "symtable.SymbolTableType": ((3, 13), None),
    "builtins.frozendict": ((3, 15), None),
}

# Rows of classes that only some builds have (Tk), or that join their family in a later version
# (a row of a class outside its family only makes more classes values of it).
OPTIONAL_ROWS = {"tkinter.EventType", "turtle.Vec2D"}
FAMILY_SINCE = {
    "multiprocessing.managers.BaseListProxy": (3, 14),
    "multiprocessing.managers.ListProxy": (3, 14),
    "multiprocessing.managers.DictProxy": (3, 14),
    "multiprocessing.managers.SetProxy": (3, 14),
}

STRUCTURAL = [cabc.Iterable, cabc.Collection, cabc.Sized, cabc.Container, cabc.Reversible]

# Modules whose import has effects beyond defining names, and the test suite.
SKIPPED = {"antigravity", "this", "idlelib", "turtledemo", "test", "__phello__", "__hello__"}


def load_table() -> list[tuple[str, str, str]]:
    text = TABLE.read_text(encoding="utf-8")
    body = text[text.index("pub const TABLE: &[Self]") :]
    body = body[: body.index("];")]
    return [m.groups() for m in ROW.finditer(body)]


def quiet_import(name: str):
    with warnings.catch_warnings(), contextlib.redirect_stdout(io.StringIO()):
        warnings.simplefilter("ignore")
        with contextlib.redirect_stderr(io.StringIO()):
            return importlib.import_module(name)


def resolve(fqn: str):
    """The object `fqn` names here, or `None`."""
    if fqn == "typing.NamedTuple":
        point = typing.NamedTuple("Point", [("x", int)])
        return point
    parts = fqn.split(".")
    for cut in range(len(parts) - 1, 0, -1):
        try:
            obj = quiet_import(".".join(parts[:cut]))
        except BaseException:  # noqa: BLE001 - a module this build lacks
            continue
        for attr in parts[cut:]:
            # `ByteString` warns that it goes in 3.17.
            with warnings.catch_warnings():
                warnings.simplefilter("ignore", DeprecationWarning)
                obj = getattr(obj, attr, None)
            if obj is None:
                return None
        return typing.get_origin(obj) or obj
    return None


def levels_of_role(role: str) -> set[str]:
    if role in ("Builtin", "Derived", "Lineage"):
        return {"builtin", "mutable", "read-only"}
    if role == "Mutable" or role.endswith("{ mutable: true }"):
        return {"mutable", "read-only"}
    return {"read-only"}


def python_reads(cls: type, kind: str) -> bool:
    """Whether a read method of the family's read-only ABC is Python code on `cls`."""
    for name in READS[kind]:
        found = inspect.getattr_static(cls, name, None)
        if inspect.isfunction(found) or isinstance(found, (classmethod, staticmethod, property)):
            return True
    return False


def check_rows(rows: list[tuple[str, str, str]]) -> list[str]:
    problems = []
    version = sys.version_info[:2]
    for fqn, kind, role in rows:
        builtin, read_only, mutable = FAMILIES[kind]
        cls = resolve(fqn)
        if cls is None:
            since, until = ROWS_ELSEWHERE.get(fqn, (None, None))
            elsewhere = fqn in ROWS_ELSEWHERE and (
                (since is not None and version < since) or (until is not None and version >= until)
            )
            if not elsewhere and fqn not in OPTIONAL_ROWS:
                problems.append(f"{fqn}: does not resolve on this interpreter")
            continue
        if fqn in ROWS_ELSEWHERE and not fqn.startswith("builtins."):
            since, until = ROWS_ELSEWHERE[fqn]
            if (since and version < since) or (until and version >= until):
                problems.append(
                    f"{fqn}: resolves here, outside the versions `ROWS_ELSEWHERE` gives"
                )
        if not isinstance(cls, type):
            problems.append(f"{fqn}: is no class ({cls!r})")
            continue
        if role == "Builtin":
            if cls is not builtin:
                problems.append(f"{fqn}: a builtin row, but it is {cls!r}")
            continue
        if role in ("ReadOnly", "Mutable"):
            if cls is not (read_only if role == "ReadOnly" else mutable):
                problems.append(f"{fqn}: an ABC row, but it is {cls!r}")
            continue
        if not issubclass(cls, read_only):
            if version < FAMILY_SINCE.get(fqn, (0, 0)):
                continue
            problems.append(f"{fqn}: is no {read_only.__name__}")
            continue
        observed = {"read-only"}
        if issubclass(cls, mutable):
            observed.add("mutable")
        if issubclass(cls, builtin):
            observed.add("builtin")
        if role == "Derived" and "builtin" not in observed:
            problems.append(f"{fqn}: a derived row, but it does not derive from {builtin.__name__}")
        if not observed <= levels_of_role(role):
            problems.append(f"{fqn}: is a value at {sorted(observed)}, the row admits {role}")
        if role == "Abc" and not inspect.isabstract(cls) and cls.__module__ != "collections.abc":
            problems.append(f"{fqn}: an ABC row, but it is a concrete class")
        if role.startswith("Native") and python_reads(cls, kind):
            problems.append(f"{fqn}: a `Native` row, but a read method is Python code")
    return problems


def stdlib_modules() -> list[str]:
    def skipped(name: str) -> bool:
        parts = name.split(".")
        return parts[0] in SKIPPED or any(
            p.startswith("_") or p in ("test", "tests", "__main__") for p in parts
        )

    names = set()
    for top in sorted(sys.stdlib_module_names):
        if skipped(top):
            continue
        try:
            module = quiet_import(top)
        except BaseException:  # noqa: BLE001 - a module this platform lacks
            continue
        names.add(top)
        for info in pkgutil.walk_packages(
            getattr(module, "__path__", []), top + ".", lambda _: None
        ):
            if not skipped(info.name):
                names.add(info.name)
    return sorted(names)


def check_complete(rows: list[tuple[str, str, str]]) -> list[str]:
    listed = {fqn: kind for fqn, kind, _ in rows}
    problems = []
    for name in stdlib_modules():
        try:
            module = quiet_import(name)
        except BaseException:  # noqa: BLE001 - a module this platform lacks
            continue
        exported = getattr(module, "__all__", ())
        for attr, cls in sorted(vars(module).items()):
            if attr.startswith("_") or not isinstance(cls, type):
                continue
            # A class can derive from it, and the module names it as its own.
            if not cls.__flags__ & (1 << 10):
                continue
            if cls.__module__ != name and attr not in exported:
                continue
            for kind, (_, read_only, _) in FAMILIES.items():
                if issubclass(cls, read_only):
                    fqn = f"{name}.{attr}"
                    if listed.get(fqn) != kind:
                        problems.append(f"{fqn}: a {read_only.__name__} without its row")
    return problems


class Everything:
    """Every method of the container ABCs, with no base and no registration."""

    def __getitem__(self, key): ...
    def __setitem__(self, key, value): ...
    def __delitem__(self, key): ...
    def __len__(self):
        return 0

    def __iter__(self):
        return iter(())

    def __reversed__(self):
        return iter(())

    def __contains__(self, item):
        return False

    def keys(self): ...
    def items(self): ...
    def values(self): ...
    def get(self, key, default=None): ...
    def index(self, item): ...
    def count(self, item): ...
    def insert(self, i, item): ...
    def add(self, item): ...
    def discard(self, item): ...
    def __le__(self, other): ...
    def __lt__(self, other): ...
    def __ge__(self, other): ...
    def __gt__(self, other): ...
    def __and__(self, other): ...
    def __or__(self, other): ...
    def __sub__(self, other): ...
    def __xor__(self, other): ...
    def isdisjoint(self, other): ...


def check_nominal(rows: list[tuple[str, str, str]]) -> list[str]:
    problems = []
    for fqn, _, role in rows:
        if role in ("ReadOnly", "Mutable", "Abc"):
            abc = resolve(fqn)
            if abc is not None and issubclass(Everything, abc):
                problems.append(f"{fqn}: accepts a class that neither derives nor registers")
    listed = {resolve(fqn) for fqn, _, _ in rows}
    for abc in STRUCTURAL:
        if not issubclass(Everything, abc):
            problems.append(f"{abc.__name__}: no longer structural")
        if abc in listed:
            problems.append(f"{abc.__name__}: a structural ABC with a row")
    return problems


def check_openings() -> list[str]:
    problems = []
    for _, read_only, _ in FAMILIES.values():

        class Registered:
            pass

        class Below(Registered):
            pass

        read_only.register(Registered)
        if not issubclass(Below, read_only):
            problems.append(f"{read_only.__name__}: a subclass of a registered class is none")
    # Hooks of subclasses answer for the ABC, so this one makes any class a `MutableSet` (the ABC
    # with the fewest other uses in this process).

    class Unrelated:
        pass

    class Hooked(cabc.MutableSet):
        @classmethod
        def __subclasshook__(cls, other):
            return other is Unrelated or NotImplemented

    if not issubclass(Unrelated, cabc.MutableSet):
        problems.append("MutableSet: a subclass's `__subclasshook__` does not answer for it")
    return problems


class Report:
    def __init__(self) -> None:
        self.failures: list[str] = []

    def check(self, name: str, problems: list[str]) -> None:
        for problem in problems:
            print(f"  FAIL {name}: {problem}")
        if not problems:
            print(f"  PASS {name}")
        self.failures.extend(problems)


def main() -> int:
    print(f"verify_container_families: CPython {sys.version.split()[0]} ({TABLE})")
    rows = load_table()
    report = Report()
    report.check("table-parses", [] if len(rows) > 100 else [f"only {len(rows)} rows parsed"])
    report.check("rows-match-cpython", check_rows(rows))
    report.check("table-is-complete", check_complete(rows))
    report.check("abcs-are-nominal", check_nominal(rows))
    report.check("what-opens-a-family", check_openings())
    return 1 if report.failures else 0


if __name__ == "__main__":
    sys.exit(main())
