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
    assert!(String::from_utf8_lossy(&out.stderr).contains("`--fix`, `--diff`, `--baseline`"));
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
            && stdout.contains("5 |     time.sleep(1)"),
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
        stdout.contains("pkg/jobs.py:17:5: EFF101 `async def _job` calls `time.sleep`"),
        "{stdout}"
    );
}

/// Replace a file's content without changing its modification time, atomically (the watch
/// never sees the change).
fn write_unnoticed(path: &std::path::Path, content: &[u8]) {
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
fn watch_skips_unreadable_callers_and_deleted_unindexed_files() {
    let p = Project::new("watch_unreadable");
    let caller = "from pkg.util import helper\n\n\nasync def handler():\n    helper()\n";
    p.write("pkg/__init__.py", "")
        .write("pkg/util.py", "def helper():\n    return 1\n")
        .write("pkg/api.py", caller)
        .write("pkg/api2.py", caller)
        .write("pkg/broken.py", "def f(:\n");
    let (child, rx) = spawn_watch(&p, &["--output-format", "concise"], "2");
    wait_for(&rx, "syntax error");
    wait_for(&rx, "[heki watch] 0 diagnostic(s) in 4 file(s)");
    // The file with the syntax error was never indexed; deleting it changes nothing else.
    std::fs::remove_file(p.dir.join("pkg/broken.py")).unwrap();
    wait_for(&rx, "[heki watch] 0 diagnostic(s) in 4 file(s)");
    // The callers change behind the watch's back (same modification time): one is no
    // longer UTF-8, the other no longer parses. When their callee starts blocking, the first
    // cannot be re-checked and is skipped; the second reports its syntax error.
    write_unnoticed(&p.dir.join("pkg/api.py"), b"\xff\xfe");
    write_unnoticed(&p.dir.join("pkg/api2.py"), b"def handler(:\n");
    p.write(
        "pkg/util.py",
        "import time\n\n\ndef helper():\n    time.sleep(1)\n",
    );
    let lines = wait_for(&rx, "[heki watch]");
    assert!(
        lines
            .iter()
            .any(|l| l.starts_with("error: pkg/api2.py:1:13: syntax error")),
        "{lines:#?}"
    );
    assert!(
        !lines.iter().any(|l| l.contains("pkg/api.py")),
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
fn watch_reports_check_errors_of_every_file() {
    let p = Project::new("watch_check_errors");
    p.write("pkg/__init__.py", "")
        .write("pkg/util.py", "def helper():\n    return 1\n")
        .write(
            "pkg/api.py",
            "from pkg.util import helper\n\n\nasync def handler():\n    helper()\n",
        )
        // A configuration error only the rules see: every file fails to check.
        .write(
            "heki.toml",
            "[[write-owners]]\nname = \"store\"\ntargets = [\"fields\"]\n",
        );
    let (child, rx) = spawn_watch(&p, &["--output-format", "concise"], "1");
    let lines = wait_for(&rx, "[heki watch] 0 diagnostic(s) in 0 file(s)");
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.contains("invalid write-owners target `fields`"))
            .count(),
        3,
        "{lines:#?}"
    );
    // The edited file fails the same way, and so does the caller its new effect reaches.
    p.write(
        "pkg/util.py",
        "import time\n\n\ndef helper():\n    time.sleep(1)\n",
    );
    let lines = wait_for(&rx, "[heki watch]");
    assert_eq!(
        lines
            .iter()
            .filter(|l| l.contains("invalid write-owners target `fields`"))
            .count(),
        2,
        "{lines:#?}"
    );
    let out = child.wait_with_output().unwrap();
    assert_eq!(out.status.code(), Some(2));
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
