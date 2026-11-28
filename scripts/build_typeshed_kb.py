#!/usr/bin/env python3
"""Write `kb/ecosystem/typeshed_stdlib.tsv` from typeshed's standard-library stubs.

The table gives the Advisory lane the types the stubs declare for the results of calls and for
attribute reads on builtin and standard-library values (docs/ANALYSIS_AND_KB_SPEC.md, "Declared
results of library calls"). It is generated at a pinned typeshed revision and checked in, so heki
has no run-time dependency on typeshed; `--check` regenerates it and fails when it differs.

Rows, one per line, tab separated:
- `C <class> <base>...`: the class's method resolution order past itself, each base a type in the
  class's own type parameters (`~0`, `~1`, ...).
- `A <spelling> <canonical>`: another name of a class or function (a re-export, or the definition
  in a private module of a class a public module re-exports).
- `F <fqn> <kind> <result>...`: what a call returns; kind `f` (a module function), `m` (a method),
  `c` (a class or static method). A result of `__getitem__` may start with `i:` (the subscript is
  no slice) or `s:` (a slice); the results of one row are joined.
- `R <fqn> <type>`: what reading an attribute or a property gives.

Types: `cls`, `cls[a, b]`, `a|b`, `~N`, `Self`, `type[X]`, `...`, `()`, and `?` for what the stubs
leave open (`Any`, `object`, a function's own type variable). Every `sys.version_info` and
`sys.platform` branch is read and joined, so a row holds on every Python and platform heki runs
for; modules removed before the oldest supported Python are skipped.
"""

from __future__ import annotations

import argparse
import ast
import io
import sys
import tarfile
import tempfile
import urllib.request
from dataclasses import dataclass, field
from pathlib import Path

TYPESHED_COMMIT = "2e4d4dcf657f82305709bbe3e86bca4d27d4dd46"
TARBALL = f"https://codeload.github.com/python/typeshed/tar.gz/{TYPESHED_COMMIT}"
REPO = Path(__file__).resolve().parent.parent
OUTPUT = REPO / "kb" / "ecosystem" / "typeshed_stdlib.tsv"
OLDEST_PYTHON = (3, 11)

UNKNOWN = ("?",)
NONE = ("none",)
NEVER = ("never",)
SELF = ("self",)

ANY_NAMES = {"typing.Any", "typing_extensions.Any", "builtins.object"}
NEVER_NAMES = {
    "typing.NoReturn",
    "typing.Never",
    "typing_extensions.NoReturn",
    "typing_extensions.Never",
}
SELF_NAMES = {"typing.Self", "typing_extensions.Self"}
STR_NAMES = {"typing.LiteralString", "typing_extensions.LiteralString"}
BOOL_FORMS = {
    "typing.TypeGuard",
    "typing.TypeIs",
    "typing_extensions.TypeGuard",
    "typing_extensions.TypeIs",
}
WRAPPERS = {
    f"{m}.{n}"
    for m in ("typing", "typing_extensions")
    for n in ("Annotated", "ClassVar", "Final", "Required", "NotRequired", "ReadOnly")
}
TYPE_FORMS = {"builtins.type", "typing.Type", "typing_extensions.Type"}
CALLABLE_FORMS = {
    "typing.Callable",
    "typing_extensions.Callable",
    "collections.abc.Callable",
    "_collections_abc.Callable",
}
TYPING_ALIASES = {
    "List": "builtins.list",
    "Dict": "builtins.dict",
    "Set": "builtins.set",
    "FrozenSet": "builtins.frozenset",
    "Tuple": "builtins.tuple",
    "Type": "builtins.type",
    "DefaultDict": "collections.defaultdict",
    "OrderedDict": "collections.OrderedDict",
    "Counter": "collections.Counter",
    "Deque": "collections.deque",
    "ChainMap": "collections.ChainMap",
    "Text": "builtins.str",
}
TYPEVAR_FACTORIES = {"TypeVar", "ParamSpec", "TypeVarTuple"}
PARAMETRIZERS = {
    "typing.Generic",
    "typing.Protocol",
    "typing_extensions.Generic",
    "typing_extensions.Protocol",
}
SKIPPED_BASES = PARAMETRIZERS | {"builtins.object"}
# Methods whose result says nothing about the value a call gives: constructors and hooks.
SKIPPED_METHODS = {"__init__", "__new__", "__init_subclass__", "__class_getitem__"}
MAX_ALIAS_DEPTH = 8


