//! End-to-end `heki check` tests through the real binary.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

/// The variables that turn the colors of the text output on or off: every run starts without
/// them, and the color tests set their own.
const COLOR_VARIABLES: [&str; 4] = ["FORCE_COLOR", "CLICOLOR_FORCE", "NO_COLOR", "CLICOLOR"];

struct Project {
    dir: PathBuf,
}

impl Project {
    fn new(name: &str) -> Self {
        let dir = common::fresh_dir(&format!("cli_{name}"));
        // Marks the project root.
        std::fs::write(dir.join("pyproject.toml"), "[project]\nname = \"t\"\n").unwrap();
        Self { dir }
    }

    fn write(&self, rel: &str, text: &str) -> &Self {
        let p = self.dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
        self
    }

    /// `heki check --heuristic <args>` in the project directory.
    fn command(&self, args: &[&str]) -> Command {
        let mut cmd = common::heki_command();
        cmd.args(["check", "--heuristic"])
            .args(args)
            .current_dir(&self.dir);
        for var in COLOR_VARIABLES {
            cmd.env_remove(var);
        }
        cmd
    }

    fn heki(&self, args: &[&str]) -> (i32, String, String) {
        outcome(self.command(args))
    }

    /// `heki check <args>` in the project directory, as a user runs it: without `--heuristic`.
    fn plain(&self, args: &[&str]) -> (i32, String, String) {
        let mut cmd = common::heki_command();
        cmd.arg("check").args(args).current_dir(&self.dir);
        for var in COLOR_VARIABLES {
            cmd.env_remove(var);
        }
        outcome(cmd)
    }
}

