//! Contract extensions end to end through the real binary: constant keys and container entries
//! (`Final` string constants in `obj.__dict__[KEY]`, `getattr`, `setattr`; item targets
//! `Class.attr["key"]`; writes into the entries of a guarded container), the `fresh-objects`
//! policy (objects under construction), and the `unresolved` strict mode (`MUT205`).

mod common;

use common::Project;
use serde_json::Value;

fn diagnostics(p: &Project, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["--output-format", "json"];
    all.extend_from_slice(args);
    let (code, stdout, stderr) = p.check(&all);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

/// `(path, line, code, qualname, target)` of every diagnostic.
fn found(diags: &[Value]) -> Vec<(String, u64, String, String, String)> {
    diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
                d["qualname"].as_str().unwrap().to_owned(),
                d["target"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn d(
    path: &str,
    line: u64,
    code: &str,
    qualname: &str,
    target: &str,
) -> (String, u64, String, String, String) {
    (
        path.into(),
        line,
        code.into(),
        qualname.into(),
        target.into(),
    )
}

fn message_at<'d>(diags: &'d [Value], path: &str, line: u64) -> &'d str {
    diags
        .iter()
        .find(|x| x["path"] == path && x["line"] == line)
        .unwrap_or_else(|| panic!("no diagnostic at {path}:{line}: {diags:#?}"))["message"]
        .as_str()
        .unwrap()
}

// ================================================================== constant keys and entries

const MODEL: &str = r#"class DataModelFieldBase:
    def __init__(self) -> None:
        self.extras: dict[str, object] = {}
        self.name = ""


class DataType:
    def __init__(self) -> None:
        self.data_types: list["DataType"] = []
        self.kwargs: dict[str, dict[str, str]] = {"kw": {}}


class DataModel:
    def __init__(self) -> None:
        self.fields: list[DataModelFieldBase] = []
        self.meta: dict[str, dict[str, str]] = {"kw": {}}
        self.notes: dict[str, dict[str, str]] = {"kw": {}}
"#;

const KEYS: &str = r#"from typing import Final

FORCED: Final = "_forced_field_assignment"
SECRET: Final[str] = "_secret"
ALIAS: Final = FORCED
INIT: Final = "init"
PLAIN = "_forced_field_assignment"
REBOUND: Final = "_forced_field_assignment"


def rebind() -> None:
    global REBOUND
    REBOUND = "other"
"#;

const KEYS_INIT: &str = "from pkg.keys import FORCED\n";

const USE: &str = r#"from typing import Final

import pkg
from pkg import keys
from pkg.keys import ALIAS, FORCED, INIT, PLAIN, REBOUND, SECRET
from pkg.model import DataModel, DataModelFieldBase, DataType

_KEY: Final = "_forced_field_assignment"


def force(field: DataModelFieldBase) -> None:
    field.__dict__[_KEY] = True
    field.extras["init"] = False


def imported(field: DataModelFieldBase) -> None:
    field.__dict__[FORCED] = True
    setattr(field, keys.FORCED, True)
    field.__dict__.pop(ALIAS, None)
    field.__dict__[pkg.FORCED] = True


def not_constants(field: DataModelFieldBase, name: str) -> None:
    field.__dict__[PLAIN] = True
    field.__dict__[REBOUND] = True
    setattr(field, name, True)


def entries(field: DataModelFieldBase, name: str) -> None:
    field.extras[INIT] = 1
    field.extras.pop("init")
    del field.extras["init"]
    field.extras["other"] = 1
    field.extras[name] = 1
    field.extras.update({})
    field.extras = {}


def nested(model: DataModel, dt: DataType, name: str) -> None:
    model.meta["kw"][name] = "v"
    model.meta["kw"].update({})
    alias = model.meta["kw"]
    alias.clear()
    model.notes["kw"][name] = "v"
    dt.kwargs["kw"][name] = "v"
    model.meta.copy()["kw"] = {}
    model.meta["kw"].copy()["x"] = "v"


def reads(field: DataModelFieldBase) -> object:
    return getattr(field, SECRET), field.__dict__.get(SECRET), field.__dict__[SECRET]


def read_only(model: DataModel) -> int:
    return len(model.fields)
"#;

const C2_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT201", "MUT203"]

[[tool.heki.write-owners]]
name = "store"
targets = [
    "DataModelFieldBase._forced_field_assignment",
    'DataModelFieldBase.extras["init"]',
    "DataModel.meta",
    "DataModel.fields",
    "DataType.data_types",
]
allowed-writers = ["pkg.store.*"]

[[tool.heki.read-owners]]
name = "secrets"
targets = ["DataModelFieldBase._secret"]
"#;

/// Rebinds `keys.PLAIN`, which is then no constant.
const REBIND: &str = r#"from pkg import keys


def reset() -> None:
    keys.PLAIN = "_other"
"#;

fn keys_project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("pyproject.toml", C2_CONFIG)
        .write("pkg/__init__.py", KEYS_INIT)
        .write("pkg/model.py", MODEL)
        .write("pkg/keys.py", KEYS)
        .write("pkg/rebind.py", REBIND)
        .write("pkg/use.py", USE);
    p
}

#[test]
fn final_constant_keys_name_attributes_and_entries() {
    let p = keys_project("keys_targets");
    let diags = diagnostics(&p, &[]);
    let u = "pkg/use.py";
    let forced = "DataModelFieldBase._forced_field_assignment";
    let init = "DataModelFieldBase.extras[\"init\"]";
    let secret = "DataModelFieldBase._secret";
    assert_eq!(
        found(&diags),
        vec![
            // A module constant of the writing module, a literal item key.
            d(u, 12, "MUT201", "force", forced),
            d(u, 13, "MUT201", "force", init),
            // Imported, through a module attribute, a `Final` alias of another constant, and a
            // re-export of the package.
            d(u, 17, "MUT201", "imported", forced),
            d(u, 18, "MUT201", "imported", forced),
            d(u, 19, "MUT201", "imported", forced),
            d(u, 20, "MUT201", "imported", forced),
            // `field.extras[INIT]`, `pop("init")`, `del ...["init"]`: the entry; another key, a
            // computed key, `update`, and rebinding the container are no write of it.
            d(u, 30, "MUT201", "entries", init),
            d(u, 31, "MUT201", "entries", init),
            d(u, 32, "MUT201", "entries", init),
            // An entry of an entry of a guarded container, also through an alias; the entries of
            // undeclared containers and of copies are not.
            d(u, 40, "MUT201", "nested", "DataModel.meta"),
            d(u, 41, "MUT201", "nested", "DataModel.meta"),
            d(u, 43, "MUT201", "nested", "DataModel.meta"),
            // Reads through `getattr`, `__dict__.get`, and `__dict__[...]` with a constant.
            d(u, 51, "MUT203", "reads", secret),
            d(u, 51, "MUT203", "reads", secret),
            d(u, 51, "MUT203", "reads", secret),
        ]
    );
    assert!(
        message_at(&diags, u, 40).contains("writes `DataModel.meta` (in place inside an item)"),
        "{diags:#?}"
    );
    assert!(
        message_at(&diags, u, 12).contains("(as a `__dict__` item)"),
        "{diags:#?}"
    );
}

#[test]
fn an_entry_target_wins_over_its_container_and_reaches_effect_boundaries() {
    let p = keys_project("keys_entry_wins");
    p.write(
        "pyproject.toml",
        r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT201", "EFF201"]

[[tool.heki.write-owners]]
name = "extras"
targets = ["DataModelFieldBase.extras", 'DataModelFieldBase.extras["init"]']

[[tool.heki.effect-boundaries]]
name = "pure"
scopes = ["pkg.use.entries"]
forbid = ["writes-targets"]
write-targets = ['DataModelFieldBase.extras["init"]']

[[tool.heki.effect-boundaries]]
name = "pass"
scopes = ["pkg.use.entries"]
allow-writes = ['DataModelFieldBase.extras["init"]']
write-universe = ["DataModelFieldBase.*"]
"#,
    );
    let diags = diagnostics(&p, &[]);
    let u = "pkg/use.py";
    let (init, extras) = (
        "DataModelFieldBase.extras[\"init\"]",
        "DataModelFieldBase.extras",
    );
    let entries: Vec<_> = found(&diags)
        .into_iter()
        .filter(|x| x.3 == "entries")
        .collect();
    assert_eq!(
        entries,
        vec![
            // The boundary forbidding the entry: one report for its three writes; the pass that
            // allows the entry reports the other writes of the class, once per attribute.
            d(
                u,
                30,
                "EFF201",
                "entries",
                "pure:DataModelFieldBase.extras[\"init\"]"
            ),
            d(u, 30, "MUT201", "entries", init),
            d(u, 31, "MUT201", "entries", init),
            d(u, 32, "MUT201", "entries", init),
            d(u, 33, "MUT201", "entries", extras),
            d(u, 34, "MUT201", "entries", extras),
            d(u, 35, "MUT201", "entries", extras),
            d(u, 36, "EFF201", "entries", "pass:DataModelFieldBase.extras"),
            d(u, 36, "MUT201", "entries", extras),
        ]
    );
    let pure = diags
        .iter()
        .find(|x| x["target"] == "pure:DataModelFieldBase.extras[\"init\"]")
        .unwrap();
    assert_eq!(pure["sites"].as_u64(), Some(3));
}

// ======================================================= constants without `Final`

const ONCE_KEYS: &str = r#"ONCE = "_forced_field_assignment"
ALIASED = ONCE
TWICE = "_forced_field_assignment"
TWICE = "_forced_field_assignment"
MOVED = "_forced_field_assignment"

from pkg.model import DataModelFieldBase


def write_moved(field: DataModelFieldBase) -> None:
    field.__dict__[MOVED] = True
"#;

const ONCE_USE: &str = r#"from typing import ClassVar, Final, Protocol

from pkg import once
from pkg.model import DataModelFieldBase
from pkg.once import ALIASED, MOVED, ONCE, TWICE

LOCAL = "_forced_field_assignment"


def module_constants(field: DataModelFieldBase) -> None:
    field.__dict__[ONCE] = True
    field.__dict__[ALIASED] = True
    field.__dict__[once.ONCE] = True
    field.__dict__[LOCAL] = True
    field.__dict__[TWICE] = True
    field.__dict__[MOVED] = True


def move() -> None:
    once.MOVED = "_other"


class Keyed:
    KEY: ClassVar[str] = "_forced_field_assignment"
    FINAL_KEY: Final = "_forced_field_assignment"
    PLAIN_KEY = "_forced_field_assignment"
    TWICE_KEY: ClassVar[str] = "_forced_field_assignment"
    TWICE_KEY = "_forced_field_assignment"

    def write(self, field: DataModelFieldBase) -> None:
        field.__dict__[self.KEY] = True
        field.__dict__[self.FINAL_KEY] = True
        field.__dict__[Keyed.KEY] = True
        field.__dict__[self.PLAIN_KEY] = True
        field.__dict__[self.TWICE_KEY] = True

    @classmethod
    def write_on_class(cls, field: DataModelFieldBase) -> None:
        field.__dict__[cls.KEY] = True


class Child(Keyed):
    def write_inherited(self, field: DataModelFieldBase) -> None:
        field.__dict__[self.KEY] = True
        field.__dict__[Child.KEY] = True


class Overridden:
    KEY: ClassVar[str] = "_forced_field_assignment"

    def write(self, field: DataModelFieldBase) -> None:
        field.__dict__[self.KEY] = True
        field.__dict__[Overridden.KEY] = True


class Override(Overridden):
    KEY: ClassVar[str] = "_other"


class Deleted:
    KEY: ClassVar[str] = "_forced_field_assignment"
    del KEY

    def write(self, field: DataModelFieldBase) -> None:
        field.__dict__[Deleted.KEY] = True


class Configured:
    KEY: ClassVar[str] = "_forced_field_assignment"

    def write(self, field: DataModelFieldBase) -> None:
        field.__dict__[self.KEY] = True


class Reconfigured:
    KEY: ClassVar[str] = "_forced_field_assignment"

    @classmethod
    def configure(cls) -> None:
        cls.KEY = "_other"

    def write(self, field: DataModelFieldBase) -> None:
        field.__dict__[self.KEY] = True


class Switched:
    KEY: ClassVar[str] = "_forced_field_assignment"

    def write(self, field: DataModelFieldBase) -> None:
        field.__dict__[self.KEY] = True


def switch() -> None:
    Switched.KEY = "_other"


class Retitled:
    KEY: ClassVar[str] = "_forced_field_assignment"

    def write(self, field: DataModelFieldBase) -> None:
        field.__dict__[self.KEY] = True


Retitled.KEY = "_other"


