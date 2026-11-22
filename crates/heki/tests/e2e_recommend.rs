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
    // The sections of what keeps reports heuristic and what heki cannot see have tests of their
    // own.
    let mut rest = out.clone();
    for header in [
        "\nWhat keeps reports heuristic\n",
        "\nWhat heki cannot see (no report either way)\n",
    ] {
        let (before, section) = rest.split_once(header).unwrap_or_else(|| panic!("{out}"));
        let after = section.split_once("\n\n").unwrap().1;
        rest = format!("{before}\n{after}");
    }
    assert_eq!(
        rest,
        "heki recommend (experimental) on .: 8 file(s), 20 function(s)\n\
         \n\
         Reports now  2 shown (2 exact, 0 heuristic), 2 heuristic withheld, 5 heuristic rule(s) \
         off\n\
         \n\
         Rules worth enabling here\n\
         \x20 rule    reports  exact  consequence  right about the code (published)  real harm  \
         block\n\
         \x20 OPT110        2      0  slowdown     78% (65-87%), n=50, 2026-11-21    6%         \
         no: slower code, not a bug\n\
         \x20 EFF111        1      -  slowdown     not measured                      -          \
         no: slower code, not a bug\n\
         \x20 No reports here: EFF102, EFF105, EFF106, EFF107, EFF109, EFF110, EFF113, MUT101, \
         MUT106, MUT107, MUT108, MUT111, MUT112, MUT113, MUT115, MUT118, OPT112\n\
         \x20 MUT103: it compares with a saved summary snapshot: run `heki snapshot --write`, then \
         select it.\n\
         \n\
         pyproject.toml: nothing to add\n"
    );
    let (_, _, note) = p.heki(&["check"]);
    assert!(
        note.contains("5 heuristic rule(s) are off and 2 heuristic report(s) are not shown"),
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

// Unix only: the directory is made unreadable with permission bits; on Windows that takes an
// access control list, which std cannot set.
#[cfg(unix)]
#[test]
fn a_directory_the_count_cannot_read_leaves_it_a_lower_bound() {
    use std::os::unix::fs::PermissionsExt;
    let p = shop("recommend_left_out_unreadable");
    p.write("locked/l.py", "");
    let locked = p.dir.join("locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let readable = std::fs::read_dir(&locked).is_ok(); // running as root
    let (code, report, _) = recommend_json(&p, &["app"]);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(code, 0);
    let left_out = &report["analyzed"]["left_out"];
    assert_eq!(left_out["truncated"], json!(!readable));
    assert_eq!(left_out["files"][0], json!("tests/test_a.py"));
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

/// Reports of opt-in rules: `MUT106` and `EFF107` (bugs, not measured), `MUT107` (a bug whose
/// measurement shows real harm below 10%), and the experimental `EFF109`.
const OPT_IN: &str = "\
import asyncio
import threading

lock = threading.Lock()

def total_and_count(rows):
    items = (r for r in rows)
    total = sum(items)
    count = len(list(items))
    return total, count

async def work():
    await asyncio.sleep(1)

async def start():
    task = asyncio.create_task(work())
    await asyncio.sleep(0)

class Point:
    def __init__(self, x):
        self.x = x

    def __hash__(self):
        return hash(self.x)

    def move(self, dx):
        self.x += dx

def twice():
    with lock:
        with lock:
            pass
";

fn shop_with_opt_in(name: &str) -> Project {
    let p = shop(name);
    p.write("app/opt_in.py", OPT_IN);
    p
}

/// The advice on the rule `code`.
fn rule<'r>(report: &'r Value, code: &str) -> &'r Value {
    report["rules"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["code"] == code)
        .unwrap_or_else(|| panic!("no advice on {code}: {report:#}"))
}

/// Every rule code but `MUT103`, comma-separated: naming them all withholds nothing.
fn every_rule() -> String {
    let (_, out, _) =
        Project::new("recommend_rules").heki(&["rule", "--all", "--output-format", "json"]);
    let rules: Vec<Value> = serde_json::from_str(&out).unwrap();
    rules
        .iter()
        .map(|r| r["code"].as_str().unwrap())
        .filter(|code| *code != "MUT103")
        .collect::<Vec<_>>()
        .join(",")
}

#[test]
fn rules_worth_enabling_follow_the_policy() {
    let p = shop_with_opt_in("recommend_rules_policy");
    let (code, out, _) = p.heki(&["recommend"]);
    assert_eq!(code, 0);
    let section = out
        .split("\nRules worth enabling here\n")
        .nth(1)
        .and_then(|rest| rest.split("\n\n").next())
        .unwrap_or_else(|| panic!("{out}"));
    assert_eq!(
        section,
        "  rule    reports  exact  consequence  right about the code (published)  real harm  block\n\
        \x20 EFF107        1      -  bug          not measured                      -          yes\n\
        \x20 MUT106        1      -  bug          not measured                      -          yes\n\
        \x20 OPT110        2      0  slowdown     78% (65-87%), n=50, 2026-11-21    6%         no: slower code, not a bug\n\
        \x20 EFF111        1      -  slowdown     not measured                      -          no: slower code, not a bug\n\
        \x20 MUT107        1      -  bug          78% (65-87%), n=50, 2026-11-21    4%         no: real harm below 10%\n\
        \x20 Optional (experimental, never in the block)\n\
        \x20 EFF109        1      -  bug          not measured                      -          no: experimental\n\
        \x20 No reports here: EFF102, EFF105, EFF106, EFF110, EFF113, MUT101, MUT108, MUT111, MUT112, MUT113, \
        MUT115, MUT118, OPT112\n\
        \x20 MUT103: it compares with a saved summary snapshot: run `heki snapshot --write`, then select it."
    );
    assert!(
        out.ends_with(
            "\npyproject.toml: keys to add or change\n  [tool.heki.lint]\n  extend-select = \
             [\"EFF107\", \"MUT106\"]  # adds EFF107, MUT106\n"
        ),
        "{out}"
    );
    let (_, report, _) = recommend_json(&p, &[]);
    assert_eq!(
        rule(&report, "EFF107"),
        &json!({"code": "EFF107", "slug": "unretained-async-task", "certainty": "heuristic",
                "activation": "opt-in", "experimental": false, "consequence": "bug",
                "enabled_now": false, "reports": 1, "exact": 0, "heuristic": 1, "baseline": null,
                "measurement": null, "in_block": true, "reason": "a bug, reported here",
                "caveats": ["not measured"]})
    );
    // The heuristic reports an exact rule withholds, with its measurement quoted.
    let opt110 = rule(&report, "OPT110");
    assert_eq!(
        (
            &opt110["enabled_now"],
            &opt110["reports"],
            &opt110["heuristic"]
        ),
        (&json!(true), &json!(2), &json!(2))
    );
    assert_eq!(
        opt110["measurement"],
        json!({"rule": "OPT110", "round": 3, "date": "2026-11-21", "sample": 50, "confirmed": 0,
               "plausible": 3, "benign": 36, "false_claims": 11, "right_percent": 78,
               "interval_percent": [65, 87], "real_harm_percent": 6})
    );
    assert_eq!(rule(&report, "MUT103")["reports"], Value::Null);
    assert_eq!(
        report["config"],
        json!({"lint": {"extend-select": ["EFF107", "MUT106"]}})
    );
    // Rules the current selection runs in full, and contract rules, are not listed.
    for code in ["EFF101", "OPT101", "MUT201", "EFF201"] {
        assert!(
            report["rules"]
                .as_array()
                .unwrap()
                .iter()
                .all(|r| r["code"] != code),
            "{code}"
        );
    }
    assert_follows_the_schema(&report);
    // A rule the configuration ignores stays out.
    let (_, ignoring, _) = recommend_json(&p, &["--ignore", "MUT106"]);
    let mut106 = rule(&ignoring, "MUT106");
    assert_eq!(
        (&mut106["in_block"], &mut106["reason"]),
        (&json!(false), &json!("the configuration ignores it"))
    );
    assert_eq!(
        ignoring["config"],
        json!({"lint": {"extend-select": ["EFF107"]}})
    );
    // A selected `MUT103` is not advice.
    let (_, selected, _) = recommend_json(&p, &["--extend-select", "MUT103"]);
    assert!(
        selected["rules"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["code"] != "MUT103")
    );
}

#[test]
fn the_block_enables_exactly_the_reports_it_promises() {
    for (file, existing) in [
        ("pyproject.toml", "[project]\nname = \"t\"\n"),
        ("heki.toml", "[lint]\nextend-select = [\"EFF105\"]\n"),
    ] {
        let p = shop_with_opt_in("recommend_round_trip");
        if file == "heki.toml" {
            std::fs::remove_file(p.dir.join("pyproject.toml")).unwrap();
        }
        p.write(file, existing);
        let (_, report, _) = recommend_json(&p, &[]);
        let block = report["config_toml"].as_str().unwrap();
        // The user adds the keys: the array is written whole, so it replaces the line.
        let merged = if existing.contains("extend-select") {
            let line = block
                .lines()
                .find(|l| l.starts_with("extend-select"))
                .unwrap();
            existing.replace("extend-select = [\"EFF105\"]", line)
        } else {
            format!("{existing}\n{block}")
        };
        p.write(file, &merged);
        let after = checked(&p, &[]);
        let promised: Vec<&Value> = report["rules"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["in_block"] == true)
            .collect();
        assert!(!promised.is_empty(), "{file}");
        for r in &promised {
            let code = r["code"].as_str().unwrap();
            let found = after.iter().filter(|d| d["rule_code"] == code).count();
            assert_eq!(json!(found), r["reports"], "{file}: {code}");
        }
        let added: u64 = promised
            .iter()
            .map(|r| r["reports"].as_u64().unwrap())
            .sum();
        assert_eq!(
            json!(after.len() as u64),
            json!(report["reports_now"]["shown"].as_u64().unwrap() + added),
            "{file}: {merged}"
        );
    }
}

#[test]
fn a_rules_reports_do_not_depend_on_the_other_rules_selected() {
    let p = shop_with_opt_in("recommend_independence");
    let all = every_rule();
    let everything = checked(&p, &["--select", &all]);
    let key = |d: &Value| {
        (
            d["path"].clone(),
            d["line"].clone(),
            d["column"].clone(),
            d["message"].clone(),
            d["certainty"].clone(),
        )
    };
    let codes: std::collections::BTreeSet<&str> = everything
        .iter()
        .map(|d| d["rule_code"].as_str().unwrap())
        .collect();
    assert!(codes.len() >= 6, "{codes:?}");
    for code in codes {
        let alone: Vec<_> = checked(&p, &["--select", code]).iter().map(key).collect();
        let among: Vec<_> = everything
            .iter()
            .filter(|d| d["rule_code"] == code)
            .map(key)
            .collect();
        assert_eq!(alone, among, "{code}");
    }
}

#[test]
fn a_baseline_splits_each_rules_reports() {
    let p = shop_with_opt_in("recommend_rules_baseline");
    p.write("app/opt_in.py", "");
    let (code, _, err) = p.heki(&[
        "check",
        "--select",
        &every_rule(),
        "--baseline",
        "b.toml",
        "--update-baseline",
    ]);
    assert_eq!(code, 0, "{err}");
    p.write("app/opt_in.py", OPT_IN);
    p.write(
        "app/lists.py",
        &format!("{LISTS}\ndef more_names(names, source):\n    known = list(source)\n    return [n for n in names if n in known]\n"),
    );
    let (_, report, _) = recommend_json(&p, &["--baseline", "b.toml"]);
    assert_eq!(
        rule(&report, "MUT106")["baseline"],
        json!({"allowed": 0, "regressions": 1})
    );
    // The baseline holds the two heuristic `OPT110` reports; the third is new.
    assert_eq!(
        rule(&report, "OPT110")["baseline"],
        json!({"allowed": 2, "regressions": 1})
    );
    let (_, out, _) = p.heki(&["recommend", "--baseline", "b.toml"]);
    assert!(
        out.contains("  rule    reports  exact  regressions  consequence  ")
            && out.contains("\n  MUT106        1      -            1  bug  "),
        "{out}"
    );
    assert_follows_the_schema(&report);
}

#[test]
fn a_rarely_harmful_bug_is_listed_without_entering_the_block() {
    let p = shop("recommend_minor_bug");
    p.write(
        "app/fill.py",
        "from collections.abc import Sequence\n\n\ndef push(items, x):\n    items.append(x)\n\n\n\
         def push_seq(items: Sequence[int], x: int) -> None:\n    push(items, x)\n",
    );
    // `MUT110` runs by default; a selection without it leaves its report off.
    let (_, report, _) = recommend_json(&p, &["--select", "EFF101"]);
    let mut110 = rule(&report, "MUT110");
    assert_eq!(
        (&mut110["consequence"], &mut110["reports"], &mut110["exact"]),
        (&json!("minor-bug"), &json!(1), &json!(0))
    );
    assert_eq!(
        (&mut110["in_block"], &mut110["reason"]),
        (&json!(false), &json!("a bug that is rarely harmful"))
    );
    let (_, out, _) = p.heki(&["recommend", "--select", "EFF101"]);
    assert!(
        out.contains(
            "\n  MUT110        1      0  minor bug    100% (51-100%), n=4, 2026-11-21   0%         \
             no: a bug that is rarely harmful\n"
        ),
        "{out}"
    );
}

#[test]
fn paths_are_written_as_heki_check_writes_them() {
    let p = shop("recommend_below_root");
    p.write("app/sub/__init__.py", "");
    // Run from below the project root: the root and the files outside the working directory
    // are written as they are, the files below it relative to it.
    let out = Command::new(env!("CARGO_BIN_EXE_heki"))
        .args(["recommend", "--output-format", "json", "sub"])
        .current_dir(p.dir.join("app"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    let root = report["root"].as_str().unwrap();
    let name = p.dir.file_name().unwrap().to_str().unwrap();
    assert!(root.ends_with(name) && root != name, "{root}");
    assert_eq!(
        report["config_file"],
        json!(format!("{root}/pyproject.toml"))
    );
    assert_eq!(report["analyzed"]["paths"], json!(["sub"]));
    let left_out: Vec<&str> = report["analyzed"]["left_out"]["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap())
        .collect();
    assert!(
        left_out.contains(&"api.py")
            && left_out.contains(&format!("{root}/tests/test_a.py").as_str()),
        "{left_out:?}"
    );
}

/// Heuristic reports of exact claims, one per closer: `EFF111` (a branch skips the wait: code),
/// `EFF113` (a loop over a library result the catalog does not size: heki's catalog), `OPT113`
/// (items of unknown type: two annotations), and `OPT110` (a loop the code counts: a condition of
/// the exact tier, nothing to apply). `handle_quietly`'s report is suppressed.
const HEURISTIC_CLAIMS: [(&str, &str); 4] = [
    (
        "app/waits.py",
        "import time\n\n\ndef pause(flag):\n    if flag:\n        return None\n    time.sleep(1)\n\n\n\
         async def handle(flag):\n    pause(flag)\n\n\nasync def handle_quietly(flag):\n    \
         pause(flag)  # heki: ignore[EFF111]\n",
    ),
    (
        "app/queries.py",
        "import json\nimport sqlite3\n\nSQL = \"SELECT * FROM t WHERE id = ?\"\n\n\n\
         def process(db: sqlite3.Cursor, text: str):\n    for x in json.loads(text):\n        \
         db.execute(SQL, (x,))\n",
    ),
    (
        "app/joins.py",
        "from dataclasses import dataclass\n\n\n@dataclass\nclass Left:\n    key: int\n    \
         other: int = 0\n\n\n@dataclass\nclass Right:\n    key: int\n    payload: int = 0\n\n\n\
         def join(lefts, rights):\n    out = []\n    for a in lefts:\n        for b in rights:\n\
         \x20           if a.key == b.key:\n                out.append((a.other, b.payload))\n    \
         return out\n",
    ),
    (
        "app/counts.py",
        "def count(probes):\n    allowed = list(range(0, 50, 3))\n    hits = []\n    for p in \
         range(40):\n        if p in allowed:\n            hits.append(p)\n    return hits\n",
    ),
];

fn heuristic_claims(name: &str) -> Project {
    let p = Project::new(name);
    p.write("app/__init__.py", "");
    for (path, text) in HEURISTIC_CLAIMS {
        p.write(path, text);
    }
    p
}

/// The rows of "What keeps reports heuristic" as `(name, cause, closer, reports, only open)`.
fn cause_rows(report: &Value) -> Vec<(String, String, String, u64, u64)> {
    report["open_premises"]["by_cause"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["name"].as_str().unwrap().to_owned(),
                r["cause"].as_str().unwrap().to_owned(),
                r["closer"].as_str().unwrap().to_owned(),
                r["reports"].as_u64().unwrap(),
                r["only_open"].as_u64().unwrap(),
            )
        })
        .collect()
}

#[test]
fn heuristic_reports_are_counted_by_the_premise_they_leave_open_and_why() {
    let p = heuristic_claims("recommend_open_premises");
    let (code, report, _) = recommend_json(&p, &[]);
    assert_eq!(code, 0);
    let section = &report["open_premises"];
    assert_eq!(section["reports"], json!(4), "{section:#}");
    assert_eq!(
        section["by_rule"],
        json!([{"rule": "EFF111", "reports": 1}, {"rule": "EFF113", "reports": 1},
               {"rule": "OPT110", "reports": 1}, {"rule": "OPT113", "reports": 1}])
    );
    let row = |name: &str, cause: &str, closer: &str, reports: u64, only: u64| {
        (
            name.to_owned(),
            cause.to_owned(),
            closer.to_owned(),
            reports,
            only,
        )
    };
    // The causes left open alone first; the two that one report leaves open together after.
    assert_eq!(
        cause_rows(&report),
        vec![
            row("Every return", "not-made-on-every-return", "code", 1, 1),
            row("Origin", "library-call-without-items", "heki-catalog", 1, 1),
            row("Trip count open", "trip-count-fixed", "none", 1, 1),
            row("Equality of keys", "item-type-unknown", "annotation", 1, 0),
            row(
                "Idle on a mismatch",
                "iterable-type-unknown",
                "annotation",
                1,
                0
            ),
        ]
    );
    let by_cause = section["by_cause"].as_array().unwrap();
    assert_eq!(
        (&by_cause[0]["kind"], &by_cause[2]["kind"]),
        (&json!("premise"), &json!("condition"))
    );
    // A heuristic side's reports count under the claim of its exact rule.
    assert_eq!(
        by_cause[0]["rules"],
        json!([{"rule": "EFF101", "reported_as": "EFF111", "reports": 1, "only_open": 1}])
    );
    assert_eq!(
        by_cause[0]["examples"],
        json!([{"path": "app/waits.py", "line": 11, "column": 5, "rule": "EFF111",
                "at": {"path": "app/waits.py", "line": 7, "column": 5}, "detail": null}])
    );
    assert_eq!(
        by_cause[2]["examples"][0]["detail"],
        json!("the loop runs at most 40 times")
    );
    assert_eq!(
        section["combinations"],
        json!([{"reports": 1, "open": [
            {"kind": "premise", "name": "Equality of keys", "cause": "item-type-unknown"},
            {"kind": "premise", "name": "Idle on a mismatch", "cause": "iterable-type-unknown"}]}])
    );
    assert_eq!(
        (&section["not_recorded"], &section["truncated"]),
        (&json!([]), &json!(false))
    );
    // Nothing of this section enters the block.
    assert_eq!(report["config"], json!({}));
    assert_follows_the_schema(&report);
    let (_, out, _) = p.heki(&["recommend"]);
    let text = out
        .split("\nWhat keeps reports heuristic\n")
        .nth(1)
        .and_then(|rest| rest.split("\n\n").next())
        .unwrap_or_else(|| panic!("{out}"));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[..3],
        [
            "  4 heuristic report(s) of exact claims: EFF111 1, EFF113 1, OPT110 1, OPT113 1",
            "  premise: cause                                 reports  only open  closed by",
            "  Every return: not-made-on-every-return               1          1  code",
        ],
        "{text}"
    );
    assert_eq!(lines[4], "    e.g. app/waits.py:11 EFF111", "{text}");
    for line in [
        "  Origin: library-call-without-items                   1          1  heki's catalog",
        "  Trip count open (condition): trip-count-fixed        1          1  none",
        "  Idle on a mismatch: iterable-type-unknown            1          0  annotation",
        "  Nothing here enters the block: an annotation or a change of the code is an edit of \
         the program.",
        "  heki's catalog takes no configuration for the causes it closes: report a case at \
         https://github.com/koxudaxi/heki/issues.",
    ] {
        assert!(lines.contains(&line), "{line}\n{text}");
    }
}

#[test]
fn closing_the_cause_in_the_code_makes_the_report_exact() {
    let p = heuristic_claims("recommend_open_premises_closed");
    let closed = [
        // The branch that skips the wait.
        ("app/waits.py", "    if flag:\n        return None\n", ""),
        // The two annotations.
        (
            "app/joins.py",
            "def join(lefts, rights):",
            "def join(lefts: list[Left], rights: list[Right]):",
        ),
        // A loop the data sizes.
        ("app/counts.py", "range(40)", "range(probes)"),
    ];
    for (path, open, close) in closed {
        let (_, text) = HEURISTIC_CLAIMS.iter().find(|(p, _)| *p == path).unwrap();
        p.write(path, &text.replace(open, close));
    }
    let shown = checked(&p, &["--select", "EFF101,OPT110,OPT113"]);
    let exact: Vec<(&str, &str)> = shown
        .iter()
        .filter(|d| d["certainty"] == "exact")
        .map(|d| {
            (
                d["rule_code"].as_str().unwrap(),
                d["path"].as_str().unwrap(),
            )
        })
        .collect();
    for (rule, path) in [
        ("EFF101", "app/waits.py"),
        ("OPT113", "app/joins.py"),
        ("OPT110", "app/counts.py"),
    ] {
        assert!(exact.contains(&(rule, path)), "{rule} {path}: {shown:#?}");
    }
    // What stays heuristic is the cause only heki's catalog closes.
    let (_, report, _) = recommend_json(&p, &[]);
    assert_eq!(
        cause_rows(&report),
        vec![(
            "Origin".to_owned(),
            "library-call-without-items".to_owned(),
            "heki-catalog".to_owned(),
            1,
            1
        )]
    );
    // Without heuristic reports of exact claims, the section says so.
    p.write("app/queries.py", "");
    let (_, out, _) = p.heki(&["recommend"]);
    assert!(
        out.contains(
            "\nWhat keeps reports heuristic\n  No heuristic reports of exact claims here.\n"
        ),
        "{out}"
    );
}

/// Calls heki is blind to: library names the catalog has no entry for (`stripe`, a package it
/// does not cover; `requests.utils.quote`, a name of a package it covers), awaited or not, on an
/// event loop or in a loop over a parameter; a call on an unannotated parameter; and a function
/// with an A1 hazard.
const BLIND: &str = "\
import requests
import stripe


async def pay(amount):
    return stripe.Charge.create(amount=amount)


async def refund(amount):
    return await stripe.Refund.create_async(amount=amount)


def capture_all(charges: list[str]):
    for charge in charges:
        stripe.Charge.capture(charge)


async def link(path: str):
    return requests.utils.quote(path)


def sync_job(client):
    return client.get(\"/jobs\")


async def poll(client):
    return sync_job(client)


def introspect():
    return locals()
";

/// `(symbol, calls, awaited, on event loop, in loops, if it waits, if it sends a query)` of each
/// name of `list`.
fn symbol_rows(list: &Value) -> Vec<(String, Value, Value)> {
    list.as_array()
        .unwrap()
        .iter()
        .map(|s| {
            (
                s["symbol"].as_str().unwrap().to_owned(),
                json!([s["calls"], s["awaited"], s["on_event_loop"], s["in_loops"]]),
                json!([s["if_waits"], s["if_queries"]]),
            )
        })
        .collect()
}

#[test]
fn library_names_without_a_catalog_entry_are_listed_with_what_heki_would_report() {
    let p = Project::new("recommend_uncatalogued");
    p.write("app/__init__.py", "").write("app/pay.py", BLIND);
    let (code, report, _) = recommend_json(&p, &[]);
    assert_eq!(code, 0);
    let blind = &report["blind_spots"];
    let packages = blind["uncatalogued"].as_array().unwrap();
    assert_eq!(packages.len(), 1, "{blind:#}");
    assert_eq!(
        (&packages[0]["package"], &packages[0]["calls"]),
        (&json!("stripe"), &json!(3))
    );
    let tried = |eff101: u64, eff103: u64| json!([{"EFF101": eff101, "EFF111": 0}, {"EFF103": eff103, "EFF113": 0}]);
    // On the event loop it would wait there; in a loop over a parameter it would send a query
    // per item; awaited, it holds nothing up.
    assert_eq!(
        symbol_rows(&packages[0]["symbols"]),
        vec![
            (
                "stripe.Charge.create".to_owned(),
                json!([1, 0, 1, 0]),
                tried(1, 0)
            ),
            (
                "stripe.Charge.capture".to_owned(),
                json!([1, 0, 0, 1]),
                tried(0, 1)
            ),
            (
                "stripe.Refund.create_async".to_owned(),
                json!([1, 1, 0, 0]),
                tried(0, 0)
            ),
        ]
    );
    assert_eq!(
        packages[0]["symbols"][0]["examples"],
        json!([{"path": "app/pay.py", "line": 6, "column": 12, "function": "app.pay.pay"}])
    );
    // A name of a package the catalog covers is listed when heki would report through it.
    assert_eq!(
        symbol_rows(&blind["catalog_may_miss"]),
        vec![(
            "requests.utils.quote".to_owned(),
            json!([1, 0, 1, 0]),
            tried(1, 0)
        )]
    );
    assert_eq!(
        (&blind["totals"]["tried"], &blind["totals"]["not_tried"]),
        (&json!(4), &json!(0))
    );
    assert_eq!(
        report["runs"],
        json!([{"label": "discovery"}, {"label": "hypothetical-catalog"}])
    );
    // No catalog entry enters the block, and no count of reports takes the trial's.
    assert_eq!(report["config"], json!({}));
    assert_eq!(report["reports_now"]["shown"], json!(0));
    assert_eq!(rule(&report, "EFF111")["reports"], json!(0));
    assert!(
        report["notes"][0]
            .as_str()
            .unwrap()
            .starts_with("\"if it waits\" and \"if it sends a query\" come from a trial run"),
        "{report:#}"
    );
    assert_follows_the_schema(&report);
    let (_, out, _) = p.heki(&["recommend"]);
    for line in [
        "  calls of packages heki's catalog does not cover: 3 call(s) of 3 name(s) in 1 \
         package(s)",
        "    name                        calls  async  loops  if it waits  if it sends a query",
        "    stripe.Charge.create            1      1      0  EFF101 1     -",
        "    stripe.Charge.capture           1      0      1  -            EFF103 1",
        "    stripe.Refund.create_async      1      0      0  -            -",
        "    requests.utils.quote      1      1      0  EFF101 1     -",
        "  1 function(s) with an A1 hazard, 0 of them too large to model: the Soundness lane \
         proves nothing in them (`heki assumptions` lists them)",
    ] {
        assert!(out.lines().any(|l| l == line), "{line}\n{out}");
    }
    assert!(
        out.contains("(if they do, report them at https://github.com/koxudaxi/heki/issues)"),
        "{out}"
    );
    // What the trial counts is what `heki check` prints with such an entry.
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.ecosystem]]\nsymbol = \"stripe.Charge.create\"\n\
         effects = [\"blocking_net\", \"db_query\"]\n",
    );
    let shown = checked(&p, &["--select", "EFF101,EFF111,EFF103,EFF113"]);
    let through: Vec<&Value> = shown
        .iter()
        .filter(|d| d["target"] == "stripe.Charge.create")
        .collect();
    assert_eq!(through.len(), 1, "{shown:#?}");
    assert_eq!(through[0]["rule_code"], "EFF101");
    // With an entry, the name is no longer one heki cannot see.
    let (_, entered, _) = recommend_json(&p, &[]);
    let names: Vec<String> = symbol_rows(&entered["blind_spots"]["uncatalogued"][0]["symbols"])
        .into_iter()
        .map(|(s, _, _)| s)
        .collect();
    assert_eq!(
        names,
        ["stripe.Charge.capture", "stripe.Refund.create_async"]
    );
}

