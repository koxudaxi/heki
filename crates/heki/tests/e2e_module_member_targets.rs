//! Call targets with a module part (`pkg:Kind`, `pkg:*`) naming the names a module binds to
//! definitions of other modules, read or called through the module object (`pkg.Kind.A`),
//! end to end through the real binary: judged at the access, not at the definition the name
//! resolves to.

mod common;

use common::Project;
use serde_json::Value;

type Report = (String, u64, String, String);

/// `(path, line, code, message)` of every report of `p`, sorted, and the warnings on stderr.
fn run(p: &Project) -> (Vec<Report>, Vec<String>) {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<Report> = diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().replace('\\', "/"),
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
                d["message"].as_str().unwrap().to_owned(),
            )
        })
        .collect();
    out.sort();
    let warnings = stderr
        .lines()
        .filter(|l| l.starts_with("warning: `targets` entry"))
        .map(str::to_owned)
        .collect();
    (out, warnings)
}

const USE: &str = r#"import pkg.fmt


def through_root():
    return pkg.Kind.A


def direct():
    from pkg.enums import Kind

    return Kind.A


def imported_from_root():
    from pkg import Kind

    return Kind.A


def submodule():
    return pkg.fmt.X


def own_definition():
    return pkg.helper()
"#;

const CALLS: &str = r#"import pkg as root
from pkg.enums import Box


def through_alias():
    return root.Kind.A


def called():
    return root.make()


def constructed():
    return root.Kind(1)


def handed():
    return [root.Kind]


def on_objects():
    Box.create()
    return Box().run()
"#;

fn project(name: &str, targets: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!(
            r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT204"]

[[tool.heki.call-owners]]
name = "root-name"
targets = [{targets}]
forbidden-callers = ["pkg.use:*", "pkg.calls:*"]
references = "reads"
attribution = "lexical"
"#
        ),
    )
    .write(
        "pkg/__init__.py",
        "from pkg.enums import Kind as Kind\nfrom pkg.enums import make as make\n\n\ndef helper():\n    return 1\n",
    )
    .write(
        "pkg/enums.py",
        "import enum\n\n\nclass Kind(enum.Enum):\n    A = 1\n\n\ndef make():\n    return Kind.A\n\n\nclass Box:\n    @classmethod\n    def create(cls):\n        return cls()\n\n    def run(self):\n        return 1\n",
    )
    .write("pkg/fmt.py", "X = 1\n")
    .write("pkg/use.py", USE)
    .write("pkg/calls.py", CALLS);
    p
}

fn report(path: &str, line: u64, caller: &str, verb: &str, named: &str) -> Report {
    (
        path.to_owned(),
        line,
        "MUT204".to_owned(),
        format!("`{caller}` {verb} `{named}` inside the forbidden callers of `root-name`"),
    )
}

#[test]
fn a_module_part_names_a_read_through_the_module_object() {
    let p = project("module_member_read", r#""pkg:Kind""#);
    let (reports, warnings) = run(&p);
    // The import of the name, from the root or from the defining module, and its uses are
    // judged at the class it resolves to, which the target does not name.
    assert_eq!(
        reports,
        vec![
            report("pkg/calls.py", 6, "through_alias", "reads", "pkg:Kind"),
            report("pkg/calls.py", 14, "constructed", "constructs", "pkg:Kind"),
            report("pkg/calls.py", 18, "handed", "hands on", "pkg:Kind"),
            report("pkg/use.py", 5, "through_root", "reads", "pkg:Kind"),
        ]
    );
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn a_module_glob_names_every_name_read_through_the_module_object() {
    let p = project("module_member_glob", r#""pkg:*""#);
    let (reports, _) = run(&p);
    // A submodule read through its package is a module, not a name the package binds; the
    // package's own function is judged at its definition, as before.
    assert_eq!(
        reports,
        vec![
            report("pkg/calls.py", 6, "through_alias", "reads", "pkg.Kind"),
            report("pkg/calls.py", 10, "called", "calls", "pkg.make"),
            report("pkg/calls.py", 14, "constructed", "constructs", "pkg.Kind"),
            report("pkg/calls.py", 18, "handed", "hands on", "pkg.Kind"),
            report("pkg/use.py", 5, "through_root", "reads", "pkg.Kind"),
            report("pkg/use.py", 25, "own_definition", "calls", "pkg.helper"),
        ]
    );
}

#[test]
fn a_dotted_target_stays_judged_at_the_definition() {
    // Without a module part, a target names the definition the access resolves to.
    let p = project(
        "module_member_dotted",
        r#""pkg.Kind", "pkg.enums.make", "pkg.missing:Kind""#,
    );
    let (reports, warnings) = run(&p);
    assert_eq!(
        reports,
        vec![report(
            "pkg/calls.py",
            10,
            "called",
            "calls",
            "pkg.enums.make"
        )]
    );
    assert_eq!(
        warnings,
        vec![
            "warning: `targets` entry `pkg.Kind` of call-owners `root-name` (from pyproject.toml:9; contract at pyproject.toml:7) names no function, method, or class in the analyzed files"
                .to_owned(),
            "warning: `targets` entry `pkg.missing:Kind` of call-owners `root-name` (from pyproject.toml:9; contract at pyproject.toml:7) names no function, method, or class in the analyzed files"
                .to_owned(),
        ]
    );
}

#[test]
fn an_effect_boundary_judges_a_call_through_the_module_object() {
    let p = Project::new("module_member_boundary");
    p.write(
        "pyproject.toml",
        r#"[project]
name = "t"

[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "no-root"
scopes = ["pkg.calls.called", "pkg.calls.through_alias"]
forbid = ["calls"]
call-targets = ["pkg:make"]
"#,
    )
    .write("pkg/__init__.py", "from pkg.enums import make as make\n")
    .write("pkg/enums.py", "def make():\n    return 1\n")
    .write(
        "pkg/calls.py",
        CALLS
            .replace("from pkg.enums import Box\n", "")
            .split("\n\ndef constructed")
            .next()
            .unwrap(),
    );
    let (reports, _) = run(&p);
    assert_eq!(reports.len(), 1, "{reports:?}");
    let (path, line, code, message) = &reports[0];
    assert_eq!(
        (path.as_str(), *line, code.as_str()),
        ("pkg/calls.py", 9, "EFF201")
    );
    assert!(message.contains("`pkg:make`"), "{message}");
}
