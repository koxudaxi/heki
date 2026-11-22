//! `call-owners` and the subclasses of a target, end to end through the real binary: with
//! `subclasses = false` a class target names the class itself, so constructing a subclass is no
//! call of it, while a `Class.method` target still covers the method's overrides; and a subclass
//! is a class whose bases, named where its class statement runs, derive from the target.

mod common;

use common::Project;
use serde_json::Value;

const ERRORS: &str = r#"class ParseError(Exception):
    pass


class SchemaError(ParseError):
    pass


class Rule:
    def check(self) -> bool:
        return True


class StrictRule(Rule):
    def check(self) -> bool:
        return False
"#;

const USE: &str = r#"from pkg.errors import ParseError, Rule, SchemaError, StrictRule


def fail() -> None:
    raise ParseError("x")


def fail_schema() -> None:
    raise SchemaError("y")


def run(rule: Rule, strict: StrictRule) -> bool:
    return rule.check() and strict.check()
"#;

fn reported(subclasses: &str) -> Vec<(u64, u64)> {
    let p = Project::new(&format!("call_owner_subclasses_{}", subclasses.len()));
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"errors\"\ntargets = [\"ParseError\", \"Rule.check\"]\nallowed-callers = [\"pkg.errors.*\"]\n{subclasses}"
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/errors.py", ERRORS)
    .write("pkg/use.py", USE);
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    diags
        .iter()
        .map(|d| (d["line"].as_u64().unwrap(), d["column"].as_u64().unwrap()))
        .collect()
}

#[test]
fn a_class_target_without_subclasses_names_the_class_itself() {
    // By default constructing the subclass `SchemaError` constructs the target `ParseError`.
    assert_eq!(reported(""), vec![(5, 11), (9, 11), (13, 12), (13, 29)]);
    // With `subclasses = false` it does not; the override `StrictRule.check` is still one of
    // the target `Rule.check`.
    assert_eq!(
        reported("subclasses = false\n"),
        vec![(5, 11), (13, 12), (13, 29)]
    );
}

const SCOPED_CORE: &str = r#"class Rule:
    def check(self) -> bool:
        return True


class Other:
    def check(self) -> bool:
        return False
"#;

// The bases a class statement passes, each named in the scope the statement runs in. A
// parameter, a local, a closure cell, or a class-body name of the imported class's name is
// another value, and so is a name a `finally` body binds differently on its ways out of the
// `try`, an expression, or an attribute of an item: the class does not derive from the target.
const SCOPED_SHADOWING: &str = r#"import pkg.core as core
from pkg.core import Other, Rule


def from_parameter(Rule):
    class Derived(Rule):
        pass

    return Derived().check()


def from_module_parameter(core):
    class Derived(core.Rule):
        pass

    return Derived().check()


def from_local(make):
    Rule = make()

    class Derived(Rule):
        pass

    return Derived().check()


def from_one_path(flag, other):
    if flag:
        Rule = other

    class Derived(Rule):
        pass

    return Derived().check()


def from_enclosing_parameter(Rule):
    def inner():
        class Derived(Rule):
            pass

        return Derived().check()

    return inner


def from_a_finally_body_rebinding_it():
    Base = Rule
    try:
        Base = Other
    finally:

        class Derived(Base):
            pass

    return Derived().check()


class Holder:
    Rule = Other

    class Derived(Rule):
        def run(self):
            return self.check()


def make_base():
    return object


class Made(make_base()):
    pass


def from_an_expression():
    return Made().check()


MODULES = [core]


def from_an_item():
    class Derived(MODULES[0].Rule):
        pass

    return Derived().check()
"#;

// The bases that are the target: a module-level class statement's, a name a function reads
// from the module, a local alias of it, an attribute of an imported module, a name an import in
// the function binds, and a class-body alias or a name the class body binds only later (Python
// reads the module's then).
const SCOPED_TARGET: &str = r#"import pkg.core as core
from pkg.core import Other, Rule


class Module(Rule):
    pass


def module_level():
    return Module().check()


def reading_the_module_class():
    class Derived(Rule):
        pass

    return Derived().check()


def through_a_local_alias():
    Base = Rule

    class Derived(Base):
        pass

    return Derived().check()


def through_a_module_attribute():
    class Derived(core.Rule[int]):  # type: ignore[misc]
        pass

    return Derived().check()


def through_a_local_import():
    from pkg.core import Rule as Imported

    class Derived(Imported):
        pass

    return Derived().check()


def through_a_local_module_import():
    import pkg.core

    class Derived(pkg.core.Rule):
        pass

    return Derived().check()


def from_a_finally_body():
    try:
        pass
    finally:

        class Derived(Rule):
            pass

    return Derived().check()


class Outer:
    class Nested(Rule):
        def run(self):
            return self.check()


class AliasHolder:
    Base = Rule

    class Derived(Base):
        def run(self):
            return self.check()


class LaterHolder:
    class Derived(Rule):
        def run(self):
            return self.check()

    Rule = Other
"#;

/// A class target's subclass family and a method target's resolution along the MRO take each
/// base as the value its class statement passes, named where the statement runs.
#[test]
fn a_class_statement_names_its_bases_in_its_own_scope() {
    let p = Project::new("call_owner_subclasses_scoped_bases");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"home\"\ntargets = [\"pkg.core.Rule\", \"Rule.check\"]\nallowed-callers = [\"pkg.core.*\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write("pkg/core.py", SCOPED_CORE)
    .write("pkg/shadowing.py", SCOPED_SHADOWING)
    .write("pkg/target.py", SCOPED_TARGET);
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
    let at = |line: u64, column: u64, func: &str, does: &str| {
        (
            "pkg/target.py".to_owned(),
            line,
            column,
            format!("`{func}` {does} outside the call owners of `home`"),
        )
    };
    let mut expected = Vec::new();
    for (line, func) in [
        (10, "module_level"),
        (17, "reading_the_module_class"),
        (26, "through_a_local_alias"),
        (33, "through_a_module_attribute"),
        (42, "through_a_local_import"),
        (51, "through_a_local_module_import"),
        (62, "from_a_finally_body"),
    ] {
        expected.push(at(line, 12, func, "constructs `pkg.core.Rule`"));
        expected.push(at(line, 12, func, "calls `Rule.check`"));
    }
    for (line, func) in [
        (68, "Outer.Nested.run"),
        (76, "AliasHolder.Derived.run"),
        (82, "LaterHolder.Derived.run"),
    ] {
        expected.push(at(line, 20, func, "calls `Rule.check`"));
    }
    expected.sort();
    assert_eq!(found, expected);
}
