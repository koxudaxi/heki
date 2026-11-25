#!/usr/bin/env python3
"""CPython differential verifier for heki's import facts (ADR-022).

For each fixture project, compares `heki inspect --imports '*' --output-format json
--target-version py3XX` with the running CPython interpreter:

- `body` edges, both ways. Each module of the project is imported first in a fresh interpreter
  whose `builtins.__import__` and `importlib.import_module` record every import the module
  body runs (in a class body it runs too, and when a lazy name it reads is loaded), with the
  module the import loads directly: the submodule of a `from` import when the import binds it,
  else the module named. Every recorded edge must be one heki lists among the `body` sites of
  the importing module (its target, a module it may load instead, or one a by-name site leaves
  open), and every `body` site heki marks unconditional and not guarded must be recorded.
- Timing classes the interpreter does not show: an `ast` reader classifies each import alias
  as inside a function, under `if TYPE_CHECKING:`, written `lazy`, or neither, and heki's
  timing of the same alias must agree (`function`, `type-checking`, `lazy` or `body` for a lazy
  import the body loads, or open when heki cannot tell which binding a read sees, and `body`,
  `lazy` or open for the others, which `__lazy_modules__` may make lazy).
- Circular imports that fail (`IMP102`): importing a module first must raise the `ImportError`
  of a partially initialized module exactly at the statement an `IMP102` report names it at, and
  every module no report names must import cleanly. The body edges a failing statement cuts off
  in every import are not required.

Fixtures under a directory named `py315` need CPython 3.15 (`lazy import`). A run that
compares no fixture fails.
"""
from __future__ import annotations

import argparse
import ast
import json
from pathlib import Path
import subprocess
import sys

MIN_PYTHON = (("py315", (3, 15)),)

# Run in a fresh interpreter with the project root and the module to import as arguments:
# prints the recorded edges as JSON lines `[importer, target]`.
RECORDER = r"""
import builtins, importlib, importlib.util, json, sys

root, first = sys.argv[1], sys.argv[2]
sys.path[:0] = [root, root + "/src"]
edges = []
MISSING = object()
original_import = builtins.__import__
original_import_module = importlib.import_module


def body_importer(frame):
    # The module whose body runs the import: the frame is its module body, or a class body
    # its module body runs; a function frame is no body.
    while frame is not None and frame.f_code.co_name != "<module>":
        if frame.f_code.co_flags & 0x1:  # CO_OPTIMIZED: a function
            return None
        frame = frame.f_back
    return None if frame is None else frame.f_globals.get("__name__")


def recorded_import(name, globals=None, locals=None, fromlist=(), level=0):
    importer = body_importer(sys._getframe(1))
    absolute = name
    if level and globals is not None:
        package = globals.get("__package__") or globals.get("__name__", "")
        try:
            absolute = importlib.util.resolve_name("." * level + name, package)
        except (ImportError, ValueError):
            absolute = None
    try:
        module = original_import(name, globals, locals, fromlist, level)
    except BaseException:
        if importer and absolute is not None:
            edges.append([importer, absolute])
        raise
    if importer and absolute is not None:
        items = [item for item in (fromlist or ()) if item != "*"]
        if not items:
            edges.append([importer, absolute])
        for item in items:
            full = absolute + "." + item
            # The name the import takes: the package's attribute, or, while the package has
            # none (the submodule is still loading), the submodule in `sys.modules`.
            bound = getattr(sys.modules.get(absolute), item, MISSING)
            submodule = full in sys.modules and bound in (MISSING, sys.modules[full])
            edges.append([importer, full if submodule else absolute])
    return module


def recorded_import_module(name, package=None):
    importer = body_importer(sys._getframe(1))
    if importer:
        edges.append([importer, importlib.util.resolve_name(name, package)])
    return original_import_module(name, package)


builtins.__import__ = recorded_import
importlib.import_module = recorded_import_module
try:
    original_import_module(first)
except BaseException as error:
    # Where the error was raised: the innermost frame of a file of the project.
    tb, where = error.__traceback__, None
    while tb is not None:
        if tb.tb_frame.f_code.co_filename.startswith(root):
            where = [tb.tb_frame.f_code.co_filename[len(root) + 1:], tb.tb_lineno]
        tb = tb.tb_next
    print(json.dumps(["error", type(error).__name__, str(error), where]))
for edge in edges:
    print(json.dumps(edge))
"""


