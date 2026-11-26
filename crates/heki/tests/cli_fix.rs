//! `heki check --fix` / `--diff` and `heki snapshot` + `MUT103`, end to end
//! (docs/fixes.md, ARCHITECTURE.md section 3.3).
//!
//! The differential gate runs `tests/difftest/opt_rewrites.py` on real CPython before and after
//! the fixes and requires byte-identical output.

mod common;

use common::{Project, repo_file, run_python};

const CASES: &str = "tests/difftest/opt_rewrites.py";

/// Source of every top-level `def` in `src`, by name.
fn functions(src: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for chunk in src.split("\ndef ").skip(1) {
        let name = chunk.split('(').next().unwrap_or_default().to_owned();
        let body = chunk.split("\n\n\n").next().unwrap_or_default().to_owned();
        out.push((name, body));
    }
    out
}

fn unchanged_except(before: &str, after: &str, rewritten: &[&str]) {
    let (b, a) = (functions(before), functions(after));
    assert_eq!(b.len(), a.len());
    for ((name, bb), (_, ab)) in b.iter().zip(&a) {
        if rewritten.contains(&name.as_str()) {
            assert_ne!(bb, ab, "{name} should be rewritten");
        } else {
            assert_eq!(bb, ab, "{name} must not change");
        }
    }
}

const DEFAULT_FIXED: &[&str] = &[
    "pos_append",
    "pos_extend_pairs",
    "pos_conditional",
    "pos_nested",
    "pos_fresh_per_outer",
    "pos_while",
    "pos_list_call",
    "pos_try_handler",
    "pos_generator",
    "pos_dict_kv",
    "pos_dict_multi",
    "pos_dict_merge",
    "pos_dict_effects",
    "pos_set_union",
];

const SIZE_DEPENDENT_FIXED: &[&str] = &[
    "sd_membership",
    "sd_rebuild",
    "sd_dedup",
    "sd_queue",
    "sd_front_insert",
];

#[test]
fn default_fix_preserves_cpython_output() {
    let p = Project::new("fix_default");
    let original = repo_file(CASES);
    p.write("cases.py", &original);
    let before = run_python(&p.dir, "cases.py");
    assert!(before.contains("naive rewrites diverge: 7/7"));

    let (code, stdout, stderr) = p.check(&["--fix", "--output-format", "concise"]);
    assert!(
        stderr.contains("Fixed 15 issue(s) in 1 file(s)."),
        "{stderr}"
    );
    // The counterexamples and blocked shapes are still reported, never rewritten.
    assert_eq!(code, 1, "{stdout}");
    assert!(!stdout.contains("[safe fix"), "{stdout}");
    let fixed = p.read("cases.py");
    unchanged_except(&original, &fixed, DEFAULT_FIXED);
    assert!(fixed.contains("        x.append(i)\n") && fixed.contains("        d[k] = v\n"));
    assert!(
        fixed.contains("            x.append(None)\n"),
        "second pass"
    );
    assert!(
        fixed.contains("        if p in allowed:\n"),
        "size-dependent stays"
    );

    let after = run_python(&p.dir, "cases.py");
    assert_eq!(before, after);

    // Nothing is left for a second run.
    let (_, _, stderr) = p.check(&["--fix"]);
    assert!(
        stderr.contains("Fixed 0 issue(s) in 0 file(s)."),
        "{stderr}"
    );
}

#[test]
fn size_dependent_fixes_need_opt_in_and_preserve_output() {
    let p = Project::new("fix_sd");
    let original = repo_file(CASES);
    p.write("cases.py", &original);
    let before = run_python(&p.dir, "cases.py");
    let (_, _, stderr) = p.check(&["--fix", "--include-size-dependent"]);
    assert!(
        stderr.contains("Fixed 20 issue(s) in 1 file(s)."),
        "{stderr}"
    );
    let fixed = p.read("cases.py");
    let mut rewritten = DEFAULT_FIXED.to_vec();
    rewritten.extend_from_slice(SIZE_DEPENDENT_FIXED);
    unchanged_except(&original, &fixed, &rewritten);
    for line in [
        "    allowed_set = set(allowed)\n",
        "    keys_set = set(keys)\n",
        "            seen.append(w); seen_set.add(w)\n",
        "    q = collections.deque([0])\n",
        "        q.appendleft(i)\n",
    ] {
        assert!(fixed.contains(line), "{line}");
    }
    assert_eq!(before, run_python(&p.dir, "cases.py"));
}

