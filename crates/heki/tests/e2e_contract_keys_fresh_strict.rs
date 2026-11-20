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
        message_at(&diags, u, 40).contains("writes `DataModel.meta` (item_mut)"),
        "{diags:#?}"
    );
    assert!(
        message_at(&diags, u, 12).contains("(dict_direct)"),
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
            // A computed name on a guarded class (`setattr`, `__dict__`); not on an object of
            // unknown class.
            d(u, 23, "MUT205", "anything", "DataModel"),
            d(u, 24, "MUT205", "anything", "DataModel"),
            // Typed by its caller: `MUT201` reports the caller.
            typed,
            // Untyped by every caller.
            d(u, 41, "MUT205", "_untyped_helper", "name"),
        ]
    );
    assert_eq!(
        message_at(&diags, u, 8),
        "`touch` writes `fields` (direct_attr) on an object of unknown class outside the write owners of `store`: heki cannot tell whether it writes `DataModel.fields`"
    );
    assert!(
        message_at(&diags, u, 23).contains(
            "writes an attribute named at run time (set_attr) on `DataModel` outside the write owners of `store`: heki cannot tell whether it writes `DataModel.fields` or `DataModel.name`"
        ),
        "{diags:#?}"
    );
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
    assert!(stdout.contains("MUT205 `touch` writes `name`"), "{stdout}");

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
        message_at(&diags, u, 18).contains("writes `meta` (item_mut)"),
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
        "`kids` reads `children` (direct_attr) on an object of unknown class outside the read owners of `children`: heki cannot tell whether it reads `Reference.children`"
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
