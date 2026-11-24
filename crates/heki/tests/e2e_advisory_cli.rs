//! `heki check` / `heki snapshot` end to end: configuration files and their errors, file
//! discovery, inline suppressions, Ratchet baseline I/O, profile dumps, and summary snapshots.

mod common;

use common::{Project, report_path};
use serde_json::Value;

const BLOCKING: &str = "import time\n\n\nasync def handler():\n    time.sleep(1)\n";

fn json(stdout: &str) -> Vec<Value> {
    serde_json::from_str(stdout).unwrap_or_else(|e| panic!("{e}: {stdout}"))
}

/// `(path, line, rule)` of every diagnostic in a JSON report.
fn hits(stdout: &str) -> Vec<(String, u64, String)> {
    json(stdout)
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn hit(path: &str, line: u64, rule: &str) -> (String, u64, String) {
    (path.to_owned(), line, rule.to_owned())
}

#[test]
fn configuration_files_are_read_from_the_root_or_an_explicit_path() {
    let p = Project::new("cfg_files");
    p.write("a.py", BLOCKING)
        .write("custom.toml", "[lint]\nignore = [\"EFF101\"]\n");
    // `--config` replaces the root's configuration.
    assert_eq!(p.check(&[]).0, 1);
    let (code, _, stderr) = p.check(&["--config", "custom.toml"]);
    assert_eq!(code, 0, "{stderr}");
    // An explicit `pyproject.toml` is read through its `[tool.heki]` table.
    p.write(
        "alt/pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nignore = [\"EFF\"]\n",
    );
    assert_eq!(p.check(&["--config", "alt/pyproject.toml"]).0, 0);

    // Without `pyproject.toml`, `.git` marks the root and the defaults apply.
    std::fs::remove_file(p.dir.join("pyproject.toml")).unwrap();
    std::fs::create_dir_all(p.dir.join(".git")).unwrap();
    let (code, stdout, _) = p.check(&["--output-format", "json"]);
    assert_eq!(code, 1);
    assert_eq!(hits(&stdout), vec![hit("a.py", 5, "EFF101")]);
}

#[test]
fn target_version_defaults_to_the_lower_bound_of_requires_python() {
    let p = Project::new("cfg_requires_python");
    p.write(
        "comps.py",
        "import requests\n\n\nasync def fetch_all(urls):\n    return [requests.get(u) for u in urls]\n",
    );
    // Python 3.11 runs the comprehension in a frame of its own, so `EFF101` names
    // `<listcomp>`; 3.12+ inlines it (PEP 709).
    let as_py311 = |args: &[&str]| {
        let mut all = vec!["--output-format", "json", "--select", "EFF101,EFF111"];
        all.extend_from_slice(args);
        let (code, stdout, stderr) = p.check(&all);
        assert_eq!(code, 1, "{stdout}{stderr}");
        stdout.contains("<listcomp>")
    };
    let pyproject = |spec: &str, tool: &str| {
        p.write(
            "pyproject.toml",
            &format!("[project]\nname = \"t\"\nrequires-python = \"{spec}\"\n{tool}"),
        );
    };
    for (spec, py311) in [
        (">=3.11", true),
        (">= 3.11.2, <4", true),
        (">3.11", true),
        ("==3.11.*", true),
        (">=3.8", true),
        (">=3", true),
        ("~=3.12", false),
        ("<3.14", false),
        (">=3.16", false),
        (">=three", false),
    ] {
        pyproject(spec, "");
        assert_eq!(as_py311(&[]), py311, "requires-python = {spec:?}");
    }
    // `target-version` and `--target-version` win over `requires-python`.
    pyproject(">=3.11", "\n[tool.heki]\ntarget-version = \"py312\"\n");
    assert!(!as_py311(&[]));
    pyproject(">=3.12", "");
    assert!(as_py311(&["--target-version", "py311"]));
    // A `heki.toml` (found or explicit) takes the bound from the `pyproject.toml` next to it.
    pyproject(">=3.11", "");
    p.write("heki.toml", "[lint]\nignore = [\"EFF102\"]\n");
    assert!(as_py311(&[]));
    std::fs::remove_file(p.dir.join("heki.toml")).unwrap();
    p.write("cfg/custom.toml", "[lint]\nignore = [\"EFF102\"]\n");
    assert!(!as_py311(&["--config", "cfg/custom.toml"]));
    p.write(
        "cfg/pyproject.toml",
        "[project]\nrequires-python = \">=3.11\"\n",
    );
    assert!(as_py311(&["--config", "cfg/custom.toml"]));
    assert!(as_py311(&["--config", "cfg/pyproject.toml"]));
    // A malformed `requires-python` (or neighbouring `pyproject.toml`) only loses the bound.
    p.write("cfg/pyproject.toml", "[project]\nrequires-python = 311\n");
    assert!(!as_py311(&["--config", "cfg/pyproject.toml"]));
    p.write("cfg/pyproject.toml", "[project\n");
    assert!(!as_py311(&["--config", "cfg/custom.toml"]));
}

#[test]
fn unreadable_and_malformed_configuration_is_a_configuration_error() {
    let p = Project::new("cfg_errors");
    p.write("a.py", BLOCKING);
    let fails = |args: &[&str], needle: &str| {
        let (code, stdout, stderr) = p.check(args);
        assert_eq!(code, 2, "{args:?}: {stdout}{stderr}");
        assert!(stderr.contains(needle), "{args:?}: {stderr}");
    };
    fails(
        &["--config", "missing.toml"],
        "error: cannot read the configuration file `missing.toml`: ",
    );
    // The cause is printed once (the message carries it; it is not also a `source()`).
    let (_, _, stderr) = p.check(&["--config", "missing.toml"]);
    assert_eq!(stderr.matches("(os error 2)").count(), 1, "{stderr}");
    p.write("alt/pyproject.toml", "[tool.heki\n");
    fails(
        &["--config", "alt/pyproject.toml"],
        "error: alt/pyproject.toml:1:11: the configuration file is not valid TOML: ",
    );
    p.write("alt/custom.toml", "[lint\n");
    fails(
        &["--config", "alt/custom.toml"],
        "error: alt/custom.toml:1:6: the configuration file is not valid TOML: ",
    );
    // Configuration that is not UTF-8 cannot be read.
    std::fs::write(p.dir.join("heki.toml"), b"\xff\xfe").unwrap();
    fails(&[], "heki.toml`: stream did not contain valid UTF-8");
    std::fs::remove_file(p.dir.join("heki.toml")).unwrap();
    std::fs::write(p.dir.join("pyproject.toml"), b"\xff\xfe").unwrap();
    fails(&[], "pyproject.toml`: stream did not contain valid UTF-8");
    p.write("pyproject.toml", "[tool.heki\n");
    fails(&[], "the configuration file is not valid TOML");

    let config = |toml: &str| {
        p.write(
            "pyproject.toml",
            &format!("[project]\nname = \"t\"\n\n{toml}"),
        );
    };
    config("[tool.heki.lint.severity]\nNOPE9 = \"warning\"\n");
    fails(&[], "unknown rule selector `NOPE9` in `lint.severity`");
    config("[tool.heki]\nextend-exclude = [\"gen[erated\"]\n");
    fails(&[], "invalid exclude glob `gen[erated`");
    config("[tool.heki.lint]\nper-file-ignores = { \"a[.py\" = [\"EFF101\"] }\n");
    fails(&[], "invalid glob `a[.py` in `lint.per-file-ignores`");
    config(
        "[[tool.heki.write-owners]]\nname = \"s\"\ntargets = [\"Model.fields\"]\nallowed-writers = [\"Store.[*\"]\n",
    );
    fails(
        &[],
        "invalid glob `Store.[*` in write-owners.allowed-writers",
    );
    config("[[tool.heki.write-owners]]\nname = \"s\"\ntargets = [\"fields\"]\n");
    fails(&[], "invalid write-owners target `fields`");
    config("[[tool.heki.readonly-returns]]\nname = \"r\"\nproducers = [\"a.[b\"]\n");
    fails(&[], "invalid glob `a.[b` in readonly-returns.producers");
    config("[[tool.heki.ecosystem]]\nsymbol = \"mylib.fetch\"\neffects = [\"network\"]\n");
    fails(
        &[],
        "invalid effect kind `network` in user ecosystem override",
    );
}

/// A configuration file's name decides which of its tables hold the settings, as for Ruff's
/// `pyproject.toml` and `ruff.toml`: a `pyproject.toml` holds them in `[tool.heki]` (its other
/// tables belong to other tools), any other file at its top level. A setting heki would not
/// read is a configuration error: an unknown key, a `[tool.heki]` table outside a
/// `pyproject.toml`, or a setting at the top level of a `pyproject.toml`.
#[test]
fn a_setting_in_the_wrong_table_or_under_an_unknown_key_is_a_configuration_error() {
    let p = Project::new("cfg_tables");
    p.write(
        "app.py",
        "import mylib


async def handler():
    mylib.fetch()
",
    );
    let entry = "symbol = \"mylib.fetch\"\neffects = [\"blocking_net\"]\n";
    let reports = |args: &[&str]| {
        let mut all = vec!["--output-format", "json"];
        all.extend_from_slice(args);
        let (code, stdout, stderr) = p.check(&all);
        assert_eq!(code, 1, "{args:?}: {stdout}{stderr}");
        hits(&stdout)
    };
    let fails = |args: &[&str], needle: &str| {
        let (code, stdout, stderr) = p.check(args);
        assert_eq!(code, 2, "{args:?}: {stdout}{stderr}");
        assert!(stderr.contains(needle), "{args:?}: {stderr}");
    };
    let expected = vec![hit("app.py", 5, "EFF101")];
    p.write("custom.toml", &format!("[[ecosystem]]\n{entry}"));
    assert_eq!(reports(&["--config", "custom.toml"]), expected);
    p.write(
        "alt/pyproject.toml",
        &format!("[project]\nname = \"t\"\n\n[tool.ruff]\nline-length = 99\n\n[[tool.heki.ecosystem]]\n{entry}"),
    );
    assert_eq!(reports(&["--config", "alt/pyproject.toml"]), expected);

    p.write("custom.toml", &format!("[[tool.heki.ecosystem]]\n{entry}"));
    fails(
        &["--config", "custom.toml"],
        "error: custom.toml:1:3: `[tool.heki]` outside a `pyproject.toml`\n",
    );
    p.write("heki.toml", &format!("[[tool.heki.ecosystem]]\n{entry}"));
    fails(
        &[],
        "error: heki.toml:1:3: `[tool.heki]` outside a `pyproject.toml`\n",
    );
    std::fs::remove_file(p.dir.join("heki.toml")).unwrap();
    p.write(
        "alt/pyproject.toml",
        &format!("[project]\nname = \"t\"\n\n[[ecosystem]]\n{entry}"),
    );
    fails(
        &["--config", "alt/pyproject.toml"],
        "error: alt/pyproject.toml:4:3: `ecosystem` at the top level of a `pyproject.toml`\n",
    );
    p.write(
        "custom.toml",
        "[lint]\nignore = [\"EFF101\"]\nbogus = true\n",
    );
    fails(
        &["--config", "custom.toml"],
        "error: custom.toml:3:1: unknown key `bogus` in [lint]\n",
    );
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki]\ntarget_version = \"py312\"\n",
    );
    fails(
        &[],
        "unknown key `target_version` in [tool.heki]\n  |\n5 | target_version = \"py312\"\n  | ^\n  = help: did you mean `target-version`?\n",
    );
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.write-owners]]\nname = \"s\"\ntarget = [\"Model.fields\"]\n",
    );
    fails(
        &[],
        "unknown key `target` in [[tool.heki.write-owners]]\n  |\n6 | target = [\"Model.fields\"]\n  | ^\n  = help: did you mean `targets`?\n",
    );
}