class Base:
    KEY: ClassVar[str] = "_forced_field_assignment"

    def write(self, field: DataModelFieldBase) -> None:
        field.__dict__[self.KEY] = True


class Inheriting(Base):
    def write_own(self, field: DataModelFieldBase) -> None:
        field.__dict__[Inheriting.KEY] = True


class ForeignInheriting(Base):
    def write_own(self, field: DataModelFieldBase) -> None:
        field.__dict__[ForeignInheriting.KEY] = True


class Untouched(Base):
    def write_own(self, field: DataModelFieldBase) -> None:
        field.__dict__[Untouched.KEY] = True


Inheriting.KEY = "_other"


class KeyProto(Protocol):
    KEY: ClassVar[str] = "_forced_field_assignment"

    def label(self) -> str: ...


class SecretKeys:
    KEY: ClassVar[str] = "_other"

    def label(self) -> str:
        return "secret"


def write_through_protocol(keys: KeyProto, field: DataModelFieldBase) -> None:
    field.__dict__[keys.KEY] = True


class SameProto(Protocol):
    KEY: ClassVar[str] = "_forced_field_assignment"

    def tag(self) -> str: ...


class SameKeys:
    KEY: ClassVar[str] = "_forced_field_assignment"

    def tag(self) -> str:
        return "same"


def write_through_same(keys: SameProto, field: DataModelFieldBase) -> None:
    field.__dict__[keys.KEY] = True


class Aliased:
    KEY: ClassVar[str] = "_forced_field_assignment"
    LABEL: ClassVar[str] = "_forced_field_assignment"

    def write(self, field: DataModelFieldBase) -> None:
        field.__dict__[self.KEY] = True


def realias() -> None:
    keys = Aliased
    same = keys
    same.LABEL = "_other"
    same.KEY = "_other"


def via_builtin(field: DataModelFieldBase, keys: dict) -> None:
    field.__dict__[keys.KEY] = True
"#;

#[test]
fn constants_bound_once_and_class_constants_name_attributes() {
    let p = Project::new("keys_bound_once");
    p.write(
        "pyproject.toml",
        r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT201", "MUT205"]

[[tool.heki.write-owners]]
name = "store"
targets = ["DataModelFieldBase._forced_field_assignment"]
allowed-writers = ["pkg.store.*"]
unresolved = "report"
"#,
    )
    .write("pkg/__init__.py", "")
    .write("pkg/model.py", MODEL)
    .write("pkg/once.py", ONCE_KEYS)
    // Handing a constant to a call is no rebinding of it.
    .write(
        "pkg/reader.py",
        "from pkg.once import ONCE\n\n\ndef drop(d: dict) -> None:\n    d.pop(ONCE, None)\n",
    )
    // Another module rebinds a `ClassVar` constant on its class, and a `Final` one.
    .write(
        "pkg/settings.py",
        "from pkg.use import Configured, ForeignInheriting, Keyed\n\n\ndef configure() -> None:\n    Configured.KEY = \"_other\"\n    Keyed.FINAL_KEY = \"_other\"\n    ForeignInheriting.KEY = \"_other\"\n",
    )
    .write("pkg/use.py", ONCE_USE);
    let u = "pkg/use.py";
    let forced = "DataModelFieldBase._forced_field_assignment";
    let class = "DataModelFieldBase";
    assert_eq!(
        found(&diagnostics(&p, &[])),
        vec![
            // Rebound by another module: the name is computed in its own module too.
            d("pkg/once.py", 11, "MUT205", "write_moved", class),
            // A module-level name bound once, also through another such name, a module
            // attribute, and in the module itself.
            d(u, 11, "MUT201", "module_constants", forced),
            d(u, 12, "MUT201", "module_constants", forced),
            d(u, 13, "MUT201", "module_constants", forced),
            d(u, 14, "MUT201", "module_constants", forced),
            // Bound twice, or rebound by another module: the name is computed.
            d(u, 15, "MUT205", "module_constants", class),
            d(u, 16, "MUT205", "module_constants", class),
            // `ClassVar` and `Final` class constants on `self`, on the class, and on `cls`; a
            // plain class attribute (an instance may shadow it) and one bound twice are not.
            d(u, 31, "MUT201", "Keyed.write", forced),
            d(u, 32, "MUT201", "Keyed.write", forced),
            d(u, 33, "MUT201", "Keyed.write", forced),
            d(u, 34, "MUT205", "Keyed.write", class),
            d(u, 35, "MUT205", "Keyed.write", class),
            d(u, 39, "MUT201", "Keyed.write_on_class", forced),
            // Inherited along the MRO.
            d(u, 44, "MUT201", "Child.write_inherited", forced),
            d(u, 45, "MUT201", "Child.write_inherited", forced),
            // A subclass defines the name again: `self` may be one, the class itself is not.
            d(u, 52, "MUT205", "Overridden.write", class),
            d(u, 53, "MUT201", "Overridden.write", forced),
            // A class constant the class body deletes is no constant, nor one stored on its
            // class elsewhere (a `Final` one stays: line 32), also through `cls` and in its
            // own module, in a function or in the module body.
            d(u, 65, "MUT205", "Deleted.write", class),
            d(u, 72, "MUT205", "Configured.write", class),
            d(u, 83, "MUT205", "Reconfigured.write", class),
            d(u, 90, "MUT205", "Switched.write", class),
            d(u, 101, "MUT205", "Retitled.write", class),
            // A subclass that code stores the name on holds its own value: the inherited
            // constant does not name it there, nor on `self` of the base, whose instance may be
            // of that subclass. One no code stores on keeps the base's constant.
            d(u, 111, "MUT205", "Base.write", class),
            d(u, 116, "MUT205", "Inheriting.write_own", class),
            d(u, 121, "MUT205", "ForeignInheriting.write_own", class),
            d(u, 126, "MUT201", "Untouched.write_own", forced),
            // An instance of a protocol is one of its implementations: the constant holds only
            // when each of them holds the same one.
            d(u, 146, "MUT205", "write_through_protocol", class),
            d(u, 163, "MUT201", "write_through_same", forced),
            // Stored through a chain of aliases of the class.
            d(u, 171, "MUT205", "Aliased.write", class),
            // An attribute of a value of no project class names no constant.
            d(u, 182, "MUT205", "via_builtin", class),
        ]
    );
}

/// A project guarding `DataModelFieldBase._forced_field_assignment` in strict mode, with
/// `pkg/use.py` reading class constants.
fn class_keys_project(name: &str, use_src: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT201", "MUT205"]

[[tool.heki.write-owners]]
name = "store"
targets = ["DataModelFieldBase._forced_field_assignment"]
unresolved = "report"
"#,
    )
    .write("pkg/__init__.py", "")
    .write("pkg/model.py", MODEL)
    .write("pkg/use.py", use_src);
    p
}

const MRO_KEYS: &str = r#"from typing import ClassVar

from pkg.model import DataModelFieldBase


class Forced:
    KEY: ClassVar[str] = "_forced_field_assignment"


class Other:
    KEY: ClassVar[str] = "_other"


class OtherFirst(Other, Forced):
    pass


def write_forced(keys: Forced, field: DataModelFieldBase) -> None:
    field.__dict__[keys.KEY] = True


def write_other_first(field: DataModelFieldBase) -> None:
    field.__dict__[OtherFirst.KEY] = True


class Listed:
    KEY: ClassVar[str] = "_forced_field_assignment"


class ListedFirst(Listed, Other):
    pass


def write_listed(keys: Listed, field: DataModelFieldBase) -> None:
    field.__dict__[keys.KEY] = True
"#;

/// A subclass finds a class attribute along its own MRO: a base it lists before the class
/// (`class OtherFirst(Other, Forced)`) defines `KEY` first, so an instance of `Forced` may read
/// `Other`'s value, and the subclass itself reads it. A subclass listing the class first reads
/// the class's constant.
#[test]
fn a_subclass_listing_another_base_first_reads_that_base_constant() {
    let p = class_keys_project("keys_subclass_mro", MRO_KEYS);
    let u = "pkg/use.py";
    let class = "DataModelFieldBase";
    let forced = "DataModelFieldBase._forced_field_assignment";
    assert_eq!(
        found(&diagnostics(&p, &[])),
        vec![
            d(u, 19, "MUT205", "write_forced", class),
            d(u, 35, "MUT201", "write_listed", forced),
        ]
    );
}

const FIELD_TRANSFORMS: &str = r#"from typing import dataclass_transform


@dataclass_transform()
class ModelBase:
    pass


@dataclass_transform()
def model(cls):
    return cls
"#;

const FIELD_KEYS: &str = r#"from dataclasses import dataclass
from typing import ClassVar, Final

from pkg.model import DataModelFieldBase
from pkg.transforms import ModelBase, model


@dataclass
class Keys:
    KEY: Final[str] = "_forced_field_assignment"
    CONST: ClassVar[str] = "_forced_field_assignment"

    @classmethod
    def write_cls(cls, field: DataModelFieldBase) -> None:
        field.__dict__[cls.KEY] = True


def write_keys(keys: Keys, field: DataModelFieldBase) -> None:
    field.__dict__[keys.KEY] = True
    field.__dict__[keys.CONST] = True
    field.__dict__[Keys.KEY] = True


@dataclass
class Base:
    KEY: Final[str] = "_forced_field_assignment"

    @classmethod
    def write_cls(cls, field: DataModelFieldBase) -> None:
        field.__dict__[cls.KEY] = True


@dataclass(slots=True)
class Slotted(Base):
    pass


@dataclass(slots=True)
class SlotKeys:
    KEY: Final[str] = "_forced_field_assignment"


class Built(ModelBase):
    KEY: Final[str] = "_forced_field_assignment"


@model
class Decorated:
    KEY: Final[str] = "_forced_field_assignment"


def write_classes(field: DataModelFieldBase) -> None:
    field.__dict__[Base.KEY] = True
    field.__dict__[Slotted.KEY] = True
    field.__dict__[SlotKeys.KEY] = True
    field.__dict__[Built.KEY] = True
    field.__dict__[Decorated.KEY] = True
"#;

/// A `Final` name with a default in a dataclass is a field: an instance holds the value it was
/// constructed with (`Keys("x").KEY`), so it is no constant there, unlike a `ClassVar`. The
/// class object keeps the default as its attribute, unless a class along the way replaces it:
/// `slots=True` (also on a subclass that `cls` may be) puts a slot descriptor there, and a
/// class a `dataclass_transform` builds, through a base class or a decorator, holds what the
/// transform's runtime makes of it.
#[test]
fn a_final_dataclass_field_is_a_constant_only_where_the_class_keeps_its_default() {
    let p = class_keys_project("keys_dataclass_fields", FIELD_KEYS);
    p.write("pkg/transforms.py", FIELD_TRANSFORMS);
    let u = "pkg/use.py";
    let class = "DataModelFieldBase";
    let forced = "DataModelFieldBase._forced_field_assignment";
    assert_eq!(
        found(&diagnostics(&p, &[])),
        vec![
            d(u, 15, "MUT201", "Keys.write_cls", forced),
            d(u, 19, "MUT205", "write_keys", class),
            d(u, 20, "MUT201", "write_keys", forced),
            d(u, 21, "MUT201", "write_keys", forced),
            d(u, 30, "MUT205", "Base.write_cls", class),
            d(u, 53, "MUT201", "write_classes", forced),
            d(u, 54, "MUT205", "write_classes", class),
            d(u, 55, "MUT205", "write_classes", class),
            d(u, 56, "MUT205", "write_classes", class),
            d(u, 57, "MUT205", "write_classes", class),
        ]
    );
}

const STARRED_KEYS: &str = r#"from pkg.model import DataModelFieldBase

STARRED = "_forced_field_assignment"
_PRIVATE = "_forced_field_assignment"

from pkg.helpers import *

AFTER = "_forced_field_assignment"


def write(field: DataModelFieldBase) -> None:
    field.__dict__[STARRED] = True
    field.__dict__[_PRIVATE] = True
    field.__dict__[AFTER] = True
"#;

/// A star import may rebind the names the module bound before it, so they hold no constant
/// there: the public ones, and a `_private` one too, which the imported module's `__all__` may
/// list. A name bound after it keeps its value.
#[test]
fn a_star_import_after_a_module_constant_may_rebind_it() {
    let p = class_keys_project("keys_star_import", STARRED_KEYS);
    p.write("pkg/helpers.py", "def helper() -> None:\n    pass\n");
    let u = "pkg/use.py";
    let forced = "DataModelFieldBase._forced_field_assignment";
    assert_eq!(
        found(&diagnostics(&p, &[])),
        vec![
            d(u, 12, "MUT205", "write", "DataModelFieldBase"),
            d(u, 13, "MUT205", "write", "DataModelFieldBase"),
            d(u, 14, "MUT201", "write", forced),
        ]
    );
}

