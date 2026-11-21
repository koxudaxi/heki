//! Contract patterns with a module part (`module:qualname`) end to end through the real binary:
//! `pkg:*` names what `pkg/__init__.py` defines without its submodules, in every key that names
//! project functions or classes, and malformed patterns are configuration errors.

mod common;

use common::Project;
use serde_json::Value;

const PKG: &str = r#"class Model:
    def __init__(self) -> None:
        self.fields: list[int] = []
        self.value = 0


def decide() -> int:
    return 1


def root_user() -> int:
    return decide()


def notify() -> None: ...


def change(m: Model) -> None:
    m.value = 1


def change_and_notify(m: Model) -> None:
    m.value = 2
    notify()


def set_value(obj) -> None:
    obj.value = 3


def via(m: Model) -> None:
    set_value(m)


def view(m: Model) -> list[int]:
    return m.fields
"#;

const USER: &str = r#"from pkg import Model, decide, view


class Other:
    def __init__(self) -> None:
        self.fields: list[int] = []


def sub_user() -> int:
    return decide()


def run() -> int:
    return decide()


def touch(m: Model, o: Other) -> None:
    m.fields = [1]
    o.fields = [2]


def grow(m: Model) -> None:
    view(m).append(1)
    m.fields.append(2)


def unknown(x) -> int:
    return x.decide()


def build() -> Other:
    return Other()


VALUE = decide()
"#;

const NIGHTLY: &str = r#"from pkg import decide


def run() -> int:
    return decide()


class Job:
    def run(self) -> int:
        return decide()
"#;

/// `pkg` defines `decide` and `Model`; `pkg.sub.user` and `pkg.jobs.nightly` use them.
fn project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("pkg/__init__.py", PKG)
        .write("pkg/sub/__init__.py", "")
        .write("pkg/sub/user.py", USER)
        .write("pkg/jobs/__init__.py", "")
        .write("pkg/jobs/nightly.py", NIGHTLY);
    p
}

