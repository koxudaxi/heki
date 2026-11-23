//! The code of a class body end to end through the real binary: a class attribute's value, a
//! method's decorator, and its default arguments run when the class statement runs, so the
//! contracts judge them in the code running it, `<module>` for a class at module level (a nested
//! class too) as for a class in a function. A method's own body is no class body.

mod common;

use common::Project;
use serde_json::Value;

/// `(path, line, column, rule, qualname)` of every diagnostic of `heki check`.
fn found(p: &Project) -> Vec<(String, u64, u64, String, String)> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json", "src"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<_> = diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["column"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
                d["qualname"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

fn d(
    path: &str,
    line: u64,
    column: u64,
    rule: &str,
    qualname: &str,
) -> (String, u64, u64, String, String) {
    (path.into(), line, column, rule.into(), qualname.into())
}

const FACTS: &str = r#"class FinalFacts:
    def __init__(self, modules: tuple[str, ...]) -> None:
        self.modules = modules


def seal(modules: tuple[str, ...]) -> FinalFacts:
    return FinalFacts(modules)
"#;

const BODY: &str = r#"from pkg.facts import FinalFacts


def decorate(value: object) -> object:
    return lambda function: function


class Pass:
    held = FinalFacts(())

    @decorate(FinalFacts(()))
    def seal(self, facts: FinalFacts = FinalFacts(())) -> None:
        pass


@decorate(FinalFacts(()))
def module_level(facts: FinalFacts = FinalFacts(())) -> None:
    pass
"#;

fn final_facts(name: &str, allowed: &str, body: &str) -> Project {
    let p = Project::new(name);
    p.write("src/pkg/__init__.py", "")
        .write("src/pkg/facts.py", FACTS)
        .write("src/pkg/body.py", body)
        .write(
            "heki.toml",
            &format!(
                r#"[[call-owners]]
name = "final-facts"
targets = ["pkg.facts.FinalFacts"]
allowed-callers = {allowed}
references = "reads"
attribution = "lexical"
"#
            ),
        );
    p
}

#[test]
fn class_body_calls_are_judged_in_the_module_body() {
    let p = final_facts(
        "class_bodies_module",
        r#"["pkg.facts:seal", "pkg.facts:seal.*"]"#,
        BODY,
    );
    assert_eq!(
        found(&p),
        vec![
            d("src/pkg/body.py", 9, 12, "MUT204", "<module>"),
            d("src/pkg/body.py", 11, 15, "MUT204", "<module>"),
            d("src/pkg/body.py", 12, 40, "MUT204", "<module>"),
            d("src/pkg/body.py", 16, 11, "MUT204", "<module>"),
            d("src/pkg/body.py", 17, 38, "MUT204", "<module>"),
        ]
    );
}

/// The module body allowed allows the bodies of its classes, a nested class's included; a class
/// in an allowed function is that function's code; a method's body is the method's.
#[test]
fn class_body_calls_follow_the_code_running_the_class_statement() {
    let allowed_module = final_facts(
        "class_bodies_allowed_module",
        r#"["pkg.body:<module>", "pkg.facts:seal"]"#,
        &BODY.replace(
            "    held = FinalFacts(())\n",
            "    held = FinalFacts(())\n\n    class Inner:\n        kept = FinalFacts(())\n",
        ),
    );
    assert_eq!(found(&allowed_module), vec![]);

    let in_function = final_facts(
        "class_bodies_in_function",
        r#"["pkg.body:build", "pkg.facts:seal"]"#,
        r#"from pkg.facts import FinalFacts


def build() -> object:
    class Local:
        held = FinalFacts(())

        def make(self) -> FinalFacts:
            return FinalFacts(())

    return Local


class Pass:
    def make(self) -> FinalFacts:
        return FinalFacts(())
"#,
    );
    assert_eq!(
        found(&in_function),
        vec![
            d(
                "src/pkg/body.py",
                9,
                20,
                "MUT204",
                "build.<locals>.Local.make"
            ),
            d("src/pkg/body.py", 16, 16, "MUT204", "Pass.make"),
        ]
    );
}

/// What a class body at module level reads, writes, and reaches is the module body's too.
#[test]
fn class_body_reads_writes_and_effects_are_the_module_body() {
    let p = Project::new("class_bodies_other_contracts");
    p.write("src/pkg/__init__.py", "")
        .write(
            "src/pkg/store.py",
            r#"class Store:
    limit: int = 3
    owner: str = ""


STORE = Store()


def load() -> int:
    return 1
"#,
        )
        .write(
            "src/pkg/body.py",
            r#"from pkg.store import STORE, Store, load


class Settings:
    limit = Store.limit
    STORE.owner = "settings"
    loaded = load()
"#,
        )
        .write(
            "heki.toml",
            r#"[[read-owners]]
name = "limit"
targets = ["pkg.store.Store.limit"]
allowed-readers = ["pkg.store:*"]
attribution = "lexical"

[[write-owners]]
name = "owner"
targets = ["pkg.store.Store.owner"]
allowed-writers = ["pkg.store:*"]
attribution = "lexical"

[[effect-boundaries]]
name = "no-load"
scopes = ["pkg.body:<module>"]
forbid = ["calls"]
call-targets = ["pkg.store.load"]
"#,
        );
    assert_eq!(
        found(&p),
        vec![
            d("src/pkg/body.py", 5, 13, "MUT203", "<module>"),
            d("src/pkg/body.py", 6, 5, "MUT201", "<module>"),
            d("src/pkg/body.py", 7, 14, "EFF201", "<module>"),
        ]
    );
}
