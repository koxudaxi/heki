//! What kind of code a project file holds (ADR-022): test code, development code, and the code
//! of the packages the project distributes. The rules and contracts ask here; none decides it
//! itself.

use crate::resolver::ModuleLayout;
use heki_env::{HekiConfig, normalize_distribution};
use serde::Serialize;

/// The kind of code of a project module ([`code_kind`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CodeKind {
    /// Code of a package the project distributes.
    Package,
    /// A test module ([`is_test_module`]) inside the tree of a package the project distributes
    /// (`src/acme/tests/`, `acme/test_io.py`): it installs with the package, but is no code
    /// of it the package's users load.
    PackageTest,
    /// A test module outside the packages the project distributes, or a module outside the
    /// source roots of a src layout (`scripts/`, `tools/`, `examples/` run from the project
    /// root): no installed distribution holds it.
    Development,
    /// heki cannot tell: the project names its packages nowhere heki reads.
    Open,
}

impl CodeKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Package => "package",
            Self::PackageTest => "package test",
            Self::Development => "development",
            Self::Open => "open",
        }
    }
}

/// Test code as pytest discovers it by default: a `test_*` function (a method of a test class
/// included) and the functions nested in one, and every function of a `test_*.py` or
/// `*_test.py` file or of a file under a `test/` or `tests/` directory.
pub fn is_test_code(qualname: &str, rel_path: &str) -> bool {
    in_test_location(rel_path) || qualname.split('.').any(|part| part.starts_with("test_"))
}

/// A test module: one of a file pytest collects tests from by default (under a `test/` or
/// `tests/` directory, `test_*.py`, `*_test.py`) or reads fixtures from (`conftest.py`).
pub fn is_test_module(rel_path: &str) -> bool {
    in_test_location(rel_path) || file_stem(rel_path) == "conftest"
}

/// The kind of code of the module in the project file `rel_path` (POSIX, relative to the
/// project root), named `module`. A file is in the tree of a package the project distributes
/// when the project is a src layout and the file is below a source root, or when its top-level
/// package is named like the project (`[project] name`, normalized) or named by the build
/// backend's settings ([`heki_env::Declared::packages`]): it is the package's code, or, as a
/// test module, [`CodeKind::PackageTest`]. Outside every source root of a src layout, and a
/// test module outside the package's tree, it is development code. Anything else is open: a
/// workspace's members, and a flat layout whose packages are named nowhere heki reads.
pub fn code_kind(
    rel_path: &str,
    module: &str,
    layout: &ModuleLayout,
    cfg: &HekiConfig,
) -> CodeKind {
    let test = is_test_module(rel_path);
    let in_package = match package_tree(rel_path, module, layout, cfg) {
        Some(in_package) => in_package,
        None if test => false,
        None => return CodeKind::Open,
    };
    match (in_package, test) {
        (true, false) => CodeKind::Package,
        (true, true) => CodeKind::PackageTest,
        (false, _) => CodeKind::Development,
    }
}

