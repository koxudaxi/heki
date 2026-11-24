//! `IMP104`, `IMP301`, and `IMP302` end to end: package code importing development code, and a
//! distribution declared only for development or only in extras loaded while a package module or
//! a base module loads. Each is reported once at its import, in a src and a flat layout, and each
//! premise broken alone silences it.

mod common;

use common::Project;
use serde_json::Value;

const RULES: &str = "IMP104,IMP301,IMP302";

/// The reports of the three rules, as JSON.
fn diagnostics(p: &Project) -> Vec<Value> {
    let (code, out, err) = p.check(&["--select", RULES, "--output-format", "json"]);
    assert!((0..=1).contains(&code), "{code}: {out}\n{err}");
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"))
}

/// Each report as `path:line CODE [target]`.
fn reports(p: &Project) -> Vec<String> {
    (diagnostics(p).iter())
        .map(|d| {
            format!(
                "{}:{} {} [{}]",
                d["path"].as_str().unwrap(),
                d["line"],
                d["rule_code"].as_str().unwrap(),
                d["target"].as_str().unwrap(),
            )
        })
        .collect()
}

/// The one report at `path`, with its related locations as `path:line message`.
fn report_at(p: &Project, path: &str) -> (String, Vec<String>) {
    let found: Vec<Value> = (diagnostics(p).into_iter())
        .filter(|d| d["path"] == path)
        .collect();
    assert_eq!(found.len(), 1, "{found:#?}");
    let related = (found[0]["related"].as_array().unwrap().iter())
        .map(|r| {
            format!(
                "{}:{} {}",
                r["path"].as_str().unwrap(),
                r["line"],
                r["message"].as_str().unwrap()
            )
        })
        .collect();
    (found[0]["message"].as_str().unwrap().to_owned(), related)
}

fn project(name: &str, pyproject: &str, files: &[(&str, &str)]) -> Project {
    let p = Project::new(name);
    p.write("pyproject.toml", pyproject);
    for (path, text) in files {
        p.write(path, text);
    }
    p
}

const SRC_PYPROJECT: &str = r#"[project]
name = "acme"
dependencies = ["httpx", "psycopg2-binary", "opencv-python-headless"]

[project.optional-dependencies]
redis = ["redis"]
yaml = ["PyYAML"]

[project.scripts]
acme-serve = "acme.serve:main"

[dependency-groups]
test = ["pytest", "freezegun", "httpx", "opencv-python", "psycopg2"]
"#;

/// A src layout: the package `acme` under `src`, its tests and scripts beside it.
fn src_layout() -> Project {
    project(
        "import_dependencies_src",
        SRC_PYPROJECT,
        &[
            (
                "src/acme/__init__.py",
                "from acme.cache import Cache\nfrom acme import clock, guarded, lazy, typed\n",
            ),
            (
                "src/acme/cache.py",
                "import redis\n\n\nclass Cache:\n    pass\n",
            ),
            (
                "src/acme/clock.py",
                "from freezegun import freeze_time, tick\n",
            ),
            // Loaded by no base module: the extra is the price of importing it.
            ("src/acme/contrib.py", "import redis\nimport yaml\n"),
            (
                "src/acme/serve.py",
                "import yaml\n\n\ndef main():\n    pass\n",
            ),
            (
                "src/acme/guarded.py",
                "try:\n    import pytest\n    import redis\nexcept ImportError:\n    pytest = redis = None\n",
            ),
            (
                "src/acme/lazy.py",
                "def run():\n    import freezegun\n    import redis\n\n    return freezegun, redis\n",
            ),
            (
                "src/acme/typed.py",
                "from typing import TYPE_CHECKING\n\nif TYPE_CHECKING:\n    import pytest\n    import redis\n    from tests import factories\n",
            ),
            // Also a dependency, or provided by one.
            (
                "src/acme/installed.py",
                "import httpx\nimport psycopg2\nimport cv2\n",
            ),
            (
                "src/acme/report.py",
                "from tests.factories import make_order, make_user\n\n\ndef sample():\n    import scripts.seed\n\n    return make_order(), scripts.seed\n",
            ),
            (
                "src/acme/optional_tests.py",
                "try:\n    from tests import factories\nexcept ImportError:\n    factories = None\n",
            ),
            // Named like a test module, in the package's tree: it installs with the package.
            (
                "src/acme/runner.py",
                "from acme.test_runners import discover\nfrom acme.tests import helpers\n",
            ),
            (
                "src/acme/test_runners.py",
                "import pytest\n\n\ndef discover():\n    pass\n",
            ),
            ("src/acme/tests/__init__.py", ""),
            ("src/acme/tests/helpers.py", "import pytest\n"),
            ("tests/__init__.py", ""),
            (
                "tests/factories.py",
                "def make_order():\n    pass\n\n\ndef make_user():\n    pass\n",
            ),
            (
                "tests/test_cache.py",
                "import pytest\nfrom tests.factories import make_order\n",
            ),
            ("scripts/__init__.py", ""),
            ("scripts/seed.py", "import pytest\n"),
        ],
    )
}

