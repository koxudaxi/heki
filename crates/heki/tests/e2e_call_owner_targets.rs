//! `targets-call-each-other` on `call-owners`: by default the targets of a contract may call each
//! other (a call of the guarded operation is checked where it is made); with `"report"` each
//! target is judged as a contract of that one target with the same keys would judge it, so one
//! contract listing independent targets reports what one contract per target reports.

mod common;

use common::Project;
use serde_json::Value;

const CORE: &str = r#"def a() -> None:
    pass


def b() -> None:
    pass


class K:
    def m(self) -> None:
        pass


def c() -> None:
    a()


def recursive(n: int) -> None:
    if n:
        recursive(n - 1)


def calls_b() -> None:
    b()


def unknown(obj) -> None:
    obj.a()


def unknown_in_target(obj) -> None:
    obj.a()
"#;

const BAD: &str = r#"from .core import a


def bad1() -> None:
    a()
"#;

const OK: &str = r#"from .core import a, b, K


def good() -> None:
    a()
    b()
    K().m()
"#;

const TARGETS: [&str; 7] = [
    "pkg.core.a",
    "pkg.core.b",
    "pkg.core.K.m",
    "pkg.core.c",
    "pkg.core.recursive",
    "pkg.core.calls_b",
    "pkg.core.unknown_in_target",
];

const KEYS: &str = r#"allowed-callers = ["pkg.ok:good"]
references = "calls"
own-receiver = "allow"
unresolved = "report"
attribution = "lexical"
"#;

fn project(name: &str, contracts: &str) -> Project {
    let p = Project::new(name);
    p.write("pkg/__init__.py", "");
    p.write("pkg/core.py", CORE);
    p.write("pkg/bad.py", BAD);
    p.write("pkg/ok.py", OK);
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\", \"MUT207\"]\n\n{contracts}"
        ),
    );
    p
}

/// One contract with every target and `extra` among its keys.
fn merged(extra: &str) -> String {
    let targets: Vec<String> = TARGETS.iter().map(|t| format!("\"{t}\"")).collect();
    format!(
        "[[tool.heki.call-owners]]\nname = \"g\"\ntargets = [{}]\n{KEYS}{extra}",
        targets.join(", ")
    )
}

