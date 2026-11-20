//! `heki check --watch`: incremental re-checks of changed files and their callers, deletions,
//! syntax errors, and configuration reloads (`HEKI_WATCH_MAX_RUNS` ends the session).

mod common;

use common::Project;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

fn wait_for(rx: &Receiver<String>, needle: &str) -> Vec<String> {
    let mut seen = Vec::new();
    loop {
        let line = rx
            .recv_timeout(Duration::from_secs(20))
            .unwrap_or_else(|_| panic!("no `{needle}` in {seen:#?}"));
        let hit = line.contains(needle);
        seen.push(line);
        if hit {
            return seen;
        }
    }
}

#[test]
fn watch_rechecks_changed_files_and_affected_callers() {
    let p = Project::new("watch");
    p.write("pkg/__init__.py", "")
        .write("pkg/util.py", "def helper():\n    return 1\n")
        .write(
            "pkg/api.py",
            "from pkg.util import helper\n\n\nasync def handler():\n    helper()\n",
        );
    let mut child = Command::new(env!("CARGO_BIN_EXE_heki"))
        .args(["check", "--watch", "--output-format", "concise"])
        .current_dir(&p.dir)
        .env("HEKI_WATCH_MAX_RUNS", "4")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    wait_for(&rx, "[heki watch] 0 diagnostic(s) in 3 file(s)");
    // `helper` starts blocking: `handler` in another file is re-checked.
    p.write(
        "pkg/util.py",
        "import time\n\n\ndef helper():\n    time.sleep(1)\n",
    );
    wait_for(&rx, "[heki watch] 1 diagnostic(s) in 3 file(s)");
    std::fs::remove_file(p.dir.join("pkg/api.py")).unwrap();
    wait_for(&rx, "[heki watch] 0 diagnostic(s) in 2 file(s)");
    p.write("pkg/util.py", "def helper(:\n");
    wait_for(&rx, "syntax error");
    wait_for(&rx, "[heki watch] 0 diagnostic(s) in 1 file(s)");
    // A new configuration file reloads the workspace; the syntax error is still reported.
    p.write("heki.toml", "[lint]\nignore = [\"EFF101\"]\n");
    let lines = wait_for(&rx, "[heki watch]");
    assert!(
        lines.iter().any(|l| l.contains("syntax error")),
        "{lines:#?}"
    );
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("pkg/api.py:5:5: EFF101 "),
        "the re-check printed the caller's diagnostic:\n{stdout}"
    );
}

#[test]
fn watch_exits_with_the_last_reports_status() {
    let p = Project::new("watch_status");
    p.write(
        "a.py",
        "def f(items):\n    x = []\n    for i in items:\n        x = x + [i]\n    return x\n",
    );
    let out = Command::new(env!("CARGO_BIN_EXE_heki"))
        .args(["check", "--watch", "--fix", "--output-format", "json"])
        .current_dir(&p.dir)
        .env("HEKI_WATCH_MAX_RUNS", "0")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("`--fix`, `--diff`, and `--update-baseline`")
    );
    let diags: Vec<serde_json::Value> = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(diags[0]["rule_code"], "OPT101");
    // Invalid configuration is reported before watching starts.
    p.write("heki.toml", "[lint]\nselect = [\"NOPE1\"]\n");
    let out = Command::new(env!("CARGO_BIN_EXE_heki"))
        .args(["check", "--watch"])
        .current_dir(&p.dir)
        .env("HEKI_WATCH_MAX_RUNS", "0")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
}

/// `heki check --watch <args>` with `HEKI_WATCH_MAX_RUNS=<max_runs>`; stderr lines arrive on
/// the channel.
fn spawn_watch(
    p: &Project,
    args: &[&str],
    max_runs: &str,
) -> (std::process::Child, Receiver<String>) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_heki"))
        .args(["check", "--watch"])
        .args(args)
        .current_dir(&p.dir)
        .env("HEKI_WATCH_MAX_RUNS", max_runs)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                return;
            }
        }
    });
    (child, rx)
}