#[test]
fn a_final_key_of_another_module_follows_its_value() {
    let p = keys_project("keys_other_value");
    p.write(
        "pkg/keys.py",
        &KEYS.replace(
            "FORCED: Final = \"_forced_field_assignment\"",
            "FORCED: Final = \"_other\"",
        ),
    );
    let lines: Vec<u64> = found(&diagnostics(&p, &[]))
        .into_iter()
        .filter(|x| x.3 == "imported")
        .map(|x| x.1)
        .collect();
    assert!(lines.is_empty(), "{lines:?}");
}

#[test]
fn an_entry_target_is_a_write_target_only() {
    let p = keys_project("keys_read_entry");
    for (section, key) in [
        ("read-owners", "targets"),
        ("effect-boundaries", "read-targets"),
    ] {
        let extra = if section == "effect-boundaries" {
            "scopes = [\"pkg.use\"]\nforbid = [\"reads-targets\"]\n"
        } else {
            ""
        };
        p.write(
            "pyproject.toml",
            &format!(
                "[project]\nname = \"t\"\n\n[[tool.heki.{section}]]\nname = \"r\"\n{extra}{key} = ['DataModelFieldBase.extras[\"init\"]']\n"
            ),
        );
        let (code, stdout, stderr) = p.check(&[]);
        assert_eq!(code, 2, "{stdout}{stderr}");
        assert!(
            stderr.contains(&format!(
                "invalid {section}{} target `DataModelFieldBase.extras[\"init\"]`: a container entry",
                if section == "effect-boundaries" {
                    ".read-targets"
                } else {
                    ""
                }
            )),
            "{stderr}"
        );
    }
}

// ============================================================================ fresh objects

const FRESH_MODEL: &str = r#"import copy
import enum
import functools


class DataType:
    def __init__(self, type: str = "") -> None:
        self.type = type
        self.data_types: list["DataType"] = []
        self.parent: "DataType | None" = None


class DataModel:
    def __init__(self) -> None:
        self.fields: list[str] = []
        self.name = ""


REGISTRY: list[object] = []


def keep(obj: object) -> None:
    REGISTRY.append(obj)


def look(obj: DataModel) -> int:
    return len(obj.fields)


def shallow(node: DataType) -> DataType:
    copied = copy.copy(node)
    copied.parent = None
    return copied


@functools.lru_cache
def cached_model() -> DataModel:
    return DataModel()


class Interned(DataModel):
    _seen: dict[str, "Interned"] = {}

    def __new__(cls, key: str = "") -> "Interned":
        return cls._seen.setdefault(key, super().__new__(cls))


class Kind(DataModel, enum.Enum):
    A = "a"
"#;

const FRESH_USE: &str = r#"from pkg.model import (
    REGISTRY,
    DataModel,
    DataType,
    Interned,
    Kind,
    cached_model,
    keep,
    look,
    shallow,
)


def copy_node(node: DataType) -> DataType:
    copied = DataType(type=node.type)
    copied.data_types = list(node.data_types)
    return copied


def escaped_first(node: DataType) -> DataType:
    copied = DataType(type=node.type)
    REGISTRY.append(copied)
    copied.data_types = []
    return copied


def kept_by_helper() -> DataModel:
    m = DataModel()
    keep(m)
    m.name = "x"
    return m


def read_by_helper() -> DataModel:
    m = DataModel()
    look(m)
    m.fields.append("x")
    return m


def from_a_factory(node: DataType) -> DataType:
    c = shallow(node)
    c.data_types = []
    c.data_types.append(node)
    return c


def _fill(m: DataModel) -> None:
    m.name = "filled"
    m.fields.append("a")


def build() -> DataModel:
    m = DataModel()
    _fill(m)
    _fill(m)
    return m


def build_existing(m: DataModel) -> DataModel:
    _fill(m)
    return m


def _keep_then_fill(m: DataModel) -> None:
    keep(m)
    m.name = "late"


def build_kept() -> DataModel:
    m = DataModel()
    _keep_then_fill(m)
    return m


def per_iteration(names: list[str]) -> list[DataModel]:
    out = []
    for n in names:
        m = DataModel()
        m.name = n
        out.append(m)
    return out


def across_iterations(names: list[str]) -> DataModel:
    m = DataModel()
    for n in names:
        m.name = n
        keep(m)
    return m


def read_only(model: DataModel) -> int:
    return len(model.fields)


def shared_ones() -> None:
    cached_model().name = "cached"
    Interned("k").name = "interned"
    Kind("a").name = "member"
"#;

fn fresh_project(name: &str, mode: &str) -> Project {
    let p = Project::new(name);
    let mode = if mode.is_empty() {
        String::new()
    } else {
        format!("fresh-objects = \"{mode}\"\n")
    };
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\", \"EFF201\"]\n\n[[tool.heki.write-owners]]\nname = \"store\"\ntargets = [\"DataType.data_types\", \"DataModel.fields\", \"DataModel.name\"]\nallowed-writers = [\"pkg.store.*\"]\n{mode}\n[[tool.heki.effect-boundaries]]\nname = \"pure\"\nscopes = [\"pkg.use.copy_node\", \"pkg.use.escaped_first\"]\nforbid = [\"writes-targets\"]\nwrite-targets = [\"DataType.*\"]\n{mode}"
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/model.py", FRESH_MODEL)
    .write("pkg/use.py", FRESH_USE);
    p
}

#[test]
fn writes_that_construct_their_object_are_allowed_by_default() {
    let u = "pkg/use.py";
    let reported = vec![
        // Stored before the write, kept by a helper first.
        d(u, 23, "EFF201", "escaped_first", "pure:DataType.*"),
        d(u, 23, "MUT201", "escaped_first", "DataType.data_types"),
        d(u, 30, "MUT201", "kept_by_helper", "DataModel.name"),
        // A shallow copy from a factory: rebinding constructs it, changing the container it
        // shares with the original does not.
        d(u, 44, "MUT201", "from_a_factory", "DataType.data_types"),
        // A parameter is no new object; a helper keeping the object before it writes.
        d(u, 61, "MUT201", "build_existing", "DataModel.fields"),
        d(u, 61, "MUT201", "build_existing", "DataModel.name"),
        d(u, 72, "MUT201", "build_kept", "DataModel.name"),
        // Allocated once before the loop and kept in an earlier iteration.
        d(u, 88, "MUT201", "across_iterations", "DataModel.name"),
        // A cached result, a class whose `__new__` hands out existing instances, an enumeration.
        d(u, 98, "MUT201", "shared_ones", "DataModel.name"),
        d(u, 99, "MUT201", "shared_ones", "DataModel.name"),
        d(u, 100, "MUT201", "shared_ones", "DataModel.name"),
    ];
    for mode in ["", "allow"] {
        let p = fresh_project(&format!("fresh_allow_{mode}"), mode);
        assert_eq!(found(&diagnostics(&p, &[])), reported, "{mode}");
    }
    // `report`: every write of a target outside the owners, as before.
    let p = fresh_project("fresh_report", "report");
    let all = found(&diagnostics(&p, &[]));
    for expected in [
        d(u, 16, "EFF201", "copy_node", "pure:DataType.*"),
        d(u, 16, "MUT201", "copy_node", "DataType.data_types"),
        d(u, 37, "MUT201", "read_by_helper", "DataModel.fields"),
        d(u, 43, "MUT201", "from_a_factory", "DataType.data_types"),
        // One report per helper write: the first call stands for both.
        d(u, 55, "MUT201", "build", "DataModel.name"),
        d(u, 80, "MUT201", "per_iteration", "DataModel.name"),
    ] {
        assert!(all.contains(&expected), "{expected:?} in {all:#?}");
    }
    assert!(reported.iter().all(|r| all.contains(r)), "{all:#?}");
    assert!(all.iter().all(|x| x.3 != "read_only"));
}

#[test]
fn fresh_objects_and_unresolved_take_known_values_only() {
    let p = fresh_project("fresh_bad_mode", "maybe");
    let (code, stdout, stderr) = p.check(&[]);
    assert_eq!(code, 2, "{stdout}{stderr}");
    assert!(
        stderr.contains(
            "unknown `fresh-objects` value `maybe` in write-owners `store` (expected `allow` or `report`)"
        ),
        "{stderr}"
    );
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.effect-boundaries]]\nname = \"b\"\nscopes = [\"pkg\"]\nforbid = [\"blocking\"]\nfresh-objects = \"no\"\n",
    );
    let (code, _, stderr) = p.check(&[]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("unknown `fresh-objects` value `no` in effect-boundaries `b`"),
        "{stderr}"
    );
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.write-owners]]\nname = \"s\"\ntargets = [\"DataModel.fields\"]\nunresolved = \"yes\"\n",
    );
    let (code, _, stderr) = p.check(&[]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains(
            "unknown `unresolved` value `yes` in write-owners `s` (expected `ignore` or `report`)"
        ),
        "{stderr}"
    );
}

// ============================================================================== strict mode

const STRICT_MODEL: &str = r#"class DataModel:
    def __init__(self, **kw: object) -> None:
        self.fields: list[str] = []
        self.name = ""
        for k, v in kw.items():
            setattr(self, k, v)


class Other:
    def __init__(self) -> None:
        self.fields: list[str] = []
"#;

const STRICT_USE: &str = r#"import copy
from typing import Any

from pkg.model import DataModel, Other


def touch(model) -> None:
    model.fields = []


def read_only(model: DataModel) -> int:
    return len(model.fields)


def loop(models: list) -> None:
    for m in models:
        m.fields.append("x")
        m.size = 3


def anything(obj: Any, name: str, model: DataModel) -> None:
    obj.name = "x"
    setattr(model, name, 1)
    model.__dict__[name] = 1
    setattr(obj, name, 1)


def known_class(o: Other) -> None:
    o.fields = []


def _typed_helper(m) -> None:
    m.name = "y"


def typed_caller(model: DataModel) -> None:
    _typed_helper(model)


def _untyped_helper(m) -> None:
    m.name = "z"


def untyped_caller(x) -> None:
    _untyped_helper(x)


def construct(node: DataModel):
    copied = copy.copy(node)
    copied.name = "c"
    return copied
"#;

fn strict_project(name: &str, strict: bool) -> Project {
    let p = Project::new(name);
    let mode = if strict {
        "unresolved = \"report\"\n"
    } else {
        ""
    };
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\", \"MUT205\"]\n\n[[tool.heki.write-owners]]\nname = \"store\"\ntargets = [\"DataModel.fields\", \"DataModel.name\"]\nallowed-writers = [\"pkg.store.*\"]\n{mode}"
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/model.py", STRICT_MODEL)
    .write("pkg/use.py", STRICT_USE)
    .write("pkg/store.py", "def save(model) -> None:\n    model.fields = []\n");
    p
}

#[test]
fn strict_mode_reports_the_writes_heki_cannot_tie_to_a_class() {
    let u = "pkg/use.py";
    let typed = d(u, 37, "MUT201", "typed_caller", "DataModel.name");
    // Off by default: `MUT205` reports nothing until a contract opts in.
    let p = strict_project("strict_off", false);
    assert_eq!(found(&diagnostics(&p, &[])), vec![typed.clone()]);
    let p = strict_project("strict_on", true);
    let diags = diagnostics(&p, &[]);
    assert_eq!(
        found(&diags),
        vec![
            // An unannotated parameter no project caller types, an item of a list of unknown
            // items, `Any`.
            d(u, 8, "MUT205", "touch", "fields"),
            d(u, 17, "MUT205", "loop", "fields"),
            d(u, 22, "MUT205", "anything", "name"),
            // A computed name on a guarded class (`setattr`, `__dict__`), and on an object of
            // unknown class, which may be any attribute of any class.
            d(u, 23, "MUT205", "anything", "DataModel"),
            d(u, 24, "MUT205", "anything", "DataModel"),
            d(u, 25, "MUT205", "anything", "*"),
            // Typed by its caller: `MUT201` reports the caller.
            typed,
            // Untyped by every caller.
            d(u, 41, "MUT205", "_untyped_helper", "name"),
        ]
    );
    assert_eq!(
        message_at(&diags, u, 8),
        "`touch` writes `fields` (by attribute) on an object of unknown class outside the write owners of `store`: heki cannot tell whether it writes `DataModel.fields`"
    );
    assert!(
        message_at(&diags, u, 23).contains(
            "writes an attribute named at run time (with `setattr`) on `DataModel` outside the write owners of `store`: heki cannot tell whether it writes `DataModel.fields` or `DataModel.name`"
        ),
        "{diags:#?}"
    );
    assert_eq!(
        message_at(&diags, u, 25),
        "`anything` writes an attribute named at run time (with `setattr`) on an object of unknown class outside the write owners of `store`: heki cannot tell whether it writes `DataModel.fields` or `DataModel.name`"
    );
}

