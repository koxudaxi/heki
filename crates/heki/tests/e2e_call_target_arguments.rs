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

const SIGNATURES: &str = r#"def po_kw(kind=False, /, **kwargs):
    return kind


def po(kind=False, /):
    return kind


def ko(x=None, *, kind=False):
    return kind


def ko_strict(*, kind=False):
    return kind


def ko_args(*args, kind=False):
    return kind


def plain(kind=False):
    return kind


def opts(**options):
    return options


def va(*kind, **kw):
    return kind


def vk(**kind):
    return kind


class Box:
    def __init__(self, kind=False, /, **kw):
        self.kind = kind

    def put(self, kind=False, /, **kw):
        return kind

    def take(self, kind=False):
        return kind

    @staticmethod
    def make(kind=False, /):
        return kind
"#;

const BINDINGS: &str = r#"from pkg.sig import Box, ko, ko_args, ko_strict, opts, plain, po, po_kw, va, vk


def keyword_of_positional_only():
    po_kw(kind=True)


def position_of_positional_only():
    po_kw(True, kind=False)


def positional_only_without_kwargs():
    po(kind=True)
    po(True)


def keyword_only():
    ko(kind=True)
    ko(True)
    ko_strict(True)
    ko_args(True)


def raising():
    plain(False, kind=True)
    plain(kind=True, other=1)


def into_kwargs():
    opts(kind=True)
    va(kind=True)
    vk(kind=True)


def receivers(b: Box):
    b.put(True)
    b.put(kind=True)
    b.make(True)
    Box(True)
    Box(kind=True)
    Box.take(b, True)
    b.take(False, kind=True)


def unpacking(args, kw):
    plain(*args, kind=True)
    plain(kind=True, **kw)
    plain(*args)
    plain(**kw)
"#;

#[test]
fn a_keyword_binds_a_parameter_only_where_python_binds_it() {
    let names = "po_kw po ko ko_strict ko_args plain opts va vk Box Box.put Box.take Box.make";
    let targets = |value: &str| {
        let texts: Vec<String> = names
            .split(' ')
            .map(|n| format!("\"{n}(kind={value})\""))
            .collect();
        texts.join(", ")
    };
    let p = Project::new("call_target_arguments_bindings");
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\", \"EFF201\"]\n\n[[tool.heki.call-owners]]\nname = \"on\"\ntargets = [{}]\n\n[[tool.heki.call-owners]]\nname = \"off\"\ntargets = [{}]\n\n[[tool.heki.effect-boundaries]]\nname = \"pure\"\nscopes = [\"pkg.use.keyword_of_positional_only\"]\nforbid = [\"calls\"]\ncall-targets = [\"po_kw(kind=True)\", \"po_kw(kind=False)\"]\n",
            targets("True"),
            targets("False")
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/sig.py", SIGNATURES)
    .write("pkg/use.py", BINDINGS);
    let call = |line: u64, func: &str, target: &str, value: &str| {
        let verb = if target == "Box" {
            "constructs"
        } else {
            "calls"
        };
        let contract = if value == "True" { "on" } else { "off" };
        report(
            "pkg/use.py",
            line,
            5,
            "MUT204",
            &format!(
                "`{func}` {verb} `{target}(kind={value})` outside the call owners of `{contract}`"
            ),
        )
    };
    let mut expected = vec![
        // The keyword lands in `**kwargs`: `kind` keeps its default.
        call(5, "keyword_of_positional_only", "po_kw", "False"),
        report(
            "pkg/use.py",
            5,
            5,
            "EFF201",
            "`keyword_of_positional_only` calls `po_kw(kind=False)`, which effect boundary `pure` forbids",
        ),
        // The position binds `kind`, the keyword lands in `**kwargs`.
        call(9, "position_of_positional_only", "po_kw", "True"),
        call(14, "positional_only_without_kwargs", "po", "True"),
        call(18, "keyword_only", "ko", "True"),
        call(19, "keyword_only", "ko", "False"),
        call(21, "keyword_only", "ko_args", "False"),
        // A keyword naming no parameter is one of `**kwargs`.
        call(30, "into_kwargs", "opts", "True"),
        call(36, "receivers", "Box.put", "True"),
        call(37, "receivers", "Box.put", "False"),
        call(38, "receivers", "Box.make", "True"),
        call(39, "receivers", "Box", "True"),
        call(40, "receivers", "Box", "False"),
        call(41, "receivers", "Box.take", "True"),
        call(46, "unpacking", "plain", "True"),
        call(47, "unpacking", "plain", "True"),
    ];
    expected.sort();
    // Nothing for the calls that raise `TypeError` (a keyword naming a positional-only parameter
    // without `**kwargs`, a position past the parameters without `*args`, a parameter bound by
    // a position and a keyword, a keyword naming no parameter without `**kwargs`), for a keyword
    // landing in `**kwargs` beside `*args` or `**kwargs` of its name, and for an argument
    // unpacking may pass.
    assert_eq!(reports(&p), expected);
}

