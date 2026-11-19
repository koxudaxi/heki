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