#[test]
fn calls_heki_does_not_resolve_are_listed_with_why() {
    let p = Project::new("recommend_unresolved");
    p.write("app/__init__.py", "").write("app/pay.py", BLIND);
    let (_, report, _) = recommend_json(&p, &[]);
    let blind = &report["blind_spots"];
    assert_eq!(
        blind["unresolved"],
        json!([{"function": "app.pay.sync_job", "path": "app/pay.py",
                "reason": "unannotated-parameter", "parameter": "client", "names": ["get"],
                "calls": 1, "awaited": 0, "on_event_loop": 1, "in_loops": 0,
                "examples": [{"path": "app/pay.py", "line": 23, "column": 12,
                              "function": "app.pay.sync_job"}]}])
    );
    assert_eq!(
        (
            &blind["totals"]["unresolved_by_reason"],
            &blind["a1_hazards"]
        ),
        (
            &json!({"unannotated-parameter": 1}),
            &json!({"functions": 1, "unmodeled": 0})
        )
    );
    let (_, out, _) = p.heki(&["recommend"]);
    assert!(
        out.contains(
            "\n  calls heki does not resolve: 1 call(s) in 1 function(s)\n    function          \
             calls  async  loops  why\n    app.pay.sync_job      1      1      0  parameter \
             `client` has no annotation (get)\n"
        ),
        "{out}"
    );
}