const SETTINGS: &str = r#"from typing import Final

LIMIT = 0
STRICT: Final = 3
RATE = 0.5
MOVED = 0
TWICE = 0
TWICE = 1
"#;

const CONSTANTS: &str = r#"from pkg.settings import LIMIT, MOVED, RATE, STRICT, TWICE
from pkg.sig import levels

FLAG = True
TOGGLE = True


def flip():
    global TOGGLE
    TOGGLE = False


def own_constant():
    levels(kind=FLAG)
    levels(FLAG)


def imported_constants():
    levels(level=LIMIT)
    levels(level=STRICT)


def no_constants():
    levels(level=MOVED)
    levels(level=TWICE)
    levels(kind=TOGGLE)
    levels(level=RATE)
    levels(level=FLAG)
"#;

#[test]
fn a_module_constant_passes_its_value() {
    let p = Project::new("call_target_arguments_constants");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"c\"\ntargets = [\"levels(kind=True)\", \"levels(kind=False)\", \"levels(level=0)\", \"levels(level=1)\", \"levels(level=3)\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write(
        "pkg/sig.py",
        "def levels(kind=None, level=None):\n    return level\n",
    )
    .write("pkg/settings.py", SETTINGS)
    .write(
        "pkg/rebind.py",
        "from pkg import settings\n\n\ndef move():\n    settings.MOVED = 3\n",
    )
    .write("pkg/use.py", CONSTANTS);
    let call = |line: u64, func: &str, target: &str| {
        report(
            "pkg/use.py",
            line,
            5,
            "MUT204",
            &format!("`{func}` calls `levels({target})` outside the call owners of `c`"),
        )
    };
    // Not matched: a constant another module rebinds, one bound twice, one a function of its
    // module rebinds, a float, and a boolean for an integer.
    assert_eq!(
        reports(&p),
        vec![
            call(14, "own_constant", "kind=True"),
            call(15, "own_constant", "kind=True"),
            call(19, "imported_constants", "level=0"),
            call(20, "imported_constants", "level=3"),
        ]
    );
}

const REBINDING_FLIPPED: &str = r#"from pkg.settings import FLAG
from pkg.sig import levels


def flip():
    global FLAG
    FLAG = False


def caller():
    flip()
    levels(kind=FLAG)
"#;

const REBINDING_TWICE: &str = r#"import os

from pkg.settings import LIMIT
from pkg.sig import levels

if os.environ.get("X"):
    LIMIT = 1


def caller():
    levels(level=LIMIT)
"#;

const REBINDING_DELETED: &str = r#"from pkg.settings import LIMIT
from pkg.sig import levels


def drop():
    global LIMIT
    del LIMIT


def caller():
    levels(level=LIMIT)
"#;

const REBINDING_STARRED: &str = r#"from pkg.settings import LIMIT
from pkg.other import *
from pkg.sig import levels


def caller():
    levels(level=LIMIT)
"#;

