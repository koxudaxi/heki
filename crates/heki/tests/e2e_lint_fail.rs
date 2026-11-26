//! `lint.fail`: the reports of the rules it lists fail `heki check` whatever their tier and
//! severity, and nothing else changes (end to end, through the real binary).

mod common;

use common::Project;

const PKG_A: &str = "from pkg import b\n\n\ndef f() -> object:\n    return b\n";
const PKG_B: &str = "from pkg import a\n\n\ndef g() -> object:\n    return a\n";
/// A heuristic `EFF101` report, a warning for the run.
const BLOCKING: &str = "import time\n\n\ndef pause(seconds: float) -> None:\n    if seconds > 0:\n        time.sleep(seconds)\n\n\nasync def handler(seconds: float) -> None:\n    pause(seconds)\n";

/// The project with one import cycle (`pkg.a` and `pkg.b`) and the `lint` table `lint`.
fn cycle(name: &str, lint: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\nrequires-python = \">=3.11\"\n\n[tool.heki.lint]\n{lint}"
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/a.py", PKG_A)
    .write("pkg/b.py", PKG_B);
    p
}

/// `heki check` as CI runs it: no `--fail-on`, no `--heuristic`.
fn check(p: &Project, args: &[&str]) -> (i32, String, String) {
    let mut all = vec!["check"];
    all.extend_from_slice(args);
    p.heki(&all)
}

