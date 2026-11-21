//! Project and module discovery end to end: source roots, which file owns a module name, the
//! target version, and how imports bind names (what `heki check` links across modules).

mod common;

use common::Project;
use serde_json::{Value, json};

/// `import time` + a blocking helper.
const BLOCKING_WAIT: &str = "import time\n\n\ndef wait():\n    time.sleep(1)\n";

/// `heki check --output-format concise` of the project: `(exit code, stdout)`.
fn concise(p: &Project) -> (i32, String) {
    let (code, stdout, stderr) = p.check(&["--output-format", "concise"]);
    assert_ne!(code, 2, "{stderr}");
    (code, stdout)
}

/// `heki check` of the project in the full format, which shows each call chain.
fn full(p: &Project) -> String {
    let (code, stdout, stderr) = p.check(&[]);
    assert_ne!(code, 2, "{stderr}");
    stdout
}

#[test]
fn src_directories_are_source_roots_unless_they_are_packages() {
    // A namespace package under `src/` (no `__init__.py`): `acme.io` links.
    let p = Project::new("disc_src_namespace");
    p.write("src/acme/io.py", BLOCKING_WAIT).write(
        "src/acme/api.py",
        "from acme.io import wait\n\n\nasync def go():\n    wait()\n",
    );
    let (code, stdout) = concise(&p);
    assert_eq!(code, 1);
    assert!(stdout.contains("src/acme/api.py:5:5: EFF101 "), "{stdout}");
    let chain = full(&p);
    assert!(
        chain.contains("= related: src/acme/api.py:5:5: `go` calls `acme.io.wait`"),
        "{chain}"
    );

    // Modules directly under `src/` are top-level modules.
    let p = Project::new("disc_src_modules");
    p.write("src/helpers.py", BLOCKING_WAIT).write(
        "src/main.py",
        "from helpers import wait\n\n\nasync def go():\n    wait()\n",
    );
    assert!(concise(&p).1.contains("src/main.py:5:5: EFF101 "));

    // `src` is a package itself: its modules are `src.*`.
    let p = Project::new("disc_src_package");
    p.write("src/__init__.py", "")
        .write("src/helpers.py", BLOCKING_WAIT)
        .write(
            "src/main.py",
            "from src.helpers import wait\n\n\nasync def go():\n    wait()\n",
        );
    assert!(full(&p).contains("`go` calls `src.helpers.wait`"));
    p.write(
        "src/main.py",
        "from helpers import wait\n\n\nasync def go():\n    wait()\n",
    );
    assert_eq!(concise(&p), (0, String::new()));
}

#[test]
fn a_package_owns_its_name_over_a_module_of_the_same_name() {
    let p = Project::new("disc_package_over_module");
    p.write("pkg/__init__.py", BLOCKING_WAIT)
        .write("pkg.py", "def wait():\n    pass\n")
        .write(
            "app.py",
            "from pkg import wait\n\n\nasync def go():\n    wait()\n",
        );
    let (code, stdout) = concise(&p);
    assert_eq!(code, 1);
    assert!(stdout.contains("app.py:5:5: EFF101 "), "{stdout}");
    // The other way round: the package's no-op wins over the module's blocking `wait`.
    p.write("pkg/__init__.py", "def wait():\n    pass\n")
        .write("pkg.py", BLOCKING_WAIT);
    assert_eq!(concise(&p), (0, String::new()));
    // A stub package still loses to the module implementation.
    std::fs::remove_file(p.dir.join("pkg/__init__.py")).unwrap();
    p.write("pkg/__init__.pyi", "def wait() -> None: ...\n");
    assert!(concise(&p).1.contains("app.py:5:5: EFF101 "));
}

