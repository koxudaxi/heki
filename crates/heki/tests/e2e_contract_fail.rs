//! `fail = true` on a contract table: its reports fail `heki check` whatever their tier and
//! severity, and nothing else changes (end to end, through the real binary).

mod common;

use common::Project;
use serde_json::Value;

const APP: &str = r#"class Model:
    def __init__(self) -> None:
        self.value = 0

    def touch(self) -> None:
        pass


class Store:
    def update(self, m: Model) -> None:
        m.value = 1
        m.touch()
        print(m.value)


def outside(m: Model) -> None:
    m.value = 2
    m.touch()
    print(m.value)
"#;

const TREE: &str = r#"class Node:
    def __init__(self) -> None:
        self.kids: list[int] = []


class Index:
    def refresh(self) -> None:
        pass


def careless(node: Node, kid: int) -> None:
    node.kids.append(kid)
"#;

const SHARED: &str = r#"def load() -> dict[str, int]:
    return {}


def bad() -> None:
    d = load()
    d["x"] = 1
"#;

const RENDER: &str = r#"class Model:
    def __init__(self) -> None:
        self.value = 0


def render(m: Model) -> None:
    m.value = 1
"#;

/// One contract kind with a violation: its table without the `fail` line, the files, and the
/// rule that reports.
struct Kind {
    key: &'static str,
    table: &'static str,
    files: &'static [(&'static str, &'static str)],
    rule: &'static str,
}

const KINDS: &[Kind] = &[
    Kind {
        key: "write-owners",
        table: "name = \"o\"\ntargets = [\"Model.value\"]\nallowed-writers = [\"app.Store.*\"]\n",
        files: &[("app.py", APP)],
        rule: "MUT201",
    },
    Kind {
        key: "read-owners",
        table: "name = \"o\"\ntargets = [\"Model.value\"]\nallowed-readers = [\"app.Store.*\"]\n",
        files: &[("app.py", APP)],
        rule: "MUT203",
    },
    Kind {
        key: "call-owners",
        table: "name = \"o\"\ntargets = [\"Model.touch\"]\nallowed-callers = [\"app.Store.*\"]\n",
        files: &[("app.py", APP)],
        rule: "MUT204",
    },
    Kind {
        key: "effect-boundaries",
        table: "name = \"o\"\nscopes = [\"app.render\"]\nforbid = [\"writes-targets\"]\nwrite-targets = [\"Model.value\"]\n",
        files: &[("app.py", RENDER)],
        rule: "EFF201",
    },
    Kind {
        key: "invalidation",
        table: "name = \"o\"\ntargets = [\"Node.kids\"]\ninvalidators = [\"Index.refresh\"]\n",
        files: &[("app.py", TREE)],
        rule: "MUT208",
    },
    Kind {
        key: "readonly-returns",
        table: "name = \"o\"\nproducers = [\"app.load\"]\n",
        files: &[("app.py", SHARED)],
        rule: "MUT202",
    },
    Kind {
        key: "import-boundaries",
        table: "name = \"o\"\nimporters = [\"pkg.a\"]\nforbidden-imports = [\"pkg.b\"]\n",
        files: &[
            ("pkg/__init__.py", ""),
            ("pkg/a.py", "import pkg.b\n"),
            ("pkg/b.py", ""),
        ],
        rule: "IMP201",
    },
];

/// The project of `kind` with `fail` as a line of its table (`""` for none).
fn project(name: &str, kind: &Kind, fail: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\nrequires-python = \">=3.11\"\n\n[[tool.heki.{}]]\n{}{fail}",
            kind.key, kind.table
        ),
    );
    for (path, text) in kind.files {
        p.write(path, text);
    }
    p
}

/// `heki check` as CI runs it: no `--fail-on`, no `--heuristic`.
fn check(p: &Project, args: &[&str]) -> (i32, String, String) {
    let mut all = vec!["check"];
    all.extend_from_slice(args);
    p.heki(&all)
}

