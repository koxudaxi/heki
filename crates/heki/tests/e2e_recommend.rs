//! `heki recommend` (experimental) end to end: the counts it takes from the analysis `heki check`
//! runs, the files the paths leave out, the baseline, the configuration file it writes for, its
//! JSON object against the published schema, and that it writes nothing.

mod common;

use common::{Project, schema};
use serde_json::{Value, json};
use std::collections::BTreeMap;

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
    // "What keeps reports heuristic" has tests of its own.
    let (before, section) = out
        .split_once("\nWhat keeps reports heuristic\n")
        .unwrap_or_else(|| panic!("{out}"));
    let after = section.split_once("\n\n").unwrap().1;
    assert_eq!(
        format!("{before}\n{after}"),
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
         \x20 No reports here: EFF102, EFF105, EFF106, EFF107, EFF109, EFF110, EFF112, EFF113, EFF114, MUT101, \
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
    let one_thread = common::heki_command()
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
        \x20 No reports here: EFF102, EFF105, EFF106, EFF110, EFF112, EFF113, EFF114, MUT101, MUT108, MUT111, MUT112, MUT113, \
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
    let out = common::heki_command()
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
        // A loop over the data, whose items the probe turns into ints.
        (
            "app/counts.py",
            "range(40):\n        if p in allowed:",
            "probes:\n        if int(p) in allowed:",
        ),
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