#[test]
fn watch_full_output_idle_polls_and_a_clean_exit() {
    let p = Project::new("watch_full");
    p.write(
        "a.py",
        "import time\n\n\nasync def handler():\n    time.sleep(1)\n",
    );
    let (child, rx) = spawn_watch(&p, &[], "1");
    wait_for(&rx, "[heki watch] 1 diagnostic(s) in 1 file(s)");
    // Polls without changes re-check nothing.
    std::thread::sleep(Duration::from_millis(600));
    p.write("a.py", "async def handler():\n    return 1\n");
    wait_for(&rx, "[heki watch] 0 diagnostic(s) in 1 file(s)");
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0), "the last report is clean");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.starts_with("a.py:5:5: EFF101 (async-blocking-call) [error]")
            && stdout.contains("5 |     time.sleep(1)")
            && stdout.contains("\n  = help: use `await asyncio.sleep(...)`"),
        "full output with the source line:\n{stdout}"
    );
}

const JOBS: &str = "import time\n\n\nclass Worker:\n    def __init__(self):\n        self.count = 0\n\n    def tick(self):\n        self.count += 1\n\n\ndef bump(w: Worker):\n    w.tick()\n\n\nasync def _job():\n    time.sleep(1)\n\n\ndef helper():\n    return 1\n\n\ndef drive():\n    _job().send(None)\n    helper()\n    helper()\n";

#[test]
fn watch_rechecks_a_coroutine_that_stops_being_sync_driven() {
    let p = Project::new("watch_sync_driven");
    p.write("pkg/__init__.py", "")
        .write("pkg/jobs.py", JOBS)
        .write("pkg/entry.py", "def other():\n    return 1\n");
    let (child, rx) = spawn_watch(&p, &["--output-format", "concise"], "2");
    // `_job` is only driven by hand (`.send(None)`): relaxed.
    wait_for(&rx, "[heki watch] 0 diagnostic(s) in 3 file(s)");
    // An `await _job()` elsewhere makes it an event-loop coroutine: `jobs.py` is re-checked.
    p.write(
        "pkg/entry.py",
        "from pkg.jobs import _job\n\n\nasync def entry():\n    await _job()\n",
    );
    wait_for(&rx, "[heki watch] 1 diagnostic(s) in 3 file(s)");
    p.write("pkg/entry.py", "def other():\n    return 1\n");
    wait_for(&rx, "[heki watch] 0 diagnostic(s) in 3 file(s)");
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("pkg/jobs.py:17:5: EFF101 `_job` reaches the blocking call `time.sleep`"),
        "{stdout}"
    );
}

/// Replace a file's content atomically, keeping its modification time (as file synchronization
/// tools do).
fn replace_keeping_mtime(path: &std::path::Path, content: &[u8]) {
    let stamp = std::fs::metadata(path).unwrap().modified().unwrap();
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content).unwrap();
    std::fs::File::options()
        .write(true)
        .open(&tmp)
        .unwrap()
        .set_modified(stamp)
        .unwrap();
    std::fs::rename(&tmp, path).unwrap();
}

#[test]
fn watch_notices_replacements_that_keep_the_modification_time() {
    let p = Project::new("watch_unreadable");
    let caller = "from pkg.util import helper\n\n\nasync def handler():\n    helper()\n";
    p.write("pkg/__init__.py", "")
        .write("pkg/util.py", "def helper():\n    return 1\n")
        .write("pkg/api.py", caller)
        .write("pkg/api2.py", caller)
        .write("pkg/broken.py", "def f(:\n");
    let (child, rx) = spawn_watch(&p, &["--output-format", "concise"], "4");
    wait_for(&rx, "syntax error");
    wait_for(&rx, "[heki watch] 0 diagnostic(s) in 4 file(s)");
    // The file with the syntax error was never indexed; deleting it changes nothing else.
    std::fs::remove_file(p.dir.join("pkg/broken.py")).unwrap();
    wait_for(&rx, "[heki watch] 0 diagnostic(s) in 4 file(s)");
    // The helper is replaced by a blocking one with the old modification time.
    replace_keeping_mtime(
        &p.dir.join("pkg/util.py"),
        b"import time\n\n\ndef helper():\n    time.sleep(1)\n",
    );
    wait_for(&rx, "[heki watch] 2 diagnostic(s) in 4 file(s)");
    // So are the callers: one is no longer UTF-8, the other no longer parses. Both are
    // reported, as `heki check` reports them. One replacement per re-check: two quick
    // replacements may land in one re-check or in two.
    replace_keeping_mtime(&p.dir.join("pkg/api.py"), b"\xff\xfe");
    let lines = wait_for(&rx, "[heki watch] 1 diagnostic(s) in 3 file(s)");
    assert!(
        lines
            .iter()
            .any(|l| l == "error: pkg/api.py: stream did not contain valid UTF-8"),
        "{lines:#?}"
    );
    replace_keeping_mtime(&p.dir.join("pkg/api2.py"), b"def handler(:\n");
    let lines = wait_for(&rx, "[heki watch] 0 diagnostic(s) in 2 file(s)");
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("error: pkg/api2.py:1:13: syntax error")),
        "{lines:#?}"
    );
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(2));
}