@dataclass
class FuncDef:
    node: ast.FunctionDef | ast.AsyncFunctionDef
    kind: str  # "f", "m", "c", or "p" (a property getter)


@dataclass
class ClassInfo:
    module: str
    name: str
    bases: list[ast.expr]
    methods: dict[str, list[FuncDef]] = field(default_factory=dict)
    attributes: dict[str, list[ast.expr]] = field(default_factory=dict)

    @property
    def fqn(self) -> str:
        return f"{self.module}.{self.name}"


@dataclass
class Module:
    name: str
    is_package: bool
    bindings: dict[str, str] = field(default_factory=dict)
    exported: set[str] = field(default_factory=set)
    all_names: set[str] | None = None
    star_from: list[str] = field(default_factory=list)
    classes: dict[str, ClassInfo] = field(default_factory=dict)
    functions: dict[str, list[FuncDef]] = field(default_factory=dict)
    aliases: dict[str, ast.expr] = field(default_factory=dict)
    typevars: set[str] = field(default_factory=set)

    def defines(self, name: str) -> bool:
        return (
            name in self.classes
            or name in self.functions
            or name in self.aliases
            or name in self.typevars
        )


def supported(versions: dict[str, str], module: str) -> bool:
    parts = module.split(".")
    for i in range(len(parts), 0, -1):
        spec = versions.get(".".join(parts[:i]))
        if spec is None:
            continue
        _, _, newest = spec.partition("-")
        if not newest:
            return True
        major, minor = (int(p) for p in newest.split("."))
        return (major, minor) >= OLDEST_PYTHON
    return False


def read_versions(stdlib: Path) -> dict[str, str]:
    versions = {}
    for line in (stdlib / "VERSIONS").read_text().splitlines():
        line = line.split("#", 1)[0].strip()
        if line:
            name, _, spec = line.partition(":")
            versions[name.strip()] = spec.strip()
    return versions


def module_files(stdlib: Path) -> list[tuple[str, Path, bool]]:
    versions = read_versions(stdlib)
    found = []
    for path in sorted(stdlib.rglob("*.pyi")):
        rel = path.relative_to(stdlib)
        if any(p.startswith("@") for p in rel.parts):
            continue
        parts = list(rel.with_suffix("").parts)
        is_package = parts[-1] == "__init__"
        if is_package:
            parts.pop()
        name = ".".join(parts)
        if supported(versions, name):
            found.append((name, path, is_package))
    return found


def absolute(module: Module, level: int, target: str | None) -> str:
    base = module.name.split(".")
    if level:
        keep = len(base) - level + (1 if module.is_package else 0)
        base = base[:keep]
        return ".".join(base + ([target] if target else []))
    return target or ""


def is_typevar_call(value: ast.expr) -> bool:
    if not isinstance(value, ast.Call):
        return False
    f = value.func
    name = f.id if isinstance(f, ast.Name) else f.attr if isinstance(f, ast.Attribute) else ""
    return name in TYPEVAR_FACTORIES


def is_alias_value(value: ast.expr) -> bool:
    return isinstance(value, (ast.Name, ast.Attribute, ast.Subscript, ast.BinOp))


def decorator_names(node: ast.FunctionDef | ast.AsyncFunctionDef) -> list[str]:
    names = []
    for d in node.decorator_list:
        d = d.func if isinstance(d, ast.Call) else d
        if isinstance(d, ast.Name):
            names.append(d.id)
        elif isinstance(d, ast.Attribute):
            names.append(d.attr)
    return names


def read_module(name: str, path: Path, is_package: bool) -> Module:
    module = Module(name, is_package)
    tree = ast.parse(path.read_text(encoding="utf-8"), filename=str(path))
    collect(module, tree.body, None)
    return module