#[test]
fn ecosystem_overrides_add_and_exclude_blocking_apis() {
    let p = Project::new("cfg_ecosystem");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.ecosystem]]\nsymbol = \"mylib.locks.acquire\"\neffects = [\"blocking_sync\"]\n\n[[tool.heki.ecosystem]]\nsymbol = \"time.sleep\"\nexcluded = true\n",
    )
    .write(
        "app.py",
        "import time\n\nfrom mylib import locks\n\n\nasync def handler():\n    locks.acquire()\n    time.sleep(1)\n",
    );
    let (code, stdout, _) = p.check(&["--output-format", "json"]);
    assert_eq!(code, 1);
    let diags = json(&stdout);
    assert_eq!(diags.len(), 1, "{stdout}");
    assert_eq!(diags[0]["line"], 7);
    assert!(
        diags[0]["message"]
            .as_str()
            .unwrap()
            .contains("blocking call `mylib.locks.acquire`"),
        "{stdout}"
    );
    // The help of a `blocking_sync` wait names the `asyncio` primitives.
    assert!(
        diags[0]["help"]
            .as_str()
            .unwrap()
            .contains("or use the `asyncio` primitive"),
        "{stdout}"
    );
}

#[test]
fn discovery_of_explicit_files_outside_paths_and_excluded_directories() {
    let p = Project::new("discovery");
    let outside = Project::new("discovery_outside");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki]\nextend-exclude = [\"generated\"]\n",
    )
    .write("a.py", BLOCKING)
    .write("b.py", BLOCKING)
    .write("generated/g.py", BLOCKING)
    // A script without the `.py` extension.
    .write("tool", BLOCKING);
    outside.write("x.py", BLOCKING);

    let (_, stdout, _) = p.check(&["--output-format", "json"]);
    assert_eq!(
        hits(&stdout),
        vec![hit("a.py", 5, "EFF101"), hit("b.py", 5, "EFF101")],
        "`generated/` is excluded and `tool` is not a Python file"
    );
    // Named files are always checked (Ruff semantics), whatever their extension.
    let (_, stdout, _) = p.check(&["a.py", "tool", "--output-format", "json"]);
    assert_eq!(
        hits(&stdout),
        vec![hit("a.py", 5, "EFF101"), hit("tool", 5, "EFF101")]
    );
    let (_, stdout, _) = p.check(&["generated/g.py", "--output-format", "json"]);
    assert_eq!(hits(&stdout), vec![hit("generated/g.py", 5, "EFF101")]);
    // A second directory outside the project root keeps its own path.
    let other = outside.dir.to_str().unwrap();
    let (_, stdout, _) = p.check(&[".", other, "--output-format", "json"]);
    let paths: Vec<String> = hits(&stdout).into_iter().map(|h| h.0).collect();
    assert_eq!(
        paths,
        vec![
            report_path(&outside.dir.join("x.py")),
            "a.py".to_owned(),
            "b.py".to_owned()
        ]
    );
}