#[test]
fn watch_follows_stubs_deleted_callees_and_files_that_stop_being_utf8() {
    let p = Project::new("watch_stub_delete");
    let calls_helper =
        |def: &str| format!("from pkg.util import helper\n\n\n{def}\n    helper()\n");
    p.write("pkg/__init__.py", "")
        .write(
            "pkg/util.py",
            "import time\n\n\ndef helper():\n    time.sleep(1)\n",
        )
        .write("pkg/api.py", &calls_helper("async def handler():"))
        // `pkg.jobs.job` is defined by the module and by its stub, which wins (the module's
        // names resolve through the stub's declarations, hence its import).
        .write("pkg/jobs.py", &calls_helper("def job():"))
        .write(
            "pkg/jobs.pyi",
            "from pkg.util import helper\n\n\ndef job() -> None: ...\n",
        );
    let (child, rx) = spawn_watch(&p, &["--output-format", "concise"], "5");
    wait_for(&rx, "[heki watch] 1 diagnostic(s) in 5 file(s)");
    // Editing the sibling stub does not mask `pkg/jobs.py`.
    p.write("pkg/jobs.pyi", "from pkg.util import helper\n\nx: int\n");
    wait_for(&rx, "[heki watch] 1 diagnostic(s) in 5 file(s)");
    // An edit of `helper` keeps `handler` reported.
    p.write(
        "pkg/util.py",
        "import time\n\n\ndef helper():\n    time.sleep(2)\n",
    );
    wait_for(&rx, "[heki watch] 1 diagnostic(s) in 5 file(s)");
    // Removing a sibling `.pyi` stub keeps the `.py` module's declarations alive.
    std::fs::remove_file(p.dir.join("pkg/jobs.pyi")).unwrap();
    wait_for(&rx, "[heki watch] 1 diagnostic(s) in 4 file(s)");
    // Deleting the callee re-checks its callers: `handler` no longer blocks.
    std::fs::remove_file(p.dir.join("pkg/util.py")).unwrap();
    wait_for(&rx, "[heki watch] 0 diagnostic(s) in 3 file(s)");
    // A changed file that is no longer UTF-8 is a read error.
    std::fs::write(p.dir.join("pkg/api.py"), b"\xff\xfe").unwrap();
    let lines = wait_for(&rx, "[heki watch]");
    assert!(
        lines
            .iter()
            .any(|l| l == "error: pkg/api.py: stream did not contain valid UTF-8"),
        "{lines:#?}"
    );
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(
        String::from_utf8(out.stdout)
            .unwrap()
            .matches(" EFF101 ")
            .count(),
        4,
        "reported by the first check and the three re-checks before the deletion"
    );
}

#[test]
fn watch_fails_like_check_on_invalid_rule_options() {
    let p = Project::new("watch_check_errors");
    p.write("pkg/__init__.py", "")
        .write("pkg/util.py", "def helper():\n    return 1\n")
        // A configuration error only the rules see: `heki check` fails before reporting.
        .write(
            "heki.toml",
            "[[write-owners]]\nname = \"store\"\ntargets = [\"fields\"]\n",
        );
    let out = Command::new(env!("CARGO_BIN_EXE_heki"))
        .args(["check", "--watch"])
        .current_dir(&p.dir)
        .env("HEKI_WATCH_MAX_RUNS", "0")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("heki error: invalid write-owners target `fields`"),
        "{stderr}"
    );
    assert_eq!(p.check(&[]).0, 2);
}