#[test]
fn each_rule_reports_its_imports_in_a_src_layout() {
    let p = src_layout();
    assert_eq!(
        reports(&p),
        [
            "src/acme/cache.py:1 IMP302 [redis]",
            "src/acme/clock.py:1 IMP301 [freezegun]",
            "src/acme/report.py:1 IMP104 [tests.factories]",
            "src/acme/report.py:5 IMP104 [scripts.seed]",
            "src/acme/serve.py:1 IMP302 [pyyaml]",
            "src/acme/test_runners.py:1 IMP301 [pytest]",
            "src/acme/tests/helpers.py:1 IMP301 [pytest]",
        ]
    );
    let found = diagnostics(&p);
    assert_eq!(found[0]["certainty"], "heuristic");
    assert_eq!(found[0]["severity"], "error");
    let (message, related) = report_at(&p, "src/acme/cache.py");
    assert_eq!(
        message,
        "`<module>` imports `redis` while `acme.cache` loads, which importing `acme` loads, and \
         the project declares `redis`, which provides it, only in the extra `redis`: `import acme` \
         needs it"
    );
    assert_eq!(
        related,
        ["src/acme/__init__.py:1 `acme` imports `acme.cache` while it loads"]
    );
    // An entry point's module is a base module.
    let (message, related) = report_at(&p, "src/acme/serve.py");
    assert_eq!(
        message,
        "`<module>` imports `yaml` while `acme.serve` loads, and the project declares `pyyaml`, \
         which provides it, only in the extra `yaml`: `import acme.serve` needs it"
    );
    assert!(related.is_empty(), "{related:?}");
    let (message, _) = report_at(&p, "src/acme/clock.py");
    assert_eq!(
        message,
        "`<module>` imports `freezegun` while `acme.clock` loads, and the project declares \
         `freezegun`, which provides it, only in the development group `test`"
    );
    let report = (found.iter())
        .find(|d| d["target"] == "scripts.seed")
        .unwrap();
    assert_eq!(
        report["message"],
        "`sample` imports `scripts.seed` when it runs, a module outside the source \
         roots (`scripts/seed.py`) that the installed package does not include"
    );
    let report = (found.iter())
        .find(|d| d["target"] == "tests.factories")
        .unwrap();
    assert_eq!(
        report["message"],
        "`<module>` imports `tests.factories` while `acme.report` loads, test code outside the \
         package (`tests/factories.py`) that the installed package does not include"
    );
}

/// The rules are opt-in: `ALL` and a prefix do not run them.
#[test]
fn only_their_codes_select_them() {
    let p = src_layout();
    for select in ["ALL", "IMP"] {
        let (_, out, _) = p.check(&["--select", select, "--output-format", "json"]);
        let found: Vec<Value> = serde_json::from_str(&out).unwrap();
        assert!(
            !found.iter().any(|d| {
                ["IMP104", "IMP301", "IMP302"].contains(&d["rule_code"].as_str().unwrap())
            }),
            "{select}: {found:#?}"
        );
    }
}