def heki_report(heki: str, root: Path) -> list[dict]:
    target = f"py3{min(max(sys.version_info[1], 11), 15)}"
    result = subprocess.run(
        [heki, "inspect", "--imports", "*", "--output-format", "json",
         "--target-version", target, str(root)],
        capture_output=True, text=True, check=False,
    )
    if result.returncode != 0:
        raise SystemExit(f"{root}: heki exited with {result.returncode}\n{result.stderr}")
    return json.loads(result.stdout)["modules"]


def circular_failures(heki: str, root: Path) -> dict[str, tuple[str, int]]:
    """Per module, where heki says importing it first of its import cycle raises `ImportError`:
    the path and line of each `IMP102` report, for each module its message names."""
    target = f"py3{min(max(sys.version_info[1], 11), 15)}"
    result = subprocess.run(
        [str(Path(heki).resolve()), "check", "--select", "IMP102", "--output-format", "json",
         "--target-version", target, "."],
        capture_output=True, text=True, check=False, cwd=root,
    )
    if result.returncode not in (0, 1):
        raise SystemExit(f"{root}: heki check exited with {result.returncode}\n{result.stderr}")
    failing: dict[str, tuple[str, int]] = {}
    for report in json.loads(result.stdout):
        named = report["message"].split(": importing ", 1)[1].split(" while no module")[0]
        if " other module" in named:
            raise SystemExit(f"{root}: {report['path']}: a report names too many modules to compare")
        where = (Path(report["path"]).as_posix(), report["line"])
        for entry in named.split("`")[1::2]:
            failing[entry] = where
    return failing


def recorded_edges(
    root: Path, modules: list[str]
) -> tuple[set[tuple[str, str]], dict[str, tuple[str, str, tuple[str, int] | None]]]:
    """The body edges importing each module first records, and per module whose import raised,
    the error's type, message, and where in the project it was raised."""
    edges: set[tuple[str, str]] = set()
    errors: dict[str, tuple[str, str, tuple[str, int] | None]] = {}
    for module in modules:
        result = subprocess.run(
            [sys.executable, "-I", "-c", RECORDER, str(root.resolve()), module],
            capture_output=True, text=True, check=False, cwd=root,
        )
        if result.returncode != 0:
            raise SystemExit(f"{root}: importing {module} failed\n{result.stderr}")
        for line in result.stdout.splitlines():
            record = json.loads(line)
            if record[0] == "error":
                where = tuple(record[3]) if record[3] else None
                errors[module] = (record[1], record[2], where)
                continue
            edges.add((record[0], record[1]))
    return edges, errors


def compare_failures(
    root: Path,
    failing: dict[str, tuple[str, int]],
    errors: dict[str, tuple[str, str, tuple[str, int] | None]],
    modules: list[str],
) -> list[str]:
    problems = []
    for module in modules:
        expected = failing.get(module)
        error = errors.get(module)
        if expected is None and error is not None:
            problems.append(f"{root.name}: importing {module} first raised {error[0]}: {error[1]}")
        elif expected is not None and error is None:
            problems.append(
                f"{root.name}: heki: importing {module} first fails at {expected[0]}:{expected[1]}; "
                f"CPython imported it"
            )
        elif expected is not None and error is not None:
            kind, message, where = error
            circular = kind == "ImportError" and "partially initialized module" in message
            if not circular or where != expected:
                problems.append(
                    f"{root.name}: heki: importing {module} first fails at "
                    f"{expected[0]}:{expected[1]}; CPython raised {kind} at {where}: {message}"
                )
    return problems


def matches(site: dict, target: str) -> bool:
    # A module the project lacks is the one the import tries, and fails, to load.
    if target in (site.get("target"), site.get("missing")):
        return True
    for other in site.get("may_load", []):
        if other == target or (other.endswith("*") and target.startswith(other[:-1])):
            return True
    # A by-name site whose module heki cannot tell may load anything.
    return site["form"] == "by_name" and not site.get("target") and not site.get("may_load")