#[test]
fn watch_validates_severities_and_loads_the_summary_snapshot() {
    let p = Project::new("watch_snapshot");
    p.write("pkg/__init__.py", "")
        .write("pkg/api.py", "def add(items):\n    return items\n");
    assert!(
        Command::new(env!("CARGO_BIN_EXE_heki"))
            .args(["snapshot", "--write"])
            .current_dir(&p.dir)
            .status()
            .unwrap()
            .success()
    );
    p.write(
        "pkg/api.py",
        "def add(items):\n    items.append(1)\n    return items\n",
    );
    let (child, rx) = spawn_watch(
        &p,
        &["--extend-select", "MUT103", "--output-format", "concise"],
        "0",
    );
    wait_for(&rx, "[heki watch] 1 diagnostic(s) in 2 file(s)");
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .starts_with("pkg/api.py:1:1: MUT103 `add` gained side effects"),
    );
    // An unknown `lint.severity` key is a configuration error before watching starts.
    p.write("heki.toml", "[lint.severity]\nNOPE1 = \"warning\"\n");
    let out = Command::new(env!("CARGO_BIN_EXE_heki"))
        .args(["check", "--watch"])
        .current_dir(&p.dir)
        .env("HEKI_WATCH_MAX_RUNS", "0")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown rule `NOPE1` in lint.severity"));
}

#[test]
fn watch_reports_what_check_reports() {
    // A call chain through three files: the positions of the hops in the files a re-check does
    // not cover come from their text, as in `heki check`.
    let p = Project::new("watch_like_check");
    p.write("pkg/__init__.py", "")
        .write(
            "pkg/low.py",
            "import requests\n\n\ndef fetch(url):\n    return requests.get(url)\n",
        )
        .write(
            "pkg/mid.py",
            "from pkg.low import fetch\n\n\ndef helper(url):\n    return fetch(url)\n",
        )
        .write(
            "pkg/top.py",
            "from pkg.mid import helper\n\n\nasync def handler(url):\n    return helper(url)\n",
        );
    let (code, check, _) = p.check(&["--output-format", "json"]);
    assert_eq!(code, 1);
    let watch = Command::new(env!("CARGO_BIN_EXE_heki"))
        .args(["check", "--watch", "--output-format", "json"])
        .current_dir(&p.dir)
        .env("HEKI_WATCH_MAX_RUNS", "0")
        .output()
        .unwrap();
    let json = |text: &[u8]| -> serde_json::Value { serde_json::from_slice(text).unwrap() };
    assert_eq!(json(&watch.stdout), json(check.as_bytes()));
}

const LEVELS: &str = "LEVELS = [\"low\", \"mid\", \"high\"]\n";
const LEVELS_GROWN: &str =
    "LEVELS = [\"low\", \"mid\", \"high\"]\n\n\ndef add(level):\n    LEVELS.append(level)\n";
const LEVELS_USER: &str = "from pkg.levels import LEVELS\n\n\ndef ranks(xs):\n    out = []\n    for x in xs:\n        out.append(LEVELS.index(x))\n    return out\n";

#[test]
fn watch_rechecks_the_user_of_a_small_display_in_another_file() {
    // `OPT112` skips scans of a small display; whether another module's list is one comes
    // from that module's declarations, so re-checking only the user (the watch re-checks the
    // files a change affects) reports what checking the whole project reports.
    let p = Project::new("watch_small_display");
    p.write("pkg/__init__.py", "")
        .write("pkg/levels.py", LEVELS)
        .write("pkg/user.py", LEVELS_USER);
    let (child, rx) = spawn_watch(&p, &["--output-format", "concise"], "2");
    wait_for(&rx, "[heki watch] 0 diagnostic(s) in 3 file(s)");
    p.write("pkg/levels.py", LEVELS_GROWN);
    wait_for(&rx, "[heki watch] 1 diagnostic(s) in 3 file(s)");
    let (code, check, _) = p.check(&["--output-format", "concise"]);
    assert_eq!(code, 1, "{check}");
    assert!(check.starts_with("pkg/user.py:7:20: OPT112 "), "{check}");
    p.write("pkg/levels.py", LEVELS);
    wait_for(&rx, "[heki watch] 0 diagnostic(s) in 3 file(s)");
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert_eq!(
        stdout, check,
        "the watch's re-check of the user equals `heki check`"
    );
}

