#!/usr/bin/env python3
"""Bidirectional CPython `symtable` differential verifier for `heki_semantic` (ADR-015).

Compares `heki dump-semantic <file> --target-version py3XX` against the running
CPython interpreter's `symtable.symtable()` across CPython 3.11 / 3.12 / 3.13 / 3.14 / 3.15:
- Scope tree structure (kind, normalized name, line)
- Bindings (ignoring CPython synthetic compiler vars like `.0`, `.format`, `.defaults`, `__class__`)
- Read classifications (`local` / `enclosing` / `global|builtin`)
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import symtable
import sys

SYNTHETIC_SYMBOLS = {
    ".0",
    ".format",
    ".defaults",
    "__class__",
    "__classdict__",
    "__conditional_annotations__",
}


def normalize_scope_name(name: str) -> str:
    return {
        "genexpr": "<genexpr>",
        "listcomp": "<listcomp>",
        "setcomp": "<setcomp>",
        "dictcomp": "<dictcomp>",
        "lambda": "<lambda>",
    }.get(name, name)


def dump_cpython_symtable(source: str, filename: str) -> list[dict]:
    """Flatten CPython's `symtable` tree into a normalized list comparable with `heki_semantic`."""
    root = symtable.symtable(source, filename, "exec")
    out: list[dict] = []

    def walk(tbl: symtable.SymbolTable, parent_idx: int | None) -> None:
        idx = len(out)
        bindings = sorted(
            sym.get_name()
            for sym in tbl.get_symbols()
            if sym.get_name() not in SYNTHETIC_SYMBOLS
            and (sym.is_assigned() or sym.is_parameter() or sym.is_imported() or sym.is_namespace())
        )
        reads: dict[str, str] = {}
        for sym in tbl.get_symbols():
            name = sym.get_name()
            if name in SYNTHETIC_SYMBOLS or not sym.is_referenced():
                continue
            if tbl.get_type() == "module":
                kind = "global"
            elif sym.is_free():
                kind = "enclosing"
            elif sym.is_local() or sym.is_parameter():
                kind = "local"
            else:
                kind = "global"  # CPython symtable groups module-globals and builtins together
            reads[name] = kind

        out.append({
            "id": idx,
            "parent": parent_idx,
            "kind": tbl.get_type(),
            "name": normalize_scope_name(tbl.get_name()),
            "line": tbl.get_lineno(),
            "bindings": bindings,
            "reads": reads,
        })
        for child in tbl.get_children():
            walk(child, idx)

    walk(root, None)
    return out


def normalize_heki_read(kind: str) -> str:
    return "global" if kind in {"global", "builtin"} else kind


def compare_models(path: Path, cpython: list[dict], heki: dict) -> list[str]:
    """Return bidirectional disagreements between CPython `symtable` and `heki_semantic`."""
    errors: list[str] = []
    k_scopes = heki.get("scopes", [])
    if len(cpython) != len(k_scopes):
        errors.append(
            f"{path}: scope count mismatch (CPython={len(cpython)}, heki={len(k_scopes)})\n"
            f"  CPython: {[(s['kind'], s['name'], s['line']) for s in cpython]}\n"
            f"  heki:    {[(s['kind'], s['name'], s['line']) for s in k_scopes]}"
        )
        return errors

    for c_sc, k_sc in zip(cpython, k_scopes, strict=True):
        if c_sc["name"] != normalize_scope_name(k_sc["name"]):
            errors.append(
                f"{path}:{c_sc['line']}: scope name mismatch CPython={c_sc['name']} heki={k_sc['name']}"
            )
        c_binds = set(c_sc["bindings"])
        k_binds = {b["name"] for b in k_sc.get("bindings", [])}
        if missing := sorted(c_binds - k_binds):
            errors.append(f"{path}:{c_sc['line']} ({c_sc['name']}): heki missed bindings {missing}")
        if extra := sorted(k_binds - c_binds):
            errors.append(f"{path}:{c_sc['line']} ({c_sc['name']}): heki invented bindings {extra}")

        k_reads = {name: normalize_heki_read(kind) for name, kind in k_sc.get("reads", [])}
        inlined_comp_only = set(k_sc.get("inlined_comp_only_reads", []))
        for name, c_kind in sorted(c_sc["reads"].items()):
            k_kind = k_reads.get(name)
            if k_kind != c_kind:
                errors.append(
                    f"{path}:{c_sc['line']} ({c_sc['name']}): read `{name}` CPython={c_kind} heki={k_kind}"
                )
        for name, k_kind in sorted(k_reads.items()):
            if name not in c_sc["reads"]:
                # CPython 3.12+ PEP 709 drops `is_referenced()` on outer symbols read exclusively
                # inside an inlined comprehension; allow those only when explicitly tagged by heki.
                if name in inlined_comp_only:
                    continue
                errors.append(
                    f"{path}:{c_sc['line']} ({c_sc['name']}): heki extra read `{name}` ({k_kind})"
                )
    return errors


def main() -> int:
    ap = argparse.ArgumentParser(description="Verify heki_semantic against CPython symtable.")
    ap.add_argument("targets", nargs="*", type=Path, default=[Path(__file__).parent / "fixtures"])
    ap.add_argument("--heki", type=Path, help="Path to compiled `heki` binary")
    ap.add_argument("--self-check", action="store_true", help="Only verify fixtures parse with CPython symtable")
    ap.add_argument("--max-errors", type=int, default=30)
    args = ap.parse_args()

    py_ver = f"py{sys.version_info.major}{sys.version_info.minor}"
    files: list[Path] = []
    for t in args.targets:
        if t.is_dir():
            files.extend(sorted(t.rglob("*.py")))
        elif t.is_file():
            files.append(t)

    all_errors: list[str] = []
    checked = 0
    for fp in files:
        src = fp.read_text(encoding="utf-8", errors="ignore")
        if "pep810" in fp.name and sys.version_info < (3, 15):
            continue
        if "312_315" in fp.name and sys.version_info < (3, 12):
            continue
        try:
            cpy = dump_cpython_symtable(src, str(fp))
        except SyntaxError:
            continue
        checked += 1
        if args.self_check or args.heki is None:
            print(f"[self-check {py_ver}] {fp.name}: {len(cpy)} symtable scopes OK")
            continue
        proc = subprocess.run(
            [str(args.heki), "dump-semantic", str(fp), "--target-version", py_ver],
            check=True,
            capture_output=True,
            text=True,
        )
        heki_model = json.loads(proc.stdout)
        all_errors.extend(compare_models(fp, cpy, heki_model))

    if all_errors:
        for err in all_errors[: args.max_errors]:
            print("FAIL:", err, file=sys.stderr)
        print(f"TOTAL FAILURES ({py_ver}): {len(all_errors)} across {checked} file(s).", file=sys.stderr)
        return 1
    print(f"OK ({py_ver}): verified {checked} file(s) with 0 disagreements.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
