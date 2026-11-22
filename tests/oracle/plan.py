"""Turns heki's exact reports into claims and a watch plan for `monitor.py`.

A claim is what one exact report states, in the terms the checkers test: the call instance, the
loop, or the line it is about, and its witnesses. The plan lists the places the monitor watches
so that the run records what each claim needs.
"""

from __future__ import annotations

import ast
import os
import re

EXACT_RULES = (
    "EFF101",
    "EFF103",
    "DATA101",
    "MUT110",
    "MUT114",
    "MUT118",
    "OPT101",
    "OPT102",
    "OPT103",
    "OPT110",
    "OPT111",
    "OPT113",
    "OPT120",
    "OPT121",
    "OPT130",
)
OPT_TYPE_RULES = {
    "OPT101": ("list",),
    "OPT102": ("set", "dict"),
    "OPT103": ("bytes", "tuple", "str"),
    "OPT110": ("list", "tuple"),
    "OPT111": ("list",),
    "OPT120": ("list",),
    "OPT121": ("list",),
}
CATALOG_ROOTS = (
    "time.",
    "requests.",
    "sqlite3.",
    "socket.",
    "subprocess.",
    "urllib.",
    "psycopg2.",
    "queue.",
    "httpx.",
    "numpy.",
    "pandas.",
)
_BACKTICK = re.compile(r"`([^`]+)`")
_CONNECT_FORM = re.compile(r"\.connect\([^)]*\)\.cursor\(\)\.|\.connect\([^)]*\)\.")


def normalize_api(name):
    """A catalog symbol as the fakes name it: `sqlite3.connect(file).execute` is
    `sqlite3.Connection.execute`; class names compare without case (`psycopg2...cursor`)."""

    def replace(match):
        return ".Cursor." if match.group(0).endswith(".cursor().") else ".Connection."

    return _CONNECT_FORM.sub(replace, name).lower()


def witnesses(report):
    names = set()
    target = report.get("target") or ""
    for text in [target, *_BACKTICK.findall(report.get("message") or "")]:
        if text.startswith(CATALOG_ROOTS):
            names.add(normalize_api(text))
    return sorted(names)


class Source:
    """One project file: its bytes, lines, and syntax tree."""

    def __init__(self, root, path):
        self.path = path
        with open(os.path.join(root, path), "rb") as f:
            self.data = f.read()
        self.text = self.data.decode("utf-8")
        self.tree = ast.parse(self.text)
        self.line_starts = [0]
        for i, byte in enumerate(self.data):
            if byte == 0x0A:
                self.line_starts.append(i + 1)
        _set_parents(self.tree)

    def pos(self, start, end):
        """(line, col, end_line, end_col) of a byte span, columns in UTF-8 bytes as CPython has them."""
        return (*self._line_col(start), *self._line_col(end))

    def _line_col(self, offset):
        lo, hi = 0, len(self.line_starts) - 1
        while lo < hi:
            mid = (lo + hi + 1) // 2
            if self.line_starts[mid] <= offset:
                lo = mid
            else:
                hi = mid - 1
        return lo + 1, offset - self.line_starts[lo]

    def node_at(self, pos, kinds=None):
        """The innermost node whose position is exactly `pos`."""
        best = None
        for node in ast.walk(self.tree):
            if not hasattr(node, "lineno") or node.end_lineno is None:
                continue
            if (node.lineno, node.col_offset, node.end_lineno, node.end_col_offset) == tuple(pos):
                if kinds is None or isinstance(node, kinds):
                    best = node
        return best

    def ancestors(self, pos):
        """Nodes enclosing `pos`, innermost first."""
        line, col, end_line, end_col = pos
        found = []
        for node in ast.walk(self.tree):
            if not hasattr(node, "lineno") or node.end_lineno is None:
                continue
            starts_before = (node.lineno, node.col_offset) <= (line, col)
            ends_after = (node.end_lineno, node.end_col_offset) >= (end_line, end_col)
            if starts_before and ends_after:
                found.append(node)
        found.sort(key=lambda n: (n.lineno, n.col_offset, -n.end_lineno, -n.end_col_offset), reverse=True)
        return found


def _set_parents(tree):
    for node in ast.walk(tree):
        for child in ast.iter_child_nodes(node):
            child.parent = node


def _node_pos(node):
    return [node.lineno, node.col_offset, node.end_lineno, node.end_col_offset]


def _loop_entry(loop_id, node, source_path, **extra):
    """A plan entry for a `for` statement or a comprehension's `for` clause."""
    if isinstance(node, (ast.For, ast.AsyncFor)):
        last = max(stmt.end_lineno for stmt in node.body)
        entry = {
            "id": loop_id,
            "path": source_path,
            "pos": _node_pos(node.iter),
            "alt_pos": [_node_pos(node)],
            "for_line": node.lineno,
            "line_range": [node.lineno, last],
            "target_names": sorted(_names(node.target)),
        }
    else:
        comp, owner = node
        entry = {
            "id": loop_id,
            "path": source_path,
            "pos": _node_pos(comp.iter),
            "alt_pos": [_node_pos(owner)],
            "for_line": comp.iter.lineno,
            "line_range": [owner.lineno, owner.end_lineno],
            "target_names": sorted(_names(comp.target)),
        }
    entry.update(extra)
    return entry


