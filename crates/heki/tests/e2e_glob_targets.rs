//! Glob targets end to end through the real binary: the last segment of a call target names the
//! function, method, or class (a glob there matches within that name, as the attribute of a
//! `Class.attr` target does), and a report of a glob target names the member it matched.

mod common;

use common::Project;
use serde_json::Value;

const CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT201", "MUT203", "MUT204", "MUT109"]

[[tool.heki.read-owners]]
name = "host-private"
targets = ["pkg.*._*"]
forbidden-readers = ["pkg.parser._*"]

[[tool.heki.call-owners]]
name = "host-private-calls"
targets = ["pkg.*._*"]
forbidden-callers = ["pkg.parser._*"]

[[tool.heki.write-owners]]
name = "flags"
targets = ["DataType._*", "DataType.name", 'DataType.tags["k"]']
allowed-writers = ["DataType.*"]

[[tool.heki.invalidation]]
name = "revisions"
targets = ["DataType._*"]
invalidators = ["DataType.note"]
exempt-writers = ["DataType.*"]
"#;

const TYPES: &str = r#"from functools import cached_property


class DataType:
    def __init__(self) -> None:
        self._flag = True
        self._mark = 0
        self.name = ""
        self.tags: dict[str, int] = {}

    def _is_container(self) -> bool:
        return self._flag

    @property
    def _texts(self) -> str:
        return ""

    @cached_property
    def _cached(self) -> int:
        return 1

    def note(self) -> None:
        pass


class Other:
    def _pure(self) -> int:
        return 1
"#;

const CONCERN: &str = r#"from pkg.types import DataType, Other


def typed(data_type: DataType) -> bool:
    return data_type._flag


def method_call(data_type: DataType) -> bool:
    return data_type._is_container()


def prop(data_type: DataType) -> str:
    return data_type._texts


def pure_method(other: Other) -> int:
    return other._pure()


def _helper(data_type: DataType) -> bool:
    return data_type._is_container()


def writes(data_type: DataType) -> None:
    _both(data_type)
    data_type.name = "n"


def _both(data_type: DataType) -> None:
    data_type._flag = False
    data_type._mark = 1


def tag(data_type: DataType) -> None:
    data_type.tags["k"] = 1
"#;

fn project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("pyproject.toml", CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/parser/__init__.py", "")
        .write("pkg/types.py", TYPES)
        .write("pkg/parser/_concern.py", CONCERN);
    p
}

