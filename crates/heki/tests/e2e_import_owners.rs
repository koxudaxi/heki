//! `import-owners` contracts end to end (`IMP201`, and `IMP202` with `unresolved = "report"`):
//! which modules a module may import, wherever the import runs (module body, class body,
//! function, by a constant name), with a `from` import of a submodule counted as an import of
//! the submodule, `TYPE_CHECKING` imports only with the key, with `reach` the modules a chain of
//! imports loads, and `relative-imports`, the modules an importer may name only relatively.

mod common;

use common::Project;
use serde_json::Value;

/// `heki check --output-format json`: the reports, each as `path:line CODE qualname target`.
fn reports(p: &Project) -> Vec<String> {
    diagnostics(p)
        .iter()
        .map(|d| {
            format!(
                "{}:{} {} {} {}",
                d["path"].as_str().unwrap(),
                d["line"],
                d["rule_code"].as_str().unwrap(),
                d["qualname"].as_str().unwrap(),
                d["target"].as_str().unwrap()
            )
        })
        .collect()
}

fn diagnostics(p: &Project) -> Vec<Value> {
    let (code, out, err) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{code}: {out}\n{err}");
    let all: Vec<Value> = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    (all.into_iter())
        .filter(|d| {
            d["rule_code"]
                .as_str()
                .is_some_and(|c| c.starts_with("IMP"))
        })
        .collect()
}

/// The message of the one report at `path:line`.
fn message(p: &Project, at: &str) -> String {
    let found: Vec<Value> = (diagnostics(p).into_iter())
        .filter(|d| format!("{}:{}", d["path"].as_str().unwrap(), d["line"]) == at)
        .collect();
    assert_eq!(found.len(), 1, "{at}: {found:#?}");
    found[0]["message"].as_str().unwrap().to_owned()
}

/// The related locations of the one report at `path:line`, each as `path:line message`.
fn related(p: &Project, at: &str) -> Vec<String> {
    let found: Vec<Value> = (diagnostics(p).into_iter())
        .filter(|d| format!("{}:{}", d["path"].as_str().unwrap(), d["line"]) == at)
        .collect();
    assert_eq!(found.len(), 1, "{at}: {found:#?}");
    (found[0]["related"].as_array().unwrap().iter())
        .map(|r| {
            format!(
                "{}:{} {}",
                r["path"].as_str().unwrap(),
                r["line"],
                r["message"].as_str().unwrap()
            )
        })
        .collect()
}

fn project(name: &str, contracts: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!("[project]\nname = \"t\"\nrequires-python = \">=3.11\"\n\n{contracts}"),
    );
    p
}

const H6_CONTRACTS: &str = r#"
[[tool.heki.import-owners]]
name = "facts-leaf"
importers = ["pkg.facts"]
allowed-imports = ["<stdlib>", "pkg._records"]

[[tool.heki.import-owners]]
name = "shared-model-backends"
importers = ["pkg.model.shared"]
forbidden-imports = ["pkg.model.backend", "pkg.model.backend.*"]

[[tool.heki.import-owners]]
name = "no-root-import"
importers = ["pkg.*"]
forbidden-imports = ["pkg"]
"#;

const SHARED: &str = r#"import importlib


def uses_the_class(value: object) -> bool:
    from pkg.model.backend import BackendModel

    return isinstance(value, BackendModel)


def imports_for_its_effect() -> None:
    import pkg.model.backend  # noqa: F401


def imports_by_name() -> object:
    return importlib.import_module("pkg.model.backend")


def imports_the_root() -> object:
    import pkg

    return pkg
"#;

/// The H6 request: a leaf module that may import only the standard library and one project
/// module, shared model code that never imports a backend however it imports it, and no module
/// importing the package root. Exactly five reports; `import pkg.model.backend` loads `pkg`
/// first but is no import of it.
#[test]
fn the_three_contracts_of_the_request_give_five_reports() {
    let p = project("imp_h6", H6_CONTRACTS);
    p.write("pkg/__init__.py", "");
    p.write(
        "pkg/facts.py",
        "import json\n\nimport pkg.model.backend\n\n\ndef table() -> str:\n    return json.dumps({})\n",
    );
    p.write("pkg/model/__init__.py", "");
    p.write(
        "pkg/model/backend.py",
        "class BackendModel:\n    pass\n\n\ndef backend_option() -> str:\n    return \"backend\"\n",
    );
    p.write("pkg/model/shared.py", SHARED);
    assert_eq!(
        reports(&p),
        [
            "pkg/facts.py:3 IMP201 <module> pkg.model.backend",
            "pkg/model/shared.py:5 IMP201 uses_the_class pkg.model.backend",
            "pkg/model/shared.py:11 IMP201 imports_for_its_effect pkg.model.backend",
            "pkg/model/shared.py:15 IMP201 imports_by_name pkg.model.backend",
            "pkg/model/shared.py:19 IMP201 imports_the_root pkg",
        ]
    );
    assert_eq!(
        message(&p, "pkg/facts.py:3"),
        "`<module>` imports `pkg.model.backend` while `pkg.facts` loads, not among the allowed \
         imports of `facts-leaf`"
    );
    assert_eq!(
        message(&p, "pkg/model/shared.py:15"),
        "`imports_by_name` imports `pkg.model.backend` by name when it runs, a forbidden import \
         of `shared-model-backends`"
    );
}