#[test]
fn an_imported_constant_its_module_binds_again_passes_nothing() {
    let user = |module: &str, name: &str, argument: &str| {
        format!(
            "from {module} import {name}\nfrom pkg.sig import levels\n\n\ndef caller():\n    levels({argument}={name})\n"
        )
    };
    let p = Project::new("call_target_arguments_rebound_imports");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"c\"\ntargets = [\"levels(kind=True)\", \"levels(level=0)\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write(
        "pkg/sig.py",
        "def levels(kind=None, level=None):\n    return level\n",
    )
    .write("pkg/settings.py", "FLAG = True\nLIMIT = 0\n")
    .write(
        "pkg/other.py",
        "LIMIT = 5\n\n\ndef bump():\n    global LIMIT\n    LIMIT = 6\n",
    )
    .write("pkg/flipped.py", REBINDING_FLIPPED)
    .write("pkg/twice.py", REBINDING_TWICE)
    .write("pkg/deleted.py", REBINDING_DELETED)
    .write("pkg/starred.py", REBINDING_STARRED)
    .write("pkg/kept.py", &user("pkg.settings", "LIMIT", "level"))
    .write(
        "pkg/relay/__init__.py",
        "from pkg.settings import FLAG\n\n\ndef reset():\n    global FLAG\n    FLAG = False\n",
    )
    .write("pkg/relayed.py", &user("pkg.relay", "FLAG", "kind"))
    .write("pkg/plain_relay/__init__.py", "from pkg.settings import FLAG\n")
    .write("pkg/plain_relayed.py", &user("pkg.plain_relay", "FLAG", "kind"));
    let call = |path: &str, target: &str| {
        report(
            path,
            6,
            5,
            "MUT204",
            &format!("`caller` calls `levels({target})` outside the call owners of `c`"),
        )
    };
    // Not matched: a name the importing module binds again in a function (`global`), in a second
    // module-level store, or deletes, one a star import may rebind, and one a module re-exporting
    // it binds again. Another module binding a global of the same name of its own changes
    // nothing here.
    assert_eq!(
        reports(&p),
        vec![
            call("pkg/kept.py", "level=0"),
            call("pkg/plain_relayed.py", "kind=True"),
        ]
    );
}

const REQUIRED: &str = r#"def guarded(required, *, kind=True):
    return required


def keyed(*, need, kind=True):
    return need


class Box:
    def check(*args, kind=False):
        return args

    def strict(*, kw):
        return kw

    def plain(self, x):
        return x
"#;

const REQUIRED_USE: &str = r#"from pkg.sig import Box, guarded, keyed


def missing(args):
    guarded(kind=True)
    guarded(1, kind=True)
    guarded(required=1, kind=True)
    keyed(kind=True)
    keyed(need=1, kind=True)
    guarded(*args, kind=True)


def receivers(b: Box):
    b.check(kind=True)
    b.strict(kw=1)
    b.plain(x=1)
    b.plain(self=b, x=1)
"#;

#[test]
fn a_call_leaving_a_parameter_unbound_passes_nothing() {
    let p = Project::new("call_target_arguments_required");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"c\"\ntargets = [\"guarded(kind=True)\", \"keyed(kind=True)\", \"Box.check(kind=True)\", \"Box.strict(kw=1)\", \"Box.plain(x=1)\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write("pkg/sig.py", REQUIRED)
    .write("pkg/use.py", REQUIRED_USE);
    let call = |line: u64, func: &str, target: &str| {
        report(
            "pkg/use.py",
            line,
            5,
            "MUT204",
            &format!("`{func}` calls `{target}` outside the call owners of `c`"),
        )
    };
    // Nothing for the calls that raise `TypeError`: a parameter without a default left unbound
    // (positional or keyword-only), a receiver no positional parameter or `*args` takes, and a
    // keyword naming the receiver's parameter. `*` unpacking may bind the parameter, and the
    // receiver of a method taking only `*args` and keywords lands in `*args`.
    assert_eq!(
        reports(&p),
        vec![
            call(6, "missing", "guarded(kind=True)"),
            call(7, "missing", "guarded(kind=True)"),
            call(9, "missing", "keyed(kind=True)"),
            call(10, "missing", "guarded(kind=True)"),
            call(14, "receivers", "Box.check(kind=True)"),
            call(16, "receivers", "Box.plain(x=1)"),
        ]
    );
}

const READ_FORMS: &str = r#"import pkg.settings as cfg
from pkg import settings
from pkg.sig import levels


def by_attribute():
    levels(kind=settings.FLAG)
    levels(level=cfg.LIMIT)


def by_local_import():
    from pkg.settings import FLAG, LIMIT
    levels(kind=FLAG)
    levels(level=LIMIT)