/// A subdirectory application whose directories are imported as top-level packages.
fn subdirectory_app(name: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "backend/config/env.py",
        "import time\n\n\ndef load():\n    time.sleep(1)\n    return {}\n",
    )
    .write(
        "backend/main.py",
        "from config.env import load\nimport config.env\n\n\nasync def handler():\n    return \
         load()\n\n\nasync def other():\n    return config.env.load()\n",
    )
    // An import that names a project module now, and still does with the source root.
    .write("backend/config/settings.py", "from . import env\n");
    p
}

/// The paths of the `EFF101` reports `heki check` prints, sorted.
fn waits_reported(p: &Project) -> Vec<String> {
    let mut paths: Vec<String> = checked(p, &["--select", "EFF101"])
        .iter()
        .map(|d| d["path"].as_str().unwrap().to_owned())
        .collect();
    paths.sort();
    paths
}

/// `pyproject.toml` with the block `report` prints.
fn apply_block(p: &Project, report: &Value) {
    let block = report["config_toml"].as_str().unwrap();
    p.write(
        "pyproject.toml",
        &format!("[project]\nname = \"t\"\n\n{block}"),
    );
}

#[test]
fn a_directory_whose_files_imports_name_becomes_a_source_root_after_its_trial() {
    let p = subdirectory_app("recommend_source_root");
    let (_, report, _) = recommend_json(&p, &[]);
    let blind = &report["blind_spots"];
    assert_eq!(
        blind["source_roots"],
        json!([{"directory": "backend", "imports": 2, "in_block": true,
                "reason": "its imports resolve, and no import, contract, or baseline entry loses its module",
                "examples": [
                    {"path": "backend/main.py", "line": 1, "name": "config.env.load"},
                    {"path": "backend/main.py", "line": 2, "name": "config.env"}]}])
    );
    // The calls into those files are the project's, not a library's.
    assert_eq!(blind["uncatalogued"], json!([]));
    assert_eq!(blind["totals"]["below_source_roots"], json!(2));
    assert_eq!(
        report["runs"],
        json!([{"label": "discovery"}, {"label": "source-roots"}])
    );
    assert_eq!(report["config"], json!({"src": ["backend"]}));
    assert_follows_the_schema(&report);
    let (_, out, _) = p.heki(&["recommend"]);
    assert!(
        out.contains(
            "\n  source root `backend`: 2 import(s) name its files (config.env.load, config.env): \
             in the block\n"
        ) && out.ends_with(
            "\npyproject.toml: keys to add or change\n  [tool.heki]\n  src = [\"backend\"]\n"
        ),
        "{out}"
    );
    // With the block, the imports resolve: the waits below the handlers are reported.
    assert_eq!(waits_reported(&p), Vec::<String>::new());
    apply_block(&p, &report);
    assert_eq!(waits_reported(&p), ["backend/main.py", "backend/main.py"]);
    let (_, again, _) = recommend_json(&p, &[]);
    assert_eq!(again["blind_spots"]["source_roots"], json!([]));
}

