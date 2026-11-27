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
    "EFF109",
    "EFF116",
    "DATA101",
    "MUT101",
    "MUT110",
    "MUT111",
    "MUT114",
    "MUT115",
    "MUT118",
    "MUT119",
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


def _changed_expressions(source, line):
    """What a statement on `line` changes in place, as `(source, kind)` candidates in the order
    a checker tries them: the receiver of a method call (`call`), the object of a subscript or
    attribute store or `del` (`store`); for an augmented assignment to a subscript or an
    attribute, its target when an in-place operator changes that object (`inplace`: `a["k"] +=
    [x]` changes the list), else the object it stores into (`store`: `a["k"]["n"] += 1` changes
    `a["k"]`)."""
    for node in ast.walk(source.tree):
        if getattr(node, "lineno", None) != line:
            continue
        segment = lambda n: ast.get_source_segment(source.text, n)  # noqa: E731
        if isinstance(node, ast.Expr) and isinstance(node.value, ast.Call):
            func = node.value.func
            if isinstance(func, ast.Attribute):
                return [(segment(func.value), "call")]
        elif isinstance(node, (ast.Assign, ast.Delete)):
            targets = node.targets
            if len(targets) == 1 and isinstance(targets[0], (ast.Subscript, ast.Attribute)):
                return [(segment(targets[0].value), "store")]
        elif isinstance(node, ast.AugAssign):
            if isinstance(node.target, (ast.Subscript, ast.Attribute)):
                return [(segment(node.target), "inplace"), (segment(node.target.value), "store")]
            if isinstance(node.target, ast.Name):
                return [(node.target.id, "inplace")]
    return []


def _copied_expression(source, line):
    """The source of the value a shallow copy on `line` copies: the receiver of `x.copy()` /
    `x.model_copy()`, the argument of `dict(x)` / `copy.copy(x)`, the value a display unpacks
    (`{**x}`, `[*x]`), the value sliced."""
    for node in ast.walk(source.tree):
        if getattr(node, "lineno", None) != line:
            continue
        if isinstance(node, ast.Call):
            func = node.func
            if isinstance(func, ast.Attribute) and func.attr in ("copy", "model_copy") and not node.args:
                return ast.get_source_segment(source.text, func.value)
            if node.args and not isinstance(node.args[0], ast.Starred):
                return ast.get_source_segment(source.text, node.args[0])
        if isinstance(node, ast.Dict) and node.keys == [None]:
            return ast.get_source_segment(source.text, node.values[0])
        if isinstance(node, ast.List) and len(node.elts) == 1 and isinstance(node.elts[0], ast.Starred):
            return ast.get_source_segment(source.text, node.elts[0].value)
        if isinstance(node, ast.Subscript) and isinstance(node.slice, ast.Slice):
            return ast.get_source_segment(source.text, node.value)
    return None


def _function_of(source, line):
    best = None
    for node in ast.walk(source.tree):
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)):
            if node.lineno <= line <= node.end_lineno:
                if best is None or node.lineno >= best.lineno:
                    best = node
    return best


def find_function(root, qualname, sources):
    """The project file that defines the function `qualname` (`Class.method`), when one does."""
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if not d.startswith("."))
        for name in sorted(filenames):
            if not name.endswith(".py"):
                continue
            path = os.path.relpath(os.path.join(dirpath, name), root).replace(os.sep, "/")
            if path not in sources:
                try:
                    sources[path] = Source(root, path)
                except (SyntaxError, UnicodeDecodeError):
                    continue
            if qualname in _qualnames(sources[path].tree):
                return path
    return None


def _qualnames(tree):
    """The qualified names of the functions a module defines in classes and at its top."""
    out = set()

    def visit(node, prefix):
        for child in ast.iter_child_nodes(node):
            if isinstance(child, ast.ClassDef):
                visit(child, f"{prefix}{child.name}.")
            elif isinstance(child, (ast.FunctionDef, ast.AsyncFunctionDef)):
                out.add(prefix + child.name)

    visit(tree, "")
    return out


def _rebinds(source, line, attr):
    """Whether the statement on `line` rebinds or deletes `<name>.attr` (an assignment, an
    augmented assignment, or `del`) rather than changing its object in place."""
    for node in ast.walk(source.tree):
        if getattr(node, "lineno", None) != line:
            continue
        targets = []
        if isinstance(node, ast.Assign):
            targets = node.targets
        elif isinstance(node, (ast.AnnAssign, ast.AugAssign)):
            targets = [node.target]
        elif isinstance(node, ast.Delete):
            targets = node.targets
        if any(isinstance(t, ast.Attribute) and t.attr == attr for t in targets):
            return True
    return False


