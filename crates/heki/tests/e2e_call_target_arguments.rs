//! Call targets with an argument list (`DataType(is_dict=True)`), end to end through the real
//! binary: a call matches only when heki establishes that it passes each constant the list
//! names, by keyword, by position, or as a default; an argument it cannot tell is no match.

mod common;

use common::Project;
use serde_json::Value;

type Report = (String, u64, u64, String, String);

/// `(path, line, column, code, message)` of every report of `p`, sorted.
fn reports(p: &Project) -> Vec<Report> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<Report> = diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().replace('\\', "/"),
                d["line"].as_u64().unwrap(),
                d["column"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
                d["message"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

fn report(path: &str, line: u64, column: u64, code: &str, message: &str) -> Report {
    (
        path.to_owned(),
        line,
        column,
        code.to_owned(),
        message.to_owned(),
    )
}

const TYPES: &str = r#"from __future__ import annotations

from dataclasses import dataclass


class DataType:
    def __init__(self, *, is_dict: bool = False, is_list: bool = False) -> None:
        self.is_dict = is_dict
        self.is_list = is_list


def container_flags(
    data_type: DataType, kind: str = "list", level: int = 0, parent: object = None, tag: str = ""
) -> DataType:
    return data_type


@dataclass
class Flags:
    is_dict: bool = False


class Shape:
    def resize(self, kind: str) -> None:
        pass


class Square(Shape):
    def resize(self, mode: str, kind: str = "dict") -> None:
        pass
"#;

const KINDS: &str = "from typing import Final\n\nDICT: Final = \"dict\"\n";

const PARSER: &str = r#"from __future__ import annotations

from pkg.kinds import DICT
from pkg.types import DataType, Flags, Shape, Square, container_flags


def build_map() -> DataType:
    return DataType(is_dict=True)


def by_keyword() -> DataType:
    return DataType(is_dict=True)


def other_keyword() -> DataType:
    return DataType(is_list=True)


def by_position(dt: DataType) -> DataType:
    return container_flags(dt, "dict")


def other_position(dt: DataType) -> DataType:
    return container_flags(dt, "list")


def by_constant(dt: DataType) -> DataType:
    return container_flags(dt, DICT)


def by_variable(dt: DataType, kind: str) -> DataType:
    return container_flags(dt, kind)


def by_default(dt: DataType) -> DataType:
    return container_flags(dt)


def unpacked(dt: DataType, opts: dict[str, object], args: list[object]) -> None:
    DataType(**opts)
    container_flags(dt, *args)


def integers_and_none(dt: DataType) -> None:
    container_flags(dt, "list", 2)
    container_flags(dt, "list", level=2, parent=None)
    container_flags(dt, level=3)


def without_init() -> None:
    Flags(is_dict=True)
    Flags(True)


def overrides(shape: Shape, square: Square) -> None:
    shape.resize("dict")
    square.resize("wide")
    square.resize("wide", "dict")


def references() -> object:
    return container_flags


def floats(dt: DataType) -> None:
    container_flags(dt, level=2.5)
"#;

fn project(name: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT204"]

[[tool.heki.call-owners]]
name = "dict-builder"
targets = ["pkg.types.DataType(is_dict=True)", 'pkg.types.container_flags(kind="dict")', "Flags(is_dict=True)"]
allowed-callers = ["pkg.parser:build_map"]
references = "calls"

[[tool.heki.call-owners]]
name = "levels"
targets = ["container_flags(level=2, parent=None)", "container_flags( level = 3, tag='' )"]

[[tool.heki.call-owners]]
name = "shapes"
targets = ['Shape.resize(kind="dict")']

[[tool.heki.call-owners]]
name = "glob"
targets = ['pkg.types.container_*(kind="dict")']
"#,
    )
    .write("pkg/__init__.py", "")
    .write("pkg/types.py", TYPES)
    .write("pkg/kinds.py", KINDS)
    .write("pkg/parser.py", PARSER);
    p
}

#[test]
fn a_call_matches_a_target_with_arguments_only_when_it_passes_them() {
    let dict = |line: u64, column: u64, func: &str, verb: &str, target: &str| {
        report(
            "pkg/parser.py",
            line,
            column,
            "MUT204",
            &format!("`{func}` {verb} `{target}` outside the call owners of `dict-builder`"),
        )
    };
    let flags = |line: u64, func: &str| {
        let mut out = vec![dict(
            line,
            12,
            func,
            "calls",
            "pkg.types.container_flags(kind=\"dict\")",
        )];
        out.push(report(
            "pkg/parser.py",
            line,
            12,
            "MUT204",
            &format!(
                "`{func}` calls `pkg.types.container_flags(kind=\"dict\")` outside the call owners of `glob`"
            ),
        ));
        out
    };
    let level = |line: u64, target: &str| {
        report(
            "pkg/parser.py",
            line,
            5,
            "MUT204",
            &format!("`integers_and_none` calls `{target}` outside the call owners of `levels`"),
        )
    };
    let mut expected = vec![
        dict(
            12,
            12,
            "by_keyword",
            "constructs",
            "pkg.types.DataType(is_dict=True)",
        ),
        // By position, with the default `None` of `parent`; by keyword.
        level(45, "container_flags(level=2, parent=None)"),
        level(46, "container_flags(level=2, parent=None)"),
        level(47, "container_flags( level = 3, tag='' )"),
        dict(51, 5, "without_init", "constructs", "Flags(is_dict=True)"),
        report(
            "pkg/parser.py",
            58,
            5,
            "MUT204",
            "`overrides` calls `Shape.resize(kind=\"dict\")` outside the call owners of `shapes`",
        ),
    ];
    expected.extend(flags(20, "by_position"));
    expected.extend(flags(28, "by_constant"));
    expected.sort();
    // Not matched: another keyword or position, a variable, a default heki keeps only by class
    // and truthiness (`"list"`), `*` and `**` unpacking, a position of a class without a project
    // `__init__`, a call whose implementations bind the position to different parameters
    // (`shape.resize("dict")` may run `Square.resize`, whose first parameter is `mode`), a
    // default `"dict"`, a float, and a reference, which passes nothing.
    assert_eq!(reports(&project("call_target_arguments")), expected);
}

const DECIDE: &str = r#"from pkg.types import DataType


def render() -> DataType:
    return make(True)


def plain() -> DataType:
    return make(False)


def make(flag: bool) -> DataType:
    if flag:
        return DataType(is_dict=True)
    return DataType(is_list=True)
"#;

#[test]
fn an_effect_boundary_reaches_a_call_with_the_arguments_of_a_target() {
    let p = Project::new("call_target_arguments_boundary");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"EFF201\"]\n\n[[tool.heki.effect-boundaries]]\nname = \"pure\"\nscopes = [\"pkg.view.render\", \"pkg.view.plain\"]\nforbid = [\"calls\"]\ncall-targets = [\"pkg.types.DataType(is_dict=True)\", 'os.getenv(key=\"HOME\")']\n",
    )
    .write("pkg/__init__.py", "")
    .write("pkg/types.py", TYPES)
    .write(
        "pkg/view.py",
        &DECIDE.replace(
            "def make(",
            "import os\n\n\ndef home() -> object:\n    return os.getenv(\"HOME\")\n\n\ndef make(",
        ),
    );
    let reached = |func: &str| {
        report(
            "pkg/view.py",
            if func == "render" { 5 } else { 9 },
            12,
            "EFF201",
            &format!(
                "`{func}` constructs `pkg.types.DataType(is_dict=True)` (via `pkg.view.make`), which effect boundary `pure` forbids"
            ),
        )
    };
    // A boundary reaches every call of its scope functions' callees, whichever branch runs; a
    // target with an argument list names nothing outside the project.
    assert_eq!(reports(&p), vec![reached("render"), reached("plain")]);
}