def collect(module: Module, body: list[ast.stmt], cls: ClassInfo | None) -> None:
    for stmt in body:
        if isinstance(stmt, ast.If):
            # Every version and platform branch: a row holds wherever heki runs.
            collect(module, stmt.body, cls)
            collect(module, stmt.orelse, cls)
        elif isinstance(stmt, (ast.FunctionDef, ast.AsyncFunctionDef)):
            add_function(module, stmt, cls)
        elif isinstance(stmt, ast.ClassDef):
            if cls is None:
                info = module.classes.setdefault(
                    stmt.name, ClassInfo(module.name, stmt.name, list(stmt.bases))
                )
                collect(module, stmt.body, info)
        elif cls is not None:
            if isinstance(stmt, ast.AnnAssign) and isinstance(stmt.target, ast.Name):
                cls.attributes.setdefault(stmt.target.id, []).append(stmt.annotation)
        elif isinstance(stmt, ast.Import):
            for alias in stmt.names:
                if alias.asname:
                    module.bindings[alias.asname] = alias.name
                    if alias.asname == alias.name:
                        module.exported.add(alias.asname)
                else:
                    top = alias.name.split(".")[0]
                    module.bindings[top] = top
        elif isinstance(stmt, ast.ImportFrom):
            source = absolute(module, stmt.level, stmt.module)
            for alias in stmt.names:
                if alias.name == "*":
                    module.star_from.append(source)
                    continue
                bound = alias.asname or alias.name
                module.bindings[bound] = f"{source}.{alias.name}"
                if alias.asname == alias.name:
                    module.exported.add(bound)
        elif isinstance(stmt, ast.Assign) and len(stmt.targets) == 1:
            target = stmt.targets[0]
            if not isinstance(target, ast.Name):
                continue
            if target.id == "__all__":
                module.all_names = names_in(stmt.value) | (module.all_names or set())
            elif is_typevar_call(stmt.value):
                module.typevars.add(target.id)
            elif is_alias_value(stmt.value):
                module.aliases.setdefault(target.id, stmt.value)
        elif isinstance(stmt, ast.AugAssign) and isinstance(stmt.target, ast.Name):
            if stmt.target.id == "__all__":
                module.all_names = names_in(stmt.value) | (module.all_names or set())
        elif isinstance(stmt, ast.AnnAssign) and isinstance(stmt.target, ast.Name):
            if is_type_alias_annotation(stmt.annotation) and stmt.value is not None:
                module.aliases.setdefault(stmt.target.id, stmt.value)


def names_in(value: ast.expr) -> set[str]:
    if isinstance(value, (ast.List, ast.Tuple)):
        return {e.value for e in value.elts if isinstance(e, ast.Constant)}
    return set()


def is_type_alias_annotation(annotation: ast.expr) -> bool:
    name = (
        annotation.id
        if isinstance(annotation, ast.Name)
        else annotation.attr
        if isinstance(annotation, ast.Attribute)
        else ""
    )
    return name == "TypeAlias"


def add_function(
    module: Module, node: ast.FunctionDef | ast.AsyncFunctionDef, cls: ClassInfo | None
) -> None:
    decorators = decorator_names(node)
    if cls is None:
        module.functions.setdefault(node.name, []).append(FuncDef(node, "f"))
        return
    if "setter" in decorators or "deleter" in decorators:
        return
    if "property" in decorators or "cached_property" in decorators:
        kind = "p"
    elif "classmethod" in decorators or "staticmethod" in decorators:
        kind = "c"
    else:
        kind = "m"
    cls.methods.setdefault(node.name, []).append(FuncDef(node, kind))


