//! Rule selection as Ruff users expect it: an empty `select` and `per-file-ignores` globs (end
//! to end, through the real binary).

mod common;

use common::Project;

const BLOCKING: &str = "import time\n\n\nasync def handler():\n    time.sleep(1)\n";

fn concise(p: &Project, args: &[&str]) -> Vec<String> {
    let mut all = vec!["--output-format", "concise"];
    all.extend_from_slice(args);
    let (code, stdout, stderr) = p.check(&all);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    stdout.lines().map(str::to_owned).collect()
}

#[test]
fn an_empty_select_starts_from_no_rules() {
    let p = Project::new("empty_select");
    p.write("app.py", BLOCKING).write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = []\nextend-select = [\"MUT106\"]\n",
    );
    assert_eq!(concise(&p, &[]), Vec::<String>::new());
    // Without the key, the default rules run.
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"MUT106\"]\n",
    );
    let out = concise(&p, &[]);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].starts_with("app.py:5:5: EFF101"), "{out:?}");
    // `--select` on the command line wins over an empty configured `select`.
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = []\n",
    );
    assert_eq!(concise(&p, &["--select", "EFF101"]).len(), 1);
}

#[test]
fn per_file_ignores_globs_match_across_directories_as_in_ruff() {
    let p = Project::new("per_file_globs");
    p.write("scripts/top.py", BLOCKING)
        .write("scripts/tools/worker.py", BLOCKING)
        .write("src/app.py", BLOCKING)
        .write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nper-file-ignores = { \"scripts/*.py\" = [\"EFF101\"] }\n",
        );
    let out = concise(&p, &[]);
    assert_eq!(out.len(), 1, "{out:?}");
    assert!(out[0].starts_with("src/app.py:5:5: EFF101"), "{out:?}");
}

/// A project with a heuristic rule's report (`EFF101`) and a contract rule's (`MUT201`).
fn heuristic_and_contract(name: &str, lint: &str) -> Project {
    let p = Project::new(name);
    p.write("app.py", BLOCKING)
        .write(
            "models.py",
            "class Model:\n    def __init__(self):\n        self.fields = []\n\n\ndef add(m: Model):\n    m.fields.append(1)\n",
        )
        .write(
            "pyproject.toml",
            &format!(
                "[project]\nname = \"t\"\n\n[tool.heki.lint]\n{lint}\n[[tool.heki.write-owners]]\nname = \"store\"\ntargets = [\"models.Model.fields\"]\nallowed-writers = [\"store.*\"]\n"
            ),
        );
    p
}

/// `heki check <args>` as the user types it (without the `--heuristic` of `Project::check`):
/// `(exit code, codes reported, stderr)`.
fn plain(p: &Project, args: &[&str]) -> (i32, Vec<String>, String) {
    let all = [&["check", "--output-format", "json"][..], args].concat();
    let (code, stdout, stderr) = p.heki(&all);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<serde_json::Value> = serde_json::from_str(&stdout).unwrap();
    let codes = diags
        .iter()
        .map(|d| d["rule_code"].as_str().unwrap().to_owned())
        .collect();
    (code, codes, stderr)
}

