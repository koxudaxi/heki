//! Owner-contract targets naming symbols outside the project, end to end through the real
//! binary: a full dotted name (`sys._getframe`) and `<external>.<name>`, the members of every
//! module and class outside the project, which a glob leaves out of when they are public API
//! though they start with an underscore. Calls and reads are judged at the symbol heki resolves
//! them to, and the unmatched-entries check does not call such a target unmatched when the
//! project uses it, nor when it names code outside the project that heki cannot list.

mod common;

use common::Project;
use serde_json::Value;

/// `(line, code, function, target)` of every diagnostic, sorted, and the warnings.
fn found(p: &Project) -> (Vec<(u64, String, String, String)>, String) {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut out: Vec<(u64, String, String, String)> = diags
        .iter()
        .map(|d| {
            let message = d["message"].as_str().unwrap();
            let quoted: Vec<&str> = message.split('`').skip(1).step_by(2).collect();
            (
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
                quoted[0].to_owned(),
                quoted[1].to_owned(),
            )
        })
        .collect();
    out.sort();
    (out, stderr)
}

fn at(line: u64, code: &str, func: &str, target: &str) -> (u64, String, String, String) {
    (line, code.to_owned(), func.to_owned(), target.to_owned())
}

/// The shape datamodel-code-generator reported: a module of a layer that must use no private
/// member of another module calls `sys._getframe` and hands it on; `_replace` of a named tuple
/// is public API.
#[test]
fn a_full_name_targets_a_symbol_outside_the_project() {
    let p = Project::new("outside_targets_repro");
    p.write(
        "pyproject.toml",
        r#"[project]
name = "h3"
version = "0"
requires-python = ">=3.11"

[tool.heki.lint]
select = ["MUT203", "MUT204", "MUT206", "MUT207"]

[[tool.heki.call-owners]]
name = "outside-private-call"
targets = ["sys._getframe", "*._*"]
forbidden-callers = ["pkg.parser._pipeline:*"]
attribution = "lexical"

[[tool.heki.read-owners]]
name = "outside-private-read"
targets = ["sys._getframe", "*._*"]
forbidden-readers = ["pkg.parser._pipeline:*"]
attribution = "lexical"
"#,
    )
    .write("pkg/__init__.py", "")
    .write("pkg/parser/__init__.py", "")
    .write(
        "pkg/parser/_pipeline.py",
        r#"import sys
from collections import namedtuple

Pair = namedtuple("Pair", "a b")


def caller_frame() -> str:
    return sys._getframe(2).f_code.co_name


def frame_function() -> object:
    return sys._getframe


def public_api(pair: Pair) -> Pair:
    return pair._replace(a=1)
"#,
    );
    let (diags, warnings) = found(&p);
    assert_eq!(
        diags,
        vec![
            at(8, "MUT204", "caller_frame", "sys._getframe"),
            at(12, "MUT203", "frame_function", "sys._getframe"),
        ]
    );
    assert!(!warnings.contains("sys._getframe"), "{warnings}");
}

const COMPAT: &str = "from sys import _getframe\n\n__all__ = [\"_getframe\"]\n";

const USE: &str = r#"import enum
import os
import sys
import typing
from collections import namedtuple
from sys import _getframe as getframe

from pkg.compat import _getframe

Pair = namedtuple("Pair", "a b")


def reexported() -> str:
    return _getframe(1).f_code.co_name


def aliased() -> str:
    return getframe(1).f_code.co_name


def bound_then_called() -> str:
    frame = getframe
    return frame(1).f_code.co_name


def local_import() -> str:
    from sys import _getframe as frame

    return frame(1).f_code.co_name


def private_call() -> None:
    os._exit(0)


def public() -> str:
    return os.getcwd()


def dunder() -> object:
    return sys.__stdout__


def sunder() -> object:
    return enum.Enum._missing_, enum.Enum._name_, enum.Enum._value_


def namedtuple_api() -> object:
    nt = typing.NamedTuple
    return nt._make, nt._asdict, nt._replace, nt._fields, nt._field_defaults


def pair_api(pair: Pair) -> Pair:
    return pair._replace(a=1)
"#;

