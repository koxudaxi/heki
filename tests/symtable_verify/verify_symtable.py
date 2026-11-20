#!/usr/bin/env python3
"""Bidirectional CPython `symtable` differential verifier for `heki_semantic` (ADR-015).

Compares `heki dump-semantic <file> --target-version py3XX` against the running
CPython interpreter's `symtable.symtable()` across CPython 3.11 / 3.12 / 3.13 / 3.14 / 3.15:
- Scope tree structure: kind, name, definition line, and parent of every scope, in
  depth-first order
- Bindings (assigned, imported, or parameter names), parameters, and `global` / `nonlocal`
  declarations, each compared on its own
- Read classifications (`local` / `enclosing` / `global|builtin`)

Every target must exist and every file must parse, except the fixtures whose minimum CPython
version (`MIN_PYTHON`) is newer than the running interpreter; a run that compares no file fails.
Sources are handed to CPython as bytes, so it decodes them as it would import them (PEP 263
encoding declarations, UTF-8 BOM), and `heki` decodes the same file the same way.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import symtable
import sys

# Fixtures that need a newer CPython than 3.11 to parse, by a marker in their file name.
MIN_PYTHON = (
    ("pep810", (3, 15)),
    ("_314", (3, 14)),
    ("313_315", (3, 13)),
    ("312_315", (3, 12)),
)

# Reads the compiler adds itself: annotation scopes inside a class read the class namespace
# through `__classdict__` (3.12+), and 3.14+ module and class blocks with conditional
# annotations read `__conditional_annotations__`.
COMPILER_READS = {"__classdict__", "__conditional_annotations__"}

# `symtable` scope types across 3.11-3.15, normalized to `heki_semantic`'s `ScopeKind`.
CPYTHON_KINDS = {
    "module": "module",
    "function": "function",
    "class": "class",
    "annotation": "annotation",
    "type alias": "type_alias",
    "type parameter": "type_parameters",  # 3.12
    "type parameters": "type_parameters",  # 3.13+
    "TypeVar bound": "type_variable",  # 3.12
    "type variable": "type_variable",  # 3.13+
}

# `heki_semantic` keeps comprehension scopes apart; CPython calls them functions.
HEKI_KINDS = {"comprehension": "function"}


def normalize_scope_name(name: str) -> str:
    return {
        "genexpr": "<genexpr>",
        "listcomp": "<listcomp>",
        "setcomp": "<setcomp>",
        "dictcomp": "<dictcomp>",
        "lambda": "<lambda>",
    }.get(name, name)


def is_compiler_symbol(name: str) -> bool:
    """`.0`, `.format`, `.defaults`, `.type_params`, `.generic_base`, ...: never user names."""
    return name.startswith(".")


def dump_cpython_symtable(source: bytes, filename: str) -> list[dict]:
    """Flatten CPython's `symtable` tree into a normalized list comparable with `heki_semantic`."""
    root = symtable.symtable(source, filename, "exec")
    out: list[dict] = []

    def walk(tbl: symtable.SymbolTable, parent_idx: int | None, parent_kind: str | None) -> None:
        idx = len(out)
        kind = CPYTHON_KINDS[str(tbl.get_type())]
        symbols = [s for s in tbl.get_symbols() if not is_compiler_symbol(s.get_name())]

        def compiler_binding(sym: symtable.Symbol) -> bool:
            # A generic class body stores its type parameters in `__type_params__`.
            return (
                sym.get_name() == "__type_params__"
                and kind == "class"
                and parent_kind == "type_parameters"
            )

        bindings = sorted(
            s.get_name()
            for s in symbols
            if (s.is_assigned() or s.is_parameter() or s.is_imported()) and not compiler_binding(s)
        )
        reads: dict[str, str] = {}
        for sym in symbols:
            name = sym.get_name()
            if not sym.is_referenced() or name in COMPILER_READS:
                continue
            if kind == "module":
                read_kind = "global"
            elif sym.is_free():
                read_kind = "enclosing"
            elif sym.is_local() or sym.is_parameter():
                read_kind = "local"
            else:
                read_kind = "global"  # CPython symtable groups module-globals and builtins together
            reads[name] = read_kind

        out.append({
            "id": idx,
            "parent": parent_idx,
            "kind": kind,
            "name": normalize_scope_name(tbl.get_name()),
            "line": tbl.get_lineno(),
            "bindings": bindings,
            "parameters": sorted(s.get_name() for s in symbols if s.is_parameter()),
            # A module records the `global` declarations of every nested scope as its own.
            "global_decls": (
                []
                if kind == "module"
                else sorted(s.get_name() for s in symbols if s.is_declared_global())
            ),
            "nonlocal_decls": sorted(s.get_name() for s in symbols if s.is_nonlocal()),
            "reads": reads,
            # `super()` makes the compiler read the implicit `__class__` cell.
            "reads_super": "super" in reads,
        })
        for child in tbl.get_children():
            walk(child, idx, kind)

    walk(root, None, None)
    return out


def heki_scopes_depth_first(heki: dict) -> list[dict]:
    """`heki_semantic` scopes in depth-first order (children by creation), parents re-indexed."""
    scopes = heki.get("scopes", [])
    children: dict[int | None, list[dict]] = {}
    for sc in scopes:
        children.setdefault(sc["parent"], []).append(sc)
    ordered: list[dict] = []
    position: dict[int, int] = {}

    def walk(sc: dict) -> None:
        position[sc["id"]] = len(ordered)
        ordered.append(sc)
        for child in sorted(children.get(sc["id"], []), key=lambda c: c["id"]):
            walk(child)

    for top in children.get(None, []):
        walk(top)
    return [
        {**sc, "parent": None if sc["parent"] is None else position[sc["parent"]]} for sc in ordered
    ]