/// Outside a package, only its public entry: every module but the package's own may import
/// `app.billing`, and none of its submodules, however the import names them.
#[test]
fn modules_outside_a_package_import_only_its_entry() {
    let p = project(
        "imp_entry",
        r#"
[[tool.heki.import-owners]]
name = "billing-entry"
exempt-importers = ["app.billing", "app.billing.*"]
forbidden-imports = ["app.billing.*"]
message = "import `app.billing` and use what it exports"
"#,
    );
    p.write("app/__init__.py", "");
    p.write(
        "app/billing/__init__.py",
        "from app.billing.invoice import Invoice\n\n__all__ = [\"Invoice\"]\n",
    );
    p.write("app/billing/invoice.py", "class Invoice:\n    pass\n");
    p.write("app/billing/tax.py", "from app.billing import invoice\n");
    p.write(
        "app/orders.py",
        "import app.billing\nfrom app.billing import Invoice\nfrom app.billing import invoice\nimport app.billing.invoice\nfrom app.billing.invoice import Invoice as Direct\n",
    );
    assert_eq!(
        reports(&p),
        [
            "app/orders.py:3 IMP201 <module> app.billing.invoice",
            "app/orders.py:4 IMP201 <module> app.billing.invoice",
            "app/orders.py:5 IMP201 <module> app.billing.invoice",
        ]
    );
    let d = &diagnostics(&p)[0];
    assert_eq!(d["help"], "import `app.billing` and use what it exports");
}

/// Groups: `<stdlib>`, `<project>`, and `<third-party>`, with a contract allowing no imports at
/// all when it sets neither list.
#[test]
fn groups_name_the_standard_library_the_project_and_the_rest() {
    let p = project(
        "imp_groups",
        r#"
[[tool.heki.import-owners]]
name = "stdlib-only"
importers = ["pkg.leaf"]
allowed-imports = ["<stdlib>"]

[[tool.heki.import-owners]]
name = "no-third-party"
importers = ["pkg.core"]
forbidden-imports = ["<third-party>"]

[[tool.heki.import-owners]]
name = "inside"
importers = ["pkg.inner"]
allowed-imports = ["<project>"]

[[tool.heki.import-owners]]
name = "sealed"
importers = ["pkg.sealed"]
unresolved = "report"
"#,
    );
    p.write("pkg/__init__.py", "");
    p.write("pkg/util.py", "");
    p.write(
        "pkg/leaf.py",
        "import json\nimport os.path\nimport requests\nimport pkg.util\n",
    );
    p.write(
        "pkg/core.py",
        "import json\nimport requests\nimport pkg.util\n",
    );
    p.write("pkg/inner.py", "import json\nimport pkg.util\n");
    p.write(
        "pkg/sealed.py",
        "import json\n\n\ndef load(name: str) -> object:\n    return __import__(f\"pkg.{name}\")\n\n\ndef load_any(name: str) -> object:\n    return __import__(name)\n",
    );
    assert_eq!(
        reports(&p),
        [
            "pkg/core.py:2 IMP201 <module> requests",
            "pkg/inner.py:1 IMP201 <module> json",
            "pkg/leaf.py:3 IMP201 <module> requests",
            "pkg/leaf.py:4 IMP201 <module> pkg.util",
            "pkg/sealed.py:1 IMP201 <module> json",
            "pkg/sealed.py:5 IMP202 load pkg.*",
            "pkg/sealed.py:9 IMP202 load_any *",
        ]
    );
    assert_eq!(
        message(&p, "pkg/sealed.py:1"),
        "`<module>` imports `json` while `pkg.sealed` loads, and `sealed` allows no imports"
    );
    assert_eq!(
        message(&p, "pkg/sealed.py:9"),
        "`load_any` imports a module heki cannot name by name when it runs, and `sealed` allows \
         no imports"
    );
}

/// With both lists, the forbidden imports are reported less the allowed ones; `exempt-importers`
/// and `ignored-imports` take importers and single imports out.
#[test]
fn allowed_imports_and_ignored_pairs_carve_out_of_forbidden_ones() {
    let p = project(
        "imp_carve",
        r#"
[[tool.heki.import-owners]]
name = "views"
importers = ["pkg.views.*"]
exempt-importers = ["pkg.views.admin"]
forbidden-imports = ["pkg.db*"]
allowed-imports = ["pkg.db.api"]
ignored-imports = ["pkg.views.legacy -> pkg.db.session", "pkg.views.* -> <stdlib>"]
"#,
    );
    p.write("pkg/__init__.py", "");
    p.write("pkg/db/__init__.py", "");
    p.write("pkg/db/api.py", "");
    p.write("pkg/db/session.py", "");
    p.write("pkg/views/__init__.py", "");
    let body = "import pkg.db.api\nimport pkg.db.session\n";
    p.write("pkg/views/home.py", body);
    p.write("pkg/views/legacy.py", body);
    p.write("pkg/views/admin.py", body);
    assert_eq!(
        reports(&p),
        ["pkg/views/home.py:2 IMP201 <module> pkg.db.session"]
    );
}

