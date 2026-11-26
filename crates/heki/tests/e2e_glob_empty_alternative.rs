//! An empty alternative in a glob (`{,.*}`) matches as in shell brace expansion, so one pattern
//! names a function and everything nested in it (`pkg.run:outer{,.*}`) wherever a configured
//! pattern is a glob. Which keys need the nested scopes named at all depends on the key and on
//! `attribution`: the pair `outer`, `outer.*` and the one pattern give the same reports.

mod common;

use common::Project;
use serde_json::Value;

const CORE: &str = r#"def target() -> None:
    pass


def run() -> None:
    pass


def run_fast() -> None:
    pass


def runner() -> None:
    pass
"#;

const RUN: &str = r#"from .core import target


def outer(xs):
    target()

    def called_only_here() -> None:
        target()

    def _private_here() -> None:
        target()

    def escapes() -> None:
        target()

    called_only_here()
    _private_here()
    f = lambda: target()
    f()
    ys = [target() for _ in xs]
    return escapes, ys
"#;

const BAD: &str = r#"from .core import run, run_fast, runner


def bad() -> None:
    run()
    run_fast()
    runner()
"#;

fn project(name: &str, config: &str) -> Project {
    let p = Project::new(name);
    p.write("pkg/__init__.py", "");
    p.write("pkg/core.py", CORE);
    p.write("pkg/run.py", RUN);
    p.write("pkg/bad.py", BAD);
    p.write(
        "pyproject.toml",
        &format!("[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\", \"EFF201\"]\n\n{config}"),
    );
    p
}

/// `path:line` of every report, sorted.
fn reports(p: &Project) -> Vec<String> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<String> = diags
        .iter()
        .map(|d| format!("{}:{}", d["path"].as_str().unwrap(), d["line"]))
        .collect();
    out.sort();
    out
}

fn call_owners(keys: &str, attribution: &str) -> String {
    format!(
        "[[tool.heki.call-owners]]\nname = \"c\"\ntargets = [\"pkg.core.target\"]\nattribution = \"{attribution}\"\n{keys}\n"
    )
}

fn reports_of(name: &str, config: &str) -> Vec<String> {
    reports(&project(name, config))
}

#[test]
fn allowed_callers_name_nested_defs_only_where_attribution_is_lexical_or_they_escape() {
    // By the call graph a nested def that only runs inside `outer` is covered by `outer`; one
    // that is handed on is a function of its own.
    assert_eq!(
        reports_of(
            "empty_alt_graph_plain",
            &call_owners("allowed-callers = [\"pkg.run:outer\"]", "call-graph")
        ),
        ["pkg/run.py:14"]
    );
    // With lexical attribution every nested def is judged by itself.
    assert_eq!(
        reports_of(
            "empty_alt_lexical_plain",
            &call_owners("allowed-callers = [\"pkg.run:outer\"]", "lexical")
        ),
        ["pkg/run.py:11", "pkg/run.py:14", "pkg/run.py:8"]
    );
    for attribution in ["call-graph", "lexical"] {
        let one = call_owners("allowed-callers = [\"pkg.run:outer{,.*}\"]", attribution);
        let pair = call_owners(
            "allowed-callers = [\"pkg.run:outer\", \"pkg.run:outer.*\"]",
            attribution,
        );
        assert_eq!(reports_of("empty_alt_one", &one), Vec::<String>::new());
        assert_eq!(reports_of("empty_alt_pair", &pair), Vec::<String>::new());
    }
}