const SORT_LOW: &str = "def sort_values(values):\n    values.sort()\n";
const SORT_MID: &str =
    "from pkg.low import sort_values\n\n\ndef relay(values):\n    sort_values(values)\n";
const SORT_STATS: &str = "from collections.abc import Sequence\n\nfrom pkg.mid import relay\n\n\ndef median(values: Sequence[float]) -> float:\n    relay(values)\n    return values[len(values) // 2]\n";

#[test]
fn watch_rechecks_a_call_chain_whose_end_moves() {
    // `MUT110`'s call chain ends at the first function whose own body changes the parameter:
    // when the middle helper starts changing it too, its summary stays the same, but the chain
    // of the report two files up loses its last hop.
    let p = Project::new("watch_chain_end");
    p.write("pkg/__init__.py", "")
        .write("pkg/low.py", SORT_LOW)
        .write("pkg/mid.py", SORT_MID)
        .write("pkg/stats.py", SORT_STATS);
    let (child, rx) = spawn_watch(&p, &[], "1");
    wait_for(&rx, "[heki watch] 1 diagnostic(s) in 4 file(s)");
    p.write("pkg/mid.py", &format!("{SORT_MID}    values[0] = 0\n"));
    wait_for(&rx, "[heki watch] 1 diagnostic(s) in 4 file(s)");
    let (code, check, _) = p.check(&[]);
    assert_eq!(code, 1, "{check}");
    assert_eq!(
        check.matches("  = related: ").count(),
        1,
        "the chain ends in `relay`:\n{check}"
    );
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("  = related: pkg/mid.py:5:5: `relay` calls `pkg.low.sort_values`"),
        "the first report goes down to `sort_values`:\n{stdout}"
    );
    assert!(
        stdout.ends_with(&check),
        "the watch's re-check equals `heki check`:\n{stdout}"
    );
}

#[test]
fn watch_reloads_when_src_becomes_a_package() {
    let p = Project::new("watch_src_layout");
    p.write(
        "src/pkg/util.py",
        "import time\n\n\ndef wait():\n    time.sleep(1)\n",
    )
    .write(
        "caller.py",
        "from pkg.util import wait\n\n\nasync def call():\n    wait()\n",
    );
    let (child, rx) = spawn_watch(&p, &["--output-format", "concise"], "1");
    // `src/` is a source root: the helper is `pkg.util`.
    wait_for(&rx, "[heki watch] 1 diagnostic(s) in 2 file(s)");
    // A package itself, `src/` names its modules `src.*`: the import no longer reaches the helper.
    p.write("src/__init__.py", "");
    wait_for(&rx, "[heki watch] 0 diagnostic(s) in 3 file(s)");
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .contains("caller.py:5:5: EFF101 "),
    );
}

#[test]
fn watch_compares_with_the_ratchet_baseline_like_check() {
    let p = Project::new("watch_baseline");
    let blocking = "import time\n\n\nasync def handler():\n    time.sleep(1)\n";
    p.write("a.py", blocking);
    assert_eq!(p.check(&["--baseline", "b.toml", "--update-baseline"]).0, 0);
    let (child, rx) = spawn_watch(
        &p,
        &["--baseline", "b.toml", "--output-format", "concise"],
        "2",
    );
    let lines = wait_for(&rx, "[heki watch] 0 diagnostic(s) in 1 file(s)");
    assert!(!lines.iter().any(|l| l.contains("ignored")), "{lines:#?}");
    // A second call grows the entry: both calls are reported, as by `heki check`.
    p.write("a.py", &format!("{blocking}    time.sleep(2)\n"));
    wait_for(&rx, "[heki watch] 2 diagnostic(s) in 1 file(s)");
    // Allowing both in the baseline file reloads the workspace.
    p.write(
        "b.toml",
        &p.read("b.toml").replace("count = 1", "count = 2"),
    );
    assert_eq!(p.check(&["--baseline", "b.toml"]).0, 0);
    wait_for(&rx, "[heki watch] 0 diagnostic(s) in 1 file(s)");
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
}