#[test]
fn an_unresolved_call_is_named_by_the_target_without_its_arguments() {
    let p = Project::new("call_target_arguments_unresolved");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\", \"MUT207\"]\n\n[[tool.heki.call-owners]]\nname = \"shapes\"\ntargets = ['Shape.resize(kind=\"dict\")']\nunresolved = \"report\"\n",
    )
    .write("pkg/__init__.py", "")
    .write("pkg/types.py", TYPES)
    .write(
        "pkg/use.py",
        "def stretch(obj) -> None:\n    obj.resize(kind=\"list\")\n",
    );
    assert_eq!(
        reports(&p),
        vec![report(
            "pkg/use.py",
            2,
            5,
            "MUT207",
            "`stretch` calls `resize` on an object of unknown class outside the call owners of `shapes`: heki cannot tell whether it calls `Shape.resize(kind=\"dict\")`",
        )]
    );
}

#[test]
fn an_argument_list_heki_cannot_read_is_a_configuration_error() {
    for (i, target) in [
        "DataType(is_dict=True",
        "DataType()",
        "DataType(True)",
        "DataType(is_dict=yes)",
        "DataType(is_dict=True, is_dict=False)",
        "DataType(is_dict=True is_list=False)",
        r#"DataType(name="a\\b")"#,
        "DataType(1x=2)",
    ]
    .into_iter()
    .enumerate()
    {
        let p = Project::new(&format!("call_target_arguments_invalid_{i}"));
        p.write(
            "pyproject.toml",
            &format!(
                "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"c\"\ntargets = ['{target}']\n"
            ),
        )
        .write("pkg/__init__.py", "")
        .write("pkg/types.py", TYPES);
        let (code, stdout, stderr) = p.check(&[]);
        assert_eq!(code, 2, "{target}: {stdout}{stderr}");
        assert!(
            stderr.contains("invalid argument list in call-owners.targets target"),
            "{target}: {stderr}"
        );
    }
}