const RUN_TIME_NAMES: &str = r#"from typing import Any

from pkg.model import DataModel, Other


def copy_by_name(target, source: object, names: list[str]) -> None:
    for name in names:
        setattr(target, name, getattr(source, name))


def store_by_key(target, key: str, value: object) -> None:
    target.__dict__[key] = value


def namespace(target: Any, values: dict) -> None:
    vars(target).update(values)
    target.__dict__.clear()


def _fill(target, name: str) -> None:
    setattr(target, name, 1)


def fill_model(model: DataModel) -> None:
    _fill(model, "x")


def _fill_other(target, name: str) -> None:
    setattr(target, name, 1)


def fill_other(other: Other) -> None:
    _fill_other(other, "x")


def plain(d: dict, name: str) -> None:
    setattr(d, name, 1)


def narrowed(target, name: str) -> None:
    if isinstance(target, Other):
        setattr(target, name, 1)


def control_typed(target: DataModel, name: str, value: object) -> None:
    setattr(target, name, value)


def control_named(target) -> None:
    target.name = "x"
"#;

/// A computed attribute name written on an object of unknown class (`setattr` with a computed
/// name, `__dict__` with a computed key, `vars(obj).update(...)`, `__dict__.clear()`) may write
/// any target: it is reported, unless every caller path binds the object to a class that is no
/// target's (`MUT201` sees no name at a caller binding a target class, so that caller leaves the
/// write to `MUT205`). A builtin container, an object an `isinstance` test narrows to another
/// class, and the allowed writers are no such write.
#[test]
fn strict_mode_reports_a_run_time_name_on_an_object_of_unknown_class() {
    let p = strict_project("strict_run_time_names", true);
    p.write("pkg/names.py", RUN_TIME_NAMES).write(
        "pkg/store.py",
        "def save(model, name: str) -> None:\n    setattr(model, name, [])\n",
    );
    let n = "pkg/names.py";
    let diags = diagnostics(&p, &[]);
    let names: Vec<_> = found(&diags).into_iter().filter(|x| x.0 == n).collect();
    assert_eq!(
        names,
        vec![
            d(n, 8, "MUT205", "copy_by_name", "*"),
            d(n, 12, "MUT205", "store_by_key", "*"),
            d(n, 16, "MUT205", "namespace", "*"),
            d(n, 17, "MUT205", "namespace", "*"),
            d(n, 21, "MUT205", "_fill", "*"),
            d(n, 46, "MUT205", "control_typed", "DataModel"),
            d(n, 50, "MUT205", "control_named", "name"),
        ]
    );
    assert_eq!(
        message_at(&diags, n, 12),
        "`store_by_key` writes an attribute named at run time (as a `__dict__` item) on an object of unknown class outside the write owners of `store`: heki cannot tell whether it writes `DataModel.fields` or `DataModel.name`"
    );
    // Off by default.
    let p = strict_project("strict_run_time_names_off", false);
    p.write("pkg/names.py", RUN_TIME_NAMES);
    assert!(
        found(&diagnostics(&p, &[]))
            .iter()
            .all(|x| x.0 != n || x.2 != "MUT205")
    );
}

const INITIALIZING_HELPERS: &str = r#"from pkg.model import DataModel


def _setup(obj, name: str, value: object) -> None:
    setattr(obj, name, value)


def _setup_also_later(obj, name: str, value: object) -> None:
    setattr(obj, name, value)


class Configured(DataModel):
    def __init__(self, name: str) -> None:
        super().__init__()
        _setup(self, name, 1)
        _setup_also_later(self, name, 1)

    def reset(self, name: str) -> None:
        _setup_also_later(self, name, 2)
"#;

/// A run-time name written on an object of unknown class may write any target, so an
/// initializer of any target's class handing its own `self` to the helper is that class's own
/// initialization: a helper only such initializers run is not reported, one a method runs too
/// is.
#[test]
fn strict_mode_leaves_a_run_time_name_to_the_initialization_running_it() {
    let p = strict_project("strict_run_time_names_init", true);
    p.write("pkg/setup.py", INITIALIZING_HELPERS);
    let s = "pkg/setup.py";
    let found: Vec<_> = found(&diagnostics(&p, &[]))
        .into_iter()
        .filter(|x| x.0 == s)
        .collect();
    assert_eq!(found, vec![d(s, 9, "MUT205", "_setup_also_later", "*")]);
}

/// Objects of unknown class that `isinstance` tests narrow to a class of the standard library.
const NARROWED: &str = r#"from dataclasses import Field


def narrowed(default: object, name: str) -> None:
    if isinstance(default, Field):
        default.name = name


def returns_unless(default, name: str) -> None:
    if not isinstance(default, Field):
        return
    default.name = name


def not_narrowed(default: object, name: str) -> None:
    if isinstance(default, Field):
        pass
    default.name = name
"#;

#[test]
fn strict_mode_knows_the_class_an_isinstance_test_narrows_to() {
    let p = strict_project("strict_narrowed", true);
    p.write("pkg/narrowed.py", NARROWED);
    let narrowed: Vec<_> = found(&diagnostics(&p, &[]))
        .into_iter()
        .filter(|x| x.0 == "pkg/narrowed.py")
        .collect();
    assert_eq!(
        narrowed,
        vec![d("pkg/narrowed.py", 18, "MUT205", "not_narrowed", "name")]
    );
}

#[test]
fn strict_findings_are_counted_by_the_ratchet_baseline() {
    let p = strict_project("strict_baseline", true);
    let args = ["--baseline", "heki-baseline.toml"];
    let mut update = args.to_vec();
    update.push("--update-baseline");
    assert_eq!(p.check(&update).0, 0);
    assert!(
        p.read("heki-baseline.toml")
            .contains("rule_code = \"MUT205\"")
    );
    assert_eq!(p.check(&args).0, 0);
    // One more write the contract cannot see is a regression.
    p.write(
        "pkg/use.py",
        &STRICT_USE.replace(
            "def touch(model) -> None:\n    model.fields = []\n",
            "def touch(model) -> None:\n    model.fields = []\n    model.name = \"\"\n",
        ),
    );
    let (code, stdout, _) = p.check(&[
        "--baseline",
        "heki-baseline.toml",
        "--output-format",
        "concise",
    ]);
    assert_eq!(code, 1, "{stdout}");
    assert!(
        stdout.contains("MUT205 [heuristic] `touch` writes `name`"),
        "{stdout}"
    );

    let (code, doc, _) = p.heki(&["rule", "MUT205"]);
    assert_eq!(code, 0);
    assert!(
        doc.starts_with("# unresolved-guarded-write (MUT205)\n\n"),
        "{doc}"
    );
}

// ================================================================ construction: every shape

const SHAPES_MODEL: &str = r#"import pydantic
from dataclasses import dataclass, field


class Model(pydantic.BaseModel):
    def __init__(self) -> None:
        self.fields: list[str] = []
        self.name = ""


@dataclass
class Rec:
    tags: list[str] = field(default_factory=list)


REG: list[object] = []
TEMPLATE = Model()
"#;

const SHAPES_USE: &str = r#"import copy
import dataclasses
import logging

from pkg.model import REG, TEMPLATE, Model, Rec

G = None


def deep(m: Model) -> None:
    c: Model = copy.deepcopy(m)
    c.fields.append("x")


def shallow_in_place(m: Model) -> None:
    c: Model = copy.copy(m)
    c.fields.append("x")


def replaced(m: Model) -> None:
    c: Model = dataclasses.replace(m, name="x")
    c.name = "y"


def dunders(m: Model) -> None:
    c: Model = m.__copy__()
    c.name = "z"
    d: Model = m.__deepcopy__({})
    d.fields.append("z")


def pydantic_copies(m: Model) -> None:
    c: Model = m.model_copy(deep=True)
    c.fields.append("x")
    s: Model = m.model_copy()
    s.fields.append("x")


def dataclass_without_init() -> None:
    r = Rec()
    r.tags.append("a")


def yielded():
    m = Model()
    yield m
    m.name = "x"


def captured() -> None:
    m = Model()
    f = lambda: m
    m.name = "x"
    f()


def stored_global() -> None:
    global G
    m = Model()
    G = m
    m.name = "x"


def in_literals() -> None:
    m = Model()
    n = Model()
    items = [m]
    table = {"k": n}
    m.name = "x"
    n.name = "y"
    print(items, table)


def logged() -> None:
    m = Model()
    logging.info("%s", m)
    m.name = "x"


def inspected() -> None:
    m = Model()
    print(m)
    len(m.fields)
    isinstance(m, Model)
    m.name = "x"


def unknown_code() -> None:
    m = Model()
    mystery(m)
    m.name = "x"


def named_at_run_time() -> None:
    m = Model()
    setattr(m, "name", "x")


def _gen(x):
    yield x


def to_a_generator() -> None:
    m = Model()
    _gen(m)
    m.name = "x"


def _take(**kw):
    return kw


def through_kwargs() -> None:
    m = Model()
    _take(obj=m)
    m.name = "x"


def _reads(x: Model) -> int:
    return len(x.fields)


def _a(m: Model) -> None:
    _reads(m)
    m.name = "a"


def build_through_two() -> Model:
    m = Model()
    _a(m)
    return m


def _keeps(x: Model) -> None:
    REG.append(x)


def _d(m: Model) -> None:
    _keeps(m)
    m.name = "d"


def build_kept_below() -> Model:
    m = Model()
    _d(m)
    return m


def _push(x):
    x.fields.append("p")


def _push_kept(x):
    REG.append(x)
    x.fields.append("p")


def build_untyped_helpers() -> Model:
    m = Model()
    _push(m)
    n = Model()
    _push_kept(n)
    return m


def _make(flag: bool) -> Model:
    if flag:
        return Model()
    shallow: Model = copy.copy(TEMPLATE)
    return shallow


def _shared() -> Model:
    return TEMPLATE


def _rec(n: int) -> Model:
    if n:
        return _rec(n - 1)
    return Model()


def factories() -> None:
    m = _make(True)
    m.name = "x"
    m.fields.append("x")
    s = _shared()
    s.name = "x"
    r = _rec(3)
    r.name = "x"


if REG:

    def _twice() -> Model:
        return Model()

    def _either() -> Model:
        return Model()
else:

    def _twice() -> Model:
        return Model()

    def _either() -> Model:
        return TEMPLATE


def duplicates() -> None:
    _twice().name = "x"
    _either().name = "x"
"#;

#[test]
fn construction_follows_copies_escapes_helpers_and_factories() {
    let p = Project::new("fresh_shapes");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\"]\n\n[[tool.heki.write-owners]]\nname = \"store\"\ntargets = [\"Model.fields\", \"Model.name\", \"Rec.tags\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write("pkg/model.py", SHAPES_MODEL)
    .write("pkg/use.py", SHAPES_USE);
    let lines = |diags: &[Value]| -> Vec<(String, u64)> {
        let mut v: Vec<(String, u64)> = found(diags)
            .into_iter()
            .filter(|x| x.0 == "pkg/use.py")
            .map(|x| (x.3, x.1))
            .collect();
        v.dedup();
        v
    };
    let q = |name: &str, line: u64| (name.to_owned(), line);
    assert_eq!(
        lines(&diagnostics(&p, &[])),
        vec![
            // A shallow copy's containers are the original's.
            q("shallow_in_place", 17),
            q("pydantic_copies", 36),
            // Stored, yielded, captured, put in containers, handed to logging or to code heki
            // does not see, to a generator, or through `**kwargs` before the write.
            q("yielded", 47),
            q("captured", 53),
            q("stored_global", 61),
            q("in_literals", 69),
            q("in_literals", 70),
            q("logged", 77),
            q("unknown_code", 91),
            q("to_a_generator", 106),
            q("through_kwargs", 116),
            // A helper keeping the object before it writes; an untyped helper keeping it.
            q("build_kept_below", 145),
            q("build_untyped_helpers", 162),
            // A factory returning a shallow copy on one path, a shared object, a recursive one.
            q("factories", 186),
            q("factories", 188),
            q("factories", 190),
            // Two definitions of which one returns a shared object.
            q("duplicates", 211),
        ]
    );
    // `fresh-objects = "report"`: every write.
    p.write(
        "pyproject.toml",
        &p.read("pyproject.toml").replace(
            "\"Rec.tags\"]\n",
            "\"Rec.tags\"]\nfresh-objects = \"report\"\n",
        ),
    );
    let all = lines(&diagnostics(&p, &[]));
    for expected in [
        q("deep", 12),
        q("replaced", 22),
        q("dunders", 27),
        q("dunders", 29),
        q("pydantic_copies", 34),
        q("dataclass_without_init", 41),
        q("inspected", 85),
        q("named_at_run_time", 96),
        q("build_through_two", 130),
        q("build_untyped_helpers", 160),
        q("factories", 185),
        q("duplicates", 210),
    ] {
        assert!(all.contains(&expected), "{expected:?} in {all:#?}");
    }
}