/// Every runtime class counts (a class body, a lazy import, a guarded one, a star import, a
/// relative import), an import under `TYPE_CHECKING` only with `type-checking = "report"`, and
/// an import of a module the project lacks below its package is none.
#[test]
fn every_import_that_runs_counts_and_type_checking_ones_with_the_key() {
    let contract = |key: &str| {
        format!(
            "[tool.heki]\ntarget-version = \"py315\"\n\n[[tool.heki.import-owners]]\nname = \"no-db\"\nimporters = [\"pkg.user\", \"pkg.opened\"]\nforbidden-imports = [\"pkg.db\"]\n{key}\n"
        )
    };
    let user = r#"from typing import TYPE_CHECKING

if TYPE_CHECKING:
    import pkg.db

try:
    import pkg.db
except ImportError:
    pass

lazy import pkg.db

from pkg.db import *
from . import db
import pkg.gone


class Holder:
    from .db import query
"#;
    for (key, expected) in [
        ("", vec![7, 11, 13, 14, 19]),
        ("type-checking = \"report\"", vec![4, 7, 11, 13, 14, 19]),
    ] {
        let p = project("imp_timing", &contract(key));
        p.write("pkg/__init__.py", "");
        p.write("pkg/db.py", "def query() -> None:\n    pass\n");
        p.write("pkg/user.py", user);
        // A list heki cannot read leaves open whether the import is lazy: it runs either way.
        p.write(
            "pkg/opened.py",
            "__lazy_modules__ = list(\"ab\")\nimport pkg.db\n",
        );
        let lines: Vec<String> =
            std::iter::once("pkg/opened.py:2 IMP201 <module> pkg.db".to_owned())
                .chain(
                    expected
                        .iter()
                        .map(|l| format!("pkg/user.py:{l} IMP201 <module> pkg.db")),
                )
                .collect();
        assert_eq!(reports(&p), lines, "{key}");
        assert_eq!(
            message(&p, "pkg/opened.py:2"),
            "`<module>` imports `pkg.db`, a forbidden import of `no-db`"
        );
        if !key.is_empty() {
            assert!(
                message(&p, "pkg/user.py:4").contains("under `TYPE_CHECKING`"),
                "{key}"
            );
            assert!(
                message(&p, "pkg/user.py:11").contains("at its first use"),
                "{key}"
            );
        }
    }
}

/// `from pkg import parse` imports the submodule only when the package binds no such name: a
/// package re-exporting its module's function leaves the import one of the package.
#[test]
fn a_from_import_of_a_name_the_package_binds_imports_the_package() {
    let p = project(
        "imp_from",
        r#"
[[tool.heki.import-owners]]
name = "no-parse-module"
importers = ["app"]
forbidden-imports = ["pkg.parse", "pkg.lex"]
"#,
    );
    p.write("pkg/__init__.py", "from .parse import parse\n");
    p.write("pkg/parse.py", "def parse() -> None:\n    pass\n");
    p.write("pkg/lex.py", "");
    p.write("app.py", "from pkg import parse\nfrom pkg import lex\n");
    assert_eq!(reports(&p), ["app.py:2 IMP201 <module> pkg.lex"]);
}

const BY_NAME: &str = r#"import importlib
import sys

from yaml import loader


def both(flag: bool) -> object:
    return importlib.import_module("pkg.db" if flag else "pkg.cache")


def mixed(flag: bool) -> object:
    return importlib.import_module("pkg.db" if flag else "json")


def named(name: str) -> object:
    return importlib.import_module(name)


def plugin(name: str) -> object:
    return importlib.import_module(f"pkg.plugins.{name}")


def harmless(name: str) -> object:
    return importlib.import_module(f"json.{name}")


def dunder() -> object:
    return __import__("pkg.db")


def gone(flag: bool) -> object:
    return importlib.import_module("pkg.db" if flag else "pkg.gone")


def entry(flag: bool) -> object:
    return importlib.import_module(f"pkg.plugins.{sys.argv[1]}" if flag else "json")


def standard(flag: bool) -> object:
    return importlib.import_module("json" if flag else "os")


def nearby(flag: bool, name: str) -> object:
    return importlib.import_module(f"json.{name}" if flag else "pkg.db")


from requests import adapters
"#;