#[test]
fn a_typing_stub_importing_builtins_patches_nothing() {
    let p = Project::new("disc_stub_builtins");
    p.write(
        "a.py",
        "def build(items):\n    xs = list()\n    for item in items:\n        xs = xs + [item]\n    return xs\n",
    )
    .write("m.pyi", "import builtins\n\n\ndef f(x: builtins.str) -> None: ...\n");
    assert!(concise(&p).1.contains("a.py:4:9: OPT101 "));
    // The same import in a module that runs may rebind `list`.
    p.write("m.py", "import builtins\n");
    assert!(!concise(&p).1.contains("OPT101"));
}

#[test]
fn a_file_that_does_not_parse_is_reported_and_the_others_are_checked() {
    // A Python 2 file cannot run, so nothing it would do is assumed: the other files are checked
    // as usual, its `builtins` patch blocks no fix, and the run fails with exit code 2.
    let p = Project::new("disc_syntax_error_file");
    p.write("pkg/__init__.py", "")
        .write(
            "pkg/api.py",
            "import time\n\n\nasync def go():\n    time.sleep(1)\n",
        )
        .write(
            "pkg/build.py",
            "def build(items):\n    xs = []\n    for item in items:\n        xs = xs + [item]\n    return xs\n",
        )
        .write(
            "pkg/py2.py",
            "import builtins\nbuiltins.list = tuple\nprint \"done\"\n",
        );
    std::fs::write(p.dir.join("pkg/bytes.py"), b"x = '\xff'\n").unwrap();
    let (code, stdout, stderr) = p.check(&["--output-format", "concise"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stdout.contains("pkg/api.py:5:5: EFF101 "), "{stdout}");
    assert!(stdout.contains("pkg/build.py:4:9: OPT101 "), "{stdout}");
    assert!(
        stderr.contains("error: pkg/py2.py:3:7: syntax error: "),
        "{stderr}"
    );
    assert!(
        stderr.contains("error: pkg/bytes.py: stream did not contain valid UTF-8"),
        "{stderr}"
    );
    // JSON holds the diagnostics only; SARIF also carries the file, as an error notification
    // of a run that did not execute successfully.
    let (_, stdout, _) = p.check(&["--output-format", "json"]);
    let diags: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(diags.as_array().map(Vec::len), Some(2), "{stdout}");
    let (code, stdout, _) = p.check(&["--output-format", "sarif"]);
    assert_eq!(code, 2);
    let sarif: Value = serde_json::from_str(&stdout).unwrap();
    let run = &sarif["runs"][0];
    assert_eq!(run["results"].as_array().map(Vec::len), Some(2), "{stdout}");
    let invocation = &run["invocations"][0];
    assert_eq!(invocation["executionSuccessful"], false, "{stdout}");
    // The unreadable file has no position.
    assert_eq!(
        invocation["toolExecutionNotifications"][0]["locations"][0],
        json!({ "physicalLocation": { "artifactLocation": { "uri": "pkg/bytes.py" } } })
    );
    let notification = &invocation["toolExecutionNotifications"][1];
    assert_eq!(notification["level"], "error");
    assert!(
        notification["message"]["text"]
            .as_str()
            .is_some_and(|m| m.starts_with("syntax error: ")),
        "{stdout}"
    );
    let place = &notification["locations"][0]["physicalLocation"];
    assert_eq!(place["artifactLocation"]["uri"], "pkg/py2.py");
    assert_eq!(place["region"], json!({ "startLine": 3, "startColumn": 7 }));
    let (code, _, stderr) = p.check(&["--fix"]);
    assert_eq!(code, 2, "{stderr}");
    assert!(p.read("pkg/build.py").contains("xs.append(item)"));
    // Once every file reads and parses, the run executes successfully.
    p.write("pkg/py2.py", "print(\"done\")\n")
        .write("pkg/bytes.py", "x = 1\n");
    let (code, stdout, _) = p.check(&["--output-format", "sarif"]);
    assert_eq!(code, 1);
    let sarif: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        sarif["runs"][0]["invocations"][0],
        json!({ "executionSuccessful": true, "toolExecutionNotifications": [] })
    );
}