def _names(target):
    return {n.id for n in ast.walk(target) if isinstance(n, ast.Name)}


def _enclosing_loops(source, pos):
    """Enclosing loops, innermost first: `For` nodes and `(comprehension, owner)` pairs."""
    loops = []
    for node in source.ancestors(pos):
        if isinstance(node, (ast.For, ast.AsyncFor)):
            if _within_body(node, pos):
                loops.append(node)
        elif isinstance(node, (ast.ListComp, ast.SetComp, ast.DictComp, ast.GeneratorExp)):
            for comp in reversed(node.generators):
                loops.append((comp, node))
    return loops


def _within_body(loop, pos):
    first = loop.body[0]
    return (pos[0], pos[1]) >= (first.lineno, first.col_offset)


def _origin_name(message):
    """The input a loop message names: `over parameter `ids``, `over `rows``."""
    match = re.search(r"over (?:[a-z ]+ )?`([^`]+)`", message or "")
    return match.group(1) if match else None


def _choose_loop(source, pos, message):
    loops = [loop for loop in _enclosing_loops(source, pos) if not isinstance(loop, tuple)]
    if not loops:
        return None
    name = _origin_name(message)
    if name:
        for loop in loops:
            if name in ast.get_source_segment(source.text, loop.iter):
                return loop
    return loops[0]


def _mutated_names(source, line):
    """Local names a statement on `line` may change in place, by how: method receivers (the
    monitor sees the builtin method called on them), subscript store and `del` bases, and
    augmented-assignment targets (in place when the name still holds the same object after)."""
    receivers, stores, augs = set(), set(), set()
    for node in ast.walk(source.tree):
        if getattr(node, "lineno", None) != line:
            continue
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
            if isinstance(node.func.value, ast.Name):
                receivers.add(node.func.value.id)
        elif isinstance(node, ast.Subscript) and isinstance(node.ctx, (ast.Store, ast.Del)):
            if isinstance(node.value, ast.Name):
                stores.add(node.value.id)
        elif isinstance(node, ast.AugAssign) and isinstance(node.target, ast.Name):
            augs.add(node.target.id)
    return {"receivers": sorted(receivers), "stores": sorted(stores), "augs": sorted(augs)}


def _all(names):
    return sorted({*names["receivers"], *names["stores"], *names["augs"]})


def _function_of(source, line):
    best = None
    for node in ast.walk(source.tree):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            if node.lineno <= line <= node.end_lineno:
                if best is None or node.lineno >= best.lineno:
                    best = node
    return best


def resolve_function(root, dotted, sources):
    """`app.m.Class.method` as (path, qualname), when a project file defines it."""
    parts = dotted.split(".")
    for cut in range(len(parts) - 1, 0, -1):
        module = "/".join(parts[:cut])
        for path in (module + ".py", module + "/__init__.py"):
            if os.path.exists(os.path.join(root, path)):
                return path, ".".join(parts[cut:])
    return None