class Stubs:
    def __init__(self, modules: dict[str, Module]) -> None:
        self.modules = modules
        self.builtins = modules["builtins"]
        self.typevars = {f"{m.name}.{t}" for m in modules.values() for t in m.typevars}
        self.classes = {c.fqn: c for m in modules.values() for c in m.classes.values()}
        self.functions = {
            f"{m.name}.{n}": defs for m in modules.values() for n, defs in m.functions.items()
        }
        self._definitions: dict[str, str] = {}

    def split(self, fqn: str) -> tuple[Module, str] | None:
        module, _, name = fqn.rpartition(".")
        found = self.modules.get(module)
        return (found, name) if found and name else None

    def definition(self, fqn: str, seen: frozenset[str] = frozenset()) -> str:
        """Where the object `fqn` names is defined, through imports and star imports."""
        if fqn in self._definitions:
            return self._definitions[fqn]
        result = fqn
        found = self.split(fqn)
        if found and fqn not in seen:
            module, name = found
            if not module.defines(name):
                if name in module.bindings:
                    result = self.definition(module.bindings[name], seen | {fqn})
                else:
                    for source in module.star_from:
                        star = self.modules.get(source)
                        if star and self.star_exports(star, name):
                            result = self.definition(f"{source}.{name}", seen | {fqn})
                            break
        self._definitions[fqn] = result
        return result

    def star_exports(self, module: Module, name: str) -> bool:
        if module.all_names is not None:
            return name in module.all_names
        if name.startswith("_"):
            return False
        return module.defines(name) or name in module.exported or bool(module.star_from)

    def resolve(self, module: Module, name: str) -> str | None:
        if module.defines(name):
            return self.definition(f"{module.name}.{name}")
        if name in module.bindings:
            return self.definition(module.bindings[name])
        for source in module.star_from:
            star = self.modules.get(source)
            if star and self.star_exports(star, name):
                return self.definition(f"{source}.{name}")
        if self.builtins.defines(name):
            return f"builtins.{name}"
        return None

    def dotted(self, module: Module, expr: ast.expr) -> str | None:
        if isinstance(expr, ast.Name):
            return self.resolve(module, expr.id)
        if isinstance(expr, ast.Attribute):
            head = self.dotted(module, expr.value)
            return self.definition(f"{head}.{expr.attr}") if head else None
        return None


@dataclass(frozen=True)
class Scope:
    module: Module
    params: tuple[str, ...]
    depth: int = 0


def union(members: list[tuple]) -> tuple:
    flat: list[tuple] = []
    for m in members:
        flat.extend(m[1] if m[0] == "union" else [m])
    if any(m == UNKNOWN for m in flat):
        return UNKNOWN
    had_none = any(m == NONE for m in flat)
    kept = sorted({m for m in flat if m not in (NONE, NEVER)}, key=serialize)
    if not kept:
        return NONE if had_none else NEVER
    return kept[0] if len(kept) == 1 else ("union", tuple(kept))


def serialize(t: tuple) -> str:
    match t:
        case ("?",):
            return "?"
        case ("self",):
            return "Self"
        case ("param", n):
            return f"~{n}"
        case ("lit", text):
            return text
        case ("type", inner):
            return f"type[{serialize(inner)}]"
        case ("union", members):
            return "|".join(serialize(m) for m in members)
        case ("cls", fqn, args):
            return f"{fqn}[{', '.join(serialize(a) for a in args)}]" if args else fqn
    raise ValueError(t)


