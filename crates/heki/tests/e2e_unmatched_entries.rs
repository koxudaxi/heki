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

    let p = repro(
        "unmatched_bad_value",
        "unmatched-contract-entries = \"report\"\n",
    );
    let (code, _, stderr) = p.heki(&["check"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("unmatched-contract-entries"), "{stderr}");
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
