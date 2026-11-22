//! `heki recommend` (experimental) end to end: the counts it takes from the analysis `heki check`
//! runs, the files the paths leave out, the baseline, the configuration file it writes for, its
//! JSON object against the published schema, and that it writes nothing.

mod common;

use common::{Project, schema};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::process::Command;

const USERS: &str = "\
import requests

def fetch_name(user_id: int) -> str:
    return requests.get(f\"https://api.example.com/users/{user_id}\", timeout=5).text

def load_user(user_id: int) -> dict[str, str]:
    return {\"id\": str(user_id), \"name\": fetch_name(user_id)}

def maybe_load(user_id: int, fresh: bool) -> dict[str, str]:
    if fresh:
        return load_user(user_id)
    return {\"id\": str(user_id)}
";

const API: &str = "\
from app.users import load_user, maybe_load

async def get_user(user_id: int) -> dict[str, str]:
    return load_user(user_id)

async def get_cached(user_id: int, fresh: bool) -> dict[str, str]:
    return maybe_load(user_id, fresh)
";

/// `OPT101` exact once; `OPT110` heuristic twice (the rewrite of a list the loop body may see
/// is not proven), withheld unless named or with `lint.heuristic`.
const LISTS: &str = "\
def collect(rows):
    acc = []
    for row in rows:
        acc = acc + [row]
    return acc

def known_names(names, source):
    known = list(source)
    found = []
    for name in names:
        if name in known:
            found.append(name)
    return found

def logged_names(names, source, log):
    known = list(source)
    found = []
    for name in names:
        log(known)
        if name in known:
            found.append(name)
    return found
";

/// A project with an exact `EFF101` and `OPT101` report, two heuristic `OPT110` reports, an
/// `EFF111` report, and four test files.
fn shop(name: &str) -> Project {
    let p = Project::new(name);
    p.write("app/__init__.py", "")
        .write("app/users.py", USERS)
        .write("app/api.py", API)
        .write("app/lists.py", LISTS);
    for t in ["a", "b", "c", "d"] {
        p.write(
            &format!("tests/test_{t}.py"),
            &format!("def test_{t}():\n    pass\n"),
        );
    }
    p
}

/// `heki recommend --output-format json <args>`, with its exit code.
fn recommend_json(p: &Project, args: &[&str]) -> (i32, Value, String) {
    let mut all = vec!["recommend", "--output-format", "json"];
    all.extend_from_slice(args);
    let (code, out, err) = p.heki(&all);
    let value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}{err}"));
    (code, value, err)
}

/// The diagnostics `heki check --output-format json <args>` prints.
fn checked(p: &Project, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["check", "--output-format", "json"];
    all.extend_from_slice(args);
    let (_, out, err) = p.heki(&all);
    serde_json::from_str::<Vec<Value>>(&out).unwrap_or_else(|e| panic!("{e}: {out}{err}"))
}

fn assert_follows_the_schema(report: &Value) {
    let (code, out, _) =
        Project::new("recommend_schema").heki(&["recommend", "--output-format-json-schema"]);
    assert_eq!(code, 0);
    let published: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(published["$id"], report["$schema"]);
    let errors = schema::errors(&published, report);
    assert!(errors.is_empty(), "{errors:#?}\n{report:#}");
}

#[test]
fn reports_now_are_what_heki_check_shows_and_withholds() {
    let p = shop("recommend_now");
    let (code, out, err) = p.heki(&["recommend"]);
    assert_eq!((code, err.as_str()), (0, ""));
    assert_eq!(
        out,
        "heki recommend (experimental) on .: 8 file(s), 20 function(s)\n\
         \n\
         Reports now  2 shown (2 exact, 0 heuristic), 2 heuristic withheld, 4 heuristic rule(s) \
         off\n\
         \n\
         pyproject.toml: nothing to add\n"
    );
    let (_, _, note) = p.heki(&["check"]);
    assert!(
        note.contains("4 heuristic rule(s) are off and 2 heuristic report(s) are not shown"),
        "{note}"
    );
    // The same counts as `heki check` for every selection, also with `lint.heuristic`.
    for args in [
        &[][..],
        &["--heuristic"],
        &["--select", "EFF101"],
        &["--ignore", "OPT101"],
        &["--extend-select", "OPT110"],
    ] {
        let (code, report, _) = recommend_json(&p, args);
        assert_eq!(code, 0);
        let now = &report["reports_now"];
        let shown = checked(&p, args);
        let exact = shown.iter().filter(|d| d["certainty"] == "exact").count();
        assert_eq!(
            (&now["shown"], &now["exact"], &now["heuristic"]),
            (
                &json!(shown.len()),
                &json!(exact),
                &json!(shown.len() - exact)
            ),
            "{args:?}"
        );
        assert_follows_the_schema(&report);
    }
    let (_, heuristic, _) = recommend_json(&p, &["--heuristic"]);
    assert_eq!(
        heuristic["reports_now"],
        json!({"shown": 5, "exact": 2, "heuristic": 3, "withheld_heuristic": 0,
               "heuristic_rules_off": 0, "baseline": null})
    );
    let (_, named, _) = recommend_json(&p, &["--extend-select", "OPT110"]);
    assert_eq!(
        (
            &named["reports_now"]["withheld_heuristic"],
            &named["reports_now"]["heuristic"]
        ),
        (&json!(0), &json!(2))
    );
    // An inline suppression counts as in `heki check`.
    p.write(
        "app/api.py",
        &API.replace(
            "    return load_user(user_id)",
            "    return load_user(user_id)  # heki: ignore[EFF101]",
        ),
    );
    let (_, suppressed, _) = recommend_json(&p, &[]);
    assert_eq!(suppressed["reports_now"]["shown"], json!(1));
}

#[test]
fn the_json_object_names_the_project_and_its_run() {
    let p = shop("recommend_json");
    let (code, report, _) = recommend_json(&p, &[]);
    assert_eq!(code, 0);
    assert_eq!(
        (&report["kind"], &report["version"], &report["heki"]),
        (
            &json!("heki-recommend"),
            &json!(1),
            &json!(env!("CARGO_PKG_VERSION"))
        )
    );
    assert_eq!(
        (
            &report["root"],
            &report["config_file"],
            &report["table_prefix"]
        ),
        (&json!("."), &json!("pyproject.toml"), &json!("tool.heki"))
    );
    assert_eq!(
        report["analyzed"],
        json!({"files": 8, "functions": 20, "errors": [], "paths": ["."],
               "left_out": {"python_files": 0, "files": [], "truncated": false}})
    );
    assert_eq!(report["runs"], json!([{"label": "discovery"}]));
    assert_eq!(
        (&report["notes"], &report["config"], &report["config_toml"]),
        (&json!([]), &json!({}), &json!(""))
    );
    let (_, text, _) = p.heki(&["recommend"]);
    assert_eq!(report["content"], json!(text));
}

#[test]
fn paths_narrower_than_the_root_leave_files_out_with_a_note() {
    let p = shop("recommend_left_out");
    let (code, out, _) = p.heki(&["recommend", "app"]);
    assert_eq!(code, 0);
    assert!(
        out.contains(
            "\nNotes\n  - 4 Python file(s) below the project root were not analyzed \
             (tests/test_a.py, tests/test_b.py, tests/test_c.py, ...): heki takes the analyzed \
             files as the whole project, so what it says about callers, readers, and writers \
             holds for those files only.\n"
        ),
        "{out}"
    );
    let (_, report, _) = recommend_json(&p, &["app"]);
    assert_eq!(
        report["analyzed"]["left_out"],
        json!({"python_files": 4, "truncated": false, "files": [
            "tests/test_a.py", "tests/test_b.py", "tests/test_c.py", "tests/test_d.py"]})
    );
    assert_eq!(report["analyzed"]["paths"], json!(["app"]));
    // Files `exclude` leaves out are not part of the project either way.
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki]\nextend-exclude = [\"tests\"]\n",
    );
    let (_, excluded, _) = recommend_json(&p, &["app"]);
    assert_eq!(excluded["analyzed"]["left_out"]["python_files"], json!(0));
    assert_follows_the_schema(&report);
}

#[test]
fn a_baseline_splits_the_reports_into_allowed_and_regressions() {
    let p = shop("recommend_baseline");
    let (code, _, err) = p.heki(&["check", "--baseline", "b.toml", "--update-baseline"]);
    assert_eq!(code, 0, "{err}");
    p.write(
        "app/api.py",
        &format!("{API}\nasync def get_again(user_id: int) -> dict[str, str]:\n    return load_user(user_id)\n"),
    );
    let (_, report, _) = recommend_json(&p, &["--baseline", "b.toml"]);
    let regressions = checked(&p, &["--baseline", "b.toml"]).len();
    // The new function's report is a regression; the two recorded ones are allowed.
    assert_eq!(
        report["reports_now"]["baseline"],
        json!({"allowed": 2, "regressions": regressions})
    );
    assert_eq!(regressions, 1);
    assert_follows_the_schema(&report);
    // The baseline of the configuration, relative to the project root.
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki]\nbaseline = \"b.toml\"\n",
    );
    let (_, out, _) = p.heki(&["recommend"]);
    assert!(
        out.contains("\n  with the baseline: 2 allowed, 1 regression(s)\n"),
        "{out}"
    );
}