#[test]
fn a_source_root_stays_out_when_a_name_the_configuration_uses_would_change() {
    let reason = |p: &Project, args: &[&str]| {
        let (_, report, _) = recommend_json(p, args);
        let roots = &report["blind_spots"]["source_roots"];
        assert_eq!(roots[0]["in_block"], json!(false), "{roots:#}");
        assert_eq!(report["config"], json!({}));
        assert_follows_the_schema(&report);
        roots[0]["reason"].as_str().unwrap().to_owned()
    };
    // A baseline entry naming a module below the directory: the reports' keys would name it
    // otherwise.
    let p = subdirectory_app("recommend_source_root_baseline");
    p.write(
        "b.toml",
        "version = 2\n\n[[entries]]\npath = \"backend/main.py\"\nqualname = \"handler\"\n\
         rule_code = \"EFF201\"\ntarget = \"backend.config.env.load->await\"\ncount = 1\n",
    );
    assert_eq!(
        reason(&p, &["--baseline", "b.toml"]),
        "the baseline names `backend.config.env.load`, which heki would write `config.env.load`"
    );
    // A contract whose rule names a definition there, by the matchers the rules use: a glob
    // in the module part, a class in it, or a pattern that would name a definition it does not
    // name now.
    let contract = |scope: &str| {
        p.write(
            "pyproject.toml",
            &format!(
                "[project]\nname = \"t\"\n\n[[tool.heki.effect-boundaries]]\nname = \"config\"\n\
                 scopes = [\"{scope}\"]\nforbid = [\"blocking\"]\n"
            ),
        );
        reason(&p, &[])
    };
    assert_eq!(
        contract("backen[d].config.env:*"),
        "`backen[d].config.env:*` (scopes) of the effect-boundaries contract `config` names \
         module `backend.config.env`, and would not name it as module `config.env`"
    );
    assert_eq!(
        contract("backend.config.env.load"),
        "`backend.config.env.load` (scopes) of the effect-boundaries contract `config` names \
         `backend.config.env.load`, and would not name it as `config.env.load`"
    );
    assert_eq!(
        contract("config.env:*"),
        "`config.env:*` (scopes) of the effect-boundaries contract `config` would name module \
         `backend.config.env` as module `config.env`, which it does not name now"
    );
    // Patterns that name the same definitions under both names let the directory in; the
    // definitions of files left out, read or not, are judged too.
    p.write(
        "pyproject.toml",
        r#"[project]
name = "t"

[[tool.heki.effect-boundaries]]
name = "config"
scopes = ["*.env:*"]
forbid = ["blocking"]

[[tool.heki.write-owners]]
name = "w"
targets = ["Model.items['k']"]
allowed-writers = ["Store.*"]

[[tool.heki.read-owners]]
name = "r"
targets = ["Model.x"]
allowed-readers = ["Store.*"]

[[tool.heki.call-owners]]
name = "c"
targets = ["Ledger.post"]
allowed-callers = ["Store.*"]

[[tool.heki.readonly-returns]]
name = "ro"
producers = ["Schema.fields"]

[[tool.heki.invalidation]]
name = "i"
targets = ["Model.x"]
invalidators = ["Index.refresh"]
"#,
    )
    .write("backend/jobs.py", "def run():\n    pass\n")
    .write("backend/broken.py", "def broken(:\n")
    .write("scripts/broken.py", "def broken(:\n");
    let paths = ["backend/main.py", "backend/config", "backend/broken.py"];
    let (_, report, _) = recommend_json(&p, &paths);
    assert_eq!(
        report["analyzed"]["left_out"]["files"],
        json!(["backend/jobs.py", "scripts/broken.py"])
    );
    assert_eq!(
        report["config"]["src"],
        json!(["backend"]),
        "{:#}",
        report["blind_spots"]["source_roots"]
    );
    // An import that resolves now and would not, also in a file the paths given leave out.
    let q = subdirectory_app("recommend_source_root_import");
    q.write("tools.py", "import time\nimport backend.config.env\n");
    let lost =
        "`backend.config.env` in tools.py would name no project module, not backend/config/env.py";
    assert_eq!(reason(&q, &[]), lost);
    assert_eq!(reason(&q, &["backend"]), lost);
    // Two files that would define one module, one of which heki cannot parse.
    let r = subdirectory_app("recommend_source_root_shared");
    r.write("main.py", "def broken(:\n");
    assert_eq!(
        reason(&r, &[]),
        "backend/main.py and main.py would each define module `main`"
    );
}

