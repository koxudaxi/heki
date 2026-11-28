//! Receiver typing by variable: what `TypeCtx::infer` gives the named variables of a function.

use crate::ModuleInput;
use crate::resolver::{Callee, TypeCtx, Val, project_decls_for};
use heki_ast::parse_module;
use heki_env::{PythonVersion, RuntimeProfile};
use heki_ir::{ModuleIr, build_module_ir};
use heki_kb::EffectiveEcosystemKb;
use heki_semantic::{SemanticModel, analyze_module};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

struct Built {
    module_name: String,
    semantic: SemanticModel,
    ir: ModuleIr,
}

fn build(module_name: &str, src: &str, version: PythonVersion) -> Built {
    let parsed = parse_module(src).expect("valid python");
    let profile = RuntimeProfile::from(version);
    let semantic = analyze_module(&parsed, src, profile);
    let ir = build_module_ir(&parsed, src, &semantic, profile);
    Built {
        module_name: module_name.into(),
        semantic,
        ir,
    }
}

fn input(b: &Built) -> ModuleInput<'_> {
    ModuleInput {
        path: PathBuf::from(format!("{}.py", b.module_name.replace('.', "/"))),
        module_name: b.module_name.clone(),
        is_package: false,
        root_is_package: false,
        content_hash: String::new(),
        semantic: &b.semantic,
        ir: &b.ir,
    }
}

fn show(v: &Val) -> String {
    match v {
        Val::Ref(f) => format!("ref {f}"),
        Val::Class(c) => format!("class {c}"),
        Val::Inst(c, _) => c.clone(),
        Val::Super(c) => format!("super {c}"),
        Val::Union(cs) => cs.join(" | "),
        Val::OneOf(fs) => format!("one of {}", fs.join(", ")),
    }
}

/// `name -> values` of the variables of `qualname` in the first module, for modules
/// `(module name, source)` analyzed for `version`.
fn types_at(
    files: &[(&str, &str)],
    version: PythonVersion,
    qualname: &str,
) -> BTreeMap<String, BTreeSet<String>> {
    let built: Vec<Built> = files.iter().map(|(m, s)| build(m, s, version)).collect();
    let inputs: Vec<ModuleInput<'_>> = built.iter().map(input).collect();
    let kb = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
    let (decls, _) = project_decls_for(&inputs, &kb, None, false);
    let cx = TypeCtx::new(&inputs[0], &decls, &kb);
    let f = built[0]
        .ir
        .functions
        .iter()
        .find(|f| f.qualname == qualname)
        .unwrap_or_else(|| panic!("no function `{qualname}`"));
    let mut out: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for (var, val) in cx.infer(f) {
        if let Some(name) = f.var_name(var) {
            out.entry(name.to_owned()).or_default().insert(show(&val));
        }
    }
    out
}

