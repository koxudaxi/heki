//! Contract entries that match nothing in the analyzed files, end to end through the real
//! binary: what `heki check` warns about (`lint.unmatched-contract-entries`), the names it
//! offers instead, and what counts as matching for each kind of entry.

mod common;

use common::Project;

const REPRO_MODEL: &str = "\
class Field:
    def __init__(self) -> None:
        self.kind = \"str\"

    def replace_type(self, kind: str) -> None:
        self.kind = kind
";

const REPRO_STORE: &str = "\
from pkg.model import Field

TYPE_CACHE_KEY = \"_type_cache\"


def cache_type(field: Field) -> None:
    field.__dict__[TYPE_CACHE_KEY] = field.kind
    field.replace_type(\"int\")
";

/// The project of the request: a `__dict__` key some code writes, one no code writes or reads,
/// an attribute that does not exist, a method that exists, and one that does not; `lint` holds
/// the rest of the `[tool.heki.lint]` table.
fn repro(name: &str, lint: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\", \"MUT204\"]\n{lint}\n\
             [[tool.heki.write-owners]]\nname = \"field-caches\"\n\
             targets = [\"pkg.model.Field._type_cache\", \"pkg.model.Field._gone_cache\", \
             \"pkg.model.Field.renamed_attribute\"]\n\
             allowed-writers = [\"pkg.store:*\"]\nunlisted-dict-keys = \"report\"\n\n\
             [[tool.heki.call-owners]]\nname = \"store-calls\"\n\
             targets = [\"pkg.model.Field.replace_type\", \"pkg.model.Field.removed_method\"]\n\
             allowed-callers = [\"pkg.store:*\"]\n"
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/model.py", REPRO_MODEL)
    .write("pkg/store.py", REPRO_STORE);
    p
}

const REPRO_ENTRIES: [&str; 3] = [
    "`targets` entry `pkg.model.Field._gone_cache` of write-owners `field-caches` names no attribute, method, or `__dict__` key of `pkg.model.Field` in the analyzed files; `pkg.model.Field._type_cache` exists",
    "`targets` entry `pkg.model.Field.renamed_attribute` of write-owners `field-caches` names no attribute, method, or `__dict__` key of `pkg.model.Field` in the analyzed files",
    "`targets` entry `pkg.model.Field.removed_method` of call-owners `store-calls` names no function, method, or class in the analyzed files",
];

/// The lines of `stderr` that start with `prefix`, without it and without the places of the
/// entry and its contracts (`a_warning_names_where_the_entry_and_its_contracts_are`).
fn lines(stderr: &str, prefix: &str) -> Vec<String> {
    stderr
        .lines()
        .filter_map(|l| l.strip_prefix(prefix))
        .map(|l| match (l.find(" (from "), l.find(") ")) {
            (Some(start), Some(end)) if start < end => format!("{}{}", &l[..start], &l[end + 1..]),
            _ => l.to_owned(),
        })
        .collect()
}

/// The lines of `stderr` that start with `prefix`, without it, as written.
fn lines_as_written<'s>(stderr: &'s str, prefix: &str) -> Vec<&'s str> {
    stderr
        .lines()
        .filter_map(|l| l.strip_prefix(prefix))
        .collect()
}

/// A warning names where the entry is written and where its contract is.
#[test]
fn a_warning_names_where_the_entry_and_its_contracts_are() {
    let p = repro("unmatched_places", "");
    let (_, stdout, stderr) = p.heki(&["check"]);
    let warned: Vec<&str> = stderr
        .lines()
        .filter_map(|l| l.strip_prefix("warning: "))
        .collect();
    assert_eq!(
        warned[2],
        "`targets` entry `pkg.model.Field.removed_method` of call-owners `store-calls` (from pyproject.toml:15; contract at pyproject.toml:13) names no function, method, or class in the analyzed files",
        "{stdout}{stderr}"
    );
}