#[test]
fn a_module_that_does_not_parse_leaves_the_names_under_it_unresolved() {
    // A project module owns its name over a module the catalog lists, also when it does not
    // parse: then the names under it are unknown, never the catalog's.
    let p = Project::new("disc_unread_module");
    p.write(
        "api.py",
        "import requests\n\n\nclass Client(requests.Session):\n    pass\n\n\nasync def fetch(url):\n    return requests.get(url)\n\n\nasync def open_session(url):\n    return requests.Session().get(url)\n\n\nasync def derived(url):\n    return Client().get(url)\n",
    );
    let (code, stdout) = concise(&p);
    assert_eq!(code, 1);
    for line in [9, 13, 17] {
        assert!(
            stdout.contains(&format!("api.py:{line}:12: EFF101 ")),
            "{stdout}"
        );
    }
    p.write("requests.py", "def get(url):\n    return url\n");
    assert!(!concise(&p).1.contains("api.py:9:12: "));
    p.write(
        "requests.py",
        "def get(url):\n    return url\nprint \"done\"\n",
    );
    let (code, stdout, stderr) = p.check(&["--output-format", "concise"]);
    assert_eq!((code, stdout.as_str()), (2, ""), "{stderr}");
    assert!(
        stderr.contains("error: requests.py:3:7: syntax error: "),
        "{stderr}"
    );
    // A name imported from a project module that does not parse resolves to nothing either.
    std::fs::remove_file(p.dir.join("requests.py")).unwrap();
    std::fs::remove_file(p.dir.join("api.py")).unwrap();
    let net = "import httpx\n\n\ndef pull(url):\n    return httpx.get(url)\n";
    p.write("pkg/__init__.py", "")
        .write("pkg/net.py", net)
        .write(
            "pkg/use.py",
            "from pkg.net import pull\n\n\nasync def go(url):\n    return pull(url)\n",
        );
    assert!(concise(&p).1.contains("pkg/use.py:5:12: EFF101 "));
    p.write("pkg/net.py", &format!("{net}print \"done\"\n"));
    let (code, stdout, stderr) = p.check(&["--output-format", "concise"]);
    assert_eq!((code, stdout.as_str()), (2, ""), "{stderr}");
    // So is a class of it that an annotation names: iterating its instance runs no query.
    std::fs::remove_dir_all(p.dir.join("pkg")).unwrap();
    p.write(
        "rows.py",
        "import sqlalchemy.orm\n\n\nasync def rows(q: sqlalchemy.orm.Query):\n    for row in q:\n        pass\n",
    );
    assert!(concise(&p).1.contains("rows.py:5:16: EFF101 "));
    p.write("sqlalchemy/orm.py", "print \"done\"\n");
    let (code, stdout, stderr) = p.check(&["--output-format", "concise"]);
    assert_eq!((code, stdout.as_str()), (2, ""), "{stderr}");
}

