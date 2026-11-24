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

/// The lines of `stderr` that start with `prefix`, without it.
fn lines<'s>(stderr: &'s str, prefix: &str) -> Vec<&'s str> {
    stderr
        .lines()
        .filter_map(|l| l.strip_prefix(prefix))
        .collect()
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

    let p = repro(
        "unmatched_error",
        "unmatched-contract-entries = \"error\"\n",
    );
    let (code, stdout, stderr) = p.heki(&["check"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert_eq!(lines(&stderr, "error: "), REPRO_ENTRIES, "{stderr}");

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

/// A project whose contract names a stale attribute of each class of [`KEYS_MODEL`], and the
/// keys `_first` and `_second` of `Named`; `lint` holds the rest of the `[tool.heki.lint]`
/// table.
fn keys_project(name: &str, lint: &str) -> Project {
    let mut targets: Vec<String> = KEYS_CLASSES
        .iter()
        .map(|c| format!("\"pkg.model.{c}.gone\""))
        .collect();
    targets.push("\"pkg.model.Named._first\"".to_owned());
    targets.push("\"pkg.model.Named._second\"".to_owned());
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
    .write("pkg/model.py", KEYS_MODEL)
    .write("pkg/copies.py", KEYS_COPIES);
    p
}

/// The message of the entry naming nothing of `class`, with `suffix`.
fn stale(class: &str, suffix: &str) -> String {
    format!(
        "`targets` entry `pkg.model.{class}.gone` of write-owners `stale` names no attribute, \
         method, or `__dict__` key of `pkg.model.{class}` in the analyzed files{suffix}"
    )
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
    let named_at_run_time = ["Computed", "FromBase", "Narrow"];
    let p = keys_project("unmatched_keys_warning", "");
    let (code, stdout, stderr) = p.heki(&["check"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    let known = KEYS_CLASSES
        .iter()
        .filter(|c| !named_at_run_time.contains(c));
    let expected: Vec<String> = known.map(|c| stale(c, "")).collect();
    assert_eq!(lines(&stderr, "warning: "), expected, "{stderr}");

    let p = keys_project(
        "unmatched_keys_error",
        "unmatched-contract-entries = \"error\"\n",
    );
    let (code, stdout, stderr) = p.heki(&["check"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let suffix = |c: &str| match named_at_run_time.contains(&c) {
        true => ", unless it is one the code names at run time",
        false => "",
    };
    let expected: Vec<String> = KEYS_CLASSES.iter().map(|c| stale(c, suffix(c))).collect();
    assert_eq!(lines(&stderr, "error: "), expected, "{stderr}");
}

/// Without the key, a run over some files of the project judges nothing: an entry naming code
/// in a file the paths leave out would look unmatched. With the key, the files given are the
/// project.
#[test]
fn a_run_over_some_files_judges_only_with_the_key() {
    let p = repro("unmatched_some_files", "");
    let (code, stdout, stderr) = p.heki(&["check", "pkg/store.py"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(lines(&stderr, "warning: ").is_empty(), "{stderr}");
    let (_, _, stderr) = p.heki(&["check", "."]);
    assert_eq!(lines(&stderr, "warning: ").len(), 3, "{stderr}");

    let p = repro(
        "unmatched_some_files_set",
        "unmatched-contract-entries = \"warning\"\n",
    );
    let (_, _, stderr) = p.heki(&["check", "pkg/store.py"]);
    let warned = lines(&stderr, "warning: ");
    assert!(
        warned.contains(&"`targets` entry `pkg.model.Field.replace_type` of call-owners `store-calls` names no function, method, or class in the analyzed files"),
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
/// or reads on an object of the class, a name written on an object heki cannot type, any name
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
            "`targets` entry `pkg.model.Field.removed_method` of call-owners #1 names no function, method, or class in the analyzed files",
            "`scopes` entry `pkg.render` of effect-boundaries `pure` names no module, class, or function in the analyzed files",
            "`write-targets` entry `pkg.model.Field.nope` of effect-boundaries `pure` names no attribute, method, or `__dict__` key of `pkg.model.Field` in the analyzed files",
            "`invalidators` entry `pkg.model.Field.announce` of invalidation `announce` names no function or method in the analyzed files",
        ],
        "{stdout}{stderr}"
    );
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