def rebound_elsewhere():
    levels(level=settings.MOVED)


class Holder:
    FLAG = True

    def __init__(self) -> None:
        self.flag = True

    def by_instance(self):
        levels(kind=self.flag)


def by_class():
    levels(kind=Holder.FLAG)
"#;

#[test]
fn a_module_constant_read_as_an_attribute_or_a_local_import_passes_its_value() {
    let p = Project::new("call_target_arguments_read_forms");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"c\"\ntargets = [\"levels(kind=True)\", \"levels(level=0)\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write(
        "pkg/sig.py",
        "def levels(kind=None, level=None):\n    return level\n",
    )
    .write("pkg/settings.py", "FLAG = True\nLIMIT = 0\nMOVED = 0\n")
    .write(
        "pkg/mover.py",
        "from pkg import settings\n\n\ndef move():\n    settings.MOVED = 5\n",
    )
    .write("pkg/use.py", READ_FORMS);
    let call = |line: u64, func: &str, target: &str| {
        report(
            "pkg/use.py",
            line,
            5,
            "MUT204",
            &format!("`{func}` calls `levels({target})` outside the call owners of `c`"),
        )
    };
    // Not matched: an attribute another module rebinds, and an attribute of a class or an
    // instance (no module constant).
    assert_eq!(
        reports(&p),
        vec![
            call(7, "by_attribute", "kind=True"),
            call(8, "by_attribute", "level=0"),
            call(13, "by_local_import", "kind=True"),
            call(14, "by_local_import", "level=0"),
        ]
    );
}

const DISPLAY_SIGNATURES: &str = r#"from dataclasses import dataclass
from typing import Final

KEY: Final = "kind"


def plain(kind=False):
    return kind


def two(kind=False, other=0):
    return kind, other


def opts(**options):
    return options


def required(value, *, kind=False):
    return value


def mutate(flags: dict) -> None:
    flags["kind"] = False


@dataclass
class Record:
    kind: bool = False
"#;

/// `**` operands: dict displays with constant keys (inline, through copies, a module constant as
/// a key, a key given twice), beside written keywords, landing in `**kwargs`, binding a required
/// parameter, and constructing a dataclass; displays something changes before the call, a
/// parameter, calls that raise `TypeError`, `dict(...)`, a display merging a parameter before its
/// key, the same display on two paths, and a display tested and returned.
const DISPLAY_USE: &str = r#"from pkg.sig import KEY, Record, mutate, opts, plain, required, two


def inline():
    plain(**{"kind": True})


def aliased():
    flags = {"kind": True}
    plain(**flags)


def copied():
    flags = {"kind": True}
    again = flags
    plain(**again)


def constant_key():
    plain(**{KEY: True})


def later_entry():
    plain(**{"kind": False, "kind": True})


def with_keywords():
    two(kind=True, **{"other": 1})
    two(**{"kind": True}, other=1)


def into_kwargs():
    opts(**{"kind": True})


def binds_required():
    required(**{"value": 1, "kind": True})


def dataclass_fields():
    Record(**{"kind": True})


def changed_by_item():
    flags = {"kind": True}
    flags["kind"] = False
    plain(**flags)


def changed_by_method():
    flags = {"kind": True}
    flags.update(kind=False)
    plain(**flags)


def changed_by_delete():
    flags = {"kind": True}
    del flags["kind"]
    plain(**flags)


def changed_by_callee():
    flags = {"kind": True}
    mutate(flags)
    plain(**flags)


def computed_key(name: str):
    plain(**{name: True})


def dict_call_and_parameter(kw: dict):
    plain(**dict(kind=True))
    plain(**kw)


def merged_before_the_key(base: dict):
    plain(**{**base, "kind": True})


def same_on_both_paths(flag: bool):
    flags = {"kind": True} if flag else {"kind": True}
    plain(**flags)


def captured():
    flags = {"kind": True}

    def change() -> None:
        flags["kind"] = False

    change()
    plain(**flags)


def repeated():
    plain(kind=True, **{"kind": True})
    plain(**{"kind": True}, **{"kind": True})
    Record(kind=True, **{"kind": True})


def unknown_name():
    plain(**{"other": 1, "kind": True})