#[test]
fn diff_previews_without_writing() {
    let p = Project::new("fix_diff");
    let src = "def f(items):\n    x = []\n    for i in items:\n        x = x + [i]\n    return x\n";
    p.write("a.py", src);
    let (code, stdout, stderr) = p.check(&["--diff"]);
    assert_eq!(code, 1, "{stderr}");
    assert_eq!(
        stdout,
        "--- a/a.py\n+++ b/a.py\n@@ -1,5 +1,5 @@\n def f(items):\n     x = []\n     for i in items:\n-        x = x + [i]\n+        x.append(i)\n     return x\n"
    );
    assert!(stderr.contains("Would fix 1 issue(s) in 1 file(s)."));
    assert_eq!(p.read("a.py"), src);
    p.write("a.py", "def f():\n    return 1\n");
    let (code, stdout, _) = p.check(&["--diff"]);
    assert_eq!((code, stdout.as_str()), (0, ""));
}

/// `--touched` fixes, previews, and fails on only the reports it shows.
#[test]
fn touched_runs_fix_only_the_reports_they_show() {
    let p = Project::new("fix_touched");
    let src = "def f(items):\n    x = []\n    for i in items:\n        x = x + [i]\n    return x\n";
    p.write("a.py", src).write("b.py", src);
    let (code, stdout, stderr) = p.check(&["--diff", "--touched", "b.py"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stdout.starts_with("--- a/b.py\n+++ b/b.py\n"), "{stdout}");
    assert!(!stdout.contains("a.py"), "{stdout}");
    assert!(
        stderr.contains("Would fix 1 issue(s) in 1 file(s)."),
        "{stderr}"
    );
    let (code, stdout, stderr) = p.check(&["--diff", "--touched", "c.py"]);
    assert_eq!((code, stdout.as_str()), (0, ""), "{stderr}");
    assert_eq!(
        stderr
            .matches("warning: `--touched` names 1 path(s)")
            .count(),
        1,
        "{stderr}"
    );
    let (code, _, stderr) = p.check(&["--fix", "--touched", "b.py"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("Fixed 1 issue(s) in 1 file(s)."),
        "{stderr}"
    );
    assert_eq!(p.read("a.py"), src);
    assert!(p.read("b.py").contains("x.append(i)"));
    // The fix left nothing in `b.py`; `a.py` still fails the whole run.
    assert_eq!(p.check(&["--touched", "b.py"]).0, 0);
    assert_eq!(p.check(&[]).0, 1);

    // A file given as a path is touched: the same rewrites, of that file only.
    p.write("b.py", src);
    let (code, stdout, stderr) = p.check(&["--diff", "b.py"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stdout.starts_with("--- a/b.py\n+++ b/b.py\n"), "{stdout}");
    assert!(!stdout.contains("a.py"), "{stdout}");
    assert!(!stderr.contains("not applied"), "{stderr}");
    let (code, _, stderr) = p.check(&["--fix", "b.py"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("Fixed 1 issue(s) in 1 file(s)."),
        "{stderr}"
    );
    assert_eq!(p.read("a.py"), src);
    assert!(p.read("b.py").contains("x.append(i)"));
}

#[test]
fn opt_diagnostics_carry_evidence_blocker_and_fix_in_json() {
    let p = Project::new("fix_json");
    p.write(
        "a.py",
        "def ok(items):\n    x = []\n    for i in items:\n        x = x + [i]\n    return x\n\n\ndef shared(items, sink):\n    x = []\n    for i in items:\n        x = x + [i]\n        sink(x)\n    return x\n",
    );
    let (_, stdout, _) = p.check(&["--output-format", "json"]);
    let diags: Vec<serde_json::Value> = serde_json::from_str(&stdout).unwrap();
    assert_eq!(diags.len(), 2);
    let ok = &diags[0];
    assert_eq!(ok["rule_code"], "OPT101");
    assert!(
        ok["evidence"]
            .as_str()
            .unwrap()
            .contains("ExclusiveAccessAtWrite(x)")
    );
    assert_eq!(ok["fix"][0]["replacement"], "x.append(i)");
    assert!(ok["blocker"].is_null());
    let shared = &diags[1];
    assert!(shared["fix"].is_null());
    assert!(
        shared["blocker"].as_str().unwrap().starts_with("Unique(x)"),
        "{shared}"
    );
    // `--select` / `--ignore` and inline suppressions apply to `OPT` rules too.
    assert_eq!(p.check(&["--ignore", "OPT"]).0, 0);
    p.write(
        "a.py",
        "def ok(items):\n    x = []\n    for i in items:\n        x = x + [i]  # heki: ignore[OPT101]\n    return x\n",
    );
    assert_eq!(p.check(&[]).0, 0);
    assert_eq!(p.check(&["--fix"]).0, 0);
    assert!(
        p.read("a.py").contains("x = x + [i]"),
        "suppressed diagnostics are not fixed"
    );
}

#[test]
fn snapshot_write_and_mut103_regressions() {
    let p = Project::new("snapshot");
    p.write(
        "pkg/__init__.py",
        "",
    )
    .write(
        "pkg/api.py",
        "def add(items, x):\n    return items + [x]\n\n\ndef _private(items):\n    items.append(1)\n",
    );
    // Without `--write` the snapshot is printed.
    let (code, stdout, _) = p.heki(&["snapshot"]);
    assert_eq!(code, 0);
    let snap: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(snap["version"], 1);
    assert!(snap["functions"].get("pkg.api:add").is_some());
    assert!(
        snap["functions"].get("pkg.api:_private").is_none(),
        "private functions are skipped"
    );
    let (code, _, stderr) = p.heki(&["snapshot", "--write"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(p.dir.join(".heki-summary-snapshot.json").exists());
    assert_eq!(p.check(&["--extend-select", "MUT103"]).0, 0);

    // `add` now mutates its argument: a regression against the snapshot.
    p.write(
        "pkg/api.py",
        "def add(items, x):\n    items.append(x)\n    return items\n\n\ndef _private(items):\n    items.append(1)\n",
    );
    let (code, stdout, _) = p.check(&["--extend-select", "MUT103", "--output-format", "concise"]);
    assert_eq!(code, 1);
    assert!(
        stdout.starts_with("pkg/api.py:1:1: MUT103 [heuristic] `add` gained side effects since the summary snapshot: mutates parameter `items`"),
        "{stdout}"
    );
    // MUT103 is opt-in.
    assert_eq!(p.check(&[]).0, 0);
    // `heki snapshot summary --write` (ARCHITECTURE.md spelling) refreshes it.
    assert_eq!(p.heki(&["snapshot", "summary", "--write"]).0, 0);
    assert_eq!(p.check(&["--extend-select", "MUT103"]).0, 0);

    // Direct execution effects newly added to a public function are caught too.
    p.write(
        "pkg/api.py",
        "import time\n\n\ndef add(items, x):\n    items.append(x)\n    time.sleep(1)\n    return items\n\n\ndef _private(items):\n    items.append(1)\n",
    );
    let (code, stdout, _) = p.check(&["--extend-select", "MUT103", "--output-format", "concise"]);
    assert_eq!(code, 1);
    assert!(
        stdout.contains("MUT103") && stdout.contains("reaches effect `sleep`"),
        "{stdout}"
    );

    // Missing and broken snapshots.
    std::fs::remove_file(p.dir.join(".heki-summary-snapshot.json")).unwrap();
    let (code, _, stderr) = p.check(&["--extend-select", "MUT103"]);
    assert_eq!(code, 0);
    assert!(
        stderr.contains("run `heki snapshot --write` first"),
        "{stderr}"
    );
    p.write(
        ".heki-summary-snapshot.json",
        "{\"version\": 99, \"functions\": {}}",
    );
    assert_eq!(p.check(&["--extend-select", "MUT103"]).0, 2);
    p.write(".heki-summary-snapshot.json", "not json");
    assert_eq!(p.check(&["--extend-select", "MUT103"]).0, 2);
}

const SNAPSHOT_BASE_API: &str =
    "from pkg import _helper\n\n\ndef run():\n    return _helper.pause()\n";

/// `heki snapshot` of the base project (the one a pull request branched from), stored as
/// `base.json` in a directory of its own, outside the root of the project that is checked.
fn base_snapshot(name: &str) -> (Project, String) {
    let base = Project::new(name);
    base.write("pkg/__init__.py", "")
        .write("pkg/_helper.py", "def pause():\n    return 1\n")
        .write("pkg/api.py", SNAPSHOT_BASE_API);
    let (code, stdout, stderr) = base.heki(&["snapshot"]);
    assert_eq!(code, 0, "{stderr}");
    base.write("base.json", &stdout);
    let path = base.dir.join("base.json").display().to_string();
    (base, path)
}

/// The project of the pull request: the private helper of another module now sleeps.
fn snapshot_head(name: &str) -> Project {
    let head = Project::new(name);
    head.write("pkg/__init__.py", "")
        .write(
            "pkg/_helper.py",
            "import time\n\n\ndef pause():\n    time.sleep(1)\n    return 1\n",
        )
        .write("pkg/api.py", SNAPSHOT_BASE_API);
    head
}

const MUT103_RUN: &[&str] = &["--select", "MUT103", "--output-format", "concise"];

#[test]
fn snapshot_path_compares_the_whole_project_with_a_file_outside_the_root() {
    let (_base, base_json) = base_snapshot("snapshot_path_base");
    let head = snapshot_head("snapshot_path_head");
    let args = [MUT103_RUN, &["--snapshot", &base_json]].concat();
    let (code, stdout, stderr) = head.check(&args);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(
        stdout.contains("pkg/api.py:4:1: MUT103") && stdout.contains("reaches effect `sleep`"),
        "{stdout}"
    );

    // A snapshot at the root, regenerated with the new effect, is not read.
    let (code, stdout, _) = head.heki(&["snapshot"]);
    assert_eq!(code, 0);
    head.write(".heki-summary-snapshot.json", &stdout);
    assert_eq!(
        head.check(MUT103_RUN).0,
        0,
        "the root file is read without the option"
    );
    let (code, stdout, stderr) = head.check(&args);
    assert_eq!(code, 1, "{stdout}{stderr}");
    assert!(stdout.contains("MUT103"), "{stdout}");
}

#[test]
fn snapshot_path_that_cannot_be_read_is_an_error() {
    let head = snapshot_head("snapshot_path_errors");
    let dir = head.dir.display().to_string();
    let run = |path: &str| {
        let args = [MUT103_RUN, &["--snapshot", path]].concat();
        head.check(&args)
    };

    let missing = format!("{dir}/missing.json");
    let (code, stdout, stderr) = run(&missing);
    assert_eq!(code, 2, "{stdout}");
    assert!(stderr.contains("missing.json"), "{stderr}");

    let broken = format!("{dir}/broken.json");
    for (text, what) in [
        ("not json", "invalid summary snapshot"),
        ("{\"version\": 1}", "invalid summary snapshot"),
        ("{\"version\": 99, \"functions\": {}}", "version"),
    ] {
        head.write("broken.json", text);
        let (code, stdout, stderr) = run(&broken);
        assert_eq!(code, 2, "{text}: {stdout}");
        assert!(stderr.contains(what), "{text}: {stderr}");
    }

    // A directory is not a snapshot.
    assert_eq!(run(&dir).0, 2);
}

#[test]
fn snapshot_path_needs_mut103_and_does_not_meet_watch() {
    let (_base, base_json) = base_snapshot("snapshot_path_needs_rule");
    let head = snapshot_head("snapshot_path_needs_rule_head");
    let (code, _, stderr) = head.check(&["--snapshot", &base_json]);
    assert_eq!(code, 2);
    assert!(stderr.contains("`MUT103` is not enabled"), "{stderr}");
    let (code, _, stderr) = head.check(&["--watch", "--snapshot", &base_json]);
    assert_eq!(code, 2);
    assert!(stderr.contains("cannot be used with"), "{stderr}");
}

/// PEP 263: a declared Latin-1 source is checked like any other, and `--fix` writes it back in
/// Latin-1 (the bytes outside the rewrite stay as they were); an encoding heki does not read is
/// an error, not a UTF-8 failure.
#[test]
fn declared_source_encodings_are_read_and_fixes_keep_them() {
    let p = Project::new("fix_latin1");
    let latin1: &[u8] = b"# coding: latin-1\nimport time\n\n\nasync def caf\xe9():\n    time.sleep(1)  # pause \xe9\n\n\ndef build(items):\n    xs = []\n    for item in items:\n        xs = xs + [item]\n    return xs\n";
    std::fs::write(p.dir.join("legacy.py"), latin1).unwrap();
    let (code, stdout, stderr) = p.check(&["--output-format", "concise"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stdout.contains("legacy.py:6:5: EFF101 "), "{stdout}");
    assert!(stdout.contains("legacy.py:12:9: OPT101 "), "{stdout}");
    let (code, diff, _) = p.check(&["--diff"]);
    assert_eq!(code, 1);
    assert!(diff.contains("+        xs.append(item)"), "{diff}");
    let (_, _, stderr) = p.check(&["--fix"]);
    assert!(
        stderr.contains("Fixed 1 issue(s) in 1 file(s)."),
        "{stderr}"
    );
    let as_latin1 = |bytes: &[u8]| bytes.iter().copied().map(char::from).collect::<String>();
    let expected = as_latin1(latin1).replace("xs = xs + [item]", "xs.append(item)");
    let fixed = std::fs::read(p.dir.join("legacy.py")).unwrap();
    assert_eq!(as_latin1(&fixed), expected, "still Latin-1");

    std::fs::write(p.dir.join("legacy.py"), b"# coding: cp1252\nx = '\x80'\n").unwrap();
    let (code, _, stderr) = p.check(&[]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("legacy.py: cannot read the file: source declares encoding `cp1252`"),
        "{stderr}"
    );
    std::fs::write(p.dir.join("legacy.py"), b"x = '\xe9'\n").unwrap();
    let (code, _, stderr) = p.check(&[]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("stream did not contain valid UTF-8"),
        "{stderr}"
    );
}

/// A module with a hoisting rewrite (`OPT130`) and a deque rewrite that adds
/// `import collections` below a `__future__` import (`OPT120`), and a call printing both.
const LINE_BREAKS: &str = r#""""Doc."""
from __future__ import annotations


def f(n):
    hits = 0
    for i in range(n):
        hits += "a" in set("abcdefghijklmnopq")
    return hits


def g(items):
    q = []
    for x in items:
        q.append(x)
    out = []
    while q:
        out.append(q.pop(0))
    return out


print(f(3), g([1, 2, 3]))
"#;

#[test]
fn files_with_cr_or_crlf_line_breaks_get_the_fixes_of_lf_files_in_their_own_line_breaks() {
    let p = Project::new("fix_line_breaks");
    for (file, br) in [("lf.py", "\n"), ("cr.py", "\r"), ("crlf.py", "\r\n")] {
        p.write(file, &LINE_BREAKS.replace('\n', br));
    }
    let before = run_python(&p.dir, "cr.py");
    let (code, _, stderr) = p.check(&["--fix", "--include-size-dependent"]);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains("Fixed 6 issue(s) in 3 file(s)."),
        "{stderr}"
    );
    let lf = p.read("lf.py");
    assert!(
        lf.contains("    hoisted = set(\"abcdefghijklmnopq\")\n    for i in range(n):\n")
            && lf.contains("from __future__ import annotations\n\nimport collections\n"),
        "{lf}"
    );
    for (file, br) in [("cr.py", "\r"), ("crlf.py", "\r\n")] {
        let fixed = std::fs::read(p.dir.join(file)).unwrap();
        assert_eq!(
            String::from_utf8(fixed).unwrap(),
            lf.replace('\n', br),
            "{file}"
        );
        assert_eq!(run_python(&p.dir, file), before, "{file}");
    }
}
