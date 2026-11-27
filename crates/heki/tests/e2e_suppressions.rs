//! Reports on suppression comments and baseline entries (`lint.report-unused-suppressions`,
//! `lint.require-suppression-codes`), and the output format `heki check` picks in GitHub Actions.

mod common;

use common::Project;
use common::lsp::{Lsp, codes, uri};
use serde_json::{Value, json};

const UNUSED: &str = "[tool.heki.lint]\nreport-unused-suppressions = true\n";
const CODES_REQUIRED: &str = "[tool.heki.lint]\nrequire-suppression-codes = true\n";

/// A blocking call in `async def` (`EFF101`, exact and on by default) under each kind of
/// suppression.
const SUPPRESSED: &str = "\
import time


async def handler():
    time.sleep(1)  # heki: ignore[EFF101]
    a = 1  # heki: ignore[EFF101, EFF105]
    b = 2  # noqa: F401, EFF101
    c = 3  # noqa
    d = 4  # heki: ignore
    e = 5  # heki: ignore[EFF101]  # type: ignore
    # heki: ignore[async-blocking-call]
    return a + b + c + d + e
";

/// `SUPPRESSED` after `--fix` with `report-unused-suppressions`: the codes that suppressed
/// nothing are gone, `EFF105` (an opt-in rule that did not run) and Ruff's codes stay.
const FIXED: &str = "\
import time


async def handler():
    time.sleep(1)  # heki: ignore[EFF101]
    a = 1  # heki: ignore[EFF105]
    b = 2  # noqa: F401
    c = 3  # noqa
    d = 4  # heki: ignore
    e = 5  # type: ignore
    return a + b + c + d + e
";

fn project(name: &str, config: &str, source: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!("[project]\nname = \"t\"\n\n{config}"),
    );
    p.write("a.py", source);
    p
}