#[test]
fn a_source_root_is_not_tried_when_heki_does_not_read_the_files_left_out() {
    let p = subdirectory_app("recommend_source_root_left_out_caps");
    for i in 0..55 {
        p.write(&format!("tests/test_{i:02}.py"), "");
    }
    let (_, report, _) = recommend_json(&p, &["backend"]);
    let roots = &report["blind_spots"]["source_roots"];
    assert_eq!(
        (&roots[0]["in_block"], &roots[0]["reason"]),
        (
            &json!(false),
            &json!(
                "not tried: the paths given leave out more Python files below the project root \
                 than heki reads (50), and a source root may change what those import"
            )
        )
    );
    assert_eq!(report["config"], json!({}));
}

#[test]
fn a_source_root_stays_out_when_a_module_would_gain_a_file() {
    let p = Project::new("recommend_source_root_gains_a_file");
    // `util` is defined twice already; `z` would define it a third time, `w` not.
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki]\nsrc = [\"x\", \"y\"]\n",
    )
    .write("x/util.py", "")
    .write("y/util.py", "")
    .write("z/util.py", "")
    .write("z/jobs/run.py", "")
    .write("z/main.py", "import jobs.run\n")
    .write("w/tools/run.py", "")
    .write("w/cli.py", "import tools.run\n");
    let (_, report, _) = recommend_json(&p, &[]);
    let verdicts: Vec<(&Value, &Value, &Value)> = (report["blind_spots"]["source_roots"])
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (&r["directory"], &r["in_block"], &r["reason"]))
        .collect();
    assert_eq!(
        verdicts,
        [
            (
                &json!("w"),
                &json!(true),
                &json!(
                    "its imports resolve, and no import, contract, or baseline entry loses its \
                     module"
                )
            ),
            (
                &json!("z"),
                &json!(false),
                &json!("x/util.py, y/util.py and z/util.py would each define module `util`")
            )
        ]
    );
    assert_eq!(report["config"], json!({"src": ["x", "y", "w"]}));
}