def unbound_required():
    required(**{"kind": True})


def rebound_later(flag: bool):
    flags = {"kind": True}
    plain(**flags)
    if flag:
        flags = {"kind": False}
    plain(**flags)


def tested_and_returned():
    flags = {"kind": True}
    if flags:
        plain(**flags)
    return flags
"#;

/// A `**` operand that is a dict display with string constant keys, which nothing changes (each
/// value holding it is only copied, merged, unpacked with `**`, tested, or returned), binds its
/// keys as written keywords bind, a later entry of a key replacing an earlier one. A mapping heki
/// does not build (a parameter) may pass any keyword, so it claims no argument, but a key after it
/// is passed; a key given twice (by two operands, or by an operand and a written keyword) makes
/// the call raise `TypeError`, so it passes nothing.
#[test]
fn a_constant_dict_display_unpacked_with_two_stars_binds_its_keys_as_keywords() {
    let p = Project::new("call_target_arguments_displays");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"c\"\ntargets = [\"plain(kind=True)\", \"two(other=1)\", \"opts(kind=True)\", \"required(kind=True)\", \"Record(kind=True)\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write("pkg/sig.py", DISPLAY_SIGNATURES)
    .write("pkg/use.py", DISPLAY_USE);
    let call = |line: u64, func: &str, target: &str| {
        let verb = if target.starts_with("Record") {
            "constructs"
        } else {
            "calls"
        };
        report(
            "pkg/use.py",
            line,
            5,
            "MUT204",
            &format!("`{func}` {verb} `{target}` outside the call owners of `c`"),
        )
    };
    assert_eq!(
        reports(&p),
        vec![
            call(5, "inline", "plain(kind=True)"),
            call(10, "aliased", "plain(kind=True)"),
            call(16, "copied", "plain(kind=True)"),
            call(20, "constant_key", "plain(kind=True)"),
            call(24, "later_entry", "plain(kind=True)"),
            call(28, "with_keywords", "two(other=1)"),
            call(29, "with_keywords", "two(other=1)"),
            call(33, "into_kwargs", "opts(kind=True)"),
            call(37, "binds_required", "required(kind=True)"),
            call(41, "dataclass_fields", "Record(kind=True)"),
            call(73, "dict_call_and_parameter", "plain(kind=True)"),
            call(78, "merged_before_the_key", "plain(kind=True)"),
            call(83, "same_on_both_paths", "plain(kind=True)"),
            call(112, "rebound_later", "plain(kind=True)"),
            report(
                "pkg/use.py",
                121,
                9,
                "MUT204",
                "`tested_and_returned` calls `plain(kind=True)` outside the call owners of `c`",
            ),
        ]
    );
}

/// Mappings built from others or on several paths, unpacked with `**`: merged displays (a later
/// entry replacing an earlier one, a parameter merged before or after the key, a display merging
/// a parameter merged in turn), `dict(...)` of a display, of a parameter with a keyword, and of
/// pairs, `a | b` of displays, of a merge, a `dict(...)` call, a phi, and a parameter, a phi of
/// displays agreeing on the key or not or of a parameter, a loop merging the mapping into itself
/// with another key or the same one, a mapping or a merged display changed in place, a mapping
/// asked again inside another, a call heki cannot resolve, more ways than are followed (a merge
/// of four mappings of two ways each, a phi of nine displays), `dict(**m)`, and a mapping also
/// unpacked into a project method.
const MERGED_USE: &str = r#"from pkg.sig import KEY, Record, opts, two


def merged():
    a = {"kind": True}
    b = {"other": 1}
    two(**{**a, **b})


def merged_later_wins():
    two(**{**{"kind": False}, **{"kind": True}})


def merged_earlier_loses():
    two(**{**{"kind": True}, **{"kind": False}})


def unknown_after_the_key(base: dict):
    two(**{"kind": True, **base})


def dict_of_a_display():
    two(**dict({"kind": True}))


def dict_with_keywords(base: dict):
    two(**dict(base, kind=True))


def dict_of_pairs(pairs: list):
    two(**dict(pairs))


def union():
    two(**({"other": 1} | {"kind": True}))


def union_replaced():
    two(**({"kind": True} | {"kind": False}))