class Planner:
    def __init__(self, root):
        self.root = root
        self.sources = {}
        self.plan = {"sites": [], "loops": [], "lines": [], "returns": []}
        self.claims = []
        self.reports = {}
        self.unmapped = []

    def source(self, path):
        if path not in self.sources:
            self.sources[path] = Source(self.root, path)
        return self.sources[path]

    def add(self, report):
        rule = report["rule_code"]
        handler = getattr(self, "_" + rule.lower(), None)
        if rule in OPT_TYPE_RULES or rule == "OPT130":
            handler = self._opt_type
        if handler is None:
            return None
        index = len(self.claims)
        source = self.source(report["path"])
        pos = source.pos(report["span"]["start"], report["span"]["end"])
        claim = {
            "id": f"k{index}",
            "rule": rule,
            "path": report["path"],
            "line": report["line"],
            "column": report["column"],
            "pos": list(pos),
            "message": report["message"],
            "target": report.get("target"),
            "help": report.get("help"),
            "related": [[r["path"], r["line"], r["column"], r["message"]] for r in report.get("related", [])],
            "has_fix": bool(report.get("fix")),
        }
        if not handler(claim, source, pos, report):
            self.unmapped.append(claim)
            return None
        self.claims.append(claim)
        self.reports[claim["id"]] = report
        return claim

    # -- per rule ------------------------------------------------------------------------------
    def _site(self, claim, pos, capture=()):
        site_id = f"c{claim['id'][1:]}"
        self.plan["sites"].append(
            {"id": site_id, "path": claim["path"], "pos": list(pos), "capture": list(capture)}
        )
        claim["site"] = site_id
        return site_id

    def _loop(self, claim, source, node, suffix="", **extra):
        loop_id = f"L{claim['id'][1:]}{suffix}"
        self.plan["loops"].append(_loop_entry(loop_id, node, claim["path"], **extra))
        return loop_id

    def _line(self, claim, line, path=None, **extra):
        line_id = f"N{claim['id'][1:]}"
        entry = {"id": line_id, "path": path or claim["path"], "line": line}
        entry.update(extra)
        self.plan["lines"].append(entry)
        claim["line_id"] = line_id
        return line_id

    def _eff101(self, claim, source, pos, report):
        self._site(claim, pos)
        claim["witnesses"] = witnesses(report)
        return True

    def _eff103(self, claim, source, pos, report):
        loop = _choose_loop(source, pos, report["message"])
        if loop is None:
            return False
        self._site(claim, pos)
        claim["loop"] = self._loop(claim, source, loop)
        claim["witnesses"] = witnesses(report)
        return True

    def _data101(self, claim, source, pos, report):
        loop = _choose_loop(source, pos, report["message"])
        target = report.get("target")
        if loop is None or not target or not target.isidentifier():
            return False
        self._site(claim, pos)
        claim["loop"] = self._loop(claim, source, loop, capture=[target])
        return True

    def _mut110(self, claim, source, pos, report):
        related = report.get("related", [])
        param = report.get("target")
        if len(related) < 2 or not param:
            return False
        change = related[-1]
        self._site(claim, pos, capture=[param])
        names = _mutated_names(self.source(change["path"]), change["line"])
        self._line(claim, change["line"], path=change["path"], chain=True, capture=_all(names))
        claim.update(names)
        claim["hops"] = [[r["path"], r["line"]] for r in related[:-1]]
        claim["change"] = [change["path"], change["line"]]
        return True

    def _mut114(self, claim, source, pos, report):
        resolved = resolve_function(self.root, report.get("target") or "", self.sources)
        if resolved is None:
            return False
        path, qualname = resolved
        ret_id = f"R{claim['id'][1:]}"
        self.plan["returns"].append({"id": ret_id, "path": path, "qualname": qualname})
        claim["ret"] = ret_id
        names = _mutated_names(source, pos[0])
        self._line(claim, pos[0], capture=_all(names))
        claim.update(names)
        function = _function_of(source, pos[0])
        claim["function"] = function.name if function is not None else None
        return True

    def _mut118(self, claim, source, pos, report):
        """The deep copy is the value of an assignment to a name (`copied = copy.deepcopy(item)`)
        of a name: the line records what the original and the copy hold in the attribute."""
        call = source.node_at(pos, (ast.Call,))
        target = report.get("target") or ""
        if call is None or len(call.args) != 1 or "." not in target:
            return False
        statement = getattr(call, "parent", None)
        arg = call.args[0]
        if not (
            isinstance(arg, ast.Name)
            and isinstance(statement, ast.Assign)
            and len(statement.targets) == 1
            and isinstance(statement.targets[0], ast.Name)
        ):
            return False
        class_qualname, attr = target.rsplit(".", 1)
        result = statement.targets[0].id
        self._line(claim, pos[0], attrs=[[arg.id, attr], [result, attr]])
        claim.update(original=arg.id, copy=result, attr=attr, class_qualname=class_qualname)
        return True

    def _opt_type(self, claim, source, pos, report):
        target = report.get("target") or ""
        types = [target] if target.isidentifier() else []
        self._line(claim, pos[0], types=types)
        claim["expected_types"] = list(OPT_TYPE_RULES.get(claim["rule"], ()))
        claim["type_name"] = target if types else None
        return True

    def _opt113(self, claim, source, pos, report):
        loops = _enclosing_loops(source, pos)
        if len(loops) < 2:
            return False
        inner, outer = loops[0], loops[1]
        target = report.get("target") or ""
        keys = [k.strip() for k in target.split("==")] if "==" in target else []
        test_lines = list(range(pos[0], pos[2] + 1))
        inner_names = _loop_entry("x", inner, claim["path"])["target_names"]
        claim["inner"] = self._loop(
            claim,
            source,
            inner,
            "i",
            track_lines=True,
            keys=keys,
            capture_body=inner_names,
        )
        claim["outer"] = self._loop(claim, source, outer, "o")
        inner_entry = self.plan["loops"][-2]
        claim["allowed_lines"] = sorted({inner_entry["for_line"], *test_lines})
        claim["test_lines"] = test_lines
        claim["keys"] = keys
        claim["inner_names"] = inner_names
        claim["comprehension"] = isinstance(inner, tuple)
        return True


def build(root, reports):
    planner = Planner(root)
    for report in reports:
        if report.get("certainty") == "exact" and report["rule_code"] in EXACT_RULES:
            planner.add(report)
    return planner