#[test]
fn a_source_root_stays_out_when_its_import_would_name_another_roots_file() {
    let p = Project::new("recommend_source_root_own_file");
    // With `a/lib` a source root, `a/lib/c.py` is `c`: once `b` is taken, `lib.c` of `a` names
    // `b`'s file, so `a` stays out, and with it `b`, under which an import `a` was not taken
    // for names a file.
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki]\nsrc = [\"a/lib\"]\n",
    )
    .write("a/lib/c.py", "")
    .write("a/run.py", "import lib.c\n")
    .write("b/lib/c.py", "")
    .write("b/server.py", "import lib.c\n")
    .write("b/worker.py", "import lib.c\n");
    let (_, report, _) = recommend_json(&p, &[]);
    let verdicts: Vec<(&Value, &Value)> = (report["blind_spots"]["source_roots"])
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (&r["directory"], &r["reason"]))
        .collect();
    assert_eq!(
        verdicts,
        [
            (
                &json!("b"),
                &json!(
                    "`lib.c` in a/run.py would name b/lib/c.py below source root `b`, not the \
                     one of a/run.py: heki cannot tell it is the file meant"
                )
            ),
            (
                &json!("a"),
                &json!("`lib.c` would still name no project module")
            )
        ]
    );
    assert_eq!(report["config"], json!({}));
}