fn reports(stdout: &str) -> Vec<Value> {
    serde_json::from_str(stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"))
}

#[test]
fn a_table_fails_the_run_with_fail_true_and_not_without_it() {
    for kind in KINDS {
        let key = kind.key;
        for (fail, expected) in [("", 0), ("fail = false\n", 0), ("fail = true\n", 1)] {
            let p = project("fail_kind", kind, fail);
            let (code, out, err) = check(&p, &["--output-format", "json"]);
            assert_eq!(code, expected, "{key} {fail:?}: {out}{err}");
            let found = reports(&out);
            assert!(
                found.iter().any(|d| d["rule_code"] == kind.rule),
                "{key}: {out}"
            );
        }
        // `fail = false` is today's behaviour: `fail-on` still decides.
        let p = project("fail_kind_off", kind, "fail = false\n");
        let (code, out, err) = check(&p, &["--fail-on", "warning"]);
        assert_eq!(code, 1, "{key}: {out}{err}");
    }
}

/// The project of `kind` whose `<kind>-defaults` table sets `fail = true`, and whose table
/// carries `own` (a `fail` line or none).
fn project_with_defaults(name: &str, kind: &Kind, own: &str) -> Project {
    let p = project(name, kind, own);
    let config = p.read("pyproject.toml");
    p.write(
        "pyproject.toml",
        &format!("{config}\n[tool.heki.{}-defaults]\nfail = true\n", kind.key),
    );
    p
}

#[test]
fn fail_in_a_defaults_table_applies_to_its_tables_and_their_own_fail_wins() {
    for kind in KINDS {
        let key = kind.key;
        for (own, expected) in [("", 1), ("fail = true\n", 1), ("fail = false\n", 0)] {
            let p = project_with_defaults("fail_defaults", kind, own);
            let (code, out, err) = check(&p, &["--output-format", "json"]);
            assert_eq!(code, expected, "{key} {own:?}: {out}{err}");
            assert!(
                reports(&out).iter().any(|d| d["rule_code"] == kind.rule),
                "{key}: {out}"
            );
        }
    }
}

#[test]
fn the_outputs_do_not_change_with_fail_true() {
    for kind in KINDS {
        for format in ["json", "sarif", "github", "concise"] {
            let without = project("fail_same", kind, "");
            let with = project("fail_same", kind, "fail = true\n");
            let (_, plain, _) = check(&without, &["--output-format", format]);
            let (_, failing, _) = check(&with, &["--output-format", format]);
            assert!(!plain.is_empty(), "{} {format}", kind.key);
            assert_eq!(plain, failing, "{} {format}", kind.key);
        }
        let with = project("fail_tier", kind, "fail = true\n");
        let (_, out, _) = check(&with, &["--output-format", "json"]);
        for d in reports(&out) {
            assert_eq!(d["certainty"], "heuristic", "{} {d:#}", kind.key);
        }
    }
}

const TWO_TABLES: &str = r#"[project]
name = "t"

[[tool.heki.write-owners]]
name = "gate"
targets = ["Model.value"]
allowed-writers = ["app.Store.*"]
fail = true

[[tool.heki.write-owners]]
name = "advice"
targets = ["Model.other"]
allowed-writers = ["app.Store.*"]
"#;

const TWO_TABLES_APP: &str = r#"class Model:
    def __init__(self) -> None:
        self.value = 0
        self.other = 0


def writes_advice(m: Model) -> None:
    m.other = 1
"#;

#[test]
fn only_the_tables_with_fail_true_fail_the_run() {
    let p = Project::new("fail_two_tables");
    p.write("pyproject.toml", TWO_TABLES)
        .write("app.py", TWO_TABLES_APP);
    // The other table's report is a heuristic warning, and does not fail the run.
    let (code, out, err) = check(&p, &["--output-format", "json"]);
    assert_eq!(code, 0, "{out}{err}");
    assert_eq!(reports(&out).len(), 1, "{out}");
    p.write(
        "app.py",
        &format!("{TWO_TABLES_APP}\n\ndef writes_gate(m: Model) -> None:\n    m.value = 1\n"),
    );
    let (code, out, err) = check(&p, &["--output-format", "json"]);
    assert_eq!(code, 1, "{out}{err}");
    assert_eq!(reports(&out).len(), 2, "{out}");
}

#[test]
fn a_report_the_baseline_records_does_not_fail_and_a_new_one_does() {
    let kind = &KINDS[0];
    let p = project("fail_baseline", kind, "fail = true\n");
    let (code, _, err) = check(&p, &["--baseline", "base.toml", "--update-baseline"]);
    assert_eq!(code, 0, "{err}");
    let (code, out, err) = check(&p, &["--baseline", "base.toml"]);
    assert_eq!(code, 0, "{out}{err}");
    p.write(
        "app.py",
        &format!("{APP}\n\ndef again(m: Model) -> None:\n    m.value = 3\n"),
    );
    let (code, out, err) = check(&p, &["--baseline", "base.toml"]);
    assert_eq!(code, 1, "{out}{err}");
}

#[test]
fn a_report_an_inline_suppression_hides_does_not_fail() {
    let p = Project::new("fail_suppressed");
    p.write("pyproject.toml", TWO_TABLES).write(
        "app.py",
        "class Model:\n    def __init__(self) -> None:\n        self.value = 0\n\n\ndef writes(m: Model) -> None:\n    m.value = 1  # heki: ignore[MUT201]\n",
    );
    let (code, out, err) = check(&p, &[]);
    assert_eq!(code, 0, "{out}{err}");
    p.write(
        "app.py",
        "class Model:\n    def __init__(self) -> None:\n        self.value = 0\n\n\ndef writes(m: Model) -> None:\n    m.value = 1\n",
    );
    assert_eq!(check(&p, &[]).0, 1);
}

#[test]
fn touched_fails_on_the_reports_it_shows() {
    let p = Project::new("fail_touched");
    p.write("pyproject.toml", TWO_TABLES)
        .write(
            "model.py",
            "class Model:\n    def __init__(self) -> None:\n        self.value = 0\n",
        )
        .write(
            "bad.py",
            "from model import Model\n\n\ndef writes(m: Model) -> None:\n    m.value = 1\n",
        )
        .write("clean.py", "def fine() -> int:\n    return 1\n");
    let (code, out, err) = check(&p, &["--touched", "bad.py"]);
    assert_eq!(code, 1, "{out}{err}");
    // The report is not in a touched file: the run shows nothing, and says that it hides one
    // that would fail.
    let (code, out, err) = check(&p, &["--touched", "clean.py"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(err.contains("would fail the run are not shown"), "{err}");
}

#[test]
fn the_closing_note_counts_only_the_reports_that_do_not_fail() {
    let p = Project::new("fail_note");
    p.write("pyproject.toml", TWO_TABLES).write(
        "app.py",
        &format!("{TWO_TABLES_APP}\n\ndef writes_gate(m: Model) -> None:\n    m.value = 1\n"),
    );
    let (code, _, err) = check(&p, &[]);
    assert_eq!(code, 1, "{err}");
    assert!(
        err.contains("heki check does not fail on 1 report(s) shown now"),
        "{err}"
    );
    p.write(
        "app.py",
        TWO_TABLES_APP
            .replace("m.other = 1", "m.value = 1")
            .as_str(),
    );
    let (code, _, err) = check(&p, &[]);
    assert_eq!(code, 1, "{err}");
    assert!(!err.contains("heki check does not fail on"), "{err}");
}

#[test]
fn fail_takes_a_boolean() {
    let kind = &KINDS[0];
    let p = project("fail_value", kind, "fail = \"yes\"\n");
    let (code, out, err) = check(&p, &[]);
    assert_eq!(code, 2, "{out}{err}");
    assert!(err.contains("fail"), "{err}");
}

#[test]
fn watch_exits_with_1_on_a_report_of_a_table_with_fail_true() {
    for (fail, expected) in [("", Some(0)), ("fail = true\n", Some(1))] {
        let p = project("fail_watch", &KINDS[0], fail);
        let out = common::heki_command()
            .args(["check", "--watch"])
            .current_dir(&p.dir)
            .env("HEKI_WATCH_MAX_RUNS", "0")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), expected, "{fail:?}");
    }
}

#[test]
fn a_producer_with_a_module_part_gates_its_reports() {
    for (producer, expected) in [("app:load", 1), ("other:load", 0)] {
        let p = Project::new("fail_producer_module");
        p.write(
            "pyproject.toml",
            &format!(
                "[project]\nname = \"t\"\n\n[[tool.heki.readonly-returns]]\nname = \"o\"\nproducers = [\"{producer}\"]\nfail = {}\n",
                producer == "app:load"
            ),
        )
        .write("app.py", SHARED);
        let (code, out, err) = check(&p, &[]);
        assert_eq!(code, expected, "{producer}: {out}{err}");
    }
}