/// One contract per target with the same keys.
fn separate() -> String {
    TARGETS
        .iter()
        .map(|t| {
            format!("[[tool.heki.call-owners]]\nname = \"s-{t}\"\ntargets = [\"{t}\"]\n{KEYS}")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `(path, line, code)` of every report, sorted: the message names the contract, which differs.
fn reports(p: &Project) -> Vec<(String, u64, String)> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<(String, u64, String)> = diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().replace('\\', "/"),
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    out.sort();
    out
}

fn at(path: &str, line: u64, code: &str) -> (String, u64, String) {
    (path.to_owned(), line, code.to_owned())
}

#[test]
fn a_contract_per_target_reports_the_calls_between_the_targets() {
    let reported = reports(&project("targets_separate", &separate()));
    assert_eq!(
        reported,
        vec![
            at("pkg/bad.py", 5, "MUT204"),
            at("pkg/core.py", 15, "MUT204"),
            at("pkg/core.py", 24, "MUT204"),
            at("pkg/core.py", 28, "MUT207"),
            at("pkg/core.py", 32, "MUT207"),
        ]
    );
}

#[test]
fn by_default_the_targets_of_one_contract_may_call_each_other() {
    let reported = reports(&project("targets_default", &merged("")));
    assert_eq!(
        reported,
        vec![
            at("pkg/bad.py", 5, "MUT204"),
            at("pkg/core.py", 28, "MUT207")
        ]
    );
    let explicit = merged("targets-call-each-other = \"allow\"\n");
    assert_eq!(reports(&project("targets_allow", &explicit)), reported);
}

#[test]
fn report_judges_each_target_like_a_contract_of_one_target() {
    let expected = reports(&project("targets_expected", &separate()));
    let together = merged("targets-call-each-other = \"report\"\n");
    assert_eq!(reports(&project("targets_report", &together)), expected);
}

#[test]
fn report_leaves_a_target_calling_itself_allowed() {
    let p = project(
        "targets_recursive",
        &merged("targets-call-each-other = \"report\"\n"),
    );
    assert!(
        !reports(&p).contains(&at("pkg/core.py", 20, "MUT204")),
        "a target calling itself is no outside call"
    );
}

#[test]
fn report_with_one_target_changes_nothing() {
    let one = |extra: &str| {
        format!(
            "[[tool.heki.call-owners]]\nname = \"g\"\ntargets = [\"pkg.core.a\"]\n{KEYS}{extra}"
        )
    };
    let default = reports(&project("targets_one_default", &one("")));
    let report = reports(&project(
        "targets_one_report",
        &one("targets-call-each-other = \"report\"\n"),
    ));
    assert_eq!(default, report);
}

#[test]
fn an_unknown_value_is_a_configuration_error() {
    let p = project(
        "targets_unknown",
        &merged("targets-call-each-other = \"deny\"\n"),
    );
    let (code, _, stderr) = p.check(&[]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("targets-call-each-other"), "{stderr}");
    assert!(stderr.contains("deny"), "{stderr}");
}

#[test]
fn report_judges_an_unresolved_call_for_each_target_it_may_reach() {
    let source = "class A:\n    def reset(self, obj) -> None:\n        obj.reset()\n\n\n\
                  class B:\n    def reset(self) -> None:\n        pass\n";
    let contracts = |targets: &[&str], extra: &str| -> String {
        targets
            .iter()
            .map(|t| {
                format!(
                    "[[tool.heki.call-owners]]\nname = \"{t}\"\ntargets = [\"{t}\"]\n{KEYS}{extra}"
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let build = |name: &str, contracts: String| {
        let p = Project::new(name);
        p.write("pkg/__init__.py", "");
        p.write("pkg/m.py", source);
        p.write(
            "pyproject.toml",
            &format!(
                "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT207\"]\n\n{contracts}"
            ),
        );
        p
    };
    let targets = ["pkg.m.A.reset", "pkg.m.B.reset"];
    let expected = reports(&build(
        "targets_unresolved_separate",
        contracts(&targets, ""),
    ));
    assert_eq!(expected, vec![at("pkg/m.py", 3, "MUT207")]);
    let both = format!(
        "[[tool.heki.call-owners]]\nname = \"g\"\ntargets = [\"{}\", \"{}\"]\n{KEYS}",
        targets[0], targets[1]
    );
    let report = format!("{both}targets-call-each-other = \"report\"\n");
    assert_eq!(
        reports(&build("targets_unresolved_report", report)),
        expected
    );
    assert_eq!(reports(&build("targets_unresolved_allow", both)), vec![]);
}

#[test]
fn report_judges_an_outside_call_for_each_target_it_names() {
    // `helper` runs only inside `vendor.f`, a project method the target `vendor.f` names: that
    // lets it call the outside `vendor.f`, not the outside `vendor.g`.
    let keys = KEYS.replace("attribution = \"lexical\"\n", "");
    let contract = |targets: &[&str], extra: &str| -> String {
        let listed: Vec<String> = targets.iter().map(|t| format!("\"{t}\"")).collect();
        format!(
            "[[tool.heki.call-owners]]\nname = \"{}\"\ntargets = [{}]\n{keys}{extra}",
            targets.join("+"),
            listed.join(", ")
        )
    };
    let build = |name: &str, contracts: String| {
        let p = Project::new(name);
        p.write("pkg/__init__.py", "");
        p.write(
            "pkg/h.py",
            "import vendor as ven\n\n\ndef helper() -> None:\n    ven.f()\n    ven.g()\n\n\n\
             class vendor:\n    def f(self) -> None:\n        helper()\n",
        );
        p.write(
            "pyproject.toml",
            &format!(
                "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n{contracts}"
            ),
        );
        p
    };
    let separate = format!(
        "{}\n{}",
        contract(&["vendor.f"], ""),
        contract(&["vendor.g"], "")
    );
    let expected = reports(&build("targets_outside_separate", separate));
    assert_eq!(expected, vec![at("pkg/h.py", 6, "MUT204")]);
    let together = contract(
        &["vendor.f", "vendor.g"],
        "targets-call-each-other = \"report\"\n",
    );
    assert_eq!(
        reports(&build("targets_outside_report", together)),
        expected
    );
}
