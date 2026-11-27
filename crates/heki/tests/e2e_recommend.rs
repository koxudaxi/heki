//! `heki recommend` (experimental) end to end: the counts it takes from the analysis `heki check`
//! runs, the files the paths leave out, the baseline, the configuration file it writes for, its
//! JSON object against the published schema, and that it writes nothing.

mod common;

use common::{Project, schema};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::OnceLock;

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

/// `OPT101` exact once, and heuristic once (the rewrite of a list the loop body hands to a call
/// is not proven), withheld unless named or with `lint.heuristic`; `OPT110` heuristic once, an
/// opt-in rule's report.
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
    found = []
    for name in names:
        log(found)
        found = found + [name]
    return found
";

/// A project with an exact `EFF101` and `OPT101` report, a heuristic `EFF101`, `OPT101`, and
/// `OPT110` report, and four test files.
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

/// The notes on what makes `heki check` fail: `failing` reports shown now fail it, `passing`
/// do not.
fn exit_notes(failing: usize, passing: usize) -> Vec<String> {
    vec![
        format!(
            "heki check fails on {failing} report(s) shown now (exit code 1): a Ratchet baseline \
             records them, so that it fails only on new ones (`heki check --baseline \
             heki-baseline.toml --update-baseline`, then `baseline = \"heki-baseline.toml\"` in \
             the configuration)."
        ),
        format!(
            "heki check does not fail on {passing} report(s) shown now: warnings, and heuristic \
             reports, which can be wrong about the code and count as warnings; `fail-on = \
             \"warning\"` in the lint settings makes them fail it."
        ),
    ]
}

/// The diagnostics `heki check --output-format json <args>` prints.
fn checked(p: &Project, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["check", "--output-format", "json"];
    all.extend_from_slice(args);
    let (_, out, err) = p.heki(&all);
    serde_json::from_str::<Vec<Value>>(&out).unwrap_or_else(|e| panic!("{e}: {out}{err}"))
}

fn assert_follows_the_schema(report: &Value) {
    // Read once: the tests run in parallel, and each `Project::new` of one name removes the
    // directory another test may be running heki in.
    static PUBLISHED: OnceLock<Value> = OnceLock::new();
    let published = PUBLISHED.get_or_init(|| {
        let (code, out, _) =
            Project::new("recommend_schema").heki(&["recommend", "--output-format-json-schema"]);
        assert_eq!(code, 0);
        serde_json::from_str(&out).unwrap()
    });
    assert_eq!(published["$id"], report["$schema"]);
    let errors = schema::errors(published, report);
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
         1. Quickest safety win: fix EFF101 at app/api.py:4 (get_user), the one exact report of a \
         bug shown now, through 4 place(s): heki check (Reports now)\n\
         2. Smallest wall for a coding agent: the effect-boundaries table \"app.lists without \
         blocking calls\" (medium): 3 function(s) of app.lists reach no blocking call heki sees; \
         with fail = true, heki check fails on an edit that makes one reach a blocking call \
         (Contract candidates; the table is in the keys at the end)\n\
         3. Biggest blind spot: 1 call(s) heki does not resolve through a parameter without \
         annotation or a value annotated `Any` or `object`; an annotation naming the class lets \
         heki follow them (What heki cannot see)\n\
         \n\
         Reports now  2 shown (2 exact, 0 heuristic), 2 heuristic withheld, 4 heuristic rule(s) \
         off\n\
         \n\
         Types the code already keeps\n\
         \x20 No read-only parameter to suggest a type for here.\n\
         \n\
         Rules worth enabling here\n\
         \x20 rule    reports  exact  consequence  right about the code (published)  real harm  \
         block\n\
         \x20 EFF101        1      0  bug          82% (69-90%), n=50, 2026-11-21    32%        \
         yes (on now: naming it adds its heuristic reports)\n\
         \x20 OPT101        1      0  slowdown     78% (45-94%), n=9, 2026-11-21     0%         \
         no: slower code, not a bug (on now: naming it adds its heuristic reports)\n\
         \x20 OPT110        1      0  slowdown     78% (65-87%), n=50, 2026-11-21    6%         \
         no: slower code, not a bug\n\
         \x20 see the 3 report(s): heki check --extend-select EFF101,OPT101,OPT110\n\
         \x20 No reports here: 31 rule(s); exact: EFF102, EFF105, OPT103, OPT111, OPT120, OPT121, \
         OPT130; experimental and exact: MUT101, MUT111; heki recommend --all names the 22 \
         heuristic one(s)\n\
         \n\
         Contract candidates\n\
         \x20 effect-boundaries  app.lists  medium\n\
         \x20   3 function(s) in 1 module(s) reach no blocking call heki sees; 1 reach code heki \
         does not see (app.lists.logged_names)\n\
         \x20   trial: EFF201 0\n\
         \x20 For every block, accesses inside code heki cannot follow (unresolved calls, \
         libraries without a catalog) are not seen.\n\
         \n\
         Notes\n\
         \x20 - heki check fails on 1 report(s) shown now (exit code 1): a Ratchet baseline \
         records them, so that it fails only on new ones (`heki check --baseline \
         heki-baseline.toml --update-baseline`, then `baseline = \"heki-baseline.toml\"` in the \
         configuration).\n\
         \x20 - heki check does not fail on 1 report(s) shown now: warnings, and heuristic \
         reports, which can be wrong about the code and count as warnings; `fail-on = \
         \"warning\"` in the lint settings makes them fail it.\n\
         \x20 - the contract candidates are invariants the code keeps today, not architectural \
         intent: adopt only the boundaries you intend to keep, give each an actionable \
         `message`, and set `fail = true` on each kept table so that its reports fail `heki \
         check`; consider a CODEOWNERS-protected heki.toml, and put the legacy violations of \
         a stricter wall in a Ratchet baseline, not in an `allowed-*` list.\n\
         \n\
         pyproject.toml: keys to add or change\n\
         \x20 [tool.heki.lint]\n\
         \x20 extend-select = [\"EFF101\"]  # adds EFF101\n\
         \n\
         \x20 [[tool.heki.effect-boundaries]]\n\
         \x20 name = \"app.lists without blocking calls\"\n\
         \x20 scopes = [\"app.lists:*\"]\n\
         \x20 forbid = [\"blocking\"]\n"
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
        let failing = (shown.iter())
            .filter(|d| d["certainty"] == "exact" && d["severity"] == "error")
            .count();
        assert_eq!(
            (
                &now["shown"],
                &now["exact"],
                &now["heuristic"],
                &now["failing"]
            ),
            (
                &json!(shown.len()),
                &json!(exact),
                &json!(shown.len() - exact),
                &json!(failing)
            ),
            "{args:?}"
        );
        // The reports that fail the run are those that make `heki check` exit with 1.
        let (check_code, _, _) = p.heki(&[&["check"][..], args].concat());
        assert_eq!(check_code, i32::from(failing > 0), "{args:?}");
        assert_follows_the_schema(&report);
    }
    let (_, heuristic, _) = recommend_json(&p, &["--heuristic"]);
    assert_eq!(
        heuristic["reports_now"],
        json!({"shown": 4, "exact": 2, "heuristic": 2, "withheld_heuristic": 0,
               "heuristic_rules_off": 0, "baseline": null, "failing": 1})
    );
    // Naming the exact rule shows its heuristic report; `EFF101`'s, not named, stays withheld.
    let (_, named, _) = recommend_json(&p, &["--extend-select", "OPT101"]);
    assert_eq!(
        (
            &named["reports_now"]["withheld_heuristic"],
            &named["reports_now"]["heuristic"]
        ),
        (&json!(1), &json!(1))
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
            &json!(3),
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
    // The contract trial and the verification run with the block: `app/lists.py` keeps an
    // effect boundary.
    assert_eq!(
        report["runs"],
        json!([{"label": "discovery"}, {"label": "contract-trial"},
               {"label": "verification"}])
    );
    assert_eq!(
        (&report["notes"], &report["config"], &report["config_toml"]),
        (
            &json!(
                [
                    exit_notes(1, 1),
                    vec![common::recommend_candidates_note(true)]
                ]
                .concat()
            ),
            &json!({"lint": {"extend-select": ["EFF101"]},
                    "effect-boundaries": [{"name": "app.lists without blocking calls",
                                           "scopes": ["app.lists:*"], "forbid": ["blocking"]}]}),
            &json!(
                "[tool.heki.lint]\nextend-select = [\"EFF101\"]  # adds EFF101\n\n\
                 [[tool.heki.effect-boundaries]]\nname = \"app.lists without blocking calls\"\n\
                 scopes = [\"app.lists:*\"]\nforbid = [\"blocking\"]\n"
            )
        )
    );
    let (_, text, _) = p.heki(&["recommend"]);
    assert_eq!(report["content"], json!(text));
    // Without a contract or rule to add, the discovery run is the only one.
    let (_, ignoring, _) = recommend_json(&p, &["--heuristic", "--ignore", "EFF201"]);
    assert_eq!(ignoring["runs"], json!([{"label": "discovery"}]));
    let mut notes = exit_notes(1, 3);
    notes.push(
        "the configuration ignores EFF201, so no effect-boundaries contract is proposed".to_owned(),
    );
    assert_eq!(
        (&ignoring["config"], &ignoring["notes"]),
        (&json!({}), &json!(notes))
    );
}