// Unix only: the directory is made unreadable with permission bits; on Windows that takes an
// access control list, which std cannot set.
#[cfg(unix)]
#[test]
fn an_unreadable_directory_is_an_io_error() {
    use std::os::unix::fs::PermissionsExt;
    let p = Project::new("walk_error");
    p.write("a.py", BLOCKING).write("locked/l.py", BLOCKING);
    let locked = p.dir.join("locked");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let readable = std::fs::read_dir(&locked).is_ok(); // running as root
    let (code, _, stderr) = p.check(&[]);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    if !readable {
        assert_eq!(code, 2, "{stderr}");
        assert!(stderr.contains("failed to walk `.`"), "{stderr}");
        assert!(stderr.contains("Permission denied"), "{stderr}");
    }
}

#[test]
fn a_source_that_is_not_utf8_is_a_read_error() {
    let p = Project::new("source_not_utf8");
    p.write("a.py", BLOCKING);
    std::fs::write(p.dir.join("latin1.py"), b"s = '\xe9'\n").unwrap();
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    // The other files are still checked; the run fails after the report.
    assert_eq!(code, 2, "{stderr}");
    assert_eq!(hits(&stdout), vec![hit("a.py", 5, "EFF101")]);
    assert!(
        stderr
            .contains("error: latin1.py: cannot read the file: stream did not contain valid UTF-8"),
        "{stderr}"
    );
}