def union_of_a_parameter(base: dict):
    two(**(base | {"kind": True}))


def both_paths(flag: bool):
    if flag:
        flags = {"kind": True, "other": 1}
    else:
        flags = {KEY: True}
    two(**flags)


def paths_disagree(flag: bool):
    flags = {"kind": True} if flag else {"other": 1}
    two(**flags)


def path_of_a_parameter(flag: bool, base: dict):
    flags = {"kind": True} if flag else base
    two(**flags)


def loop_adds_keys(n: int):
    flags = {"kind": True}
    for i in range(n):
        flags = {**flags, "other": i}
    two(**flags)


def loop_replaces_the_key(n: int):
    flags = {"kind": True}
    for _ in range(n):
        flags = {**flags, "kind": False}
    two(**flags)


def merged_then_changed():
    flags = {**{"kind": True}}
    flags["kind"] = False
    two(**flags)


def source_changed():
    a = {"kind": True}
    a["kind"] = False
    two(**{**a})


def source_kept_in_another():
    a = {"kind": True}
    holder = {"a": a}
    holder["a"]["kind"] = False
    two(**{**a})


def dataclass_on_both_paths(flag: bool):
    flags = {"kind": True} if flag else {KEY: True}
    Record(**flags)


def merged_open_mapping(base: dict):
    two(**{"other": 1, **{**base, "kind": True}})


def union_of_a_union():
    two(**(({"other": 1} | {"other": 2}) | {"kind": True}))


def union_of_a_call():
    two(**(dict(other=1) | {"kind": True}))


def union_of_paths(flag: bool):
    two(**(({"other": 1} if flag else {"other": 2}) | {"kind": True}))


def asked_in_order():
    a = {"kind": True}
    two(**a)
    two(**{**a, "other": 1})


def unknown_receiver(obj):
    obj.put(**{"kind": True})


def many_ways(a: bool, b: bool, c: bool, d: bool):
    f = {"kind": True, "w": 0} if a else {"kind": True, "w": 1}
    g = {"x": 0} if b else {"x": 1}
    h = {"y": 0} if c else {"y": 1}
    i = {"z": 0} if d else {"z": 1}
    opts(**{**f, **g, **h, **i})


def nine_paths(n: int):
    if n == 0:
        flags = {"kind": True, "other": 0}
    elif n == 1:
        flags = {"kind": True, "other": 1}
    elif n == 2:
        flags = {"kind": True, "other": 2}
    elif n == 3:
        flags = {"kind": True, "other": 3}
    elif n == 4:
        flags = {"kind": True, "other": 4}
    elif n == 5:
        flags = {"kind": True, "other": 5}
    elif n == 6:
        flags = {"kind": True, "other": 6}
    elif n == 7:
        flags = {"kind": True, "other": 7}
    else:
        flags = {"kind": True, "other": 8}
    opts(**flags)


def dict_of_an_unpacked_display():
    two(**dict(**{"kind": True}))


class Holder:
    def put(self, kind=False):
        return kind


def unpacked_into_a_method():
    flags = {"kind": True}
    Holder().put(**flags)
    two(**flags)
"#;

/// A mapping merged from others (`{**a, **b}`, `dict(a, k=v)`, `a | b` of mappings the function
/// builds) passes the entries of the last one of each key, and one heki does not build replaces
/// every key before it; a phi of mappings passes a key where every input passes the same
/// constant for it, as two callees must agree. A mapping something changes, or one merged from
/// a mapping something changes, passes nothing heki claims, nor does one of more than eight
/// ways.
#[test]
fn mappings_merged_or_built_on_several_paths_pass_the_keys_every_way_agrees_on() {
    let p = Project::new("call_target_arguments_merged");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"c\"\ntargets = [\"two(kind=True)\", \"Record(kind=True)\", \"opts(kind=True)\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write("pkg/sig.py", DISPLAY_SIGNATURES)
    .write("pkg/use.py", MERGED_USE);
    let call = |line: u64, func: &str| {
        report(
            "pkg/use.py",
            line,
            5,
            "MUT204",
            &format!("`{func}` calls `two(kind=True)` outside the call owners of `c`"),
        )
    };
    assert_eq!(
        reports(&p),
        vec![
            call(7, "merged"),
            call(11, "merged_later_wins"),
            call(23, "dict_of_a_display"),
            call(27, "dict_with_keywords"),
            call(35, "union"),
            call(51, "both_paths"),
            call(68, "loop_adds_keys"),
            report(
                "pkg/use.py",
                99,
                5,
                "MUT204",
                "`dataclass_on_both_paths` constructs `Record(kind=True)` outside the call owners of `c`",
            ),
            call(103, "merged_open_mapping"),
            call(107, "union_of_a_union"),
            call(111, "union_of_a_call"),
            call(115, "union_of_paths"),
            call(120, "asked_in_order"),
            call(121, "asked_in_order"),
            call(159, "dict_of_an_unpacked_display"),
            call(170, "unpacked_into_a_method"),
        ]
    );
}