def compare_edges(
    root: Path,
    report: list[dict],
    edges: set[tuple[str, str]],
    failing: dict[str, tuple[str, int]],
) -> list[str]:
    problems = []
    by_module = {m["module"]: m for m in report}
    # The first line each file fails at: no import runs the body past it.
    cut: dict[str, int] = {}
    for path, line in failing.values():
        cut[path] = min(cut.get(path, line), line)
    for importer, target in sorted(edges):
        module = by_module.get(importer)
        if module is None:
            continue
        sites = [s for s in module["imports"] or [] if s["timing"] == "body"]
        if not any(matches(s, target) for s in sites):
            problems.append(f"{root.name}: CPython: {importer} imports {target} while it loads; heki lists no such body import")
    for module in report:
        for site in module["imports"] or []:
            must = site["timing"] == "body" and site["unconditional"] and not site["guarded"]
            if not must or not (site.get("target") or site.get("may_load")):
                continue
            if site["line"] > cut.get(module["path"], site["line"]):
                continue
            if not any(i == module["module"] and matches(site, t) for i, t in edges):
                problems.append(
                    f"{root.name}: heki: {module['module']} line {site['line']} always imports "
                    f"{site.get('target') or site.get('may_load')} while it loads; CPython did not"
                )
    return problems


class Classes(ast.NodeVisitor):
    """The class of each import alias by its position: `function`, `type-checking`, `lazy`, or
    `plain`."""

    def __init__(self) -> None:
        self.classes: dict[tuple[int, int], str] = {}
        self.context: list[str] = []

    def visit_FunctionDef(self, node: ast.AST) -> None:
        self.context.append("function")
        self.generic_visit(node)
        self.context.pop()

    visit_AsyncFunctionDef = visit_FunctionDef
    visit_Lambda = visit_FunctionDef

    def visit_If(self, node: ast.If) -> None:
        test = node.test
        guard = (isinstance(test, ast.Name) and test.id == "TYPE_CHECKING") or (
            isinstance(test, ast.Attribute) and test.attr == "TYPE_CHECKING"
        )
        self.visit(node.test)
        self.context.append("type-checking" if guard else "")
        for stmt in node.body:
            self.visit(stmt)
        self.context.pop()
        for stmt in node.orelse:
            self.visit(stmt)

    def record(self, node: ast.Import | ast.ImportFrom) -> None:
        lazy = getattr(node, "is_lazy", 0)
        kinds = [c for c in self.context if c]
        kind = kinds[0] if kinds else ("lazy" if lazy else "plain")
        for alias in node.names:
            if alias.name != "*":
                self.classes[(alias.lineno, alias.col_offset)] = kind

    visit_Import = record
    visit_ImportFrom = record


ACCEPTED = {
    "function": {"function"},
    "type-checking": {"type-checking"},
    "lazy": {"lazy", "body", None},
    "plain": {"body", "lazy", None},
}


def compare_classes(root: Path, report: list[dict]) -> list[str]:
    problems = []
    for module in report:
        path = root / module["path"]
        if not path.suffix == ".py" or module["imports"] is None:
            continue
        classes = Classes()
        classes.visit(ast.parse(path.read_bytes(), str(path)))
        for site in module["imports"]:
            kind = classes.classes.get((site["line"], site["column"] - 1))
            if kind is not None and site["timing"] not in ACCEPTED[kind]:
                problems.append(
                    f"{root.name}: {module['path']}:{site['line']}: the ast reader says {kind}, "
                    f"heki says {site['timing']}"
                )
    return problems


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--heki", required=True)
    parser.add_argument("fixtures", type=Path)
    args = parser.parse_args()
    problems: list[str] = []
    compared = 0
    for root in sorted(p for p in args.fixtures.rglob("pyproject.toml")):
        root = root.parent
        needs = max(
            (version for marker, version in MIN_PYTHON if marker in root.parts),
            default=(3, 11),
        )
        if sys.version_info[:2] < needs:
            continue
        report = heki_report(args.heki, root)
        modules = [m["module"] for m in report if m["imports"] is not None]
        failing = circular_failures(args.heki, root)
        edges, errors = recorded_edges(root, modules)
        problems += compare_failures(root, failing, errors, modules)
        problems += compare_edges(root, report, edges, failing)
        problems += compare_classes(root, report)
        compared += 1
    for problem in problems:
        print(problem)
    if compared == 0:
        print("no fixture compared")
        return 1
    print(f"{compared} fixture project(s) compared, {len(problems)} disagreement(s)")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