fn external_project(name: &str, read_targets: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT203\", \"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"calls\"\ntargets = [\"<external>._*\"]\nforbidden-callers = [\"pkg.parser.*\"]\n\n[[tool.heki.read-owners]]\nname = \"reads\"\ntargets = {read_targets}\nforbidden-readers = [\"pkg.parser.*\"]\n"
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/compat.py", COMPAT)
    .write("pkg/parser/__init__.py", "")
    .write("pkg/parser/use.py", USE);
    p
}

/// `<external>._*` names the private members of modules and classes outside the project, at
/// the symbol a call or a read resolves to: through `from sys import _getframe`, an alias, and a
/// project module re-exporting it. An import binding it is a read, at the import (`<module>`,
/// in `pkg.parser.use`; `pkg.compat` is no forbidden reader); a name loaded only to be called is
/// the call's, one bound to a local first is read there. Public names, dunder and sunder names,
/// and the `namedtuple` API are left out. Written out, `<external>._replace` names that name.
#[test]
fn external_members_are_the_private_members_outside_the_project() {
    let (diags, warnings) = found(&external_project("external_members", "[\"<external>._*\"]"));
    let getframe = "sys._getframe";
    assert_eq!(
        diags,
        vec![
            at(6, "MUT203", "<module>", getframe),
            at(8, "MUT203", "<module>", getframe),
            at(14, "MUT204", "reexported", getframe),
            at(18, "MUT204", "aliased", getframe),
            at(22, "MUT203", "bound_then_called", getframe),
            at(23, "MUT204", "bound_then_called", getframe),
            at(27, "MUT203", "local_import", getframe),
            at(29, "MUT204", "local_import", getframe),
            at(33, "MUT204", "private_call", "os._exit"),
        ]
    );
    assert!(warnings.is_empty(), "{warnings}");
    let (diags, _) = found(&external_project(
        "external_members_written",
        "[\"<external>._replace\", \"<external>.__stdout__\"]",
    ));
    let reads: Vec<_> = diags.iter().filter(|d| d.1 == "MUT203").collect();
    assert_eq!(
        reads,
        vec![
            &at(41, "MUT203", "dunder", "sys.__stdout__"),
            &at(50, "MUT203", "namedtuple_api", "typing.NamedTuple._replace"),
        ]
    );
}

/// A target naming code outside the project is matched when the project uses it, and is not
/// judged when nothing does but its first name is a module outside the project (of the standard
/// library, or one the project imports) or it is an `<external>` entry. A misspelled class is
/// still reported.
#[test]
fn outside_targets_are_no_unmatched_entries() {
    let targets = "[\"sys._getframe\", \"os._exit\", \"yaml._private\", \"<external>._x*\", \"Feild.removed\"]";
    let p = external_project("external_unmatched", targets);
    p.write(
        "pkg/loader.py",
        "import yaml\n\n\ndef load(text):\n    return yaml.safe_load(text)\n",
    );
    let (_, warnings) = found(&p);
    let warned: Vec<&str> = warnings
        .lines()
        .filter(|l| l.starts_with("warning:"))
        .collect();
    assert_eq!(
        warned,
        vec![
            "warning: `targets` entry `Feild.removed` of read-owners `reads` names no class in the analyzed files"
        ],
        "{warnings}"
    );
}

/// `<external>` takes one name after it; a bare `<external>` is no `Class.attr` either.
#[test]
fn a_misplaced_external_holder_is_a_configuration_error() {
    for (target, needle) in [
        (
            "<external>.os._exit",
            "`<external>` stands for every module and class outside the project",
        ),
        (
            "pkg.<external>._x",
            "`<external>` stands for every module and class outside the project",
        ),
        (
            "<external>",
            "invalid read-owners target `<external>` (expected `Class.attr`)",
        ),
    ] {
        let p = external_project("external_bad", &format!("[\"{target}\"]"));
        let (code, stdout, stderr) = p.check(&[]);
        assert_eq!(code, 2, "{target}: {stdout}{stderr}");
        assert!(stderr.contains(needle), "{target}: {stderr}");
    }
}