// ================================================================ strict mode: every shape

const STRICT2_USE: &str = r#"from pkg.model import DataModel


def plain_object(obj: object) -> None:
    obj.fields = []


def computed(model: DataModel, name: str) -> None:
    object.__setattr__(model, name, 1)
    model.__dict__.update({})
    model.__dict__.popitem()
    model.__dict__.pop(name)
    del model.__dict__[name]


def containers(x) -> None:
    x.fields[0] = "a"
    x.meta["k"]["j"] = 1


def _defaulted(m=None) -> None:
    m.fields = []


def calls_with_default() -> None:
    _defaulted()


def _set(m) -> None:
    m.fields = []


def passes_a_new_unknown() -> None:
    _set(object())


def _set_both(m) -> None:
    m.name = ""


def typed_caller(model: DataModel) -> None:
    _set_both(model)
"#;

#[test]
fn strict_mode_walks_callers_and_honors_the_contract_scopes() {
    let p = Project::new("strict_shapes");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT205\"]\n\n[[tool.heki.write-owners]]\nname = \"store\"\ntargets = [\"DataModel.fields\", \"DataModel.name\", \"DataModel.meta\"]\nallowed-writers = [\"pkg.store.*\"]\nunresolved = \"report\"\n",
    )
    .write("pkg/__init__.py", "")
    .write("pkg/model.py", STRICT_MODEL.replace("        self.name = \"\"\n", "        self.name = \"\"\n        self.meta: dict[str, dict[str, int]] = {}\n").as_str())
    .write("pkg/use.py", STRICT2_USE)
    .write(
        "pkg/store.py",
        "from pkg.use import _set_both\n\n\ndef save(m) -> None:\n    _set_both(m)\n",
    );
    let diags = diagnostics(&p, &[]);
    let u = "pkg/use.py";
    assert_eq!(
        found(&diags),
        vec![
            d(u, 5, "MUT205", "plain_object", "fields"),
            d(u, 9, "MUT205", "computed", "DataModel"),
            d(u, 10, "MUT205", "computed", "DataModel"),
            d(u, 11, "MUT205", "computed", "DataModel"),
            d(u, 12, "MUT205", "computed", "DataModel"),
            d(u, 13, "MUT205", "computed", "DataModel"),
            d(u, 17, "MUT205", "containers", "fields"),
            d(u, 18, "MUT205", "containers", "meta"),
            // A parameter left to its default, and `object()`: no project class.
            d(u, 22, "MUT205", "_defaulted", "fields"),
            d(u, 30, "MUT205", "_set", "fields"),
        ]
    );
    assert!(
        message_at(&diags, u, 9).contains(
            "heki cannot tell whether it writes `DataModel.fields`, `DataModel.name`, or 1 more"
        ),
        "{diags:#?}"
    );
    assert!(
        message_at(&diags, u, 18).contains("writes `meta` (in place inside an item)"),
        "{diags:#?}"
    );
    // `forbidden-writers`: only the forbidden layer is reported.
    p.write(
        "pyproject.toml",
        &p.read("pyproject.toml").replace(
            "unresolved = ",
            "forbidden-writers = [\"pkg.use.computed\"]\nunresolved = ",
        ),
    );
    let only: Vec<String> = found(&diagnostics(&p, &[]))
        .into_iter()
        .map(|x| x.3)
        .collect();
    assert!(
        only.iter().all(|q| q == "computed") && only.len() == 5,
        "{only:?}"
    );
    // The rule can be ignored like any other.
    assert!(diagnostics(&p, &["--ignore", "MUT205"]).is_empty());
}

// ============================================================ constants bound to constants

const MORE: &str = r#"from typing import Final

import pkg.keys
from pkg import keys
from pkg.keys import *
from pkg.keys import FORCED
from pkg.keys import FORCED as RENAMED

IMPORTED: Final = FORCED
RENAMED_ALIAS: Final = RENAMED
ATTR: Final = keys.FORCED
DOTTED: Final = pkg.keys.FORCED
STARRED: Final = SECRET
CALLED: Final = str(FORCED)
"#;

const ALIAS_USE: &str = r#"from typing import Final

from pkg.c7 import K as K7
from pkg.c8 import K as K8
from pkg.chain import LONG, SHORT
from pkg.keys import FORCED
from pkg.model import DataModelFieldBase
from pkg.more import ATTR, CALLED, DOTTED, IMPORTED, RENAMED_ALIAS, STARRED

OWN: Final = FORCED


def aliases(field: DataModelFieldBase) -> None:
    field.__dict__[IMPORTED] = True
    field.__dict__[RENAMED_ALIAS] = True
    field.__dict__[ATTR] = True
    field.__dict__[DOTTED] = True
    field.__dict__[OWN] = True
    field.__dict__[SHORT] = True
    field.__dict__[K7] = True


def not_constants(field: DataModelFieldBase) -> None:
    field.__dict__[STARRED] = True
    field.__dict__[CALLED] = True
    field.__dict__[LONG] = True
    field.__dict__[K8] = True


class Setter:
    def __init__(self) -> None:
        self.name = "_forced_field_assignment"

    def apply(self, field: DataModelFieldBase) -> None:
        setattr(field, self.name, True)


NODE = DataModelFieldBase()
setattr(NODE, FORCED, True)
NODE.__dict__[OWN] = True
"#;

/// `k1 = k0` ... `k8 = k7` below a literal `k0`; `SHORT` is bound to `k6`, `LONG` to `k7`.
fn copied_constants() -> String {
    let mut text = String::from("from typing import Final\n\nk0 = \"_forced_field_assignment\"\n");
    for i in 1..9 {
        text.push_str(&format!("k{i} = k{}\n", i - 1));
    }
    text.push_str("\nSHORT: Final = k6\nLONG: Final = k7\n");
    text
}

#[test]
fn constants_bound_to_other_constants_are_followed_a_bounded_way() {
    let p = Project::new("keys_aliases");
    p.write(
        "pyproject.toml",
        r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT201", "MUT205"]

[[tool.heki.write-owners]]
name = "store"
targets = ["DataModelFieldBase._forced_field_assignment"]
allowed-writers = ["pkg.store.*"]
unresolved = "report"
"#,
    )
    .write("pkg/__init__.py", KEYS_INIT)
    .write("pkg/model.py", MODEL)
    .write("pkg/keys.py", KEYS)
    .write("pkg/more.py", MORE)
    .write("pkg/chain.py", &copied_constants())
    .write("pkg/w.py", ALIAS_USE)
    .write(
        "pkg/c0.py",
        "from typing import Final\n\nK: Final = \"_forced_field_assignment\"\n",
    );
    // `c<k>.K` is bound to `c<k-1>.K`, imported.
    for k in 1..9 {
        p.write(
            &format!("pkg/c{k}.py"),
            &format!(
                "from typing import Final\n\nfrom pkg.c{} import K as PREVIOUS\n\nK: Final = PREVIOUS\n",
                k - 1
            ),
        );
    }
    let w = "pkg/w.py";
    let forced = "DataModelFieldBase._forced_field_assignment";
    let class = "DataModelFieldBase";
    assert_eq!(
        found(&diagnostics(&p, &[])),
        vec![
            // Constants bound to an imported constant (also renamed), to an attribute of an
            // imported module, to one of the module itself bound to an import, through six
            // module-level copies, and through seven modules.
            d(w, 14, "MUT201", "aliases", forced),
            d(w, 15, "MUT201", "aliases", forced),
            d(w, 16, "MUT201", "aliases", forced),
            d(w, 17, "MUT201", "aliases", forced),
            d(w, 18, "MUT201", "aliases", forced),
            d(w, 19, "MUT201", "aliases", forced),
            d(w, 20, "MUT201", "aliases", forced),
            // A star-imported name, a call, seven copies, eight modules: no constant, so the
            // strict mode counts the writes.
            d(w, 24, "MUT205", "not_constants", class),
            d(w, 25, "MUT205", "not_constants", class),
            d(w, 26, "MUT205", "not_constants", class),
            d(w, 27, "MUT205", "not_constants", class),
            // An attribute holding the name.
            d(w, 35, "MUT205", "Setter.apply", class),
            // The module body reads an imported constant and its own alias of one.
            d(w, 39, "MUT201", "<module>", forced),
            d(w, 40, "MUT201", "<module>", forced),
        ]
    );
}

// ======================================================= construction: more data-flow shapes

const SHAPES: &str = r#"import copy

from pkg.model import DataModelFieldBase

REGISTRY: list[object] = []
CURRENT = DataModelFieldBase()


def risky() -> None:
    pass


def _inspect(obj: object) -> None:
    print(obj)


def _keep(obj: object) -> None:
    REGISTRY.append(obj)


def handed(obj) -> None:
    dup = copy.copy(obj)
    _inspect(dup)
    dup.name = "x"
    kept = copy.copy(obj)
    _keep(kept)
    kept.name = "x"


def _touch(node: DataModelFieldBase) -> None:
    node.name = "x"


def lookup() -> DataModelFieldBase:
    return CURRENT


def mixed() -> None:
    _touch(DataModelFieldBase())
    _touch(lookup())


def fresh_only() -> None:
    _touch(DataModelFieldBase())
    _touch(DataModelFieldBase())


def after_escape_in_try() -> None:
    node = DataModelFieldBase()
    try:
        REGISTRY.append(node)
        risky()
    except ValueError:
        node.name = "x"


def in_except() -> None:
    node = DataModelFieldBase()
    try:
        risky()
    except ValueError:
        node.name = "x"


def either(flag: bool) -> None:
    node = DataModelFieldBase() if flag else CURRENT
    node.name = "x"


def register() -> None:
    field = DataModelFieldBase()
    field.extras["k"] = 1
    del field.extras["j"]
    REGISTRY.append(field)


def unregister() -> None:
    field = DataModelFieldBase()
    del field.extras["j"]
    REGISTRY.append(field)


def _log(*objs: object) -> None:
    print(*objs)


def starred(extra: list[object]) -> None:
    node = DataModelFieldBase()
    _log(*extra, node)
    node.name = "x"


def logged_copy(obj) -> None:
    dup = copy.copy(obj)
    _log(dup)
    dup.name = "x"


def _fresh_named() -> DataModelFieldBase:
    node = DataModelFieldBase()
    node.name = "x"
    return node


def uses_fresh() -> None:
    REGISTRY.append(_fresh_named())


def nonlocal_holder() -> None:
    holder = None

    def keep() -> None:
        nonlocal holder
        node = DataModelFieldBase()
        holder = node
        node.name = "x"

    keep()
    print(holder)
"#;

/// `def <name>() -> None:` binding a new object to `n0`, then `n1 = n0` ... and writing it.
fn fresh_through(name: &str, copies: usize) -> String {
    let mut text = format!("\n\ndef {name}() -> None:\n    n0 = DataModelFieldBase()\n");
    for i in 1..=copies {
        text.push_str(&format!("    n{i} = n{}\n", i - 1));
    }
    text.push_str(&format!("    n{copies}.name = \"x\"\n"));
    text
}

#[test]
fn construction_through_handed_copies_merged_chains_handlers_and_phis() {
    let p = Project::new("fresh_more_shapes");
    let ops = format!(
        "{SHAPES}{}{}",
        fresh_through("fourteen", 14),
        fresh_through("fifteen", 15)
    );
    p.write(
        "pyproject.toml",
        r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT201", "MUT205"]

[[tool.heki.write-owners]]
name = "store"
targets = ["DataModelFieldBase.name", "DataModelFieldBase.extras"]
allowed-writers = ["pkg.store.*"]
unresolved = "report"
"#,
    )
    .write("pkg/__init__.py", "")
    .write("pkg/model.py", MODEL)
    .write("pkg/ops.py", &ops);
    let o = "pkg/ops.py";
    let name = "DataModelFieldBase.name";
    assert_eq!(
        found(&diagnostics(&p, &[])),
        vec![
            // A copy of an object of unknown class handed to a helper that keeps it; the one
            // handed to a helper that only reads it (line 24) is still under construction.
            d(o, 27, "MUT205", "handed", "name"),
            // One chain through `_touch` writes an object `lookup` did not create; both of
            // `fresh_only`'s create theirs.
            d(o, 39, "MUT201", "mixed", name),
            // Stored before the handler writes it; a handler writing an object nothing else
            // saw (line 62) constructs it.
            d(o, 54, "MUT201", "after_escape_in_try", name),
            // New on one path only. (`register` and `unregister` change the entries of the object
            // they create.)
            d(o, 67, "MUT201", "either", name),
            // After `*` unpacking, no argument binds a known parameter: it may be kept. (A copy
            // handed to `*objs` that only prints them, line 96, and a private factory filling the
            // object it creates, line 101, construct their objects.)
            d(o, 90, "MUT201", "starred", name),
            // Stored in a closure variable before the write.
            d(o, 116, "MUT201", "nonlocal_holder.<locals>.keep", name),
            // Followed through fourteen copies, not fifteen.
            d(o, 158, "MUT201", "fifteen", name),
        ]
    );
}