/// Calls of `functools.partial` objects the function makes: holding a keyword, a position, or a
/// mapping, passed the keyword at the call, nested, constructing a class, a keyword replaced at
/// the call, a mapping unpacked at the call over held keywords, objects the function does not
/// make here (a parameter, one of two on different paths), one returned uncalled, and positions
/// unpacked with `*` in the object or at the call.
const PARTIAL_USE: &str = r#"import functools
from functools import partial

from pkg.sig import Record, plain, two


def held_keyword():
    make = functools.partial(plain, kind=True)
    make()


def held_position():
    partial(plain, True)()


def passed_at_the_call():
    make = partial(two)
    make(kind=True)


def nested():
    make = partial(partial(two, other=1), kind=True)
    make()


def held_mapping():
    make = partial(two, **{"kind": True})
    make(other=2)


def constructs():
    make = partial(Record, kind=True)
    return make()


def replaced_at_the_call():
    make = partial(plain, kind=True)
    make(kind=False)


def unpacked_at_the_call(opts: dict):
    make = partial(plain, kind=True)
    make(**opts)


def handed_in(make):
    make()


def made_on_two_paths(flag: bool):
    make = partial(plain, kind=True) if flag else partial(plain, kind=True)
    make()


def kept_for_later():
    return partial(Record, kind=True)


def positions_unpacked_in_the_object(xs: list):
    partial(plain, *xs)()


def positions_unpacked_at_the_call(xs: list):
    make = partial(two, kind=True)
    make(*xs)
"#;

/// Calling a `functools.partial` object calls the callable it holds with the positions it holds
/// first and its keywords, which a keyword the call writes replaces: the call of the object is
/// the call site, where the callable runs (the `partial(...)` call only keeps the arguments, so
/// with `references = "calls"` it is no reference there). An object the function does not make
/// through copies, or a call unpacking a mapping that may replace the keywords it holds, is not
/// followed; one only returned is a reference, which passes no arguments.
#[test]
fn a_call_of_a_partial_object_passes_what_the_object_holds() {
    let p = Project::new("call_target_arguments_partial");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"c\"\ntargets = [\"plain(kind=True)\", \"two(kind=True)\", \"Record(kind=True)\"]\n\n[[tool.heki.call-owners]]\nname = \"records\"\ntargets = [\"pkg.sig.Record\"]\nreferences = \"calls\"\n",
    )
    .write("pkg/__init__.py", "")
    .write("pkg/sig.py", DISPLAY_SIGNATURES)
    .write("pkg/use.py", PARTIAL_USE);
    let call = |line: u64, column: u64, func: &str, target: &str| {
        report(
            "pkg/use.py",
            line,
            column,
            "MUT204",
            &format!("`{func}` calls `{target}` outside the call owners of `c`"),
        )
    };
    assert_eq!(
        reports(&p),
        vec![
            call(9, 5, "held_keyword", "plain(kind=True)"),
            call(13, 5, "held_position", "plain(kind=True)"),
            call(18, 5, "passed_at_the_call", "two(kind=True)"),
            call(23, 5, "nested", "two(kind=True)"),
            call(28, 5, "held_mapping", "two(kind=True)"),
            report(
                "pkg/use.py",
                33,
                12,
                "MUT204",
                "`constructs` constructs `Record(kind=True)` outside the call owners of `c`",
            ),
            report(
                "pkg/use.py",
                33,
                12,
                "MUT204",
                "`constructs` constructs `pkg.sig.Record` outside the call owners of `records`",
            ),
            report(
                "pkg/use.py",
                56,
                20,
                "MUT204",
                "`kept_for_later` hands on `pkg.sig.Record` outside the call owners of `records`",
            ),
            call(65, 5, "positions_unpacked_at_the_call", "two(kind=True)"),
        ]
    );
}