/// The exit code, stdout, and stderr of `cmd`.
fn outcome(mut cmd: Command) -> (i32, String, String) {
    let out = cmd.output().unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8(out.stdout).unwrap(),
        String::from_utf8(out.stderr).unwrap(),
    )
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A call that always blocks the event loop: an exact `EFF101` report, an `error`.
const BLOCKING: &str = "import time\n\nasync def handler():\n    time.sleep(1)\n";
/// A blocking call one branch of a helper makes: a heuristic report of a blocking call on the
/// event loop, an `error` whose claim that the call always runs is open.
const MAYBE_BLOCKING: &str = "import time\n\n\ndef pause(seconds: float) -> None:\n    if seconds > 0:\n        time.sleep(seconds)\n\n\nasync def handler(seconds: float) -> None:\n    pause(seconds)\n";
/// A quadratic concatenation in a loop: an exact `OPT101` report, a `warning` (slower code).
const SLOW: &str = "def concat(items):\n    out = []\n    for x in items:\n        out = out + [x]\n    return out\n";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn semantic_fixtures_gate_json() {
    let p = Project::new("fixtures");
    let fixtures = std::fs::read_to_string(repo_root().join("tests/semantic/fixtures.py")).unwrap();
    p.write("tests/semantic/fixtures.py", &fixtures).write(
        "heki.toml",
        "[lint]\nrelax-async-in-tests = false\nextend-select = [\"MUT101\"]\n\n[[write-owners]]\nname = \"store\"\ntargets = [\"Model.fields\"]\nallowed-writers = [\"Store.*\"]\n",
    );
    let (code, stdout, _) = p.heki(&["--output-format", "json"]);
    assert_eq!(code, 1);
    let diags: Vec<serde_json::Value> = serde_json::from_str(&stdout).unwrap();
    let mut got: Vec<(u64, String)> = diags
        .iter()
        .map(|d| {
            (
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    got.sort();
    let expected: Vec<(u64, String)> = [
        (27, "MUT101"),
        (73, "MUT201"),
        (76, "MUT201"),
        (78, "MUT201"),
        (92, "MUT114"),
        (108, "EFF101"),
        (143, "EFF102"),
    ]
    .iter()
    .map(|(l, c)| (*l, (*c).to_owned()))
    .collect();
    assert_eq!(got, expected);
    assert_eq!(diags[0]["path"], "tests/semantic/fixtures.py");
}

#[test]
fn exit_codes_and_clean_runs() {
    let p = Project::new("exit");
    p.write("pkg/__init__.py", "")
        .write("pkg/ok.py", "def f():\n    return 1\n");
    let (code, stdout, stderr) = p.heki(&[]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stderr.contains("All checks passed"));

    // A warning is reported without failing the run.
    p.write("pkg/slow.py", SLOW);
    let (code, stdout, stderr) = p.heki(&[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.starts_with("pkg/slow.py:4:9: OPT101 (list-quadratic-concat) [warning] "),
        "{stdout}"
    );
    std::fs::remove_file(p.dir.join("pkg/slow.py")).unwrap();
    // An exact error fails it.
    p.write("pkg/bad.py", BLOCKING);
    let (code, stdout, stderr) = p.heki(&[]);
    assert_eq!(code, 1);
    assert!(
        stdout.starts_with("pkg/bad.py:4:5: EFF101 (async-blocking-call) [error] "),
        "{stdout}"
    );
    assert!(stdout.contains("4 |     time.sleep(1)"));
    assert!(stderr.contains("Found 1 diagnostic(s)"));

    assert_eq!(p.heki(&["--select", "NOPE9"]).0, 2);
    assert_eq!(p.heki(&["--target-version", "py27"]).0, 2);
    assert_eq!(p.heki(&["missing_dir"]).0, 2);
    let (code, _, stderr) = p.heki(&["--ignore", "EFF101"]);
    assert_eq!(code, 0);
    assert!(stderr.contains("All checks passed"), "{stderr}");

    p.write("pkg/broken.py", "def f(:\n");
    let (code, _, stderr) = p.heki(&[]);
    assert_eq!(code, 2);
    // Errors print the paths diagnostics print.
    assert!(stderr.contains("error: pkg/broken.py:1:7: "), "{stderr}");
}

/// Only a certain bug that can be critical stops a run by default: an exact report of an
/// `error` rule. A heuristic report counts as a warning, whatever its rule's severity.
#[test]
fn only_exact_errors_fail_a_run_by_default() {
    let p = Project::new("exact_errors_fail");
    // No configuration: a call that always blocks the event loop fails the run.
    p.write("app.py", BLOCKING);
    let (code, stdout, stderr) = p.plain(&[]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stdout.starts_with("app.py:4:5: EFF101 (async-blocking-call) [error] "),
        "{stdout}"
    );
    let (code, stdout, _) = p.plain(&["--output-format", "json"]);
    let diags: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        (code, &diags[0]["severity"], &diags[0]["certainty"]),
        (1, &"error".into(), &"exact".into()),
        "{stdout}"
    );
    std::fs::remove_file(p.dir.join("app.py")).unwrap();
    // A blocking call the event loop may run: a heuristic `EFF101` report, shown with
    // `--heuristic` as an error, which passes the run unless `fail-on = "warning"`.
    p.write("maybe.py", MAYBE_BLOCKING);
    let (code, stdout, stderr) = p.plain(&[]);
    assert_eq!((code, stdout.as_str()), (0, ""), "{stderr}");
    for selection in [&[][..], &["--select", "EFF101"]] {
        let args = [selection, &["--heuristic", "--output-format", "json"]].concat();
        let (code, stdout, stderr) = p.plain(&args);
        assert_eq!(code, 0, "{selection:?}: {stderr}");
        let diags: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        let diags = diags.as_array().unwrap();
        assert_eq!(diags.len(), 1, "{stdout}");
        assert_eq!(
            (
                &diags[0]["rule_code"],
                &diags[0]["severity"],
                &diags[0]["certainty"]
            ),
            (&"EFF101".into(), &"error".into(), &"heuristic".into()),
            "{stdout}"
        );
        let failing = [selection, &["--heuristic", "--fail-on", "warning"]].concat();
        assert_eq!(p.plain(&failing).0, 1, "{selection:?}");
    }
    p.write(
        "heki.toml",
        "[lint]\nheuristic = true\nfail-on = \"warning\"\n",
    );
    assert_eq!(p.plain(&[]).0, 1);
    assert_eq!(p.plain(&["--fail-on", "error"]).0, 0);
}

#[test]
fn inline_suppressions_ignore_hashes_in_strings() {
    let p = Project::new("noqa");
    p.write(
        "a.py",
        "import time\n\nasync def h():\n    time.sleep(len(\"# heki: ignore[EFF101]\"))\n    time.sleep(1)  # heki: ignore[EFF101]\n    time.sleep(2)  # noqa: async-blocking-call\n    time.sleep(3)  # heki: ignore[MUT201]\n",
    );
    let (code, stdout, _) = p.heki(&["--output-format", "concise"]);
    assert_eq!(code, 1);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "{stdout}");
    assert!(lines[0].starts_with("a.py:4:5: EFF101 "), "{stdout}");
    assert!(lines[1].starts_with("a.py:7:5: EFF101 "), "{stdout}");
}

#[test]
fn inline_suppressions_and_source_lines_in_files_with_cr_or_crlf_line_breaks() {
    let p = Project::new("line_breaks");
    for (file, br) in [("cr.py", "\r"), ("crlf.py", "\r\n"), ("lf.py", "\n")] {
        let src = [
            "import time  # heki: ignore[EFF101]",
            "",
            "async def h():",
            "    time.sleep(1)  # heki: ignore[EFF101]",
            "    time.sleep(2)",
            "",
        ]
        .join(br);
        p.write(file, &src);
    }
    let (code, stdout, _) = p.heki(&["--output-format", "concise"]);
    assert_eq!(code, 1);
    // Each file reports only the unsuppressed call, on its own line 5.
    let starts: Vec<&str> = stdout
        .lines()
        .map(|l| l.split(" EFF101 ").next().unwrap())
        .collect();
    assert_eq!(
        starts,
        ["cr.py:5:5:", "crlf.py:5:5:", "lf.py:5:5:"],
        "{stdout}"
    );
    let (_, full, _) = p.heki(&["cr.py"]);
    assert!(full.contains("\n5 |     time.sleep(2)\n"), "{full}");
}

#[test]
fn ratchet_baseline_update_and_compare() {
    let p = Project::new("ratchet");
    // The exit code tells a regression of these warnings apart.
    p.write("a.py", BLOCKING)
        .write("heki.toml", "[lint]\nfail-on = \"warning\"\n");
    let (code, _, stderr) = p.heki(&["--baseline", "heki-baseline.toml", "--update-baseline"]);
    assert_eq!(code, 0, "{stderr}");
    let text = std::fs::read_to_string(p.dir.join("heki-baseline.toml")).unwrap();
    assert!(
        text.contains("[[entries]]") && text.contains("rule_code = \"EFF101\""),
        "{text}"
    );
    assert!(
        text.contains("path = \"a.py\"") && text.contains("count = 1"),
        "{text}"
    );

    // Same violations: no regression.
    assert_eq!(p.heki(&["--baseline", "heki-baseline.toml"]).0, 0);
    // Moving code around does not matter (no line numbers in keys).
    p.write("a.py", &format!("# moved\n\n{BLOCKING}"));
    assert_eq!(p.heki(&["--baseline", "heki-baseline.toml"]).0, 0);

    // A new violation is a regression; only it is reported.
    p.write("b.py", BLOCKING);
    let (code, stdout, _) = p.heki(&[
        "--baseline",
        "heki-baseline.toml",
        "--output-format",
        "concise",
    ]);
    assert_eq!(code, 1);
    assert_eq!(stdout.lines().count(), 1);
    assert!(stdout.starts_with("b.py:"), "{stdout}");

    // Fixing everything leaves a shrinkable entry: exit 0 with a note.
    std::fs::remove_file(p.dir.join("b.py")).unwrap();
    p.write("a.py", "async def handler():\n    return 1\n");
    let (code, _, stderr) = p.heki(&["--baseline", "heki-baseline.toml"]);
    assert_eq!(code, 0);
    assert!(stderr.contains("can shrink 1 -> 0"), "{stderr}");

    assert_eq!(p.heki(&["--update-baseline"]).0, 2);
    p.write("bad-baseline.toml", "[[entries]]\npath = 3\n");
    assert_eq!(p.heki(&["--baseline", "bad-baseline.toml"]).0, 2);
}

#[test]
fn output_formats_and_config_selection() {
    let p = Project::new("formats");
    p.write("src/pkg/__init__.py", "")
        .write("src/pkg/api.py", BLOCKING)
        .write("src/pkg/legacy.py", BLOCKING)
        .write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nper-file-ignores = { \"src/pkg/legacy.py\" = [\"EFF\"] }\n\n[tool.heki.lint.severity]\nEFF101 = \"error\"\n",
        );
    // The message is one sentence; the help is a line of its own in `full`, a field in JSON,
    // the result's `properties.help` in SARIF, and left out of `concise` and `github`.
    let message = "`handler` runs the waiting call `time.sleep` on the event loop";
    let help = "use `await asyncio.sleep(...)`, or offload it with `asyncio.to_thread`";
    let (code, stdout, _) = p.heki(&["--output-format", "github"]);
    assert_eq!(code, 1);
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        vec![format!(
            "::error file=src/pkg/api.py,line=4,col=5,title=heki EFF101::{message}"
        )]
    );
    let (_, stdout, _) = p.heki(&["--output-format", "concise"]);
    assert_eq!(stdout, format!("src/pkg/api.py:4:5: EFF101 {message}\n"));
    let (_, stdout, _) = p.heki(&[]);
    assert!(
        stdout.ends_with(&format!(
            "  = related: src/pkg/api.py:4:5: `handler` calls `time.sleep`\n  = help: {help}\n\n"
        )),
        "{stdout}"
    );
    let (_, stdout, _) = p.heki(&["--output-format", "sarif"]);
    let sarif: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(sarif["runs"][0]["tool"]["driver"]["name"], "heki");
    assert_eq!(sarif["runs"][0]["results"][0]["level"], "error");
    assert_eq!(sarif["runs"][0]["results"][0]["message"]["text"], message);
    assert_eq!(sarif["runs"][0]["results"][0]["properties"]["help"], help);

    // `src/` layout: the module is `pkg.api`, so the qualname and summaries resolve there.
    let (_, stdout, _) = p.heki(&["--output-format", "json"]);
    let diags: Vec<serde_json::Value> = serde_json::from_str(&stdout).unwrap();
    assert_eq!(diags[0]["qualname"], "handler");
    assert_eq!(diags[0]["message"], message);
    assert_eq!(diags[0]["help"], help);

    // EFF105 is opt-in.
    p.write(
        "src/pkg/fs.py",
        "async def h(p):\n    return open(p).read()\n",
    );
    let (_, stdout, _) = p.heki(&["--output-format", "concise"]);
    assert!(!stdout.contains("EFF105"));
    let (_, stdout, _) = p.heki(&["--output-format", "concise", "--extend-select", "EFF105"]);
    assert!(stdout.contains("src/pkg/fs.py:2:12: EFF105"), "{stdout}");
}

const SLOW_AND_BLOCKING: &str = r#"import time


def concat(items):
    out = []
    for x in items:
        out = out + [x]
    return out


def join(orders, customers):
    for o in orders:
        for c in customers:
            if o.cid == c.id:
                print(o, c)


async def handler():
    time.sleep(1)
"#;

#[test]
fn slower_code_defaults_to_warning_and_a_blocked_event_loop_to_error() {
    let p = Project::new("opt_warning");
    p.write("app.py", SLOW_AND_BLOCKING);
    let severities = |args: &[&str]| -> Vec<(String, String)> {
        let (_, stdout, _) = p.heki(&[&["--output-format", "json"][..], args].concat());
        let diags: Vec<serde_json::Value> = serde_json::from_str(&stdout).unwrap();
        diags
            .iter()
            .map(|d| {
                (
                    d["rule_code"].as_str().unwrap().to_owned(),
                    d["severity"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    };
    let pair = |rule: &str, severity: &str| (rule.to_owned(), severity.to_owned());
    // The `OPT` rules report slower code, not wrong code: `warning`. A call that blocks the
    // event loop holds up every other task, as a deadlock does: an `error`, as a bug is.
    assert_eq!(
        severities(&[]),
        vec![
            pair("OPT101", "warning"),
            pair("OPT113", "warning"),
            pair("EFF101", "error"),
        ]
    );
    // Warnings alone do not fail the check; the label and the SARIF level change.
    let (code, stdout, _) = p.heki(&["--select", "OPT"]);
    assert_eq!(code, 0, "{stdout}");
    assert!(
        stdout.starts_with("app.py:7:9: OPT101 (list-quadratic-concat) [warning] "),
        "{stdout}"
    );
    let (_, stdout, _) = p.heki(&["--output-format", "sarif", "--select", "OPT113"]);
    let sarif: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(sarif["runs"][0]["results"][0]["level"], "warning");
    // `lint.severity` still overrides the default, by code or by name.
    p.write(
        "heki.toml",
        "[lint.severity]\nOPT101 = \"error\"\nnested-loop-equijoin = \"error\"\nEFF101 = \"warning\"\n",
    );
    assert_eq!(
        severities(&[]),
        vec![
            pair("OPT101", "error"),
            pair("OPT113", "error"),
            pair("EFF101", "warning"),
        ]
    );
    // An error fails it.
    assert_eq!(p.heki(&["--select", "OPT"]).0, 1);
    assert_eq!(p.heki(&["--select", "EFF101"]).0, 0);
}

/// A write of `Model.fields` outside the functions its contract ([`WRITE_OWNERS`]) allows:
/// `MUT201`, an `error` (a broken contract).
const CONTRACT_BREACH: &str = "class Model:\n    def __init__(self):\n        self.fields = []\n\n\ndef add(m: Model):\n    m.fields.append(1)\n";
const WRITE_OWNERS: &str = "[[write-owners]]\nname = \"store\"\ntargets = [\"models.Model.fields\"]\nallowed-writers = [\"store.*\"]\n";
const FORMATS: [&str; 5] = ["full", "concise", "json", "github", "sarif"];

#[test]
fn warnings_do_not_fail_the_check_unless_fail_on_says_so() {
    let p = Project::new("fail_on");
    p.write("app.py", SLOW);
    // Only warnings: they are printed, and the run passes.
    let (code, stdout, stderr) = p.heki(&[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.starts_with("app.py:4:9: OPT101 (list-quadratic-concat) [warning] "),
        "{stdout}"
    );
    assert!(stderr.contains("Found 1 diagnostic(s)"), "{stderr}");
    // `--fail-on warning` or `lint.fail-on = "warning"` makes them fail it.
    assert_eq!(p.heki(&["--fail-on", "warning"]).0, 1);
    p.write("heki.toml", "[lint]\nfail-on = \"warning\"\n");
    assert_eq!(p.heki(&[]).0, 1);
    // The command line overrides the configuration both ways.
    assert_eq!(p.heki(&["--fail-on", "error"]).0, 0);
    p.write("heki.toml", "[lint]\nfail-on = \"error\"\n");
    assert_eq!(p.heki(&[]).0, 0);
    assert_eq!(p.heki(&["--fail-on", "warning"]).0, 1);
    // Every output format exits alike.
    for format in FORMATS {
        let (code, stdout, _) = p.heki(&["--output-format", format]);
        assert_eq!(code, 0, "{format}: {stdout}");
        assert!(stdout.contains("OPT101"), "{format}: {stdout}");
        let (code, _, _) = p.heki(&["--output-format", format, "--fail-on", "warning"]);
        assert_eq!(code, 1, "{format}");
    }
    // A heuristic report counts as a warning, whatever its severity: a broken contract (an
    // `error` the registry keeps heuristic) fails the run only at `warning`.
    p.write("models.py", CONTRACT_BREACH)
        .write("heki.toml", WRITE_OWNERS);
    for format in FORMATS {
        for (fail_on, expected) in [("error", 0), ("warning", 1)] {
            let (code, stdout, _) = p.heki(&["--output-format", format, "--fail-on", fail_on]);
            assert_eq!(code, expected, "{format} {fail_on}: {stdout}");
            assert!(stdout.contains("MUT201"), "{format}: {stdout}");
        }
    }
    let (code, stdout, _) = p.heki(&["--output-format", "full"]);
    assert_eq!(code, 0, "{stdout}");
    assert!(
        stdout.contains(": MUT201 (write-owner-violation) [error, heuristic] "),
        "{stdout}"
    );
    // An exact error fails the run at either setting, in every format.
    p.write("blocking.py", BLOCKING);
    for format in FORMATS {
        for fail_on in ["error", "warning"] {
            let (code, stdout, _) = p.heki(&["--output-format", format, "--fail-on", fail_on]);
            assert_eq!(code, 1, "{format} {fail_on}: {stdout}");
            assert!(stdout.contains("EFF101"), "{format}: {stdout}");
        }
    }
    let (code, stdout, _) = p.heki(&["--output-format", "concise"]);
    assert_eq!(code, 1, "{stdout}");
    // Other values are errors, in the configuration and on the command line.
    p.write("heki.toml", "[lint]\nfail-on = \"info\"\n");
    let (code, _, stderr) = p.heki(&[]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("fail-on"), "{stderr}");
    assert_eq!(p.heki(&["--fail-on", "info"]).0, 2);
}

#[test]
fn a_text_run_says_which_reports_it_shows_do_not_fail_it() {
    let note = "heki check does not fail on 1 report(s) shown now: warnings, and heuristic reports";
    let p = Project::new("not_failing_note");
    p.write("models.py", CONTRACT_BREACH)
        .write("heki.toml", WRITE_OWNERS);
    // A broken contract is shown as an error, and passes the run.
    for format in ["full", "concise"] {
        let (code, _, stderr) = p.heki(&["--output-format", format]);
        assert_eq!(code, 0, "{format}: {stderr}");
        assert!(stderr.contains(note), "{format}: {stderr}");
    }
    // Other formats keep their output.
    let (_, stdout, stderr) = p.heki(&["--output-format", "json"]);
    assert!(
        !stdout.contains(note) && !stderr.contains(note),
        "{stdout}{stderr}"
    );
    // At `fail-on = "warning"` the report fails the run, and nothing is left to say.
    let (code, _, stderr) = p.heki(&["--fail-on", "warning"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(!stderr.contains("does not fail on"), "{stderr}");
    // A clean run has no note.
    let clean = Project::new("not_failing_note_clean");
    clean.write("app.py", "x = 1\n");
    let (code, _, stderr) = clean.heki(&[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(!stderr.contains("does not fail on"), "{stderr}");
}

#[test]
fn ratchet_regressions_fail_at_the_fail_on_severity() {
    let p = Project::new("ratchet_fail_on");
    p.write("a.py", BLOCKING)
        .write("models.py", CONTRACT_BREACH)
        .write(
            "heki.toml",
            &format!("baseline = \"heki-baseline.toml\"\n{WRITE_OWNERS}"),
        );
    let (code, _, stderr) = p.heki(&["--update-baseline"]);
    assert_eq!(code, 0, "{stderr}");
    // The recorded exact error keeps passing, at either setting.
    assert_eq!(p.heki(&[]).0, 0);
    assert_eq!(p.heki(&["--fail-on", "warning"]).0, 0);
    // A new exact error is a regression that fails the run.
    p.write("b.py", BLOCKING);
    let (code, stdout, _) = p.heki(&["--output-format", "concise"]);
    assert_eq!(code, 1, "{stdout}");
    assert!(stdout.starts_with("b.py:4:5: EFF101 "), "{stdout}");
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    std::fs::remove_file(p.dir.join("b.py")).unwrap();
    // A new warning, and a new heuristic report of an error rule, are regressions reported
    // without failing the run, unless `fail-on` says so.
    for (file, text, report) in [
        ("slow.py", SLOW, "slow.py:4:9: OPT101 "),
        (
            "more.py",
            "from models import Model\n\n\ndef add_more(m: Model):\n    m.fields.append(2)\n",
            "more.py:5:5: MUT201 ",
        ),
    ] {
        p.write(file, text);
        let (code, stdout, stderr) = p.heki(&["--output-format", "concise"]);
        assert_eq!(code, 0, "{stderr}");
        assert!(stdout.starts_with(report), "{stdout}");
        assert!(stderr.contains("Found 1 Ratchet regression(s)"), "{stderr}");
        assert_eq!(p.heki(&["--fail-on", "warning"]).0, 1);
        std::fs::remove_file(p.dir.join(file)).unwrap();
    }
    p.write("b.py", BLOCKING);
    // A baseline that is not updated because an entry grew fails the run, whatever the
    // regressions' severity: the update did not happen.
    let (code, _, stderr) = p.heki(&["--update-baseline"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("not updated"), "{stderr}");
}

#[test]
fn sarif_columns_and_carets_after_non_ascii_text() {
    let p = Project::new("non_ascii_columns");
    p.write(
        "a.py",
        "import time\n\nasync def h():\n    label = \"\u{e9}\"; time.sleep(1)\n",
    );
    let (_, full, _) = p.heki(&[]);
    // Byte column 19 in text output; the caret sits under `time` (the accented letter is one column).
    assert!(full.starts_with("a.py:4:19: EFF101"), "{full}");
    assert!(
        full.contains("4 |     label = \"\u{e9}\"; time.sleep(1)\n  |                  ^\n"),
        "{full}"
    );
    let (_, sarif, _) = p.heki(&["--output-format", "sarif"]);
    let v: serde_json::Value = serde_json::from_str(&sarif).unwrap();
    let result = &v["runs"][0]["results"][0];
    for loc in [&result["locations"][0], &result["relatedLocations"][0]] {
        assert_eq!(
            loc["physicalLocation"]["region"]["startColumn"], 18,
            "{loc}"
        );
    }
}

// Unix only: on Windows `\` separates the components of a path.
#[cfg(unix)]
#[test]
fn file_names_with_backslashes_and_uri_delimiters_are_reported_as_they_are() {
    let p = Project::new("odd_names");
    let quadratic = "import time\n\n\nasync def h():\n    time.sleep(1)\n\n\ndef f(items):\n    x = []\n    for i in items:\n        x = x + [i]\n    return x\n";
    p.write("a\\b.py", quadratic).write("c#d.py", quadratic);
    let (_, concise, _) = p.heki(&["--output-format", "concise"]);
    let paths: Vec<&str> = concise
        .lines()
        .map(|l| l.split(':').next().unwrap())
        .collect();
    // On POSIX `\` is part of the name, not a separator.
    assert_eq!(
        paths,
        ["a\\b.py", "a\\b.py", "c#d.py", "c#d.py"],
        "{concise}"
    );
    let (_, sarif, _) = p.heki(&["--output-format", "sarif"]);
    let v: serde_json::Value = serde_json::from_str(&sarif).unwrap();
    let uris: Vec<&str> = v["runs"][0]["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            r["locations"][0]["physicalLocation"]["artifactLocation"]["uri"]
                .as_str()
                .unwrap()
        })
        .collect();
    assert_eq!(uris, ["a%5Cb.py", "a%5Cb.py", "c%23d.py", "c%23d.py"]);
    let (_, diff, _) = p.heki(&["--diff", "a\\b.py"]);
    assert!(
        diff.starts_with("--- \"a/a\\\\b.py\"\n+++ \"b/a\\\\b.py\"\n"),
        "{diff}"
    );
}

// Unix only: the file name holds a line break and `:`, which Windows does not allow.
#[cfg(unix)]
#[test]
fn github_output_never_lets_project_text_start_a_log_line() {
    let p = Project::new("github_logs");
    p.write("a.py", "x = 1\n").write(
        "b.toml",
        "version = 2\n[[entries]]\npath = \"a.py\"\nqualname = \"f\"\nrule_code = \"EFF101\"\ntarget = \"x\\n::error file=a.py,line=1::forged\"\ncount = 1\n",
    );
    p.write("bad\n::error file=a.py::forged.py", "def f(:\n");
    let (code, stdout, stderr) = p.heki(&["--baseline", "b.toml", "--output-format", "github"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    // The stale entry's note and the syntax error's header both stay on one line each.
    assert!(stderr.lines().all(|l| !l.starts_with("::")), "{stderr}");
    assert!(
        stderr.contains("\nerror: bad%0A::error file=a.py::forged.py:1:7: syntax error"),
        "{stderr}"
    );
    assert!(
        stderr.contains("`x%0A::error file=a.py,line=1::forged` can shrink"),
        "{stderr}"
    );
    // Other formats print the text as it is.
    let (_, _, text) = p.heki(&["--baseline", "b.toml"]);
    assert!(text.contains("`x\n::error"), "{text}");
}

#[test]
fn ratchet_baselines_listing_one_key_twice_are_rejected() {
    let p = Project::new("baseline_twice");
    p.write("a.py", BLOCKING);
    let entry = |count: &str| {
        format!(
            "[[entries]]\npath = \"a.py\"\nqualname = \"handler\"\nrule_code = \"EFF101\"\ntarget = \"time.sleep\"\ncount = {count}\n"
        )
    };
    // Their sum would not fit the count (and would wrap to an allowance of 0).
    p.write(
        "b.toml",
        &format!("version = 2\n{}{}", entry("4294967295"), entry("1")),
    );
    let (code, stdout, stderr) = p.heki(&["--baseline", "b.toml"]);
    assert_eq!(code, 2, "{stdout}{stderr}");
    assert!(
        stderr.contains(
            "error: b.toml: the baseline lists a.py::handler EFF101 `time.sleep` more than once\n  = help: keep one of the entries\n"
        ),
        "{stderr}"
    );
}

#[test]
fn update_baseline_only_shrinks_an_existing_baseline() {
    let p = Project::new("baseline_shrinks");
    p.write("a.py", BLOCKING);
    let update = ["--baseline", "b.toml", "--update-baseline"];
    assert_eq!(p.heki(&update).0, 0);
    let recorded = std::fs::read_to_string(p.dir.join("b.toml")).unwrap();
    // A second call in the recorded function grows its entry: reported, not taken in.
    p.write("a.py", &format!("{BLOCKING}    time.sleep(2)\n"));
    let (code, stdout, stderr) = p.heki(&[&update[..], &["--output-format", "concise"]].concat());
    assert_eq!(code, 1, "{stderr}");
    assert_eq!(stdout.lines().count(), 2, "{stdout}");
    assert!(
        stderr.contains("allows 1 site(s), found 2")
            && stderr.contains("error: baseline `b.toml` not updated: 1 entry(ies) grew"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(p.dir.join("b.toml")).unwrap(),
        recorded
    );
    // A version this heki does not know is not overwritten either.
    p.write("b.toml", &recorded.replace("version = 3", "version = 9"));
    let (code, _, stderr) = p.heki(&update);
    assert_eq!(code, 2);
    assert!(stderr.contains("has version 9"), "{stderr}");
    assert!(
        std::fs::read_to_string(p.dir.join("b.toml"))
            .unwrap()
            .contains("version = 9")
    );
}

/// A blocking call a helper makes when it is asked to: a heuristic `EFF101` report at the call
/// in `handler`, exact once the `if` goes.
const GUARDED_TOOL: &str = "import subprocess\n\n\ndef run_tool(flag: bool) -> None:\n    if flag:\n        subprocess.run([\"true\"])\n\n\nasync def handler() -> None:\n    run_tool(True)\n";

/// [`GUARDED_TOOL`] without the `if`.
fn unguarded(src: &str) -> String {
    src.replace("    if flag:\n        subprocess.run", "    subprocess.run")
}

#[test]
fn a_baseline_entry_of_a_heuristic_report_does_not_allow_an_exact_one() {
    let p = Project::new("baseline_tiers");
    p.write("a.py", GUARDED_TOOL);
    let update = ["--baseline", "b.toml", "--update-baseline"];
    let (code, _, stderr) = p.heki(&update);
    assert_eq!(code, 0, "{stderr}");
    let recorded = std::fs::read_to_string(p.dir.join("b.toml")).unwrap();
    assert!(recorded.contains("\nversion = 3\n"), "{recorded}");
    assert!(
        recorded.contains("target = \"subprocess.run\"\ncount = 1\nheuristic = 1\n"),
        "{recorded}"
    );
    // The entry allowed the weaker claim: the exact report is a regression, with or without
    // `--heuristic`.
    p.write("a.py", &unguarded(GUARDED_TOOL));
    let note = "note: baseline entry a.py::handler EFF101 `subprocess.run` allows 1 site(s) (0 exact), found 1 (1 exact)";
    for (code, stdout, stderr) in [
        p.heki(&["--baseline", "b.toml"]),
        p.plain(&["--baseline", "b.toml"]),
    ] {
        assert_eq!(code, 1, "{stdout}{stderr}");
        assert!(stdout.contains("always runs the waiting call"), "{stdout}");
        assert!(stderr.contains(note), "{stderr}");
        assert!(stderr.contains("Found 1 Ratchet regression(s)"), "{stderr}");
    }
    // Nor does `--update-baseline` take it in.
    let (code, _, stderr) = p.heki(&update);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("not updated: 1 entry(ies) grew"),
        "{stderr}"
    );
    assert_eq!(
        std::fs::read_to_string(p.dir.join("b.toml")).unwrap(),
        recorded
    );
}

#[test]
fn an_exact_report_that_turns_heuristic_passes_and_its_entry_can_shrink() {
    // The call `handler` makes always runs `subprocess.run`, two calls down, and `tool` can run
    // `subprocess.check_call` before it: the exact report claims the first, and the heuristic
    // report, once `step()` is guarded too, names the second, which is shorter. One key holds
    // both.
    let src = "import subprocess\n\n\ndef tool(flag: bool) -> None:\n    if flag:\n        subprocess.check_call([\"true\"])\n    step()\n\n\ndef step() -> None:\n    subprocess.run([\"true\"])\n\n\nasync def handler(flag: bool) -> None:\n    tool(flag)\n";
    let p = Project::new("baseline_exact_to_heuristic");
    p.write("a.py", src);
    let (code, stdout, _) = p.heki(&["--output-format", "concise"]);
    assert_eq!(code, 1);
    assert!(
        stdout.contains("`a.tool` always runs the waiting call `subprocess.run`"),
        "{stdout}"
    );
    let update = ["--baseline", "b.toml", "--update-baseline"];
    assert_eq!(p.heki(&update).0, 0);
    let recorded = std::fs::read_to_string(p.dir.join("b.toml")).unwrap();
    assert!(
        recorded.contains("target = \"subprocess.check_call\"\ncount = 1\n")
            && !recorded.contains("heuristic ="),
        "{recorded}"
    );
    p.write(
        "a.py",
        &src.replace("    step()\n", "    if flag:\n        step()\n"),
    );
    let (code, stdout, stderr) = p.heki(&["--baseline", "b.toml"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stdout.is_empty(), "{stdout}");
    assert!(
        stderr.contains("note: baseline entry a.py::handler EFF101 `subprocess.check_call` can shrink 1 (1 exact) -> 1 (0 exact) (`--update-baseline`)"),
        "{stderr}"
    );
    // The entry shrinks to the heuristic report: the exact one is a regression again.
    assert_eq!(p.heki(&update).0, 0);
    let shrunk = std::fs::read_to_string(p.dir.join("b.toml")).unwrap();
    assert!(
        shrunk.contains("target = \"subprocess.check_call\"\ncount = 1\nheuristic = 1\n"),
        "{shrunk}"
    );
    let (code, _, stderr) = p.heki(&["--baseline", "b.toml"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(!stderr.contains("can shrink"), "{stderr}");
    p.write("a.py", src);
    assert_eq!(p.heki(&["--baseline", "b.toml"]).0, 1);
}

#[test]
fn version_2_baselines_are_compared_as_before_until_they_are_rewritten() {
    let p = Project::new("baseline_v2");
    p.write("a.py", GUARDED_TOOL);
    let check = ["--baseline", "b.toml"];
    let update = ["--baseline", "b.toml", "--update-baseline"];
    assert_eq!(p.heki(&update).0, 0);
    // As heki wrote it before entries recorded their heuristic sites.
    let v2 = std::fs::read_to_string(p.dir.join("b.toml"))
        .unwrap()
        .replace("version = 3", "version = 2")
        .replace("heuristic = 1\n", "");
    p.write("b.toml", &v2);
    p.write("a.py", &unguarded(GUARDED_TOOL));
    let note = "note: baseline b.toml does not record which sites were heuristic (written before version 3), so a recorded heuristic report that is exact now is no regression; `--update-baseline` rewrites it to record them";
    let (code, stdout, stderr) = p.heki(&check);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert!(stderr.contains(note), "{stderr}");
    // A site more is still a regression.
    p.write(
        "a.py",
        &format!("{}    run_tool(False)\n", unguarded(GUARDED_TOOL)),
    );
    let (code, _, stderr) = p.heki(&check);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("allows 1 site(s), found 2") && stderr.contains(note),
        "{stderr}"
    );
    // `--update-baseline` compares like a check, says nothing of the version, and rewrites the
    // file once nothing grew.
    let (code, _, stderr) = p.heki(&update);
    assert_eq!(code, 1, "{stderr}");
    assert!(!stderr.contains(note), "{stderr}");
    assert_eq!(std::fs::read_to_string(p.dir.join("b.toml")).unwrap(), v2);
    p.write("a.py", &unguarded(GUARDED_TOOL));
    assert_eq!(p.heki(&update).0, 0);
    let rewritten = std::fs::read_to_string(p.dir.join("b.toml")).unwrap();
    assert!(rewritten.contains("\nversion = 3\n"), "{rewritten}");
    let (code, _, stderr) = p.heki(&check);
    assert_eq!(code, 0, "{stderr}");
    assert!(!stderr.contains("note: baseline"), "{stderr}");
}

#[test]
fn baselines_and_snapshots_are_kept_when_a_file_fails_to_parse() {
    let p = Project::new("keep_on_errors");
    p.write("a.py", BLOCKING)
        .write("b.py", "def f(x):\n    x.append(1)\n");
    let update = ["--baseline", "b.toml", "--update-baseline"];
    assert_eq!(p.heki(&update).0, 0);
    let snapshot = |args: &[&str]| {
        common::heki_command()
            .arg("snapshot")
            .args(args)
            .current_dir(&p.dir)
            .output()
            .unwrap()
    };
    assert!(snapshot(&["--write"]).status.success());
    let read = |f: &str| std::fs::read_to_string(p.dir.join(f)).unwrap();
    let (baseline, snap) = (read("b.toml"), read(".heki-summary-snapshot.json"));
    // A syntax error drops both files from the run: writing the rest would forget them.
    p.write("a.py", "def f(:\n").write("b.py", "def f(:\n");
    let (code, _, stderr) = p.heki(&update);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("error: baseline `b.toml` not written: 2 file(s) could not be analyzed"),
        "{stderr}"
    );
    let out = snapshot(&["--write"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("not written: 2 file(s) could not be analyzed")
    );
    assert_eq!(
        (read("b.toml"), read(".heki-summary-snapshot.json")),
        (baseline, snap)
    );
}

/// An `async def` with `n` blocking calls on lines of their own (from line 4).
fn sleeps(n: usize) -> String {
    let mut src = String::from("import time\n\nasync def h():\n");
    for i in 0..n {
        src.push_str(&format!("    time.sleep({})\n", i + 1));
    }
    src
}

#[test]
fn folded_blocking_calls_are_suppressed_and_baselined_one_by_one() {
    let p = Project::new("fold_after_baseline");
    p.write("a.py", &sleeps(11));
    let concise = ["--output-format", "concise"];
    let (code, stdout, _) = p.heki(&concise);
    assert_eq!(code, 1);
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    // Exact reports fold by the call each of them always runs, and say it.
    assert!(
        stdout.contains("(+10 more async call sites that always run `time.sleep`)"),
        "{stdout}"
    );
    // Suppressing the first call leaves the other ten, below the folding threshold.
    p.write(
        "a.py",
        &sleeps(11).replacen("time.sleep(1)", "time.sleep(1)  # heki: ignore[EFF101]", 1),
    );
    let (code, stdout, _) = p.heki(&concise);
    assert_eq!(code, 1);
    assert_eq!(stdout.lines().count(), 10, "{stdout}");
    assert!(stdout.starts_with("a.py:5:5: EFF101 "), "{stdout}");
    // The baseline records every site: a twelfth call is a regression.
    p.write("a.py", &sleeps(11));
    let baseline = ["--baseline", "b.toml"];
    assert_eq!(
        p.heki(&[&baseline[..], &["--update-baseline"]].concat()).0,
        0
    );
    assert!(
        std::fs::read_to_string(p.dir.join("b.toml"))
            .unwrap()
            .contains("count = 11")
    );
    p.write("a.py", &sleeps(12));
    let (code, stdout, stderr) = p.heki(&[&baseline[..], &concise[..]].concat());
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("allows 11 site(s), found 12"), "{stderr}");
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert!(stdout.contains("(+11 more async call sites"), "{stdout}");
}

/// The hidden `--walk-steps` option writes, whatever the exit code, the steps each owner's walks
/// took per function, for `scripts/check_walk_shapes.py`; a path it cannot write is an error.
#[test]
fn walk_steps_are_written_as_json_and_hidden_from_help() {
    let p = Project::new("walk_steps");
    p.write("m.py", BLOCKING);
    // A run that fails (its warning, with `--fail-on warning`) writes them too.
    let (code, _, stderr) = p.heki(&["--fail-on", "warning", "--walk-steps", "steps.json", "."]);
    assert_eq!(code, 1, "{stderr}");
    let steps = std::fs::read_to_string(p.dir.join("steps.json")).unwrap();
    let steps: serde_json::Value = serde_json::from_str(&steps).unwrap();
    assert!(steps.is_array(), "{steps}");

    let (code, _, stderr) = p.heki(&["--walk-steps", "missing/steps.json", "."]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("error: missing/steps.json: cannot write the file: "),
        "{stderr}"
    );

    let (code, help, _) = p.heki(&["--help"]);
    assert_eq!(code, 0);
    assert!(!help.contains("walk-steps"), "{help}");
}

/// The full text output ends with where each reported rule is explained; the other formats
/// are for programs, and SARIF holds each rule's page in its `helpUri`.
#[test]
fn full_output_ends_with_the_pages_of_the_reported_rules() {
    let p = Project::new("rule_pages");
    p.write("a.py", BLOCKING);
    let (_, _, full) = p.heki(&[]);
    assert!(
        full.ends_with(
            "\nEFF101 (async-blocking-call): `heki rule EFF101`, https://heki.koxudaxi.dev/rules/async-blocking-call/\n"
        ),
        "{full}"
    );
    let (_, _, concise) = p.heki(&["--output-format", "concise"]);
    assert!(!concise.contains("heki rule"), "{concise}");
    let (_, sarif, _) = p.heki(&["--output-format", "sarif"]);
    let sarif: serde_json::Value = serde_json::from_str(&sarif).unwrap();
    assert_eq!(
        sarif["runs"][0]["tool"]["driver"]["rules"][0]["helpUri"],
        "https://heki.koxudaxi.dev/rules/async-blocking-call/"
    );
}

/// The escape codes of the colored text output, as Ruff writes them.
mod codes {
    pub const RESET: &str = "\x1b[0m";
    pub const PATH: &str = "\x1b[1m";
    /// The `:` of a location, with its reset.
    pub const SEP: &str = "\x1b[36m:\x1b[0m";
    pub const RULE_CODE: &str = "\x1b[1m\x1b[31m";
    pub const ERROR: &str = "\x1b[1m\x1b[91m";
    pub const WARNING: &str = "\x1b[1m\x1b[33m";
    pub const GUTTER: &str = "\x1b[1m\x1b[94m";
    pub const HELP: &str = "\x1b[1m\x1b[96m";
    pub const DELETION: &str = "\x1b[31m";
    pub const INSERTION: &str = "\x1b[32m";
}

/// `text` without the escape codes that color it (`ESC [ ... m`).
fn uncolored(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('\x1b') {
        out.push_str(&rest[..start]);
        let end = rest[start..]
            .find('m')
            .expect("an escape code ends with `m`");
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out
}

/// Color variables set for one run.
type Vars<'a> = &'a [(&'a str, &'a str)];

fn stdout_of(mut cmd: Command, vars: Vars) -> String {
    let out = cmd.envs(vars.iter().copied()).output().unwrap();
    String::from_utf8(out.stdout).unwrap()
}

const QUADRATIC: &str =
    "def f(items):\n    x = []\n    for i in items:\n        x = x + [i]\n    return x\n";

/// A project with an error (`EFF101`, raised by `lint.severity`), a warning with a fix
/// (`OPT101`), and the plain `full` output of both.
fn colored_project(name: &str) -> (Project, String) {
    let p = Project::new(name);
    p.write("app.py", BLOCKING)
        .write("opt.py", QUADRATIC)
        .write("heki.toml", "[lint.severity]\nEFF101 = \"error\"\n");
    let plain = stdout_of(p.command(&[]), &[]);
    assert!(plain.contains("(async-blocking-call) [error"), "{plain}");
    assert!(
        plain.contains("(list-quadratic-concat) [warning"),
        "{plain}"
    );
    (p, plain)
}

/// The text output takes Ruff's colors, and only escape codes are added: without them it is the
/// plain output. The machine formats never change.
#[test]
fn colored_text_output_has_ruffs_colors_and_the_plain_text() {
    use codes::*;
    let (p, plain) = colored_project("colors");
    let forced = |args: &[&str]| stdout_of(p.command(args), &[("FORCE_COLOR", "1")]);
    let full = forced(&[]);
    assert_eq!(uncolored(&full), plain);
    for styled in [
        format!(
            "{PATH}app.py{RESET}{SEP}4{SEP}5{SEP} {RULE_CODE}EFF101{RESET} (async-blocking-call) [{ERROR}error{RESET}"
        ),
        format!(
            "\n  {GUTTER}|{RESET}\n{GUTTER}4 |{RESET}     time.sleep(1)\n  {GUTTER}|{RESET}     {ERROR}^{RESET}\n"
        ),
        format!(
            "\n  {GUTTER}={RESET} {GUTTER}related{RESET}: {PATH}app.py{RESET}{SEP}4{SEP}5{SEP} `handler` calls"
        ),
        format!("\n  {GUTTER}={RESET} {HELP}help{RESET}: "),
        format!("(list-quadratic-concat) [{WARNING}warning{RESET}"),
        format!("\n  {GUTTER}|{RESET}         {WARNING}^{RESET}\n"),
    ] {
        assert!(full.contains(&styled), "{styled:?} not in {full:?}");
    }

    let concise = forced(&["--output-format", "concise"]);
    assert_eq!(
        uncolored(&concise),
        stdout_of(p.command(&["--output-format", "concise"]), &[])
    );
    assert!(
        concise.starts_with(&format!(
            "{PATH}app.py{RESET}{SEP}4{SEP}5{SEP} {RULE_CODE}EFF101{RESET} "
        )),
        "{concise:?}"
    );

    let diff = forced(&["--diff"]);
    assert_eq!(uncolored(&diff), stdout_of(p.command(&["--diff"]), &[]));
    assert_eq!(
        diff,
        format!(
            "--- {DELETION}a/opt.py{RESET}\n+++ {INSERTION}b/opt.py{RESET}\n@@ -1,5 +1,5 @@\n def f(items):\n     x = []\n     for i in items:\n{DELETION}-        x = x + [i]{RESET}\n{INSERTION}+        x.append(i){RESET}\n     return x\n"
        )
    );

    for format in ["json", "github", "sarif"] {
        let args = ["--output-format", format];
        let machine = stdout_of(p.command(&args), &[]);
        assert_eq!(forced(&args), machine, "{format}");
        let always = stdout_of(
            p.command(&[&args[..], &["--color", "always"]].concat()),
            &[],
        );
        assert_eq!(always, machine, "{format}");
    }
}

/// Ruff's decision when stdout is not a terminal: `--color always` / `never` decide alone, then
/// `FORCE_COLOR` (not empty) and `CLICOLOR_FORCE` (not `0`) turn color on, also over
/// `NO_COLOR`; otherwise the output is plain.
#[test]
fn piped_output_is_colored_only_when_color_is_forced() {
    let (p, plain) = colored_project("color_piped");
    let cases: &[(Vars, &[&str], bool)] = &[
        (&[], &[], false),
        (&[("FORCE_COLOR", "1")], &[], true),
        (&[("FORCE_COLOR", "0")], &[], true),
        (&[("FORCE_COLOR", "")], &[], false),
        (&[("CLICOLOR_FORCE", "1")], &[], true),
        (&[("CLICOLOR_FORCE", "")], &[], true),
        (&[("CLICOLOR_FORCE", "0")], &[], false),
        (&[("FORCE_COLOR", "1"), ("NO_COLOR", "1")], &[], true),
        (&[("CLICOLOR_FORCE", "1"), ("NO_COLOR", "1")], &[], true),
        (&[("CLICOLOR", "1")], &[], false),
        (&[], &["--color", "always"], true),
        (&[("NO_COLOR", "1")], &["--color", "always"], true),
        (&[("FORCE_COLOR", "1")], &["--color", "never"], false),
        (&[("CLICOLOR_FORCE", "1")], &["--color", "never"], false),
        (&[("FORCE_COLOR", "1")], &["--color", "auto"], true),
    ];
    for &(vars, args, colored) in cases {
        let out = stdout_of(p.command(args), vars);
        assert_eq!(out.contains('\x1b'), colored, "{vars:?} {args:?}:\n{out}");
        assert_eq!(uncolored(&out), plain, "{vars:?} {args:?}");
    }
}

/// Ruff's decision when stdout is a terminal: colored, unless `NO_COLOR` is set (to anything),
/// `CLICOLOR` is `0`, or `--color never` is given; `FORCE_COLOR` and `--color always` win over
/// `NO_COLOR`. The terminal is a pseudo-terminal Python opens, which Windows does not have.
#[cfg(unix)]
#[test]
fn terminal_output_is_colored_unless_no_color_is_set() {
    // `heki` with its stdout on the terminal; stderr stays a pipe.
    const ON_TERMINAL: &str = "import os, subprocess, sys
main, side = os.openpty()
child = subprocess.Popen(sys.argv[1:], stdout=side)
os.close(side)
out = b''
while True:
    try:
        chunk = os.read(main, 65536)
    except OSError:
        break
    if not chunk:
        break
    out += chunk
child.wait()
sys.stdout.buffer.write(out)
";
    let (p, plain) = colored_project("color_terminal");
    let cases: &[(Vars, &[&str], bool)] = &[
        (&[], &[], true),
        (&[], &["--color", "auto"], true),
        (&[("NO_COLOR", "1")], &[], false),
        (&[("NO_COLOR", "")], &[], false),
        (&[("CLICOLOR", "0")], &[], false),
        (&[("CLICOLOR", "1")], &[], true),
        (&[], &["--color", "never"], false),
        (&[("NO_COLOR", "1"), ("FORCE_COLOR", "1")], &[], true),
        (&[("NO_COLOR", "1")], &["--color", "always"], true),
    ];
    for &(vars, args, colored) in cases {
        let heki = p.command(args);
        let mut on_terminal = Command::new(common::python());
        on_terminal
            .args(["-c", ON_TERMINAL])
            .arg(heki.get_program())
            .args(heki.get_args())
            .current_dir(&p.dir);
        for (var, value) in heki.get_envs() {
            match value {
                Some(value) => on_terminal.env(var, value),
                None => on_terminal.env_remove(var),
            };
        }
        // The terminal ends lines with CR LF.
        let out = stdout_of(on_terminal, vars).replace("\r\n", "\n");
        assert_eq!(out.contains('\x1b'), colored, "{vars:?} {args:?}:\n{out}");
        assert_eq!(uncolored(&out), plain, "{vars:?} {args:?}");
    }
}

const USERS: &str = "import requests\n\n\ndef fetch_name(user_id):\n    return requests.get(f\"https://api.example.com/users/{user_id}\", timeout=5).text\n\n\ndef load_user(user_id):\n    return {\"id\": user_id, \"name\": fetch_name(user_id)}\n";
const API: &str = "from app.users import load_user\n\n\nasync def get_user(user_id):\n    return load_user(user_id)\n";

/// `app/api.py` blocks on `requests.get` in `app/users.py`, two helpers down; `app/jobs.py`
/// blocks on its own; `app/__init__.py` reports nothing. Each report is an exact error.
fn touched_project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("app/__init__.py", "")
        .write("app/users.py", USERS)
        .write("app/api.py", API)
        .write("app/jobs.py", BLOCKING);
    p
}

/// The first `path:line:column: CODE` of each concise line.
fn sites(stdout: &str) -> Vec<&str> {
    stdout
        .lines()
        .map(|l| l.split_once(": ").map_or(l, |(site, _)| site))
        .collect()
}

#[test]
fn touched_runs_show_and_fail_on_the_reports_that_touch_the_files() {
    let p = touched_project("touched");
    let concise = |touched: &[&str]| {
        let (code, stdout, stderr) =
            p.heki(&[&["--output-format", "concise", "--touched"][..], touched].concat());
        (code, sites(&stdout).join("\n"), stderr)
    };
    let (code, stdout, stderr) = p.heki(&["--output-format", "concise"]);
    assert_eq!(code, 1, "{stderr}");
    assert_eq!(
        sites(&stdout),
        ["app/api.py:5:12", "app/jobs.py:4:5"],
        "{stdout}"
    );

    // The report in `app/api.py` comes from `app/users.py`: its call chain touches the file.
    let (code, shown, stderr) = concise(&["app/users.py"]);
    assert_eq!((code, shown.as_str()), (1, "app/api.py:5:12"), "{stderr}");
    assert!(
        stderr.contains("Found 1 diagnostic(s) for 1 touched file(s) (4 file(s) analyzed)."),
        "{stderr}"
    );
    // A file's own reports; the paths name the file however they are spelled.
    let jobs = p.dir.join("app/jobs.py");
    for spelled in [
        "app/jobs.py",
        "./app/jobs.py",
        "app/../app/jobs.py",
        jobs.to_str().unwrap(),
    ] {
        let (code, shown, stderr) = concise(&[spelled]);
        assert_eq!(
            (code, shown.as_str()),
            (1, "app/jobs.py:4:5"),
            "{spelled}: {stderr}"
        );
    }
    let (code, shown, _) = concise(&["app/api.py", "app/jobs.py"]);
    assert_eq!(
        (code, shown.as_str()),
        (1, "app/api.py:5:12\napp/jobs.py:4:5")
    );
    // `--touched` is repeatable.
    let (code, stdout, _) = p.heki(&[
        "--output-format",
        "concise",
        "--touched",
        "app/api.py",
        "--touched",
        "app/jobs.py",
    ]);
    assert_eq!((code, sites(&stdout).len()), (1, 2), "{stdout}");

    // A file no report touches passes, and so does a run whose paths are none of the analyzed
    // files: each such path is named in one warning, and is no error.
    let (code, shown, stderr) = concise(&["app/__init__.py"]);
    assert_eq!((code, shown.as_str()), (0, ""), "{stderr}");
    assert!(
        stderr.contains("All checks passed for 1 touched file(s) (4 file(s) analyzed)."),
        "{stderr}"
    );
    assert!(!stderr.contains("warning:"), "{stderr}");
    let (code, shown, stderr) = concise(&["gone.py", "heki.toml", "app"]);
    assert_eq!((code, shown.as_str()), (0, ""), "{stderr}");
    assert_eq!(stderr.matches("warning:").count(), 1, "{stderr}");
    assert!(
        stderr.contains(
            "warning: `--touched` names 3 path(s) that are not among the analyzed files (missing, not a Python file under PATHS, or excluded): `gone.py`, `heki.toml`, `app`"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains("All checks passed for 0 touched file(s) (4 file(s) analyzed)."),
        "{stderr}"
    );
    // A touched file that does not parse is analyzed: its error is reported, and fails the run.
    p.write("app/broken.py", "def f(:\n");
    let (code, shown, stderr) = concise(&["app/broken.py"]);
    assert_eq!((code, shown.as_str()), (2, ""), "{stderr}");
    assert!(
        stderr.contains("app/broken.py:1:7: syntax error"),
        "{stderr}"
    );
    assert!(
        stderr.contains("Found 0 diagnostic(s) for 1 touched file(s) (4 file(s) analyzed); 1 file(s) could not be analyzed."),
        "{stderr}"
    );
    assert!(!stderr.contains("warning:"), "{stderr}");
    std::fs::remove_file(p.dir.join("app/broken.py")).unwrap();
    // A file the paths leave out is not analyzed, so it touches nothing.
    p.write("tools/report.py", "def report():\n    return 1\n");
    let (code, shown, stderr) = p.heki(&[
        "tools",
        "--output-format",
        "concise",
        "--touched",
        "app/users.py",
    ]);
    assert_eq!((code, sites(&shown)), (0, vec![]), "{stderr}");
    assert!(stderr.contains("`app/users.py`"), "{stderr}");
    std::fs::remove_dir_all(p.dir.join("tools")).unwrap();
    // A file among the paths is checked in its project: it joins the touched files.
    let (code, shown, stderr) = p.heki(&[
        "app/jobs.py",
        "--output-format",
        "concise",
        "--touched",
        "app/users.py",
    ]);
    assert_eq!(
        (code, sites(&shown)),
        (1, vec!["app/api.py:5:12", "app/jobs.py:4:5"]),
        "{stderr}"
    );
    assert!(
        stderr.contains("Found 2 diagnostic(s) for 2 touched file(s) (4 file(s) analyzed)."),
        "{stderr}"
    );

    // Every format shows the same reports.
    let (code, stdout, stderr) = p.heki(&["--output-format", "json", "--touched", "app/users.py"]);
    assert_eq!(code, 1, "{stderr}");
    let reports: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let reports = reports.as_array().unwrap();
    assert_eq!(reports.len(), 1, "{stdout}");
    assert_eq!(reports[0]["path"], "app/api.py");
    assert!(
        reports[0]["related"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["path"] == "app/users.py"),
        "{stdout}"
    );
    let (code, stdout, _) = p.heki(&["--output-format", "github", "--touched", "app/jobs.py"]);
    assert_eq!(code, 1);
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert!(
        stdout.starts_with("::error file=app/jobs.py,line=4,"),
        "{stdout}"
    );
    let (code, stdout, _) = p.heki(&["--output-format", "sarif", "--touched", "app/users.py"]);
    assert_eq!(code, 1);
    let sarif: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let results = sarif["runs"][0]["results"].as_array().unwrap();
    assert_eq!(results.len(), 1, "{stdout}");
    assert_eq!(
        results[0]["locations"][0]["physicalLocation"]["artifactLocation"]["uri"],
        "app/api.py"
    );
    // The full format lists the pages of the rules it shows.
    let (code, _, stderr) = p.heki(&["--touched", "app/__init__.py"]);
    assert_eq!(code, 0);
    assert!(!stderr.contains("`heki rule EFF101`"), "{stderr}");

    // A run that watches every change, or that writes the baseline, covers the whole project.
    for option in ["--watch", "--update-baseline"] {
        let (code, stdout, stderr) = p.heki(&[option, "--touched", "app/jobs.py"]);
        assert_eq!((code, stdout.as_str()), (2, ""), "{option}: {stderr}");
        assert!(stderr.contains("cannot be used with"), "{option}: {stderr}");
    }
}

/// A project that keeps files heki cannot parse on purpose (the fixtures of a parser): a run
/// over given files exits with whether those files could be analyzed, and names the others in a
/// note; a run over directories reports every file it could not analyze and fails. No run that
/// reports a file error says that all checks passed.
#[test]
fn runs_over_given_files_fail_only_on_their_own_file_errors() {
    let p = Project::new("fixture_errors");
    p.write("src/p/__init__.py", "")
        .write("src/p/a.py", "def f():\n    return 1\n")
        .write("tests/data/bad1.py", "def f(:\n")
        .write("tests/data/bad2.py", "x = = 1\n");
    let note = "note: 2 file(s) the run does not report on could not be analyzed: `tests/data/bad1.py`, `tests/data/bad2.py`; reports that need their code are missing: name a file to see its error, or exclude it with `extend-exclude`";
    for args in [&["src/p/a.py"][..], &["--touched", "src/p/a.py"]] {
        let (code, stdout, stderr) = p.heki(args);
        assert_eq!((code, stdout.as_str()), (0, ""), "{args:?}: {stderr}");
        assert!(stderr.contains(note), "{args:?}: {stderr}");
        assert!(!stderr.contains("error:"), "{args:?}: {stderr}");
        assert!(
            stderr.contains("All checks passed for 1 touched file(s)"),
            "{args:?}: {stderr}"
        );
    }
    let (code, stdout, stderr) = p.heki(&["src/p/a.py", "--diff"]);
    assert_eq!((code, stdout.as_str()), (0, ""), "{stderr}");
    assert!(stderr.contains(note), "{stderr}");
    assert!(!stderr.contains("error:"), "{stderr}");
    // A given file that does not parse fails the run; the other one is a note.
    let (code, _, stderr) = p.heki(&["src/p/a.py", "tests/data/bad1.py"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("error: tests/data/bad1.py:1:7: syntax error"),
        "{stderr}"
    );
    assert!(
        stderr.contains(
            "note: 1 file(s) the run does not report on could not be analyzed: `tests/data/bad2.py`"
        ),
        "{stderr}"
    );
    assert!(
        stderr.contains("Found 0 diagnostic(s) for 2 touched file(s) (2 file(s) analyzed); 1 file(s) could not be analyzed."),
        "{stderr}"
    );
    // Directories: every file below them is asked for.
    for dirs in [&["."][..], &["src", "tests"]] {
        let (code, _, stderr) = p.heki(dirs);
        assert_eq!(code, 2, "{dirs:?}: {stderr}");
        assert!(!stderr.contains("All checks passed"), "{dirs:?}: {stderr}");
        assert!(!stderr.contains("note: 2 file(s)"), "{dirs:?}: {stderr}");
        assert!(
            stderr.contains("Found 0 diagnostic(s) in 2 file(s); 2 file(s) could not be analyzed."),
            "{dirs:?}: {stderr}"
        );
        assert_eq!(
            stderr.matches("error: tests/data/").count(),
            2,
            "{dirs:?}: {stderr}"
        );
    }
    let (code, _, stderr) = p.heki(&["src"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("All checks passed (2 file(s))."),
        "{stderr}"
    );
    // The note names the first three of the files it counts.
    p.write("tests/data/bad3.py", "y = = 2\n")
        .write("tests/data/bad4.py", "z = = 3\n");
    let (code, _, stderr) = p.heki(&["src/p/a.py"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains(
            "note: 4 file(s) the run does not report on could not be analyzed: `tests/data/bad1.py`, `tests/data/bad2.py`, `tests/data/bad3.py`, ...; reports"
        ),
        "{stderr}"
    );
}

#[test]
fn touched_runs_judge_only_the_baseline_entries_of_the_reports_they_show() {
    let p = touched_project("touched_baseline");
    p.write(
        "heki.toml",
        "baseline = \"b.toml\"\n\n[lint]\nreport-unused-suppressions = true\n",
    )
    .write(
        "app/orders.py",
        "import requests\n\n\ndef load_order(order_id):\n    return requests.get(f\"https://api.example.com/orders/{order_id}\", timeout=5).text\n",
    )
    // One baseline entry with two sites: one through each helper.
    .write(
        "app/api.py",
        "from app.orders import load_order\nfrom app.users import load_user\n\n\nasync def page(user_id):\n    return load_user(user_id), load_order(user_id)\n",
    );
    let (code, _, stderr) = p.heki(&["--update-baseline"]);
    assert_eq!(code, 0, "{stderr}");
    let baseline = std::fs::read_to_string(p.dir.join("b.toml")).unwrap();
    assert!(baseline.contains("count = 2"), "{baseline}");
    let check = |touched: &str| {
        let (code, stdout, stderr) = p.heki(&["--output-format", "concise", "--touched", touched]);
        (code, sites(&stdout).join("\n"), stderr)
    };

    // The entry counts both sites, whichever of them the run shows: nothing grew or shrank.
    for touched in ["app/users.py", "app/orders.py", "app/api.py"] {
        let (code, shown, stderr) = check(touched);
        assert_eq!((code, shown.as_str()), (0, ""), "{touched}: {stderr}");
        assert!(!stderr.contains("baseline entry"), "{touched}: {stderr}");
        assert!(
            stderr.contains("All checks passed for 1 touched file(s) (5 file(s) analyzed)."),
            "{touched}: {stderr}"
        );
    }

    // A fixed report's entry is judged by a run that touches its file only.
    p.write("app/jobs.py", "async def handler():\n    return 1\n");
    let unused = "error: baseline entry app/jobs.py::handler EFF101 `time.sleep` matched no report";
    let (code, _, stderr) = check("app/users.py");
    assert_eq!(code, 0, "{stderr}");
    assert!(!stderr.contains("baseline entry"), "{stderr}");
    let (code, _, stderr) = check("app/jobs.py");
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains(unused), "{stderr}");
    let (code, _, stderr) = p.heki(&["--output-format", "concise"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains(unused), "{stderr}");
    p.write("app/jobs.py", BLOCKING);

    // A third site grows the entry: a run shows the sites of the grown entry that touch its
    // files, and fails on them.
    p.write(
        "app/api.py",
        "from app.orders import load_order\nfrom app.users import load_user\n\n\nasync def page(user_id):\n    return load_user(user_id), load_order(user_id), load_user(user_id + 1)\n",
    );
    let grew =
        "note: baseline entry app/api.py::page EFF101 `requests.get` allows 2 site(s), found 3";
    let (code, shown, stderr) = check("app/users.py");
    assert_eq!(
        (code, shown.as_str()),
        (1, "app/api.py:6:12\napp/api.py:6:53"),
        "{stderr}"
    );
    assert!(stderr.contains(grew), "{stderr}");
    assert!(
        stderr
            .contains("Found 2 Ratchet regression(s) for 1 touched file(s) (5 file(s) analyzed)."),
        "{stderr}"
    );
    let (code, shown, stderr) = check("app/jobs.py");
    assert_eq!((code, shown.as_str()), (0, ""), "{stderr}");
    assert!(!stderr.contains("baseline entry"), "{stderr}");
}

#[test]
fn touched_runs_fold_only_the_reports_they_show() {
    let p = Project::new("touched_fold");
    p.write("app/__init__.py", "").write(
        "app/slow.py",
        "import time\n\n\ndef wait():\n    time.sleep(1)\n",
    );
    for i in 0..11 {
        p.write(
            &format!("app/c{i:02}.py"),
            "from app.slow import wait\n\n\nasync def h():\n    wait()\n",
        );
    }
    let concise = ["--output-format", "concise"];
    let (_, stdout, _) = p.heki(&concise);
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert!(stdout.contains("(+10 more async call sites"), "{stdout}");
    // One caller's report alone, although the folded report of the whole run is another file's.
    let (code, stdout, stderr) = p.heki(&[&concise[..], &["--touched", "app/c07.py"]].concat());
    assert_eq!(code, 1, "{stderr}");
    assert_eq!(sites(&stdout), ["app/c07.py:5:5"], "{stdout}");
    assert!(!stdout.contains("more async call sites"), "{stdout}");
    // Every caller reaches the touched helper: they fold as in the whole run.
    let (_, stdout, _) = p.heki(&[&concise[..], &["--touched", "app/slow.py"]].concat());
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert!(stdout.contains("(+10 more async call sites"), "{stdout}");
}