class Converter:
    def __init__(self, stubs: Stubs, canonical: dict[str, str]) -> None:
        self.stubs = stubs
        self.canonical = canonical

    def cls(self, fqn: str, args: tuple = ()) -> tuple:
        return ("cls", self.canonical.get(fqn, fqn), args)

    def name(self, fqn: str | None, scope: Scope) -> tuple:
        if fqn is None or fqn in ANY_NAMES:
            return UNKNOWN
        if fqn in self.stubs.typevars:
            return ("param", scope.params.index(fqn)) if fqn in scope.params else UNKNOWN
        if fqn in SELF_NAMES:
            return SELF
        if fqn in NEVER_NAMES:
            return NEVER
        if fqn in STR_NAMES:
            return self.cls("builtins.str")
        if fqn in BOOL_FORMS:
            return self.cls("builtins.bool")
        module, _, last = fqn.rpartition(".")
        if module in ("typing", "typing_extensions") and last in TYPING_ALIASES:
            return self.cls(TYPING_ALIASES[last])
        if fqn in CALLABLE_FORMS:
            return self.cls("collections.abc.Callable")
        found = self.stubs.split(fqn)
        if found and found[1] in found[0].aliases:
            if scope.depth >= MAX_ALIAS_DEPTH:
                return UNKNOWN
            alias_scope = Scope(found[0], (), scope.depth + 1)
            return self.convert(found[0].aliases[found[1]], alias_scope)
        if fqn in self.stubs.classes:
            return self.cls(fqn)
        return UNKNOWN

    def convert(self, expr: ast.expr | None, scope: Scope) -> tuple:
        match expr:
            case None:
                return UNKNOWN
            case ast.Constant(value=None):
                return NONE
            case ast.Constant(value=str() as text):
                try:
                    parsed = ast.parse(text, mode="eval").body
                except SyntaxError:
                    return UNKNOWN
                return self.convert(parsed, scope)
            case ast.Name() | ast.Attribute():
                return self.name(self.stubs.dotted(scope.module, expr), scope)
            case ast.BinOp(op=ast.BitOr()):
                return union([self.convert(expr.left, scope), self.convert(expr.right, scope)])
            case ast.Subscript():
                return self.subscript(expr, scope)
        return UNKNOWN

    def subscript(self, expr: ast.Subscript, scope: Scope) -> tuple:
        head = self.stubs.dotted(scope.module, expr.value)
        items = list(expr.slice.elts) if isinstance(expr.slice, ast.Tuple) else [expr.slice]
        last = head.rpartition(".")[2] if head else ""
        typing_form = bool(head) and head.rpartition(".")[0] in ("typing", "typing_extensions")
        if head is None:
            return UNKNOWN
        if typing_form and last == "Optional":
            return union([self.convert(items[0], scope), NONE])
        if typing_form and last == "Union":
            return union([self.convert(i, scope) for i in items])
        if head in WRAPPERS:
            return self.convert(items[0], scope)
        if typing_form and last == "Literal":
            return union([self.literal(i, scope) for i in items])
        if head in BOOL_FORMS:
            return self.cls("builtins.bool")
        if head in TYPE_FORMS:
            inner = self.convert(items[0], scope)
            return ("type", inner) if inner[0] in ("cls", "self") else UNKNOWN
        if head in CALLABLE_FORMS:
            result = self.argument(items[-1], scope) if len(items) == 2 else UNKNOWN
            return self.cls("collections.abc.Callable", (("lit", "..."), result))
        base = self.name(head, scope)
        if base[0] != "cls":
            return UNKNOWN
        if base[1] == "builtins.tuple":
            if isinstance(expr.slice, ast.Tuple) and not expr.slice.elts:
                return self.cls("builtins.tuple", (("lit", "()"),))
            args = tuple(
                ("lit", "...")
                if isinstance(i, ast.Constant) and i.value is Ellipsis
                else self.argument(i, scope)
                for i in items
            )
            return self.cls("builtins.tuple", args)
        return ("cls", base[1], tuple(self.argument(i, scope) for i in items))

    def argument(self, item: ast.expr, scope: Scope) -> tuple:
        """A type argument: a parameter list (`Callable`'s), `None` and `Never` say nothing."""
        if isinstance(item, ast.List):
            return UNKNOWN
        t = self.convert(item, scope)
        return UNKNOWN if t in (NONE, NEVER) else t

    def literal(self, item: ast.expr, scope: Scope) -> tuple:
        match item:
            case ast.Constant(value=None):
                return NONE
            case ast.Constant(value=bool()):
                return self.cls("builtins.bool")
            case ast.Constant(value=int()):
                return self.cls("builtins.int")
            case ast.Constant(value=str()):
                return self.cls("builtins.str")
            case ast.Constant(value=bytes()):
                return self.cls("builtins.bytes")
            case ast.UnaryOp(operand=ast.Constant(value=int())):
                return self.cls("builtins.int")
            case ast.Attribute():
                enum_class = self.stubs.dotted(scope.module, item.value)
                return self.name(enum_class, scope)
        return UNKNOWN


def typevars_in(stubs: Stubs, module: Module, expr: ast.expr, out: list[str]) -> None:
    for node in ast.walk(expr):
        if isinstance(node, (ast.Name, ast.Attribute)):
            fqn = stubs.dotted(module, node)
            if fqn in stubs.typevars and fqn not in out:
                out.append(fqn)


def class_params(stubs: Stubs, info: ClassInfo) -> tuple[str, ...]:
    module = stubs.modules[info.module]
    for base in info.bases:
        if isinstance(base, ast.Subscript) and stubs.dotted(module, base.value) in PARAMETRIZERS:
            found: list[str] = []
            typevars_in(stubs, module, base.slice, found)
            return tuple(found)
    found = []
    for base in info.bases:
        if isinstance(base, ast.Subscript):
            typevars_in(stubs, module, base.slice, found)
    return tuple(found)