#[test]
fn a_source_root_stays_out_when_an_import_would_name_another_file() {
    let p = Project::new("recommend_source_root_other_file");
    p.write("backend/__init__.py", "")
        .write(
            "backend/lib/config/env.py",
            "import time\n\n\ndef load():\n    time.sleep(1)\n",
        )
        .write("backend/lib/app.py", "import config.env\n")
        // With `backend/lib` a source root, `backend` would still be a module, but the file
        // this import names would be `config.env`.
        .write(
            "main.py",
            "import backend.lib.config.env\n\n\nasync def handler():\n    \
             backend.lib.config.env.load()\n",
        )
        // A candidate the block takes.
        .write(
            "tools/helpers/run.py",
            "import time\n\n\ndef go():\n    time.sleep(1)\n",
        )
        .write(
            "tools/cli.py",
            "import helpers.run\n\n\nasync def main():\n    helpers.run.go()\n",
        );
    let (_, report, _) = recommend_json(&p, &[]);
    let roots = &report["blind_spots"]["source_roots"];
    let verdicts: Vec<(&Value, &Value, &Value)> = roots
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (&r["directory"], &r["in_block"], &r["reason"]))
        .collect();
    assert_eq!(
        verdicts,
        [
            (
                &json!("backend/lib"),
                &json!(false),
                &json!(
                    "`backend.lib.config.env` in main.py would name `lib.config.env` of \
                     backend/__init__.py, not backend/lib/config/env.py"
                )
            ),
            (
                &json!("tools"),
                &json!(true),
                &json!(
                    "its imports resolve, and no import, contract, or baseline entry loses its \
                     module"
                )
            )
        ]
    );
    assert_eq!(report["config"], json!({"src": ["tools"]}));
    assert_follows_the_schema(&report);
    // With the block, the import that resolves now still does, and the one of `tools` too.
    assert_eq!(waits_reported(&p), ["main.py"]);
    apply_block(&p, &report);
    assert_eq!(waits_reported(&p), ["main.py", "tools/cli.py"]);
}

#[test]
fn a_source_root_stays_out_when_it_would_rename_what_a_root_taken_before_resolves() {
    let p = Project::new("recommend_source_root_nested");
    p.write(
        "a/main.py",
        "import b.c\nimport b.d\n\n\nasync def handler():\n    b.c.load()\n",
    )
    .write("a/b/worker.py", "import c\n")
    .write(
        "a/b/c.py",
        "import time\n\n\ndef load():\n    time.sleep(1)\n",
    )
    .write("a/b/d.py", "");
    let (_, report, _) = recommend_json(&p, &[]);
    let roots = &report["blind_spots"]["source_roots"];
    let verdicts: Vec<(&Value, &Value, &Value)> = roots
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (&r["directory"], &r["imports"], &r["in_block"]))
        .collect();
    assert_eq!(
        verdicts,
        [
            (&json!("a"), &json!(2), &json!(true)),
            (&json!("a/b"), &json!(1), &json!(false))
        ]
    );
    // `b.c` names a file only once `a` is taken; `a/b` would name that file `c`.
    assert_eq!(
        roots[1]["reason"],
        json!("`b.c` in a/main.py would name no project module, not a/b/c.py")
    );
    assert_eq!(report["config"], json!({"src": ["a"]}));
    assert_follows_the_schema(&report);
    let (_, out, _) = p.heki(&["recommend"]);
    assert!(
        out.contains(
            "\n  source root `a/b`: 1 import(s) name its files (c): stays out: `b.c` in \
             a/main.py would name no project module, not a/b/c.py\n"
        ),
        "{out}"
    );
    // With the block, the imports `a` was taken for resolve.
    assert_eq!(waits_reported(&p), Vec::<String>::new());
    apply_block(&p, &report);
    assert_eq!(waits_reported(&p), ["a/main.py"]);
}

#[test]
fn an_import_a_root_taken_before_resolves_by_chance_does_not_hold_back_its_own() {
    let p = Project::new("recommend_source_root_two_services");
    p.write("x/config/__init__.py", "")
        .write("x/run.py", "import config\n")
        .write(
            "y/config/env.py",
            "import time\n\n\ndef load():\n    time.sleep(1)\n",
        )
        // With `x` taken, this names `env` of x/config/__init__.py; it is `y`'s import.
        .write(
            "y/app.py",
            "import config.env\n\n\nasync def handler():\n    config.env.load()\n",
        );
    let (_, report, _) = recommend_json(&p, &[]);
    let roots = &report["blind_spots"]["source_roots"];
    let verdicts: Vec<(&Value, &Value)> = roots
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (&r["directory"], &r["in_block"]))
        .collect();
    assert_eq!(
        verdicts,
        [(&json!("x"), &json!(true)), (&json!("y"), &json!(true))]
    );
    assert_eq!(report["config"], json!({"src": ["x", "y"]}));
    // With the block, each service's import names its own file.
    assert_eq!(waits_reported(&p), Vec::<String>::new());
    apply_block(&p, &report);
    assert_eq!(waits_reported(&p), ["y/app.py"]);
}

#[test]
fn a_report_naming_several_library_names_counts_for_the_first() {
    let p = Project::new("recommend_several_names");
    // Every implementation `x.go()` runs calls a library name of its own.
    p.write(
        "app.py",
        "import stripe\n\n\nclass Base:\n    def go(self):\n        stripe.a()\n\n\n\
         class Sub(Base):\n    def go(self):\n        stripe.b()\n\n\n\
         async def handle(x: Base):\n    x.go()\n",
    );
    let (_, report, _) = recommend_json(&p, &[]);
    let symbols = &report["blind_spots"]["uncatalogued"][0]["symbols"];
    assert_eq!(
        (&symbols[0]["symbol"], &symbols[0]["if_waits"]["EFF101"]),
        (&json!("stripe.a"), &json!(1))
    );
    assert_eq!(
        (&symbols[1]["symbol"], &symbols[1]["if_waits"]["EFF101"]),
        (&json!("stripe.b"), &json!(0))
    );
    assert!(
        report["notes"].as_array().unwrap().contains(&json!(
            "A report naming several of the library names tried counts for the first by name."
        )),
        "{report:#}"
    );
}