/// `(path, line, qualname, target)` of every `rule` diagnostic under `config`.
fn reports(p: &Project, rule: &str, config: &str) -> Vec<(String, u64, String, String)> {
    p.write(
        "pyproject.toml",
        &format!("[project]\nname = \"t\"\n\n{config}"),
    );
    let (code, stdout, stderr) = p.check(&["--select", rule, "--output-format", "json"]);
    assert!(code == 0 || code == 1, "{config}: {stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    diags
        .iter()
        .map(|d| {
            assert_eq!(d["rule_code"], rule, "{d:#}");
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["qualname"].as_str().unwrap().to_owned(),
                d["target"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn d(path: &str, line: u64, qualname: &str, target: &str) -> (String, u64, String, String) {
    (path.into(), line, qualname.into(), target.into())
}

const USER_PY: &str = "pkg/sub/user.py";
const NIGHTLY_PY: &str = "pkg/jobs/nightly.py";
const PKG_PY: &str = "pkg/__init__.py";

fn call_owners(targets: &str, rest: &str) -> String {
    format!("[[tool.heki.call-owners]]\nname = \"home\"\ntargets = [{targets}]\n{rest}\n")
}

#[test]
fn a_module_part_names_the_module_without_its_submodules() {
    let p = project("module_callers");
    let callers = |targets: &str, rest: &str| reports(&p, "MUT204", &call_owners(targets, rest));
    let t = "pkg.decide";
    let outside_pkg = |target: &str| {
        vec![
            d(NIGHTLY_PY, 5, "run", target),
            d(NIGHTLY_PY, 10, "Job.run", target),
            d(USER_PY, 10, "sub_user", target),
            d(USER_PY, 14, "run", target),
            d(USER_PY, 35, "<module>", target),
        ]
    };
    // `pkg:*` names what `pkg/__init__.py` defines; `pkg.*` also names its submodules.
    assert_eq!(
        callers("\"pkg.decide\"", "allowed-callers = [\"pkg:*\"]"),
        outside_pkg(t)
    );
    assert_eq!(
        callers("\"pkg.decide\"", "allowed-callers = [\"pkg.*\"]"),
        vec![]
    );
    // A glob module part spans dots, and the qualname part is matched whole.
    assert_eq!(
        callers(
            "\"pkg.decide\"",
            "allowed-callers = [\"pkg:*\", \"pkg.*:run\"]"
        ),
        vec![
            d(NIGHTLY_PY, 10, "Job.run", t),
            d(USER_PY, 10, "sub_user", t),
            d(USER_PY, 35, "<module>", t),
        ]
    );
    // A target with a module part, and one whose module defines no such function.
    assert_eq!(
        callers("\"pkg:decide\"", "allowed-callers = [\"pkg:*\"]"),
        outside_pkg("pkg:decide")
    );
    assert_eq!(
        callers("\"pkg.sub:decide\"", "allowed-callers = [\"pkg:*\"]"),
        vec![]
    );
    // A module part naming no project module is no error; it names nothing.
    let mut everyone = vec![d(PKG_PY, 12, "root_user", t)];
    everyone.extend(outside_pkg(t));
    assert_eq!(
        callers("\"pkg.decide\"", "allowed-callers = [\"nowhere:*\"]"),
        everyone
    );
    // `forbidden-callers`, and an `exempt-methods` entry that is a pattern by its colon alone.
    assert_eq!(
        callers(
            "\"pkg.decide\"",
            "forbidden-callers = [\"pkg.jobs.nightly:*\"]"
        ),
        vec![d(NIGHTLY_PY, 5, "run", t), d(NIGHTLY_PY, 10, "Job.run", t)]
    );
    let others = "allowed-callers = [\"pkg.sub.*\", \"pkg.jobs.*\"]";
    assert_eq!(
        callers("\"pkg.decide\"", others),
        vec![d(PKG_PY, 12, "root_user", t)]
    );
    assert_eq!(
        callers(
            "\"pkg.decide\"",
            &format!("{others}\nexempt-methods = [\"pkg:root_user\"]")
        ),
        vec![]
    );
    // The strict mode counts the target's last name: `x.decide()` on an unknown object.
    assert_eq!(
        reports(
            &p,
            "MUT207",
            &call_owners(
                "\"pkg:decide\"",
                "allowed-callers = [\"pkg:*\"]\nunresolved = \"report\""
            )
        ),
        vec![d(USER_PY, 28, "unknown", "decide")]
    );
}

#[test]
fn a_module_part_in_the_class_of_attribute_targets() {
    let p = project("module_targets");
    let writes = |target: &str| {
        reports(
            &p,
            "MUT201",
            &format!(
                "[[tool.heki.write-owners]]\nname = \"own\"\ntargets = [\"{target}\"]\nallowed-writers = [\"pkg:*\"]\n"
            ),
        )
    };
    // `pkg:*.fields` names `pkg.Model`, not `pkg.sub.user.Other` (line 19).
    let model = |t: &str| {
        vec![
            d(USER_PY, 18, "touch", t),
            d(USER_PY, 23, "grow", t),
            d(USER_PY, 24, "grow", t),
        ]
    };
    assert_eq!(writes("pkg:*.fields"), model("pkg:*.fields"));
    let mut both = model("pkg.*.fields");
    both.insert(1, d(USER_PY, 19, "touch", "pkg.*.fields"));
    assert_eq!(writes("pkg.*.fields"), both);
    assert_eq!(writes("pkg:Model.fields"), model("pkg:Model.fields"));
    assert_eq!(writes("pkg.sub:Model.fields"), vec![]);
    let reads = |allowed: &str| {
        reports(
            &p,
            "MUT203",
            &format!(
                "[[tool.heki.read-owners]]\nname = \"read\"\ntargets = [\"pkg:Model.fields\"]\nallowed-readers = [\"{allowed}\"]\n"
            ),
        )
    };
    assert_eq!(
        reads("pkg:*"),
        vec![d(USER_PY, 24, "grow", "pkg:Model.fields")]
    );
    assert_eq!(
        reads("nowhere:*"),
        vec![
            d(PKG_PY, 36, "view", "pkg:Model.fields"),
            d(USER_PY, 24, "grow", "pkg:Model.fields"),
        ]
    );
}

#[test]
fn a_module_part_in_effect_boundaries() {
    let p = project("module_boundaries");
    let boundary = |scopes: &str, targets: &str, rest: &str| {
        reports(
            &p,
            "EFF201",
            &format!(
                "[[tool.heki.effect-boundaries]]\nname = \"pure\"\nscopes = [\"{scopes}\"]\nforbid = [\"calls\"]\ncall-targets = [\"{targets}\"]\n{rest}\n"
            ),
        )
    };
    let t = "pure:calls:pkg:decide";
    assert_eq!(
        boundary("pkg:*", "pkg:decide", ""),
        vec![d(PKG_PY, 12, "root_user", t)]
    );
    // A scope without a module part names the functions of `pkg` and of its submodules.
    assert_eq!(
        boundary("pkg", "pkg:decide", ""),
        vec![
            d(PKG_PY, 12, "root_user", t),
            d(NIGHTLY_PY, 5, "run", t),
            d(NIGHTLY_PY, 10, "Job.run", t),
            d(USER_PY, 10, "sub_user", t),
            d(USER_PY, 14, "run", t),
        ]
    );
    // `allowed-callees` names a constructed class by its module.
    let builds = |rest: &str| boundary("pkg.sub.user:build", "pkg.sub.user:Other", rest);
    let constructs = vec![d(USER_PY, 32, "build", "pure:calls:pkg.sub.user:Other")];
    assert_eq!(builds(""), constructs);
    assert_eq!(builds("allowed-callees = [\"pkg.sub.user:Other\"]"), vec![]);
    assert_eq!(builds("allowed-callees = [\"pkg:Other\"]"), constructs);
}

#[test]
fn a_module_part_in_invalidation_and_readonly_returns() {
    let p = project("module_summaries");
    let invalidation = |targets: &str, rest: &str| {
        reports(
            &p,
            "MUT109",
            &format!(
                "[[tool.heki.invalidation]]\nname = \"inv\"\ntargets = [\"{targets}\"]\n{rest}\n"
            ),
        )
    };
    // `via` writes through `set_value`, which the summary of `via` ties to `Model`.
    let t = "inv:pkg:Model.value";
    let notify = "invalidators = [\"pkg:notify\"]";
    assert_eq!(
        invalidation("pkg:Model.value", notify),
        vec![d(PKG_PY, 19, "change", t), d(PKG_PY, 32, "via", t)]
    );
    assert_eq!(invalidation("pkg.sub:Model.value", notify), vec![]);
    assert_eq!(
        invalidation("pkg:Model.value", "invalidators = [\"pkg.sub:notify\"]"),
        vec![
            d(PKG_PY, 19, "change", t),
            d(PKG_PY, 23, "change_and_notify", t),
            d(PKG_PY, 32, "via", t),
        ]
    );
    assert_eq!(
        invalidation(
            "pkg:Model.value",
            &format!("{notify}\nexempt-writers = [\"pkg:change\"]")
        ),
        vec![d(PKG_PY, 32, "via", t)]
    );
    // The summaries tell `pkg.Model` from `pkg.sub.user.Other` (line 19).
    let fields = |t: &str| {
        vec![
            d(USER_PY, 18, "touch", t),
            d(USER_PY, 23, "grow", t),
            d(USER_PY, 24, "grow", t),
        ]
    };
    assert_eq!(
        invalidation("pkg:*.fields", notify),
        fields("inv:pkg:*.fields")
    );
    let mut both = fields("inv:pkg.*.fields");
    both.insert(1, d(USER_PY, 19, "touch", "inv:pkg.*.fields"));
    assert_eq!(invalidation("pkg.*.fields", notify), both);
    let producers = |producer: &str| {
        reports(
            &p,
            "MUT202",
            &format!(
                "[[tool.heki.readonly-returns]]\nname = \"ro\"\nproducers = [\"{producer}\"]\n"
            ),
        )
    };
    assert_eq!(
        producers("pkg:view"),
        vec![d(USER_PY, 23, "grow", "pkg.view")]
    );
    assert_eq!(producers("pkg.sub:view"), vec![]);
    assert_eq!(
        producers("pkg:Model.fields"),
        vec![
            d(USER_PY, 23, "grow", "pkg.Model.fields"),
            d(USER_PY, 24, "grow", "pkg.Model.fields"),
        ]
    );
    assert_eq!(producers("pkg.sub:Model.fields"), vec![]);
}

#[test]
fn malformed_module_patterns_exit_2() {
    let p = project("module_errors");
    for (config, needle) in [
        (
            call_owners("\"pkg.decide\"", "allowed-callers = [\"pkg:\"]"),
            "invalid pattern `pkg:` in call-owners.allowed-callers (expected `module:qualname`",
        ),
        (
            "[[tool.heki.effect-boundaries]]\nname = \"b\"\nscopes = [\":f\"]\nforbid = [\"calls\"]\ncall-targets = [\"pkg.decide\"]\n".to_owned(),
            "invalid pattern `:f` in effect-boundaries.scopes",
        ),
        (
            "[[tool.heki.write-owners]]\nname = \"w\"\ntargets = [\"a:b:c.d\"]\n".to_owned(),
            "invalid pattern `a:b:c` in write-owners.targets",
        ),
        // The attribute comes after the module part.
        (
            "[[tool.heki.write-owners]]\nname = \"w\"\ntargets = [\"pkg:Model\"]\n".to_owned(),
            "invalid write-owners target `pkg:Model` (expected `Class.attr`)",
        ),
        (
            "[[tool.heki.readonly-returns]]\nname = \"r\"\nproducers = [\"pkg:[x\"]\n".to_owned(),
            "invalid glob `pkg:[x` in readonly-returns.producers",
        ),
    ] {
        p.write(
            "pyproject.toml",
            &format!("[project]\nname = \"t\"\n\n{config}"),
        );
        let (code, stdout, stderr) = p.check(&[]);
        assert_eq!(code, 2, "{config}: {stdout}{stderr}");
        assert!(stderr.contains(needle), "{config}: {stderr}");
    }
}