/// With `fail-on = "warning"` set, the note on the contract candidates does not ask for it.
#[test]
fn the_candidates_note_does_not_ask_for_a_fail_on_that_is_set() {
    let p = shop("recommend_candidates_note_fail_on");
    p.write("heki.toml", "[lint]\nfail-on = \"warning\"\n");
    let (_, report, _) = recommend_json(&p, &[]);
    assert_eq!(
        report["contracts"].as_array().map(Vec::len),
        Some(1),
        "{report:#}"
    );
    let notes = report["notes"].as_array().unwrap();
    assert!(
        notes.contains(&json!(common::recommend_candidates_note(false))),
        "{notes:?}"
    );
    assert_eq!(notes.len(), 2, "{notes:?}");
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
    // The regression is an exact error: `heki check` fails on it, and on nothing else, so no
    // note proposes a baseline or `fail-on`.
    assert_eq!(report["reports_now"]["failing"], json!(1));
    assert_eq!(p.heki(&["check", "--baseline", "b.toml"]).0, 1);
    assert!(
        (report["notes"].as_array().unwrap().iter())
            .all(|note| !note.as_str().unwrap().starts_with("heki check")),
        "{report:#}"
    );
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
    assert!(
        err.starts_with("error: pyproject.toml:2:1: unknown key `nope` in [tool.heki]\n"),
        "{err}"
    );
}