#[test]
fn fixes_that_cannot_be_written_are_an_io_error() {
    let p = Project::new("fix_readonly");
    let src =
        "def build(items):\n    x = []\n    for i in items:\n        x = x + [i]\n    return x\n";
    p.write("a.py", src);
    let path = p.dir.join("a.py");
    let writable = std::fs::metadata(&path).unwrap().permissions();
    let mut read_only = writable.clone();
    read_only.set_readonly(true);
    std::fs::set_permissions(&path, read_only).unwrap();
    // The system's text for the error; none when running as root, who writes anyway.
    let denied = std::fs::OpenOptions::new().write(true).open(&path).err();
    let (code, _, stderr) = p.check(&["--fix"]);
    std::fs::set_permissions(&path, writable).unwrap();
    if let Some(denied) = denied {
        assert_eq!(code, 2, "{stderr}");
        assert!(
            stderr.contains(&format!("error: a.py: cannot write the file: {denied}\n")),
            "{stderr}"
        );
        assert_eq!(p.read("a.py"), src);
    }
}

#[test]
fn inline_directives_without_rule_lists_do_not_suppress() {
    let p = Project::new("directives");
    p.write(
        "a.py",
        "import time\n\n\nasync def handler():\n    time.sleep(1)  # heki: ignore EFF101\n    time.sleep(2)  # noqa:\n    time.sleep(3)  # heki: ignore\n    time.sleep(4)  # noqa\n    time.sleep(5)  # heki: ignore[EFF101] trailing words\n    time.sleep(6)  # heki: disable[EFF101]\n    time.sleep(7)  # heki: ignore[\n    time.sleep(8)  # heki: ignore[]\n    time.sleep(9)  # heki: ignore[EFF101\n",
    );
    let (code, stdout, _) = p.check(&["--output-format", "json"]);
    assert_eq!(code, 1);
    // A rule list needs brackets (`# heki: ignore[EFF101]`) or codes (`# noqa: EFF101`);
    // `heki:` directives other than `ignore` suppress nothing, and neither does a bracketed
    // list that is unfinished or empty (only the bare forms suppress every rule).
    assert_eq!(
        hits(&stdout),
        vec![
            hit("a.py", 5, "EFF101"),
            hit("a.py", 6, "EFF101"),
            hit("a.py", 10, "EFF101"),
            hit("a.py", 11, "EFF101"),
            hit("a.py", 12, "EFF101"),
            hit("a.py", 13, "EFF101")
        ]
    );
}