/// By-name imports: one known name, several names that all break the contract, and, with
/// `unresolved = "report"`, the ones heki cannot name whole (`IMP202`): some names breaking it,
/// a name the code does not fix, a head of a name, and an external `from a import c`.
#[test]
fn by_name_imports_are_exact_only_when_every_name_breaks_the_contract() {
    let contract = |unresolved: &str| {
        format!(
            r#"
[[tool.heki.import-owners]]
name = "app-imports"
importers = ["app"]
forbidden-imports = ["pkg.db", "pkg.cache", "pkg.plugins.*", "yaml.loader", "requests"]
{unresolved}
"#
        )
    };
    for (unresolved, expected) in [
        (
            "",
            vec![
                "app.py:8 IMP201 both pkg.cache|pkg.db",
                "app.py:28 IMP201 dunder pkg.db",
                "app.py:47 IMP201 <module> requests",
            ],
        ),
        (
            "unresolved = \"report\"",
            vec![
                "app.py:4 IMP202 <module> yaml.loader",
                "app.py:8 IMP201 both pkg.cache|pkg.db",
                "app.py:12 IMP202 mixed json|pkg.db",
                "app.py:16 IMP202 named *",
                "app.py:20 IMP202 plugin pkg.plugins.*",
                "app.py:28 IMP201 dunder pkg.db",
                "app.py:32 IMP202 gone pkg.db|pkg.gone",
                "app.py:36 IMP202 entry json|pkg.plugins.*",
                "app.py:44 IMP202 nearby pkg.db|json.*",
                "app.py:47 IMP201 <module> requests",
            ],
        ),
    ] {
        let p = project("imp_by_name", &contract(unresolved));
        p.write("pkg/__init__.py", "");
        p.write("pkg/db.py", "");
        p.write("pkg/cache.py", "");
        p.write("pkg/plugins/__init__.py", "");
        p.write("app.py", BY_NAME);
        assert_eq!(reports(&p), expected, "{unresolved}");
        if !unresolved.is_empty() {
            assert_eq!(
                message(&p, "app.py:4"),
                "`<module>` imports `yaml.loader`, or only `yaml` while `app` loads, which may be \
                 a forbidden import of `app-imports`"
            );
            assert_eq!(
                message(&p, "app.py:20"),
                "`plugin` imports a module whose name starts with `pkg.plugins.` by name when it \
                 runs, which may be a forbidden import of `app-imports`"
            );
            assert_eq!(
                message(&p, "app.py:12"),
                "`mixed` imports `json` or `pkg.db` by name when it runs, which may be a \
                 forbidden import of `app-imports`"
            );
        }
    }
}

