//! `call-owners` with `subclasses = false`, end to end through the real binary: a class target
//! names the class itself, so constructing a subclass is no call of it, while a `Class.method`
//! target still covers the method's overrides.

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
