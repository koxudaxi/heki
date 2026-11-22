//! `MUT118` (`sentinel-lost-in-deepcopy`) end to end: a `copy.deepcopy` of an instance holding an
//! `object()` sentinel in an attribute that a function tests against the sentinel by identity,
//! reported at the deep copy, and the code that keeps the sentinel or never stores it.

mod common;

use common::Project;
use serde_json::Value;

fn diagnostics(p: &Project, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["--output-format", "json", "--select", "MUT118"];
    all.extend_from_slice(args);
    let (code, stdout, stderr) = p.check(&all);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

/// `(path, line)` of every `MUT118` report.
fn reported(diags: &[Value]) -> Vec<(String, u64)> {
    diags
        .iter()
        .filter(|d| d["rule_code"] == "MUT118")
        .map(|d| {
            let path = d["path"].as_str().unwrap().replace('\\', "/");
            let file = path.rsplit('/').next().unwrap().to_owned();
            (file, d["line"].as_u64().unwrap())
        })
        .collect()
}

/// The repro of the datamodel-code-generator report: the positive and the code that keeps its
/// sentinel or never stores one on an instance.
const FIELDS: &str = r#"from __future__ import annotations

import copy
from dataclasses import dataclass, field

from typing_extensions import Sentinel

UNDEFINED = object()
MISSING = Sentinel("MISSING")
_UNREAD = object()


@dataclass
class Field:
    name: str
    default: object = UNDEFINED
    extras: dict[str, object] = field(default_factory=dict)


@dataclass
class Option:
    name: str
    value: object = MISSING


def has_default(item: Field) -> bool:
    return item.default is not UNDEFINED


def has_value(item: Option) -> bool:
    return item.value is not MISSING


def isolated(item: Field) -> Field:
    return copy.deepcopy(item)


def isolated_option(item: Option) -> Option:
    return copy.deepcopy(item)


def read_once(values: dict[str, object], key: str) -> object:
    if (value := values.get(key, _UNREAD)) is _UNREAD:
        return None
    return value


_NOTHING = object()


def lookup(values: dict[str, object], key: str, default: object = _NOTHING) -> object:
    if default is _NOTHING:
        return values[key]
    return values.get(key, default)


KEEP = object()


@dataclass
class Kept:
    value: object = KEEP

    def __deepcopy__(self, memo: dict[int, object]) -> Kept:
        return Kept(self.value if self.value is KEEP else copy.deepcopy(self.value, memo))


def is_kept(item: Kept) -> bool:
    return item.value is KEEP


def isolated_kept(item: Kept) -> Kept:
    return copy.deepcopy(item)
"#;

#[test]
fn the_deep_copy_of_a_dataclass_whose_default_is_a_tested_sentinel() {
    let p = Project::new("mut118_fields");
    p.write("pkg/__init__.py", "")
        .write("pkg/fields.py", FIELDS);
    let diags = diagnostics(&p, &[]);
    assert_eq!(reported(&diags), vec![("fields.py".to_owned(), 35)]);
    let d = &diags[0];
    assert_eq!(
        d["message"],
        "`copy.deepcopy` receives an instance of `Field`, whose `default` can hold the `object()` sentinel `UNDEFINED` that `has_default` tests by identity: the copy holds a new object there, which the test does not match"
    );
    let related = d["related"].as_array().unwrap();
    assert_eq!(related.len(), 1, "{related:?}");
    assert_eq!(related[0]["line"], 27);
    assert_eq!(
        related[0]["message"],
        "`has_default` tests `default` against `UNDEFINED` by identity"
    );
    // A heuristic rule: it runs when named or with `--heuristic`, not by default.
    let (_, stdout, _) = p.heki(&["check", "--output-format", "json"]);
    assert!(!stdout.contains("MUT118"), "{stdout}");
}

/// The sentinel, the class storing it, the test, and the deep copy, each in its own module; a
/// store in a method, a field default given through `dataclasses.field`, a constructed object,
/// `from copy import deepcopy`, a deep copy of `self`, an object typed by a subclass, and a
/// sentinel read through its module.
#[test]
fn stores_tests_and_copies_across_modules() {
    let p = Project::new("mut118_modules");
    p.write("pkg/__init__.py", "")
        .write("pkg/marks.py", "MISSING = object()\nOTHER = object()\n")
        .write(
            "pkg/nodes.py",
            r#"import copy
from dataclasses import dataclass, field

from pkg import marks
from pkg.marks import MISSING


class Node:
    def __init__(self):
        self.mark = MISSING

    def clone(self):
        return copy.deepcopy(self)


class Leaf(Node):
    pass


@dataclass
class Spec:
    name: str
    value: object = field(default=MISSING)


class Tagged:
    def __init__(self):
        self.tag = marks.MISSING
        self.flag = marks.OTHER
"#,
        )
        .write(
            "pkg/checks.py",
            r#"from pkg import marks
from pkg.marks import MISSING
from pkg.nodes import Node, Spec


def marked(node: Node) -> bool:
    return MISSING is node.mark


def given(spec: Spec) -> bool:
    return spec.value is not marks.MISSING


def tagged(t: "Tagged") -> bool:
    tag = t.tag
    return tag is marks.MISSING or t.flag is marks.OTHER


from pkg.nodes import Tagged  # noqa: E402
"#,
        )
        .write(
            "pkg/copies.py",
            r#"import copy
from copy import deepcopy

from pkg.nodes import Leaf, Spec, Tagged


def copy_leaf(leaf: Leaf):
    return copy.deepcopy(leaf)


def copy_spec():
    return deepcopy(Spec("a"))


def copy_tagged(t: Tagged):
    return deepcopy(t)


def defaults():
    class Defaults:
        SPEC = deepcopy(Spec("default"))

    return Defaults
"#,
        );
    let diags = diagnostics(&p, &[]);
    assert_eq!(
        reported(&diags),
        vec![
            ("copies.py".to_owned(), 8),
            ("copies.py".to_owned(), 12),
            ("copies.py".to_owned(), 16),
            ("copies.py".to_owned(), 21),
            ("nodes.py".to_owned(), 13),
        ]
    );
    let related = |i: usize| {
        let r = &diags[i]["related"][0];
        let path = r["path"].as_str().unwrap().replace('\\', "/");
        (
            path.rsplit('/').next().unwrap().to_owned(),
            r["line"].as_u64().unwrap(),
        )
    };
    assert_eq!(related(0), ("checks.py".to_owned(), 7));
    assert_eq!(related(1), ("checks.py".to_owned(), 11));
}

/// What `copy.deepcopy` does is decided by the class: a hook keeps any value, so does a
/// `copyreg` registration; a `memo` may map the sentinel to itself.
#[test]
fn a_copy_hook_a_registration_or_a_memo_decides_the_copy() {
    let p = Project::new("mut118_hooks");
    p.write("pkg/__init__.py", "").write(
        "pkg/hooks.py",
        r#"import copy
import copyreg

MISSING = object()


class Reduced:
    def __init__(self):
        self.mark = MISSING

    def __reduce__(self):
        return (Reduced, ())


class StateKept:
    def __init__(self):
        self.mark = MISSING

    def __setstate__(self, state):
        self.__dict__.update(state, mark=MISSING)


class Base:
    def __getstate__(self):
        return {}


class Derived(Base):
    def __init__(self):
        self.mark = MISSING


class Registered:
    def __init__(self):
        self.mark = MISSING


copyreg.pickle(Registered, lambda r: (Registered, ()))


class Dispatched:
    def __init__(self):
        self.mark = MISSING


copyreg.dispatch_table[Dispatched] = lambda r: (Dispatched, ())


class Control:
    def __init__(self):
        self.mark = MISSING


def marked(a: Reduced, b: StateKept, c: Derived, d: Registered, e: Control) -> bool:
    return (
        a.mark is MISSING
        or b.mark is MISSING
        or c.mark is MISSING
        or d.mark is MISSING
        or e.mark is MISSING
    )


def dispatched(f: Dispatched) -> bool:
    return f.mark is MISSING


def copies(
    a: Reduced, b: StateKept, c: Derived, d: Registered, e: Control, f: Dispatched, memo: dict
):
    return (
        copy.deepcopy(a),
        copy.deepcopy(b),
        copy.deepcopy(c),
        copy.deepcopy(d),
        copy.deepcopy(e, memo),
        copy.deepcopy(e),
        copy.deepcopy(f),
    )
"#,
    );
    assert_eq!(
        reported(&diagnostics(&p, &[])),
        vec![("hooks.py".to_owned(), 77)]
    );
    // A registration of a class heki cannot name may be of any class.
    p.write(
        "pkg/anyclass.py",
        "import copyreg\n\n\ndef register(obj, reduce):\n    copyreg.pickle(type(obj), reduce)\n",
    );
    assert_eq!(reported(&diagnostics(&p, &[])), Vec::new());
}

/// A sentinel the class never stores on an instance (a class attribute, a field `__init__` does
/// not take, a `ClassVar`), a name bound twice or rebound by another module, a test with `==`,
/// a test of an attribute of an unrelated class, and one on an object of no known class.
#[test]
fn values_that_are_no_sentinel_stored_on_the_instance() {
    let p = Project::new("mut118_negatives");
    p.write("pkg/__init__.py", "")
        .write(
            "pkg/marks.py",
            "MISSING = object()\nTWICE = object()\nTWICE = object()\nREBOUND = object()\n",
        )
        .write(
            "pkg/rebind.py",
            "from pkg import marks\n\nmarks.REBOUND = 1\n",
        )
        .write(
            "pkg/models.py",
            r#"import copy
from dataclasses import dataclass, field
from typing import ClassVar

from pkg.marks import MISSING, REBOUND, TWICE


class Plain:
    mark: object = MISSING
    extra = dict()


class Deleted:
    mark = MISSING
    del mark


class NotData:
    mark: object = field(default=MISSING)


@dataclass
class NotTaken:
    mark: object = field(default=MISSING, init=False)
    kind: ClassVar[object] = MISSING


@dataclass(init=False)
class NoInit:
    mark: object = MISSING


class Twice:
    def __init__(self):
        self.mark = TWICE
        self.other = REBOUND


class Compared:
    def __init__(self):
        self.mark = MISSING


class Unrelated:
    def __init__(self):
        self.mark = None


class Control:
    def __init__(self):
        self.mark = MISSING


def tests(
    p: Plain, n: NotTaken, i: NoInit, t: Twice, c: Compared, u: Unrelated, k: Control
) -> bool:
    return (
        p.mark is MISSING
        or n.mark is MISSING
        or n.kind is MISSING
        or i.mark is MISSING
        or t.mark is TWICE
        or t.other is REBOUND
        or c.mark == MISSING
        or u.mark is MISSING
        or k.mark is MISSING
    )


def untyped(x) -> bool:
    return x.mark is MISSING


def copies(p: Plain, n: NotTaken, i: NoInit, t: Twice, c: Compared, k: Control):
    return (
        copy.deepcopy(p),
        copy.deepcopy(n),
        copy.deepcopy(i),
        copy.deepcopy(t),
        copy.deepcopy(c),
        copy.deepcopy(k),
    )
"#,
        );
    let diags = diagnostics(&p, &[]);
    assert_eq!(
        reported(&diags),
        vec![("models.py".to_owned(), 81)],
        "{diags:#?}"
    );
}

/// Open premises: an object that may be of a subclass holding the sentinel, a subclass that
/// may decide the copy, a base outside the project, and a model the catalog describes but
/// whose reads it does not.
#[test]
fn open_premises_give_heuristic_reports() {
    let p = Project::new("mut118_open");
    p.write("pkg/__init__.py", "").write(
        "pkg/open_.py",
        r#"import copy

from pydantic import BaseModel
from somelib import Base

MISSING = object()


class Parent:
    pass


class Child(Parent):
    def __init__(self):
        self.mark = MISSING


class Node:
    def __init__(self):
        self.mark = MISSING


class Shared(Node):
    def __deepcopy__(self, memo):
        return self


class Model(BaseModel):
    mark: object = MISSING


class Lib(Base):
    def __init__(self):
        self.mark = MISSING


def marked(c: Child, n: Node, m: Model, x: Lib) -> bool:
    return c.mark is MISSING or n.mark is MISSING or m.mark is MISSING or x.mark is MISSING


def copy_parent(p: Parent):
    return copy.deepcopy(p)


def copy_node(n: Node):
    return copy.deepcopy(n)


def copy_model(m: Model):
    return copy.deepcopy(m)


def copy_lib(x: Lib):
    return copy.deepcopy(x)
"#,
    );
    let diags = diagnostics(&p, &[]);
    assert_eq!(
        reported(&diags),
        vec![
            ("open_.py".to_owned(), 42),
            ("open_.py".to_owned(), 46),
            ("open_.py".to_owned(), 50),
            ("open_.py".to_owned(), 54),
        ]
    );
    assert!(
        diags[0]["message"]
            .as_str()
            .unwrap()
            .starts_with("`copy.deepcopy` may receive an instance of `Child`,"),
        "{}",
        diags[0]["message"]
    );
    for d in &diags {
        assert_eq!(d["certainty"], "heuristic", "{d}");
    }
}

/// The sentinel idiom of a parameter default stored on the instance (`def __init__(self,
/// default=MISSING): self.default = default`): after literal defaults and among keyword-only
/// parameters; a parameter rebound before the store holds something else.
#[test]
fn a_parameter_default_stored_on_the_instance() {
    let p = Project::new("mut118_parameter_defaults");
    p.write("pkg/__init__.py", "").write(
        "pkg/models.py",
        r#"import copy
from typing import Any

UNDEFINED: Any = object()


class DataModel:
    def __init__(self, name, size=1, *, default: Any = UNDEFINED, other=None):
        self.name = name
        self.default: Any = default
        self.other = other

    def has_default(self) -> bool:
        return self.default is not UNDEFINED


class Reassigned:
    def __init__(self, default=UNDEFINED):
        default = default or 1
        self.default = default


def given(r: Reassigned) -> bool:
    return r.default is UNDEFINED


def copied(m: DataModel):
    return copy.deepcopy(m)


def copied_reassigned(r: Reassigned):
    return copy.deepcopy(r)
"#,
    );
    assert_eq!(
        reported(&diagnostics(&p, &[])),
        vec![("models.py".to_owned(), 28)]
    );
}

/// An instance held below the copied object's attributes, found by their types (an optional
/// attribute, the items of a list, the implementations of a protocol): datamodel-code-generator's
/// own shape, where `copy.deepcopy(field.data_type)` copied the models its references reach. What
/// the attributes hold at run time may be other objects, so the reports are heuristic. A class
/// with a copy hook or a `copyreg` registration on the way leads nowhere (it copies its instances
/// itself), through a tuple or a union too, and so do attributes holding a class object, a
/// callable, `Self`, or a dict of values of no known class (`Any`, or a name heki cannot
/// resolve).
#[test]
fn an_instance_below_the_copied_objects_attributes() {
    let p = Project::new("mut118_below");
    p.write("pkg/__init__.py", "")
        .write(
            "pkg/base.py",
            r#"import copyreg
from typing import Any, Callable, Optional, Protocol, Self

from pydantic import BaseModel

UNDEFINED: Any = object()


class ReferenceSource(Protocol):
    name: str


class Reference(BaseModel):
    path: str
    source: Optional[ReferenceSource] = None


class DataType(BaseModel):
    type: Optional[str] = None
    reference: Optional[Reference] = None
    data_types: list["DataType"] = []


class DataModel:
    def __init__(self, name: str, default: Any = UNDEFINED) -> None:
        self.name = name
        self.default: Any = default

    def has_default(self) -> bool:
        return self.default is not UNDEFINED


class Field(BaseModel):
    name: str
    data_type: DataType


class Shared:
    def __init__(self, model: DataModel) -> None:
        self.model = model

    def __deepcopy__(self, memo):
        return self


class Wrapper:
    def __init__(self, shared: Shared, label: str) -> None:
        self.shared = shared
        self.label = label


class Registered:
    def __init__(self, model: DataModel) -> None:
        self.model = model


copyreg.pickle(Registered, lambda r: (Registered, (r.model,)))


class Kept:
    def __init__(self, registered: Registered) -> None:
        self.registered = registered


class Beta:
    def __init__(self) -> None:
        self.flag = UNDEFINED


class Alpha:
    def __init__(self, beta: Beta) -> None:
        self.beta = beta
        self.flag = UNDEFINED


def flagged(a: Alpha, b: Beta) -> bool:
    return a.flag is UNDEFINED or b.flag is UNDEFINED


class Pair:
    def __init__(self, alpha: Alpha) -> None:
        self.alpha = alpha


class Bundle:
    factory: type[DataModel]
    check: Callable[[DataModel], bool]
    parent: Self
    options: dict[str, Any]
    unknown: dict[str, "NoSuchClass"]

    def __init__(
        self, pair: tuple[Shared, int], either: Shared | int, kind: type[DataModel]
    ) -> None:
        self.pair = pair
        self.either = either
        self.kind = kind
"#,
        )
        .write(
            "pkg/custom.py",
            r#"import copy

from pkg.base import Bundle, Field, Kept, Pair, Wrapper


def isolated_type(field: Field):
    return copy.deepcopy(field.data_type)


def isolated_wrapper(wrapper: Wrapper):
    return copy.deepcopy(wrapper)


def isolated_bundle(bundle: Bundle):
    return copy.deepcopy(bundle)


def isolated_kept(kept: Kept):
    return copy.deepcopy(kept)


def isolated_pair(pair: Pair):
    return copy.deepcopy(pair)
"#,
        );
    let diags = diagnostics(&p, &[]);
    assert_eq!(
        reported(&diags),
        vec![("custom.py".to_owned(), 7), ("custom.py".to_owned(), 23)]
    );
    // Of two holders below `alpha`, the report names the first by name.
    assert!(
        diags[1]["message"].as_str().unwrap().starts_with(
            "`copy.deepcopy` may copy an instance of `Alpha` held in `alpha` or in what it holds,"
        ),
        "{}",
        diags[1]["message"]
    );
    assert_eq!(
        diags[0]["message"],
        "`copy.deepcopy` may copy an instance of `DataModel` held in `reference` or in what it holds, whose `default` can hold the `object()` sentinel `UNDEFINED` that `DataModel.has_default` tests by identity: the copy holds a new object there, which the test does not match"
    );
    assert_eq!(diags[0]["certainty"], "heuristic");
}
