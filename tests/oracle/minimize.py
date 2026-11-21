"""Shrinks a project that refutes a claim to a small one that still refutes it the same way.

The property kept is "the oracle still finds a refutation of the same rule and kind". The steps,
repeated until none applies or the time budget runs out:

1. keep only the entry point and the input that refuted;
2. delete statements, one at a time, in every module, function, class, and block (an emptied
   block gets `pass`);
3. replace a compound statement (`if`, `for`, `while`, `with`, `try`) by one of its bodies;
4. drop the names an import line brings in that nothing uses.

Each candidate is checked from scratch: heki runs on it again, so the repro is a project heki
reports on and the program refutes, not a fragment of one.

A refutation that stands on how `premise_generator.py` built the program (a premise it made
false, with an exact report) keeps its program text: a reduction could make the premise true
again while the report stays. Only its entry points narrow.
"""

from __future__ import annotations

import ast
import copy
import json
import os
import shutil
import tempfile
import time

import driver

# Refutations that stand on how the program was built (`premise_generator.py`): a premise the
# generator made false, with an exact report of the rule.
BY_CONSTRUCTION = ("stripped-premise", "relevance-premise")


def read_project(root):
    files = {}
    for base, _dirs, names in os.walk(root):
        for name in names:
            path = os.path.join(base, name)
            rel = os.path.relpath(path, root).replace(os.sep, "/")
            if rel == "oracle.json" or "__pycache__" in rel:
                continue
            with open(path, encoding="utf-8") as f:
                files[rel] = f.read()
    with open(os.path.join(root, "oracle.json"), encoding="utf-8") as f:
        meta = json.load(f)
    return files, meta


def write_project(root, files, meta):
    for rel, text in files.items():
        path = os.path.join(root, rel)
        os.makedirs(os.path.dirname(path), exist_ok=True)
        with open(path, "w", encoding="utf-8") as f:
            f.write(text)
    with open(os.path.join(root, "oracle.json"), "w", encoding="utf-8") as f:
        json.dump(meta, f, indent=1)


class Minimizer:
    def __init__(self, tools, rule, kind, budget):
        self.tools = tools
        self.rule = rule
        self.kind = kind
        self.deadline = time.monotonic() + budget
        self.tries = 0
        self.last = None

    def holds(self, files, meta):
        if time.monotonic() > self.deadline:
            return False
        self.tries += 1
        with tempfile.TemporaryDirectory(prefix="oracle-min-") as tmp:
            root = os.path.realpath(tmp)
            write_project(root, files, meta)
            try:
                result = driver.check_project(root, self.tools, meta["entries"])
            except Exception:  # noqa: BLE001 - a candidate that breaks the pipeline is rejected
                return False
        for finding in result["refutations"]:
            if finding["rule"] == self.rule and finding["kind"] == self.kind:
                self.last = finding
                return True
        return False

    def run(self, files, meta, finding):
        meta = dict(meta)
        entry, index = finding.get("entry"), finding.get("input")
        if finding["kind"] in BY_CONSTRUCTION:
            # The premise is false by how the generator built the program, which a reduction
            # could undo (unwrapping an early return restores "Every return") while the report
            # stays exact: the program text is kept, and only the entry points narrow.
            return files, self._narrow_entries(files, meta)
        if entry is not None:
            only = dict(meta["entries"][entry])
            narrowed = dict(meta, entries=[dict(only, inputs=[only["inputs"][index]])])
            if self.holds(files, narrowed):
                meta = narrowed
            else:
                narrowed = dict(meta, entries=[only])
                if self.holds(files, narrowed):
                    meta = narrowed
        changed = True
        while changed and time.monotonic() < self.deadline:
            changed = False
            for rel in sorted(files, key=lambda p: (not p.endswith(".py"), p)):
                if not rel.endswith(".py") or rel == "app/__init__.py":
                    continue
                for transform in (delete_statement, unwrap_statement, drop_import_names):
                    while time.monotonic() < self.deadline:
                        candidate = self._first_holding(files, meta, rel, transform)
                        if candidate is None:
                            break
                        files = candidate
                        changed = True
            for rel in list(files):
                if rel.endswith(".txt") or (
                    rel.endswith(".py") and not files[rel].strip() and rel != "app/__init__.py"
                ):
                    smaller = {k: v for k, v in files.items() if k != rel}
                    if self.holds(smaller, meta):
                        files = smaller
                        changed = True
        return files, meta

    def _narrow_entries(self, files, meta):
        for entry in meta["entries"]:
            narrowed = dict(meta, entries=[dict(entry, inputs=entry["inputs"][:1])])
            if self.holds(files, narrowed):
                return narrowed
        return meta

    def _first_holding(self, files, meta, rel, transform):
        try:
            tree = ast.parse(files[rel])
        except SyntaxError:
            return None
        for text in transform(tree):
            if text == files[rel]:
                continue
            candidate = dict(files)
            candidate[rel] = text
            if self.holds(candidate, meta):
                return candidate
        return None


