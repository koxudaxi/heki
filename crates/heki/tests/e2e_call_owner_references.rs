//! `references = "calls"` on `call-owners` and on the `calls` token of effect boundaries, end to
//! end through the real binary: a reference handing a target on to be called later (passed,
//! stored, returned, yielded, raised, held by a display) counts as a call of it, while a use that
//! only names a class (`isinstance`, `except`, a base class, `cast`, a typing subscript, an
//! annotation, a comparison) does not. Without the key only calls are reported. With
//! `references = "calls-and-bases"`, a class statement naming a class target among its bases
//! counts too.

mod common;

use common::Project;
use serde_json::Value;

const CORE: &str = r#"class Rule:
    def check(self) -> bool:
        return True


class Error(Exception):
    pass


def decide() -> int:
    return 1


def register(callback: object) -> None:
    pass
"#;

// A module alias of the function (`run` is `pkg.core.decide` wherever it is used).
const HELPERS: &str = "from pkg import core\n\nrun = core.decide\n";

const SITE: &str = r#"import functools
import itertools
from contextlib import suppress
from dataclasses import dataclass, field
from typing import Optional, TypeVar, cast

from pkg import core
from pkg.core import Error, Rule, decide, register
from pkg.core import decide as chosen
from pkg.helpers import run

T = TypeVar("T", bound=Rule)
Maybe = Optional[Rule]
HANDLERS = {"a": decide}
ALIAS = decide


@dataclass
class Holder:
    rule: Rule = field(default_factory=Rule)


class Sub(Rule):
    pass


def as_key(values: list[int]) -> list[int]:
    return sorted(values, key=decide)


def as_value() -> object:
    return decide


def in_display() -> list[object]:
    return [decide]


def as_partial() -> object:
    return functools.partial(decide)


def as_starmap(pairs: list[tuple[int, int]]) -> object:
    return itertools.starmap(decide, pairs)


def through_aliases() -> tuple[object, object, object]:
    return chosen, run, core.decide


def stored(obj: object) -> None:
    obj.callback = decide
    register(decide)


def yielded() -> object:
    yield decide


def raised() -> None:
    raise Error


def per_item_class(values: list[int]) -> object:
    return list(map(Rule, values))


def _make() -> object:
    return Rule


def build() -> object:
    return _make()


def method_refs(rule: Rule) -> tuple[object, object]:
    return rule.check, Rule.check


def names_only(x: object, kind: type, f: object) -> bool:
    try:
        cast(Rule, x)
    except (Error, OSError):
        pass
    with suppress(Error):
        print(decide)
    decide
    return isinstance(x, Rule) or isinstance(x, (Rule, Error)) or issubclass(kind, Rule) or f is decide


def annotated(rule: Rule) -> Rule:
    local: Optional[Rule] = rule
    return local


def called() -> int:
    return decide()


def not_targets(values: list[int]) -> object:
    register(T)
    register(len)
    return values.copy


def partial_key(values: list[int]) -> list[int]:
    return sorted(values, key=functools.partial(decide))


LATE = core.decide
LATER = [LATE]

if HANDLERS:
    PICKED = decide
else:
    PICKED = None
CHOSEN = [PICKED]


def unpacked() -> object:
    return {**HANDLERS}


# A class's own initialization hands its own method on, directly or through a private helper:
# allowed, as calling it there is.
class Registered(Rule):
    def __init__(self) -> None:
        register(self.check)


class Delegated(Rule):
    def __init__(self) -> None:
        _register_check(self)


def _register_check(rule: Rule) -> None:
    register(rule.check)


TWICE = [LATE, LATE]


class Shadowing:
    decide = decide
    register(decide)
"#;

fn project(name: &str, references: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\nrequires-python = \">=3.11\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"home\"\ntargets = [\"pkg.core.decide\", \"pkg.core.Rule\", \"pkg.core.Error\", \"Rule.check\"]\nallowed-callers = [\"pkg.core.*\"]\n{references}\n[[tool.heki.call-owners]]\nname = \"plain\"\ntargets = [\"core.decide\"]\nallowed-callers = [\"pkg.core.*\"]\n"
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/core.py", CORE)
    .write("pkg/helpers.py", HELPERS)
    .write("pkg/site.py", SITE);
    p
}

