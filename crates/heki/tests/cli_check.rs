//! End-to-end `heki check` tests through the real binary.

use std::path::{Path, PathBuf};
use std::process::Command;

struct Project {
    dir: PathBuf,
}

impl Project {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!("heki_cli_{name}_{}", std::process::id()));
        if dir.exists() {
            std::fs::remove_dir_all(&dir).unwrap();
        }
        std::fs::create_dir_all(&dir).unwrap();
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

    fn heki(&self, args: &[&str]) -> (i32, String, String) {
        let out = Command::new(env!("CARGO_BIN_EXE_heki"))
            .arg("check")
            .args(args)
            .current_dir(&self.dir)
            .output()
            .unwrap();
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
        "[lint]\nrelax-async-in-tests = false\n\n[[write-owners]]\nname = \"store\"\ntargets = [\"Model.fields\"]\nallowed-writers = [\"Store.*\"]\n",
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
        (92, "MUT202"),
        (108, "EFF101"),
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

    p.write("pkg/bad.py", BLOCKING);
    let (code, stdout, stderr) = p.heki(&[]);
    assert_eq!(code, 1);
    assert!(
        stdout.starts_with("pkg/bad.py:4:5: EFF101 (async-blocking-call) [error]"),
        "{stdout}"
    );
    assert!(stdout.contains("4 |     time.sleep(1)"));
    assert!(stderr.contains("Found 1 diagnostic(s)"));

    assert_eq!(p.heki(&["--select", "NOPE9"]).0, 2);
    assert_eq!(p.heki(&["--target-version", "py27"]).0, 2);
    assert_eq!(p.heki(&["missing_dir"]).0, 2);
    assert_eq!(p.heki(&["--ignore", "EFF101"]).0, 0);

    p.write("pkg/broken.py", "def f(:\n");
    let (code, _, stderr) = p.heki(&[]);
    assert_eq!(code, 2);
    assert!(stderr.contains("pkg/broken.py:1:"), "{stderr}");
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
    p.write("a.py", BLOCKING);
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
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nper-file-ignores = { \"src/pkg/legacy.py\" = [\"EFF\"] }\n\n[tool.heki.lint.severity]\nEFF101 = \"warning\"\n",
        );
    let (code, stdout, _) = p.heki(&["--output-format", "github"]);
    assert_eq!(code, 1);
    assert_eq!(
        stdout.lines().collect::<Vec<_>>(),
        vec![
            "::warning file=src/pkg/api.py,line=4,col=5,title=heki EFF101::`async def handler` calls `time.sleep`, which reaches a blocking call `time.sleep` (sleep) on the event loop; offload it (`asyncio.to_thread`) or use an async API"
        ]
    );
    let (_, stdout, _) = p.heki(&["--output-format", "sarif"]);
    let sarif: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(sarif["runs"][0]["tool"]["driver"]["name"], "heki");
    assert_eq!(sarif["runs"][0]["results"][0]["level"], "warning");

    // `src/` layout: the module is `pkg.api`, so the qualname and summaries resolve there.
    let (_, stdout, _) = p.heki(&["--output-format", "json"]);
    let diags: Vec<serde_json::Value> = serde_json::from_str(&stdout).unwrap();
    assert_eq!(diags[0]["qualname"], "handler");

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

#[test]
fn sarif_columns_and_carets_after_non_ascii_text() {
    let p = Project::new("non_ascii_columns");
    p.write(
        "a.py",
        "import time\n\nasync def h():\n    label = \"é\"; time.sleep(1)\n",
    );
    let (_, full, _) = p.heki(&[]);
    // Byte column 19 in text output; the caret sits under `time` (`é` is one column).
    assert!(full.starts_with("a.py:4:19: EFF101"), "{full}");
    assert!(
        full.contains("4 |     label = \"é\"; time.sleep(1)\n  |                  ^\n"),
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
    // The stale entry's note and the syntax error both stay on one line each.
    assert_eq!(stderr.lines().count(), 2, "{stderr}");
    assert!(stderr.lines().all(|l| !l.starts_with("::")), "{stderr}");
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
            "baseline b.toml lists a.py::handler EFF101 `time.sleep` more than once; keep one entry"
        ),
        "{stderr}"
    );
}