// A copy handed to a helper by a position past its positional parameters: the tuple `*args`
// holds it, so the helper keeps it only by keeping that parameter, never through a keyword-only
// parameter after `*args`.
const PACKED: &str = r#"import copy

REGISTRY: list[object] = []


def _log(*objs: object) -> None:
    print(*objs)


def _keep_all(*objs: object) -> None:
    REGISTRY.extend(objs)


def _keep_label(first: object, *rest: object, label: object = None) -> None:
    REGISTRY.append(label)


def logged_second(obj) -> None:
    dup = copy.copy(obj)
    _log(1, dup)
    dup.name = "x"


def kept_second(obj) -> None:
    dup = copy.copy(obj)
    _keep_all(1, dup)
    dup.name = "x"


def beside_keyword_only(obj) -> None:
    dup = copy.copy(obj)
    _keep_label(1, dup)
    dup.name = "x"
"#;

#[test]
fn construction_through_arguments_star_args_holds() {
    let p = Project::new("fresh_packed_arguments");
    p.write(
        "pyproject.toml",
        r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT201", "MUT205"]

[[tool.heki.write-owners]]
name = "store"
targets = ["DataModelFieldBase.name"]
allowed-writers = ["pkg.store.*"]
unresolved = "report"
"#,
    )
    .write("pkg/__init__.py", "")
    .write("pkg/model.py", MODEL)
    .write("pkg/packed.py", PACKED);
    // Only the copy a helper keeps through `*objs` is written outside its construction.
    assert_eq!(
        found(&diagnostics(&p, &[])),
        vec![d("pkg/packed.py", 27, "MUT205", "kept_second", "name")]
    );
}

// A copy handed by keyword after `*` unpacking: the keyword binds its parameter exactly, so the
// helper keeps the copy only by keeping that parameter; a position after the unpacking may bind
// any parameter.
const AFTER_UNPACKING: &str = r#"import copy

REGISTRY: list[object] = []


def _show(*rest: object, label: object = None) -> None:
    print(label)


def _keep_label(*rest: object, label: object = None) -> None:
    REGISTRY.append(label)


def shown(obj, extra: list[object]) -> None:
    dup = copy.copy(obj)
    _show(*extra, label=dup)
    dup.name = "x"


def kept(obj, extra: list[object]) -> None:
    dup = copy.copy(obj)
    _keep_label(*extra, label=dup)
    dup.name = "x"


def positional(obj, extra: list[object]) -> None:
    dup = copy.copy(obj)
    _show(*extra, dup)
    dup.name = "x"
"#;

#[test]
fn construction_through_a_keyword_after_unpacking() {
    let p = Project::new("fresh_keyword_after_unpacking");
    p.write(
        "pyproject.toml",
        r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT201", "MUT205"]

[[tool.heki.write-owners]]
name = "store"
targets = ["DataModelFieldBase.name"]
allowed-writers = ["pkg.store.*"]
unresolved = "report"
"#,
    )
    .write("pkg/__init__.py", "")
    .write("pkg/model.py", MODEL)
    .write("pkg/after.py", AFTER_UNPACKING);
    assert_eq!(
        found(&diagnostics(&p, &[])),
        vec![
            d("pkg/after.py", 23, "MUT205", "kept", "name"),
            d("pkg/after.py", 29, "MUT205", "positional", "name"),
        ]
    );
}

// ================================================== strict mode and entries: more shapes

const STRICT_SHAPES: &str = r#"from pkg.model import DataModel, DataModelFieldBase


class Tree:
    def __init__(self) -> None:
        self.levels: dict[str, dict[str, list[str]]] = {}


class Holder:
    def __init__(self) -> None:
        self.items: list[int] = []
        self.name = ""

    def reset(self) -> None:
        self.__dict__.clear()


def wipe(field: DataModelFieldBase) -> None:
    field.__dict__.clear()


def _stamp(obj) -> None:
    obj.name = "x"


def stamps(a: object, b: object, flag: bool) -> None:
    _stamp(a if flag else b)


def entries(model: DataModel, left: DataModel, right: DataModel, flag: bool, name: str) -> None:
    model.meta["kw"][name] = "v"
    src = left.meta["kw"] if flag else right.meta["kw"]
    src[name] = "v"
    if flag:
        either = model.meta["a"]
    else:
        either = model.meta["b"]
    either[name] = "v"
    mixed = model.meta["a"] if flag else {}
    mixed[name] = "v"
    other = model.meta["a"] if flag else model.notes["a"]
    other[name] = "v"


def deep(tree: Tree, names: list[str]) -> None:
    tree.levels["a"]["b"].append(names[0])


def _label(obj) -> None:
    obj.name = "x"


def label_all(options: dict[str, object]) -> None:
    _label(**options)
"#;

/// `def <name>(obj) -> None:` binding `obj.items` to `c0`, then `c1 = c0` ... and appending.
fn container_through(name: &str, copies: usize) -> String {
    let mut text = format!("\n\ndef {name}(obj) -> None:\n    c0 = obj.items\n");
    for i in 1..=copies {
        text.push_str(&format!("    c{i} = c{}\n", i - 1));
    }
    text.push_str(&format!("    c{copies}.append(1)\n"));
    text
}

#[test]
fn strict_mode_and_entries_through_clears_phis_and_nested_loads() {
    let p = Project::new("strict_more_shapes");
    let ops = format!(
        "{STRICT_SHAPES}{}{}",
        container_through("fourteen", 14),
        container_through("fifteen", 15)
    );
    p.write(
        "pyproject.toml",
        r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT201", "MUT205", "EFF201"]

[[tool.heki.write-owners]]
name = "store"
targets = ["DataModelFieldBase.name", "DataModel.meta", "Tree.levels", "Holder.items"]
allowed-writers = ["pkg.store.*"]
unresolved = "report"

[[tool.heki.effect-boundaries]]
name = "pass"
scopes = ["pkg.ops.entries"]
allow-writes = []
write-universe = ['DataModel.meta["kw"]']
"#,
    )
    .write("pkg/__init__.py", "")
    .write("pkg/model.py", MODEL)
    .write("pkg/ops.py", &ops);
    let o = "pkg/ops.py";
    let meta = "DataModel.meta";
    let diags = diagnostics(&p, &[]);
    assert_eq!(
        found(&diags),
        vec![
            // `__dict__.clear()` removes attributes heki cannot name, on `self` too.
            d(o, 15, "MUT205", "Holder.reset", "Holder"),
            d(o, 19, "MUT205", "wipe", "DataModelFieldBase"),
            // A caller passes one of two parameters: of unknown class there.
            d(o, 23, "MUT205", "_stamp", "name"),
            // The entry the pass does not declare, written twice (also through a phi of two
            // objects' entries).
            d(o, 31, "EFF201", "entries", "pass:DataModel.meta[\"kw\"]"),
            d(o, 31, "MUT201", "entries", meta),
            d(o, 33, "MUT201", "entries", meta),
            // A phi of two entries of one container: an entry heki cannot name. A phi with a
            // new dictionary (line 40) or with an entry of another container (line 42) is no
            // entry of it, as for the containers themselves.
            d(o, 38, "MUT201", "entries", meta),
            // An entry of an entry.
            d(o, 46, "MUT201", "deep", "Tree.levels"),
            // A caller passes only `**options`: no argument binds the parameter.
            d(o, 50, "MUT205", "_label", "name"),
            // The container of an object of unknown class through fourteen copies, not fifteen.
            d(o, 73, "MUT205", "fourteen", "items"),
        ]
    );
    let pass = diags.iter().find(|x| x["rule_code"] == "EFF201").unwrap();
    assert_eq!(pass["sites"].as_u64(), Some(2));
}

// ==================================================== strict reads and calls (MUT206, MUT207)

const STRICT_ACCESS_MODEL: &str = r#"class Reference:
    def __init__(self) -> None:
        self.children: list["Reference"] = []


class DataType:
    def __init__(self) -> None:
        self.ref: Reference | None = None

    def replace_reference(self, ref: Reference) -> None:
        self.ref = ref


class Other:
    def __init__(self) -> None:
        self.children: list[str] = []

    def replace_reference(self, ref: Reference) -> None:
        pass
"#;

const STRICT_ACCESS_USE: &str = r#"from typing import Any, Protocol

from pkg.model import DataType, Other, Reference


class Opaque(Protocol):
    pass


def kids(ref):
    return ref.children


def kids_any(ref: Any):
    return getattr(ref, "children"), ref.__dict__["children"]


def kids_typed(ref: Reference):
    return ref.children


def kids_of_other(obj):
    return obj.children


def call_kids_of_other(o: Other) -> None:
    kids_of_other(o)


def kids_handed(ref):
    return ref.children


def call_kids_handed(r: Reference) -> None:
    kids_handed(r)


def kids_opaque(ref: Opaque):
    return ref.children


def swap(dt, ref: Reference) -> None:
    dt.replace_reference(ref)


def swap_alias(dt, ref: Reference) -> None:
    replace = dt.replace_reference
    replace(ref)


def swap_getattr(dt, ref: Reference) -> None:
    getattr(dt, "replace_reference")(ref)


def swap_typed(dt: DataType, ref: Reference) -> None:
    dt.replace_reference(ref)


def swap_other(o: Other, ref: Reference) -> None:
    o.replace_reference(ref)


def call_unknown(callback) -> None:
    callback()


def narrowed(obj: object, ref: Reference) -> None:
    if isinstance(obj, DataType):
        obj.replace_reference(ref)


def kids_of_each(refs):
    out = []
    for ref in refs:
        out.append(ref.children)
    return out
"#;

const STRICT_ACCESS_STORE: &str = r#"class GenerationStore:
    def swap(self, dt, ref) -> None:
        dt.replace_reference(ref)

    def kids(self, ref):
        return ref.children
"#;