def substitute(t: tuple, env: tuple) -> tuple:
    match t:
        case ("param", n):
            return env[n] if n < len(env) else UNKNOWN
        case ("type", inner):
            return ("type", substitute(inner, env))
        case ("union", members):
            return union([substitute(m, env) for m in members])
        case ("cls", fqn, args):
            return ("cls", fqn, tuple(substitute(a, env) for a in args))
    return t


class Table:
    def __init__(self, stubs: Stubs) -> None:
        self.stubs = stubs
        self.canonical = canonical_names(stubs)
        self.convert = Converter(stubs, self.canonical)
        self.params = {fqn: class_params(stubs, c) for fqn, c in stubs.classes.items()}
        self._bases: dict[str, list[tuple]] = {}
        self._mro: dict[str, list[tuple]] = {}

    def scope(self, info: ClassInfo | None, module: Module) -> Scope:
        return Scope(module, self.params[info.fqn] if info else ())

    def bases(self, info: ClassInfo) -> list[tuple]:
        if info.fqn not in self._bases:
            scope = self.scope(info, self.stubs.modules[info.module])
            found = []
            for expr in info.bases:
                head = expr.value if isinstance(expr, ast.Subscript) else expr
                fqn = self.stubs.dotted(scope.module, head)
                if fqn is None or fqn in SKIPPED_BASES or fqn not in self.stubs.classes:
                    continue
                t = self.convert.convert(expr, scope)
                if t[0] == "cls" and t[1] != info.fqn:
                    found.append(("cls", fqn, t[2]))
            self._bases[info.fqn] = found
        return self._bases[info.fqn]

    def mro(self, fqn: str, seen: frozenset[str] = frozenset()) -> list[tuple]:
        """The bases past `fqn` in method resolution order, in `fqn`'s type parameters."""
        if fqn in self._mro:
            return self._mro[fqn]
        if fqn in seen:
            return []
        linear: list[tuple] = []
        for base in self.bases(self.stubs.classes[fqn]):
            env = base[2]
            for entry in [("cls", base[1], tuple(("param", i) for i in range(len(env))))] + self.mro(base[1], seen | {fqn}):
                placed = substitute(entry, env) if env else ("cls", entry[1], ())
                if all(e[1] != placed[1] for e in linear):
                    linear.append(placed)
        self._mro[fqn] = linear
        return linear

    def result(self, defs: list[FuncDef], scope: Scope, subscript: bool) -> list[str]:
        alts = []
        for d in defs:
            t = self.convert.convert(d.node.returns, scope)
            if t == NONE or t == NEVER:
                continue
            text = serialize(union([t]))
            if subscript:
                text = f"{subscript_condition(d, scope, self.convert)}{text}"
            if text not in alts:
                alts.append(text)
        if all(a.rpartition(":")[2] == "?" for a in alts):
            return []
        return alts

    def rows(self) -> list[str]:
        out: set[str] = set()
        for spelling, target in self.canonical.items():
            if spelling != target:
                out.add(f"A\t{spelling}\t{target}")
        for alias, target in aliases_of_functions(self.stubs).items():
            out.add(f"A\t{alias}\t{target}")
        for fqn, info in self.stubs.classes.items():
            name = self.canonical.get(fqn, fqn)
            mro = [serialize(e) for e in self.mro(fqn)]
            if mro:
                out.add("\t".join(["C", name, *mro]))
            scope = self.scope(info, self.stubs.modules[info.module])
            for method, defs in info.methods.items():
                if method in SKIPPED_METHODS:
                    continue
                getters = [d for d in defs if d.kind == "p"]
                if getters:
                    alts = self.result(getters, scope, False)
                    if alts:
                        out.add(f"R\t{name}.{method}\t{'|'.join(alts)}")
                    continue
                kind = "c" if all(d.kind == "c" for d in defs) else "m"
                alts = self.result(defs, scope, method == "__getitem__")
                if alts:
                    out.add("\t".join(["F", f"{name}.{method}", kind, *alts]))
            for attr, annotations in info.attributes.items():
                t = union([self.convert.convert(a, scope) for a in annotations])
                if t not in (UNKNOWN, NONE, NEVER):
                    out.add(f"R\t{name}.{attr}\t{serialize(t)}")
        for fqn, defs in self.stubs.functions.items():
            module = self.stubs.modules[fqn.rpartition(".")[0]]
            alts = self.result(defs, Scope(module, ()), False)
            if alts:
                out.add("\t".join(["F", fqn, "f", *alts]))
        return sorted(out)