#[test]
fn the_configuration_file_decides_where_the_keys_go() {
    let p = shop("recommend_config_file");
    std::fs::remove_file(p.dir.join("pyproject.toml")).unwrap();
    p.write(
        "heki.toml",
        "[lint]\nextend-select = [\"EFF105\"]\nheuristic = true\n",
    );
    let (_, report, _) = recommend_json(&p, &[]);
    assert_eq!(
        (&report["config_file"], &report["table_prefix"]),
        (&json!("heki.toml"), &json!(""))
    );
    let (_, out, _) = p.heki(&["recommend"]);
    assert!(
        out.ends_with(
            "\nheki.toml: keys to add or change\n  [[effect-boundaries]]\n  name = \"app.lists \
             without blocking calls\"\n  scopes = [\"app.lists:*\"]\n  forbid = [\"blocking\"]\n"
        ),
        "{out}"
    );
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
    let content = none["content"].as_str().unwrap();
    assert!(
        content.contains("\npyproject.toml: keys to add or change\n  [tool.heki.lint]\n")
            && content.contains("\n  [[tool.heki.effect-boundaries]]\n"),
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
/// measurement shows real harm below 10%), and the experimental `EFF110`.
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

async def read_line(reader: asyncio.StreamReader):
    return await reader.readline()

async def both(reader: asyncio.StreamReader):
    return await asyncio.gather(read_line(reader), read_line(reader))
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
        \x20 EFF101        1      0  bug          82% (69-90%), n=50, 2026-11-21    32%        yes (on now: naming it adds its heuristic reports)\n\
        \x20 EFF107        1      -  bug          not measured                      -          yes\n\
        \x20 MUT106        1      -  bug          not measured                      -          yes\n\
        \x20 MUT107        1      -  bug          78% (65-87%), n=50, 2026-11-21    4%         no: real harm below 10%\n\
        \x20 OPT101        1      0  slowdown     78% (45-94%), n=9, 2026-11-21     0%         no: slower code, not a bug (on now: naming it adds its heuristic reports)\n\
        \x20 OPT110        1      0  slowdown     78% (65-87%), n=50, 2026-11-21    6%         no: slower code, not a bug\n\
        \x20 Optional (experimental, never in the block)\n\
        \x20 EFF110        1      -  bug          not measured                      -          no: experimental\n\
        \x20 see the 7 report(s): heki check --extend-select EFF101,EFF107,MUT106,MUT107,OPT101,OPT110,EFF110\n\
        \x20 No reports here: 27 rule(s); exact: EFF102, EFF105, OPT103, OPT111, OPT120, OPT121, \
        OPT130; experimental and exact: MUT101, MUT111; heki recommend --all names the 18 heuristic \
        one(s)"
    );
    assert!(
        out.contains(
            "\npyproject.toml: keys to add or change\n  [tool.heki.lint]\n  extend-select = \
             [\"EFF101\", \"EFF107\", \"MUT106\"]  # adds EFF101, EFF107, MUT106\n"
        ),
        "{out}"
    );
    // `--all` names the heuristic ones, and `MUT103`, as a saved snapshot does.
    let quiet_lines = |args: &[&str]| -> Vec<String> {
        let mut all = vec!["recommend"];
        all.extend_from_slice(args);
        let (_, out, _) = p.heki(&all);
        (out.lines())
            .skip_while(|l| !l.starts_with("  No reports here: "))
            .take_while(|l| !l.is_empty())
            .map(str::to_owned)
            .collect()
    };
    let listed = quiet_lines(&["--all"]);
    assert!(
        listed[0].ends_with(
            "; experimental and exact: MUT101, MUT111; heuristic: EFF106, EFF112, EFF114, \
             EFF115, EFF117, EFF118, IMP101, IMP102, IMP104, IMP301, IMP302, MUT108, MUT112, \
             MUT113, MUT115, MUT118, MUT120, OPT112"
        ),
        "{listed:?}"
    );
    let snapshot = "  MUT103: it compares with a saved summary snapshot: run `heki snapshot --write`, \
                    then select it.";
    assert_eq!(listed[1], snapshot);
    let (code, _, err) = p.heki(&["snapshot", "--write"]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(quiet_lines(&[])[1], snapshot);
    std::fs::remove_file(p.dir.join(".heki-summary-snapshot.json")).unwrap();
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
    let opt101 = rule(&report, "OPT101");
    assert_eq!(
        (
            &opt101["enabled_now"],
            &opt101["reports"],
            &opt101["heuristic"]
        ),
        (&json!(true), &json!(1), &json!(1))
    );
    assert_eq!(
        opt101["measurement"],
        json!({"rule": "OPT101", "round": 3, "date": "2026-11-21", "sample": 9, "confirmed": 0,
               "plausible": 0, "benign": 7, "false_claims": 2, "right_percent": 78,
               "interval_percent": [45, 94], "real_harm_percent": 0})
    );
    // An opt-in exact rule: every report naming it would add.
    let opt110 = rule(&report, "OPT110");
    assert_eq!(
        (
            &opt110["activation"],
            &opt110["enabled_now"],
            &opt110["reports"],
            &opt110["exact"],
            &opt110["in_block"],
            &opt110["reason"]
        ),
        (
            &json!("opt-in"),
            &json!(false),
            &json!(1),
            &json!(0),
            &json!(false),
            &json!("slower code, not a bug")
        )
    );
    assert_eq!(rule(&report, "MUT103")["reports"], Value::Null);
    assert_eq!(
        report["config"]["lint"],
        json!({"extend-select": ["EFF101", "EFF107", "MUT106"]})
    );
    // Rules the current selection runs in full, and contract rules, are not listed.
    for code in ["EFF103", "OPT102", "MUT201", "EFF201"] {
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
        ignoring["config"]["lint"],
        json!({"extend-select": ["EFF101", "EFF107"]})
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
        let before = checked(&p, &[]);
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
        // A rule on by default (`EFF101`) is in the block for its heuristic reports: the block
        // adds them to the exact ones shown already.
        for r in &promised {
            let code = r["code"].as_str().unwrap();
            let count =
                |reports: &[Value]| reports.iter().filter(|d| d["rule_code"] == code).count();
            let added = count(&after) - count(&before);
            assert_eq!(json!(added), r["reports"], "{file}: {code}");
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

/// The command the text of `heki recommend <args>` names under "Rules worth enabling here".
fn command_shown(p: &Project, args: &[&str]) -> Vec<String> {
    let mut all = vec!["recommend"];
    all.extend_from_slice(args);
    let (_, out, _) = p.heki(&all);
    let line = (out.lines())
        .find(|l| l.starts_with("  see the "))
        .unwrap_or_else(|| panic!("{out}"));
    let command = line.split_once(": heki ").unwrap().1;
    command.split_whitespace().map(str::to_owned).collect()
}

#[test]
fn the_command_under_the_rules_shows_every_report_they_count() {
    let p = shop_with_opt_in("recommend_rules_command");
    for (args, codes) in [
        (vec![], "EFF101,EFF107,MUT106,MUT107,OPT101,OPT110,EFF110"),
        // The paths given are the command's; a rule the configuration ignores stays out of it,
        // since `ignore` wins over `extend-select`.
        (
            vec!["app", "--ignore", "MUT106"],
            "EFF101,EFF107,MUT107,OPT101,OPT110,EFF110",
        ),
    ] {
        let (_, report, _) = recommend_json(&p, &args);
        let command = command_shown(&p, &args);
        assert_eq!(command.last().unwrap(), codes, "{command:?}");
        if args.is_empty() {
            assert_eq!(command, ["check", "--extend-select", codes]);
        } else {
            assert_eq!(command[..2], ["check", "app"]);
        }
        let named: Vec<&str> = codes.split(',').collect();
        let counted: u64 = (report["rules"].as_array().unwrap().iter())
            .filter(|r| named.contains(&r["code"].as_str().unwrap()))
            .map(|r| r["reports"].as_u64().unwrap())
            .sum();
        assert_eq!(counted, 7 - u64::from(!args.is_empty()));
        // The command carries the options given to `heki recommend`.
        let run: Vec<&str> = command[1..].iter().map(String::as_str).collect();
        let shown = checked(&p, &run);
        assert_eq!(
            shown.len() as u64,
            report["reports_now"]["shown"].as_u64().unwrap() + counted,
            "{run:?}"
        );
    }
    // Analysis options and a file argument: the command shows the reports the whole analysis
    // counted, with the configuration it used.
    let c = shop_with_opt_in("recommend_rules_command_options");
    c.write("custom.toml", "extend-exclude = [\"app/users.py\"]\n")
        .write("pyproject.toml", "[project]\nname = \"t\"\n");
    for args in [
        vec!["--config", "custom.toml"],
        vec!["app/opt_in.py"],
        vec!["app/opt_in.py", "--config", "custom.toml"],
        vec!["--heuristic"],
        vec!["--no-heuristic"],
        vec!["--target-version", "py312"],
    ] {
        let (_, report, _) = recommend_json(&c, &args);
        let command = command_shown(&c, &args);
        // `analyzed.paths` keeps the paths given, as in version 3; the command names the
        // directory analyzed.
        let mut given: Vec<&str> = (args.iter().copied())
            .filter(|a| a.ends_with(".py"))
            .collect();
        if given.is_empty() {
            given.push(".");
        }
        assert_eq!(report["analyzed"]["paths"], json!(given), "{args:?}");
        assert_eq!(command[0], "check");
        assert_eq!(
            command.iter().any(|w| w == "--config"),
            args.contains(&"--config"),
            "{command:?}"
        );
        assert!(!command.iter().any(|w| w.ends_with(".py")), "{command:?}");
        let named: Vec<&str> = command.last().unwrap().split(',').collect();
        let counted: u64 = (report["rules"].as_array().unwrap().iter())
            .filter(|r| named.contains(&r["code"].as_str().unwrap()))
            .map(|r| r["reports"].as_u64().unwrap())
            .sum();
        let run: Vec<&str> = command[1..].iter().map(String::as_str).collect();
        assert_eq!(
            checked(&c, &run).len() as u64,
            report["reports_now"]["shown"].as_u64().unwrap() + counted,
            "{args:?} {run:?}"
        );
    }
    // A path a shell would split is quoted.
    let q = Project::new("recommend_rules_command_quoted");
    q.write("my app/opt_in.py", OPT_IN);
    let (_, out, _) = q.heki(&["recommend", "my app"]);
    assert!(
        out.contains(": heki check 'my app' --extend-select "),
        "{out}"
    );
}

/// The sections of the text, in the order it prints them.
const SECTIONS: &[&str] = &[
    "\n1. ",
    "\nReports now  ",
    "\nTypes the code already keeps\n",
    "\nRules worth enabling here\n",
    "\nWhat heki cannot see (no report either way)\n",
    "\nContract candidates\n",
    "\nWhat keeps reports heuristic\n",
    "\nNotes\n",
    "\npyproject.toml: keys to add or change\n",
];

#[test]
fn the_sections_come_in_their_order() {
    let p = shop_with_opt_in("recommend_section_order");
    let (_, out, _) = p.heki(&["recommend"]);
    let at: Vec<usize> = SECTIONS
        .iter()
        .map(|s| out.find(s).unwrap_or_else(|| panic!("{s:?}: {out}")))
        .collect();
    assert!(at.is_sorted(), "{at:?}: {out}");
}

/// The takeaways `heki recommend <args>` starts with, without their numbers, which count up
/// from 1.
fn takeaways(p: &Project, args: &[&str]) -> Vec<String> {
    let mut all = vec!["recommend"];
    all.extend_from_slice(args);
    let (_, out, _) = p.heki(&all);
    let mut lines = out.lines().skip(2);
    let mut found = Vec::new();
    while let Some(line) =
        (lines.next()).and_then(|l| l.strip_prefix(&format!("{}. ", found.len() + 1)))
    {
        found.push(line.to_owned());
    }
    found
}

#[test]
fn the_takeaways_come_first_one_of_each_kind() {
    let p = shop_with_opt_in("recommend_takeaways");
    let (_, out, _) = p.heki(&["recommend"]);
    let lines = takeaways(&p, &[]);
    assert_eq!(
        lines,
        [
            "Quickest safety win: fix EFF101 at app/api.py:4 (get_user), the one exact report of \
             a bug shown now, through 4 place(s): heki check (Reports now)",
            "Smallest wall for a coding agent: the effect-boundaries table \"scopes of app \
             without blocking calls\" (medium): 11 function(s) of app.lists and 1 more reach no \
             blocking call heki sees; with fail = true, heki check fails on an edit that makes \
             one reach a blocking call (Contract candidates; the table is in the keys at the end)",
            "Biggest blind spot: 1 call(s) heki does not resolve through a parameter without \
             annotation or a value annotated `Any` or `object`; an annotation naming the class \
             lets heki follow them (What heki cannot see)",
        ],
        "{out}"
    );
    // Each says what a part further down says: the report heki check prints, the table in the
    // keys, the calls the section counts.
    let eff101 = checked(&p, &[]);
    assert!(
        (eff101.iter())
            .any(|d| d["rule_code"] == "EFF101" && d["line"] == 4 && d["certainty"] == "exact"),
        "{eff101:#?}"
    );
    assert!(
        out.contains("\n  name = \"scopes of app without blocking calls\"\n"),
        "{out}"
    );
    assert!(
        out.contains("\n  calls heki does not resolve: 1 call(s) in 1 function(s)\n"),
        "{out}"
    );
    // `--all` lists more, and starts with the same takeaways.
    assert_eq!(takeaways(&p, &["--all"]), lines);
    // At `fail-on = "warning"`, a contract's reports fail the run without `fail = true`.
    p.write("heki.toml", "[lint]\nfail-on = \"warning\"\n");
    assert!(takeaways(&p, &[])[1].ends_with(
        "; at fail-on = \"warning\", heki check fails on an edit that makes one reach a \
             blocking call (Contract candidates; the table is in the keys at the end)"
    ));
}

#[test]
fn the_quickest_safety_win_is_the_rule_with_the_most_exact_reports_of_a_bug() {
    let p = shop("recommend_takeaway_rule");
    let lines = takeaways(&p, &["--select", "OPT101"]);
    assert_eq!(
        lines[0],
        "Quickest safety win: EFF101 adds 1 exact report(s) of a bug here: heki check --select \
         OPT101 --extend-select EFF101 (Rules worth enabling here)"
    );
    let command: Vec<&str> = "--select OPT101 --extend-select EFF101"
        .split(' ')
        .collect();
    let shown = checked(&p, &command);
    assert!(
        (shown.iter()).any(|d| d["rule_code"] == "EFF101" && d["certainty"] == "exact"),
        "{shown:#?}"
    );
    // With EFF101 on, the exact report of a bug shown now through the fewest places.
    p.write(
        "app/jobs.py",
        "import time\n\n\nasync def tick():\n    time.sleep(1)\n",
    );
    assert_eq!(
        takeaways(&p, &[])[0],
        "Quickest safety win: fix EFF101 at app/jobs.py:5 (tick), of the 2 exact reports of a bug \
         shown now the one through the fewest places (2): heki check (Reports now)"
    );
    // With a baseline, the regressions among its reports.
    let (code, _, err) = p.heki(&[
        "check",
        "--select",
        &every_rule(),
        "--baseline",
        "b.toml",
        "--update-baseline",
    ]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        takeaways(&p, &["--select", "OPT101", "--baseline", "b.toml"])[0],
        "Quickest safety win: EFF101 adds 2 exact report(s) of a bug here, 0 regression(s) among \
         its reports: heki check --select OPT101 --baseline b.toml --extend-select EFF101 (Rules \
         worth enabling here)"
    );
}

#[test]
fn a_takeaway_with_nothing_behind_it_is_left_out() {
    let p = Project::new("recommend_takeaways_none");
    p.write("app.py", "def f():\n    return 1\n");
    let (_, out, _) = p.heki(&["recommend"]);
    assert!(
        out.lines().nth(2).unwrap().starts_with("Reports now  "),
        "{out}"
    );
    // Nothing removes an A1 hazard but a change of the code.
    p.write("app.py", "def introspect():\n    return locals()\n");
    let (_, out, _) = p.heki(&["recommend"]);
    assert!(out.contains(" function(s) with an A1 hazard"), "{out}");
    assert_eq!(takeaways(&p, &[]), Vec::<String>::new(), "{out}");
}

#[test]
fn a_source_root_the_block_adds_comes_first() {
    let p = subdirectory_app("recommend_takeaways_src");
    p.write(
        "backend/jobs.py",
        "import time\n\n\nasync def tick():\n    time.sleep(1)\n",
    );
    assert_eq!(
        takeaways(&p, &[]),
        [
            "Biggest blind spot: 2 import(s) name files below backend, which heki does not take \
             for a source root; add src = [\"backend\"] (keys at the end), then run heki \
             recommend again: it renames modules (What heki cannot see)",
            "Quickest safety win: fix EFF101 at backend/jobs.py:5 (tick), the one exact report of \
             a bug shown now, through 2 place(s): heki check (Reports now)",
        ]
    );
}

#[test]
fn the_biggest_blind_spot_is_the_part_with_the_most_calls_a_setting_or_an_annotation_removes() {
    let p = Project::new("recommend_takeaway_blind");
    p.write("app/__init__.py", "").write("app/pay.py", BLIND);
    let blind_spot = |p: &Project| takeaways(p, &[]).pop().unwrap();
    // Three calls of `stripe`, one of a name the catalog lacks, one on a parameter without
    // annotation.
    assert_eq!(
        blind_spot(&p),
        "Biggest blind spot: 3 call(s) of 3 library name(s) without a catalog entry, which heki \
         takes to have no effect, such as stripe.Charge.create; an [[tool.heki.ecosystem]] entry \
         for a name whose effects you know removes it (What heki cannot see)"
    );
    // In a heki.toml, the table is at the top level.
    p.write("heki.toml", "");
    assert!(
        blind_spot(&p).contains("; an [[ecosystem]] entry for a name"),
        "{}",
        blind_spot(&p)
    );
    // A name of a package the catalog covers is for the catalog.
    let q = Project::new("recommend_takeaway_catalog");
    q.write(
        "app.py",
        "import requests\n\n\nasync def link(path: str):\n    return requests.utils.quote(path)\n",
    );
    assert_eq!(
        takeaways(&q, &[]),
        [format!(
            "Biggest blind spot: 1 call(s) of 1 name(s) heki's catalog lacks in packages it \
             covers, such as requests.utils.quote; if they wait or send a query, report them at \
             {}/issues for a catalog entry (What heki cannot see)",
            env!("CARGO_PKG_REPOSITORY")
        )]
    );
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
    // The baseline holds the heuristic `OPT110` report; the second is new.
    assert_eq!(
        rule(&report, "OPT110")["baseline"],
        json!({"allowed": 1, "regressions": 1})
    );
    let (_, out, _) = p.heki(&["recommend", "--baseline", "b.toml"]);
    assert!(
        out.contains("  rule    reports  exact  regressions  consequence  ")
            && out.contains("\n  MUT106        1      -            1  bug  "),
        "{out}"
    );
    // A run with the baseline prints the regressions of the reports the command adds.
    let regressions: u64 = (report["rules"].as_array().unwrap().iter())
        .filter_map(|r| r["baseline"]["regressions"].as_u64())
        .sum();
    assert!(
        out.contains(&format!(
            ", {regressions} regression(s) among them: heki check --baseline b.toml --extend-select "
        )),
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
fn a_bug_without_an_assessed_harm_is_listed_for_review() {
    let p = shop("recommend_listed_bug");
    p.write(
        "app/counter.py",
        "import threading\n\n\nclass Counter:\n    def __init__(self) -> None:\n        \
         self._lock = threading.Lock()\n        self.n = 0\n\n    def start(self) -> None:\n        \
         threading.Thread(target=self._run).start()\n\n    def _run(self) -> None:\n        \
         with self._lock:\n            self.n += 1\n\n    def reset(self) -> None:\n        \
         self.n = 0\n",
    );
    let (_, report, _) = recommend_json(&p, &[]);
    let eff117 = rule(&report, "EFF117");
    assert_eq!(
        (&eff117["in_block"], &eff117["reports"], &eff117["reason"]),
        (&json!(false), &json!(1), &json!("listed for review"))
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

/// Heuristic reports of exact claims, one per closer: `EFF101` (a branch skips the wait: code),
/// `EFF103` (a loop over a library result the catalog does not size: heki's catalog), `OPT113`
/// (items of unknown type: two annotations), and `OPT110` (a loop the code counts: a condition of
/// the exact tier, nothing to apply). `handle_quietly`'s report is suppressed.
const HEURISTIC_CLAIMS: [(&str, &str); 4] = [
    (
        "app/waits.py",
        "import time\n\n\ndef pause(flag):\n    if flag:\n        return None\n    time.sleep(1)\n\n\n\
         async def handle(flag):\n    pause(flag)\n\n\nasync def handle_quietly(flag):\n    \
         pause(flag)  # heki: ignore[EFF101]\n",
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
        json!([{"rule": "EFF101", "reports": 1}, {"rule": "EFF103", "reports": 1},
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
    assert_eq!(
        by_cause[0]["rules"],
        json!([{"rule": "EFF101", "reports": 1, "only_open": 1}])
    );
    assert_eq!(
        by_cause[0]["examples"],
        json!([{"path": "app/waits.py", "line": 11, "column": 5, "rule": "EFF101",
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
    // Nothing of this section enters the block: it holds the rules the rules section enables.
    let enabled: Vec<&Value> = (report["rules"].as_array().unwrap().iter())
        .filter(|r| r["in_block"] == true)
        .map(|r| &r["code"])
        .collect();
    assert_eq!(
        report["config"],
        json!({"lint": {"extend-select": enabled}}),
        "{report:#}"
    );
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
            "  4 heuristic report(s) of exact claims: EFF101 1, EFF103 1, OPT110 1, OPT113 1",
            "  premise: cause                                 reports  only open  closed by",
            "  Every return: not-made-on-every-return               1          1  code",
        ],
        "{text}"
    );
    assert_eq!(lines[4], "    e.g. app/waits.py:11 EFF101", "{text}");
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
    let tried = |eff101: u64, eff103: u64| json!([{"exact": eff101, "heuristic": 0}, {"exact": eff103, "heuristic": 0}]);
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
    // The module also keeps an effect boundary (contract candidates), tried after the catalog,
    // and a parameter it only reads (a suggestion), tried first.
    assert_eq!(
        report["runs"],
        json!([{"label": "discovery"}, {"label": "annotation-trial"},
               {"label": "hypothetical-catalog"}, {"label": "contract-trial"},
               {"label": "verification"}])
    );
    // No catalog entry enters the block, and no count of reports takes the trial's.
    let keys: Vec<&String> = report["config"].as_object().unwrap().keys().collect();
    assert_eq!(keys, ["effect-boundaries"]);
    assert_eq!(report["reports_now"]["shown"], json!(0));
    // `EFF101` withholds no report here, so it is not listed.
    assert!(
        report["rules"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["code"] != "EFF101"),
        "{report:#}"
    );
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
        "    name                   calls  async  loops  if it waits     if it sends a query",
        "    stripe.Charge.create       1      1      0  EFF101 1 exact  -",
        "    stripe.Charge.capture      1      0      1  -               EFF103 1 exact",
        "    and 1 more name(s) whose trial changes no report (stripe.Refund.create_async)",
        "    requests.utils.quote      1      1      0  EFF101 1 exact  -",
        "  if it waits, if it sends a query: what heki check would print if the call waited on \
         the network or sent a database query; a trial, not a fact (Notes)",
        "  1 function(s) with an A1 hazard, 0 of them too large to model: the Soundness lane \
         proves nothing in them (`heki assumptions` lists them)",
    ] {
        assert!(out.lines().any(|l| l == line), "{line}\n{out}");
    }
    assert!(
        out.contains("(if they do, report them at https://github.com/koxudaxi/heki/issues)"),
        "{out}"
    );
    // A name whose trial changes no report comes after those whose trial changes one, however
    // busy it is (on the event loop in test code, where `EFF101` reports nothing); the JSON keeps
    // its order by reach.
    p.write(
        "tests/test_refunds.py",
        "import stripe\n\n\nasync def test_refunds():\n    stripe.Refund.create_async(amount=1)\n    \
         stripe.Refund.create_async(amount=2)\n",
    );
    let (_, busy, _) = recommend_json(&p, &[]);
    assert_eq!(
        busy["blind_spots"]["uncatalogued"][0]["symbols"][0]["symbol"],
        "stripe.Refund.create_async"
    );
    let (_, out, _) = p.heki(&["recommend"]);
    assert!(
        out.contains(
            "\n    stripe.Charge.create       1      1      0  EFF101 1 exact  -\n    \
             stripe.Charge.capture      1      0      1  -               EFF103 1 exact\n    \
             and 1 more name(s) whose trial changes no report (stripe.Refund.create_async)\n"
        ),
        "{out}"
    );
    std::fs::remove_file(p.dir.join("tests/test_refunds.py")).unwrap();
    // What the trial counts is what `heki check` prints with such an entry.
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.ecosystem]]\nsymbol = \"stripe.Charge.create\"\n\
         effects = [\"blocking_net\", \"db_query\"]\n",
    );
    let shown = checked(&p, &["--select", "EFF101,EFF103"]);
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

#[test]
fn a_block_setting_src_is_not_verified_on_the_files_as_they_are_named_now() {
    let p = subdirectory_app("recommend_source_root_unverified");
    p.write(
        "backend/tasks.py",
        "import asyncio\n\n\nasync def work():\n    await asyncio.sleep(1)\n\n\n\
         async def start():\n    task = asyncio.create_task(work())\n    await asyncio.sleep(0)\n",
    );
    let (_, report, _) = recommend_json(&p, &[]);
    assert_eq!(
        report["config"],
        json!({"lint": {"extend-select": ["EFF107"]}, "src": ["backend"]})
    );
    assert_eq!(
        report["runs"],
        json!([{"label": "discovery"}, {"label": "source-roots"}])
    );
    assert!(
        report["notes"].as_array().unwrap().contains(&json!(
            "the block sets src, which renames modules and resolves imports heki does not resolve \
             now: the counts of the block's rules are of the files as they are named now; run \
             heki recommend again with it for the counts under it"
        )),
        "{report:#}"
    );
    assert_follows_the_schema(&report);
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
    .write("backend/api/main.py", &p.read("backend/main.py"))
    .write("backend/jobs.py", "def run():\n    pass\n")
    .write("backend/broken.py", "def broken(:\n")
    .write("scripts/broken.py", "def broken(:\n");
    let paths = ["backend/api", "backend/config"];
    let (_, report, _) = recommend_json(&p, &paths);
    assert_eq!(
        report["analyzed"]["left_out"]["files"],
        json!([
            "backend/broken.py",
            "backend/jobs.py",
            "backend/main.py",
            "scripts/broken.py"
        ])
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
fn a_source_root_stays_out_when_a_workspace_members_package_would_lose_its_name() {
    let p = Project::new("recommend_source_root_workspace_member");
    p.write(
        "pyproject.toml",
        "[tool.uv.workspace]\nmembers = [\"packages/*\"]\n",
    )
    .write("packages/api/pyproject.toml", "")
    .write("packages/api/acme_api/__init__.py", "")
    .write(
        "packages/api/acme_api/io.py",
        "import time\n\n\ndef get():\n    time.sleep(1)\n",
    )
    // A `src` setting replaces the detection of the members' packages too: below the block's
    // roots this import would name nothing.
    .write(
        "app.py",
        "import acme_api.io\nimport helpers\n\n\nasync def main():\n    acme_api.io.get()\n    \
         helpers.run()\n",
    )
    .write("tools/lib/helpers.py", "def run():\n    pass\n");
    let (_, report, _) = recommend_json(&p, &[]);
    let root = &report["blind_spots"]["source_roots"][0];
    assert_eq!(
        (&root["directory"], &root["in_block"], &root["reason"]),
        (
            &json!("tools/lib"),
            &json!(false),
            &json!(
                "`acme_api.io` in app.py would name no project module, not \
                 packages/api/acme_api/io.py"
            )
        )
    );
    assert_eq!(report["config"].get("src"), None);
    assert_follows_the_schema(&report);
    assert_eq!(waits_reported(&p), ["app.py"]);
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
        (&symbols[0]["symbol"], &symbols[0]["if_waits"]["exact"]),
        (&json!("stripe.a"), &json!(1))
    );
    assert_eq!(
        (&symbols[1]["symbol"], &symbols[1]["if_waits"]["exact"]),
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
    // Test code comes after the rest whatever its reach, as `relax-async-in-tests` judges it: a
    // package directory named `test_*` is no test location.
    p.write(
        "tests/test_jobs.py",
        "async def test_poll(client):\n    client.run()\n    client.stop()\n",
    )
    .write(
        "test_utils/helpers.py",
        "async def help_poll(client):\n    client.run()\n    client.stop()\n    client.wait()\n",
    );
    let (_, report, _) = recommend_json(&p, &[]);
    let listed: Vec<&str> = (report["blind_spots"]["unresolved"]
        .as_array()
        .unwrap()
        .iter())
    .take(3)
    .map(|u| u["function"].as_str().unwrap())
    .collect();
    assert_eq!(
        listed,
        [
            "test_utils.helpers.help_poll",
            "tests.test_jobs.test_poll",
            "app.job_11"
        ]
    );
    let (_, out, _) = p.heki(&["recommend"]);
    let rows: Vec<&str> = (out.lines())
        .skip_while(|l| !l.starts_with("  calls heki does not resolve: "))
        .skip(2)
        .take_while(|l| l.starts_with("    "))
        .map(|l| l.split_whitespace().next().unwrap())
        .collect();
    let mut jobs: Vec<String> = vec!["test_utils.helpers.help_poll".into(), "app.job_11".into()];
    jobs.extend((0..8).map(|i| format!("app.job_{i:02}")));
    jobs.push("and".into());
    assert_eq!(rows, jobs, "{out}");
    assert!(
        out.contains("\n    and 4 more, 1 of them in test code\n"),
        "{out}"
    );
}

#[test]
fn long_lists_fold_into_counts_in_the_text() {
    // A project below the working directory: paths are written below its root.
    let p = Project::new("recommend_long_lists");
    let calls = |package: &str, names: &[String]| -> String {
        let body: String = names
            .iter()
            .map(|n| format!("    {package}.{n}()\n"))
            .collect();
        format!("def use_{package}():\n{body}\n\n")
    };
    let alpha: Vec<String> = (0..60).map(|i| format!("f{i:02}")).collect();
    let beta: Vec<String> = (0..10).map(|i| format!("g{i}")).collect();
    let jobs: String = (0..52)
        .map(|i| format!("def j{i:02}(client):\n    client.run()\n\n\n"))
        .collect();
    p.write("proj/pyproject.toml", "[project]\nname = \"proj\"\n")
        .write("proj/app/__init__.py", "")
        .write(
            "proj/app/calls.py",
            &format!(
                "import alpha\nimport beta\n\n\n{}{}",
                calls("alpha", &alpha),
                calls("beta", &beta)
            ),
        )
        .write("proj/app/jobs.py", &jobs)
        .write(
            "proj/tests/test_jobs.py",
            "async def test_job(client):\n    client.run()\n",
        );
    let (_, report, _) = recommend_json(&p, &["proj"]);
    assert_eq!(report["root"], "proj");
    let totals = &report["blind_spots"]["totals"];
    assert_eq!(
        (
            &totals["tried"],
            &totals["not_tried"],
            &totals["unresolved_groups"]
        ),
        (&json!(64), &json!(6), &json!(53))
    );
    let (_, out, _) = p.heki(&["recommend", "proj"]);
    // 50 names of `alpha` are listed, and 10 of `beta`, 6 of which are past the 64 tried; no
    // trial changes a report, so there is no table.
    for line in [
        "    54 name(s) whose trial changes no report (alpha.f00, alpha.f01, alpha.f02, ...)",
        "    6 name(s) not tried, past the first 64 by reach",
        "    and 10 more name(s), not listed",
        // The test function is listed first by reach and printed after the rest; the groups
        // past the list are not known.
        "    and 43 more, at least 1 of them in test code",
    ] {
        assert!(out.lines().any(|l| l == line), "{line}\n{out}");
    }
    assert!(!out.contains("a trial, not a fact"), "{out}");
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
        .map(|s| (s["symbol"].clone(), s["if_waits"]["exact"].clone()))
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
        json!({"exact": 0, "heuristic": 1})
    );
    assert_follows_the_schema(&report);
}

/// The report lists the entries of the configured contracts that match nothing in the analyzed
/// files, as `heki check` warns about them, whatever `lint.unmatched-contract-entries` says.
#[test]
fn the_report_lists_the_contract_entries_that_match_nothing() {
    let p = Project::new("recommend_unmatched_entries");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nunmatched-contract-entries = \"ignore\"\n\n\
         [[tool.heki.write-owners]]\nname = \"fields\"\n\
         targets = [\"pkg.model.Field.kind\", \"pkg.model.Field.knd\"]\n\
         allowed-writers = [\"pkg.model:*\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write(
        "pkg/model.py",
        "class Field:\n    def __init__(self) -> None:\n        self.kind = \"str\"\n",
    );
    let (code, report, err) = recommend_json(&p, &[]);
    assert_eq!(code, 0, "{err}");
    assert_eq!(
        report["unmatched_entries"],
        json!([{
            "kind": "write-owners",
            "key": "targets",
            "entry": "pkg.model.Field.knd",
            "contracts": ["fields"],
            "names_no": "attribute, method, or `__dict__` key of `pkg.model.Field`",
            "nearest": ["pkg.model.Field.kind"],
            "entry_at": [{"file": "pyproject.toml", "line": 9}],
            "contracts_at": [{"file": "pyproject.toml", "line": 7}],
        }])
    );
    assert_follows_the_schema(&report);
    let (_, text, _) = p.heki(&["recommend"]);
    assert!(
        text.contains("\nContract entries that match nothing\n  - `targets` entry `pkg.model.Field.knd` of write-owners `fields` (from pyproject.toml:9; contract at pyproject.toml:7) names no attribute, method, or `__dict__` key of `pkg.model.Field` in the analyzed files; `pkg.model.Field.kind` exists\n"),
        "{text}"
    );
}