fn strict_access_project(name: &str, mode: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!(
            r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT203", "MUT204", "MUT206", "MUT207"]

[[tool.heki.read-owners]]
name = "children"
targets = ["Reference.children"]
allowed-readers = ["pkg.store.*"]
{mode}
[[tool.heki.call-owners]]
name = "mutators"
targets = ["DataType.replace_reference"]
allowed-callers = ["pkg.store.*"]
{mode}"#
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/model.py", STRICT_ACCESS_MODEL)
    .write("pkg/use.py", STRICT_ACCESS_USE)
    .write("pkg/store.py", STRICT_ACCESS_STORE);
    p
}

#[test]
fn strict_modes_report_the_reads_and_calls_heki_cannot_tie_to_a_class() {
    let u = "pkg/use.py";
    let typed = vec![
        d(u, 19, "MUT203", "kids_typed", "Reference.children"),
        d(u, 56, "MUT204", "swap_typed", "DataType.replace_reference"),
        d(u, 69, "MUT204", "narrowed", "DataType.replace_reference"),
    ];
    // Off by default: the typed rules alone.
    let p = strict_access_project("strict_access_off", "");
    assert_eq!(found(&diagnostics(&p, &[])), typed);
    let p = strict_access_project("strict_access_on", "unresolved = \"report\"\n");
    let diags = diagnostics(&p, &[]);
    assert_eq!(
        found(&diags),
        vec![
            // An unannotated parameter, `Any` (also through `getattr` and `__dict__`).
            d(u, 11, "MUT206", "kids", "children"),
            d(u, 15, "MUT206", "kids_any", "children"),
            d(u, 15, "MUT206", "kids_any", "children"),
            typed[0].clone(),
            // A parameter its only caller binds to a `Reference`: no other rule sees that read.
            // One bound to an `Other` reads no target.
            d(u, 31, "MUT206", "kids_handed", "children"),
            // A protocol without members.
            d(u, 39, "MUT206", "kids_opaque", "children"),
            // A method call, a bound-method alias, `getattr`; not a typed call (`MUT204`), a
            // method of another class, a value of unknown origin, or a narrowed receiver.
            d(u, 43, "MUT207", "swap", "replace_reference"),
            d(u, 48, "MUT207", "swap_alias", "replace_reference"),
            d(u, 52, "MUT207", "swap_getattr", "replace_reference"),
            typed[1].clone(),
            typed[2].clone(),
            // A loop variable, which no caller binds.
            d(u, 75, "MUT206", "kids_of_each", "children"),
        ]
    );
    assert_eq!(
        message_at(&diags, u, 11),
        "`kids` reads `children` (by attribute) on an object of unknown class outside the read owners of `children`: heki cannot tell whether it reads `Reference.children`"
    );
    assert_eq!(
        message_at(&diags, u, 43),
        "`swap` calls `replace_reference` on an object of unknown class outside the call owners of `mutators`: heki cannot tell whether it calls `DataType.replace_reference`"
    );
    // The rules can be ignored like any other, and run without the typed rules.
    assert_eq!(
        found(&diagnostics(&p, &["--ignore", "MUT206,MUT207"])),
        typed
    );
    let strict: Vec<_> = found(&diags)
        .into_iter()
        .filter(|x| x.2 == "MUT206" || x.2 == "MUT207")
        .collect();
    assert_eq!(
        found(&diagnostics(&p, &["--ignore", "MUT203,MUT204"])),
        strict
    );
}

#[test]
fn strict_reads_and_calls_honor_the_contract_scopes_and_values() {
    let p = strict_access_project("strict_access_scopes", "unresolved = \"report\"\n");
    // `forbidden-*`: only the functions they name report.
    p.write(
        "pyproject.toml",
        &p.read("pyproject.toml").replace(
            "allowed-readers = [\"pkg.store.*\"]\n",
            "allowed-readers = [\"pkg.store.*\"]\nforbidden-readers = [\"pkg.use.kids_any\"]\n",
        ),
    );
    let reads: Vec<_> = found(&diagnostics(&p, &[]))
        .into_iter()
        .filter(|x| x.2 == "MUT206")
        .collect();
    assert_eq!(
        reads,
        vec![
            d("pkg/use.py", 15, "MUT206", "kids_any", "children"),
            d("pkg/use.py", 15, "MUT206", "kids_any", "children"),
        ]
    );
    let strict = p.read("pyproject.toml");
    for (section, contract) in [("read-owners", "children"), ("call-owners", "mutators")] {
        let at = strict.find(&format!("name = \"{contract}\"")).unwrap();
        let (head, tail) = strict.split_at(at);
        let config = format!(
            "{head}{}",
            tail.replacen("unresolved = \"report\"", "unresolved = \"maybe\"", 1)
        );
        p.write("pyproject.toml", &config);
        let (code, stdout, stderr) = p.check(&[]);
        assert_eq!(code, 2, "{stdout}{stderr}");
        assert!(
            stderr.contains(&format!(
                "unknown `unresolved` value `maybe` in {section} `{contract}` (expected `ignore` or `report`)"
            )),
            "{stderr}"
        );
    }
}

const DOTLESS_MODEL: &str = r#"import codecs


class Model:
    def __init__(self) -> None:
        self.reset()

    def reset(self) -> None:
        pass

    def rewind(self) -> None:
        pass


class SubModel(Model):
    def __init__(self) -> None:
        super().__init__()
        self.reset()


class Decoder(codecs.IncrementalDecoder):
    pass
"#;

const DOTLESS_USE: &str = r#"from pkg.model import Decoder, Model, SubModel


def redo(obj) -> None:
    obj.reset()


def redo_model(model: Model) -> None:
    redo(model)


def redo_sub(obj) -> None:
    obj.reset()


def redo_submodel(sub: SubModel) -> None:
    redo_sub(sub)


def redo_decoder(obj) -> None:
    obj.reset()


def decode(decoder: Decoder) -> None:
    redo_decoder(decoder)


def rewind_any(obj) -> None:
    obj.rewind()


def rewind_model(model: Model) -> None:
    rewind_any(model)


def typed(model: Model) -> None:
    model.reset()
"#;

/// A call target naming no class (`reset`) names the methods of that name: an instance of a
/// class whose MRO finds such a method calls the target, so a helper calling it on an object
/// its callers bind to one is reported (`MUT207`), and an initializer of such a class calling
/// it on its own `self` initializes itself (`MUT204`). A `reset` inherited from a library base
/// is none, and neither is a method a target naming a function (`util.rewind`) does not name.
#[test]
fn a_call_target_naming_no_class_covers_the_classes_defining_the_method() {
    let p = Project::new("strict_dotless_call_target");
    p.write(
        "pyproject.toml",
        r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT204", "MUT207"]

[[tool.heki.call-owners]]
name = "resets"
targets = ["reset"]
unresolved = "report"

[[tool.heki.call-owners]]
name = "rewinds"
targets = ["util.rewind"]
unresolved = "report"
"#,
    )
    .write("pkg/__init__.py", "")
    .write("pkg/model.py", DOTLESS_MODEL)
    .write("pkg/util.py", "def rewind() -> None:\n    pass\n")
    .write("pkg/use.py", DOTLESS_USE);
    let u = "pkg/use.py";
    assert_eq!(
        found(&diagnostics(&p, &[])),
        vec![
            d(u, 5, "MUT207", "redo", "reset"),
            d(u, 13, "MUT207", "redo_sub", "reset"),
            d(u, 37, "MUT204", "typed", "reset"),
        ]
    );
}

/// One module per use of the dict of module globals, each binding `KEY: Final = "_hidden"` and
/// writing `node.__dict__[KEY]` (line 9): `(module, the body of `use`, module-level lines)`.
const GLOBALS_USES: &[(&str, &str, &str)] = &[
    ("read_get", "    return globals().get(name)", ""),
    ("read_item", "    return globals()[name]", ""),
    ("read_in", "    return name in globals()", ""),
    (
        "read_loop",
        "    for key in globals():\n        print(key)\n    return None",
        "",
    ),
    ("read_len", "    return len(globals())", ""),
    (
        "read_alias",
        "    found = globals()\n    return found.get(name)",
        "",
    ),
    ("read_items", "    return sorted(globals().items())", ""),
    ("read_is", "    return globals() is value", ""),
    (
        "read_self_compared",
        "    return globals() == globals()",
        "",
    ),
    (
        "read_module_level",
        "    return None",
        "SIZE = len(globals())\n",
    ),
    (
        "read_local_elsewhere",
        "    return len(globals())",
        "def measure(len: int) -> int:\n    return len\n",
    ),
    (
        "read_class_attribute",
        "    return None",
        "class Sized:\n    len = staticmethod(register)\n\n    \
         def size(self) -> int:\n        return len(globals())\n",
    ),
    (
        "write_item",
        "    globals()[name] = value\n    return None",
        "",
    ),
    (
        "write_update",
        "    globals().update({name: value})\n    return None",
        "",
    ),
    ("write_del", "    del globals()[name]\n    return None", ""),
    (
        "write_setdefault",
        "    return globals().setdefault(name, value)",
        "",
    ),
    (
        "write_handed",
        "    register(globals())\n    return None",
        "",
    ),
    ("write_returned", "    return globals()", ""),
    (
        "write_builtin",
        "    fetch = globals\n    fetch()[name] = value\n    return None",
        "",
    ),
    (
        "write_alias",
        "    found = globals()\n    found[name] = value\n    return None",
        "",
    ),
    (
        "write_own_len",
        "    return len(globals())",
        "def len(ns: dict) -> int:\n    ns[\"KEY\"] = \"other\"\n    return 0\n",
    ),
    (
        "write_imported_len",
        "    return len(globals())",
        "from pkg.writer import len\n",
    ),
    (
        "write_parameter_len",
        "    def apply(len):\n        return len(globals())\n\n    return apply(register)",
        "",
    ),
    (
        "write_local_len",
        "    len = register\n    return len(globals())",
        "",
    ),
    ("write_index", "    return value[globals()]", ""),
    ("write_contained", "    return globals() in value", ""),
    ("write_compared", "    return value == globals()", ""),
    ("write_sentinel", "    return iter(register, globals())", ""),
    (
        "write_own_globals",
        "    globals()[name] = value\n    return None",
        "def globals() -> dict:\n    return {}\n",
    ),
];

/// A project whose write owners own `pkg.model.Node._hidden`, with `pkg/writer.py` defining a
/// `len` that stores into the dict it receives.
fn globals_project(name: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\", \"MUT205\"]\n\n\
         [[tool.heki.write-owners]]\nname = \"c\"\ntargets = [\"pkg.model.Node._hidden\"]\n\
         allowed-writers = [\"pkg.model.Store.*\"]\nunresolved = \"report\"\n",
    )
    .write("pkg/__init__.py", "")
    .write(
        "pkg/model.py",
        "class Node:\n    def __init__(self) -> None:\n        self.kind = \"a\"\n",
    )
    .write(
        "pkg/writer.py",
        "def len(ns: dict) -> int:\n    ns[\"KEY\"] = \"other\"\n    return 0\n",
    );
    p
}

/// Reading the dict of module globals (`globals().get(name)`, `globals()[name]`, `name in
/// globals()`, iterating it, `len(globals())` with the real builtin) binds no name, so the
/// module's constants still name keys; storing into it, deleting from it, a method changing it,
/// or handing it on may bind any name, even one declared `Final`. Handing it on includes the
/// builtin `globals` itself, a call of a reading builtin's name that the module, an import, a
/// parameter, or a local rebinds (`def len(ns)`; a class attribute does not rebind it for the
/// methods), and a place where another object's method receives the dict: an index
/// (`obj[globals()]`), a container tested (`globals() in obj`), a comparison (`obj ==
/// globals()`), the sentinel of `iter`. A call of a function the module names `globals` may
/// return the dict too.
#[test]
fn only_a_change_through_the_dict_of_module_globals_unfixes_its_constants() {
    let p = globals_project("keys_globals_uses");
    for (module, body, tail) in GLOBALS_USES {
        p.write(
            &format!("pkg/{module}.py"),
            &format!(
                "from typing import Final\n\nfrom pkg.model import Node\n\nKEY: Final = \"_hidden\"\n\n\n\
                 def write(node: Node) -> None:\n    node.__dict__[KEY] = 1\n\n\n\
                 def register(found: object) -> None:\n    pass\n\n\n\
                 def use(name: str, value: object) -> object:\n{body}\n\n\n{tail}"
            ),
        );
    }
    let codes: Vec<(String, String)> = found(&diagnostics(&p, &[]))
        .into_iter()
        .filter(|x| x.1 == 9)
        .map(|x| (x.0, x.2))
        .collect();
    let expected: Vec<(String, String)> = GLOBALS_USES
        .iter()
        .map(|(module, _, _)| {
            let code = if module.starts_with("read_") {
                "MUT201"
            } else {
                "MUT205"
            };
            (format!("pkg/{module}.py"), code.to_owned())
        })
        .collect();
    let mut sorted = expected.clone();
    sorted.sort();
    assert_eq!(codes, sorted);
}

// ================================================================ an object's namespace

const NAMESPACE_MODEL: &str = r#"CACHE_KEY = "_cache_key"


class Field:
    def __init__(self) -> None:
        self.required = False


class Other:
    def __init__(self) -> None:
        self.required = False
"#;

const NAMESPACE_USE: &str = r#"from collections.abc import Mapping

from pkg.model import CACHE_KEY, Field, Other


def through_vars(field: Field) -> None:
    vars(field)[CACHE_KEY] = 1


def update_through_vars(field: Field) -> None:
    vars(field).update(required=True)


def in_place_namespace(field: Field) -> None:
    field.__dict__ |= {"required": True}


def in_place_alias(field: Field) -> None:
    values = field.__dict__
    values |= {"required": True}


def control_subscript(field: Field) -> None:
    field.__dict__[CACHE_KEY] = 1


def control_update(field: Field) -> None:
    field.__dict__.update(required=True)


def setdefault_vars(field: Field) -> None:
    vars(field).setdefault("required", True)


def delete_vars(field: Field) -> None:
    del vars(field)["required"]


def alias_vars(field: Field, other: Mapping[str, object]) -> None:
    ns = vars(field)
    ns["required"] = True
    ns |= other


def another_class(other: Other) -> None:
    vars(other)["required"] = True
    other.__dict__ |= {"required": True}


def reads(field: Field) -> object:
    return vars(field)["required"], vars(field).get("required")


def plain_dict(d: dict[str, object]) -> None:
    d |= {"required": True}


def fresh() -> Field:
    made = Field()
    vars(made)["required"] = True
    return made
"#;

const NAMESPACE_SHADOWED: &str = r#"from pkg.model import Field


def vars(obj: object) -> dict[str, object]:
    return {}


def shadowed(field: Field) -> None:
    vars(field)["required"] = True
"#;

