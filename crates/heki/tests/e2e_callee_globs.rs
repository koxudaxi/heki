//! Call targets naming every callee by an argument list (`*(module="pkg.backend.*")`), string
//! globs in argument lists, and argument lists no function or class a target names can take,
//! end to end through the real binary.

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

fn report(path: &str, line: u64, code: &str, message: &str) -> Report {
    (path.to_owned(), line, code.to_owned(), message.to_owned())
}

const LIB: &str = r#"def make(module, kind=None):
    return module


def keywords(**options):
    return options


class Backend:
    def __init__(self, name):
        self.name = name

    def run(self, kind=None):
        return kind
"#;

const PARSER: &str = r#"from pkg.lib import Backend, keywords, make

BACKEND = "pkg.backend.dataclass"


def build(factory):
    return factory(module=BACKEND)


def through_method(loader):
    return loader.build(module="pkg.backend.msgspec")


def outside():
    return dict(module=BACKEND)


def project_function():
    return make(module=BACKEND)


def project_kwargs():
    return keywords(module=BACKEND)


def through_mapping(factory):
    options = {"module": BACKEND}
    return factory(**options)


def positional(factory):
    return factory(BACKEND)


def unknown_value(factory, name):
    return factory(module=name)


def other_value(factory):
    return factory(module="pkg.frontend.dataclass")


def other_keyword(factory):
    return factory(backend=BACKEND)


def untargeted(loader):
    return loader.run(kind="x")


def numeric(factory):
    return factory(module=1)
"#;

/// A module the contract does not forbid: its calls are allowed.
const ALLOWED: &str = r#"BACKEND = "pkg.backend.dataclass"


def build(factory):
    return factory(module=BACKEND)
"#;