/// Run `check` with the type context of the first of the modules `(module name, source)`.
fn with_cx(files: &[(&str, &str)], check: impl FnOnce(&TypeCtx<'_>)) {
    let built: Vec<Built> = files
        .iter()
        .map(|(m, s)| build(m, s, PythonVersion::PY311))
        .collect();
    let inputs: Vec<ModuleInput<'_>> = built.iter().map(input).collect();
    let kb = EffectiveEcosystemKb::load_builtin(&[]).unwrap();
    let (decls, _) = project_decls_for(&inputs, &kb, None, false);
    check(&TypeCtx::new(&inputs[0], &decls, &kb));
}

/// A callee as `kind: targets`.
fn callee(c: &Callee) -> String {
    match c {
        Callee::Project(keys, binds) => {
            let names: Vec<String> = keys.iter().map(|k| k.qualname.clone()).collect();
            format!("project {binds} {}", names.join(","))
        }
        Callee::External(s) => format!("external {s}"),
        Callee::TooMany => "too many".into(),
        Callee::Unresolved => "unresolved".into(),
    }
}

fn types(src: &str, qualname: &str) -> BTreeMap<String, BTreeSet<String>> {
    types_at(&[("m", src)], PythonVersion::PY311, qualname)
}

/// The values of `name` (empty when it has none).
fn of(types: &BTreeMap<String, BTreeSet<String>>, name: &str) -> Vec<String> {
    types
        .get(name)
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default()
}

fn one(c: &str) -> Vec<String> {
    vec![c.to_owned()]
}

const CLASSES: &str = r#"
from typing import Iterator


class Model:
    def __init__(self):
        self.fields = []


class Field:
    pass


class Sub(Model):
    pass


class Bag:
    def __iter__(self) -> Iterator[Field]:
        return iter([])

    def __getitem__(self, i) -> Model:
        return Model()


class Plain:
    pass
"#;

fn with_classes(body: &str) -> String {
    format!("{CLASSES}\n{body}")
}

#[test]
fn annotations_give_items_to_loops_subscripts_and_unpacking() {
    let src = with_classes(
        r#"
from typing import AbstractSet, AsyncIterator, Counter, Generator, ItemsView, Iterable, Mapping, Optional, Sequence
import collections


def f(
    models: list[Model],
    seq: Sequence[Model],
    it: "Iterable[Model]",
    gen: Generator[Model, None, None],
    mapping: Mapping[str, Model],
    pair: tuple[Model, Field],
    many: tuple[Model, ...],
    empty: tuple[()],
    nested: dict[str, list[Model]],
    counts: Counter[Model],
    view: ItemsView[str, Model],
    maybe: Optional[list[Model]],
    either: "list[Model] | None",
    deque: collections.deque[Model],
    frozen: AbstractSet[Model],
    bag: Bag,
    plain: Plain,
    *args: Model,
    **kwargs: Field,
):
    for a in models:
        pass
    for b in seq:
        pass
    for c in it:
        pass
    for d in gen:
        pass
    for key in mapping:
        pass
    first, second = pair
    third = pair[-1]
    fourth = pair[5]
    some = many[0]
    head = models[0]
    tail = models[1:]
    rest = pair[1:]
    value = mapping["k"]
    for inner in nested["k"]:
        pass
    for counted in counts:
        pass
    number = counts[Model()]
    for k2, viewed in view:
        pass
    for e in maybe:
        pass
    for g in either:
        pass
    for h in deque:
        pass
    for i in frozen:
        pass
    for j in args:
        pass
    for kw in kwargs.values():
        pass
    for walked in bag:
        pass
    picked = bag[0]
    for nothing in plain:
        pass
    absent = plain[0]
    for never in empty:
        pass
"#,
    );
    let t = types(&src, "f");
    for name in [
        "a", "b", "c", "d", "first", "some", "head", "value", "inner",
    ] {
        assert_eq!(of(&t, name), one("m.Model"), "{name}");
    }
    for name in ["counted", "viewed", "e", "g", "h", "i", "j", "picked"] {
        assert_eq!(of(&t, name), one("m.Model"), "{name}");
    }
    assert_eq!(of(&t, "key"), one("builtins.str"));
    assert_eq!(of(&t, "k2"), one("builtins.str"));
    assert_eq!(of(&t, "second"), one("m.Field"));
    assert_eq!(of(&t, "third"), one("m.Field"));
    assert_eq!(of(&t, "kw"), one("m.Field"));
    assert_eq!(of(&t, "walked"), one("m.Field"));
    assert_eq!(of(&t, "number"), one("builtins.int"));
    assert_eq!(of(&t, "tail"), one("builtins.list"));
    assert_eq!(of(&t, "rest"), one("builtins.tuple"));
    // Any of the items for a position past the end; nothing for what has no items.
    assert_eq!(of(&t, "fourth"), one("m.Field | m.Model"));
    assert!(of(&t, "nothing").is_empty());
    assert!(of(&t, "absent").is_empty());
    assert!(of(&t, "never").is_empty());
    // `*args: T` is a tuple of `T`, `**kwargs: T` a dict of them.
    assert_eq!(of(&t, "args"), one("builtins.tuple"));
    assert_eq!(of(&t, "kwargs"), one("builtins.dict"));
}

#[test]
fn container_methods_and_wrapping_builtins_keep_the_items() {
    let src = with_classes(
        r#"
import itertools
from itertools import chain


def f(models: list[Model], by_name: dict[str, Model], fields: list[Field], nested: list[list[Model]], pairs: list[tuple[str, Model]], unknown):
    for k, v in by_name.items():
        pass
    for v2 in by_name.values():
        pass
    for k3 in by_name.keys():
        pass
    got = by_name.get("k")
    got_none = by_name.get("k", None)
    got_other = by_name.get("k", Field())
    popped = by_name.pop("k")
    kept = by_name.setdefault("k", Model())
    item = by_name.popitem()
    last = models.pop()
    other = models.index(1)
    for i, e in enumerate(models):
        pass
    for z1, z2 in zip(models, fields):
        pass
    for r in reversed(models):
        pass
    for s in sorted(models):
        pass
    for it in iter(models):
        pass
    n = next(iter(models))
    n_none = next(iter(models), None)
    for l in list(models):
        pass
    for t in tuple(models):
        pass
    for st in set(models):
        pass
    for fr in frozenset(models):
        pass
    for flt in filter(None, models):
        pass
    for c in chain(models, models):
        pass
    for c2 in itertools.chain(models, fields):
        pass
    for c3 in chain.from_iterable(nested):
        pass
    for c4 in chain.from_iterable(unknown):
        pass
    for dk, dv in dict(by_name).items():
        pass
    for pk, pv in dict(pairs).items():
        pass
    nothing = dict(models)
    fresh = dict()
    tup = tuple()
    lst = list()
    called = iter(unknown, None)
    bare = next()
"#,
    );
    let t = types(&src, "f");
    for name in [
        "v", "v2", "got", "got_none", "popped", "kept", "last", "e", "z1", "r", "s", "it", "n",
        "n_none", "l", "t", "st", "fr", "flt", "c", "c3", "dv", "pv",
    ] {
        assert_eq!(of(&t, name), one("m.Model"), "{name}");
    }
    for name in ["k", "k3", "dk", "pk"] {
        assert_eq!(of(&t, name), one("builtins.str"), "{name}");
    }
    assert_eq!(of(&t, "i"), one("builtins.int"));
    assert_eq!(of(&t, "z2"), one("m.Field"));
    assert_eq!(of(&t, "item"), one("builtins.tuple"));
    // A default of another class or items of two classes give a union; unknown items, none.
    assert_eq!(of(&t, "got_other"), one("m.Field | m.Model"));
    assert_eq!(of(&t, "c2"), one("m.Field | m.Model"));
    assert!(of(&t, "c4").is_empty());
    assert!(of(&t, "other").is_empty());
    assert!(of(&t, "called").is_empty());
    assert!(of(&t, "bare").is_empty());
    for name in ["nothing", "fresh"] {
        assert_eq!(of(&t, name), one("builtins.dict"), "{name}");
    }
    assert_eq!(of(&t, "tup"), one("builtins.tuple"));
    assert_eq!(of(&t, "lst"), one("builtins.list"));
}

#[test]
fn displays_and_the_items_added_to_them() {
    let src = with_classes(
        r#"
def f(models: list[Model], by_name: dict[str, Model], m: Model, field: Field, unknown):
    for a in [m, m]:
        pass
    for b in (m, field):
        pass
    x, y = (m, field)
    for c in (m, *models):
        pass
    for d in {m}:
        pass
    for e in {"k": m}.values():
        pass
    for g in {**by_name}.values():
        pass
    for h in {**unknown}.values():
        pass
    for i in [*models]:
        pass
    out = []
    out.append(m)
    for j in out:
        pass
    alias = out
    alias.insert(0, m)
    more = []
    more.extend(models)
    for k in more:
        pass
    seen = set()
    added = {field}
    added.add(field)
    for l in added:
        pass
    grown = {m}
    grown.update(models)
    for n in grown:
        pass
    table = {}
    table["k"] = m
    for o in table.values():
        pass
    other = {}
    other.setdefault("k", m)
    for p in other.values():
        pass
    merged = {}
    merged.update(by_name)
    for q in merged.values():
        pass
    slots = [m]
    slots[0] = m
    for r in slots:
        pass
    for s in [m, None]:
        pass
    text = f"{m}"
    for t in ():
        pass
"#,
    );
    let t = types(&src, "f");
    for name in [
        "a", "c", "d", "e", "g", "i", "j", "k", "n", "o", "p", "q", "r", "s", "x",
    ] {
        assert_eq!(of(&t, name), one("m.Model"), "{name}");
    }
    assert_eq!(of(&t, "y"), one("m.Field"));
    assert_eq!(of(&t, "l"), one("m.Field"));
    assert_eq!(of(&t, "text"), one("builtins.str"));
    // Items of two classes are a union; an unknown mapping and an empty display give none.
    assert_eq!(of(&t, "b"), one("m.Field | m.Model"));
    assert!(of(&t, "h").is_empty());
    assert!(of(&t, "t").is_empty());
    assert_eq!(of(&t, "seen"), one("builtins.set"));
}

const HOLDERS: &str = r#"
from typing import Iterator

from m import Field, Model

REGISTRY: dict[str, Model] = {}
DEFAULTS = [Model()]


class Holder:
    annotated: list[Model]

    def __init__(self, models: list[Model]):
        self.copies = [Model() for _ in models]
        self.declared: dict[str, list[Field]] = {}
        self.plain = models


def make() -> list[Model]:
    return []


def walk() -> Iterator[Model]:
    yield Model()
"#;

#[test]
fn attributes_returns_and_globals_carry_their_items() {
    let user = r#"
from holders import DEFAULTS, REGISTRY, Holder, make, walk


def f(h: Holder):
    for a in h.annotated:
        pass
    for b in h.copies:
        pass
    for c in h.declared["k"]:
        pass
    for d in h.plain:
        pass
    for e in make():
        pass
    for g in walk():
        pass
    for i in REGISTRY.values():
        pass
    for j in DEFAULTS:
        pass
"#;
    let files = [("user", user), ("holders", HOLDERS), ("m", CLASSES)];
    let t = types_at(&files, PythonVersion::PY311, "f");
    for name in ["a", "b", "d", "e", "g", "i", "j"] {
        assert_eq!(of(&t, name), one("m.Model"), "{name}");
    }
    assert_eq!(of(&t, "c"), one("m.Field"));
}

const SCOPES: &str = r#"
from m import Field, Model


def f(models: list[Model], groups: list[list[Model]], by_name: dict[str, Model]):
    for a in [m for m in models if m]:
        pass
    for b in {m for m in models}:
        pass
    for c in {k: m for k, m in by_name.items()}.values():
        pass
    for d in (m for m in models):
        pass
    for e in [x for g in groups for x in g]:
        pass
    first = next(m for m in models)
    for g in (Field() for _ in models):
        pass
    nothing = [m for m in []]
"#;

#[test]
fn comprehension_scopes_iterate_what_the_enclosing_function_hands_them() {
    for version in [PythonVersion::PY311, PythonVersion::PY312] {
        let files = [("scopes", SCOPES), ("m", CLASSES)];
        let t = types_at(&files, version, "f");
        for name in ["a", "b", "c", "d", "e", "first"] {
            assert_eq!(of(&t, name), one("m.Model"), "{name} {version:?}");
        }
        assert_eq!(of(&t, "g"), one("m.Field"), "{version:?}");
        assert_eq!(of(&t, "nothing"), one("builtins.list"), "{version:?}");
        // The target inside the scope of its own (Python 3.11, generator expressions).
        let scope = if version == PythonVersion::PY311 {
            "f.<locals>.<listcomp>"
        } else {
            "f.<locals>.<genexpr>"
        };
        let inner = types_at(&files, version, scope);
        assert_eq!(of(&inner, "m"), one("m.Model"), "{version:?}");
    }
}

#[test]
fn declared_locals_keep_their_declaration_unless_a_subclass_is_assigned() {
    let src = with_classes(
        r#"
from typing import Any, Final, Literal, TypeAlias


def make():
    return [Model()]


def f(unknown) -> None:
    a: list[Model] = []
    a.append(Field())
    b: Model = Sub()
    c: Model = Field()
    d: Model = None
    e: Model = unknown
    g: Any = Model()
    h: object = Sub()
    i: Final = [Model()]
    j: TypeAlias = Model
    k: Literal["a"] = "a"
    for x in a:
        pass
"#,
    );
    let t = types(&src, "f");
    assert_eq!(of(&t, "x"), one("m.Model"));
    assert_eq!(of(&t, "b"), one("m.Sub"));
    assert_eq!(of(&t, "g"), one("m.Model"));
    assert_eq!(of(&t, "h"), one("m.Sub"));
    // A bare `Final` takes the value's type; `TypeAlias` and `Literal` declare no instance (the
    // alias holds the class object, the literal has its constant's class).
    assert_eq!(of(&t, "i"), one("builtins.list"));
    assert_eq!(of(&t, "j"), one("ref m.Model"));
    assert_eq!(of(&t, "k"), one("builtins.str"));
    assert_eq!(of(&t, "c"), one("m.Model"));
    assert_eq!(of(&t, "d"), one("m.Model"));
    assert_eq!(of(&t, "e"), one("m.Model"));
}

#[test]
fn phis_join_the_values_that_reach_them() {
    let src = with_classes(
        r#"
from typing import Optional


class Tree:
    parent: Optional["Tree"] = None


def f(flag, models: list[Model], by_name: dict[str, Model], tree: Tree):
    if flag:
        late = Model()
    late.fields = []
    node = models[0]
    while flag:
        node = node.fields
    current = tree
    while current is not None:
        up = current
        current = current.parent
    if flag and (found := by_name.get("a")):
        found.fields = []
    if flag or (other := by_name.get("b")) is None:
        pass
    else:
        other.fields = []
    unset = None
    return node, up, other, unset


x = Model()
y = x
"#,
    );
    let t = types(&src, "f");
    // A local assigned on some paths only has the type of the assignments.
    assert_eq!(of(&t, "late"), one("m.Model"));
    // A variable rebound in a loop to values of its own class keeps it; to another class
    // (`node = node.fields`), it has none.
    assert_eq!(of(&t, "up"), one("m.Tree"));
    assert!(of(&t, "current").iter().all(|c| c == "m.Tree"));
    assert!(!of(&t, "node").contains(&"builtins.list".to_owned()));
    // A walrus inside `and` / `or`.
    assert_eq!(of(&t, "found"), one("m.Model"));
    assert_eq!(of(&t, "other"), one("m.Model"));
    // `None` alone gives no type.
    assert!(of(&t, "unset").is_empty());
    let module = types(&src, "<module>");
    assert_eq!(of(&module, "y"), one("m.Model"));
}

#[test]
fn methods_type_self_and_cls() {
    let src = with_classes(
        r#"
class Owner:
    def method(self):
        it = self
        return it

    @classmethod
    def build(cls):
        made = cls
        return made

    @staticmethod
    def helper(value):
        same = value
        return same

    def sup(self):
        parent = super()
        return parent
"#,
    );
    assert_eq!(of(&types(&src, "Owner.method"), "it"), one("m.Owner"));
    assert_eq!(
        of(&types(&src, "Owner.build"), "made"),
        one("class m.Owner")
    );
    assert!(of(&types(&src, "Owner.helper"), "same").is_empty());
    assert_eq!(
        of(&types(&src, "Owner.sup"), "parent"),
        one("super m.Owner")
    );
}

#[test]
fn properties_give_their_getters_return_annotations() {
    let src = with_classes(
        r#"
import functools
from functools import cached_property


class Holder:
    @property
    def model(self) -> Model:
        return Model()

    @functools.cached_property
    def fields(self) -> list[Field]:
        return []

    @cached_property
    def cached(self) -> "Model":
        return Model()

    @property
    def settable(self) -> Model:
        return self._m

    @settable.setter
    def settable(self, value: Model) -> None:
        self._m = value

    @property
    def untyped(self):
        return Model()

    def plain(self) -> Model:
        return Model()


class Child(Holder):
    @property
    def model(self) -> Sub:
        return Sub()

    def use(self):
        up = super().model
        return up


class Other:
    def model(self) -> Model:
        return Model()


def f(h: Holder, c: Child, o: Other):
    a = h.model
    for x in h.fields:
        pass
    b = h.cached
    s = h.settable
    u = h.untyped
    p = h.plain
    cm = c.model
    inherited = c.cached
    bound = o.model
    called = o.model()
    value_called = h.model()
    return a, b, s, u, p, cm, inherited, bound, called, value_called
"#,
    );
    let t = types(&src, "f");
    for name in ["a", "b", "s", "inherited", "called"] {
        assert_eq!(of(&t, name), one("m.Model"), "{name}");
    }
    assert_eq!(of(&t, "x"), one("m.Field"));
    assert_eq!(of(&t, "cm"), one("m.Sub"));
    // A getter without a return annotation, a method read as an attribute, and a method of a
    // name some property has give no type.
    for name in ["u", "p", "bound", "value_called"] {
        assert!(of(&t, name).is_empty(), "{name}");
    }
    assert_eq!(of(&types(&src, "Child.use"), "up"), one("m.Model"));
}

#[test]
fn method_results_and_self_follow_the_definition_the_mro_finds() {
    let src = with_classes(
        r#"
from typing import Self


class Base:
    parent: Self | None = None
    children: list[Self]

    def clone(self) -> Self:
        return self

    def make(self) -> Model:
        return Model()

    @classmethod
    def create(cls) -> "Self":
        return cls()

    def merge(self, other: Self) -> None:
        o = other
        mine: Self = other
        return o, mine


class Derived(Base):
    def make(self) -> Sub:
        return Sub()


class Factory:
    def __call__(self) -> Model:
        return Model()


def make_self() -> Self:
    pass


def g(b: Base, d: Derived, fa: Factory):
    c1 = b.clone()
    c2 = d.clone()
    m1 = b.make()
    m2 = d.make()
    k1 = Base.create()
    k2 = Derived.create()
    pa = d.parent
    for ch in d.children:
        pass
    made = fa()
    lone = make_self()
    return c1, c2, m1, m2, k1, k2, pa, made, lone
"#,
    );
    let t = types(&src, "g");
    for name in ["c1", "k1"] {
        assert_eq!(of(&t, name), one("m.Base"), "{name}");
    }
    for name in ["c2", "k2", "pa", "ch"] {
        assert_eq!(of(&t, name), one("m.Derived"), "{name}");
    }
    // A type checker takes the type of `b.make()` from `Base.make`, whatever overrides it.
    assert_eq!(of(&t, "m1"), one("m.Model"));
    assert_eq!(of(&t, "m2"), one("m.Sub"));
    assert_eq!(of(&t, "made"), one("m.Model"));
    // `Self` outside a class names nothing.
    assert!(of(&t, "lone").is_empty());
    let merge = types(&src, "Base.merge");
    assert_eq!(of(&merge, "o"), one("m.Base"));
    assert_eq!(of(&merge, "mine"), one("m.Base"));
}

#[test]
fn copies_partials_and_casts_keep_or_name_the_type() {
    let src = with_classes(
        r#"
import copy
import dataclasses
import functools
import typing
from dataclasses import dataclass
from typing import TypeVar, cast

from pydantic import BaseModel

T = TypeVar("T")


@dataclass
class Rec:
    name: str


class PM(BaseModel):
    name: str


def make_model(n: int) -> Model:
    return Model()


def h(m: Model, r: Rec, pm: PM, x):
    c1 = copy.copy(m)
    c2 = copy.deepcopy(m)
    c3 = dataclasses.replace(r, name="b")
    c4 = pm.model_copy()
    c5 = copy.replace(r, name="c")
    part = functools.partial(make_model, 1)
    pr = part()
    pc = functools.partial(Model)()
    k1 = typing.cast("Model", x)
    k2 = cast(Model, x)
    k3 = cast("list[Field]", x)
    for kf in k3:
        pass
    k4 = cast("T", x)
    k5 = cast(x, x)
    none = copy.copy(x)
    kw = part.keywords
    return c1, c2, c3, c4, c5, pr, pc, k1, k2, k4, k5, none, kw
"#,
    );
    let t = types(&src, "h");
    for name in ["c1", "c2", "pr", "pc", "k1", "k2"] {
        assert_eq!(of(&t, name), one("m.Model"), "{name}");
    }
    for name in ["c3", "c5"] {
        assert_eq!(of(&t, name), one("m.Rec"), "{name}");
    }
    assert_eq!(of(&t, "c4"), one("m.PM"));
    assert_eq!(of(&t, "kf"), one("m.Field"));
    // A partial object is no value call resolution takes; a cast to a `TypeVar` or to an
    // unknown type, and a copy of an untyped value, give no type.
    for name in ["part", "k4", "k5", "none", "kw"] {
        assert!(of(&t, name).is_empty(), "{name}");
    }
}

const UNIONS: &str = r#"
from typing import Optional, Union


class A:
    name: Model

    def make(self) -> Field:
        return Field()


class B:
    name: Model

    def make(self) -> Field:
        return Field()


class C(A):
    pass


class Holder:
    def __init__(self, flag):
        self.part = A() if flag else B()


EITHER = A() if Holder else B()


def g(h: Holder):
    held = h.part
    either = EITHER
    return held, either


def f(
    u: A | B,
    o: Optional[Union[A, B]],
    many: A | B | Model | Field,
    crowd: A | B | C | Holder | Model | Field | Sub | Bag | Plain,
    items: list[A | B],
    flag,
    lone: Union[A, None],
    selfish: "A | Self",
):
    x5 = lone
    x6 = selfish
    x1 = u
    x2 = o
    x3 = many
    x7 = crowd
    for it in items:
        x4 = it
    pick = A() if flag else B()
    named = u.name
    made = u.make()
    if isinstance(u, A):
        n1 = u
    else:
        n2 = u
    if not isinstance(u, B):
        n3 = u
    if isinstance(u, A) and flag:
        n4 = u
    if flag and isinstance(u, A):
        n5 = u
    if not isinstance(u, A) or flag:
        pass
    else:
        n6 = u
    while isinstance(u, A):
        n7 = u
        break
    for v in items:
        if not isinstance(v, B):
            continue
        n8 = v
    if isinstance(u, (A, B)):
        n9 = u
    if isinstance(flag, C):
        n10 = flag
    only_a = [y for y in items if isinstance(y, A)]
    for z in only_a:
        n11 = z
    if isinstance(u, A) or flag:
        n12 = u
    if not not not not not not not not not not isinstance(u, A):
        deep = u
    if ok := isinstance(u, A):
        walrus = u
    if isinstance(u, (A, flag)):
        partly = u
    instance = A()
    if isinstance(u, instance):
        by_instance = u
    going = flag
    while going:
        if isinstance(u, B):
            looped = u
        going = flag.next()
    after = u
    return x1, x2, x3, x4, pick, named, made, n1, n2, n3, n4, n5, n6, n7, n8, n9, n10, n11
"#;

#[test]
fn unions_of_a_few_classes_and_isinstance_narrowing() {
    let src = with_classes(UNIONS);
    for version in [PythonVersion::PY311, PythonVersion::PY312] {
        let t = types_at(&[("m", src.as_str())], version, "f");
        for name in ["x1", "x2", "x4", "pick", "n9", "after"] {
            assert_eq!(of(&t, name), one("m.A | m.B"), "{name}");
        }
        // A declared union keeps up to eight classes; more give no type, nor does `Self`
        // outside a method.
        assert_eq!(of(&t, "x3"), one("m.A | m.B | m.Field | m.Model"));
        assert!(of(&t, "x7").is_empty());
        assert!(of(&t, "x6").is_empty());
        assert_eq!(of(&t, "x5"), one("m.A"));
        // A member of a union has the join of its classes' declarations.
        assert_eq!(of(&t, "named"), one("m.Model"));
        assert_eq!(of(&t, "made"), one("m.Field"));
        for name in ["n1", "n3", "n4", "n5", "n6", "n7", "n11"] {
            assert_eq!(of(&t, name), one("m.A"), "{name} ({version:?})");
        }
        for name in ["n2", "n8"] {
            assert_eq!(of(&t, name), one("m.B"), "{name}");
        }
        // A check of an untyped value names the class; one that may be false narrows nothing.
        assert_eq!(of(&t, "n10"), one("m.C"));
        assert_eq!(of(&t, "n12"), one("m.A | m.B"));
        // Nested too deep, a check of an unknown class or of an instance: no narrowing.
        for name in ["deep", "partly", "by_instance"] {
            assert_eq!(of(&t, name), one("m.A | m.B"), "{name}");
        }
        assert_eq!(of(&t, "walrus"), one("m.A"));
        assert_eq!(of(&t, "looped"), one("m.B"));
        // Attributes and globals keep a union of what they are assigned.
        let g = types_at(&[("m", src.as_str())], version, "g");
        assert_eq!(of(&g, "held"), one("m.A | m.B"));
        assert_eq!(of(&g, "either"), one("m.A | m.B"));
    }
}

const DISPATCH: &str = r#"
from typing import Protocol

from pydantic import BaseModel


class A:
    def run(self):
        pass

    @property
    def size(self) -> int:
        return 1


class B:
    def run(self):
        pass

    @staticmethod
    def only(x):
        pass

    @property
    def size(self) -> int:
        return 2


class C:
    def only(self):
        pass


class D:
    pass


class M1(BaseModel):
    pass


class M2(BaseModel):
    pass


class Wide:
    def go(self):
        pass


class Wide1(Wide):
    def go(self):
        pass


class Wide2(Wide):
    def go(self):
        pass


class Wide3(Wide):
    def go(self):
        pass


class Wide4(Wide):
    def go(self):
        pass


class Wide5(Wide):
    def go(self):
        pass


class Wide6(Wide):
    def go(self):
        pass


class Wide7(Wide):
    def go(self):
        pass


class Wide8(Wide):
    def go(self):
        pass


class Narrow:
    def go(self):
        pass


class Narrow1(Narrow):
    def go(self):
        pass


class Narrow2(Narrow):
    def go(self):
        pass


class Narrow3(Narrow):
    def go(self):
        pass


class Narrow4(Narrow):
    def go(self):
        pass


class Runner(Protocol):
    def run(self) -> None: ...


class Marker(Protocol):
    pass
"#;

#[test]
fn calls_on_unions_and_protocols_resolve_class_by_class() {
    with_cx(&[("m", DISPATCH)], |cx| {
        let union = |cs: &[&str]| Val::Union(cs.iter().map(|c| format!("m.{c}")).collect());
        let method = |v: &Val, m: &str| callee(&cx.resolve_method(Some(v), m));
        // Every class's targets together; a class without the method adds none.
        assert_eq!(
            method(&union(&["A", "B"]), "run"),
            "project true A.run,B.run"
        );
        assert_eq!(method(&union(&["A", "D"]), "run"), "project true A.run");
        assert_eq!(method(&union(&["C", "D"]), "nothing"), "unresolved");
        // A static method and a method (the call decides how arguments bind).
        assert_eq!(
            method(&union(&["B", "C"]), "only"),
            "project true B.only,C.only"
        );
        // The one external method two classes inherit; external and project targets, or two
        // external methods, leave it unresolved. A member the catalog does not list for the
        // base is not known to be there: unresolved.
        assert_eq!(
            method(&union(&["M1", "M2"]), "model_copy"),
            "external pydantic.BaseModel.model_copy"
        );
        assert_eq!(
            method(&union(&["M1", "M2"]), "_copy_model_type"),
            "unresolved"
        );
        assert_eq!(
            method(
                &Val::Union(vec!["builtins.str".into(), "builtins.list".into()]),
                "copy"
            ),
            "unresolved"
        );
        assert_eq!(method(&union(&["A", "M1"]), "run"), "unresolved");
        assert_eq!(
            method(
                &Val::Union(vec!["m.M1".into(), "builtins.str".into()]),
                "copy"
            ),
            "unresolved"
        );
        // More targets than the CHA limit: together, or for one class already.
        assert_eq!(
            method(&union(&["Narrow", "Narrow1"]), "go"),
            "project true Narrow.go,Narrow1.go,Narrow2.go,Narrow3.go,Narrow4.go"
        );
        assert_eq!(method(&union(&["A", "Wide"]), "go"), "too many");
        assert_eq!(
            method(
                &union(&["Narrow", "Wide1", "Wide2", "Wide3", "Wide4"]),
                "go"
            ),
            "too many"
        );
        // A protocol dispatches to the classes implementing it; one without members to none,
        // and `typing.Protocol` is not known to define the method.
        assert_eq!(
            method(&Val::Inst("m.Runner".into(), false), "run"),
            "project true A.run,B.run"
        );
        assert_eq!(
            method(&Val::Inst("m.Marker".into(), false), "run"),
            "unresolved"
        );
        // Calling a union value, reading a property on one.
        assert_eq!(
            callee(&cx.resolve_call(Some(&union(&["A", "B"])))),
            "unresolved"
        );
        assert_eq!(
            cx.resolve_attr_load(Some(&union(&["A", "B"])), "size")
                .map(|c| callee(&c)),
            Some("project true A.size,B.size".into())
        );
        assert_eq!(
            cx.resolve_attr_load(Some(&union(&["C", "D"])), "size"),
            None
        );
        assert_eq!(
            cx.resolve_attr_load(Some(&Val::Ref("m.A".into())), "size"),
            None
        );
        assert!(union(&["A", "B"]).recv().is_none());
    });
}

#[test]
fn imported_aliases_name_their_class_and_type_vars_nothing() {
    let m2 = r#"
from typing import TypeVar

Alias = list[int]
T = TypeVar("T")
"#;
    let m = r#"
from m2 import Alias, T


def f(a: Alias, t: "T", items: "list[Alias]"):
    x = a
    y = t
    for z in items:
        pass
"#;
    let t = types_at(&[("m", m), ("m2", m2)], PythonVersion::PY311, "f");
    // `Alias` is a type alias of `list[int]` bound once: an annotation naming it names a list.
    assert_eq!(of(&t, "x"), one("builtins.list"));
    assert_eq!(of(&t, "z"), one("builtins.list"));
    assert!(of(&t, "y").is_empty(), "y: {:?}", of(&t, "y"));
    assert_eq!(of(&t, "items"), one("builtins.list"));
}

#[test]
fn edges_of_items_calls_and_annotations() {
    let src = with_classes(
        r#"
import m as mod
from typing import Generic, TypeVar, Union

T = TypeVar("T")


class Box(Generic[T]):
    pass


def f(
    pair: tuple[Model, Field],
    triple: tuple[Model, Field, Model],
    plain: Plain,
    unknown,
    either: Union[Model, None],
    boxed: Box[Model],
    i: int,
):
    cls = mod.Model
    gen = (x.y for x in unknown)
    made = Model()
    called = made()
    nothing = None
    merged = {**nothing}
    from_none = dict(nothing)
    alias = list[Model]
    sliced = plain[1:]
    any_item = pair[i]
    first, *rest = triple
    from_unknown = dict(unknown)
    from_plain = dict(plain)
    classes = [Model]
    for c in classes:
        pass
    declared: Model = Field
    for e in [either]:
        pass
    box = boxed
"#,
    );
    let t = types(&src, "f");
    assert_eq!(of(&t, "cls"), one("ref m.Model"));
    assert!(of(&t, "gen").is_empty());
    assert!(of(&t, "called").is_empty());
    assert_eq!(of(&t, "merged"), one("builtins.dict"));
    assert_eq!(of(&t, "from_none"), one("builtins.dict"));
    // `list[Model]` is an alias of the class `list`: calling it constructs a list.
    assert_eq!(of(&t, "alias"), one("ref builtins.list"));
    assert!(of(&t, "sliced").is_empty());
    assert_eq!(of(&t, "any_item"), one("m.Field | m.Model"));
    // A starred target would shift the positions: no position is trusted, any item can be there.
    assert_eq!(of(&t, "first"), one("m.Field | m.Model"));
    assert_eq!(of(&t, "from_unknown"), one("builtins.dict"));
    assert_eq!(of(&t, "from_plain"), one("builtins.dict"));
    assert_eq!(of(&t, "c"), one("ref m.Model"));
    assert_eq!(of(&t, "declared"), one("m.Model"));
    assert_eq!(of(&t, "e"), one("m.Model"));
    assert_eq!(of(&t, "box"), one("m.Box"));
}