/// `vars(obj)` (the builtin) is `obj.__dict__`, and `|=` updates either in place: an entry
/// stored, deleted, or set by a container method through it is a write of the attribute it
/// names, and `update` and `|=` write names heki cannot tell (`MUT205`). A read through it is a
/// read (`MUT203`); a name shadowing the builtin, another class, a plain dict, and an object the
/// function constructs (nothing holds it: `vars` only reads it) are not writes of a target.
#[test]
fn vars_is_the_namespace_and_in_place_or_writes_through_it() {
    let p = Project::new("namespace_forms");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\", \"MUT205\", \"MUT203\"]\n\n\
         [[tool.heki.write-owners]]\nname = \"store\"\n\
         targets = [\"pkg.model.Field.required\", \"pkg.model.Field._cache_key\"]\n\
         allowed-writers = [\"pkg.store:*\"]\nunresolved = \"report\"\n\n\
         [[tool.heki.read-owners]]\nname = \"reads\"\ntargets = [\"pkg.model.Field.required\"]\n\
         allowed-readers = [\"pkg.store:*\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write("pkg/model.py", NAMESPACE_MODEL)
    .write("pkg/use.py", NAMESPACE_USE)
    .write("pkg/shadowed.py", NAMESPACE_SHADOWED);
    let u = "pkg/use.py";
    let (cache, required, field) = (
        "pkg.model.Field._cache_key",
        "pkg.model.Field.required",
        "Field",
    );
    let diags = diagnostics(&p, &[]);
    assert_eq!(
        found(&diags),
        vec![
            d(u, 7, "MUT201", "through_vars", cache),
            d(u, 11, "MUT205", "update_through_vars", field),
            d(u, 15, "MUT205", "in_place_namespace", field),
            d(u, 20, "MUT205", "in_place_alias", field),
            d(u, 24, "MUT201", "control_subscript", cache),
            d(u, 28, "MUT205", "control_update", field),
            d(u, 32, "MUT201", "setdefault_vars", required),
            d(u, 36, "MUT201", "delete_vars", required),
            d(u, 41, "MUT201", "alias_vars", required),
            d(u, 42, "MUT205", "alias_vars", field),
            d(u, 51, "MUT203", "reads", required),
            d(u, 51, "MUT203", "reads", required),
        ]
    );
    assert!(
        message_at(&diags, u, 15).contains(
            "`in_place_namespace` writes an attribute named at run time (as a `__dict__` item) on `Field`"
        ),
        "{diags:#?}"
    );
}

// ======================================================== `__dict__` keys no target names

const UNLISTED_MODEL: &str = r#"CACHE_KEY = "_cache_key"


class Base:
    def __init__(self) -> None:
        self.kind = ""


class Field(Base):
    def __init__(self) -> None:
        super().__init__()
        self.extras: dict[str, object] = {}
        self.__dict__["_init_cache"] = {}


class Sub(Field):
    pass


class Other:
    def __init__(self) -> None:
        self.kind = ""
"#;

const UNLISTED_USE: &str = r#"from pkg.model import CACHE_KEY, Base, Field, Other, Sub

OTHER_KEY = "_type_cache"


def listed(field: Field) -> None:
    field.__dict__[CACHE_KEY] = 1


def unlisted(field: Field) -> None:
    field.__dict__[OTHER_KEY] = 1


def unlisted_literal(field: Field) -> None:
    field.__dict__.setdefault("_name_cache", "x")


def through_vars(field: Field) -> None:
    del vars(field)["_gone"]


def on_a_subclass(sub: Sub) -> None:
    sub.__dict__.pop("_sub_cache", None)


def container_of_an_entry_target(field: Field) -> None:
    field.__dict__["extras"] = {}


def on_a_base(base: Base) -> None:
    base.__dict__["_base_cache"] = 1


def on_another_class(other: Other) -> None:
    other.__dict__["_other_cache"] = 1


def computed(field: Field, name: str) -> None:
    field.__dict__[name] = 1


def plain_attributes(field: Field) -> None:
    field._plain = 1
    setattr(field, "_named", 1)
"#;

fn unlisted_project(name: &str, mode: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\", \"MUT205\"]\n\n\
             [[tool.heki.write-owners]]\nname = \"caches\"\n\
             targets = [\"pkg.model.Field._cache_key\", 'pkg.model.Field.extras[\"init\"]']\n\
             allowed-writers = [\"pkg.store.*\"]\n{mode}"
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/model.py", UNLISTED_MODEL)
    .write("pkg/use.py", UNLISTED_USE)
    .write(
        "pkg/store.py",
        "from pkg.model import Field\n\n\ndef save(field: Field) -> None:\n    field.__dict__[\"_stored\"] = 1\n",
    );
    p
}

/// With `unlisted-dict-keys = "report"`, a constant key written through the namespace of an
/// object of a target's class (or a subclass) that no target covering it names is a `MUT201`
/// write, under the contract's allowance. A key a target names (an entry target names its
/// container), a base class the targets do not cover, another class, a computed key, plain
/// attribute stores, the class's own initialization, and the allowed writers are not.
#[test]
fn unlisted_dict_keys_reports_the_namespace_keys_no_target_names() {
    let u = "pkg/use.py";
    let listed = d(u, 7, "MUT201", "listed", "pkg.model.Field._cache_key");
    let p = unlisted_project("unlisted_keys_off", "");
    assert_eq!(found(&diagnostics(&p, &[])), vec![listed.clone()]);
    let p = unlisted_project("unlisted_keys_on", "unlisted-dict-keys = \"report\"\n");
    let diags = diagnostics(&p, &[]);
    assert_eq!(
        found(&diags),
        vec![
            listed,
            d(u, 11, "MUT201", "unlisted", "Field._type_cache"),
            d(u, 15, "MUT201", "unlisted_literal", "Field._name_cache"),
            d(u, 19, "MUT201", "through_vars", "Field._gone"),
            d(u, 23, "MUT201", "on_a_subclass", "Sub._sub_cache"),
        ]
    );
    assert_eq!(
        message_at(&diags, u, 11),
        "`unlisted` writes `pkg.model.Field._type_cache` (as a `__dict__` item), a `__dict__` key no target names, outside the write owners of `caches`"
    );
    // The same writes judged lexically.
    let p = unlisted_project(
        "unlisted_keys_lexical",
        "unlisted-dict-keys = \"report\"\nattribution = \"lexical\"\n",
    );
    assert_eq!(found(&diagnostics(&p, &[])).len(), 5);
    let p = unlisted_project("unlisted_keys_bad", "unlisted-dict-keys = \"all\"\n");
    let (code, stdout, stderr) = p.check(&[]);
    assert_eq!(code, 2, "{stdout}{stderr}");
    assert!(
        stderr.contains(
            "unknown `unlisted-dict-keys` value `all` in write-owners `caches` (expected `ignore` or `report`)"
        ),
        "{stderr}"
    );
}

const LISTED_ELSEWHERE_MODEL: &str = r#"from __future__ import annotations

CACHE_KEY = "_cache_key"


class Field:
    def __init__(self) -> None:
        self.required = False


class Sub(Field):
    pass
"#;

const LISTED_ELSEWHERE_PARSER: &str = r#"from __future__ import annotations

from pkg.model import CACHE_KEY, Field, Sub

OTHER_KEY = "_type_cache"


def listed(field: Field) -> None:
    field.__dict__[CACHE_KEY] = 1


def unlisted(field: Field) -> None:
    field.__dict__[OTHER_KEY] = 1


def unlisted_literal(field: Field) -> None:
    field.__dict__.setdefault("_name_cache", "x")


def named_for_the_subclass(sub: Sub) -> None:
    sub.__dict__["_sub_cache"] = 1


def subclass_key_on_the_base(field: Field) -> None:
    field.__dict__["_sub_cache"] = 1


def bookkeeping(field: Field) -> None:
    field.__dict__.setdefault("_derived_from", None)


def read_target(field: Field) -> None:
    field.__dict__["_read_cache"] = 1
"#;

/// A `write-owners` contract `name` guarding `target`, allowing the writers `allowed` and judging
/// writes lexically, with `keys` added.
fn write_owner(name: &str, target: &str, allowed: &str, keys: &str) -> String {
    format!(
        "[[tool.heki.write-owners]]\nname = \"{name}\"\ntargets = [\"{target}\"]\n\
         allowed-writers = [\"{allowed}\"]\nattribution = \"lexical\"\n{keys}\n"
    )
}

/// `unlisted-dict-keys = "report"` counts a key as listed when a target of any `write-owners`
/// contract covering the written object's class names it: a key another contract guards is that
/// contract's write only, in both directions. A target on a subclass names the key for the
/// subclass, not for its base; a `read-owners` target lists nothing; and a contract allowing
/// every writer lists keys any function may write.
#[test]
fn unlisted_dict_keys_count_the_keys_every_write_owners_contract_names() {
    let (store, unlisted) = ("pkg.store:*", "unlisted-dict-keys = \"report\"\n");
    let p = Project::new("unlisted_keys_listed_elsewhere");
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\", \"MUT205\"]\n\n{}{}{}{}\
             [[tool.heki.read-owners]]\nname = \"reads\"\ntargets = [\"pkg.model.Field._read_cache\"]\n\
             allowed-readers = [\"pkg.store:*\"]\n",
            write_owner("caches", "pkg.model.Field._cache_key", store, unlisted),
            write_owner("other", "pkg.model.Field._type_cache", store, unlisted),
            write_owner("subclass", "pkg.model.Sub._sub_cache", store, ""),
            write_owner("bookkeeping", "pkg.model.Field._derived_*", "*", ""),
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/model.py", LISTED_ELSEWHERE_MODEL)
    .write("pkg/parser.py", LISTED_ELSEWHERE_PARSER)
    .write(
        "pkg/store.py",
        "from pkg.model import Field\n\n\ndef save(field: Field) -> None:\n    field.__dict__[\"_type_cache\"] = 1\n    field.__dict__[\"_stored\"] = 1\n",
    );
    let u = "pkg/parser.py";
    let diags = diagnostics(&p, &[]);
    assert_eq!(
        found(&diags),
        vec![
            d(u, 9, "MUT201", "listed", "pkg.model.Field._cache_key"),
            d(u, 13, "MUT201", "unlisted", "pkg.model.Field._type_cache"),
            d(u, 17, "MUT201", "unlisted_literal", "Field._name_cache"),
            d(
                u,
                21,
                "MUT201",
                "named_for_the_subclass",
                "pkg.model.Sub._sub_cache",
            ),
            d(
                u,
                25,
                "MUT201",
                "subclass_key_on_the_base",
                "Field._sub_cache",
            ),
            d(u, 33, "MUT201", "read_target", "Field._read_cache"),
        ]
    );
    assert_eq!(
        message_at(&diags, u, 13),
        "`unlisted` writes `pkg.model.Field._type_cache` (as a `__dict__` item) outside the write owners of `other`"
    );
    assert_eq!(
        message_at(&diags, u, 17),
        "`unlisted_literal` writes `pkg.model.Field._name_cache` (as a `__dict__` item), a `__dict__` key no target names, outside the write owners of `caches`"
    );
}

/// A star import binds the names its module exports, which heki does not read, so it may bind
/// the name of any builtin: `len(globals())` may hand the dict to another `len` (the one
/// `pkg.writer` defines), which may bind a name declared `Final`. That holds for a star import
/// of a module that binds no `len` too (`pkg.helpers`), and wherever the import stands: a
/// function runs after it (`_KEY`, a private name the import does not rebind, bound before an
/// import at the end of the module). Without a star import the key stays a constant.
#[test]
fn a_star_import_may_bind_the_name_of_a_reading_builtin() {
    let p = globals_project("keys_globals_star_import");
    p.write("pkg/helpers.py", "def helper() -> None:\n    pass\n");
    let modules = [
        ("no_star", "", "KEY", ""),
        ("star_writer", "from pkg.writer import *", "KEY", ""),
        ("star_helpers", "from pkg.helpers import *", "KEY", ""),
        ("star_last", "", "_KEY", "\n\nfrom pkg.writer import *\n"),
    ];
    for (module, head, key, tail) in modules {
        p.write(
            &format!("pkg/{module}.py"),
            &format!(
                "from typing import Final\n\nfrom pkg.model import Node\n{head}\n\n{key}: Final = \"_hidden\"\n\n\n\
                 def write(node: Node) -> None:\n    node.__dict__[{key}] = 1\n\n\n\
                 def use(name: str) -> object:\n    return len(globals())\n{tail}"
            ),
        );
    }
    let codes: Vec<(String, u64, String)> = found(&diagnostics(&p, &[]))
        .into_iter()
        .map(|x| (x.0, x.1, x.2))
        .collect();
    let mut expected: Vec<(String, u64, String)> = modules
        .iter()
        .map(|(module, ..)| {
            let code = if *module == "no_star" {
                "MUT201"
            } else {
                "MUT205"
            };
            (format!("pkg/{module}.py"), 10, code.to_owned())
        })
        .collect();
    expected.sort();
    assert_eq!(codes, expected);
}
