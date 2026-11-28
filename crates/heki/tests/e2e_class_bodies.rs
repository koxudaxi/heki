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
                common::tiered_code(d).to_owned(),
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

/// `(line, rule)` of each report of `rules` on `app.py` holding `source`.
fn rules_on(name: &str, rules: &str, source: &str) -> Vec<(u64, String)> {
    let p = Project::new(name);
    p.write("app.py", source);
    let (code, stdout, stderr) = p.check(&["--select", rules, "--output-format", "json", "."]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<(u64, String)> = diags
        .iter()
        .map(|d| {
            (
                d["line"].as_u64().unwrap(),
                common::tiered_code(d).to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

const WAITS: &str = r#"import time

_IS_SYNC = False


def blocking() -> None:
    time.sleep(1)


def parse(text: str) -> int:
    return int(text)


def on_some_paths(flag: bool) -> None:
    if flag:
        class Held:
            value = blocking()


def always() -> None:
    class Held:
        value = blocking()


def caught(text: str) -> None:
    try:
        class Held:
            parsed = parse(text)
            value = blocking()
    except ValueError:
        pass


def in_finally(text: str) -> None:
    try:
        parse(text)
    finally:
        class Held:
            value = blocking()


def nested(flag: bool) -> None:
    class Outer:
        if flag:
            class Inner:
                value = blocking()


def nested_always() -> None:
    class Outer:
        class Inner:
            value = blocking()


async def handle(flag: bool, text: str) -> None:
    on_some_paths(flag)
    always()
    caught(text)
    in_finally(text)
    nested(flag)
    nested_always()


async def handle_never() -> None:
    if _IS_SYNC:
        class Held:
            value = blocking()


async def handle_branch(flag: bool) -> None:
    if flag:
        class Held:
            value = blocking()
"#;

/// A function has made a call of a class body whenever it returns only when it has run the
/// `class` statement to its end whenever it returns: not behind a branch, not where the body may
/// raise into a handler that goes on, and for a nested class, the statements around it too. A
/// class statement in a `finally` body runs on the way out of the `try`, and one no path
/// reaches never runs its body: its call is not reported.
#[test]
fn a_class_body_call_runs_on_every_return_only_with_its_class_statement() {
    assert_eq!(
        rules_on("class_bodies_every_return", "EFF101", WAITS),
        vec![
            (56, "EFF101 [heuristic]".to_owned()),
            (57, "EFF101".to_owned()),
            (58, "EFF101 [heuristic]".to_owned()),
            (59, "EFF101".to_owned()),
            (60, "EFF101 [heuristic]".to_owned()),
            (61, "EFF101".to_owned()),
            // The reported call is the class body's own: it waits whenever it returns.
            (73, "EFF101".to_owned()),
        ]
    );
}

/// The same holds along an `EFF103` chain: a helper that runs the query in a class body behind
/// a branch does not send it on every iteration.
#[test]
fn a_query_in_a_class_body_behind_a_branch_is_not_sent_on_every_iteration() {
    let open = r#"import sqlite3

SQL = "SELECT * FROM t WHERE id = ?"


def fetch(db: sqlite3.Cursor, x: str) -> None:
    db.execute(SQL, (x,))


def query(db: sqlite3.Cursor, x: str, flag: bool) -> None:
    if flag:
        class Row:
            value = fetch(db, x)


def process(db: sqlite3.Cursor, items: list[str], flag: bool) -> None:
    for x in items:
        query(db, x, flag)
"#;
    assert_eq!(
        rules_on("class_bodies_query_branch", "EFF103", open),
        vec![(18, "EFF103 [heuristic]".to_owned())]
    );
    let closed = open.replace(
        "    if flag:\n        class Row:\n            value = fetch(db, x)\n",
        "    class Row:\n        value = fetch(db, x)\n",
    );
    assert_eq!(
        rules_on("class_bodies_query_always", "EFF103", &closed),
        vec![(17, "EFF103".to_owned())]
    );
}

/// At module level, a class statement behind a branch, a nested one included, still runs its
/// body's calls in the module body: only the guarantee that they run on every path depends on the
/// statement, and no chain enters a module body.
#[test]
fn a_class_body_behind_a_branch_at_module_level_is_the_module_body() {
    let p = Project::new("class_bodies_module_branch");
    p.write("src/pkg/__init__.py", "")
        .write("src/pkg/store.py", "def load() -> int:\n    return 1\n")
        .write(
            "src/pkg/body.py",
            r#"import os

from pkg.store import load

if os.environ.get("SETTINGS"):
    class Settings:
        loaded = load()

        class Inner:
            again = load()
"#,
        )
        .write(
            "heki.toml",
            r#"[[effect-boundaries]]
name = "no-load"
scopes = ["pkg.body:<module>"]
forbid = ["calls"]
call-targets = ["pkg.store.load"]
"#,
        );
    assert_eq!(
        found(&p),
        vec![d("src/pkg/body.py", 7, 18, "EFF201", "<module>")]
    );
    // One report of the target, for both calls.
    let (_, stdout, _) = p.check(&["--output-format", "json", "src"]);
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    assert_eq!(diags[0]["sites"], 2);
}