#[test]
fn watch_keeps_reporting_errors_of_unchanged_files_and_forgets_unreadable_ones() {
    let p = Project::new("watch_persistent_errors");
    p.write("pkg/__init__.py", "")
        .write("pkg/bad.py", "def f(:\n")
        .write(
            "pkg/util.py",
            "import time\n\n\ndef helper():\n    time.sleep(1)\n",
        )
        .write(
            "pkg/api.py",
            "from pkg.util import helper\n\n\nasync def handler():\n    helper()\n",
        );
    let (child, rx) = spawn_watch(&p, &["--output-format", "concise"], "2");
    wait_for(&rx, "[heki watch] 1 diagnostic(s) in 3 file(s)");
    // Another file changes: the syntax error of the unchanged one is still there.
    p.write(
        "pkg/api.py",
        "from pkg.util import helper\n\n\nasync def handler():\n    helper()\n    helper()\n",
    );
    let lines = wait_for(&rx, "[heki watch] 2 diagnostic(s) in 3 file(s)");
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("error: pkg/bad.py:1:7: syntax error")),
        "{lines:#?}"
    );
    // The helper can no longer be read: like `heki check`, the project goes on without it, and
    // the caller no longer reaches the blocking call.
    std::fs::write(p.dir.join("pkg/util.py"), b"\xff\xfe").unwrap();
    let lines = wait_for(&rx, "[heki watch] 0 diagnostic(s) in 2 file(s)");
    assert!(
        lines.iter().any(|l| l.starts_with("error: pkg/util.py: ")),
        "{lines:#?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("error: pkg/bad.py:1:7: syntax error")),
        "{lines:#?}"
    );
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(p.check(&["--output-format", "concise"]).1, "");
}

#[test]
fn watch_rechecks_a_caller_saved_in_the_same_poll_as_its_callee() {
    let p = Project::new("watch_one_poll");
    let caller = "from z import wait\n\n\nasync def handler():\n    wait()\n";
    p.write("a.py", caller)
        .write("z.py", "def wait():\n    return 1\n");
    let (child, rx) = spawn_watch(&p, &["--output-format", "concise"], "1");
    wait_for(&rx, "[heki watch] 0 diagnostic(s) in 2 file(s)");
    // Both saved between two polls: `a.py` is analyzed first, against the old `wait`.
    p.write("a.py", &format!("# edited\n{caller}"))
        .write("z.py", "import time\n\n\ndef wait():\n    time.sleep(1)\n");
    wait_for(&rx, "[heki watch] 1 diagnostic(s) in 2 file(s)");
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn watch_survives_a_deleted_input_and_a_broken_configuration() {
    let p = Project::new("watch_recovers");
    let blocking = "import time\n\n\nasync def handler():\n    time.sleep(1)\n";
    p.write("api.py", blocking);
    let (child, rx) = spawn_watch(&p, &["api.py", "--output-format", "concise"], "4");
    wait_for(&rx, "[heki watch] 1 diagnostic(s) in 1 file(s)");
    // The watched file is gone for a while (a checkout, an editor's atomic save), then back.
    std::fs::remove_file(p.dir.join("api.py")).unwrap();
    wait_for(&rx, "[heki watch] 0 diagnostic(s) in 0 file(s)");
    p.write("api.py", blocking);
    wait_for(&rx, "[heki watch] 1 diagnostic(s) in 1 file(s)");
    // The configuration is malformed for a while: reported once, then fixed.
    p.write("pyproject.toml", "[project\n");
    let lines = wait_for(&rx, "[heki watch] 0 diagnostic(s) in 0 file(s)");
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("error: ") && l.contains("TOML")),
        "{lines:#?}"
    );
    // Polls that find the broken configuration unchanged report nothing.
    assert!(rx.recv_timeout(Duration::from_millis(600)).is_err());
    p.write("pyproject.toml", "[project]\nname = \"t\"\n");
    wait_for(&rx, "[heki watch] 1 diagnostic(s) in 1 file(s)");
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn watch_reads_changed_files_in_their_declared_encoding() {
    let p = Project::new("watch_latin1");
    let legacy = |body: &[u8]| {
        [
            b"# -*- coding: iso-8859-1 -*-\n# caf\xe9\n".as_slice(),
            body,
        ]
        .concat()
    };
    std::fs::write(
        p.dir.join("legacy.py"),
        legacy(b"async def handler():\n    pass\n"),
    )
    .unwrap();
    let (child, rx) = spawn_watch(&p, &["--output-format", "concise"], "1");
    wait_for(&rx, "[heki watch] 0 diagnostic(s) in 1 file(s)");
    std::fs::write(
        p.dir.join("legacy.py"),
        legacy(b"import time\n\n\nasync def handler():\n    time.sleep(1)  # \xe9\n"),
    )
    .unwrap();
    wait_for(&rx, "[heki watch] 1 diagnostic(s) in 1 file(s)");
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(
        String::from_utf8(out.stdout)
            .unwrap()
            .contains("legacy.py:7:5: EFF101 ")
    );
}