/// A run over the whole project warns about the entries that name nothing, which do not fail
/// it; a `__dict__` key some code writes on an object of the class and a method that exists
/// name something. `"error"` makes them errors that fail the run, and `"ignore"` drops them.
/// The contracts whose rules do not run are not judged.
#[test]
fn a_run_warns_about_the_entries_that_match_nothing() {
    let p = repro("unmatched_default", "");
    let (code, stdout, stderr) = p.heki(&["check"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert_eq!(lines(&stderr, "warning: "), REPRO_ENTRIES, "{stderr}");
    assert!(stderr.contains("All checks passed"), "{stderr}");
    assert!(!stderr.contains("errors above"), "{stderr}");

    let p = repro(
        "unmatched_error",
        "unmatched-contract-entries = \"error\"\n",
    );
    let (code, stdout, stderr) = p.heki(&["check"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert_eq!(lines(&stderr, "error: "), REPRO_ENTRIES, "{stderr}");
    // A run these errors fail does not say that all checks passed.
    assert!(!stderr.contains("All checks passed"), "{stderr}");
    assert!(
        stderr.contains("; the errors above fail the run."),
        "{stderr}"
    );

    let p = repro(
        "unmatched_ignore",
        "unmatched-contract-entries = \"ignore\"\n",
    );
    let (code, stdout, stderr) = p.heki(&["check"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(lines(&stderr, "warning: ").is_empty(), "{stderr}");

    // A contract whose rules do not run is not judged.
    let p = repro("unmatched_rules_off", "");
    let (_, _, stderr) = p.heki(&["check", "--ignore", "MUT201,MUT205"]);
    assert_eq!(lines(&stderr, "warning: "), REPRO_ENTRIES[2..], "{stderr}");

    let p = repro(
        "unmatched_bad_value",
        "unmatched-contract-entries = \"report\"\n",
    );
    let (code, _, stderr) = p.heki(&["check"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("unmatched-contract-entries"), "{stderr}");
}

const REPRO_COPY: &str = "

def copy_field(field: Field) -> Field:
    new = Field()
    for key, value in field.__dict__.items():
        new.__dict__[key] = value
    return new
";

/// A copy of the entries an instance has already (a `__deepcopy__` writing each key of
/// `self.__dict__` into the new object's) adds no attribute: the entries naming nothing are
/// reported as they are without the copy.
#[test]
fn a_copy_of_the_namespace_hides_no_entry() {
    let p = repro(
        "unmatched_namespace_copy",
        "unmatched-contract-entries = \"error\"\n",
    );
    p.write("pkg/store.py", &format!("{REPRO_STORE}{REPRO_COPY}"));
    let (code, stdout, stderr) = p.heki(&["check"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert_eq!(lines(&stderr, "error: "), REPRO_ENTRIES, "{stderr}");
}

const KEYS_MODEL: &str = "\
class Field:
    kind: str = \"str\"

    def __deepcopy__(self, memo: dict) -> \"Field\":
        new = Field()
        for key, value in self.__dict__.items():
            new.__dict__[key] = value
        return new


class Through:
    kind: str = \"str\"


class Base:
    kind: str = \"str\"


class Sub(Base):
    extra: int = 0


class Named:
    kind: str = \"str\"


class Removed:
    kind: str = \"str\"


class Computed:
    kind: str = \"str\"


class Other:
    kind: str = \"str\"


class FromBase:
    kind: str = \"str\"


class Narrow(FromBase):
    extra: int = 0
";

const KEYS_COPIES: &str = "\
from pkg.model import Base, Computed, FromBase, Named, Narrow, Other, Removed, Sub, Through


def keys(new: Through, old: Through) -> None:
    for key in old.__dict__.keys():
        new.__dict__[key] = old.__dict__[key]


def through_vars(new: Through, old: Through) -> None:
    for key in vars(old):
        setattr(new, key, getattr(old, key))


def listed_items(new: Through, old: Through) -> None:
    for key, value in list(vars(old).items()):
        object.__setattr__(new, key, value)


def copied_items(new: Through, old: Through) -> None:
    for key, value in old.__dict__.copy().items():
        new.__dict__.setdefault(key, value)


def updated(new: Through, old: Through) -> None:
    new.__dict__.update(old.__dict__)


def from_a_subclass(new: Base, old: Sub) -> None:
    for key, value in old.__dict__.items():
        new.__dict__[key] = value


def constants(new: Named) -> None:
    for key in (\"_first\", \"_second\"):
        setattr(new, key, None)


def removals(old: Removed, name: str) -> None:
    del old.__dict__[name]
    old.__dict__.pop(name, None)
    old.__dict__.clear()


def computed(new: Computed, name: str) -> None:
    new.__dict__[name] = 1


def from_another_class(new: Computed, old: Other) -> None:
    for key, value in old.__dict__.items():
        new.__dict__[key] = value


def from_a_base(new: Narrow, old: FromBase) -> None:
    for key, value in old.__dict__.items():
        new.__dict__[key] = value
";

/// The classes of [`KEYS_MODEL`].
const KEYS_CLASSES: [&str; 10] = [
    "Field", "Through", "Base", "Sub", "Named", "Removed", "Computed", "Other", "FromBase",
    "Narrow",
];

/// A project whose contract names a stale attribute `gone` of each of `classes`, which `model`
/// defines and `copies` writes, and the attributes `extra`; `lint` holds the rest of the
/// `[tool.heki.lint]` table.
fn stale_project(
    name: &str,
    lint: &str,
    (model, copies): (&str, &str),
    classes: &[&str],
    extra: &[&str],
) -> Project {
    let targets: Vec<String> = (classes.iter().map(|c| format!("pkg.model.{c}.gone")))
        .chain(extra.iter().map(|t| (*t).to_owned()))
        .map(|t| format!("\"{t}\""))
        .collect();
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\"]\n{lint}\n\
             [[tool.heki.write-owners]]\nname = \"stale\"\ntargets = [{}]\n\
             allowed-writers = [\"pkg.*\"]\n",
            targets.join(", ")
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/model.py", model)
    .write("pkg/copies.py", copies);
    p
}

/// The message of the entry naming nothing of `class`, with `suffix`.
fn stale(class: &str, suffix: &str) -> String {
    format!(
        "`targets` entry `pkg.model.{class}.gone` of write-owners `stale` names no attribute, \
         method, or `__dict__` key of `pkg.model.{class}` in the analyzed files{suffix}"
    )
}

/// The entries of a [`stale_project`] naming nothing: under `"warning"` those of the classes
/// outside `named_at_run_time` (the classes the code gives attributes named at run time), under
/// `"error"` every one, those of `named_at_run_time` as ones that may name nothing.
fn assert_stale(
    name: &str,
    files: (&str, &str),
    (classes, extra): (&[&str], &[&str]),
    named_at_run_time: &[&str],
) {
    let p = stale_project(&format!("{name}_warning"), "", files, classes, extra);
    let (code, stdout, stderr) = p.heki(&["check"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    let known = classes.iter().filter(|c| !named_at_run_time.contains(c));
    let expected: Vec<String> = known.map(|c| stale(c, "")).collect();
    assert_eq!(lines(&stderr, "warning: "), expected, "{stderr}");

    let lint = "unmatched-contract-entries = \"error\"\n";
    let p = stale_project(&format!("{name}_error"), lint, files, classes, extra);
    let (code, stdout, stderr) = p.heki(&["check"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let suffix = |c: &str| match named_at_run_time.contains(&c) {
        true => ", unless it is one the code names at run time",
        false => "",
    };
    let expected: Vec<String> = classes.iter().map(|c| stale(c, suffix(c))).collect();
    assert_eq!(lines(&stderr, "error: "), expected, "{stderr}");
}

/// A key a store takes from the namespace of an instance of the class or a subclass names an
/// attribute the instance has already: through `items()`, `keys()`, iterating the namespace or
/// `vars(obj)`, a `list` or a copy of them, and `update`, stored through the namespace,
/// `setattr`, or `object.__setattr__`, also in the class's own module. A key a loop takes from
/// a tuple of string constants names those constants, and a deletion adds no attribute. A key
/// heki cannot trace (a parameter, one of another class's namespace or of a base's) may name
/// anything: the entries naming nothing else of its class family are reported only under
/// `"error"`, as ones that may name nothing.
#[test]
fn only_a_key_heki_cannot_trace_may_name_anything() {
    assert_stale(
        "unmatched_keys",
        (KEYS_MODEL, KEYS_COPIES),
        (
            &KEYS_CLASSES,
            &["pkg.model.Named._first", "pkg.model.Named._second"],
        ),
        &["Computed", "FromBase", "Narrow"],
    );
}

const FIELDS_MODEL: &str = r#"import copy
import dataclasses

import attrs
from pydantic import BaseModel, v1


class DataType(BaseModel):
    kind: str = "str"

    def __deepcopy__(self, memo: dict) -> "DataType":
        new_obj = self.__class__.__new__(self.__class__)
        for field_name in self.__class__.model_fields:
            object.__setattr__(new_obj, field_name, copy.deepcopy(getattr(self, field_name), memo))
        return new_obj


class Leaves(BaseModel):
    kind: str = "str"

    def __deepcopy__(self, memo: dict) -> "Leaves":
        cls = self.__class__
        model_fields = cls.model_fields
        new_obj: Leaves = cls.model_construct()
        for field_name in model_fields:
            object.__setattr__(new_obj, field_name, getattr(self, field_name))
        for key, value in self.__dict__.items():
            if key not in model_fields:
                new_obj.__dict__[key] = value
        return new_obj


class ByType(BaseModel):
    kind: str = "str"


class ByInstance(BaseModel):
    kind: str = "str"


class ByClass(BaseModel):
    kind: str = "str"

    @classmethod
    def blank(cls, old: "ByClass") -> "ByClass":
        new = cls.model_construct()
        for name, info in cls.__pydantic_fields__.items():
            new.__dict__[name] = getattr(old, name)
        return new


class Named(BaseModel):
    kind: str = "str"


class Legacy(v1.BaseModel):
    kind: str = "str"


@dataclasses.dataclass
class Point:
    x: int = 0


@dataclasses.dataclass
class Plain:
    x: int = 0


class SubPlain(Plain):
    y: int = 0


@attrs.define
class Pair:
    a: int = 0


class Target(BaseModel):
    kind: str = "str"


class Source(BaseModel):
    other: str = "str"


class Shadowing(BaseModel):
    kind: str = "str"
    model_fields = {"anything": None}


class Proxied(BaseModel):
    kind: str = "str"

    @property
    def __class__(self):
        return Source
"#;

const FIELDS_COPIES: &str = r#"import dataclasses
from dataclasses import fields

import attrs

from pkg.model import (
    ByInstance, ByType, Legacy, Named, Pair, Plain, Point, Proxied, Shadowing, Source,
    SubPlain, Target,
)


def by_type(new: ByType, old: ByType) -> None:
    for name in type(old).model_fields.keys():
        setattr(new, name, getattr(old, name))


def by_instance(new: ByInstance, old: ByInstance) -> None:
    new.__dict__.update(old.model_fields)


def named(new: Named, old: Named) -> None:
    for name in list(Named.__fields__):
        setattr(new, name, getattr(old, name))


def legacy(new: Legacy, old: Legacy) -> None:
    for name in old.__fields__:
        setattr(new, name, getattr(old, name))


def point(new: Point, old: Point) -> None:
    for field in dataclasses.fields(old):
        setattr(new, field.name, getattr(old, field.name))


def plain(new: Plain, old: Plain | SubPlain) -> None:
    for field in fields(old):
        object.__setattr__(new, field.name, getattr(old, field.name))
    for name in old.__dataclass_fields__:
        new.__dict__[name] = getattr(old, name)


def pair(new: Pair, old: Pair) -> None:
    for a in attrs.fields(type(old)):
        setattr(new, a.name, getattr(old, a.name))
    for a in old.__attrs_attrs__:
        setattr(new, a.name, getattr(old, a.name))


def from_another_class(new: Target, old: Source) -> None:
    for name in Source.model_fields:
        setattr(new, name, getattr(old, name))


def shadowing(new: Shadowing) -> None:
    for name in Shadowing.model_fields:
        setattr(new, name, None)


def proxied(new: Proxied, old: Proxied) -> None:
    for name in old.__class__.model_fields:
        setattr(new, name, None)
"#;

/// The classes of [`FIELDS_MODEL`].
const FIELDS_CLASSES: [&str; 15] = [
    "DataType",
    "Leaves",
    "ByType",
    "ByInstance",
    "ByClass",
    "Named",
    "Legacy",
    "Point",
    "Plain",
    "SubPlain",
    "Pair",
    "Target",
    "Source",
    "Shadowing",
    "Proxied",
];

/// A key taken from the fields a class or a subclass declares names an attribute the class
/// declares, as the catalog keeps them (`[[declared_fields]]`): a pydantic model's
/// `model_fields`, `__pydantic_fields__`, and `__fields__` (also pydantic 1's), read on the
/// class (named, `cls`, `type(obj)`, `obj.__class__`) or an instance, their keys or pairs, and
/// `update` with them; a dataclass's `__dataclass_fields__` and the `name` of each of
/// `dataclasses.fields(obj)`, a subclass's too; an attrs class's `__attrs_attrs__` and
/// `attrs.fields(cls)`. The `__deepcopy__` of the request reports its class's stale entries
/// plainly. Another class's fields, a class binding `model_fields` itself, and a class whose
/// `__class__` may be another class may still give any name.
#[test]
fn a_key_from_the_fields_a_class_declares_names_an_attribute() {
    assert_stale(
        "unmatched_fields",
        (FIELDS_MODEL, FIELDS_COPIES),
        (&FIELDS_CLASSES, &[]),
        &["Target", "Shadowing", "Proxied"],
    );
}

const WRAPPED_MODEL: &str = r#"class Config:
    timeout = 1


class Dicted:
    kind: str = "str"


class DictPairs:
    kind: str = "str"


class Updated:
    kind: str = "str"


class Values:
    kind: str = "str"


class FromClass:
    kind: str = "str"


class Copied:
    kind: str = "str"


class ListCopied:
    kind: str = "str"


class Wrapped:
    kind: str = "str"


class Picked:
    kind: str = "str"
"#;

const WRAPPED_COPIES: &str = r#"from pkg.model import (
    Config, Copied, DictPairs, Dicted, FromClass, ListCopied, Picked, Updated, Values, Wrapped,
)


def dicted(new: Dicted, old: Dicted) -> None:
    for name in dict(vars(old)):
        setattr(new, name, getattr(old, name))


def dict_pairs(new: DictPairs, old: DictPairs) -> None:
    for name in dict(old.__dict__.items()):
        setattr(new, name, getattr(old, name))


def updated(new: Updated, old: Updated) -> None:
    new.__dict__.update(dict(vars(old)))


def values(new: Values, old: Values) -> None:
    for name in vars(old).values():
        setattr(new, name, None)


def from_class(new: FromClass) -> None:
    for name in vars(Config):
        setattr(new, name, getattr(Config, name))


def copied(new: Copied) -> None:
    for name in ["kind"].copy():
        setattr(new, name, None)


def list_copied(new: ListCopied) -> None:
    for name in list(("kind",)).copy():
        setattr(new, name, None)


def wrapped(new: Wrapped, old: Wrapped) -> None:
    for name in list(list(list(list(list(vars(old)))))):
        setattr(new, name, getattr(old, name))


def picked(new: Picked, old: Picked) -> None:
    pair = next(iter(vars(old).items()))
    setattr(new, pair[0], pair[1])
"#;

/// The classes of [`WRAPPED_MODEL`] the contract names.
const WRAPPED_CLASSES: [&str; 9] = [
    "Dicted",
    "DictPairs",
    "Updated",
    "Values",
    "FromClass",
    "Copied",
    "ListCopied",
    "Wrapped",
    "Picked",
];

/// A `dict` of the namespace of an instance or of its pairs gives its keys, to a loop and to
/// `update`. The values of a namespace, the namespace of a class object, a copy of a display of
/// constants or of a `list` of one (only the keys of a namespace or a mapping go through
/// `copy()`), more than four wrappers around a namespace, and the first of a pair no loop takes
/// may give any name.
#[test]
fn a_dict_of_a_namespace_gives_its_keys() {
    assert_stale(
        "unmatched_wrapped",
        (WRAPPED_MODEL, WRAPPED_COPIES),
        (&WRAPPED_CLASSES, &[]),
        &[
            "Values",
            "FromClass",
            "Copied",
            "ListCopied",
            "Wrapped",
            "Picked",
        ],
    );
}

/// Without the key, a run over some files of the project judges nothing: an entry naming code
/// in a file the paths leave out would look unmatched. With the key, the files given are the
/// project. A file given alone is checked in its project, which the run analyzes whole.
#[test]
fn a_run_over_some_files_judges_only_with_the_key() {
    let p = repro("unmatched_some_files", "");
    p.write("pkg/stores/__init__.py", "")
        .write("pkg/stores/store.py", &p.read("pkg/store.py"));
    let (_, _, stderr) = p.heki(&["check", "pkg/stores"]);
    assert!(lines(&stderr, "warning: ").is_empty(), "{stderr}");
    let (_, _, stderr) = p.heki(&["check", "."]);
    assert_eq!(lines(&stderr, "warning: ").len(), 3, "{stderr}");
    let (_, _, stderr) = p.heki(&["check", "pkg/store.py"]);
    assert_eq!(lines(&stderr, "warning: ").len(), 3, "{stderr}");

    let p = repro(
        "unmatched_some_files_set",
        "unmatched-contract-entries = \"warning\"\n",
    );
    p.write("pkg/stores/__init__.py", "")
        .write("pkg/stores/store.py", &p.read("pkg/store.py"));
    let (_, _, stderr) = p.heki(&["check", "pkg/stores"]);
    let warned = lines(&stderr, "warning: ");
    assert!(
        warned.iter().any(|w| w == "`targets` entry `pkg.model.Field.replace_type` of call-owners `store-calls` names no function, method, or class in the analyzed files"),
        "{stderr}"
    );
}

const SHAPES_MODEL: &str = "\
from typing import Protocol


class Base:
    def __init__(self) -> None:
        self.base_attr = 1


class Field(Base):
    kind: str = \"str\"

    def replace_type(self, kind: str) -> None:
        self.kind = kind

    def __deepcopy__(self, memo: dict) -> \"Field\":
        return self


class Dynamic:
    def __init__(self, **values: object) -> None:
        for name, value in values.items():
            setattr(self, name, value)


class Host(Protocol):
    def run(self) -> None: ...


def helper() -> None:
    pass
";

const SHAPES_STORE: &str = "\
from pkg.model import Field

KEY = \"_cache\"


def save(field: Field) -> None:
    field.__dict__[KEY] = 1


def peek(field: Field) -> object:
    return field.__dict__.get(\"_peeked\")


def save_all(fields: list) -> None:
    for field in fields:
        save(field)


def touch(obj) -> None:
    obj.__dict__[\"_touched\"] = 1


def fill(obj, name: str) -> None:
    setattr(obj, name, 1)
";

const SHAPES_CONFIG: &str = r#"[project]
name = "t"

[[tool.heki.write-owners]]
name = "writes"
targets = [
    "pkg.model.Field.kind",
    "pkg.model.Field.base_attr",
    "pkg.model.Field._cache",
    "pkg.model.Field._peeked",
    "pkg.model.Field._touched",
    "pkg.model.Field._*",
    "pkg.model.Dynamic.anything",
    'pkg.model.Field.extras["k"]',
    "pkg.model.F*.kind",
    "pkg.model.*Store._x",
    "pkg.model.Field.gone",
    "pkg.models.Field.kind",
    "pkg.gone.*Store._x",
]
allowed-writers = [
    "pkg.store:*",
    "pkg.store:save",
    "pkg.model:Field",
    "pkg.model:helper.*",
    "pkg.*.helper_*",
    "pkg.store:sav",
    "pkg.gone:*",
    "pkg.*.gone_function",
]
exempt-methods = ["__deepcopy__", "__*__", "__deep_copy__", "Field.__deep_copy__"]
allowed-protocols = ["pkg.model.Host", "pkg.model.Hots"]
forbid-cached-property-on = ["pkg.model.Field", "pkg.model.Gone"]

[[tool.heki.read-owners]]
name = "reads"
targets = ["pkg.model.Field.kind"]
allowed-readers = ["pkg.store:*"]
forbidden-readers = ["pkg.model.*", "pkg.views.*"]

[[tool.heki.call-owners]]
targets = [
    "pkg.model.Field.replace_type",
    "pkg.model.Field",
    "pkg.model:helper(x=1)",
    "pkg.model.Field.removed_method",
]
allowed-callers = ["pkg.store:*"]

[[tool.heki.effect-boundaries]]
name = "pure"
scopes = ["pkg.store", "pkg.render"]
forbid = ["writes-targets"]
write-targets = ["pkg.model.Field.kind", "pkg.model.Field.nope"]

[[tool.heki.invalidation]]
name = "announce"
targets = ["pkg.model.Field.kind"]
invalidators = ["pkg.model.Field.replace_type", "pkg.model.Field.announce"]
exempt-writers = ["pkg.store:save"]

[[tool.heki.write-owners]]
name = "again"
targets = ["pkg.model.Field.gone"]
allowed-writers = ["pkg.store:*"]
"#;

/// Each kind of entry is judged by what its rule matches. Not reported: an attribute the class
/// body declares, one a project base sets on `self`, a constant `__dict__` key the code writes
/// on an object of the class (a key it only reads is reported), a name written on an object heki cannot type, any name
/// of a class that stores attributes named at run time, a glob attribute of a class that exists,
/// a glob class in a module that exists, a function, a class, the functions a glob names inside
/// a function that exists, a glob held by a glob, a call target with an argument list, a module
/// scope, and an exempt method or method glob. A name computed at run time on an object heki
/// cannot type gives no class any name. Reported once per
/// key and text, naming every contract that holds it (an unnamed one by its position), with the
/// names one part of the entry away: the same holder and a like last part, or the same last part
/// and a like holder.
#[test]
fn each_kind_of_entry_is_judged_by_what_its_rule_matches() {
    let p = Project::new("unmatched_shapes");
    p.write("pyproject.toml", SHAPES_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", SHAPES_MODEL)
        .write("pkg/store.py", SHAPES_STORE);
    let (_, stdout, stderr) = p.heki(&["check"]);
    assert_eq!(
        lines(&stderr, "warning: "),
        [
            "`targets` entry `pkg.model.Field._peeked` of write-owners `writes` names no attribute, method, or `__dict__` key of `pkg.model.Field` in the analyzed files",
            "`targets` entry `pkg.model.Field.extras[\"k\"]` of write-owners `writes` names no attribute, method, or `__dict__` key of `pkg.model.Field` in the analyzed files",
            "`targets` entry `pkg.model.Field.gone` of write-owners `writes` and `again` names no attribute, method, or `__dict__` key of `pkg.model.Field` in the analyzed files",
            "`targets` entry `pkg.models.Field.kind` of write-owners `writes` names no class in the analyzed files; `pkg.model.Field.kind` exists",
            "`targets` entry `pkg.gone.*Store._x` of write-owners `writes` names no class in the analyzed files",
            "`allowed-writers` entry `pkg.store:sav` of write-owners `writes` names no function or method in the analyzed files; `pkg.store:save` or `pkg.store:save_all` exist",
            "`allowed-writers` entry `pkg.gone:*` of write-owners `writes` names no function or method in the analyzed files",
            "`allowed-writers` entry `pkg.*.gone_function` of write-owners `writes` names no function or method in the analyzed files",
            "`exempt-methods` entry `__deep_copy__` of write-owners `writes` names no method in the analyzed files; `__deepcopy__` exists",
            "`exempt-methods` entry `Field.__deep_copy__` of write-owners `writes` names no method in the analyzed files; `Field.__deepcopy__` exists",
            "`allowed-protocols` entry `pkg.model.Hots` of write-owners `writes` names no class in the analyzed files; `pkg.model.Host` exists",
            "`forbid-cached-property-on` entry `pkg.model.Gone` of write-owners `writes` names no class in the analyzed files",
            "`forbidden-readers` entry `pkg.views.*` of read-owners `reads` names no function or method in the analyzed files",
            "`targets` entry `pkg.model:helper(x=1)` of call-owners #1 names no function, method, or class taking `x` in the analyzed files",
            "`targets` entry `pkg.model.Field.removed_method` of call-owners #1 names no function, method, or class in the analyzed files",
            "`scopes` entry `pkg.render` of effect-boundaries `pure` names no module, class, or function in the analyzed files",
            "`write-targets` entry `pkg.model.Field.nope` of effect-boundaries `pure` names no attribute, method, or `__dict__` key of `pkg.model.Field` in the analyzed files",
            "`invalidators` entry `pkg.model.Field.announce` of invalidation `announce` names no function or method in the analyzed files",
        ],
        "{stdout}{stderr}"
    );
}

/// A `{...}` group in the holder of a `Class.attr` target may hold dotted alternatives, as a
/// module glob can: `pkg.{a.Base,a.Sub}.only_sub` names the attribute of both classes (of
/// `Base` through its subclass) and is no error, while each alternative that is no class is
/// one.
#[test]
fn a_holder_brace_may_hold_dotted_alternatives() {
    let p = Project::new("unmatched_dotted_brace_holder");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT203\"]\n\
         unmatched-contract-entries = \"error\"\n\n\
         [[tool.heki.read-owners]]\nname = \"dotted\"\n\
         targets = [\"pkg.{a.Base,a.Sub}.only_sub\"]\nallowed-readers = [\"pkg.a:*\"]\n\n\
         [[tool.heki.read-owners]]\nname = \"gone\"\n\
         targets = [\"pkg.{a.Gone,a.Nope}.only_sub\"]\nallowed-readers = [\"pkg.a:*\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write(
        "pkg/a.py",
        "class Base:\n    pass\n\n\nclass Sub(Base):\n    only_sub = 1\n",
    )
    .write(
        "pkg/b.py",
        "from pkg.a import Sub\n\n\ndef read(s: Sub) -> int:\n    return s.only_sub\n",
    );
    let (code, stdout, stderr) = p.heki(&["check"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert_eq!(
        lines(&stderr, "error: "),
        [
            "`targets` entry `pkg.{a.Gone,a.Nope}.only_sub` of read-owners `gone` lists `pkg.a.Gone.only_sub` and `pkg.a.Nope.only_sub`, which name no class in the analyzed files"
        ],
        "{stdout}{stderr}"
    );
    assert!(
        stdout.contains("outside the read owners of `dotted`"),
        "{stdout}{stderr}"
    );
}

const BRACES_STORE: &str = "\
class Base:
    a = 1
    b = 2

    def op(self) -> None:
        pass


def run(base: Base) -> None:
    base.op()


def outer() -> None:
    def inner() -> None:
        pass

    inner()
";

/// A project of `pkg.store`, a class `pkg.dyn:Dyn` the code gives attributes named at run time,
/// and a namespace package `pkg.zz` (no `__init__.py`) with a module below it, judging the
/// contracts `contracts` under `unmatched-contract-entries = "error"`.
fn braces_project(name: &str, select: &str, contracts: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = {select}\n\
             unmatched-contract-entries = \"error\"\n\n{contracts}"
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/store.py", BRACES_STORE)
    .write(
        "pkg/dyn.py",
        "class Dyn:\n    def __init__(self, name: str) -> None:\n        setattr(self, name, 1)\n",
    )
    .write("pkg/zz/mod.py", "def f() -> None:\n    pass\n");
    p
}

const BRACES_CONTRACTS: &str = r#"
[[tool.heki.write-owners]]
name = "writes"
targets = ["pkg.store:Base.{a,zz}[\"k\"]"]
allowed-writers = ["pkg.store:run"]

[[tool.heki.read-owners]]
name = "reads"
targets = ["pkg.store:Base.{a,c}", "pkg.{store,gone}:Base.{a,zz}", "pkg.dyn:Dyn.{x,y}"]
allowed-readers = ["pkg.store:run"]

[[tool.heki.call-owners]]
name = "calls"
targets = ["pkg.store:Base.op", "pkg.store:{run,gone}(base=1)"]
allowed-callers = [
    "pkg.store:outer{,.*}",
    "pkg{,.zz.mod}:f",
    "pkg.zz{,.*}:*",
    "pkg.store:missing{,.*}",
    "pkg.gone{,.*}:*",
    "pkg.store:Base.{c,d}",
    "pkg.store:{outr,outter}",
    "pkg.store:{run,gone_a,gone_b,gone_c,gone_d}",
]
"#;

/// Braces are taken first. A group without an empty alternative lists names, each judged
/// (in a call target without its argument list, in a `Class.attr` target without its key):
/// the error lists those naming nothing, one line per what they name none of. A group with an
/// empty alternative is one name with its extensions, which names something when one of them
/// does: `outer` or a scope inside it, `pkg:f` or `pkg.zz.mod:f`, the namespace package
/// `pkg.zz` or a module below it.
#[test]
fn braces_list_names_or_extend_one() {
    let p = braces_project(
        "unmatched_braces",
        "[\"MUT201\", \"MUT203\", \"MUT204\"]",
        BRACES_CONTRACTS,
    );
    let (code, stdout, stderr) = p.heki(&["check"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert_eq!(
        lines(&stderr, "error: "),
        [
            "`targets` entry `pkg.store:Base.{a,zz}[\"k\"]` of write-owners `writes` lists `pkg.store:Base.zz[\"k\"]`, which names no attribute, method, or `__dict__` key of `pkg.store:Base` in the analyzed files",
            "`targets` entry `pkg.store:Base.{a,c}` of read-owners `reads` lists `pkg.store:Base.c`, which names no attribute, method, or `__dict__` key of `pkg.store:Base` in the analyzed files",
            "`targets` entry `pkg.{store,gone}:Base.{a,zz}` of read-owners `reads` lists `pkg.store:Base.zz`, which names no attribute, method, or `__dict__` key of `pkg.store:Base` in the analyzed files",
            "`targets` entry `pkg.{store,gone}:Base.{a,zz}` of read-owners `reads` lists `pkg.gone:Base.a` and `pkg.gone:Base.zz`, which name no class in the analyzed files; `pkg.store:Base.a` exists",
            "`targets` entry `pkg.dyn:Dyn.{x,y}` of read-owners `reads` lists `pkg.dyn:Dyn.x` and `pkg.dyn:Dyn.y`, which name no attribute, method, or `__dict__` key of `pkg.dyn:Dyn` in the analyzed files, unless they are ones the code names at run time",
            "`targets` entry `pkg.store:{run,gone}(base=1)` of call-owners `calls` lists `pkg.store:gone(base=1)`, which names no function, method, or class in the analyzed files",
            "`allowed-callers` entry `pkg.store:missing{,.*}` of call-owners `calls` names no function or method in the analyzed files",
            "`allowed-callers` entry `pkg.gone{,.*}:*` of call-owners `calls` names no function or method in the analyzed files",
            "`allowed-callers` entry `pkg.store:Base.{c,d}` of call-owners `calls` lists `pkg.store:Base.c` and `pkg.store:Base.d`, which name no function or method in the analyzed files",
            "`allowed-callers` entry `pkg.store:{outr,outter}` of call-owners `calls` lists `pkg.store:outr` and `pkg.store:outter`, which name no function or method in the analyzed files; `pkg.store:outer` exists",
            "`allowed-callers` entry `pkg.store:{run,gone_a,gone_b,gone_c,gone_d}` of call-owners `calls` lists `pkg.store:gone_a`, `pkg.store:gone_b`, `pkg.store:gone_c`, and 1 more, which name no function or method in the analyzed files",
        ],
        "{stdout}{stderr}"
    );
}

/// A glob holder is judged by the parts written out before its first glob: `pkg.zz.*:*` needs
/// a module `pkg.zz` or one below it (the namespace package has one), and `pkg.*.run_*` needs
/// `pkg`. A glob without a written-out part (`*_helper`, `*.run_*`) is not judged, and a glob in
/// the last part needs what holds it.
#[test]
fn a_glob_holder_is_judged_by_its_written_out_prefix() {
    let contracts = r#"
[[tool.heki.call-owners]]
name = "calls"
targets = ["pkg.store:Base.op"]
allowed-callers = [
    "pkg.zz.*:*",
    "pkg.*.run_*",
    "*_helper",
    "*.run_*",
    "pkg.store:Base.*",
    "pkg.gone.*:*",
    "gone.*.x*",
    "pkg.store:Gone.*",
]
"#;
    let p = braces_project("unmatched_glob_holder", "[\"MUT204\"]", contracts);
    let (code, stdout, stderr) = p.heki(&["check"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert_eq!(
        lines(&stderr, "error: "),
        [
            "`allowed-callers` entry `pkg.gone.*:*` of call-owners `calls` names no function or method in the analyzed files",
            "`allowed-callers` entry `gone.*.x*` of call-owners `calls` names no function or method in the analyzed files",
            "`allowed-callers` entry `pkg.store:Gone.*` of call-owners `calls` names no function or method in the analyzed files",
        ],
        "{stdout}{stderr}"
    );
}

const FOLDED_MAIN: &str = r#"[project]
name = "t"

[tool.heki]
include = ["contracts/more.toml"]

[tool.heki.lint]
select = ["MUT204", "IMP201"]
unmatched-contract-entries = "error"

[tool.heki.groups]
callers = ["pkg.store:outer{,.*}", "pkg.store:{outr,outter}", "pkg.zz.*:*", "pkg.gone.*:*"]

[[tool.heki.call-owners]]
name = "main"
targets = ["pkg.store:Base.op"]
allowed-callers = ["pkg.store:run", "@callers"]
"#;

const FOLDED_INCLUDED: &str = r#"[groups]
stale = ["pkg.store:{run,gone}"]
more-callers = [
    "pkg.{store,gone}:run",
    "pkg.zz.*:*",
    "pkg.gone_dir.*:*",
]
modules = ["pkg.{store,gone_m}", "pkg.gone_pkg.*"]

[[call-owners]]
name = "included"
targets = ["pkg.store:{run,gone}"]
allowed-callers = ["pkg.store:Base.{op,c}", "pkg.zz{,.*}:*", "pkg.gone{,.*}:*", "@more-callers"]

[[import-boundaries]]
name = "imports"
importers = ["@modules"]
allowed-imports = ["os"]
"#;

/// An entry a group or an included file writes into a contract is judged as one written in the
/// contract: its braces taken first, a glob holder by its written-out prefix, a module pattern
/// by its first name. The line names the group and the file and line of the entry, and a group
/// no contract uses is reported without judging its entries.
#[test]
fn entries_from_groups_and_included_files_are_judged_alike() {
    let p = braces_project("unmatched_folded", "[]", "");
    p.write("pyproject.toml", FOLDED_MAIN)
        .write("contracts/more.toml", FOLDED_INCLUDED);
    let (code, stdout, stderr) = p.heki(&["check"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert_eq!(
        lines_as_written(&stderr, "error: "),
        [
            "the group `stale` (contracts/more.toml:2) is used by no contract",
            "`allowed-callers` entry `pkg.store:{outr,outter}` of call-owners `main` (from group `callers` at pyproject.toml:12; contract at pyproject.toml:14) lists `pkg.store:outr` and `pkg.store:outter`, which name no function or method in the analyzed files; `pkg.store:outer` exists",
            "`allowed-callers` entry `pkg.gone.*:*` of call-owners `main` (from group `callers` at pyproject.toml:12; contract at pyproject.toml:14) names no function or method in the analyzed files",
            "`targets` entry `pkg.store:{run,gone}` of call-owners `included` (from contracts/more.toml:12; contract at contracts/more.toml:10) lists `pkg.store:gone`, which names no function, method, or class in the analyzed files",
            "`allowed-callers` entry `pkg.store:Base.{op,c}` of call-owners `included` (from contracts/more.toml:13; contract at contracts/more.toml:10) lists `pkg.store:Base.c`, which names no function or method in the analyzed files",
            "`allowed-callers` entry `pkg.gone{,.*}:*` of call-owners `included` (from contracts/more.toml:13; contract at contracts/more.toml:10) names no function or method in the analyzed files",
            "`allowed-callers` entry `pkg.{store,gone}:run` of call-owners `included` (from group `more-callers` at contracts/more.toml:4; contract at contracts/more.toml:10) lists `pkg.gone:run`, which names no function or method in the analyzed files; `pkg.store:run` exists",
            "`allowed-callers` entry `pkg.gone_dir.*:*` of call-owners `included` (from group `more-callers` at contracts/more.toml:6; contract at contracts/more.toml:10) names no function or method in the analyzed files",
            "`importers` entry `pkg.{store,gone_m}` of import-boundaries `imports` (from group `modules` at contracts/more.toml:8; contract at contracts/more.toml:15) lists `pkg.gone_m`, which names no module in the analyzed files",
            "`importers` entry `pkg.gone_pkg.*` of import-boundaries `imports` (from group `modules` at contracts/more.toml:8; contract at contracts/more.toml:15) names no module in the analyzed files",
        ],
        "{stdout}{stderr}"
    );
    let (code, json, stderr) = p.heki(&["recommend", "--output-format", "json"]);
    assert_eq!(code, 0, "{json}{stderr}");
    let report: serde_json::Value = serde_json::from_str(&json).unwrap();
    let entry = (report["unmatched_entries"].as_array().unwrap().iter())
        .find(|e| e["entry"] == "pkg.{store,gone}:run")
        .unwrap_or_else(|| panic!("{json}"));
    assert_eq!(entry["alternatives"], serde_json::json!(["pkg.gone:run"]));
    assert_eq!(
        entry["written_at"],
        serde_json::json!([{"file": "contracts/more.toml", "line": 4, "group": "more-callers"}])
    );
}

const PROJECT_FIRST_CONTRACTS: &str = r#"
[[tool.heki.effect-boundaries]]
name = "pure"
scopes = ["pkg.store"]
forbid = ["reads-targets", "calls"]
read-targets = ["pkg.store:Base.a", "sys.platform", "Base.nothing", "pkg.store:Base.gone"]
call-targets = ["pkg.store:run", "os.getenv", "pkg.store:gone"]
allowed-callees = ["pkg.store", "json.*", "pkg.gone"]

[[tool.heki.readonly-returns]]
name = "shared"
producers = [
    "pkg.store:run",
    "pkg.store:Base.a",
    "json.loads",
    "Base.whatever",
    "pkg.store:Base.gone",
    "pkg.gone",
    "pkg.store:gone",
]

[[tool.heki.import-boundaries]]
name = "imports"
importers = ["pkg.store", "pkg.zz", "tests.*", "pkg.gone"]
exempt-importers = ["pkg.dyn", "pkg.gone_exempt"]
allowed-imports = ["<stdlib>", "os", "pkg.store:Base", "pkg.zz.mod", "pkg.*", "pkg.gone_module:Name"]
relative-imports = ["pkg.store", "pkg.gone_relative.*"]
ignored-imports = [
    "pkg.store -> os",
    "tests -> pkg.store",
    "pkg.store -> pkg.gone_pair",
    "pkg.gone_importer -> os",
]

[[tool.heki.import-boundaries]]
name = "forbidden"
forbidden-imports = ["pkg.store", "pkg.gone_forbidden"]
"#;

/// The keys that may name code outside the project by its full name are judged when an entry's
/// first name is a module of the project: an effect boundary's read and call targets and
/// allowed callees, the producers of `readonly-returns` (a function or an attribute), and the
/// module patterns of `import-boundaries` (each side of an `ignored-imports` pair, a namespace
/// package naming a module). `sys.platform`, `Base.nothing`, `<stdlib>`, and `tests.*` are not
/// judged. A baseline records none of them and hides none.
#[test]
fn entries_whose_first_name_is_a_project_module_are_judged() {
    let p = braces_project(
        "unmatched_project_first",
        "[\"EFF201\", \"MUT202\", \"IMP201\"]",
        PROJECT_FIRST_CONTRACTS,
    );
    let expected = [
        "`read-targets` entry `pkg.store:Base.gone` of effect-boundaries `pure` names no attribute, method, or `__dict__` key of `pkg.store:Base` in the analyzed files",
        "`call-targets` entry `pkg.store:gone` of effect-boundaries `pure` names no function, method, or class in the analyzed files",
        "`allowed-callees` entry `pkg.gone` of effect-boundaries `pure` names no module, class, or function in the analyzed files",
        "`producers` entry `pkg.store:Base.gone` of readonly-returns `shared` names no function, method, or attribute in the analyzed files",
        "`producers` entry `pkg.gone` of readonly-returns `shared` names no function, method, or attribute in the analyzed files",
        "`producers` entry `pkg.store:gone` of readonly-returns `shared` names no function, method, or attribute in the analyzed files",
        "`importers` entry `pkg.gone` of import-boundaries `imports` names no module in the analyzed files",
        "`exempt-importers` entry `pkg.gone_exempt` of import-boundaries `imports` names no module in the analyzed files",
        "`allowed-imports` entry `pkg.gone_module:Name` of import-boundaries `imports` names no module in the analyzed files",
        "`relative-imports` entry `pkg.gone_relative.*` of import-boundaries `imports` names no module in the analyzed files",
        "`ignored-imports` entry `pkg.store -> pkg.gone_pair` of import-boundaries `imports` names no module in the analyzed files",
        "`ignored-imports` entry `pkg.gone_importer -> os` of import-boundaries `imports` names no module in the analyzed files",
        "`forbidden-imports` entry `pkg.gone_forbidden` of import-boundaries `forbidden` names no module in the analyzed files",
    ];
    let (code, stdout, stderr) = p.heki(&["check"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert_eq!(lines(&stderr, "error: "), expected, "{stdout}{stderr}");

    let baseline = ["--baseline", "heki-baseline.toml"];
    let (code, stdout, stderr) = p.heki(&[&["check", "--update-baseline"][..], &baseline].concat());
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(
        !p.read("heki-baseline.toml").contains("gone"),
        "{stdout}{stderr}"
    );
    let (code, stdout, stderr) = p.heki(&[&["check"][..], &baseline].concat());
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert_eq!(lines(&stderr, "error: "), expected, "{stdout}{stderr}");
}

/// An entry many contracts hold is one line, naming the first three contracts and counting the
/// others.
#[test]
fn an_entry_many_contracts_hold_is_one_line() {
    let p = Project::new("unmatched_many_contracts");
    let mut config = String::from("[project]\nname = \"t\"\n");
    for name in ["a", "b", "c", "d", "e"] {
        config.push_str(&format!(
            "\n[[tool.heki.call-owners]]\nname = \"{name}\"\ntargets = [\"pkg.model.helper\"]\n\
             allowed-callers = [\"pkg.store:gone\"]\n"
        ));
    }
    p.write("pyproject.toml", &config)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", "def helper() -> None:\n    pass\n")
        .write("pkg/store.py", "def save() -> None:\n    pass\n");
    let (_, _, stderr) = p.heki(&["check"]);
    assert_eq!(
        lines(&stderr, "warning: "),
        [
            "`allowed-callers` entry `pkg.store:gone` of call-owners `a`, `b`, `c`, and 2 more names no function or method in the analyzed files"
        ],
        "{stderr}"
    );
}

const PRIVATE_MODEL: &str = "\
class Parser:
    def __init__(self) -> None:
        self.items = []

    def __postprocess(self) -> None:
        pass

    def __run(self) -> None:
        def step() -> None:
            self.__postprocess()

        step()


class Outer:
    class __Inner:
        value = 0

        def run(self) -> None:
            pass
";

const PRIVATE_CONFIG: &str = r#"[project]
name = "t"

[[tool.heki.call-owners]]
name = "private"
targets = [
    "pkg.base:Parser._Parser__postprocess",
    "pkg.base:Parser.__postprocess",
    "pkg.base:Parser._Parser__gone",
    "pkg.base:Parser._Other__postprocess",
    "pkg.base:Outer._Outer__Inner.run",
]
allowed-callers = [
    "pkg.base:Parser._Parser__run",
    "pkg.base:Parser._Parser__run.*",
    "pkg.base:Parser._Parser__walk",
]

[[tool.heki.write-owners]]
name = "items"
targets = [
    "pkg.base.Parser.items",
    "pkg.base.Outer._Outer__Inner.value",
    "pkg.base.Outer._Outer__Inner.gone",
]
allowed-writers = ["pkg.base:Parser._Parser__run", "pkg.base:Parser._Parser__*"]
exempt-methods = ["Parser._Parser__postprocess", "_Parser__postprocess"]
forbid-cached-property-on = ["pkg.base.Outer._Outer__Inner", "pkg.base.Outer.__Inner"]

[[tool.heki.effect-boundaries]]
name = "pure"
scopes = ["pkg.base:Parser._Parser__run", "pkg.base:Outer._Outer__Inner"]
forbid = ["writes-targets"]
write-targets = ["pkg.base.Parser.items"]
"#;

/// A private member is named in either spelling the contracts accept, as written in its class
/// (`Parser.__postprocess`) or as the class holds it (`Parser._Parser__postprocess`): by a call
/// target, a function pattern and the functions a glob names inside it, a class and the
/// attributes of a target, and a scope.
/// The held spelling of a member or attribute that does not exist, or with another class's
/// prefix, names nothing, and an exempt method without its class is named by its last part as
/// written.
#[test]
fn a_private_member_is_named_in_either_spelling() {
    let p = Project::new("unmatched_private_members");
    p.write("pyproject.toml", PRIVATE_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/base.py", PRIVATE_MODEL);
    let (_, stdout, stderr) = p.heki(&["check"]);
    assert_eq!(
        lines(&stderr, "warning: "),
        [
            "`targets` entry `pkg.base.Outer._Outer__Inner.gone` of write-owners `items` names no attribute, method, or `__dict__` key of `pkg.base.Outer._Outer__Inner` in the analyzed files",
            "`exempt-methods` entry `_Parser__postprocess` of write-owners `items` names no method in the analyzed files; `__postprocess` exists",
            "`targets` entry `pkg.base:Parser._Parser__gone` of call-owners `private` names no function, method, or class in the analyzed files",
            "`targets` entry `pkg.base:Parser._Other__postprocess` of call-owners `private` names no function, method, or class in the analyzed files; `pkg.base:Parser.__postprocess` exists",
            "`allowed-callers` entry `pkg.base:Parser._Parser__walk` of call-owners `private` names no function or method in the analyzed files",
        ],
        "{stdout}{stderr}"
    );
}

/// A `__dict__` key target the code only reads names nothing: a class declares no such key, so
/// only a write of it (an item store, `setdefault`, construction) makes it exist. An
/// attribute the class declares stays named by its declaration, also when only read through the
/// namespace.
#[test]
fn a_dict_key_only_read_names_nothing() {
    let p = Project::new("unmatched_read_only_key");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\"]\n\
         unmatched-contract-entries = \"error\"\n\n\
         [[tool.heki.write-owners]]\nname = \"field-cache\"\n\
         targets = [\"pkg.model.Field._cache\", \"pkg.model.Field._gone\", \
         \"pkg.model.Field._stored\", \"pkg.model.Field._updated\", \"pkg.model.Field.kind\"]\n\
         allowed-writers = [\"pkg.store:*\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write(
        "pkg/model.py",
        "class Field:\n    def __init__(self) -> None:\n        self.kind = \"str\"\n",
    )
    .write(
        "pkg/reader.py",
        "from pkg.model import Field\n\nKEY = \"_cache\"\n\n\n\
         def read(field: Field) -> object:\n    return field.__dict__.get(KEY)\n\n\n\
         def read_item(field: Field) -> object:\n    return field.__dict__[\"_stored\"], field.__dict__[\"kind\"]\n",
    )
    .write(
        "pkg/store.py",
        "from pkg.model import Field\n\n\ndef write(field: Field) -> None:\n    \
         field.__dict__[\"_stored\"] = 1\n    field.__dict__.setdefault(\"_updated\", 1)\n",
    );
    let (code, stdout, stderr) = p.heki(&["check"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert_eq!(
        lines(&stderr, "error: "),
        [
            "`targets` entry `pkg.model.Field._cache` of write-owners `field-cache` names no attribute, method, or `__dict__` key of `pkg.model.Field` in the analyzed files",
            "`targets` entry `pkg.model.Field._gone` of write-owners `field-cache` names no attribute, method, or `__dict__` key of `pkg.model.Field` in the analyzed files",
        ],
        "{stderr}"
    );
}

/// The same read-only key of a read-owners target exists: a read of it is what the contract
/// guards, so the target matches however the key is read, and a write-owners target of the key
/// stays unmatched.
#[test]
fn a_dict_key_only_read_names_a_read_owners_target() {
    let p = Project::new("unmatched_read_only_key_read_owners");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\", \"MUT203\"]\n\
         unmatched-contract-entries = \"error\"\n\n\
         [[tool.heki.write-owners]]\nname = \"field-cache-writes\"\n\
         targets = [\"pkg.model.Field._cache\"]\nallowed-writers = [\"pkg.reader:*\"]\n\n\
         [[tool.heki.read-owners]]\nname = \"field-cache-reads\"\n\
         targets = [\"pkg.model.Field._cache\", \"pkg.model.Field._gone\"]\n\
         allowed-readers = [\"pkg.reader:*\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write(
        "pkg/model.py",
        "class Field:\n    def __init__(self) -> None:\n        self.kind = \"str\"\n",
    )
    .write(
        "pkg/reader.py",
        "from pkg.model import Field\n\n\n\
         def read(field: Field) -> object:\n    return field.__dict__.get(\"_cache\")\n",
    );
    let (code, stdout, stderr) = p.heki(&["check"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert_eq!(
        lines(&stderr, "error: "),
        [
            "`targets` entry `pkg.model.Field._cache` of write-owners `field-cache-writes` names no attribute, method, or `__dict__` key of `pkg.model.Field` in the analyzed files",
            "`targets` entry `pkg.model.Field._gone` of read-owners `field-cache-reads` names no attribute, method, or `__dict__` key of `pkg.model.Field` in the analyzed files",
        ],
        "{stderr}"
    );
}
