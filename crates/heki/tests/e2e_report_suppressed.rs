//! `heki check --report-suppressed`: the reports that inline suppressions hid, in a JSON file.

mod common;

use common::Project;
use serde_json::{Value, json};

/// A blocking call in `async def` (`EFF101`, exact and on by default) under each kind of
/// suppression, and the comments that hid nothing.
const SUPPRESSED: &str = "\
import time


async def handler():
    time.sleep(1)  # heki: ignore[EFF101]
    time.sleep(2)  # noqa: EFF101
    time.sleep(3)  # noqa
    time.sleep(4)  # heki: ignore
    a = 1  # heki: ignore[EFF101]
    b = 2  # noqa: F401
";

const LOUD: &str = "\
import time


async def loud():
    time.sleep(1)
";

fn project(name: &str, source: &str) -> Project {
    let p = Project::new(name);
    p.write("a.py", source);
    p
}

/// The document `--report-suppressed out.json` wrote, and the run's exit code.
fn report(p: &Project, args: &[&str]) -> (i32, Value) {
    let mut all = vec!["check", "--report-suppressed", "out.json"];
    all.extend_from_slice(args);
    let (code, _, stderr) = p.heki(&all);
    assert!(code <= 1, "{stderr}");
    let text = p.read("out.json");
    (
        code,
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{e}: {text}")),
    )
}

/// `(file, line, kind)` of each entry: the report's line and the directive's kind.
fn listed(doc: &Value) -> Vec<(String, u64, String)> {
    doc["suppressed"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| {
            (
                e["file"].as_str().unwrap().to_owned(),
                e["line"].as_u64().unwrap(),
                e["directive"]["kind"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn a_heki_ignore_that_hid_a_report_is_listed_and_the_run_passes() {
    let p = project(
        "rs_heki_ignore",
        "import time\n\n\nasync def f():\n    time.sleep(1)  # heki: ignore[EFF101]\n",
    );
    let (code, doc) = report(&p, &[]);
    assert_eq!(code, 0);
    assert_eq!(
        doc,
        json!({
            "schema_version": 1,
            "suppressed": [{
                "code": "EFF101",
                "file": "a.py",
                "function": "f",
                "target": "time.sleep",
                "line": 5,
                "column": 5,
                "directive": {"kind": "heki-ignore", "file": "a.py", "line": 5, "column": 20},
            }],
        })
    );
}

#[test]
fn each_kind_of_directive_is_named_and_what_hid_nothing_is_not_listed() {
    let p = project("rs_kinds", SUPPRESSED);
    let (code, doc) = report(&p, &[]);
    assert_eq!(code, 0);
    // Not listed: the `heki: ignore[EFF101]` on a line without a report (line 9), and the Ruff-only
    // `noqa: F401` (line 10).
    let want = |line, kind: &str| ("a.py".to_owned(), line, kind.to_owned());
    assert_eq!(
        listed(&doc),
        [
            want(5, "heki-ignore"),
            want(6, "noqa-code"),
            want(7, "noqa-bare"),
            want(8, "heki-ignore-bare"),
        ]
    );
    // An unused directive stays out of it when `report-unused-suppressions` reports it.
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nreport-unused-suppressions = true\n",
    );
    let (code, doc) = report(&p, &[]);
    assert_eq!(code, 1);
    assert_eq!(listed(&doc).len(), 4);
}

#[test]
fn a_ruff_only_noqa_hides_nothing() {
    let p = project(
        "rs_ruff_only",
        "import time\n\n\nasync def f():\n    time.sleep(1)  # noqa: F401\n",
    );
    let (code, doc) = report(&p, &[]);
    assert_eq!(code, 1);
    assert_eq!(doc, json!({"schema_version": 1, "suppressed": []}));
}

#[test]
fn the_file_is_the_same_next_to_every_output_format_and_on_every_run() {
    let p = project("rs_formats", SUPPRESSED);
    p.write("b.py", LOUD);
    let (_, first) = report(&p, &[]);
    let bytes = p.read("out.json");
    for format in ["full", "concise", "json", "github", "sarif"] {
        let (code, _) = report(&p, &["--output-format", format]);
        assert_eq!(code, 1, "{format}");
        assert_eq!(p.read("out.json"), bytes, "{format}");
    }
    assert_eq!(first["suppressed"].as_array().unwrap().len(), 4);
}

#[test]
fn entries_are_sorted_by_file_then_function() {
    let p = project("rs_sorted", SUPPRESSED);
    let hidden = "import time\n\n\nasync def z():\n    time.sleep(1)  # noqa\n";
    p.write("0.py", hidden);
    p.write("sub/b.py", hidden);
    let (_, doc) = report(&p, &[]);
    let files: Vec<String> = listed(&doc).into_iter().map(|e| e.0).collect();
    assert_eq!(
        files,
        ["0.py", "a.py", "a.py", "a.py", "a.py", "sub/b.py"],
        "{doc}"
    );
}

#[test]
fn a_path_that_cannot_be_written_exits_with_2() {
    let p = project("rs_unwritable", SUPPRESSED);
    let (code, _, stderr) = p.heki(&["check", "--report-suppressed", "missing/dir/out.json"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("cannot write the file"), "{stderr}");
}

#[test]
fn modes_the_file_cannot_describe_are_refused() {
    let p = project("rs_conflicts", SUPPRESSED);
    for flag in [
        "--watch",
        "--update-baseline",
        "--fix",
        "--diff",
        "--generate-prompt",
    ] {
        let (code, _, stderr) = p.heki(&["check", "--report-suppressed", "out.json", flag]);
        assert_eq!(code, 2, "{flag}: {stderr}");
        assert!(stderr.contains("cannot be used with"), "{flag}: {stderr}");
    }
}

#[test]
fn touched_lists_what_the_run_shows() {
    let p = project("rs_touched", SUPPRESSED);
    p.write(
        "b.py",
        "import time\n\n\nasync def g():\n    time.sleep(1)  # noqa\n",
    );
    let (_, all) = report(&p, &[]);
    assert_eq!(listed(&all).len(), 5);
    let (_, only_b) = report(&p, &["--touched", "b.py"]);
    assert_eq!(
        listed(&only_b),
        [("b.py".to_owned(), 5, "noqa-bare".to_owned())]
    );
    // `b.py` is analyzed too, and a PATHS file selects the reports like `--touched`.
    let (_, only_a) = report(&p, &["a.py"]);
    assert_eq!(listed(&only_a).len(), 4);
}

#[test]
fn a_baseline_changes_nothing_in_it() {
    let p = project("rs_baseline", SUPPRESSED);
    p.write("b.py", LOUD);
    let (code, _, stderr) = p.heki(&["check", "--baseline", "base.toml", "--update-baseline"]);
    assert_eq!(code, 0, "{stderr}");
    let (code, with) = report(&p, &["--baseline", "base.toml"]);
    assert_eq!(code, 0);
    p.write("base.toml", "version = 3\n");
    let (code, without) = report(&p, &["--baseline", "base.toml"]);
    assert_eq!(code, 1);
    assert_eq!(with, without);
    assert_eq!(listed(&with).len(), 4);
}