#[test]
fn watch_marks_the_functions_a_profile_shows_hot_like_check() {
    let p = Project::new("watch_profile");
    // A method with a size-dependent `OPT110` rewrite (`allowed` becomes a set).
    p.write(
        "app.py",
        "class A:\n    def run(self, n):\n        allowed = list(range(0, 50, 3))\n        hits = []\n        for p in range(n):\n            if p in allowed:\n                hits.append(p)\n        return hits\n",
    )
    .write("p.txt", "tid:1;app.py:A.run:4 95\ntid:1;other.py:idle:1 5\n");
    let args = ["--profile", "p.txt", "--output-format", "json"];
    let (code, check, _) = p.check(&args);
    assert_eq!(code, 1);
    assert!(
        check.contains("profile: `A.run` is on the stack in 95.0% of samples"),
        "{check}"
    );
    let watch = Command::new(env!("CARGO_BIN_EXE_heki"))
        .args(["check", "--watch"])
        .args(args)
        .current_dir(&p.dir)
        .env("HEKI_WATCH_MAX_RUNS", "0")
        .output()
        .unwrap();
    let json = |text: &[u8]| -> serde_json::Value { serde_json::from_slice(text).unwrap() };
    assert_eq!(json(&watch.stdout), json(check.as_bytes()));
}

#[test]
fn watch_drops_a_package_moved_away_with_its_modules() {
    let p = Project::new("watch_package_moved");
    p.write("main.py", "x = 1\n")
        .write("pkg/__init__.py", "")
        .write(
            "pkg/core.py",
            "import time\n\n\ndef helper():\n    time.sleep(1)\n",
        )
        .write(
            "pkg/users.py",
            "from pkg.core import helper\n\n\nasync def handler():\n    helper()\n",
        );
    let (child, rx) = spawn_watch(&p, &["--output-format", "concise"], "1");
    wait_for(&rx, "[heki watch] 1 diagnostic(s) in 4 file(s)");
    // One rename takes the callee and its caller out of the project at once (a hidden
    // directory is not discovered).
    std::fs::rename(p.dir.join("pkg"), p.dir.join(".pkg_old")).unwrap();
    wait_for(&rx, "[heki watch] 0 diagnostic(s) in 1 file(s)");
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(0));
}

// Unix only: the directory is made unreadable with permission bits; on Windows that takes an
// access control list, which std cannot set.
#[cfg(unix)]
#[test]
fn watch_reports_a_directory_it_cannot_list_until_it_can() {
    use std::os::unix::fs::PermissionsExt;
    let p = Project::new("watch_unlistable");
    p.write(
        "a.py",
        "import time\n\n\nasync def handler():\n    time.sleep(1)\n",
    )
    .write("sub/b.py", "x = 1\n");
    let locked = p.dir.join("sub");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let readable = std::fs::read_dir(&locked).is_ok(); // running as root
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    if readable {
        return;
    }
    let (child, rx) = spawn_watch(&p, &["--output-format", "concise"], "2");
    wait_for(&rx, "[heki watch] 1 diagnostic(s) in 2 file(s)");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let lines = wait_for(&rx, "[heki watch] 1 diagnostic(s) in 2 file(s)");
    // Reported once while the directory stays unlistable; the last results are kept.
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("error: failed to walk `.`") && l.contains("Permission denied")),
        "{lines:#?}"
    );
    assert!(rx.recv_timeout(Duration::from_millis(600)).is_err());
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    let lines = wait_for(&rx, "[heki watch] 1 diagnostic(s) in 2 file(s)");
    assert!(
        !lines.iter().any(|l| l.starts_with("error: ")),
        "{lines:#?}"
    );
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(1));
}