/// Whether the file `rel_path` of the module `module` is in the tree of a package the project
/// distributes ([`code_kind`]); `None` when heki cannot tell.
fn package_tree(
    rel_path: &str,
    module: &str,
    layout: &ModuleLayout,
    cfg: &HekiConfig,
) -> Option<bool> {
    // The project's own src layout: its roots sit at the project root, with no workspace
    // member's roots or packages beside them.
    let src_layout = !layout.source_roots.is_empty()
        && layout.member_packages.is_empty()
        && layout.source_roots.iter().all(|root| !root.contains('/'));
    if src_layout {
        let below = |root: &String| {
            rel_path
                .strip_prefix(root.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
        };
        return Some(layout.source_roots.iter().any(below));
    }
    let top = normalize_distribution(module.split('.').next().unwrap_or(module));
    let named = cfg
        .project_name
        .as_deref()
        .is_some_and(|name| normalize_distribution(name) == top);
    let built = (cfg.declared.iter())
        .flat_map(|d| &d.packages)
        .any(|package| normalize_distribution(package) == top);
    (named || built).then_some(true)
}

/// Under a `test/` or `tests/` directory, or a `test_*.py` or `*_test.py` file.
fn in_test_location(rel_path: &str) -> bool {
    let in_tests = rel_path
        .rsplit_once('/')
        .is_some_and(|(dirs, _)| dirs.split('/').any(|c| c == "tests" || c == "test"));
    let stem = file_stem(rel_path);
    in_tests || stem.starts_with("test_") || stem.ends_with("_test")
}

/// The file name of `rel_path` without `.py`.
fn file_stem(rel_path: &str) -> &str {
    let file = rel_path.rsplit('/').next().unwrap_or(rel_path);
    file.strip_suffix(".py").unwrap_or(file)
}

#[cfg(test)]
mod tests {
    use super::*;
    use heki_env::Declared;

    #[test]
    fn test_code_and_test_modules() {
        for (qualname, path, test, module) in [
            ("test_login", "app/views.py", true, false),
            ("TestLogin.test_redirect", "app/views.py", true, false),
            ("test_login.<locals>.helper", "app/views.py", true, false),
            ("handler", "src/test/api.py", true, true),
            ("handler", "evals/web/test_flows.py", true, true),
            ("handler", "app/testing.py", false, false),
            ("contest", "app/contest/views.py", false, false),
            ("run", "pkg/core.py", false, false),
            ("test_run", "pkg/core.py", true, false),
            ("TestCore.test_run.inner", "pkg/core.py", true, false),
            ("run", "tests/helpers.py", true, true),
            ("run", "pkg/test/data.py", true, true),
            ("run", "pkg/test_core.py", true, true),
            ("run", "pkg/core_test.py", true, true),
            ("run", "conftest.py", false, true),
            ("run", "pkg/conftest.py", false, true),
            ("run", "tests.py", false, false),
            ("run", "testing/core.py", false, false),
        ] {
            assert_eq!(is_test_code(qualname, path), test, "{qualname} {path}");
            assert_eq!(is_test_module(path), module, "{path}");
        }
    }

    #[test]
    fn package_development_and_open_code() {
        let layout = |roots: &[&str], members: &[&str]| ModuleLayout {
            source_roots: roots.iter().map(|r| (*r).to_owned()).collect(),
            member_packages: members.iter().map(|r| (*r).to_owned()).collect(),
            root_is_package: false,
        };
        let cfg = HekiConfig {
            project_name: Some("Acme-Core".to_owned()),
            declared: Some(Declared {
                packages: ["acme_tools".to_owned()].into(),
                ..Declared::default()
            }),
            ..HekiConfig::default()
        };
        let src = layout(&["src"], &[]);
        let flat = layout(&[], &[]);
        let workspace = layout(&["src", "packages/b/src"], &[]);
        for (path, module, layout, kind) in [
            ("src/acme/io.py", "acme.io", &src, CodeKind::Package),
            (
                "src/acme/tests/test_io.py",
                "acme.tests.test_io",
                &src,
                CodeKind::PackageTest,
            ),
            (
                "tests/test_io.py",
                "tests.test_io",
                &src,
                CodeKind::Development,
            ),
            (
                "acme_core/test_io.py",
                "acme_core.test_io",
                &flat,
                CodeKind::PackageTest,
            ),
            (
                "scripts/release.py",
                "scripts.release",
                &src,
                CodeKind::Development,
            ),
            ("srcx/tool.py", "srcx.tool", &src, CodeKind::Development),
            ("acme_core/io.py", "acme_core.io", &flat, CodeKind::Package),
            (
                "acme_tools/run.py",
                "acme_tools.run",
                &flat,
                CodeKind::Package,
            ),
            ("other/run.py", "other.run", &flat, CodeKind::Open),
            ("conftest.py", "conftest", &flat, CodeKind::Development),
            ("tools/run.py", "tools.run", &workspace, CodeKind::Open),
        ] {
            assert_eq!(code_kind(path, module, layout, &cfg), kind, "{path}");
        }
        assert_eq!(
            code_kind("acme/io.py", "acme.io", &flat, &HekiConfig::default()),
            CodeKind::Open
        );
        assert_eq!(CodeKind::Development.as_str(), "development");
        assert_eq!(CodeKind::PackageTest.as_str(), "package test");
    }
}