def _loaded_target(source, target, rebound=frozenset()):
    """An assignment target as an expression `monitor.static_eval` loads without running code
    (`env`, `self.env`, `opts[k]`), or `None`. A target whose base or key the statement binds
    again is `None`: the store used the earlier value, the readback would use the new one."""
    if isinstance(target, ast.Name):
        return target.id
    if any(isinstance(n, ast.Name) and n.id in rebound for n in ast.walk(target)):
        return None
    if isinstance(target, ast.Attribute) and isinstance(target.value, ast.Name):
        return ast.get_source_segment(source.text, target)
    if (
        isinstance(target, ast.Subscript)
        and isinstance(target.value, ast.Name)
        and isinstance(target.slice, (ast.Constant, ast.Name))
    ):
        return ast.get_source_segment(source.text, target)
    return None


def _alone_on_its_line(source, statement):
    """Whether `statement` is on one line that no other statement shares: the monitor's line
    event ends when execution moves to another line, so it holds exactly one run of the
    statement."""
    line = statement.lineno
    if statement.end_lineno != line:
        return False
    ancestors = set()
    node = getattr(statement, "parent", None)
    while node is not None:
        ancestors.add(id(node))
        node = getattr(node, "parent", None)
    for other in ast.walk(source.tree):
        if isinstance(other, ast.ExceptHandler):
            # Its match runs on that line, its body only when the match succeeds.
            if other.lineno == line:
                return False
            continue
        if not isinstance(other, ast.stmt) or other is statement:
            continue
        if other.lineno == line or (other.end_lineno == line and id(other) not in ancestors):
            return False
    return True