#[test]
fn ratchet_baseline_files_that_cannot_be_read_or_written() {
    let p = Project::new("baseline_io");
    p.write("a.py", BLOCKING);
    let (code, _, stderr) = p.check(&["--baseline", "missing.toml"]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("error: missing.toml: cannot read the baseline: "),
        "{stderr}"
    );
    let (code, _, stderr) = p.check(&["--baseline", "no-dir/b.toml", "--update-baseline"]);
    assert_eq!(code, 2);
    // The cause is the system's own text.
    let missing_dir = std::fs::write(p.dir.join("no-dir/b.toml"), "").unwrap_err();
    assert!(
        stderr.contains(&format!(
            "error: no-dir/b.toml: cannot write the baseline: {missing_dir}\n"
        )),
        "{stderr}"
    );
    // `baseline` in the configuration is used when `--baseline` is absent.
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki]\nbaseline = \"heki-baseline.toml\"\n",
    );
    assert_eq!(p.check(&["--update-baseline"]).0, 0);
    assert!(
        p.read("heki-baseline.toml")
            .contains("rule_code = \"EFF101\"")
    );
    let (code, _, stderr) = p.check(&[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("All checks passed"), "{stderr}");
}

#[test]
fn snapshots_record_keyed_self_writes_and_effect_kinds() {
    let p = Project::new("snapshot_kinds");
    p.write("pkg/__init__.py", "").write(
        "pkg/conf.py",
        "import sqlite3\nimport subprocess\n\nimport pandas as pd\nimport requests\n\n\nclass Settings:\n    def __init__(self):\n        self.params = {}\n        self.name = \"\"\n        self.rows = []\n\n    def set_proxy(self, url):\n        self.params[\"proxy\"] = url\n        self.name = url\n\n    def stash(self, row):\n        _keep(self.rows, row)\n\n    def pick(self, flag, row):\n        if flag:\n            rows = self.rows\n        else:\n            rows = self.rows\n        rows.append(row)\n\n\ndef _keep(items, item):\n    items.append(item)\n\n\ndef _fetch(u):\n    return requests.get(u)\n\n\ndef _query(conn: sqlite3.Connection):\n    return conn.execute(\"select 1\")\n\n\ndef _build():\n    return subprocess.run([\"make\"])\n\n\ndef _grow(frames):\n    df = None\n    for f in frames:\n        df = pd.concat([df, f])\n    return df\n\n\ndef sync_all(u, conn, frames):\n    return _fetch(u), _query(conn), _build(), _grow(frames)\n",
    );
    let (code, stdout, _) = p.heki(&["snapshot"]);
    assert_eq!(code, 0);
    let snap: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        snap["functions"]["pkg.conf:Settings.set_proxy"]["mutates_self_attrs"],
        serde_json::json!(["name", "params[\"proxy\"]"])
    );
    // `self.rows` handed to a helper that mutates it, and mutated through a local bound to
    // `self.rows` on both branches.
    for method in ["stash", "pick"] {
        assert_eq!(
            snap["functions"][format!("pkg.conf:Settings.{method}")]["mutates_self_attrs"],
            serde_json::json!(["rows"]),
            "{method}"
        );
    }
    assert_eq!(
        snap["functions"]["pkg.conf:sync_all"]["transitive_exec_effects"],
        serde_json::json!([
            "blocking_db",
            "blocking_net",
            "blocking_subprocess",
            "db_query",
            "quadratic_realloc",
            "unpooled_client_alloc"
        ])
    );
}

