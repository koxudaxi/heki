//! What the paths of `heki check` name. A file is checked in its project: the run analyzes the
//! project root heki finds for it and shows, fixes, and fails on the reports that touch the file,
//! as `--touched` does. Directories are analyzed as given, and a run over part of the project
//! leaves the baseline entries of the other files as they are and notes the project modules its
//! files import that it left out.

mod common;

use common::Project;
use std::path::Path;

const USERS: &str = "import requests\n\n\ndef fetch_name(user_id):\n    return requests.get(f\"https://api.example.com/users/{user_id}\", timeout=5).text\n\n\ndef load_user(user_id):\n    return {\"id\": user_id, \"name\": fetch_name(user_id)}\n";
const API: &str = "from app.users import load_user\n\n\nasync def get_user(user_id):\n    return load_user(user_id)\n";
const BLOCKING: &str = "import time\n\n\nasync def handler():\n    time.sleep(1)\n";

/// `heki check --output-format concise <args>` run in `dir`: the exit code, the `path:line:col`
/// of each report, and stderr.
fn concise_in(dir: &Path, args: &[&str]) -> (i32, Vec<String>, String) {
    let out = common::heki_command()
        .args(["check", "--output-format", "concise"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let sites = stdout
        .lines()
        .map(|l| l.split_once(": ").map_or(l, |(site, _)| site).to_owned())
        .collect();
    let stderr = String::from_utf8(out.stderr).unwrap();
    (out.status.code().unwrap(), sites, stderr)
}

fn concise(p: &Project, args: &[&str]) -> (i32, Vec<String>, String) {
    concise_in(&p.dir, args)
}

/// `app/api.py` blocks on `requests.get` two helpers down in `app/users.py`; `app/jobs.py` blocks
/// on its own. Warnings fail the run.
fn app(name: &str) -> Project {
    let p = Project::new(name);
    p.write("heki.toml", "[lint]\nfail-on = \"warning\"\n")
        .write("app/__init__.py", "")
        .write("app/users.py", USERS)
        .write("app/api.py", API)
        .write("app/jobs.py", BLOCKING);
    p
}

#[test]
fn a_file_is_checked_in_its_project() {
    let p = app("paths_file");
    let (code, sites, _) = concise(&p, &[]);
    assert_eq!(
        (code, sites),
        (1, vec!["app/api.py:5:12".into(), "app/jobs.py:5:5".into()])
    );

    // The report of `app/api.py` needs `app/users.py`, which the run analyzes too.
    let (code, sites, stderr) = concise(&p, &["app/api.py"]);
    assert_eq!(
        (code, sites),
        (1, vec!["app/api.py:5:12".into()]),
        "{stderr}"
    );
    assert!(
        stderr.contains("Found 1 diagnostic(s) for 1 touched file(s) (4 file(s) analyzed)."),
        "{stderr}"
    );
    // As with `--touched`, a report whose call chain passes through the file is shown.
    let (code, sites, _) = concise(&p, &["app/users.py"]);
    assert_eq!((code, sites), (1, vec!["app/api.py:5:12".into()]));
    // Several files: one run over their project.
    let (code, sites, stderr) = concise(&p, &["app/api.py", "app/jobs.py"]);
    assert_eq!(
        (code, sites),
        (1, vec!["app/api.py:5:12".into(), "app/jobs.py:5:5".into()])
    );
    assert!(
        stderr.contains("for 2 touched file(s) (4 file(s) analyzed)"),
        "{stderr}"
    );
    // A file no report touches passes.
    let (code, sites, stderr) = concise(&p, &["app/__init__.py"]);
    assert_eq!((code, sites), (0, vec![]), "{stderr}");

    // From a directory below the root, the project is still the root's.
    let (code, sites, stderr) = concise_in(&p.dir.join("app"), &["api.py"]);
    assert_eq!((code, sites), (1, vec!["api.py:5:12".into()]), "{stderr}");
    assert!(stderr.contains("(4 file(s) analyzed)"), "{stderr}");
    // So from outside the project.
    let outside = common::fresh_dir("paths_file_outside");
    let api = p.dir.join("app/api.py");
    let (code, sites, stderr) = concise_in(&outside, &[api.to_str().unwrap()]);
    assert_eq!((code, sites.len()), (1, 1), "{stderr}");
    assert!(sites[0].ends_with("api.py:5:12"), "{sites:?}");
    assert!(stderr.contains("(4 file(s) analyzed)"), "{stderr}");
    std::fs::remove_dir_all(&outside).unwrap();

    // With directories, the directories are analyzed and the files select what is shown.
    p.write("tests/test_api.py", "from app.api import get_user\n");
    let (code, sites, _) = concise(&p, &["app", "app/jobs.py"]);
    assert_eq!((code, sites), (1, vec!["app/jobs.py:5:5".into()]));
    let (code, sites, stderr) = concise(&p, &["app", "tests/test_api.py"]);
    assert_eq!((code, sites), (0, vec![]), "{stderr}");
    assert!(
        stderr.contains(
            "warning: PATHS name 1 file(s) that are not among the analyzed files (not a Python file of the project or below the directories given, or excluded): `tests/test_api.py`"
        ),
        "{stderr}"
    );

    // A file that the project leaves out, excluded or not a Python file, is named in a warning.
    p.write(
        "heki.toml",
        "extend-exclude = [\"generated\"]\n\n[lint]\nfail-on = \"warning\"\n",
    )
    .write("generated/g.py", BLOCKING)
    .write("tool", BLOCKING);
    let (code, sites, stderr) = concise(&p, &["generated/g.py", "tool"]);
    assert_eq!((code, sites), (0, vec![]), "{stderr}");
    assert!(stderr.contains("PATHS name 2 file(s)"), "{stderr}");
    assert!(
        stderr.contains("No reports in or through 0 touched file(s) (5 file(s) analyzed)."),
        "{stderr}"
    );
}

#[test]
fn a_file_outside_any_project_is_analyzed_alone() {
    // No configuration file and no Git working tree above it: the file is the whole program.
    let dir = common::fresh_dir("paths_no_project");
    std::fs::write(dir.join("a.py"), BLOCKING).unwrap();
    std::fs::write(dir.join("b.py"), BLOCKING).unwrap();
    std::fs::write(dir.join("tool"), BLOCKING).unwrap();
    let (code, sites, stderr) = concise_in(&dir, &["a.py", "tool"]);
    assert_eq!(
        (code, sites),
        (1, vec!["a.py:5:5".into(), "tool:5:5".into()]),
        "{stderr}"
    );
    assert!(
        stderr.contains("Found 2 diagnostic(s) in 2 file(s)."),
        "{stderr}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn an_exact_report_of_a_file_holds_for_its_project() {
    // `app/fast.py` rebinds `load`: the project run reports a heuristic `EFF101`, and so does a
    // run naming `app/api.py`, which analyzes the rebinding too; alone, the file would make an
    // exact `EFF101` "on every path".
    let p = Project::new("paths_rebound");
    p.write("app/__init__.py", "")
        .write(
            "app/api.py",
            "import time\n\n\ndef load():\n    time.sleep(1)\n\n\nasync def handler():\n    load()\n",
        )
        .write(
            "app/fast.py",
            "import app.api\n\n\ndef quick():\n    return None\n\n\napp.api.load = quick\n",
        );
    for args in [&[][..], &["app/api.py"]] {
        let (code, sites, stderr) = concise(&p, args);
        assert_eq!((code, sites), (0, vec![]), "{args:?}: {stderr}");
        let (_, stdout, _) = p.heki(
            &[
                &["check", "--heuristic", "--output-format", "concise"][..],
                args,
            ]
            .concat(),
        );
        assert!(
            stdout.starts_with("app/api.py:9:5: EFF101 [heuristic] ")
                && stdout.lines().count() == 1,
            "{args:?}: {stdout}"
        );
    }
}

#[test]
fn a_file_run_judges_and_fixes_suppressions_in_the_project() {
    let p = app("paths_suppressions");
    p.write(
        "heki.toml",
        "[lint]\nfail-on = \"warning\"\nreport-unused-suppressions = true\n",
    )
    .write(
        "app/api.py",
        &API.replace(
            "load_user(user_id)\n",
            "load_user(user_id)  # heki: ignore[EFF101]\n",
        ),
    );
    let (code, sites, stderr) = concise(&p, &["app/api.py"]);
    assert_eq!((code, sites), (0, vec![]), "{stderr}");
    let (code, stdout, stderr) = p.heki(&["check", "--diff", "app/api.py"]);
    assert_eq!((code, stdout.as_str()), (0, ""), "{stderr}");
    assert!(
        stderr.contains("Would fix 0 issue(s) in 0 file(s)."),
        "{stderr}"
    );
    let (code, _, stderr) = p.heki(&["check", "--fix", "app/api.py"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(p.read("app/api.py").contains("# heki: ignore[EFF101]"));
}

#[test]
fn a_file_run_judges_only_the_baseline_entries_it_shows() {
    let p = app("paths_baseline");
    let (code, _, stderr) = p.heki(&["check", "--baseline", "b.toml", "--update-baseline"]);
    assert_eq!(code, 0, "{stderr}");
    let recorded = p.read("b.toml");
    assert_eq!(recorded.matches("[[entries]]").count(), 2, "{recorded}");
    // `app/jobs.py` no longer blocks: only a run that shows it says its entry can shrink.
    p.write("app/jobs.py", "async def handler():\n    return 1\n");
    let (code, sites, stderr) = concise(&p, &["--baseline", "b.toml", "app/api.py"]);
    assert_eq!((code, sites), (0, vec![]), "{stderr}");
    assert!(!stderr.contains("baseline entry"), "{stderr}");
    let (_, _, stderr) = concise(&p, &["--baseline", "b.toml"]);
    assert!(
        stderr.contains(
            "note: baseline entry app/jobs.py::handler EFF101 `time.sleep` can shrink 1 -> 0"
        ),
        "{stderr}"
    );
    // A baseline records every report of what a run covers: a file names what to show.
    let (code, stdout, stderr) = p.heki(&[
        "check",
        "--baseline",
        "b.toml",
        "--update-baseline",
        "app/api.py",
    ]);
    assert_eq!((code, stdout.as_str()), (2, ""), "{stderr}");
    assert!(
        stderr.contains(
            "error: `--update-baseline` records the reports of directories, and `app/api.py` is a file"
        ),
        "{stderr}"
    );
    assert_eq!(p.read("b.toml"), recorded);
    // `--watch` shows every report of what it analyzes.
    let (code, _, stderr) = p.heki(&["check", "--watch", "app/api.py"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(
        stderr.contains("error: `--watch` watches directories, and `app/api.py` is a file"),
        "{stderr}"
    );
}

/// Two packages that do not import each other, each blocking once.
fn two_packages(name: &str) -> Project {
    let p = Project::new(name);
    for package in ["pkg_a", "pkg_b"] {
        p.write(&format!("{package}/__init__.py"), "")
            .write(&format!("{package}/api.py"), BLOCKING);
    }
    p
}

#[test]
fn a_run_over_part_of_the_project_keeps_the_entries_of_the_other_files() {
    let p = two_packages("paths_part_baseline");
    let (code, _, stderr) = p.heki(&["check", "--baseline", "b.toml", "--update-baseline"]);
    assert_eq!(code, 0, "{stderr}");
    let entry = |package: &str| format!("path = \"{package}/api.py\"");
    // `pkg_b` no longer blocks; a run over `pkg_a` neither judges nor removes its entry.
    p.write("pkg_b/api.py", "async def handler():\n    return 1\n");
    let (code, _, stderr) = concise(&p, &["--baseline", "b.toml", "pkg_a"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(!stderr.contains("baseline entry"), "{stderr}");
    let (code, _, stderr) = p.heki(&[
        "check",
        "--baseline",
        "b.toml",
        "--update-baseline",
        "pkg_a",
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("Wrote Ratchet baseline b.toml (2 entries, 1 diagnostic(s))."),
        "{stderr}"
    );
    let baseline = p.read("b.toml");
    assert!(
        baseline.contains(&entry("pkg_a")) && baseline.contains(&entry("pkg_b")),
        "{baseline}"
    );
    // The run over `pkg_b` removes it; the entry of `pkg_a` stays.
    let (code, _, stderr) = p.heki(&[
        "check",
        "--baseline",
        "b.toml",
        "--update-baseline",
        "pkg_b",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let baseline = p.read("b.toml");
    assert!(
        baseline.contains(&entry("pkg_a")) && !baseline.contains(&entry("pkg_b")),
        "{baseline}"
    );
    // The whole project then has no regression.
    let (code, sites, stderr) = concise(&p, &["--baseline", "b.toml", "--fail-on", "warning"]);
    assert_eq!((code, sites), (0, vec![]), "{stderr}");

    // A baseline that counts diagnostics (version 1) is rewritten only whole: site counts beside
    // its entries would mix units.
    p.write(
        "b.toml",
        "[[entries]]\npath = \"pkg_a/api.py\"\nqualname = \"handler\"\nrule_code = \"EFF101\"\ntarget = \"time.sleep\"\ncount = 1\n\n[[entries]]\npath = \"pkg_b/api.py\"\nqualname = \"handler\"\nrule_code = \"EFF101\"\ntarget = \"time.sleep\"\ncount = 1\n",
    );
    let legacy = p.read("b.toml");
    let (code, _, stderr) = p.heki(&[
        "check",
        "--baseline",
        "b.toml",
        "--update-baseline",
        "pkg_a",
    ]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("baseline `b.toml` not updated: it counts diagnostics (written before version 2), and this run covers part of the project"),
        "{stderr}"
    );
    assert_eq!(p.read("b.toml"), legacy);
    let (code, _, stderr) = p.heki(&["check", "--baseline", "b.toml", "--update-baseline"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        p.read("b.toml").starts_with("# heki Ratchet baseline"),
        "{}",
        p.read("b.toml")
    );

    // So is a version 2 file: some exact reports have other keys in version 3, and the kept
    // entries of the files the run does not cover could not be keyed again.
    p.write(
        "b.toml",
        "version = 2\n\n[[entries]]\npath = \"pkg_a/api.py\"\nqualname = \"handler\"\nrule_code = \"EFF101\"\ntarget = \"time.sleep\"\ncount = 1\n\n[[entries]]\npath = \"pkg_b/api.py\"\nqualname = \"handler\"\nrule_code = \"EFF101\"\ntarget = \"time.sleep\"\ncount = 1\n",
    );
    let legacy = p.read("b.toml");
    let (code, _, stderr) = p.heki(&[
        "check",
        "--baseline",
        "b.toml",
        "--update-baseline",
        "pkg_a",
    ]);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("baseline `b.toml` not updated: it was written before version 3, which keys some reports by another call, and this run covers part of the project"),
        "{stderr}"
    );
    assert_eq!(p.read("b.toml"), legacy);
    let (code, _, stderr) = p.heki(&["check", "--baseline", "b.toml", "--update-baseline"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        p.read("b.toml").contains("version = 3"),
        "{}",
        p.read("b.toml")
    );
}

#[test]
fn a_run_below_the_root_notes_the_project_modules_it_leaves_out() {
    let p = Project::new("paths_left_out_imports");
    p.write("app/__init__.py", "")
        .write(
            "app/api.py",
            "from lib.clients import fetch\n\n\nasync def handler(url):\n    return fetch(url)\n",
        )
        .write("lib/__init__.py", "")
        .write(
            "lib/clients.py",
            "import requests\n\n\ndef fetch(url):\n    return requests.get(url, timeout=5).text\n",
        )
        .write("tools/__init__.py", "")
        .write("tools/report.py", BLOCKING);
    let note = |root: &str| {
        format!(
            "note: the analyzed files import 1 project module(s) that PATHS leave out: `lib.clients` (lib/clients.py); reports that need their code are missing: check the project root `{root}`, with `--touched` for the files to report on"
        )
    };
    let (code, sites, stderr) = concise(&p, &["app"]);
    assert_eq!((code, sites), (0, vec![]), "{stderr}");
    assert!(stderr.contains(&note(".")), "{stderr}");
    for args in [&[][..], &["--touched", "api.py"]] {
        let (code, sites, stderr) = concise_in(&p.dir.join("app"), args);
        assert_eq!((code, sites), (0, vec![]), "{args:?}: {stderr}");
        assert!(stderr.contains(&note("..")), "{args:?}: {stderr}");
    }
    // A run over the root, or a file checked in its project, leaves nothing out.
    let (_, sites, stderr) = concise(&p, &[]);
    assert_eq!(sites, ["app/api.py:5:12", "tools/report.py:5:5"]);
    assert!(!stderr.contains("leave out"), "{stderr}");
    let (_, sites, stderr) = concise_in(&p.dir.join("app"), &["api.py"]);
    assert_eq!(sites, ["api.py:5:12"]);
    assert!(!stderr.contains("leave out"), "{stderr}");
    // A chosen set of files that imports nothing outside it is no such run.
    let (_, sites, stderr) = concise(&p, &["tools"]);
    assert_eq!(sites, ["tools/report.py:5:5"]);
    assert!(!stderr.contains("leave out"), "{stderr}");
    // An import under `if TYPE_CHECKING:` never runs.
    p.write(
        "tools/report.py",
        "from typing import TYPE_CHECKING\n\nif TYPE_CHECKING:\n    from lib.clients import fetch\n",
    );
    let (_, _, stderr) = concise(&p, &["tools"]);
    assert!(!stderr.contains("leave out"), "{stderr}");
    // A plain import, a star import, and an import in a function all run; a relative import
    // past the top-level package names nothing. The note names three modules.
    p.write("lib/a.py", "")
        .write("lib/b.py", "")
        .write("lib/c.py", "")
        .write(
            "tools/report.py",
            "import lib.a\nfrom lib.b import *\nfrom lib.clients import fetch\n\n\ndef late():\n    from lib import c\n    from ... import gone\n",
        );
    let (_, _, stderr) = concise(&p, &["tools"]);
    assert!(
        stderr.contains(
            "note: the analyzed files import 4 project module(s) that PATHS leave out: `lib.a` (lib/a.py), `lib.b` (lib/b.py), `lib.c` (lib/c.py), ...; "
        ),
        "{stderr}"
    );
}

#[test]
fn other_commands_take_a_file_as_its_project() {
    let p = app("paths_commands");
    // The summaries of every function of the project.
    let (code, stdout, stderr) = p.heki(&["snapshot", "app/api.py"]);
    assert_eq!(code, 0, "{stderr}");
    let snapshot: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(
        snapshot["functions"]["app.users:fetch_name"].is_object(),
        "{stdout}"
    );
    let (code, stdout, stderr) = p.heki(&["recommend", "--output-format", "json", "app/api.py"]);
    assert_eq!(code, 0, "{stderr}");
    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["analyzed"]["files"], 4, "{stdout}");
    assert_eq!(
        report["analyzed"]["left_out"]["python_files"], 0,
        "{stdout}"
    );
    // The prompt names the file among the touched ones.
    let (code, stdout, stderr) = p.heki(&[
        "check",
        "--output-format",
        "json",
        "app/api.py",
        "--generate-prompt",
    ]);
    assert_eq!(code, 0, "{stderr}");
    let prompt: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        prompt["configuration"]["touched"],
        serde_json::json!(["app/api.py"])
    );
    assert_eq!(prompt["diagnostics"]["total"], 1, "{stdout}");
}
