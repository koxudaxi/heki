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
        let out = self.command(args).output().unwrap();
        (
            out.status.code().unwrap(),
            String::from_utf8(out.stdout).unwrap(),
            String::from_utf8(out.stderr).unwrap(),
        )
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

const BLOCKING: &str = "import time\n\nasync def handler():\n    time.sleep(1)\n";

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
    p.write("pkg/bad.py", BLOCKING);
    let (code, stdout, stderr) = p.heki(&[]);
    assert_eq!(code, 0);
    assert!(
        stdout.starts_with("pkg/bad.py:4:5: EFF101 (async-blocking-call) [warning] "),
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

#[test]
fn inline_suppressions_ignore_hashes_in_strings() {
    let p = Project::new("noqa");
    p.write(
        "a.py",
        "import time\n\nasync def h():\n    time.sleep(len(\"# heki: ignore[EFF101]\"))\n    time.sleep(1)  # heki: ignore[EFF101]\n    time.sleep(2)  # noqa: async-blocking-call\n    time.sleep(3)  # heki: ignore[MUT201]\n",
    );
    let (code, stdout, _) = p.heki(&["--output-format", "concise"]);
    assert_eq!(code, 0);
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
    assert_eq!(code, 0);
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
fn slower_code_defaults_to_warning() {
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
    // These rules report slower code, not wrong code: `warning` (a bug is an `error`).
    assert_eq!(
        severities(&[]),
        vec![
            pair("OPT101", "warning"),
            pair("OPT113", "warning"),
            pair("EFF101", "warning"),
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
        "[lint.severity]\nOPT101 = \"error\"\nnested-loop-equijoin = \"error\"\n",
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
}

/// A write of `Model.fields` outside the functions its contract ([`WRITE_OWNERS`]) allows:
/// `MUT201`, an `error` (a broken contract).
const CONTRACT_BREACH: &str = "class Model:\n    def __init__(self):\n        self.fields = []\n\n\ndef add(m: Model):\n    m.fields.append(1)\n";
const WRITE_OWNERS: &str = "[[write-owners]]\nname = \"store\"\ntargets = [\"models.Model.fields\"]\nallowed-writers = [\"store.*\"]\n";
const FORMATS: [&str; 5] = ["full", "concise", "json", "github", "sarif"];

#[test]
fn warnings_do_not_fail_the_check_unless_fail_on_says_so() {
    let p = Project::new("fail_on");
    p.write("app.py", BLOCKING);
    // Only warnings: they are printed, and the run passes.
    let (code, stdout, stderr) = p.heki(&[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stdout.starts_with("app.py:4:5: EFF101 (async-blocking-call) [warning] "),
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
        assert!(stdout.contains("EFF101"), "{format}: {stdout}");
        let (code, _, _) = p.heki(&["--output-format", format, "--fail-on", "warning"]);
        assert_eq!(code, 1, "{format}");
    }
    // An error fails the run at either setting, in every format.
    p.write("models.py", CONTRACT_BREACH)
        .write("heki.toml", WRITE_OWNERS);
    for format in FORMATS {
        for fail_on in ["error", "warning"] {
            let (code, stdout, _) = p.heki(&["--output-format", format, "--fail-on", fail_on]);
            assert_eq!(code, 1, "{format} {fail_on}: {stdout}");
            assert!(stdout.contains("MUT201"), "{format}: {stdout}");
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
    assert_eq!(p.heki(&[]).0, 0);
    // A new error is a regression that fails the run.
    p.write(
        "more.py",
        "from models import Model\n\n\ndef add_more(m: Model):\n    m.fields.append(2)\n",
    );
    let (code, stdout, _) = p.heki(&["--output-format", "concise"]);
    assert_eq!(code, 1, "{stdout}");
    assert!(stdout.starts_with("more.py:5:5: MUT201 "), "{stdout}");
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    std::fs::remove_file(p.dir.join("more.py")).unwrap();
    // A new warning is a regression that is reported without failing the run, unless
    // `fail-on` says so.
    p.write("b.py", BLOCKING);
    let (code, stdout, stderr) = p.heki(&["--output-format", "concise"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.starts_with("b.py:4:5: EFF101 "), "{stdout}");
    assert!(stderr.contains("Found 1 Ratchet regression(s)"), "{stderr}");
    assert_eq!(p.heki(&["--fail-on", "warning"]).0, 1);
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
    p.write("b.toml", &recorded.replace("version = 2", "version = 9"));
    let (code, _, stderr) = p.heki(&update);
    assert_eq!(code, 2);
    assert!(stderr.contains("has version 9"), "{stderr}");
    assert!(
        std::fs::read_to_string(p.dir.join("b.toml"))
            .unwrap()
            .contains("version = 9")
    );
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
    assert_eq!(code, 0);
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
    assert_eq!(code, 0);
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
    assert_eq!(code, 0, "{stderr}");
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
