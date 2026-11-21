//! `attribution` on `write-owners`, `read-owners`, and `call-owners`, end to end through the real
//! binary. By default (`"call-graph"`) a private helper hands its accesses to its callers and a
//! function that only runs inside allowed functions is allowed; with `"lexical"` an access is
//! judged in the function whose code holds it, a lambda, comprehension, or generator expression
//! being part of the function it is written in.

mod common;

use common::Project;
use serde_json::Value;

/// `(path, line, column, code, message)` of every report of `p`, sorted.
fn reports(p: &Project) -> Vec<(String, u64, u64, String, String)> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<(String, u64, u64, String, String)> = diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().replace('\\', "/"),
                d["line"].as_u64().unwrap(),
                d["column"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
                d["message"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

fn report(
    path: &str,
    line: u64,
    column: u64,
    code: &str,
    message: &str,
) -> (String, u64, u64, String, String) {
    (
        path.to_owned(),
        line,
        column,
        code.to_owned(),
        message.to_owned(),
    )
}

const DECIDE_CORE: &str = "def decide() -> int:\n    return 1\n\n\nclass Base:\n    pass\n";

const DECIDE_SITE: &str = r#"from pkg.core import Base, decide


def adapter() -> int:
    return public_helper() + _private_helper()


def public_helper() -> int:
    return decide()


def _private_helper() -> int:
    return decide()


class Derived(Base):
    pass
"#;

#[test]
fn a_lexical_contract_judges_the_function_holding_the_call() {
    let run = |keys: &str| {
        let p = Project::new(&format!("attribution_decide_{}", keys.len()));
        p.write(
            "pyproject.toml",
            &format!(
                "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"home\"\ntargets = [\"pkg.core.decide\", \"pkg.core.Base\"]\nallowed-callers = [\"pkg.core:*\", \"pkg.site:adapter\"]\nsubclasses = false\n{keys}"
            ),
        )
        .write("pkg/__init__.py", "")
        .write("pkg/core.py", DECIDE_CORE)
        .write("pkg/site.py", DECIDE_SITE);
        reports(&p)
    };
    let helper = |line: u64, func: &str| {
        report(
            "pkg/site.py",
            line,
            12,
            "MUT204",
            &format!("`{func}` calls `pkg.core.decide` outside the call owners of `home`"),
        )
    };
    let base = report(
        "pkg/site.py",
        16,
        1,
        "MUT204",
        "`<module>` subclasses `pkg.core.Base` outside the call owners of `home`",
    );
    // Both helpers only run inside the allowed `adapter`, and the class base is no reference.
    assert_eq!(run("references = \"calls\"\n"), vec![]);
    assert_eq!(
        run("references = \"calls\"\nattribution = \"call-graph\"\n"),
        vec![]
    );
    assert_eq!(
        run("references = \"calls\"\nattribution = \"lexical\"\n"),
        vec![helper(9, "public_helper"), helper(13, "_private_helper")]
    );
    assert_eq!(
        run("references = \"calls-and-bases\"\n"),
        vec![base.clone()]
    );
    assert_eq!(
        run("references = \"calls-and-bases\"\nattribution = \"lexical\"\n"),
        vec![
            helper(9, "public_helper"),
            helper(13, "_private_helper"),
            base
        ]
    );
}

const SCOPES: &str = r#"from pkg.core import decide


def adapter(xs: list[int]) -> int:
    total = sum(decide() for _ in xs)
    pick = lambda: decide()
    return total + pick()


def outer() -> int:
    def inner() -> int:
        return decide()

    return inner()


HANDLERS = [lambda: decide()]
"#;

const LAYER: &str = r#"from pkg.core import decide
from pkg.util import _relay


def entry() -> int:
    return _relay() + decide()
"#;

const UTIL: &str = r#"from pkg.core import decide


def _relay() -> int:
    return decide()
"#;

#[test]
fn a_lexical_contract_keeps_anonymous_scopes_with_their_function() {
    let run = |attribution: &str| {
        let p = Project::new(&format!("attribution_scopes_{}", attribution.len()));
        p.write(
            "pyproject.toml",
            &format!(
                "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"home\"\ntargets = [\"pkg.core.decide\"]\nallowed-callers = [\"pkg.core:*\", \"pkg.site:adapter\"]\nforbidden-callers = [\"pkg.site.*\"]\n{attribution}\n[[tool.heki.call-owners]]\nname = \"layer\"\ntargets = [\"pkg.core.decide\"]\nforbidden-callers = [\"pkg.layer.*\"]\n{attribution}"
            ),
        )
        .write("pkg/__init__.py", "")
        .write("pkg/core.py", DECIDE_CORE)
        .write("pkg/site.py", SCOPES)
        .write("pkg/layer.py", LAYER)
        .write("pkg/util.py", UTIL);
        reports(&p)
    };
    let home = |line: u64, column: u64, func: &str| {
        report(
            "pkg/site.py",
            line,
            column,
            "MUT204",
            &format!("`{func}` calls `pkg.core.decide` inside the forbidden callers of `home`"),
        )
    };
    let layer = |line: u64, column: u64, message: &str| {
        report(
            "pkg/layer.py",
            line,
            column,
            "MUT204",
            &format!("{message} inside the forbidden callers of `layer`"),
        )
    };
    let direct = layer(6, 23, "`entry` calls `pkg.core.decide`");
    // The private `_relay` hands its call to the forbidden `entry`; the lambda nothing calls
    // keeps its call.
    assert_eq!(
        run(""),
        vec![
            layer(
                6,
                12,
                "`entry` calls `pkg.core.decide` via `pkg.util._relay`"
            ),
            direct.clone(),
            home(12, 16, "outer.<locals>.inner"),
            home(17, 21, "<module>.<locals>.<lambda>"),
        ]
    );
    // Judged lexically, `_relay` is no forbidden caller, and the generator expression and the
    // lambda are part of the allowed `adapter`, while a nested `def` is a function of its own and
    // a lambda of the module body is the module's.
    assert_eq!(
        run("attribution = \"lexical\"\n"),
        vec![
            direct,
            home(12, 16, "outer.<locals>.inner"),
            home(17, 21, "<module>"),
        ]
    );
}

const VAULT: &str = r#"class Vault:
    def __init__(self) -> None:
        self.secret = 1
        print(self.secret)
"#;

const PEEK: &str = r#"from pkg.vault import Vault


def _peek(v: Vault) -> int:
    return v.secret


def show(v: Vault) -> int:
    return v.secret


def guess(obj) -> int:
    return obj.secret
"#;

const STORE: &str = r#"from pkg.peek import _peek, guess, show
from pkg.vault import Vault


def store_read(v: Vault) -> int:
    return _peek(v) + show(v) + guess(v)
"#;

#[test]
fn a_lexical_read_contract_judges_the_function_holding_the_read() {
    let run = |attribution: &str| {
        let p = Project::new(&format!("attribution_reads_{}", attribution.len()));
        p.write(
            "pyproject.toml",
            &format!(
                "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT203\", \"MUT206\"]\n\n[[tool.heki.read-owners]]\nname = \"vault\"\ntargets = [\"Vault.secret\"]\nallowed-readers = [\"pkg.store:*\"]\nunresolved = \"report\"\n{attribution}"
            ),
        )
        .write("pkg/__init__.py", "")
        .write("pkg/vault.py", VAULT)
        .write("pkg/peek.py", PEEK)
        .write("pkg/store.py", STORE);
        reports(&p)
    };
    // Every read runs inside the allowed `store_read`.
    assert_eq!(run(""), vec![]);
    // The initializer reading its own instance stays allowed; the parameter `guess` reads on is
    // an instance of a target class, which its allowed caller binds.
    assert_eq!(
        run("attribution = \"lexical\"\n"),
        vec![
            report(
                "pkg/peek.py",
                5,
                12,
                "MUT203",
                "`_peek` reads `Vault.secret` (direct_attr) outside the read owners of `vault`"
            ),
            report(
                "pkg/peek.py",
                9,
                12,
                "MUT203",
                "`show` reads `Vault.secret` (direct_attr) outside the read owners of `vault`"
            ),
            report(
                "pkg/peek.py",
                13,
                12,
                "MUT206",
                "`guess` reads `secret` (direct_attr) on an object of unknown class outside the read owners of `vault`: heki cannot tell whether it reads `Vault.secret`"
            ),
        ]
    );
}

const MODEL: &str = r#"class Model:
    def __init__(self) -> None:
        self.fields: list[int] = []

    def seal(self) -> None:
        pass
"#;

const WRITERS: &str = r#"from pkg.model import Model


def _reset(m: Model) -> None:
    m.fields = []


def public_set(m: Model) -> None:
    m.fields = [1]


def fill(items: list[int]) -> None:
    items.append(1)


def touch(m: Model) -> None:
    fill(m.fields)


def _touch_privately(m: Model) -> None:
    fill(m.fields)


def call_seal(obj) -> None:
    obj.seal()
"#;

const WRITE_STORE: &str = r#"from pkg.model import Model
from pkg.writers import _reset, _touch_privately, call_seal, fill, public_set


def store_reset(m: Model) -> None:
    _reset(m)
    public_set(m)
    fill(m.fields)
    _touch_privately(m)
    call_seal(m)
"#;

#[test]
fn a_lexical_write_contract_judges_the_function_holding_the_write() {
    let run = |attribution: &str| {
        let p = Project::new(&format!("attribution_writes_{}", attribution.len()));
        p.write(
            "pyproject.toml",
            &format!(
                "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\", \"MUT204\", \"MUT207\"]\n\n[[tool.heki.write-owners]]\nname = \"model\"\ntargets = [\"Model.fields\"]\nallowed-writers = [\"pkg.store:*\"]\n{attribution}\n[[tool.heki.call-owners]]\nname = \"seal\"\ntargets = [\"Model.seal\"]\nallowed-callers = [\"pkg.store:*\"]\nunresolved = \"report\"\n{attribution}"
            ),
        )
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", MODEL)
        .write("pkg/writers.py", WRITERS)
        .write("pkg/store.py", WRITE_STORE);
        reports(&p)
    };
    let touch = report(
        "pkg/writers.py",
        17,
        5,
        "MUT201",
        "`touch` writes `Model.fields` (indirect_helper) via `pkg.writers.fill` outside the write owners of `model`",
    );
    // `touch` hands the field to a helper that changes it, and nothing calls it.
    assert_eq!(run(""), vec![touch.clone()]);
    // The initializer writing its own instance stays allowed; the private helper changing the
    // field through `fill` is judged itself; the call on a parameter of unknown class is not
    // tied by its allowed caller any more.
    assert_eq!(
        run("attribution = \"lexical\"\n"),
        vec![
            report(
                "pkg/writers.py",
                5,
                5,
                "MUT201",
                "`_reset` writes `Model.fields` (direct_attr) outside the write owners of `model`"
            ),
            report(
                "pkg/writers.py",
                9,
                5,
                "MUT201",
                "`public_set` writes `Model.fields` (direct_attr) outside the write owners of `model`"
            ),
            touch,
            report(
                "pkg/writers.py",
                21,
                5,
                "MUT201",
                "`_touch_privately` writes `Model.fields` (indirect_helper) via `pkg.writers.fill` outside the write owners of `model`"
            ),
            report(
                "pkg/writers.py",
                25,
                5,
                "MUT207",
                "`call_seal` calls `seal` on an object of unknown class outside the call owners of `seal`: heki cannot tell whether it calls `Model.seal`"
            ),
        ]
    );
}

#[test]
fn an_unknown_attribution_value_is_a_configuration_error() {
    for (section, keys) in [
        ("write-owners", "targets = [\"Model.fields\"]"),
        ("read-owners", "targets = [\"Model.fields\"]"),
        ("call-owners", "targets = [\"Model.seal\"]"),
    ] {
        let p = Project::new(&format!("attribution_unknown_{}", section.len()));
        p.write(
            "pyproject.toml",
            &format!(
                "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\", \"MUT203\", \"MUT204\"]\n\n[[tool.heki.{section}]]\nname = \"c\"\n{keys}\nattribution = \"callers\"\n"
            ),
        )
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", MODEL);
        let (code, stdout, stderr) = p.check(&[]);
        assert_eq!(code, 2, "{stdout}{stderr}");
        assert!(
            stderr.contains("attribution") && stderr.contains("`call-graph` or `lexical`"),
            "{stderr}"
        );
    }
}