fn json_reports(stdout: &str) -> Vec<Value> {
    serde_json::from_str(stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"))
}

/// `(line, code, target)` of each report.
fn found(reports: &[Value]) -> Vec<(u64, &str, &str)> {
    reports
        .iter()
        .map(|d| {
            (
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap(),
                d["target"].as_str().unwrap(),
            )
        })
        .collect()
}

#[test]
fn codes_that_suppressed_nothing_are_reported_only_when_asked() {
    let p = project("unused_off", "", SUPPRESSED);
    let (code, stdout, stderr) = p.heki(&["check", "--output-format", "json"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(json_reports(&stdout).is_empty(), "{stdout}");

    let p = project("unused_on", UNUSED, SUPPRESSED);
    let (code, stdout, stderr) = p.heki(&["check", "--output-format", "json"]);
    assert_eq!(code, 1, "{stderr}");
    let reports = json_reports(&stdout);
    // Not reported: a code that suppressed a report (line 5), a rule that did not run (`EFF105`),
    // another tool's code (`F401`), and the bare forms.
    assert_eq!(
        found(&reports),
        [
            (6, "unused-suppression", "EFF101"),
            (7, "unused-suppression", "EFF101"),
            (10, "unused-suppression", "EFF101"),
            (11, "unused-suppression", "async-blocking-call"),
        ],
        "{stdout}"
    );
    let first = &reports[0];
    assert_eq!(first["rule_slug"], "report-unused-suppressions");
    assert_eq!(first["severity"], "error");
    assert_eq!(first["column"], 12);
    assert_eq!(first["qualname"], "handler");
    assert_eq!(
        first["message"],
        "No `EFF101` report on this line to suppress"
    );
    assert_eq!(first["help"], "remove `EFF101` from the suppression");
    assert_eq!(reports[2]["help"], "remove the suppression");

    let (_, concise, _) = p.heki(&["check", "--output-format", "concise"]);
    assert!(
        concise
            .contains("a.py:6:12: unused-suppression No `EFF101` report on this line to suppress"),
        "{concise}"
    );
}

#[test]
fn only_codes_of_rules_that_ran_on_the_file_are_judged() {
    // `--select` leaves `EFF101` off; `per-file-ignores` turns it off in `a.py`; a heuristic
    // rule left off (`EFF117`) did not run either.
    let p = project("unused_ran", UNUSED, SUPPRESSED);
    let (code, stdout, _) = p.heki(&["check", "--select", "DATA101", "--output-format", "json"]);
    assert_eq!((code, json_reports(&stdout).len()), (0, 0), "{stdout}");
    let p = project(
        "unused_per_file",
        &format!("{UNUSED}per-file-ignores = {{ \"a.py\" = [\"EFF101\"] }}\n"),
        SUPPRESSED,
    );
    let (code, stdout, _) = p.heki(&["check", "--output-format", "json"]);
    assert_eq!((code, json_reports(&stdout).len()), (0, 0), "{stdout}");
    let p = project(
        "unused_heuristic",
        UNUSED,
        "def f(xs):\n    return [x for x in xs]  # heki: ignore[EFF117]\n",
    );
    let (code, stdout, _) = p.heki(&["check", "--output-format", "json"]);
    assert_eq!((code, json_reports(&stdout).len()), (0, 0), "{stdout}");
    let (code, stdout, _) = p.heki(&["check", "--heuristic", "--output-format", "json"]);
    assert_eq!(code, 1);
    assert_eq!(
        found(&json_reports(&stdout)),
        [(2, "unused-suppression", "EFF117")]
    );
}

#[test]
fn fix_removes_unused_codes_only_when_asked() {
    let p = project("unused_fix_off", "", SUPPRESSED);
    let (code, _, stderr) = p.heki(&["check", "--fix"]);
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(p.read("a.py"), SUPPRESSED);

    let p = project("unused_fix_on", UNUSED, SUPPRESSED);
    let (_, diff, _) = p.heki(&["check", "--diff"]);
    for line in [
        "-    # heki: ignore[async-blocking-call]\n",
        "+    b = 2  # noqa: F401\n",
    ] {
        assert!(diff.contains(line), "{diff}");
    }
    assert_eq!(p.read("a.py"), SUPPRESSED);
    let (code, _, stderr) = p.heki(&["check", "--fix"]);
    assert!(
        stderr.contains("Fixed 4 issue(s) in 1 file(s)."),
        "{stderr}"
    );
    assert_eq!(code, 0, "{stderr}");
    assert_eq!(p.read("a.py"), FIXED);
}

#[test]
fn removing_a_standalone_suppression_takes_only_its_line_whatever_the_line_ending() {
    for (name, eol) in [("lf", "\n"), ("crlf", "\r\n"), ("cr", "\r")] {
        let source = format!(
            "# heki: ignore[async-blocking-call]{eol}x = 1{eol}# heki: ignore[async-blocking-call]{eol}"
        );
        let p = project(&format!("unused_fix_eol_{name}"), UNUSED, &source);
        let (code, _, stderr) = p.heki(&["check", "--fix"]);
        assert_eq!(code, 0, "{name}: {stderr}");
        assert_eq!(p.read("a.py"), format!("x = 1{eol}"), "{name}");
    }
}

#[test]
fn several_unused_codes_of_one_comment_are_one_report() {
    let p = project(
        "unused_several",
        UNUSED,
        "async def handler():\n    return 1  # heki: ignore[EFF101, DATA101]\n",
    );
    let (code, stdout, _) = p.heki(&["check", "--output-format", "json"]);
    assert_eq!(code, 1);
    let reports = json_reports(&stdout);
    assert_eq!(
        found(&reports),
        [(2, "unused-suppression", "EFF101, DATA101")]
    );
    assert_eq!(
        reports[0]["message"],
        "No `EFF101` or `DATA101` report on this line to suppress"
    );
}

#[test]
fn a_bare_heki_ignore_is_reported_when_codes_are_required_and_still_suppresses() {
    let p = project("codes_off", "", SUPPRESSED);
    let (code, stdout, _) = p.heki(&["check", "--output-format", "json"]);
    assert_eq!((code, json_reports(&stdout).len()), (0, 0), "{stdout}");

    // Without `report-unused-suppressions`, a code that suppresses nothing (line 8) is not reported.
    let source = "import time\n\n\nasync def handler():\n    time.sleep(1)  # heki: ignore\n    x = 1  # heki: ignore\n    time.sleep(2)  # noqa\n    y = 2  # heki: ignore[EFF101]\n";
    let p = project("codes_on", CODES_REQUIRED, source);
    let (code, stdout, stderr) = p.heki(&["check", "--output-format", "json"]);
    assert_eq!(code, 1, "{stderr}");
    let reports = json_reports(&stdout);
    // The `EFF101` reports stay suppressed, and a bare `# noqa` is Ruff's as much as heki's.
    assert_eq!(
        found(&reports),
        [
            (5, "suppression-without-code", "# heki: ignore"),
            (6, "suppression-without-code", "# heki: ignore"),
        ],
        "{stdout}"
    );
    assert_eq!(reports[0]["rule_slug"], "require-suppression-codes");
    assert_eq!(
        reports[0]["message"],
        "`# heki: ignore` names no rule: it suppresses every rule on this line"
    );
    assert_eq!(
        reports[0]["help"],
        "name the rules it suppresses here: `# heki: ignore[EFF101]`"
    );
    assert_eq!(
        reports[1]["help"],
        "name the rules it is for: `# heki: ignore[CODE]`"
    );
    assert!(reports.iter().all(|d| d["fix"].is_null()), "{stdout}");
    let (_, _, stderr) = p.heki(&["check", "--fix"]);
    assert!(stderr.contains("Fixed 0 issue(s)"), "{stderr}");
    assert_eq!(p.read("a.py"), source);
}

#[test]
fn baseline_entries_that_matched_nothing_fail_the_run_when_asked() {
    let entry = |path: &str, code: &str, target: &str| {
        format!(
            "[[entries]]\npath = \"{path}\"\nqualname = \"handler\"\nrule_code = \"{code}\"\ntarget = \"{target}\"\ncount = 1\n\n"
        )
    };
    let baseline = format!(
        "version = 2\n\n{}{}{}",
        entry("a.py", "EFF101", "time.sleep"),
        entry("b.py", "EFF101", "time.sleep"),
        entry("a.py", "EFF102", "requests.Session"),
    );
    let clean = "async def handler():\n    return 1\n";
    for (name, config, unused) in [("entries_off", "", false), ("entries_on", UNUSED, true)] {
        let p = project(name, config, clean);
        p.write("b.py", clean);
        p.write("heki-baseline.toml", &baseline);
        let (code, _, stderr) = p.heki(&["check", "--baseline", "heki-baseline.toml", "a.py"]);
        // `b.py` was not checked, and `EFF102` (heuristic, left off) did not run.
        let reported = "error: baseline entry a.py::handler EFF101 `time.sleep` matched no report (`--update-baseline` removes it)";
        assert_eq!(stderr.contains(reported), unused, "{stderr}");
        assert_eq!(code, i32::from(unused), "{stderr}");
        assert!(
            !stderr.contains("b.py::handler EFF101 `time.sleep` matched"),
            "{stderr}"
        );
        assert!(
            !stderr.contains("EFF102 `requests.Session` matched"),
            "{stderr}"
        );
        assert_eq!(
            stderr.contains("a.py::handler EFF101 `time.sleep` can shrink 1 -> 0"),
            !unused,
            "{stderr}"
        );
    }
}

#[test]
fn a_baseline_entry_of_a_withheld_heuristic_report_still_matches() {
    // `OPT101` is exact, and its report here is heuristic (`x` escapes to `sink`, so the rewrite
    // is not proven): recorded with `--heuristic`, it is withheld without it.
    let escaping = "def build(items, sink):\n    x = []\n    for i in items:\n        x = x + [i]\n        sink(x)\n    return x\n";
    let p = project("entries_withheld", UNUSED, escaping);
    let (code, _, stderr) = p.heki(&[
        "check",
        "--heuristic",
        "--baseline",
        "b.toml",
        "--update-baseline",
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(p.read("b.toml").contains("rule_code = \"OPT101\""));
    let (code, _, stderr) = p.heki(&["check", "--baseline", "b.toml"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("1 heuristic report(s) are not shown"),
        "{stderr}"
    );
    assert!(!stderr.contains("matched no report"), "{stderr}");
    p.write("a.py", "def build(items, sink):\n    return list(items)\n");
    let (code, _, stderr) = p.heki(&["check", "--baseline", "b.toml"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("OPT101 `x` matched no report"), "{stderr}");
}

#[test]
fn github_actions_get_annotations_unless_a_format_is_chosen() {
    let p = project(
        "github_actions",
        "",
        "import time\n\n\nasync def handler():\n    time.sleep(1)\n",
    );
    let run = |env: &[(&str, &str)], args: &[&str]| {
        let out = common::heki_command()
            .arg("check")
            .args(args)
            .envs(env.iter().copied())
            .current_dir(&p.dir)
            .output()
            .unwrap();
        String::from_utf8(out.stdout).unwrap()
    };
    let github = run(&[("GITHUB_ACTIONS", "true")], &[]);
    assert!(github.starts_with("::error "), "{github}");
    for (env, args, start) in [
        (
            &[("GITHUB_ACTIONS", "false")][..],
            &[][..],
            "a.py:5:5: EFF101 ",
        ),
        (
            &[("GITHUB_ACTIONS", "true")],
            &["--output-format", "concise"],
            "a.py:5:5: EFF101 ",
        ),
        (
            &[("GITHUB_ACTIONS", "true"), ("HEKI_OUTPUT_FORMAT", "full")],
            &[],
            "a.py:5:5: EFF101 ",
        ),
        (&[("HEKI_OUTPUT_FORMAT", "github")], &[], "::error "),
        (
            &[("HEKI_OUTPUT_FORMAT", "github")],
            &["--output-format", "concise"],
            "a.py:5:5: EFF101 ",
        ),
    ] {
        let out = run(env, args);
        assert!(out.starts_with(start), "{env:?} {args:?}: {out}");
    }
}

#[test]
fn the_server_reports_unused_codes_and_removes_them() {
    let p = project("unused_lsp", UNUSED, SUPPRESSED);
    let a = uri(&p.dir.join("a.py"));
    let mut lsp = Lsp::start(&p.dir, &["server", "--stdio"]);
    lsp.request(
        "initialize",
        json!({"processId": null, "rootUri": uri(&p.dir), "capabilities": {}}),
    );
    lsp.notify("initialized", json!({}));
    lsp.ready();
    lsp.open(&a, SUPPRESSED);
    let diags = lsp.diagnostics(&a, |d| d.len() == 4);
    assert!(
        codes(&diags).iter().all(|c| c == "unused-suppression"),
        "{diags:#?}"
    );
    // Line 7: `    b = 2  # noqa: F401, EFF101`.
    let actions = lsp.code_actions(&a, &diags[1]["range"], &diags[1..2]);
    assert_eq!(actions.len(), 1, "{actions:#?}");
    assert_eq!(actions[0]["title"], "Remove `EFF101` from the suppression");
    assert_eq!(actions[0]["kind"], "quickfix");
    assert_eq!(actions[0]["isPreferred"], true);
    assert_eq!(
        actions[0]["edit"]["changes"][&a][0],
        json!({
            "range": {"start": {"line": 6, "character": 19}, "end": {"line": 6, "character": 31}},
            "newText": "F401"
        })
    );
    let actions = lsp.code_actions(&a, &diags[2]["range"], &diags[2..3]);
    assert_eq!(actions[0]["title"], "Remove the suppression");
    lsp.stop(true);
}

#[test]
fn baseline_entries_of_reports_on_comments_are_judged_by_their_settings() {
    let both =
        "[tool.heki.lint]\nreport-unused-suppressions = true\nrequire-suppression-codes = true\n";
    let p = project(
        "entries_of_comments",
        both,
        "def f():\n    x = 1  # heki: ignore\n    y = 2  # heki: ignore[EFF101]\n    return x + y\n",
    );
    let (code, _, stderr) = p.heki(&["check", "--baseline", "b.toml", "--update-baseline"]);
    assert_eq!(code, 0, "{stderr}");
    let recorded = p.read("b.toml");
    for code in ["unused-suppression", "suppression-without-code"] {
        assert!(
            recorded.contains(&format!("rule_code = \"{code}\"")),
            "{recorded}"
        );
    }
    let (code, _, stderr) = p.heki(&["check", "--baseline", "b.toml"]);
    assert_eq!(code, 0, "{stderr}");
    p.write("a.py", "def f():\n    return 3\n");
    let (code, _, stderr) = p.heki(&["check", "--baseline", "b.toml"]);
    assert_eq!(code, 1, "{stderr}");
    for entry in [
        "a.py::f suppression-without-code `# heki: ignore` matched no report",
        "a.py::f unused-suppression `EFF101` matched no report",
    ] {
        assert!(stderr.contains(entry), "{stderr}");
    }
}