/// `(line, column, message)` of every `MUT204` report, sorted.
fn reports(p: &Project) -> Vec<(u64, u64, String)> {
    reports_with(p, &[])
}

/// [`reports`] of a check given the further arguments `args`.
fn reports_with(p: &Project, args: &[&str]) -> Vec<(u64, u64, String)> {
    let (code, stdout, stderr) = p.check(&[&["--output-format", "json"], args].concat());
    assert_eq!(code, 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<(u64, u64, String)> = diags
        .iter()
        .map(|d| {
            (
                d["line"].as_u64().unwrap(),
                d["column"].as_u64().unwrap(),
                d["message"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

#[test]
fn references_handing_a_target_on_count_as_calls_only_with_the_key() {
    let calls_only = [
        (
            28,
            12,
            "`as_key` calls `pkg.core.decide` outside the call owners of `home`",
        ),
        (
            28,
            12,
            "`as_key` calls `core.decide` outside the call owners of `plain`",
        ),
        (
            44,
            12,
            "`as_starmap` calls `pkg.core.decide` outside the call owners of `home`",
        ),
        (
            44,
            12,
            "`as_starmap` calls `core.decide` outside the call owners of `plain`",
        ),
        (
            97,
            12,
            "`called` calls `pkg.core.decide` outside the call owners of `home`",
        ),
        (
            97,
            12,
            "`called` calls `core.decide` outside the call owners of `plain`",
        ),
        // The partial object is the callable `sorted` calls: one call, no reference.
        (
            107,
            12,
            "`partial_key` calls `pkg.core.decide` outside the call owners of `home`",
        ),
        (
            107,
            12,
            "`partial_key` calls `core.decide` outside the call owners of `plain`",
        ),
    ];
    let mut without: Vec<(u64, u64, String)> = calls_only
        .iter()
        .map(|(l, c, m)| (*l, *c, (*m).to_owned()))
        .collect();
    without.sort();
    assert_eq!(reports(&project("references_ignored", "")), without);
    assert_eq!(
        reports(&project(
            "references_ignored_explicitly",
            "references = \"ignore\"\n"
        )),
        without
    );

    // Only the contract with the key counts references; `plain` still reports only calls.
    let hands = |line: u64, column: u64, func: &str, target: &str| {
        (
            line,
            column,
            format!("`{func}` hands on `{target}` outside the call owners of `home`"),
        )
    };
    let mut with = vec![
        // The names the module binds are read without a load: where they are handed on.
        hands(14, 1, "<module>", "pkg.core.decide"),
        hands(20, 40, "<module>", "pkg.core.Rule"),
        hands(32, 12, "as_value", "pkg.core.decide"),
        hands(36, 13, "in_display", "pkg.core.decide"),
        hands(40, 30, "as_partial", "pkg.core.decide"),
        hands(48, 12, "through_aliases", "pkg.core.decide"),
        hands(48, 20, "through_aliases", "pkg.core.decide"),
        hands(48, 25, "through_aliases", "pkg.core.decide"),
        hands(52, 20, "stored", "pkg.core.decide"),
        hands(53, 14, "stored", "pkg.core.decide"),
        hands(57, 11, "yielded", "pkg.core.decide"),
        hands(61, 11, "raised", "pkg.core.Error"),
        hands(65, 21, "per_item_class", "pkg.core.Rule"),
        (
            73,
            12,
            "`build` hands on `pkg.core.Rule` via `pkg.site._make` outside the call owners of `home`"
                .to_owned(),
        ),
        hands(77, 12, "method_refs", "Rule.check"),
        hands(77, 24, "method_refs", "Rule.check"),
        hands(111, 1, "<module>", "pkg.core.decide"),
        hands(117, 1, "<module>", "pkg.core.decide"),
        // One value reached twice (a display holding it twice), and a name a class body reads
        // before binding it.
        hands(140, 1, "<module>", "pkg.core.decide"),
        hands(144, 14, "<module>", "pkg.core.decide"),
    ];
    with.extend(without);
    with.sort();
    assert_eq!(
        reports(&project("references_counted", "references = \"calls\"\n")),
        with
    );

    // The bases of the class statements count too: `Rule` is a name an import binds, read
    // without a load of its own, so each is reported at its class statement.
    let subclasses = |line: u64| {
        (
            line,
            1,
            "`<module>` subclasses `pkg.core.Rule` outside the call owners of `home`".to_owned(),
        )
    };
    with.extend([subclasses(23), subclasses(126), subclasses(131)]);
    with.sort();
    assert_eq!(
        reports(&project(
            "references_counted_with_bases",
            "references = \"calls-and-bases\"\n"
        )),
        with
    );
}

const BASES: &str = r#"from dataclasses import dataclass

import pkg.core as core
from pkg.core import Rule, Strict


class Plain(Rule):
    pass


@dataclass
class Dotted(core.Rule):
    pass


class Parametrized(Rule[int]):  # type: ignore[misc]
    pass


class FromSubclass(Strict):
    pass


class WithMetaclass(metaclass=Rule):  # type: ignore[misc]
    pass


class Outer:
    class Nested(Rule):
        pass


def factory() -> type:
    class Local(Rule):
        class Inner(core.Rule):
            pass

    return Local


def _private_factory() -> None:
    class Hidden(Rule):
        pass


def calls_the_private_factory() -> None:
    _private_factory()


def tagged() -> type:
    tag = "local"

    class Tagged(Rule):
        name = tag

    return Tagged


import enum


class Color(enum.Enum):
    RED = 1
"#;

/// `(line, column, message)` of the `MUT204` reports of [`BASES`] with `subclasses` (a line of
/// the contract, or nothing).
fn base_reports(subclasses: &str) -> Vec<(u64, u64, String)> {
    let p = Project::new(&format!("references_bases_{}", subclasses.len()));
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"home\"\ntargets = [\"pkg.core.Rule\"]\nallowed-callers = [\"pkg.core.*\"]\nreferences = \"calls-and-bases\"\n{subclasses}"
        ),
    )
    .write("pkg/__init__.py", "")
    .write(
        "pkg/core.py",
        &format!("{CORE}\n\nclass Strict(Rule):\n    pass\n"),
    )
    .write("pkg/bases.py", BASES);
    reports(&p)
}

#[test]
fn a_class_base_naming_a_target_counts_with_calls_and_bases() {
    let at = |line: u64, column: u64, func: &str| {
        (
            line,
            column,
            format!("`{func}` subclasses `pkg.core.Rule` outside the call owners of `home`"),
        )
    };
    let via = |line: u64, column: u64| {
        (
            line,
            column,
            "`calls_the_private_factory` subclasses `pkg.core.Rule` via `pkg.bases._private_factory` outside the call owners of `home`"
                .to_owned(),
        )
    };
    // At the base where the statement loads it (`core.Rule`, `Rule[int]`, a base read in a class
    // body or a function), else at the class statement. A keyword is no base, a nested class is
    // reported for the function running the outer class statement, and a private helper's base
    // at its caller, as every reference. A class capturing a local of its function names its
    // base after the capture, and a base outside the project names no target.
    let named = [
        at(7, 1, "<module>"),
        at(12, 14, "<module>"),
        at(16, 20, "<module>"),
        at(29, 18, "<module>"),
        at(34, 17, "factory"),
        at(35, 21, "factory"),
        via(47, 5),
        at(53, 18, "tagged"),
    ];
    let mut covered = named.to_vec();
    // By default a class target covers its subclasses, also as a base.
    covered.push(at(20, 1, "<module>"));
    covered.sort();
    assert_eq!(base_reports(""), covered);
    let mut exact = named.to_vec();
    exact.sort();
    assert_eq!(base_reports("subclasses = false\n"), exact);
}

// The bases a class statement passes, each named in the scope the statement runs in: a
// parameter, a local, a closure cell, or a class-body name of the same name as the imported
// target is another value.
const SCOPED: &str = r#"import pkg.core as core
from pkg.core import Error, Rule


def from_parameter(Rule):
    class Derived(Rule):
        pass

    return Derived


def from_module_parameter(core):
    class Derived(core.Rule):
        pass

    return Derived


def from_local(make):
    Rule = make()

    class Derived(Rule):
        pass

    return Derived


def from_one_path(flag, other):
    if flag:
        Rule = other

    class Derived(Rule):
        pass

    return Derived


def from_enclosing_parameter(Rule):
    def inner():
        class Derived(Rule):
            pass

        return Derived

    return inner


def from_call(make):
    class Derived(make()):
        pass

    return Derived


class Holder:
    Rule = Error

    class Derived(Rule):
        pass


def through_alias():
    Base = Rule

    class Derived(Base):
        pass

    return Derived


def through_enclosing_alias():
    Base = Rule

    def inner():
        class Derived(Base):
            pass

        return Derived

    return inner


def reading_the_module_class():
    def inner():
        class Derived(Rule):
            pass

        return Derived

    return inner


class AliasHolder:
    Base = Rule

    class Derived(Base):
        pass
"#;

// The module rebinds the imported name: a class statement before the rebinding subclasses the
// import, one after it (or in a function, which runs once the module has run) the new value.
const REBOUND: &str = r#"from pkg.core import Error, Rule


class Before(Rule):
    pass


def later():
    class Derived(Rule):
        pass

    return Derived


Rule = Error


class After(Rule):
    pass
"#;

// The name holds the target on one path and another class on the other: no established base.
const FALLBACK: &str = r#"try:
    from pkg.core import Rule
except ImportError:
    Rule = object


class Fallback(Rule):
    pass
"#;

#[test]
fn a_class_base_is_named_in_the_scope_of_its_class_statement() {
    let p = Project::new("references_scoped_bases");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"home\"\ntargets = [\"pkg.core.Rule\"]\nallowed-callers = [\"pkg.core.*\"]\nreferences = \"calls-and-bases\"\n",
    )
    .write("pkg/__init__.py", "")
    .write("pkg/core.py", CORE)
    .write("pkg/scoped.py", SCOPED)
    .write("pkg/rebound.py", REBOUND)
    .write("pkg/fallback.py", FALLBACK);
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut found: Vec<(String, u64, u64, String)> = diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["column"].as_u64().unwrap(),
                d["message"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    found.sort();
    let at = |path: &str, line: u64, column: u64, func: &str| {
        (
            path.to_owned(),
            line,
            column,
            format!("`{func}` subclasses `pkg.core.Rule` outside the call owners of `home`"),
        )
    };
    let scoped = |line: u64, column: u64, func: &str| at("pkg/scoped.py", line, column, func);
    // The target through a local alias, a closure cell holding it, a module global read in a
    // nested function, and a class-body alias; the import before the module rebinds it. None of
    // the bases shadowing the import (a parameter, a module parameter, a local, a local bound on
    // one path, an enclosing function's parameter, a call's result, a class-body name) is it.
    let mut expected = vec![
        at("pkg/rebound.py", 4, 1, "<module>"),
        scoped(65, 5, "through_alias"),
        scoped(75, 23, "through_enclosing_alias.<locals>.inner"),
        scoped(85, 23, "reading_the_module_class.<locals>.inner"),
        scoped(96, 5, "<module>"),
    ];
    expected.sort();
    assert_eq!(found, expected);
}

// Scopes nested in a function read the names it binds through their cells: a local import is
// handed on wherever such a scope hands it on, as a module import is.
const NESTED: &str = r#"from pkg.core import Rule as ModuleRule
from pkg.core import register


def class_body() -> type:
    from pkg.core import Rule

    class Config:
        rule = staticmethod(Rule)

    return Config


def class_body_module_import() -> type:
    class Config:
        rule = staticmethod(ModuleRule)

    return Config


def function_body() -> object:
    from pkg.core import Rule

    return staticmethod(Rule)


def nested_def() -> object:
    from pkg.core import decide

    def inner() -> None:
        register(decide)

    return inner


def in_lambda() -> object:
    from pkg.core import decide

    return lambda: register(decide)


def in_comprehension(values: list[int]) -> list[object]:
    from pkg.core import decide

    return [register(decide) for _ in values]


def in_generator(values: list[int]) -> list[object]:
    from pkg.core import decide

    return list(register(decide) for _ in values)


def nested_class() -> type:
    from pkg.core import Rule

    class Outer:
        class Inner:
            rule = staticmethod(Rule)

    return Outer


def class_body_scopes() -> type:
    from pkg.core import decide

    class Config:
        hooks = [register(decide) for _ in range(3)]
        hook = lambda: register(decide)  # noqa: E731

    return Config


def unread() -> type:
    from pkg.core import Rule  # noqa: F401

    class Config:
        rule = staticmethod(int)

    return Config


def shadowed_in_class() -> type:
    from pkg.core import Rule

    class Config:
        Rule = int
        rule = staticmethod(Rule)

    return Config


def rebound_after() -> object:
    from pkg.core import decide

    def inner() -> None:
        register(decide)

    decide = len
    return inner


def rebound_inside() -> object:
    from pkg.core import decide

    def inner() -> None:
        nonlocal decide
        decide = len
        register(decide)

    return inner


def names_only() -> object:
    from pkg.core import Rule

    return lambda value: isinstance(value, Rule)
"#;

#[test]
fn a_scope_nested_in_a_function_hands_on_the_names_it_imports() {
    let p = Project::new("references_nested_scopes");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\nrequires-python = \">=3.11\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"home\"\ntargets = [\"pkg.core.decide\", \"pkg.core.Rule\"]\nallowed-callers = [\"pkg.core.*\"]\nreferences = \"calls\"\nattribution = \"lexical\"\n",
    )
    .write("pkg/__init__.py", "")
    .write("pkg/core.py", CORE)
    .write("pkg/nested.py", NESTED);
    let hands = |line: u64, column: u64, func: &str, target: &str| {
        (
            line,
            column,
            format!("`{func}` hands on `{target}` outside the call owners of `home`"),
        )
    };
    // A class body counts for the function running its class statement, and so do the lambdas
    // and comprehensions it holds. The name a nested scope reads is a load of its own; the
    // function's own reads of its import are the import's value, reported at the use. Silent:
    // an import no scope reads, a class-body name shadowing it, a cell the function or a nested
    // scope binds again to another value, and a use that only names it.
    let expected = [
        hands(9, 29, "class_body", "pkg.core.Rule"),
        hands(16, 29, "class_body_module_import", "pkg.core.Rule"),
        hands(24, 12, "function_body", "pkg.core.Rule"),
        hands(31, 18, "nested_def.<locals>.inner", "pkg.core.decide"),
        hands(39, 29, "in_lambda", "pkg.core.decide"),
        hands(51, 26, "in_generator", "pkg.core.decide"),
        hands(59, 33, "nested_class", "pkg.core.Rule"),
        hands(68, 27, "class_body_scopes", "pkg.core.decide"),
        hands(69, 33, "class_body_scopes", "pkg.core.decide"),
    ];
    // Before 3.12 a comprehension is a scope of its own reading the cell; from 3.12 on it runs
    // in the function (PEP 709) and reads the import's value.
    for (version, comprehension) in [("py311", (45, 22)), ("py312", (45, 13))] {
        let mut want = expected.to_vec();
        want.push(hands(
            comprehension.0,
            comprehension.1,
            "in_comprehension",
            "pkg.core.decide",
        ));
        want.sort();
        let found = reports_with(&p, &["--target-version", version]);
        assert_eq!(found, want, "{version}");
    }
}

#[test]
fn an_effect_boundary_counts_references_for_calls_only_with_the_key() {
    let boundary = |references: &str| {
        let p = Project::new(&format!("references_boundary_{}", references.len()));
        p.write(
            "pyproject.toml",
            &format!(
                "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"EFF201\"]\n\n[[tool.heki.effect-boundaries]]\nname = \"pure\"\nscopes = [\"pkg.pure\"]\nforbid = [\"calls\"]\ncall-targets = [\"pkg.core.decide\"]\n{references}"
            ),
        )
        .write("pkg/__init__.py", "")
        .write("pkg/core.py", CORE)
        .write(
            "pkg/pure_ops.py",
            "from pkg.core import decide\n\n\ndef helper() -> object:\n    return decide\n",
        )
        .write(
            "pkg/pure.py",
            "from pkg.pure_ops import helper\n\n\ndef render() -> object:\n    return helper()\n",
        );
        p.check(&["--output-format", "concise"])
    };
    let (code, stdout, stderr) = boundary("");
    assert_eq!(code, 0, "{stdout}{stderr}");
    let (code, stdout, stderr) = boundary("references = \"calls\"\n");
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stdout.contains(
            "pkg/pure.py:5:12: EFF201 [heuristic] `render` hands on `pkg.core.decide` (via `pkg.pure_ops.helper`), which effect boundary `pure` forbids"
        ),
        "{stdout}"
    );
}

#[test]
fn an_effect_boundary_counts_class_bases_with_calls_and_bases() {
    let boundary = |references: &str| {
        let p = Project::new(&format!("references_boundary_bases_{}", references.len()));
        p.write(
            "pyproject.toml",
            &format!(
                "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"EFF201\"]\n\n[[tool.heki.effect-boundaries]]\nname = \"pure\"\nscopes = [\"pkg.pure\"]\nforbid = [\"calls\"]\ncall-targets = [\"pkg.core.Rule\"]\n{references}"
            ),
        )
        .write("pkg/__init__.py", "")
        .write("pkg/core.py", CORE)
        .write(
            "pkg/pure_ops.py",
            "from pkg.core import Rule\n\n\ndef helper() -> None:\n    class Local(Rule):\n        pass\n\n\ndef shadowed(Rule: type) -> None:\n    class Local(Rule):\n        pass\n",
        )
        .write(
            "pkg/pure.py",
            "from pkg.pure_ops import helper, shadowed\n\n\ndef render() -> None:\n    helper()\n\n\ndef paint() -> None:\n    shadowed(dict)\n",
        );
        p.check(&["--output-format", "concise"])
    };
    let (code, stdout, stderr) = boundary("references = \"calls\"\n");
    assert_eq!(code, 0, "{stdout}{stderr}");
    let (code, stdout, stderr) = boundary("references = \"calls-and-bases\"\n");
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stdout.contains(
            "pkg/pure.py:5:5: EFF201 [heuristic] `render` subclasses `pkg.core.Rule` (via `pkg.pure_ops.helper`), which effect boundary `pure` forbids"
        ),
        "{stdout}"
    );
    // A parameter of the imported class's name is another class.
    assert!(!stdout.contains("`paint`"), "{stdout}");
}

#[test]
fn an_unknown_references_value_is_a_configuration_error() {
    for (section, keys) in [
        (
            "call-owners",
            "targets = [\"pkg.core.decide\"]\nallowed-callers = [\"pkg.core.*\"]",
        ),
        (
            "effect-boundaries",
            "scopes = [\"pkg.site\"]\nforbid = [\"calls\"]\ncall-targets = [\"pkg.core.decide\"]",
        ),
    ] {
        let p = Project::new(&format!("references_unknown_{}", section.len()));
        p.write(
            "pyproject.toml",
            &format!(
                "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\", \"EFF201\"]\n\n[[tool.heki.{section}]]\nname = \"c\"\n{keys}\nreferences = \"always\"\n"
            ),
        )
        .write("pkg/__init__.py", "")
        .write("pkg/core.py", CORE);
        let (code, stdout, stderr) = p.check(&[]);
        assert_eq!(code, 2, "{stdout}{stderr}");
        assert!(stderr.contains("references"), "{stderr}");
    }
}