#[test]
fn a_file_that_does_not_parse_exits_2_after_the_report() {
    let p = shop("recommend_errors");
    p.write("app/broken.py", "def f(:\n");
    let (code, out, err) = p.heki(&["recommend"]);
    assert_eq!(code, 2);
    assert!(
        out.starts_with("heki recommend (experimental) on .: 8 file(s)"),
        "{out}"
    );
    assert!(
        err.starts_with("error: app/broken.py:1:7: syntax error"),
        "{err}"
    );
    let (code, report, _) = recommend_json(&p, &[]);
    assert_eq!(code, 2);
    let errors = report["analyzed"]["errors"].as_array().unwrap();
    assert!(
        errors.len() == 1
            && errors[0]
                .as_str()
                .unwrap()
                .starts_with("app/broken.py:1:7: "),
        "{errors:?}"
    );
    assert_follows_the_schema(&report);
    // A file that does not parse was analyzed as far as heki can: it is not left out.
    let (_, narrower, _) = recommend_json(&p, &["app"]);
    assert_eq!(
        narrower["analyzed"]["left_out"]["files"],
        json!([
            "tests/test_a.py",
            "tests/test_b.py",
            "tests/test_c.py",
            "tests/test_d.py"
        ])
    );
    // A configuration error is an error of the command.
    p.write("pyproject.toml", "[tool.heki]\nnope = 1\n");
    let (code, out, err) = p.heki(&["recommend"]);
    assert_eq!((code, out.as_str()), (2, ""));
    assert!(err.starts_with("heki error: "), "{err}");
}