#[test]
fn heuristic_rules_run_when_named_or_with_lint_heuristic_and_contract_rules_when_declared() {
    let p = heuristic_and_contract("heuristic_selection", "");
    // No rule is exact: the default set is the contract rules.
    assert_eq!(plain(&p, &[]).1, ["MUT201"]);
    // A prefix or `ALL` selects no heuristic rule without `lint.heuristic`; a contract rule it
    // matches still runs.
    assert_eq!(plain(&p, &["--select", "EFF"]).1, Vec::<String>::new());
    assert_eq!(plain(&p, &["--select", "ALL"]).1, ["MUT201"]);
    // An exact code or name selects it.
    assert_eq!(plain(&p, &["--select", "EFF101"]).1, ["EFF101"]);
    assert_eq!(
        plain(&p, &["--extend-select", "async-blocking-call"]).1,
        ["EFF101", "MUT201"]
    );
    // `--heuristic` adds every heuristic rule that is not opt-in, by default and for prefixes.
    assert_eq!(plain(&p, &["--heuristic"]).1, ["EFF101", "MUT201"]);
    assert_eq!(plain(&p, &["--heuristic", "--select", "EFF"]).1, ["EFF101"]);
    // The last of `--heuristic` / `--no-heuristic` wins.
    assert_eq!(plain(&p, &["--heuristic", "--no-heuristic"]).1, ["MUT201"]);

    // The configuration key, which the command line overrides both ways.
    let p = heuristic_and_contract("heuristic_key", "heuristic = true\n");
    assert_eq!(plain(&p, &[]).1, ["EFF101", "MUT201"]);
    assert_eq!(plain(&p, &["--no-heuristic"]).1, ["MUT201"]);
    let p = heuristic_and_contract("heuristic_key_off", "heuristic = false\n");
    assert_eq!(plain(&p, &["--heuristic"]).1, ["EFF101", "MUT201"]);
    let p = heuristic_and_contract("heuristic_key_typed", "heuristic = \"yes\"\n");
    let (code, _, stderr) = p.heki(&["check"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("heuristic"), "{stderr}");
}

#[test]
fn a_text_run_says_how_many_heuristic_rules_it_left_off() {
    let p = heuristic_and_contract("heuristic_hint", "");
    let every = heki_rules_off_by_default(&p);
    let note = |n: usize| {
        format!(
            "note: {n} heuristic rule(s) are off; enable them with `--heuristic` or `lint.heuristic = true`\n"
        )
    };
    for format in ["full", "concise"] {
        let (code, stdout, stderr) = p.heki(&["check", "--output-format", format]);
        assert_eq!(code, 1, "{stdout}{stderr}");
        assert!(stderr.ends_with(&note(every)), "{format}: {stderr}");
    }
    // Only the rules a selection would run with `lint.heuristic` count, not the ignored ones.
    let (_, _, stderr) = p.heki(&["check", "--select", "EFF"]);
    assert!(stderr.ends_with(&note(3)), "{stderr}");
    let (_, _, stderr) = p.heki(&["check", "--select", "EFF", "--ignore", "EFF102"]);
    assert!(stderr.ends_with(&note(2)), "{stderr}");
    // Nothing left off, nothing said.
    for args in [
        &["--heuristic"][..],
        &["--select", "EFF101"],
        &["--select", "MUT201"],
    ] {
        let (_, _, stderr) = p.heki(&[&["check"][..], args].concat());
        assert!(!stderr.contains("heuristic"), "{args:?}: {stderr}");
    }
    // Machine-readable formats print no note.
    for format in ["json", "github", "sarif"] {
        let (_, _, stderr) = p.heki(&["check", "--output-format", format]);
        assert!(!stderr.contains("heuristic"), "{format}: {stderr}");
    }
}

/// The heuristic rules that would run by default once exact, from `heki rule`.
fn heki_rules_off_by_default(p: &Project) -> usize {
    let (code, out, _) = p.heki(&["rule", "--all", "--output-format", "json"]);
    assert_eq!(code, 0);
    let rules: Vec<serde_json::Value> = serde_json::from_str(&out).unwrap();
    let off = rules
        .iter()
        .filter(|r| r["activation"] == "default" && r["certainty"] == "heuristic")
        .count();
    assert!(off > 0);
    off
}

#[test]
fn every_output_labels_a_heuristic_rule_and_only_that() {
    let p = heuristic_and_contract("heuristic_labels", "heuristic = true\n");
    let (_, full, _) = p.heki(&["check"]);
    assert!(
        full.contains("app.py:5:5: EFF101 (async-blocking-call) [error, heuristic] `handler`"),
        "{full}"
    );
    assert!(
        full.contains("MUT201 (write-owner-violation) [error, heuristic] "),
        "{full}"
    );
    let (_, concise, _) = p.heki(&["check", "--output-format", "concise"]);
    assert!(
        concise.starts_with("app.py:5:5: EFF101 [heuristic] `handler`"),
        "{concise}"
    );
    let (_, github, _) = p.heki(&["check", "--output-format", "github"]);
    assert!(
        github.contains(",title=heki EFF101 (heuristic)::"),
        "{github}"
    );
    let (_, json, _) = p.heki(&["check", "--output-format", "json"]);
    let diags: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    assert!(
        diags.iter().all(|d| d["certainty"] == "heuristic"),
        "{json}"
    );
    let (_, sarif, _) = p.heki(&["check", "--output-format", "sarif"]);
    let sarif: serde_json::Value = serde_json::from_str(&sarif).unwrap();
    for rule in sarif["runs"][0]["tool"]["driver"]["rules"]
        .as_array()
        .unwrap()
    {
        assert_eq!(
            rule["properties"],
            serde_json::json!({ "precision": "medium", "tags": ["heuristic"] }),
            "{rule}"
        );
    }
}