def subscript_condition(d: FuncDef, scope: Scope, convert: Converter) -> str:
    """`s:` for an overload of `__getitem__` taking only a slice, `i:` for one taking none."""
    params = d.node.args.posonlyargs + d.node.args.args
    if len(params) < 2:
        return ""
    t = convert.convert(params[1].annotation, scope)
    members = t[1] if t[0] == "union" else (t,)
    slices = [m[0] == "cls" and m[1] == "builtins.slice" for m in members]
    if all(slices):
        return "s:"
    if not any(slices) and t != UNKNOWN:
        return "i:"
    return ""


def public(fqn: str) -> bool:
    return not any(part.startswith("_") for part in fqn.rpartition(".")[0].split("."))


def spellings(stubs: Stubs) -> dict[str, set[str]]:
    """Every exported spelling of each class and function, by its definition."""
    found: dict[str, set[str]] = {}
    for module in stubs.modules.values():
        names = set(module.exported) | (module.all_names or set())
        for name in names:
            if module.defines(name) or name not in module.bindings:
                continue
            target = stubs.definition(module.bindings[name])
            if target in stubs.classes or target in stubs.functions:
                found.setdefault(target, set()).add(f"{module.name}.{name}")
        for source in module.star_from:
            star = stubs.modules.get(source)
            if star is None:
                continue
            for name in star.all_names or {n for n in star.classes} | set(star.functions):
                if name.startswith("_") and star.all_names is None:
                    continue
                target = stubs.definition(f"{source}.{name}")
                if target in stubs.classes or target in stubs.functions:
                    found.setdefault(target, set()).add(f"{module.name}.{name}")
    return found


def preferred(definition: str, names: set[str]) -> str:
    if public(definition):
        return definition
    candidates = sorted((n for n in names if public(n)), key=lambda n: (n.count("."), n))
    return candidates[0] if candidates else definition


def canonical_names(stubs: Stubs) -> dict[str, str]:
    """Each spelling of a class, and the definition, to the spelling the table uses."""
    canonical = {}
    for definition, names in spellings(stubs).items():
        if definition not in stubs.classes:
            continue
        chosen = preferred(definition, names)
        for name in names | {definition}:
            canonical[name] = chosen
    for fqn in stubs.classes:
        canonical.setdefault(fqn, fqn)
    return canonical


def aliases_of_functions(stubs: Stubs) -> dict[str, str]:
    found = {}
    for definition, names in spellings(stubs).items():
        if definition in stubs.functions:
            for name in names - {definition}:
                found[name] = definition
    return found


def load(stdlib: Path) -> Stubs:
    modules = {}
    for name, path, is_package in module_files(stdlib):
        modules[name] = read_module(name, path, is_package)
    return Stubs(modules)


def render(stdlib: Path) -> str:
    rows = Table(load(stdlib)).rows()
    header = [
        f"# Generated by scripts/build_typeshed_kb.py from typeshed {TYPESHED_COMMIT} (stdlib); do not edit.",
        "# Rows and types: the script's docstring and docs/ANALYSIS_AND_KB_SPEC.md.",
    ]
    return "\n".join(header + rows) + "\n"


def download(into: Path) -> Path:
    with urllib.request.urlopen(TARBALL, timeout=120) as response:
        data = response.read()
    with tarfile.open(fileobj=io.BytesIO(data), mode="r:gz") as archive:
        members = [m for m in archive.getmembers() if "/stdlib/" in m.name or m.name.endswith("/stdlib")]
        archive.extractall(into, members=members, filter="data")
    return next(into.iterdir()) / "stdlib"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--typeshed", type=Path, help=f"a typeshed checkout at {TYPESHED_COMMIT}")
    parser.add_argument("--check", action="store_true", help="fail if the table differs")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory() as tmp:
        stdlib = args.typeshed / "stdlib" if args.typeshed else download(Path(tmp))
        text = render(stdlib)
    if args.check:
        if OUTPUT.read_text(encoding="utf-8") != text:
            print(f"{OUTPUT.relative_to(REPO)} is out of date: run scripts/build_typeshed_kb.py", file=sys.stderr)
            return 1
        return 0
    OUTPUT.write_text(text, encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