def normalize_heki_read(kind: str) -> str:
    return "global" if kind in {"global", "builtin"} else kind


def compare_sets(where: str, what: str, cpython: list[str], heki: list[str]) -> list[str]:
    errors = []
    if missing := sorted(set(cpython) - set(heki)):
        errors.append(f"{where}: heki missed {what} {missing}")
    if extra := sorted(set(heki) - set(cpython)):
        errors.append(f"{where}: heki invented {what} {extra}")
    return errors


def compare_models(path: Path, cpython: list[dict], heki: dict) -> list[str]:
    """Return bidirectional disagreements between CPython `symtable` and `heki_semantic`."""
    errors: list[str] = []
    k_scopes = heki_scopes_depth_first(heki)
    c_tree = [(s["kind"], s["name"], s["line"], s["parent"]) for s in cpython]
    k_tree = [
        (HEKI_KINDS.get(s["kind"], s["kind"]), normalize_scope_name(s["name"]), s["line"], s["parent"])
        for s in k_scopes
    ]
    if c_tree != k_tree:
        errors.append(
            f"{path}: scope tree mismatch (kind, name, line, parent)\n"
            f"  CPython: {c_tree}\n"
            f"  heki:    {k_tree}"
        )
        return errors

    for c_sc, k_sc in zip(cpython, k_scopes, strict=True):
        where = f"{path}:{c_sc['line']} ({c_sc['name']})"
        k_bindings = k_sc.get("bindings", [])
        errors += compare_sets(where, "bindings", c_sc["bindings"], [b["name"] for b in k_bindings])
        errors += compare_sets(
            where, "parameters", c_sc["parameters"], [b["name"] for b in k_bindings if b["is_parameter"]]
        )
        k_globals = [] if c_sc["kind"] == "module" else k_sc.get("global_decls", [])
        errors += compare_sets(where, "global declarations", c_sc["global_decls"], k_globals)
        errors += compare_sets(
            where, "nonlocal declarations", c_sc["nonlocal_decls"], k_sc.get("nonlocal_decls", [])
        )

        k_reads = {name: normalize_heki_read(kind) for name, kind in k_sc.get("reads", [])}
        inlined_comp_only = set(k_sc.get("inlined_comp_only_reads", []))
        for name, c_kind in sorted(c_sc["reads"].items()):
            k_kind = k_reads.get(name)
            if k_kind is None and name == "__class__" and c_sc["reads_super"]:
                continue
            if k_kind != c_kind:
                errors.append(f"{where}: read `{name}` CPython={c_kind} heki={k_kind}")
        for name, k_kind in sorted(k_reads.items()):
            if name not in c_sc["reads"]:
                # CPython 3.12+ PEP 709 drops `is_referenced()` on outer symbols read exclusively
                # inside an inlined comprehension; allow those only when explicitly tagged by heki.
                if name in inlined_comp_only:
                    continue
                errors.append(f"{where}: heki extra read `{name}` ({k_kind})")
    return errors


def min_python(path: Path) -> tuple[int, int]:
    for marker, version in MIN_PYTHON:
        if marker in path.name:
            return version
    return (3, 11)


def main() -> int:
    ap = argparse.ArgumentParser(description="Verify heki_semantic against CPython symtable.")
    ap.add_argument("targets", nargs="*", type=Path, default=[Path(__file__).parent / "fixtures"])
    ap.add_argument("--heki", type=Path, help="Path to compiled `heki` binary")
    ap.add_argument("--self-check", action="store_true", help="Only verify fixtures parse with CPython symtable")
    ap.add_argument("--max-errors", type=int, default=30)
    args = ap.parse_args()
    if args.heki is None and not args.self_check:
        ap.error("--heki is required unless --self-check is given")

    py_ver = f"py{sys.version_info.major}{sys.version_info.minor}"
    files: list[Path] = []
    for t in args.targets:
        if t.is_dir():
            files.extend(sorted(t.rglob("*.py")))
        elif t.is_file():
            files.append(t)
        else:
            print(f"FAIL: target `{t}` does not exist", file=sys.stderr)
            return 1

    all_errors: list[str] = []
    checked = 0
    for fp in files:
        if sys.version_info[:2] < min_python(fp):
            continue
        try:
            cpy = dump_cpython_symtable(fp.read_bytes(), str(fp))
        except SyntaxError as e:
            all_errors.append(f"{fp}: CPython {py_ver} cannot parse it: {e}")
            continue
        checked += 1
        if args.self_check:
            print(f"[self-check {py_ver}] {fp.name}: {len(cpy)} symtable scopes OK")
            continue
        proc = subprocess.run(
            [str(args.heki), "dump-semantic", str(fp), "--target-version", py_ver],
            capture_output=True,
            text=True,
        )
        if proc.returncode != 0:
            all_errors.append(f"{fp}: heki dump-semantic failed: {proc.stderr.strip()}")
            continue
        all_errors.extend(compare_models(fp, cpy, json.loads(proc.stdout)))

    if checked == 0:
        all_errors.append(f"no file to verify on {py_ver} under {[str(t) for t in args.targets]}")
    if all_errors:
        for err in all_errors[: args.max_errors]:
            print("FAIL:", err, file=sys.stderr)
        print(f"TOTAL FAILURES ({py_ver}): {len(all_errors)} across {checked} file(s).", file=sys.stderr)
        return 1
    print(f"OK ({py_ver}): verified {checked} file(s) with 0 disagreements.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