#[test]
fn a_listed_rule_fails_the_run_and_without_the_list_a_new_cycle_passes() {
    let (code, out, err) = check(&cycle("lint_fail_off", "select = [\"IMP101\"]\n"), &[]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(err.contains("does not fail on 1 report(s)"), "{err}");
    let lint = "select = [\"IMP101\"]\nfail = [\"IMP101\"]\n";
    let (code, out, err) = check(&cycle("lint_fail_on", lint), &[]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains("IMP101"), "{out}");
    // The report fails the run: the closing note has nothing to say about it, and its tier and
    // severity are the ones it had.
    assert!(!err.contains("does not fail on"), "{err}");
    assert!(out.contains("[warning, heuristic]"), "{out}");
}

#[test]
fn a_config_file_names_the_list_as_the_lint_table_of_its_own() {
    let p = cycle("lint_fail_config", "");
    p.write("heki.toml", "[lint]\nselect = [\"IMP101\"]\n");
    assert_eq!(check(&p, &["--config", "heki.toml", "pkg"]).0, 0);
    p.write(
        "heki.toml",
        "[lint]\nselect = [\"IMP101\"]\nfail = [\"IMP101\"]\n",
    );
    let (code, out, err) = check(&p, &["--config", "heki.toml", "pkg"]);
    assert_eq!(code, 1, "{out}{err}");
}

#[test]
fn another_rule_alone_does_not_fail_the_run() {
    let lint = "select = [\"IMP101\", \"EFF101\"]\nfail = [\"IMP101\"]\n";
    let p = cycle("lint_fail_other", lint);
    std::fs::remove_file(p.dir.join("pkg/b.py")).unwrap();
    p.write("eff.py", BLOCKING);
    let (code, out, err) = check(&p, &[]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains("EFF101") && !out.contains("IMP101"), "{out}");
    assert!(err.contains("does not fail on 1 report(s)"), "{err}");
    // The cycle comes back next to it: the run fails on it, and the note counts the other one.
    p.write("pkg/b.py", PKG_B);
    let (code, out, err) = check(&p, &[]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains("IMP101") && out.contains("EFF101"), "{out}");
    assert!(err.contains("does not fail on 1 report(s)"), "{err}");
}

#[test]
fn a_prefix_a_name_and_a_family_name_the_rules_as_select_does() {
    for listed in ["IMP", "import-cycle", "ALL", "imp101"] {
        let lint = format!("select = [\"IMP101\"]\nfail = [\"{listed}\"]\n");
        let (code, out, err) = check(&cycle("lint_fail_prefix", &lint), &[]);
        assert_eq!(code, 1, "{listed}: {out}{err}");
    }
    let lint = "select = [\"IMP101\"]\nfail = [\"EFF\", \"MUT\"]\n";
    assert_eq!(check(&cycle("lint_fail_family", lint), &[]).0, 0);
}

#[test]
fn a_listed_rule_that_is_not_selected_does_nothing() {
    let lint = "select = [\"EFF101\"]\nfail = [\"IMP101\"]\n";
    let (code, out, err) = check(&cycle("lint_fail_unselected", lint), &[]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(!out.contains("IMP101"), "{out}");
}

#[test]
fn a_report_the_baseline_records_does_not_fail_and_a_new_one_does() {
    let lint = "select = [\"IMP101\"]\nfail = [\"IMP101\"]\n";
    let p = cycle("lint_fail_baseline", lint);
    let (code, _, err) = check(&p, &["--baseline", "base.toml", "--update-baseline"]);
    assert_eq!(code, 0, "{err}");
    let (code, out, err) = check(&p, &["--baseline", "base.toml"]);
    assert_eq!(code, 0, "{out}{err}");
    p.write("pkg/c.py", "from pkg import d\n")
        .write("pkg/d.py", "from pkg import c\n");
    let (code, out, err) = check(&p, &["--baseline", "base.toml"]);
    assert_eq!(code, 1, "{out}{err}");
}

#[test]
fn a_report_an_inline_suppression_hides_does_not_fail() {
    let lint = "select = [\"IMP101\"]\nfail = [\"IMP101\"]\n";
    let p = cycle("lint_fail_suppressed", lint);
    p.write(
        "pkg/b.py",
        "from pkg import a  # heki: ignore[IMP101]\n\n\ndef g() -> object:\n    return a\n",
    );
    let (code, out, err) = check(&p, &[]);
    assert_eq!(code, 0, "{out}{err}");
    p.write("pkg/b.py", PKG_B);
    assert_eq!(check(&p, &[]).0, 1);
}

#[test]
fn touched_fails_on_the_reports_it_shows() {
    let lint = "select = [\"IMP101\"]\nfail = [\"IMP101\"]\n";
    let p = cycle("lint_fail_touched", lint);
    // The report is in `pkg/b.py`.
    let (code, out, err) = check(&p, &["--touched", "pkg/b.py"]);
    assert_eq!(code, 1, "{out}{err}");
    // A file that is not part of the cycle shows nothing, and the note says what it hides.
    p.write("clean.py", "def fine() -> int:\n    return 1\n");
    let (code, out, err) = check(&p, &["--touched", "clean.py"]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(err.contains("would fail the run are not shown"), "{err}");
}

#[test]
fn a_code_that_names_no_rule_is_a_configuration_error() {
    for listed in ["NOPE1", "", "EFF999"] {
        let lint = format!("select = [\"IMP101\"]\nfail = [\"{listed}\"]\n");
        let (code, out, err) = check(&cycle("lint_fail_unknown", &lint), &[]);
        assert_eq!(code, 2, "{listed:?}: {out}{err}");
        assert!(
            err.contains("unknown rule selector") && err.contains("lint.fail"),
            "{err}"
        );
    }
}

#[test]
fn fail_takes_a_list_of_rules() {
    let (code, out, err) = check(&cycle("lint_fail_type", "fail = true\n"), &[]);
    assert_eq!(code, 2, "{out}{err}");
    assert!(err.contains("fail"), "{err}");
}

#[test]
fn a_listed_contract_rule_fails_the_run_whatever_its_table_says() {
    let lint = "select = [\"IMP201\"]\nfail = [\"IMP201\"]\n";
    let p = Project::new("lint_fail_contract");
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\nrequires-python = \">=3.11\"\n\n[tool.heki.lint]\n{lint}\n\
             [[tool.heki.import-boundaries]]\nname = \"o\"\nimporters = [\"pkg.a\"]\n\
             forbidden-imports = [\"pkg.b\"]\n"
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/a.py", "import pkg.b\n")
    .write("pkg/b.py", "");
    let (code, out, err) = check(&p, &[]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(out.contains("IMP201"), "{out}");
}

#[test]
fn watch_exits_with_1_on_a_report_of_a_listed_rule() {
    for (fail, expected) in [("", Some(0)), ("fail = [\"IMP101\"]\n", Some(1))] {
        let lint = format!("select = [\"IMP101\"]\n{fail}");
        let p = cycle("lint_fail_watch", &lint);
        let out = common::heki_command()
            .args(["check", "--watch"])
            .current_dir(&p.dir)
            .env("HEKI_WATCH_MAX_RUNS", "0")
            .output()
            .unwrap();
        assert_eq!(out.status.code(), expected, "{fail:?}");
    }
}