#[test]
fn syntax_newer_than_the_target_version_is_analyzed_not_reported() {
    // Reporting syntax the target cannot run is Ruff's single-file check; heki analyzes the
    // file with the newest grammar and the target's semantics.
    let p = Project::new("disc_target_syntax");
    p.write(
        "templates.py",
        "import time\n\n\nasync def render(name):\n    time.sleep(1)\n    return t\"hello {name}\"\n",
    );
    let (code, stdout, stderr) =
        p.check(&["--target-version", "py311", "--output-format", "concise"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stdout.contains("templates.py:5:5: EFF101 "), "{stdout}");
    assert!(!stderr.contains("syntax error"), "{stderr}");
}

#[test]
fn requires_python_exclusions_move_the_target_version() {
    let p = Project::new("disc_requires_python");
    p.write(
        "comps.py",
        "import requests\n\n\nasync def fetch_all(urls):\n    return [requests.get(u) for u in urls]\n",
    );
    // Python 3.11 runs the comprehension in a frame of its own (`<listcomp>` in the report);
    // 3.12+ inlines it (PEP 709).
    let runs_as_py311 = |spec: &str| {
        p.write(
            "pyproject.toml",
            &format!("[project]\nname = \"t\"\nrequires-python = \"{spec}\"\n"),
        );
        let (code, stdout, stderr) = p.check(&["--select", "EFF101"]);
        assert_eq!(code, 1, "{spec}: {stderr}");
        stdout.contains("<listcomp>")
    };
    assert!(runs_as_py311(">=3.11"));
    assert!(!runs_as_py311(">=3.11, !=3.11.*"));
    assert!(runs_as_py311("<3.12"));
    assert!(runs_as_py311(">=3.9, <3.11"));
}

#[test]
fn a_function_local_class_does_not_merge_with_a_module_class_of_its_name() {
    let p = Project::new("disc_local_class");
    p.write(
        "a.py",
        "import time\n\n\nclass Worker:\n    def run(self):\n        pass\n\n\ndef factory():\n    class Worker:\n        def run(self):\n            time.sleep(1)\n\n    return Worker\n\n\nasync def go():\n    Worker().run()\n",
    );
    assert_eq!(concise(&p), (0, String::new()));
    let (_, stdout, _) = p.heki(&["dump-ir", "a.py"]);
    assert!(
        stdout.contains("\"factory.<locals>.Worker.run\""),
        "{stdout}"
    );
}

#[test]
fn an_alias_spelling_the_first_component_binds_the_submodule() {
    let p = Project::new("disc_dotted_alias");
    p.write("pkg/__init__.py", "def wait():\n    pass\n")
        .write("pkg/io.py", BLOCKING_WAIT)
        .write(
            "app.py",
            "import pkg.io as pkg\n\n\nasync def go():\n    pkg.wait()\n",
        );
    let (code, stdout) = concise(&p);
    assert_eq!(code, 1);
    assert!(stdout.contains("app.py:5:5: EFF101 "), "{stdout}");
    let chain = full(&p);
    assert!(
        chain.contains("= related: app.py:5:5: `go` calls `pkg.io.wait`"),
        "{chain}"
    );
    // Without the alias `import pkg.io` binds `pkg`: its no-op `wait`.
    p.write(
        "app.py",
        "import pkg.io\n\n\nasync def go():\n    pkg.wait()\n",
    );
    assert_eq!(concise(&p), (0, String::new()));
}

/// Only what is `typing.TYPE_CHECKING` at run time guards type-only imports, on every clause
/// of an `if` statement.
#[test]
fn type_checking_guards_resolve_to_the_typing_flag_on_every_clause() {
    let p = Project::new("disc_type_checking");
    let blocking_pause = "\n\nasync def go():\n    pause(1)\n";
    // A class attribute that is true at run time: the import runs.
    p.write(
        "flags.py",
        &format!("class Flags:\n    TYPE_CHECKING = True\n\n\nif Flags.TYPE_CHECKING:\n    from time import sleep as pause\n{blocking_pause}"),
    )
    // `elif typing.TYPE_CHECKING:` never runs: the `else` binds `pause`.
    .write(
        "elif_guard.py",
        &format!("import sys\nimport typing\n\nif sys.version_info >= (9, 0):\n    pass\nelif typing.TYPE_CHECKING:\n    from time import sleep as pause\nelse:\n    def pause(n):\n        pass\n{blocking_pause}"),
    )
    // `else` of `not TYPE_CHECKING` is the type-only clause.
    .write(
        "negated.py",
        &format!("from typing import TYPE_CHECKING as TC\n\nif not TC:\n    def pause(n):\n        pass\nelse:\n    from time import sleep as pause\n{blocking_pause}"),
    )
    // The `TYPE_CHECKING = False` fallback is a guard too.
    .write(
        "fallback.py",
        &format!("try:\n    from typing import TYPE_CHECKING\nexcept ImportError:\n    TYPE_CHECKING = False\n\nif TYPE_CHECKING:\n    from time import sleep as pause\nelse:\n    def pause(n):\n        pass\n{blocking_pause}"),
    );
    let (code, stdout) = concise(&p);
    assert_eq!(code, 1);
    let reported: Vec<&str> = stdout
        .lines()
        .map(|l| l.split(':').next().unwrap())
        .collect();
    assert_eq!(reported, ["flags.py"], "{stdout}");
}

#[test]
fn a_later_definition_replaces_an_imported_name() {
    let p = Project::new("disc_import_then_def");
    let call = "\n\nasync def go():\n    sleep(0)\n";
    // The local no-op replaces the import before any call can run.
    p.write(
        "replaced.py",
        &format!("from time import sleep\n\n\ndef sleep(n):\n    pass\n{call}"),
    )
    // A conditional fallback, or a definition before the import, does not.
    .write(
        "fallback.py",
        &format!(
            "from time import sleep\n\nif not sleep:\n    def sleep(n):\n        pass\n{call}"
        ),
    )
    .write(
        "imported_last.py",
        &format!("def sleep(n):\n    pass\n\n\nfrom time import sleep\n{call}"),
    );
    let (code, stdout) = concise(&p);
    assert_eq!(code, 1);
    let reported: Vec<&str> = stdout
        .lines()
        .map(|l| l.split(':').next().unwrap())
        .collect();
    assert_eq!(reported, ["fallback.py", "imported_last.py"], "{stdout}");
}

#[test]
fn a_relative_import_past_the_top_level_package_binds_nothing() {
    let p = Project::new("disc_relative_beyond");
    p.write("pkg/__init__.py", "")
        .write(
            "pkg/mod.py",
            "def helper():\n    try:\n        from ..time import sleep\n    except ImportError:\n        return\n    sleep(1)\n\n\nasync def go():\n    helper()\n",
        )
        .write(
            "pkg/sub/__init__.py",
            "",
        )
        .write(
            "pkg/sub/inner.py",
            "from ..blocking import wait\n\n\nasync def go():\n    wait()\n",
        )
        .write("pkg/blocking.py", BLOCKING_WAIT);
    let (code, stdout) = concise(&p);
    assert_eq!(code, 1);
    // `from ..blocking` in `pkg.sub.inner` is `pkg.blocking`; `..time` in `pkg.mod` raises.
    assert!(stdout.contains("pkg/sub/inner.py:5:5: EFF101 "), "{stdout}");
    assert!(!stdout.contains("pkg/mod.py"), "{stdout}");
}

// Unix only: the directory is made unreadable with permission bits; on Windows that takes an
// access control list, which std cannot set.
#[cfg(unix)]
#[test]
fn an_excluded_directory_is_never_entered() {
    use std::os::unix::fs::PermissionsExt;
    let p = Project::new("disc_excluded_unreadable");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki]\nextend-exclude = [\"generated\"]\n",
    )
    .write(
        "a.py",
        "import time\n\n\nasync def go():\n    time.sleep(1)\n",
    )
    .write("generated/g.py", "x = 1\n");
    let generated = p.dir.join("generated");
    std::fs::set_permissions(&generated, std::fs::Permissions::from_mode(0o000)).unwrap();
    let result = concise(&p);
    std::fs::set_permissions(&generated, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (code, stdout) = result;
    assert_eq!(code, 1);
    assert!(stdout.contains("a.py:5:5: EFF101 "), "{stdout}");
}

#[test]
fn relative_imports_may_climb_to_a_project_root_that_is_a_package() {
    let p = Project::new("disc_root_package_relative");
    p.write("__init__.py", "")
        .write("blocking.py", BLOCKING_WAIT)
        .write("pkg/__init__.py", "")
        .write(
            "pkg/mod.py",
            "from ..blocking import wait\n\n\nasync def go():\n    wait()\n",
        );
    let (code, stdout) = concise(&p);
    assert_eq!(code, 1);
    assert!(stdout.contains("pkg/mod.py:5:5: EFF101 "), "{stdout}");
    // Without the root package the same import climbs past `pkg`.
    std::fs::remove_file(p.dir.join("__init__.py")).unwrap();
    assert_eq!(concise(&p), (0, String::new()));
}