#[test]
fn the_configuration_file_decides_where_the_keys_go() {
    let p = shop("recommend_config_file");
    std::fs::remove_file(p.dir.join("pyproject.toml")).unwrap();
    p.write("heki.toml", "[lint]\nextend-select = [\"EFF105\"]\n");
    let (_, report, _) = recommend_json(&p, &[]);
    assert_eq!(
        (&report["config_file"], &report["table_prefix"]),
        (&json!("heki.toml"), &json!(""))
    );
    let (_, out, _) = p.heki(&["recommend"]);
    assert!(out.ends_with("\nheki.toml: nothing to add\n"), "{out}");
    p.write("other.toml", "[lint]\n");
    let (_, other, _) = recommend_json(&p, &["--config", "other.toml"]);
    assert_eq!(
        (&other["config_file"], &other["table_prefix"]),
        (&json!("other.toml"), &json!(""))
    );
    // Without a configuration file, the keys would go into a new `pyproject.toml`.
    std::fs::remove_file(p.dir.join("heki.toml")).unwrap();
    let (_, none, _) = recommend_json(&p, &[]);
    assert_eq!(
        (&none["config_file"], &none["table_prefix"]),
        (&Value::Null, &json!("tool.heki"))
    );
    assert!(
        none["content"]
            .as_str()
            .unwrap()
            .ends_with("\npyproject.toml: nothing to add\n"),
        "{none}"
    );
    assert_follows_the_schema(&none);
}

/// Every file of the project with its text.
fn tree(p: &Project) -> BTreeMap<String, String> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut BTreeMap<String, String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                out.insert(rel, std::fs::read_to_string(&path).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(&p.dir, &p.dir, &mut out);
    out
}