/// An allowed list that covers every module under the head of a by-name import leaves it
/// unreported, and one that does not reports it as open.
#[test]
fn a_head_is_open_unless_the_allowed_imports_cover_it() {
    let contract = |allowed: &str| {
        format!(
            "[[tool.heki.import-owners]]\nname = \"plugins\"\nimporters = [\"app\"]\nallowed-imports = [{allowed}]\nunresolved = \"report\"\n"
        )
    };
    for (allowed, expected) in [
        (r#""<stdlib>", "pkg.plugins.*""#, vec![]),
        (r#""<stdlib>", "pkg.*""#, vec![]),
        (
            r#""<stdlib>", "pkg.plugins.a""#,
            vec!["app.py:5 IMP202 plugin pkg.plugins.*"],
        ),
        (
            r#""<stdlib>", "<project>""#,
            vec!["app.py:5 IMP202 plugin pkg.plugins.*"],
        ),
    ] {
        let p = project("imp_head", &contract(allowed));
        p.write("pkg/__init__.py", "");
        p.write("pkg/plugins/__init__.py", "");
        p.write(
            "app.py",
            "import importlib\n\n\ndef plugin(name: str) -> object:\n    return importlib.import_module(f\"pkg.plugins.{name}\")\n",
        );
        assert_eq!(reports(&p), expected, "{allowed}");
    }
}

/// A forbidden list none of whose patterns can match under a head leaves the by-name import
/// unreported: a literal outside the head, a glob whose literal start leaves it.
#[test]
fn a_head_no_forbidden_pattern_can_match_is_no_report() {
    for (forbidden, reported) in [
        (r#""pkg.db""#, false),
        (r#""pkg.plugins""#, false),
        (r#""pkg.plugins.alpha""#, true),
        (r#""pkg.db.*""#, false),
        (r#""pkg.*""#, true),
        (r#""<project>""#, true),
    ] {
        let p = project(
            "imp_head_forbidden",
            &format!(
                "[[tool.heki.import-owners]]\nname = \"plugins\"\nimporters = [\"app\"]\nforbidden-imports = [{forbidden}]\nunresolved = \"report\"\n"
            ),
        );
        p.write("pkg/__init__.py", "");
        p.write("pkg/db.py", "");
        p.write("pkg/plugins/__init__.py", "");
        p.write(
            "app.py",
            "import importlib\n\n\ndef plugin(name: str) -> object:\n    return importlib.import_module(f\"pkg.plugins.{name}\")\n",
        );
        let expected: Vec<&str> = if reported {
            vec!["app.py:5 IMP202 plugin pkg.plugins.*"]
        } else {
            vec![]
        };
        assert_eq!(reports(&p), expected, "{forbidden}");
    }
}

const CLI: &str = r#"import pkg.models.base


def run() -> None:
    import pkg.helpers
"#;

/// `reach = "import-time"`: what loading the importer loads while it loads, through the parent
/// `__init__` files of what it imports, each reached module once with a shortest chain; an
/// import inside a function runs later. `reach = "all"` follows every import that runs.
#[test]
fn reach_follows_the_chains_of_imports() {
    let contract = |reach: &str| {
        format!(
            "[[tool.heki.import-owners]]\nname = \"startup\"\nimporters = [\"pkg.cli\"]\nforbidden-imports = [\"torch\", \"pkg.heavy\"]\nreach = \"{reach}\"\n"
        )
    };
    let write = |p: &Project| {
        p.write("pkg/__init__.py", "");
        p.write("pkg/cli.py", CLI);
        p.write("pkg/models/__init__.py", "import torch.nn\n");
        p.write("pkg/models/base.py", "import pkg.heavy\n");
        p.write("pkg/heavy.py", "");
        p.write("pkg/helpers.py", "from pkg import heavy\n");
    };
    let p = project("imp_reach", &contract("import-time"));
    write(&p);
    assert_eq!(
        reports(&p),
        [
            "pkg/cli.py:1 IMP201 <module> pkg.heavy",
            "pkg/cli.py:1 IMP201 <module> torch",
        ]
    );
    let torch = (diagnostics(&p).into_iter())
        .find(|d| d["target"] == "torch")
        .unwrap();
    assert_eq!(
        torch["message"],
        "`<module>` imports `pkg.models.base` while `pkg.cli` loads, and `torch` through it, a \
         forbidden import of `startup`"
    );
    let steps: Vec<String> = (torch["related"].as_array().unwrap().iter())
        .map(|r| {
            format!(
                "{}:{} {}",
                r["path"].as_str().unwrap(),
                r["line"],
                r["message"].as_str().unwrap()
            )
        })
        .collect();
    assert_eq!(
        steps,
        [
            "pkg/models/__init__.py:1 `pkg.models` imports `torch.nn` while it loads, which loads its package `torch` first"
        ]
    );
    let p = project("imp_reach_all", &contract("all"));
    write(&p);
    assert_eq!(
        reports(&p),
        [
            "pkg/cli.py:1 IMP201 <module> pkg.heavy",
            "pkg/cli.py:1 IMP201 <module> torch",
        ]
    );
    // Without the module-level chain, the function's import leads to `pkg.heavy`.
    p.write("pkg/models/base.py", "");
    assert_eq!(
        reports(&p),
        [
            "pkg/cli.py:1 IMP201 <module> torch",
            "pkg/cli.py:5 IMP201 run pkg.heavy",
        ]
    );
    assert_eq!(
        related(&p, "pkg/cli.py:5"),
        ["pkg/helpers.py:1 `pkg.helpers` imports `pkg.heavy` while it loads"]
    );
}

/// A walk takes no step an `ignored-imports` pair accepts, judges the open sites it reaches,
/// and never reports the packages holding the importer, which load before it.
#[test]
fn reach_skips_accepted_imports_and_reports_open_sites_on_the_way() {
    let p = project(
        "imp_reach_open",
        r#"
[[tool.heki.import-owners]]
name = "startup"
importers = ["pkg.cli"]
forbidden-imports = ["pkg", "pkg.heavy", "pkg.plugins.*", "yaml.loader"]
ignored-imports = ["pkg.lazy -> pkg.heavy"]
reach = "all"
unresolved = "report"
"#,
    );
    p.write("pkg/__init__.py", "");
    p.write("pkg/plugins/__init__.py", "");
    p.write("pkg/heavy.py", "");
    p.write("pkg/lazy.py", "import pkg.heavy\n");
    p.write(
        "pkg/loader.py",
        "import importlib\n\n\ndef load(name: str) -> object:\n    return importlib.import_module(f\"pkg.plugins.{name}\")\n",
    );
    p.write(
        "pkg/tools.py",
        "import importlib\nfrom yaml import dump\nfrom yaml import loader\n\n\ndef pick(flag: bool) -> object:\n    return importlib.import_module(\"pkg.heavy\" if flag else \"json\")\n\n\ndef both(flag: bool) -> object:\n    return importlib.import_module(\"pkg.heavy\" if flag else \"pkg\")\n",
    );
    p.write(
        "pkg/cli.py",
        "import pkg.lazy\nimport pkg.loader\nimport importlib\nimport pkg.tools\n\n\ndef own(name: str) -> object:\n    return importlib.import_module(name)\n",
    );
    assert_eq!(
        reports(&p),
        [
            "pkg/cli.py:2 IMP202 <module> pkg.plugins.*",
            "pkg/cli.py:4 IMP201 <module> pkg|pkg.heavy",
            "pkg/cli.py:4 IMP202 <module> json|pkg.heavy",
            "pkg/cli.py:4 IMP202 <module> yaml.loader",
            "pkg/cli.py:8 IMP202 own *",
        ]
    );
    assert_eq!(
        message(&p, "pkg/cli.py:2"),
        "`<module>` imports `pkg.loader` while `pkg.cli` loads, and through it a module whose \
         name starts with `pkg.plugins.`, which may be a forbidden import of `startup`"
    );
    assert_eq!(
        related(&p, "pkg/cli.py:2"),
        [
            "pkg/loader.py:5 `pkg.loader` imports a module whose name starts with `pkg.plugins.` by name in `load`"
        ]
    );
}

/// A contract heki cannot read is a configuration error naming the key.
#[test]
fn invalid_contracts_are_configuration_errors() {
    for (contract, expected) in [
        (
            "allowed-imports = [\"<django>\"]",
            "unknown group `<django>` in import-owners.allowed-imports",
        ),
        (
            "ignored-imports = [\"pkg.a pkg.b\"]",
            "invalid pair `pkg.a pkg.b` in import-owners.ignored-imports",
        ),
        (
            "reach = \"far\"",
            "unknown `reach` value `far` in import-owners `bad`",
        ),
        (
            "type-checking = \"maybe\"",
            "unknown `type-checking` value `maybe` in import-owners `bad`",
        ),
        (
            "unresolved = \"yes\"",
            "unknown `unresolved` value `yes` in import-owners `bad`",
        ),
        (
            "forbidden-imports = [\"pkg.[\"]",
            "invalid glob `pkg.[` in import-owners.forbidden-imports",
        ),
        (
            "relative-imports = [\"<stdlib>\"]",
            "group `<stdlib>` in import-owners.relative-imports: a relative import reaches only \
             the modules of the importer's own top-level package",
        ),
        (
            "relative-imports = [\"<django>\"]",
            "group `<django>` in import-owners.relative-imports",
        ),
    ] {
        let p = project(
            "imp_invalid",
            &format!("[[tool.heki.import-owners]]\nname = \"bad\"\n{contract}\n"),
        );
        p.write("app.py", "import json\n");
        let (code, out, err) = p.check(&[]);
        assert_eq!(code, 2, "{contract}: {out}\n{err}");
        assert!(err.contains(expected), "{contract}: {err}");
    }
}

const RUNTIME_A: &str = r#"from . import b


def relative() -> int:
    return b.helper()


def absolute() -> int:
    from pkg._runtime import b as absolute_b

    return absolute_b.helper()
"#;

fn runtime_package(p: &Project) {
    p.write("pkg/__init__.py", "");
    p.write("pkg/_runtime/__init__.py", "");
    p.write("pkg/_runtime/b.py", "def helper() -> int:\n    return 1\n");
    p.write("pkg/_runtime/c.py", "");
}

/// A package copied into other packages under another name, whose modules may import each other
/// only relatively: `relative-imports` lists them once, and an import naming one by its full
/// name is reported, also when `allowed-imports` lists it too.
#[test]
fn relative_imports_allow_a_module_only_through_a_relative_import() {
    for allowed in [
        r#""<stdlib>""#,
        r#""<stdlib>", "pkg._runtime", "pkg._runtime.*""#,
    ] {
        let p = project(
            "imp_relative",
            &format!(
                "[[tool.heki.import-owners]]\nname = \"runtime-leaf\"\nimporters = [\"pkg._runtime\", \"pkg._runtime.*\"]\nallowed-imports = [{allowed}]\nrelative-imports = [\"pkg._runtime\", \"pkg._runtime.*\"]\n"
            ),
        );
        runtime_package(&p);
        p.write("pkg/_runtime/a.py", RUNTIME_A);
        assert_eq!(
            reports(&p),
            ["pkg/_runtime/a.py:9 IMP201 absolute pkg._runtime.b"],
            "{allowed}"
        );
        assert_eq!(
            message(&p, "pkg/_runtime/a.py:9"),
            "`absolute` imports `pkg._runtime.b` when it runs, which `runtime-leaf` allows only \
             through a relative import"
        );
    }
}

const SPELLINGS: &str = r#"import importlib
from typing import TYPE_CHECKING

from . import b
from .b import helper
from .. import shared
import pkg._runtime.b
from pkg import shared as shared_again
lazy from . import c
lazy from pkg._runtime import c as c_again

if TYPE_CHECKING:
    from pkg._runtime import c as checked


def by_name(flag: bool, name: str) -> object:
    importlib.import_module(".b", __package__)
    importlib.import_module(".c" if flag else ".b", __package__)
    importlib.import_module("pkg._runtime.b")
    importlib.import_module(".b", "pkg._runtime")
    __import__("pkg._runtime.c")
    __import__("b", globals(), None, [], 1)
    importlib.import_module(".b" if flag else "pkg._runtime.b", __package__)
    importlib.import_module(".b" if flag else "pkg._runtime.c", __package__)
    importlib.import_module(".b" if flag else "json", __package__)
    importlib.import_module(f"pkg._runtime.{name}")
    return importlib.import_module(f".plugins.{name}", __package__)
"#;

/// What names a module relatively: leading dots, and a relative name anchored at the module's
/// own `__package__` or `__name__`. A full name, a relative name anchored at a package the code
/// spells (also in a module constant), and a by-name import whose names include the full name of the module are reported;
/// with `unresolved = "report"`, so are a by-name import some of whose names break the contract,
/// a head that may name a module of the key, a relative name anchored at a package only known at
/// run time (a name another module gives), and `__import__` with a `level`, whose module heki cannot name. Lazy imports count, and those under `TYPE_CHECKING` only with the key.
#[test]
fn relative_imports_judge_every_spelling_of_an_import() {
    let contract = |keys: &str| {
        format!(
            "[tool.heki]\ntarget-version = \"py315\"\n\n[[tool.heki.import-owners]]\nname = \"runtime-leaf\"\nimporters = [\"pkg._runtime\", \"pkg._runtime.*\"]\nallowed-imports = [\"<stdlib>\"]\nrelative-imports = [\"pkg\", \"pkg.*\"]\n{keys}\n"
        )
    };
    let exact = [
        "pkg/_runtime/a.py:7 IMP201 <module> pkg._runtime.b",
        "pkg/_runtime/a.py:8 IMP201 <module> pkg.shared",
        "pkg/_runtime/a.py:10 IMP201 <module> pkg._runtime.c",
        "pkg/_runtime/a.py:19 IMP201 by_name pkg._runtime.b",
        "pkg/_runtime/a.py:20 IMP201 by_name pkg._runtime.b",
        "pkg/_runtime/a.py:21 IMP201 by_name pkg._runtime.c",
        "pkg/_runtime/a.py:23 IMP201 by_name pkg._runtime.b",
        "pkg/_runtime/anchored.py:7 IMP201 anchored pkg._runtime.b",
    ];
    let open = [
        "pkg/_runtime/a.py:22 IMP202 by_name *",
        "pkg/_runtime/a.py:24 IMP202 by_name pkg._runtime.b|pkg._runtime.c",
        "pkg/_runtime/a.py:26 IMP202 by_name pkg._runtime.*",
        "pkg/_runtime/dynamic.py:6 IMP202 dynamic *",
    ];
    for (keys, expected) in [
        ("", exact.to_vec()),
        (
            "type-checking = \"report\"\nunresolved = \"report\"",
            [
                exact[..3].to_vec(),
                vec!["pkg/_runtime/a.py:13 IMP201 <module> pkg._runtime.c"],
                exact[3..].to_vec(),
                open.to_vec(),
            ]
            .concat(),
        ),
    ] {
        let p = project("imp_relative_spellings", &contract(keys));
        runtime_package(&p);
        p.write("pkg/shared.py", "");
        p.write(
            "pkg/_runtime/__init__.py",
            "import importlib\n\nimportlib.import_module(\".c\", __name__)\n",
        );
        p.write("pkg/_runtime/a.py", SPELLINGS);
        p.write(
            "pkg/_runtime/anchored.py",
            "import importlib\n\nPACKAGE = \"pkg._runtime\"\n\n\ndef anchored() -> object:\n    return importlib.import_module(\".b\", PACKAGE)\n",
        );
        p.write(
            "pkg/_runtime/dynamic.py",
            "import importlib\nfrom sys import platform as PACKAGE\n\n\ndef dynamic() -> object:\n    return importlib.import_module(\".b\", PACKAGE)\n",
        );
        let mut found = reports(&p);
        found.sort();
        let mut expected = expected;
        expected.sort_unstable();
        assert_eq!(found, expected, "{keys}");
        if !keys.is_empty() {
            assert_eq!(
                message(&p, "pkg/_runtime/a.py:24"),
                "`by_name` imports `pkg._runtime.b` or `pkg._runtime.c` by name when it runs, \
                 which may be a module `runtime-leaf` allows only through a relative import"
            );
            assert_eq!(
                message(&p, "pkg/_runtime/a.py:26"),
                "`by_name` imports a module whose name starts with `pkg._runtime.` by name when \
                 it runs, which may be a module `runtime-leaf` allows only through a relative \
                 import"
            );
        }
    }
}

const COMBINED: &str = "import json\nimport pkg._runtime.b\nimport pkg.other\nfrom . import b\n";

/// `relative-imports` alone allows only the relative imports of its modules; with
/// `forbidden-imports`, a relative import of a module both name is allowed and only the forbidden
/// modules are reported besides; an `ignored-imports` pair accepts an import by the full name.
#[test]
fn relative_imports_combine_with_the_other_lists() {
    let relative = "relative-imports = [\"pkg._runtime.*\"]";
    for (keys, expected) in [
        (
            relative.to_owned(),
            vec![
                "pkg/_runtime/a.py:1 IMP201 <module> json",
                "pkg/_runtime/a.py:2 IMP201 <module> pkg._runtime.b",
                "pkg/_runtime/a.py:3 IMP201 <module> pkg.other",
            ],
        ),
        (
            format!("{relative}\nforbidden-imports = [\"pkg\", \"pkg.*\"]"),
            vec![
                "pkg/_runtime/a.py:2 IMP201 <module> pkg._runtime.b",
                "pkg/_runtime/a.py:3 IMP201 <module> pkg.other",
            ],
        ),
        (
            format!("{relative}\nallowed-imports = [\"<stdlib>\", \"pkg.*\"]"),
            vec!["pkg/_runtime/a.py:2 IMP201 <module> pkg._runtime.b"],
        ),
        (
            format!("{relative}\nignored-imports = [\"pkg._runtime.a -> pkg._runtime.b\"]"),
            vec![
                "pkg/_runtime/a.py:1 IMP201 <module> json",
                "pkg/_runtime/a.py:3 IMP201 <module> pkg.other",
            ],
        ),
    ] {
        let p = project(
            "imp_relative_lists",
            &format!(
                "[[tool.heki.import-owners]]\nname = \"rt\"\nimporters = [\"pkg._runtime.a\"]\n{keys}\n"
            ),
        );
        runtime_package(&p);
        p.write("pkg/other.py", "");
        p.write("pkg/_runtime/a.py", COMBINED);
        assert_eq!(reports(&p), expected, "{keys}");
        if keys == relative {
            assert_eq!(
                message(&p, "pkg/_runtime/a.py:1"),
                "`<module>` imports `json` while `pkg._runtime.a` loads, not among the allowed \
                 imports of `rt`"
            );
        }
    }
}

const REASONS: &str = r#"import importlib


def by_name(flag: bool, name: str) -> object:
    importlib.import_module("pkg._runtime.b" if flag else "torch")
    importlib.import_module("pkg._runtime.b" if flag else f"pkg.x{name}")
    importlib.import_module("pkg._runtime.b" if flag else "pkg.plugins.x")
    return importlib.import_module(f"pkg.{name}")
"#;

/// The alternatives of a by-name import keep their own reasons: a module the contract allows only
/// relatively and one it does not allow however it is named, an alternative heki cannot name,
/// whose package's search path the project replaces, and a head that may name either kind.
#[test]
fn a_by_name_import_keeps_the_reason_of_each_alternative() {
    let p = project(
        "imp_relative_reasons",
        "[[tool.heki.import-owners]]\nname = \"rt\"\nimporters = [\"pkg._runtime.a\"]\nallowed-imports = [\"<stdlib>\"]\nrelative-imports = [\"pkg._runtime.*\"]\nunresolved = \"report\"\n",
    );
    runtime_package(&p);
    p.write("pkg/plugins/__init__.py", "__path__ = [\"/opt/plugins\"]\n");
    p.write("pkg/plugins/x.py", "");
    p.write("pkg/_runtime/a.py", REASONS);
    assert_eq!(
        reports(&p),
        [
            "pkg/_runtime/a.py:5 IMP201 by_name pkg._runtime.b|torch",
            "pkg/_runtime/a.py:6 IMP202 by_name pkg._runtime.b|pkg.x*",
            "pkg/_runtime/a.py:7 IMP202 by_name pkg._runtime.b|*",
            "pkg/_runtime/a.py:8 IMP202 by_name pkg.*",
        ]
    );
    assert_eq!(
        message(&p, "pkg/_runtime/a.py:5"),
        "`by_name` imports `pkg._runtime.b` by name when it runs, which `rt` allows only through \
         a relative import, or `torch`, not among the allowed imports of `rt`"
    );
    let either = "which may be a module `rt` allows only through a relative import, or which may \
                  be outside the allowed imports of `rt`";
    assert_eq!(
        message(&p, "pkg/_runtime/a.py:6"),
        format!("`by_name` imports `pkg._runtime.b` or `pkg.x*` by name when it runs, {either}")
    );
    assert_eq!(
        message(&p, "pkg/_runtime/a.py:8"),
        format!(
            "`by_name` imports a module whose name starts with `pkg.` by name when it runs, \
             {either}"
        )
    );
}

/// A relative import reaches only the importer's own top-level package: an importer outside the
/// package of a module the key names cannot import it without a report.
#[test]
fn an_importer_outside_the_package_cannot_import_its_modules() {
    let p = project(
        "imp_relative_outside",
        "[[tool.heki.import-owners]]\nname = \"rt\"\nimporters = [\"app\"]\nallowed-imports = [\"<stdlib>\"]\nrelative-imports = [\"pkg._runtime.*\"]\n",
    );
    runtime_package(&p);
    p.write("app.py", "import json\nfrom pkg._runtime import b\n");
    assert_eq!(reports(&p), ["app.py:2 IMP201 <module> pkg._runtime.b"]);
}

const REACH_A: &str = r#"from . import b
import pkg._runtime.c


def later() -> None:
    import pkg._runtime.b
"#;

/// With `reach`, how an import names its module is judged at the importer's own imports that the
/// contract counts; a module of the key that a chain reaches is allowed, however the modules on
/// the chain import it.
#[test]
fn reach_judges_how_only_the_importer_names_its_imports() {
    for (reach, expected) in [
        (
            "import-time",
            vec![
                "pkg/_runtime/a.py:1 IMP201 <module> torch",
                "pkg/_runtime/a.py:2 IMP201 <module> pkg._runtime.c",
            ],
        ),
        (
            "all",
            vec![
                "pkg/_runtime/a.py:1 IMP201 <module> torch",
                "pkg/_runtime/a.py:2 IMP201 <module> pkg._runtime.c",
                "pkg/_runtime/a.py:6 IMP201 later pkg._runtime.b",
            ],
        ),
    ] {
        let p = project(
            "imp_relative_reach",
            &format!(
                "[[tool.heki.import-owners]]\nname = \"rt\"\nimporters = [\"pkg._runtime.a\"]\nforbidden-imports = [\"torch\"]\nrelative-imports = [\"pkg._runtime.*\"]\nreach = \"{reach}\"\n"
            ),
        );
        runtime_package(&p);
        p.write("pkg/_runtime/b.py", "import pkg._runtime.c\nimport torch\n");
        p.write("pkg/_runtime/a.py", REACH_A);
        assert_eq!(reports(&p), expected, "{reach}");
        assert_eq!(
            message(&p, "pkg/_runtime/a.py:2"),
            "`<module>` imports `pkg._runtime.c` while `pkg._runtime.a` loads, which `rt` allows \
             only through a relative import"
        );
    }
}