def _statement_paths(tree):
    """Paths to every statement: lists of (field, index) from the module down."""
    out = []

    def walk(node, prefix):
        for field in ("body", "orelse", "finalbody", "handlers"):
            stmts = getattr(node, field, None)
            if not isinstance(stmts, list):
                continue
            for i, child in enumerate(stmts):
                path = prefix + [(field, i)]
                if isinstance(child, ast.stmt):
                    out.append(path)
                walk(child, path)

    walk(tree, [])
    return out


def _resolve(tree, path):
    node = tree
    for field, index in path[:-1]:
        node = getattr(node, field)[index]
    return node, path[-1]


def _unparse(tree):
    ast.fix_missing_locations(tree)
    return ast.unparse(tree) + "\n"


def delete_statement(tree):
    for path in reversed(_statement_paths(tree)):
        copied = copy.deepcopy(tree)
        parent, (field, index) = _resolve(copied, path)
        stmts = getattr(parent, field)
        del stmts[index]
        if field == "handlers" and not stmts and not parent.finalbody:
            continue
        if not stmts and field == "body" and not isinstance(parent, ast.Module):
            stmts.append(ast.Pass())
        yield _unparse(copied)


def _bodies(stmt):
    if isinstance(stmt, (ast.If, ast.For, ast.AsyncFor, ast.While, ast.With, ast.AsyncWith)):
        return [stmt.body] + ([stmt.orelse] if getattr(stmt, "orelse", None) else [])
    if isinstance(stmt, ast.Try):
        return [stmt.body] + [h.body for h in stmt.handlers] + ([stmt.finalbody] if stmt.finalbody else [])
    return []


def unwrap_statement(tree):
    for path in reversed(_statement_paths(tree)):
        parent, (field, index) = _resolve(tree, path)
        for k in range(len(_bodies(getattr(parent, field)[index]))):
            copied = copy.deepcopy(tree)
            cparent, (cfield, cindex) = _resolve(copied, path)
            choice = _bodies(getattr(cparent, cfield)[cindex])[k]
            getattr(cparent, cfield)[cindex : cindex + 1] = choice
            yield _unparse(copied)


def drop_import_names(tree):
    used = {n.id for n in ast.walk(tree) if isinstance(n, ast.Name)}
    used |= {
        n.value.id for n in ast.walk(tree) if isinstance(n, ast.Attribute) and isinstance(n.value, ast.Name)
    }
    for path in _statement_paths(tree):
        parent, (field, index) = _resolve(tree, path)
        stmt = getattr(parent, field)[index]
        if not isinstance(stmt, ast.ImportFrom) or len(stmt.names) < 2:
            continue
        keep = [a for a in stmt.names if (a.asname or a.name) in used]
        if keep and len(keep) < len(stmt.names):
            copied = copy.deepcopy(tree)
            cparent, (cfield, cindex) = _resolve(copied, path)
            getattr(cparent, cfield)[cindex].names = [
                a for a in getattr(cparent, cfield)[cindex].names if (a.asname or a.name) in used
            ]
            yield _unparse(copied)


def minimize(project_root, finding, tools, out_root, budget=120.0):
    """Writes the smallest project found to `out_root`; returns the number of candidates tried."""
    files, meta = read_project(project_root)
    minimizer = Minimizer(tools, finding["rule"], finding["kind"], budget)
    if not minimizer.holds(files, meta):
        return {"tries": minimizer.tries, "reproduced": False}
    files, meta = minimizer.run(files, meta, finding)
    if os.path.exists(out_root):
        shutil.rmtree(out_root)
    write_project(out_root, files, meta)
    with open(os.path.join(out_root, "finding.json"), "w", encoding="utf-8") as f:
        json.dump(minimizer.last, f, indent=1)
    return {"tries": minimizer.tries, "reproduced": True}