/// `module-distributions` decides which distributions provide a module: `cv2` comes from the
/// development group's `opencv-python` alone, and `freezegun` from a dependency.
#[test]
fn the_users_map_decides_the_distribution() {
    let p = src_layout();
    p.write(
        "pyproject.toml",
        &format!(
            "{SRC_PYPROJECT}\n[tool.heki.imports]\nmodule-distributions = {{ cv2 = [\"opencv-python\"], freezegun = [\"httpx\"] }}\n"
        ),
    );
    assert_eq!(
        reports(&p),
        [
            "src/acme/cache.py:1 IMP302 [redis]",
            "src/acme/installed.py:3 IMP301 [opencv-python]",
            "src/acme/report.py:1 IMP104 [tests.factories]",
            "src/acme/report.py:5 IMP104 [scripts.seed]",
            "src/acme/serve.py:1 IMP302 [pyyaml]",
            "src/acme/test_runners.py:1 IMP301 [pytest]",
            "src/acme/tests/helpers.py:1 IMP301 [pytest]",
        ]
    );
}

/// Dependencies a build backend computes, or no declarations at all: nothing is known about
/// what installing the package installs.
#[test]
fn unknown_dependencies_give_no_dependency_report() {
    let p = src_layout();
    for pyproject in [
        SRC_PYPROJECT.replace(
            "dependencies = [\"httpx\", \"psycopg2-binary\", \"opencv-python-headless\"]",
            "dynamic = [\"dependencies\"]",
        ),
        "[tool.ruff]\nline-length = 100\n".to_owned(),
    ] {
        p.write("pyproject.toml", &pyproject);
        let found = reports(&p);
        assert!(
            !found.iter().any(|r| r.contains("IMP30")),
            "{pyproject}: {found:#?}"
        );
    }
}

/// A distribution in a development group and an extra is an extra's.
#[test]
fn a_distribution_in_an_extra_and_a_group_is_an_extras() {
    let p = src_layout();
    p.write(
        "pyproject.toml",
        &SRC_PYPROJECT.replace("redis = [\"redis\"]", "redis = [\"redis\", \"freezegun\"]"),
    );
    let found = reports(&p);
    assert!(
        found.contains(&"src/acme/clock.py:1 IMP302 [freezegun]".to_owned()),
        "{found:#?}"
    );
    assert!(
        !found.iter().any(|r| r.contains("IMP301 [freezegun]")),
        "{found:#?}"
    );
}

/// A handler around an import on the way from the base module: importing it does not fail.
#[test]
fn a_handled_import_on_the_way_breaks_the_chain() {
    let p = src_layout();
    p.write(
        "src/acme/__init__.py",
        "try:\n    from acme.cache import Cache\nexcept Exception:\n    Cache = None\n",
    );
    let found = reports(&p);
    assert!(!found.iter().any(|r| r.contains("cache.py")), "{found:#?}");
    // On one branch only, it need not run.
    p.write(
        "src/acme/__init__.py",
        "import sys\n\nif sys.platform == \"win32\":\n    from acme.cache import Cache\n",
    );
    let found = reports(&p);
    assert!(!found.iter().any(|r| r.contains("cache.py")), "{found:#?}");
}

const FLAT_PYPROJECT: &str = r#"[project]
name = "acme"
dependencies = []

[project.optional-dependencies]
redis = ["redis"]

[tool.poetry.group.dev.dependencies]
pytest = "*"
"#;

/// A flat layout: the package `acme` named like the project, a top-level module beside it, and
/// tests.
fn flat_layout() -> Project {
    project(
        "import_dependencies_flat",
        FLAT_PYPROJECT,
        &[
            (
                "acme/__init__.py",
                "import helpers\nfrom acme import store\n",
            ),
            (
                "acme/store.py",
                "import redis\nfrom tests import conftest\n",
            ),
            ("helpers.py", "import pytest\n"),
            (
                "other/__init__.py",
                "import pytest\nimport tests.conftest\n",
            ),
            ("tests/__init__.py", ""),
            ("tests/conftest.py", "import pytest\n"),
        ],
    )
}