#[test]
fn mut103_skips_new_functions_and_honors_per_file_ignores() {
    let p = Project::new("snapshot_scope");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nper-file-ignores = { \"pkg/legacy.py\" = [\"MUT103\"] }\n",
    )
    .write("pkg/__init__.py", "")
    .write("pkg/legacy.py", "def legacy(items):\n    return items\n")
    .write("pkg/api.py", "def add(items):\n    return items\n");
    assert_eq!(p.heki(&["snapshot", "--write"]).0, 0);
    let mutating =
        |name: &str| format!("def {name}(items):\n    items.append(1)\n    return items\n");
    p.write("pkg/legacy.py", &mutating("legacy")).write(
        "pkg/api.py",
        &format!("{}\n\n{}", mutating("add"), mutating("added")),
    );
    let (code, stdout, _) = p.check(&["--select", "MUT103", "--output-format", "json"]);
    assert_eq!(code, 1);
    // `legacy` is ignored for its file; `added` has no snapshot entry yet.
    assert_eq!(hits(&stdout), vec![hit("pkg/api.py", 1, "MUT103")]);
}

/// A parameter stored in a module global escapes; a returned one does not.
#[test]
fn mut103_reports_parameters_published_to_globals_and_closures() {
    let p = Project::new("snapshot_escapes");
    let before = "latest = None\n\n\ndef publish(x):\n    pass\n\n\ndef give(x):\n    pass\n";
    p.write("pkg/__init__.py", "").write("pkg/pub.py", before);
    assert_eq!(p.heki(&["snapshot", "--write"]).0, 0);
    p.write(
        "pkg/pub.py",
        "latest = None\n\n\ndef publish(x):\n    global latest\n    latest = x\n\n\ndef give(x):\n    return x\n",
    );
    let (code, stdout, _) = p.check(&["--select", "MUT103", "--output-format", "json"]);
    assert_eq!(code, 1);
    assert_eq!(hits(&stdout), vec![hit("pkg/pub.py", 4, "MUT103")]);
}