#[test]
fn the_text_lists_the_unresolved_calls_with_the_most_reach_first() {
    let p = Project::new("recommend_unresolved_many");
    let jobs: String = (0..12)
        .map(|i| format!("def job_{i:02}(client):\n    client.run()\n\n\n"))
        .collect();
    p.write(
        "app.py",
        &format!("{jobs}async def poll(client):\n    job_11(client)\n"),
    );
    let (_, report, _) = recommend_json(&p, &[]);
    let blind = &report["blind_spots"];
    assert_eq!(blind["totals"]["unresolved_groups"], json!(12));
    // The call on the event loop first, then by function.
    assert_eq!(blind["unresolved"][0]["function"], json!("app.job_11"));
    assert_eq!(blind["unresolved"][1]["function"], json!("app.job_00"));
    let (_, out, _) = p.heki(&["recommend"]);
    assert!(
        out.contains("\n    app.job_08      1      0      0  parameter `client` has no annotation (run)\n    and 2 more\n"),
        "{out}"
    );
}

#[test]
fn a_project_heki_sees_whole_says_so() {
    let p = Project::new("recommend_nothing_unseen");
    p.write("__init__.py", "")
        .write("other.py", "def f() -> int:\n    return 1\n")
        .write(
            "mod.py",
            "from . import other\n\n\ndef g() -> int:\n    return other.f()\n",
        );
    let (_, out, _) = p.heki(&["recommend"]);
    assert!(
        out.contains(
            "\nWhat heki cannot see (no report either way)\n  Nothing: every call resolves, every \
             library name called has a catalog entry, and every function is modeled.\n"
        ),
        "{out}"
    );
}

#[test]
fn an_import_below_several_directories_is_the_one_of_the_importing_files_own() {
    let p = Project::new("recommend_source_root_services");
    p.write("svc_a/utils.py", "def a():\n    pass\n")
        .write("svc_a/jobs.py", "")
        .write("svc_a/views.py", "")
        .write("svc_a/forms.py", "")
        .write(
            "svc_a/main.py",
            "import forms\nimport jobs\nimport utils\nimport views\n\n\ndef run():\n    \
             utils.a()\n",
        )
        .write("svc_b/utils.py", "def b():\n    pass\n")
        // Below both, and in neither: no candidate's. With `svc_a` a source root it would name
        // svc_a/utils.py, which nothing shows it means.
        .write("tool.py", "import utils\n")
        // A candidate taken whether `svc_a` is or not.
        .write("tools/helpers/run.py", "")
        .write("tools/cli.py", "import helpers.run\n");
    let verdict = |p: &Project| {
        let (_, report, _) = recommend_json(p, &[]);
        assert_follows_the_schema(&report);
        let roots = &report["blind_spots"]["source_roots"];
        assert_eq!(roots.as_array().unwrap().len(), 2, "{roots:#}");
        assert_eq!(
            (&roots[0]["directory"], &roots[0]["imports"]),
            (&json!("svc_a"), &json!(4))
        );
        (
            roots[0]["in_block"].clone(),
            roots[0]["reason"].clone(),
            report["config"].clone(),
        )
    };
    // Left out, `svc_a` lets the others be tried again without it.
    assert_eq!(
        verdict(&p),
        (
            json!(false),
            json!(
                "`utils` in tool.py would name svc_a/utils.py below source root `svc_a`, not the \
                 one of tool.py: heki cannot tell it is the file meant"
            ),
            json!({"src": ["tools"]})
        )
    );
    p.write("tool.py", "");
    assert_eq!(verdict(&p).2, json!({"src": ["svc_a", "tools"]}));
    let (_, out, _) = p.heki(&["recommend"]);
    assert!(
        out.contains(
            "\n  source root `svc_a`: 4 import(s) name its files (forms, jobs, utils, ...): in the \
             block\n"
        ),
        "{out}"
    );
}

#[test]
fn a_regular_package_is_no_source_root_candidate() {
    let p = Project::new("recommend_source_root_package");
    // As a source root, backend/__init__.py would be a module named `__init__`.
    p.write("backend/__init__.py", "")
        .write("backend/config/env.py", "def load():\n    pass\n")
        .write(
            "backend/main.py",
            "import config.env\n\n\ndef run():\n    config.env.load()\n",
        );
    let (_, report, _) = recommend_json(&p, &[]);
    let blind = &report["blind_spots"];
    assert_eq!(blind["source_roots"], json!([]));
    assert_eq!(report["config"], json!({}));
    // No source root makes heki follow it: the call stays one heki takes for a library's.
    assert_eq!(blind["uncatalogued"][0]["package"], json!("config"));
}

#[test]
fn a_directory_above_a_source_root_does_not_resolve_what_the_root_names() {
    let p = Project::new("recommend_source_root_above");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki]\nsrc = [\"backend/lib\"]\n",
    )
    .write("backend/lib/config/env.py", "def load():\n    pass\n")
    .write("backend/main.py", "import lib.config.env\n");
    let (_, report, _) = recommend_json(&p, &[]);
    let roots = &report["blind_spots"]["source_roots"];
    assert_eq!(
        (
            &roots[0]["directory"],
            &roots[0]["in_block"],
            &roots[0]["reason"]
        ),
        (
            &json!("backend"),
            &json!(false),
            &json!("`lib.config.env` would still name no project module")
        )
    );
}

#[test]
fn names_the_catalog_lacks_come_by_what_heki_would_report_through_them() {
    let p = Project::new("recommend_catalog_may_miss");
    p.write(
        "app.py",
        "import requests\nimport stripe\n# Past the top of the package tree: no module.\n\
         from .. import nothing\n\n\nasync def link(path: str):\n    return \
         requests.utils.quote(path)\n\n\nasync def unlink(path: str):\n    a = \
         requests.utils.unquote(path)\n    b = requests.utils.unquote(path)\n    return a, b\n\n\n\
         def fetch_one(x, flag):\n    if flag:\n        return None\n    return \
         stripe.Charge.retrieve(x)\n\n\ndef fetch_all(items: list[str], flag: bool):\n    for x \
         in items:\n        fetch_one(x, flag)\n",
    );
    let (_, report, _) = recommend_json(&p, &[]);
    let blind = &report["blind_spots"];
    let missing: Vec<(Value, Value)> = blind["catalog_may_miss"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| (s["symbol"].clone(), s["if_waits"]["EFF101"].clone()))
        .collect();
    assert_eq!(
        missing,
        [
            (json!("requests.utils.unquote"), json!(2)),
            (json!("requests.utils.quote"), json!(1))
        ]
    );
    // A query on some paths only would be reported as heuristic.
    assert_eq!(
        blind["uncatalogued"][0]["symbols"][0]["if_queries"],
        json!({"EFF103": 0, "EFF113": 1})
    );
    assert_follows_the_schema(&report);
}