const MODELS: &str = r#"from pydantic import BaseModel


class DataType(BaseModel):
    is_dict: bool = False
    is_list: bool = False


class MapType(DataType):
    @classmethod
    def of_map(cls) -> "MapType":
        return cls.model_construct(is_dict=True)


class Reference(BaseModel):
    path: str = ""
"#;

/// Pydantic's builders of a model without its `__init__`: `model_construct` with a keyword, a
/// mapping, another keyword, and through `cls`; `model_validate` of a display, by keyword, with
/// another value, of a parameter annotated `dict`, of an unannotated one, and of an instance;
/// `model_copy` with `update` and without, on a value of either of two models; and
/// `model_construct` of the library's class.
const MODEL_USE: &str = r#"from pydantic import BaseModel

from pkg.models import DataType, Reference


def construct():
    return DataType.model_construct(is_dict=True)


def construct_from_a_mapping():
    flags = {"is_dict": True}
    return DataType.model_construct(**flags)


def construct_other():
    return DataType.model_construct(is_list=True)


def validate():
    return DataType.model_validate({"is_dict": True})


def validate_by_keyword():
    return DataType.model_validate(obj={"is_dict": True})


def validate_other():
    return DataType.model_validate({"is_dict": False})


def validate_a_dict(data: dict):
    return Reference.model_validate(data)


def validate_anything(data):
    return Reference.model_validate(data)


def validate_an_instance(ref: Reference):
    return Reference.model_validate(ref)


def copy_with_update(data_type: DataType):
    return data_type.model_copy(update={"is_dict": True})


def copy_without(data_type: DataType):
    return data_type.model_copy()


def copy_reference(ref: Reference):
    return ref.model_copy()


def copy_either(model: DataType | Reference):
    return model.model_copy()


def library_class():
    return BaseModel.model_construct()
"#;

/// The catalog's builders of a pydantic model (Layer 2 `builds_receiver`) construct the class of
/// their receiver from the field values they take: `model_construct` from its keywords,
/// `model_validate` from the dict it is passed (given anything else, it may return an instance
/// as it is, and builds nothing heki claims), and `model_copy` from `update`. A class target
/// counts each of them as a construction, and one with an argument list each passing its
/// constants.
#[test]
fn a_pydantic_builder_constructs_its_receivers_class_from_the_values_it_takes() {
    let p = Project::new("call_target_arguments_pydantic");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"maps\"\ntargets = [\"pkg.models.DataType(is_dict=True)\"]\n\n[[tool.heki.call-owners]]\nname = \"references\"\ntargets = [\"pkg.models.Reference\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write("pkg/models.py", MODELS)
    .write("pkg/use.py", MODEL_USE);
    let built = |path: &str, line: u64, func: &str, target: &str, contract: &str| {
        report(
            path,
            line,
            16,
            "MUT204",
            &format!("`{func}` constructs `{target}` outside the call owners of `{contract}`"),
        )
    };
    let map = |line: u64, func: &str| {
        let mut r = built(
            "pkg/use.py",
            line,
            func,
            "pkg.models.DataType(is_dict=True)",
            "maps",
        );
        r.2 = 12;
        r
    };
    let reference = |line: u64, func: &str| {
        let mut r = built(
            "pkg/use.py",
            line,
            func,
            "pkg.models.Reference",
            "references",
        );
        r.2 = 12;
        r
    };
    assert_eq!(
        reports(&p),
        vec![
            built(
                "pkg/models.py",
                12,
                "MapType.of_map",
                "pkg.models.DataType(is_dict=True)",
                "maps"
            ),
            map(7, "construct"),
            map(12, "construct_from_a_mapping"),
            map(20, "validate"),
            map(24, "validate_by_keyword"),
            reference(32, "validate_a_dict"),
            map(44, "copy_with_update"),
            reference(52, "copy_reference"),
        ]
    );
}
