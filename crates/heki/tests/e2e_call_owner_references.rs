//! `references = "calls"` on `call-owners` and on the `calls` token of effect boundaries, end to
//! end through the real binary: a reference handing a target on to be called later (passed,
//! stored, returned, yielded, raised, held by a display) counts as a call of it, while a use that
//! only names a class (`isinstance`, `except`, a base class, `cast`, a typing subscript, an
//! annotation, a comparison) does not. Without the key only calls are reported.

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
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
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