/// A module of the project that is not package code is judged when a package module loads it:
/// the report names that module, with the import leading there. Test code stops the chain: an
/// installed package does not have `tests.conftest`, so its import of `pytest` never runs there.
#[test]
fn each_rule_reports_its_imports_in_a_flat_layout() {
    let p = flat_layout();
    assert_eq!(
        reports(&p),
        [
            "acme/store.py:1 IMP302 [redis]",
            "acme/store.py:2 IMP104 [tests.conftest]",
            "helpers.py:1 IMP301 [pytest]",
        ]
    );
    let (message, related) = report_at(&p, "helpers.py");
    assert_eq!(
        message,
        "`<module>` imports `pytest` while `helpers` loads, which the package module `acme` \
         loads while it loads, and the project declares `pytest`, which provides it, only in \
         the development group `dev`"
    );
    assert_eq!(
        related,
        ["acme/__init__.py:1 `acme` imports `helpers` while it loads"]
    );
}

/// A flat layout whose package is named nowhere heki reads: no module is package code.
#[test]
fn package_code_heki_cannot_name_gives_no_report() {
    let p = flat_layout();
    p.write(
        "pyproject.toml",
        &FLAT_PYPROJECT.replace("\"acme\"", "\"acme-dist\""),
    );
    assert_eq!(reports(&p), Vec::<String>::new());
}

/// Several declared distributions may provide one module, or one sits in several groups or
/// extras: the report names them all.
#[test]
fn several_distributions_and_places_are_named() {
    let p = project(
        "import_dependencies_several",
        r#"[project]
name = "acme"
dependencies = []

[project.optional-dependencies]
cv = ["opencv-python"]
headless = ["opencv-python-headless"]

[dependency-groups]
test = ["pytest", "ruamel", "ruamel.yaml"]
lint = ["pytest"]
"#,
        &[(
            "acme/__init__.py",
            "import cv2\nimport pytest\nimport ruamel.yaml\n",
        )],
    );
    let messages: Vec<String> = (diagnostics(&p).iter())
        .map(|d| d["message"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        messages,
        [
            "`<module>` imports `cv2` while `acme` loads, and the project declares \
             `opencv-python` and `opencv-python-headless`, which may provide it, only in the \
             extras `cv` and `headless`: `import acme` needs them",
            "`<module>` imports `pytest` while `acme` loads, and the project declares `pytest`, \
             which provides it, only in the development groups `lint` and `test`",
            "`<module>` imports `ruamel.yaml` while `acme` loads, and the project declares \
             `ruamel` and `ruamel-yaml`, which may provide it, only in the development group \
             `test`",
        ]
    );
}

/// A test module in the package's tree installs with it: importing it is no import of
/// development code, and what its load loads is judged with the package module loading it.
#[test]
fn test_modules_in_the_package_install_with_it() {
    let p = src_layout();
    let (message, related) = report_at(&p, "src/acme/test_runners.py");
    assert_eq!(
        message,
        "`<module>` imports `pytest` while `acme.test_runners` loads, which the package module \
         `acme.runner` loads while it loads, and the project declares `pytest`, which provides \
         it, only in the development group `test`"
    );
    assert_eq!(
        related,
        ["src/acme/runner.py:1 `acme.runner` imports `acme.test_runners` while it loads"]
    );
    let found = reports(&p);
    assert!(!found.iter().any(|r| r.contains("runner.py")), "{found:#?}");
}

/// The project's own name in a group adds that group's extras, no distribution, and a copy of a
/// module the project holds in a directory that is no package (a vendored package the code may
/// put on `sys.path`) may be the one an import finds: neither gives a report. `acme` is a
/// namespace package whose `acme.plugins` another distribution provides.
#[test]
fn the_projects_own_name_and_copies_provide_nothing() {
    let p = project(
        "import_dependencies_own",
        r#"[project]
name = "acme"
dependencies = []

[project.optional-dependencies]
redis = ["redis"]

[dependency-groups]
dev = ["acme[redis]", "packaging", "pytest"]
"#,
        &[
            (
                "acme/core.py",
                "import acme.plugins\nimport packaging\nimport pytest\n",
            ),
            ("acme/_vendor/packaging/__init__.py", ""),
        ],
    );
    assert_eq!(reports(&p), ["acme/core.py:3 IMP301 [pytest]"]);
}