/// An effect boundary's `call-targets` take `<external>._*` too; its report names the entry.
#[test]
fn a_boundary_forbids_calling_external_private_members() {
    let p = external_project("external_boundary", "[\"pkg.compat.Unused.x\"]");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"EFF201\"]\n\n[[tool.heki.effect-boundaries]]\nname = \"no-private\"\nscopes = [\"pkg.parser.use.private_call\", \"pkg.parser.use.public\"]\nforbid = [\"calls\"]\ncall-targets = [\"<external>._*\"]\n",
    );
    let (diags, _) = found(&p);
    assert_eq!(
        diags,
        vec![at(33, "EFF201", "private_call", "<external>._*")]
    );
}

/// Contracts on symbols outside the project, each with its allowed functions: `pkg.frames`
/// calls and reads `sys._getframe`, `pkg.exits` calls `os._exit` and also `sys._getframe`.
fn allowed_outside_project() -> Project {
    let p = Project::new("outside_allowed_examples");
    p.write(
        "pyproject.toml",
        r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT203", "MUT204"]

[[tool.heki.call-owners]]
name = "frames"
targets = ["sys._getframe"]
allowed-callers = ["pkg.frames.*"]

[[tool.heki.call-owners]]
name = "exits"
targets = ["os._exit"]
allowed-callers = ["pkg.exits.*"]

[[tool.heki.read-owners]]
name = "frame-reads"
targets = ["sys._getframe"]
allowed-readers = ["pkg.frames.*"]
"#,
    )
    .write("pkg/__init__.py", "")
    .write(
        "pkg/frames.py",
        "import sys\n\n\ndef caller() -> str:\n    return sys._getframe(1).f_code.co_name\n\n\ndef frame_function() -> object:\n    return sys._getframe\n",
    )
    .write(
        "pkg/exits.py",
        "import os\nimport sys\n\n\ndef quit_now() -> None:\n    sys._getframe(0)\n    os._exit(0)\n",
    )
    .write(
        "pkg/other.py",
        "import os\nimport sys\n\n\ndef bad_call() -> str:\n    return sys._getframe(1).f_code.co_name\n\n\ndef bad_read() -> object:\n    return sys._getframe\n\n\ndef bad_exit() -> None:\n    os._exit(1)\n",
    );
    p
}

/// A report on a symbol outside the project names the allowed functions that use it the same
/// way today: the allowed callers calling it, not an allowed reader only reading it, nor a
/// function another contract allows; the allowed readers reading it.
#[test]
fn reports_on_outside_symbols_name_the_allowed_functions_using_them() {
    let p = allowed_outside_project();
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let found: Vec<(String, u64, String, Vec<String>)> = diags
        .iter()
        .map(|d| {
            let examples = d["allowed_examples"].as_array().unwrap();
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
                examples
                    .iter()
                    .map(|e| e["fqn"].as_str().unwrap().to_owned())
                    .collect(),
            )
        })
        .collect();
    let report = |path: &str, line: u64, code: &str, example: &str| {
        (
            path.to_owned(),
            line,
            code.to_owned(),
            vec![example.to_owned()],
        )
    };
    assert_eq!(
        found,
        vec![
            report("pkg/exits.py", 6, "MUT204", "pkg.frames.caller"),
            report("pkg/other.py", 6, "MUT204", "pkg.frames.caller"),
            report("pkg/other.py", 10, "MUT203", "pkg.frames.frame_function"),
            report("pkg/other.py", 14, "MUT204", "pkg.exits.quit_now"),
        ]
    );
}

/// `heki inspect` gives a function no call-target role of an `<external>` target, which names
/// symbols outside the project only, also when its name matches the target's last name.
#[test]
fn an_external_target_names_no_project_function() {
    let p = external_project("external_roles", "[\"<external>._*\"]");
    p.write(
        "pkg/parser/helpers.py",
        "def _private() -> None:\n    pass\n",
    );
    let (code, stdout, stderr) = p.heki(&[
        "inspect",
        "pkg.parser.helpers._private",
        "--output-format",
        "json",
    ]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    let report: Value = serde_json::from_str(&stdout).unwrap();
    let roles: Vec<(&str, Vec<&str>)> = report["contracts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            let roles = c["roles"].as_array().unwrap();
            (
                c["kind"].as_str().unwrap(),
                roles.iter().map(|r| r.as_str().unwrap()).collect(),
            )
        })
        .collect();
    assert_eq!(
        roles,
        vec![
            ("read-owners", vec!["forbidden-reader"]),
            ("call-owners", vec!["forbidden-caller"]),
        ]
    );
}