def _base_kind(node):
    """How the changed object's expression makes it, as the checker tells the cases apart."""
    if isinstance(node, (ast.List, ast.Dict, ast.Set, ast.ListComp, ast.DictComp, ast.SetComp)):
        return "display"
    if isinstance(node, ast.Subscript):
        return "slice"
    if isinstance(node, ast.Attribute):
        return "attribute"
    return "call"


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

    def _eff109(self, claim, source, pos, report):
        self._site(claim, pos)
        return True

    def _eff116(self, claim, source, pos, report):
        # A generator handed to a library call (`threading.Thread->scan`) runs where the library
        # calls it, which the runs do not follow.
        target = report.get("target") or ""
        if not target or "->" in target:
            return False
        self._site(claim, pos)
        claim["generator"] = target
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

    def _mut101(self, claim, source, pos, report):
        target = report.get("target") or ""
        written, _, prop = target.partition("->")
        owner, _, attr = written.rpartition(".")
        writer = report.get("qualname")
        if not (prop and owner and attr and writer):
            return False
        getter = f"{owner}.{prop}"
        getter_path = find_function(self.root, getter, self.sources)
        if getter_path is None:
            return False
        ret_id = f"R{claim['id'][1:]}"
        self.plan["returns"].append({"id": ret_id, "path": getter_path, "qualname": getter})
        writer_ret = f"W{claim['id'][1:]}"
        self.plan["returns"].append(
            {"id": writer_ret, "path": claim["path"], "qualname": writer, "kept": {"prop": prop}}
        )
        self._line(claim, pos[0], kept={"prop": prop, "attr": attr, "getter": getter})
        claim.update(
            {
                "ret": ret_id,
                "writer_ret": writer_ret,
                "prop": prop,
                "attr": attr,
                "getter": getter,
                "rebinds": _rebinds(source, pos[0], attr),
            }
        )
        return True

    def _mut111(self, claim, source, pos, report):
        target = report.get("target") or ""
        owner, _, prop = target.rpartition(".")
        node = source.node_at(pos, (ast.Attribute,))
        receiver = node.value.id if node is not None and isinstance(node.value, ast.Name) else None
        writers = [r for r in report.get("related", [])]
        if not (owner and prop and receiver):
            return False
        getter = f"{owner}.{prop}"
        self._line(
            claim,
            pos[0],
            kept={"prop": prop, "attr": prop, "getter": getter, "receiver": receiver},
        )
        claim.update({"prop": prop, "getter": getter, "writers": len(writers)})
        return True

    def _mut115(self, claim, source, pos, report):
        """The copy call (the first related location) and the change: for each candidate of
        what the line changes, what the copied value holds at its access path when the copy line
        starts, what the copy holds there when it ends, and the object at the change line."""
        related = report.get("related", [])
        copy_var = report.get("target") or ""
        if not related or not copy_var.isidentifier():
            return False
        original = _copied_expression(source, related[0]["line"])
        if not original:
            return False
        changes = []
        for changed, kind in _changed_expressions(source, pos[0]):
            if not changed or not changed.startswith((copy_var + ".", copy_var + "[")):
                continue
            path = changed[len(copy_var):]
            changes.append(
                {"held": f"({original}){path}", "copy": f"{copy_var}{path}", "changed": changed, "kind": kind}
            )
        if not changes:
            return False
        copy_id = f"Nc{claim['id'][1:]}"
        self.plan["lines"].append(
            {
                "id": copy_id,
                "path": claim["path"],
                "line": related[0]["line"],
                "evals": sorted({e for c in changes for e in (c["held"], c["copy"])}),
            }
        )
        self._line(claim, pos[0], evals=sorted({c["changed"] for c in changes}))
        claim["copy_line_id"] = copy_id
        claim["changes"] = changes
        return True

    def _mut118(self, claim, source, pos, report):
        """The deep copy is the value of an assignment to a name (`copied = copy.deepcopy(item)`)
        of a name, on one line: the line records what the original and the copy hold in the
        attribute. A line event ends when execution moves to another line, so a statement over
        several lines would end before the copy is bound."""
        call = source.node_at(pos, (ast.Call,))
        target = report.get("target") or ""
        if call is None or len(call.args) != 1 or "." not in target:
            return False
        statement = getattr(call, "parent", None)
        arg = call.args[0]
        if not (
            isinstance(arg, ast.Name)
            and isinstance(statement, ast.Assign)
            and statement.lineno == statement.end_lineno
            and len(statement.targets) == 1
            and isinstance(statement.targets[0], ast.Name)
        ):
            return False
        class_qualname, attr = target.rsplit(".", 1)
        result = statement.targets[0].id
        self._line(claim, pos[0], attrs=[[arg.id, attr], [result, attr]])
        claim.update(original=arg.id, copy=result, attr=attr, class_qualname=class_qualname)
        return True

    def _mut119(self, claim, source, pos, report):
        """The change is a statement alone on one line: a call of the changing method whose
        result is unused or bound or stored (`env = os.environ.copy().update(...)`), an item
        assignment, or an item deletion. The line records the object the change ran on (the
        method's receiver, or what project code at the subscript's object returned), whether
        anything holds it when the line ends, and what the assignment's targets hold then."""
        node = source.node_at(pos, (ast.Call, ast.Assign, ast.Subscript))
        targets = []
        if isinstance(node, ast.Call) and isinstance(node.func, ast.Attribute):
            form, change, base, statement = "method", node, node.func.value, getattr(node, "parent", None)
            if isinstance(statement, ast.Assign) and statement.value is node:
                targets = statement.targets
            elif not isinstance(statement, ast.Expr):
                return False
        elif isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Subscript):
            form, change, statement = "store", node.targets[0], node
            base = change.value
        elif isinstance(node, ast.Subscript) and isinstance(getattr(node, "parent", None), ast.Delete):
            form, change, base, statement = "delete", node, node.value, node.parent
        else:
            return False
        if not _alone_on_its_line(source, statement):
            return False
        rebound = frozenset(
            n.id for n in ast.walk(statement) if isinstance(n, ast.Name) and isinstance(n.ctx, ast.Store)
        )
        loaded = [_loaded_target(source, t, rebound) for t in targets]
        results = [r for r in loaded if r is not None]
        lost = {"change": _node_pos(change), "base": _node_pos(base), "results": results}
        if isinstance(base, ast.Subscript) and isinstance(base.slice, ast.Slice):
            bounds = [base.slice.lower, base.slice.upper, base.slice.step]
            if all(b is None or isinstance(b, ast.Constant) for b in bounds):
                lost["sliced"] = _loaded_target(source, base.value, rebound)
        self._line(claim, statement.lineno, lost=lost)
        kind, _, member = (report.get("target") or "").partition(".")
        claim.update(
            form=form,
            kind=kind,
            member=member,
            base=_base_kind(base),
            results=results,
            unloaded_results=len(loaded) - len(results),
        )
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