fn project(name: &str, targets: &str, unresolved: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!(
            r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT204", "MUT207"]

[[tool.heki.call-owners]]
name = "backend-module"
targets = [{targets}]
forbidden-callers = ["pkg.parser:*"]
unresolved = "{unresolved}"
attribution = "lexical"
"#
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/lib.py", LIB)
    .write("pkg/parser.py", PARSER)
    .write("pkg/allowed.py", ALLOWED);
    p
}

fn calls(line: u64, caller: &str, called: &str) -> Report {
    report(
        "pkg/parser.py",
        line,
        "MUT204",
        &format!("`{caller}` calls `{called}` inside the forbidden callers of `backend-module`"),
    )
}

#[test]
fn a_callee_glob_names_every_call_passing_the_constants() {
    let p = project(
        "callee_glob",
        r#"'*(module="pkg.backend.dataclass")', "Backend.run""#,
        "report",
    );
    let target = r#"*(module="pkg.backend.dataclass")"#;
    let (reports, warnings) = run(&p);
    // A parameter, a library class, a project function, a function taking `**kwargs`, and the
    // entries of a mapping the function builds: whatever the callee, a keyword passing the
    // constant matches. A position, a value heki cannot tell, another value or keyword, and
    // the module the contract does not forbid are not reported, and a call on an object of
    // unknown class is judged by its arguments, not by its name: only the target naming a
    // method reports one under `MUT207`.
    let project_function = r#"pkg.lib.make(module="pkg.backend.dataclass")"#;
    let project_kwargs = r#"pkg.lib.keywords(module="pkg.backend.dataclass")"#;
    assert_eq!(
        reports,
        vec![
            calls(7, "build", target),
            calls(15, "outside", target),
            calls(19, "project_function", project_function),
            calls(23, "project_kwargs", project_kwargs),
            calls(28, "through_mapping", target),
            report(
                "pkg/parser.py",
                48,
                "MUT207",
                "`untargeted` calls `run` on an object of unknown class inside the forbidden callers of `backend-module`: heki cannot tell whether it calls `Backend.run`",
            ),
        ]
    );
    assert_eq!(warnings, Vec::<String>::new());
}

#[test]
fn a_string_glob_matches_the_strings_it_matches_whole() {
    let p = project("string_glob", r#"'*(module="pkg.backend.*")'"#, "ignore");
    let target = r#"*(module="pkg.backend.*")"#;
    let (reports, _) = run(&p);
    // A method of an object of unknown class is judged by its arguments too, whatever
    // `unresolved` says: the target names every callee.
    let project_function = r#"pkg.lib.make(module="pkg.backend.*")"#;
    let project_kwargs = r#"pkg.lib.keywords(module="pkg.backend.*")"#;
    assert_eq!(
        reports,
        vec![
            calls(7, "build", target),
            calls(11, "through_method", target),
            calls(15, "outside", target),
            calls(19, "project_function", project_function),
            calls(23, "project_kwargs", project_kwargs),
            calls(28, "through_mapping", target),
        ]
    );
    // On a named callee, the glob matches the same way.
    let p = project(
        "string_glob_named",
        r#"'make(module="pkg.*.data*")'"#,
        "ignore",
    );
    let (reports, _) = run(&p);
    assert_eq!(
        reports,
        vec![calls(
            19,
            "project_function",
            r#"make(module="pkg.*.data*")"#
        )]
    );
}

#[test]
fn an_argument_list_no_named_callable_takes_is_warned_about() {
    let p = project(
        "untaken_parameter",
        r#"'make(backend="x")', 'keywords(backend="x")', 'Backend(name="x")', 'Backend(kind="x")', 'make(module="x", backend=1)'"#,
        "ignore",
    );
    let (_, warnings) = run(&p);
    // `keywords` takes `**options`, and `Backend` takes `name`.
    let warned = |entry: &str, taking: &str| {
        format!(
            "warning: `targets` entry `{entry}` of call-owners `backend-module` (from pyproject.toml:9; contract at pyproject.toml:7) names no function, method, or class taking {taking} in the analyzed files"
        )
    };
    assert_eq!(
        warnings,
        vec![
            warned(r#"make(backend="x")"#, "`backend`"),
            warned(r#"Backend(kind="x")"#, "`kind`"),
            warned(r#"make(module="x", backend=1)"#, "`module` and `backend`"),
        ]
    );
}

#[test]
fn a_module_qualified_argument_list_is_checked_against_the_project_declarations() {
    let p = project(
        "module_qualified_parameter",
        r#"'pkg.lib:make(backend="x")', 'pkg.lib:make(module="x")', 'pkg.parser:dumps(indent=1)'"#,
        "ignore",
    );
    // `pkg.parser` binds the library function `dumps`, whose signature heki does not have.
    p.write(
        "pkg/parser.py",
        &format!("from json import dumps\n\n{PARSER}"),
    );
    let (_, warnings) = run(&p);
    assert_eq!(
        warnings,
        vec![
            "warning: `targets` entry `pkg.lib:make(backend=\"x\")` of call-owners `backend-module` (from pyproject.toml:9; contract at pyproject.toml:7) names no function, method, or class taking `backend` in the analyzed files"
        ]
    );
}

#[test]
fn a_bad_string_glob_is_a_configuration_error() {
    let p = project("bad_string_glob", r#"'*(module="pkg.[backend")'"#, "ignore");
    let (code, _, stderr) = p.check(&[]);
    assert_eq!(code, 2, "{stderr}");
    assert!(stderr.contains("a string glob is not valid"), "{stderr}");
}

#[test]
fn an_effect_boundary_names_every_call_passing_the_constants() {
    let p = Project::new("callee_glob_boundary");
    p.write(
        "pyproject.toml",
        r#"[project]
name = "t"

[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "no-backend"
scopes = ["pkg.parser.build", "pkg.parser.positional"]
forbid = ["calls"]
call-targets = ['*(module="pkg.backend.*")']
"#,
    )
    .write("pkg/__init__.py", "")
    .write("pkg/lib.py", LIB)
    .write("pkg/parser.py", PARSER);
    let (reports, _) = run(&p);
    assert_eq!(reports.len(), 1, "{reports:?}");
    let (path, line, code, message) = &reports[0];
    assert_eq!(
        (path.as_str(), *line, code.as_str()),
        ("pkg/parser.py", 7, "EFF201")
    );
    assert!(
        message.contains(r#"`*(module="pkg.backend.*")`"#),
        "{message}"
    );
}

/// A project forbidding the calls of `pkg.client` that `targets` names, by `call-owners` and
/// by an effect boundary on `pkg.client.fetch`.
fn client_project(name: &str, targets: &str, client: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!(
            r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT204", "EFF201"]

[[tool.heki.call-owners]]
name = "blocked"
targets = [{targets}]
forbidden-callers = ["pkg.client:*"]
attribution = "lexical"

[[tool.heki.effect-boundaries]]
name = "no-blocked"
scopes = ["pkg.client.fetch"]
forbid = ["calls"]
call-targets = [{targets}]
"#
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/client.py", client);
    p
}

/// `(line, code)` of every report of `p`.
fn lines_and_codes(p: &Project) -> Vec<(u64, String)> {
    let (reports, _) = run(p);
    (reports.into_iter())
        .map(|(_, line, code, _)| (line, code))
        .collect()
}

#[test]
fn a_catalogued_library_call_passes_the_constants_it_writes() {
    // `requests.get` makes an ecosystem site of its own; its keywords still name the target.
    let client = r#"import requests


def fetch(url):
    return requests.get(url, params="blocked")


def other(url):
    return requests.get(url, params="open")
"#;
    let p = client_project("callee_glob_library", r#"'*(params="blocked")'"#, client);
    assert_eq!(
        lines_and_codes(&p),
        vec![(5, "EFF201".to_owned()), (5, "MUT204".to_owned())]
    );
}

#[test]
fn a_glob_character_in_brackets_matches_itself() {
    // `[*]`, `[{]`, `[[]`, and `[?]` are the characters themselves, also beside a group with
    // an empty alternative, which expands around them. The boundary reports the first call of
    // each target, `MUT204` every call.
    let client = r#"def fetch(factory):
    factory(module="pkg*")
    factory(module="pkgx")
    factory(module="pkg.{")
    factory(module="pkg.{.x")
    factory(module="pkg.y")
    factory(module="q[?")
    return factory(module="q[x")
"#;
    let p = client_project(
        "callee_glob_literal",
        r#"'*(module="pkg[*]")', '*(module="pkg.[{]{,.x}")', '*(module="q[[][?]")'"#,
        client,
    );
    let report = |line, code: &str| (line, code.to_owned());
    assert_eq!(
        lines_and_codes(&p),
        vec![
            report(2, "EFF201"),
            report(2, "MUT204"),
            report(4, "EFF201"),
            report(4, "MUT204"),
            report(5, "MUT204"),
            report(7, "EFF201"),
            report(7, "MUT204"),
        ]
    );
}