#[test]
fn it_writes_nothing_and_prints_the_same_report_on_every_run() {
    let p = shop("recommend_determinism");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"MUT103\"]\n",
    );
    let before = tree(&p);
    let (code, first, err) = p.heki(&["recommend", "--output-format", "json"]);
    assert_eq!(code, 0);
    // `MUT103` compares with a snapshot, as in `heki check`.
    assert!(
        err.starts_with("warning: `MUT103` is enabled but ")
            && err.contains("run `heki snapshot --write` first"),
        "{err}"
    );
    let (_, second, _) = p.heki(&["recommend", "--output-format", "json"]);
    let one_thread = Command::new(env!("CARGO_BIN_EXE_heki"))
        .args(["recommend", "--output-format", "json"])
        .env("RAYON_NUM_THREADS", "1")
        .current_dir(&p.dir)
        .output()
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(first, String::from_utf8(one_thread.stdout).unwrap());
    assert_eq!(tree(&p), before);
}

#[test]
fn the_command_says_it_is_experimental() {
    let (code, out, _) = Project::new("recommend_help").heki(&["recommend", "--help"]);
    assert_eq!(code, 0);
    assert!(
        out.starts_with("Experimental: advice on the configuration"),
        "{out}"
    );
}

#[test]
fn reports_sharing_a_root_cause_count_once_as_heki_check_prints_them() {
    let p = shop("recommend_folded");
    let handlers: String = (0..12)
        .map(|i| format!("async def handler_{i}(user_id: int) -> dict[str, str]:\n    return load_user(user_id)\n\n"))
        .collect();
    p.write(
        "app/many.py",
        &format!("from app.users import load_user\n\n{handlers}"),
    );
    let shown = checked(&p, &[]);
    let eff101 = shown.iter().filter(|d| d["rule_code"] == "EFF101").count();
    assert_eq!(
        eff101, 1,
        "the 13 call sites fold into one report: {shown:#?}"
    );
    let (_, report, _) = recommend_json(&p, &[]);
    assert_eq!(report["reports_now"]["shown"], json!(shown.len()));
    // With a baseline, the regressions fold as `heki check` prints them.
    p.write("app/many.py", "");
    let (code, _, err) = p.heki(&["check", "--baseline", "b.toml", "--update-baseline"]);
    assert_eq!(code, 0, "{err}");
    p.write(
        "app/many.py",
        &format!("from app.users import load_user\n\n{handlers}"),
    );
    let (_, report, _) = recommend_json(&p, &["--baseline", "b.toml"]);
    let regressions = checked(&p, &["--baseline", "b.toml"]);
    assert_eq!(
        report["reports_now"]["baseline"],
        json!({"allowed": 2, "regressions": regressions.len()})
    );
    assert_eq!(regressions.len(), 1, "{regressions:#?}");
}

#[test]
fn the_count_of_files_left_out_stops_at_its_caps() {
    let p = shop("recommend_left_out_caps");
    for i in 0..55 {
        p.write(&format!("tests/test_{i:02}.py"), "");
    }
    let (_, report, _) = recommend_json(&p, &["app"]);
    let left_out = &report["analyzed"]["left_out"];
    assert_eq!(
        (&left_out["python_files"], &left_out["truncated"]),
        (&json!(50), &json!(true))
    );
    assert_eq!(left_out["files"][49], json!("tests/test_49.py"));
    assert!(
        report["notes"][0].as_str().unwrap().starts_with(
            "at least 50 Python file(s) below the project root were not analyzed \
                (tests/test_00.py, tests/test_01.py, tests/test_02.py, ...): "
        ),
        "{report:#}"
    );
    assert_follows_the_schema(&report);
    // The count looks at the first 10,000 entries below the root only.
    let q = shop("recommend_left_out_entries");
    q.write("aa/one.py", "");
    for i in 0..10_000 {
        q.write(&format!("ab/f{i:05}.txt"), "");
    }
    let (_, report, _) = recommend_json(&q, &["app"]);
    assert_eq!(
        report["analyzed"]["left_out"],
        json!({"python_files": 1, "files": ["aa/one.py"], "truncated": true})
    );
}