#[test]
fn forbidden_callers_with_an_empty_alternative_equal_the_pair() {
    for attribution in ["call-graph", "lexical"] {
        let plain = reports_of(
            "empty_alt_forbidden_plain",
            &call_owners("forbidden-callers = [\"pkg.run:outer\"]", attribution),
        );
        let one = reports_of(
            "empty_alt_forbidden_one",
            &call_owners("forbidden-callers = [\"pkg.run:outer{,.*}\"]", attribution),
        );
        let pair = reports_of(
            "empty_alt_forbidden_pair",
            &call_owners(
                "forbidden-callers = [\"pkg.run:outer\", \"pkg.run:outer.*\"]",
                attribution,
            ),
        );
        assert_eq!(one, pair, "{attribution}");
        // The nested defs are forbidden only where the pattern names them.
        assert!(one.len() > plain.len(), "{attribution}: {one:?} {plain:?}");
    }
}

#[test]
fn scopes_include_nested_functions_without_naming_them() {
    let boundary = |scopes: &str| {
        format!(
            "[[tool.heki.effect-boundaries]]\nname = \"b\"\nscopes = [{scopes}]\nforbid = [\"calls\"]\ncall-targets = [\"pkg.core.target\"]\n"
        )
    };
    let plain = reports_of("empty_alt_scopes_plain", &boundary("\"pkg.run:outer\""));
    let one = reports_of("empty_alt_scopes_one", &boundary("\"pkg.run:outer{,.*}\""));
    let pair = reports_of(
        "empty_alt_scopes_pair",
        &boundary("\"pkg.run:outer\", \"pkg.run:outer.*\""),
    );
    assert!(!plain.is_empty());
    assert_eq!(plain, one);
    assert_eq!(plain, pair);
}

#[test]
fn targets_take_an_empty_alternative() {
    let guarded = |targets: &str| {
        format!(
            "[[tool.heki.call-owners]]\nname = \"c\"\ntargets = [{targets}]\nreferences = \"calls\"\n"
        )
    };
    let named = |p: &str| reports_of("empty_alt_targets", &guarded(p));
    assert_eq!(
        named("\"pkg.core.run{,_*}\""),
        ["pkg/bad.py:5", "pkg/bad.py:6"]
    );
    assert_eq!(
        named("\"pkg.core.run\", \"pkg.core.run_*\""),
        named("\"pkg.core.run{,_*}\"")
    );
    assert_eq!(named("\"pkg.core.run\""), ["pkg/bad.py:5"]);
}

#[test]
fn module_patterns_and_paths_take_an_empty_alternative() {
    let imports = |allowed: &str| {
        let p = Project::new("empty_alt_imports");
        p.write("pkg/__init__.py", "");
        p.write("pkg/api/__init__.py", "");
        p.write("pkg/api/v1.py", "");
        p.write("pkg/app.py", "import pkg.api\nimport pkg.api.v1\n");
        p.write(
            "pyproject.toml",
            &format!(
                "[project]\nname = \"t\"\n\n[[tool.heki.import-boundaries]]\nname = \"b\"\nimporters = [\"pkg.app\"]\nallowed-imports = [{allowed}]\n"
            ),
        );
        let (_, out, err) = p.check(&["--output-format", "json"]);
        let diags: Vec<Value> =
            serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}{out}{err}"));
        diags.len()
    };
    assert_eq!(imports("\"pkg.api\""), 1);
    assert_eq!(imports("\"pkg.api{,.*}\""), 0);

    let guarded = "[[tool.heki.call-owners]]\nname = \"c\"\ntargets = [\"pkg.core.run\"]\n";
    let with_paths = |tool: &str| {
        let p = project("empty_alt_paths", "");
        p.write(
            "pyproject.toml",
            &format!("[project]\nname = \"t\"\n\n{tool}\n{guarded}"),
        );
        reports(&p)
    };
    assert_eq!(with_paths(""), ["pkg/bad.py:5"]);
    let ignored = "[tool.heki.lint]\nper-file-ignores = { \"pkg/{,x}bad.py\" = [\"MUT204\"] }\n";
    assert_eq!(with_paths(ignored), Vec::<String>::new());
    assert_eq!(
        with_paths("[tool.heki]\nexclude = [\"pkg/{,x}bad.py\"]\n"),
        Vec::<String>::new()
    );
}