/// `(line, code, message)` of every diagnostic, in order.
fn reports(p: &Project) -> Vec<(u64, String, String)> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    diags
        .iter()
        .map(|d| {
            (
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
                d["message"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn a_glob_call_target_names_members_and_reports_name_what_they_matched() {
    let p = project("glob_targets");
    let found = reports(&p);
    let calls: Vec<(u64, &str)> = found
        .iter()
        .filter(|r| r.1 == "MUT204")
        .map(|r| (r.0, r.2.as_str()))
        .collect();
    // `pkg.*._*` names the private methods and properties of the classes in `pkg` (and the
    // private helper `_both`), not every function of the private module `pkg.parser._concern`:
    // the forbidden callers are no targets, so their calls are reported.
    assert_eq!(
        calls,
        vec![
            (
                9,
                "`method_call` calls `pkg.types.DataType._is_container` inside the forbidden callers of `host-private-calls`"
            ),
            (
                13,
                "`prop` calls `pkg.types.DataType._texts` inside the forbidden callers of `host-private-calls`"
            ),
            (
                17,
                "`pure_method` calls `pkg.types.Other._pure` inside the forbidden callers of `host-private-calls`"
            ),
            (
                25,
                "`writes` calls `pkg.parser._concern._both` inside the forbidden callers of `host-private-calls`"
            ),
        ]
    );
    let message = |line: u64, code: &str| {
        found
            .iter()
            .find(|r| r.0 == line && r.1 == code)
            .unwrap_or_else(|| panic!("no {code} at {line}: {found:#?}"))
            .2
            .clone()
    };
    // Reads name the attribute of the class they read.
    assert_eq!(
        message(5, "MUT203"),
        "`typed` reads `pkg.types.DataType._flag` (by attribute) inside the forbidden readers of `host-private`"
    );
    // One report of a glob target stands for every member a helper writes; it names the first
    // and lists the others. A literal target is named as configured.
    assert_eq!(
        message(25, "MUT201"),
        "`writes` writes `pkg.types.DataType._flag` (by attribute) via `pkg.parser._concern._both` outside the write owners of `flags` (also `pkg.types.DataType._mark`)"
    );
    assert_eq!(
        message(25, "MUT109"),
        "`writes` writes `pkg.types.DataType._flag` (by attribute) via `pkg.parser._concern._both` and returns on some path without calling an invalidator of `revisions` (also `pkg.types.DataType._mark`)"
    );
    assert_eq!(
        message(26, "MUT201"),
        "`writes` writes `DataType.name` (by attribute) outside the write owners of `flags`"
    );
    // The key of an entry target is no glob.
    assert_eq!(
        message(35, "MUT201"),
        "`tag` writes `DataType.tags[\"k\"]` (in place) outside the write owners of `flags`"
    );
}

/// A glob naming a class: a report of a construction names the class constructed.
#[test]
fn a_glob_class_target_names_the_class_constructed() {
    let p = Project::new("glob_class_target");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"facts\"\ntargets = [\"pkg.*.Final*\"]\nallowed-callers = [\"pkg.facts.*\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write(
        "pkg/facts.py",
        "class FinalFacts:\n    def __init__(self, n: int) -> None:\n        self.n = n\n\n\ndef make() -> FinalFacts:\n    return FinalFacts(1)\n",
    )
    .write(
        "pkg/use.py",
        "from pkg.facts import FinalFacts\n\n\ndef build() -> FinalFacts:\n    return FinalFacts(2)\n",
    );
    assert_eq!(
        reports(&p),
        vec![(
            5,
            "MUT204".to_owned(),
            "`build` constructs `pkg.facts.FinalFacts` outside the call owners of `facts`"
                .to_owned()
        )]
    );
}

/// A glob in the last segment matches the last name only, and the rest of the target what holds
/// it: `pkg.api_*` names `pkg.api_commit` and not `pkg.api_helpers.api_reset`, and `_*` names
/// `pkg.core._guarded` and not `pkg._internal.public`. Those are no targets, so their calls of
/// the targets are reported.
#[test]
fn a_last_segment_glob_names_no_member_of_a_nested_holder() {
    let p = Project::new("glob_last_segment");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"api\"\ntargets = [\"pkg.api_*\"]\nallowed-callers = [\"pkg.service.*\"]\n\n[[tool.heki.call-owners]]\nname = \"private\"\ntargets = [\"_*\"]\nallowed-callers = [\"pkg.service.*\"]\n",
    )
    .write("pkg/__init__.py", "def api_commit() -> None:\n    pass\n")
    .write(
        "pkg/api_helpers.py",
        "from pkg import api_commit\n\n\ndef api_reset() -> None:\n    api_commit()\n",
    )
    .write("pkg/core.py", "def _guarded() -> None:\n    pass\n")
    .write(
        "pkg/_internal.py",
        "from pkg.core import _guarded\n\n\ndef public() -> None:\n    _guarded()\n",
    );
    assert_eq!(
        reports(&p),
        vec![
            (
                5,
                "MUT204".to_owned(),
                "`public` calls `pkg.core._guarded` outside the call owners of `private`"
                    .to_owned()
            ),
            (
                5,
                "MUT204".to_owned(),
                "`api_reset` calls `pkg.api_commit` outside the call owners of `api`".to_owned()
            ),
        ]
    );
}

const CONTEXTS: &str = r#"from typing import NamedTuple


class Context(NamedTuple):
    module: str


class Parser:
    def _secret(self) -> int:
        return 1


class SubParser(Parser):
    def _sub_only(self) -> int:
        return 2
"#;

const RENDER: &str = r#"from pkg.parser.base import Context, Parser


def typed(ctx: Context) -> Context:
    return ctx._replace(module="m")


def untyped(contexts):
    return [ctx._replace(module="m") for ctx in contexts]


def untyped_api(ctx):
    return ctx._asdict(), ctx._fields


def control(parser: Parser) -> int:
    return parser._secret()


def untyped_defined(parser):
    return parser._secret()


def untyped_subclass_member(parser):
    return parser._sub_only()


def untyped_written_elsewhere(parser):
    return parser._cache


def untyped_unknown_name(parser):
    return parser._nobody_has_this()
"#;

/// Strict `call-owners` and `read-owners` contracts on `(calls, reads)` targets, over a
/// `NamedTuple` and a class with a subclass, one of whose instances another module gives `_cache`.
fn unknown_objects_project(name: &str, (calls, reads): (&str, &str)) -> Project {
    let contract = |section: &str, targets: &str, who: &str| {
        format!(
            "[[tool.heki.{section}]]\nname = \"{section}\"\ntargets = {targets}\nforbidden-{who} = [\"pkg.parser._render:*\"]\nattribution = \"lexical\"\nunresolved = \"report\"\n\n"
        )
    };
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT203\", \"MUT204\", \"MUT206\", \"MUT207\"]\n\n{}{}",
            contract("call-owners", calls, "callers"),
            contract("read-owners", reads, "readers"),
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/parser/__init__.py", "")
    .write("pkg/parser/base.py", CONTEXTS)
    .write(
        "pkg/parser/setup.py",
        "from pkg.parser.base import Parser\n\n\ndef attach(parser: Parser) -> None:\n    parser._cache = {}\n",
    )
    .write("pkg/parser/_render.py", RENDER);
    p
}

/// With `unresolved = "report"`, a glob target matches on an object of unknown class only the
/// names a class it names (or a project subclass, or a project base) has from project code: what
/// it defines, and what the code writes on objects typed with it. `_replace`, `_asdict`, and
/// `_fields` of a `NamedTuple` are `tuple`'s API, and a name no class has is no target's. A
/// written-out last name matches by name.
#[test]
fn a_glob_target_matches_on_an_object_of_unknown_class_the_members_its_classes_have() {
    let codes = |found: Vec<(u64, String, String)>| -> Vec<(u64, String)> {
        found
            .into_iter()
            .map(|(line, code, _)| (line, code))
            .collect()
    };
    let globs = "[\"pkg.parser.base.Context._*\", \"pkg.parser.base.Parser._*\"]";
    let globs = unknown_objects_project("unknown_objects_globs", (globs, globs));
    let found = reports(&globs);
    assert_eq!(
        codes(found.clone()),
        vec![
            (17, "MUT204".to_owned()),
            (21, "MUT207".to_owned()),
            (25, "MUT207".to_owned()),
            (29, "MUT206".to_owned()),
        ],
        "{found:#?}"
    );
    // The call matches the target of the class defining `_secret` only.
    assert!(
        found[1]
            .2
            .ends_with("heki cannot tell whether it calls `pkg.parser.base.Parser._*`"),
        "{found:#?}"
    );
    let written = "[\"pkg.parser.base.Context._replace\"]";
    let written = unknown_objects_project("unknown_objects_written", (written, written));
    assert_eq!(codes(reports(&written)), vec![(9, "MUT207".to_owned())]);
    // A glob naming no holder names the members of every project class; one naming module
    // functions names no member of any object.
    let every = ("[\"_*\"]", "[\"pkg.parser.base.Context.module\"]");
    assert_eq!(
        codes(reports(&unknown_objects_project(
            "unknown_objects_every_class",
            every
        ))),
        vec![
            (17, "MUT204".to_owned()),
            (21, "MUT207".to_owned()),
            (25, "MUT207".to_owned()),
        ]
    );
    let functions = (
        "[\"pkg.parser.setup.*\"]",
        "[\"pkg.parser.base.Context.module\"]",
    );
    let functions = unknown_objects_project("unknown_objects_functions", functions);
    let (code, stdout, stderr) = functions.check(&[]);
    assert_eq!(code, 0, "{stdout}{stderr}");
}

/// The shape datamodel-code-generator reported: `Context._*` and `Parser._*`, with an untyped
/// `ctx._replace(...)` (no target class defines `_replace`) and a typed call of a private
/// method of `Parser`.
#[test]
fn a_named_tuple_method_on_an_object_of_unknown_class_is_no_glob_target() {
    let p = Project::new("unknown_objects_repro");
    p.write(
        "pyproject.toml",
        r#"[project]
name = "h2"
version = "0"
requires-python = ">=3.11"

[tool.heki.lint]
select = ["MUT203", "MUT204", "MUT206", "MUT207"]

[[tool.heki.call-owners]]
name = "host-private-call"
targets = ["pkg.parser.base.Context._*", "pkg.parser.base.Parser._*"]
forbidden-callers = ["pkg.parser._render:*"]
attribution = "lexical"
unresolved = "report"
"#,
    )
    .write("pkg/__init__.py", "")
    .write("pkg/parser/__init__.py", "")
    .write(
        "pkg/parser/base.py",
        "from typing import NamedTuple\n\n\nclass Context(NamedTuple):\n    module: str\n\n\nclass Parser:\n    def _secret(self) -> int:\n        return 1\n",
    )
    .write(
        "pkg/parser/_render.py",
        "from pkg.parser.base import Context, Parser\n\n\ndef typed(ctx: Context) -> Context:\n    return ctx._replace(module=\"m\")\n\n\ndef untyped(contexts):\n    return [ctx._replace(module=\"m\") for ctx in contexts]\n\n\ndef control(parser: Parser) -> int:\n    return parser._secret()\n",
    );
    assert_eq!(
        reports(&p),
        vec![(
            13,
            "MUT204".to_owned(),
            "`control` calls `pkg.parser.base.Parser._secret` inside the forbidden callers of `host-private-call`".to_owned()
        )]
    );
}

/// A class whose instances the code gives a name computed at run time may have any member, so
/// its glob target matches any name on an object of unknown class: also the `NamedTuple` API
/// and `_nobody_has_this`. A computed name the instance has already (a key of another
/// instance's namespace), or one written on an object of unknown class, gives it no member.
#[test]
fn a_class_given_names_computed_at_run_time_may_have_any_member() {
    let codes = |p: &Project| -> Vec<(u64, String)> {
        reports(p)
            .into_iter()
            .map(|(line, code, _)| (line, code))
            .collect()
    };
    let owned = |found: &[(u64, &str)]| -> Vec<(u64, String)> {
        found.iter().map(|(l, c)| (*l, (*c).to_owned())).collect()
    };
    let globs = "[\"pkg.parser.base.Parser._*\"]";
    let p = unknown_objects_project("unknown_objects_existing_names", (globs, globs));
    p.write(
        "pkg/parser/names.py",
        "from pkg.parser.base import Parser\n\n\ndef copy_from(parser: Parser, other: Parser) -> None:\n    for key in vars(other):\n        setattr(parser, key, getattr(other, key))\n\n\ndef attach_anything(obj, name: str) -> None:\n    setattr(obj, name, None)\n",
    );
    assert_eq!(
        codes(&p),
        owned(&[
            (17, "MUT204"),
            (21, "MUT207"),
            (25, "MUT207"),
            (29, "MUT206")
        ])
    );
    p.write(
        "pkg/parser/names.py",
        "from pkg.parser.base import Parser\n\n\ndef attach_named(parser: Parser, name: str) -> None:\n    setattr(parser, name, None)\n",
    );
    assert_eq!(
        codes(&p),
        owned(&[
            (9, "MUT207"),
            (13, "MUT207"),
            (13, "MUT206"),
            (17, "MUT204"),
            (21, "MUT207"),
            (25, "MUT207"),
            (29, "MUT206"),
            (33, "MUT207"),
        ])
    );
}