#[test]
fn snapshot_files_that_cannot_be_read_or_written() {
    let p = Project::new("snapshot_io");
    p.write("a.py", "def f(items):\n    return items\n");
    // A directory where the snapshot file belongs.
    std::fs::create_dir_all(p.dir.join(".heki-summary-snapshot.json")).unwrap();
    let (code, _, stderr) = p.check(&["--extend-select", "MUT103"]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("error: .heki-summary-snapshot.json: cannot read the summary snapshot: "),
        "{stderr}"
    );
    let (code, _, stderr) = p.heki(&["snapshot", "--write"]);
    assert_eq!(code, 2);
    assert!(
        stderr.contains("error: .heki-summary-snapshot.json: cannot write the summary snapshot: "),
        "{stderr}"
    );
}

#[test]
fn opt_rules_without_edits_and_per_rule_selection() {
    let p = Project::new("opt_selection");
    p.write(
        "a.py",
        "def member_while(n):\n    allowed = list(range(0, 50, 3))\n    hits = []\n    i = 0\n    while i < n:\n        if i in allowed:\n            hits.append(i)\n        i += 1\n    return hits\n\n\ndef build(items):\n    x = []\n    for i in items:\n        x = x + [i]\n    return x\n",
    );
    let (code, stdout, _) = p.check(&["--extend-select", "OPT110", "--output-format", "json"]);
    assert_eq!(code, 1);
    let diags = json(&stdout);
    // Hoisting out of a `while` loop has no edit: the rewrite is proven safe but manual.
    assert_eq!(diags[0]["rule_code"], "OPT110");
    assert!(
        diags[0]["help"]
            .as_str()
            .unwrap()
            .ends_with("[proven safe; rewrite it by hand]"),
        "{stdout}"
    );
    assert!(diags[0]["fix"].is_null());
    // One `OPT` rule can be ignored while the others still run, also when it is selected.
    let ignored = ["--extend-select", "OPT110", "--ignore", "OPT110"];
    let (_, stdout, _) = p.check(&[&ignored[..], &["--output-format", "json"]].concat());
    assert_eq!(hits(&stdout), vec![hit("a.py", 15, "OPT101")]);
}

#[test]
fn an_exclude_set_too_large_to_compile_is_a_configuration_error() {
    // Every pattern is a valid glob, but together they exceed the matcher's size limit.
    let p = Project::new("cfg_glob_limit");
    p.write("a.py", BLOCKING).write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki]\nextend-exclude = [\"{}\"]\n",
            "[!a]".repeat(150_000)
        ),
    );
    let (code, _, stderr) = p.check(&[]);
    assert_eq!(code, 2);
    assert!(
        stderr.starts_with("error: invalid exclude glob `.bzr, .direnv,")
            && stderr.contains("error building NFA\n  = help: "),
        "{}",
        &stderr[..200.min(stderr.len())]
    );
}
