//! The declared results of builtin and standard-library calls and attribute reads, from the
//! table generated from typeshed's stubs (`kb/ecosystem/typeshed_stdlib.tsv`): a call on the
//! result of another resolves, end to end through the binary. An effect boundary reporting
//! unresolved calls marks each call heki still cannot resolve; every other call of the scopes
//! resolves.

mod common;

use common::Project;
use serde_json::Value;

const CONFIG: &str = r#"[project]
name = "pkg"
version = "0"

[[tool.heki.effect-boundaries]]
name = "no-required"
scopes = ["pkg.m.*"]
forbid = ["writes-targets"]
write-targets = ["Field.required"]
unresolved = "report"
"#;

/// The issue's shapes and the forms of a declared result: a method's (`str.strip -> str`), a
/// function's (`shlex.split -> list[str]`), `Self` (`astimezone`), a class method called by
/// name (`datetime.now`), a method a library base declares (`bool.bit_length`, from `int`), a
/// class spelled by its public re-export (`io.StringIO`), a property (`datetime.year`), an
/// attribute (a named tuple's field), a container's type argument (`deque[str].popleft`), the
/// overload of a subscript by its index (`bytes[0]` is an `int`), an enumeration's member and
/// its value of the mixed-in type, and a class pattern narrowing its subject.
const SHAPES: &str = r#"import collections
import datetime
import io
import shlex
import shutil
from enum import Enum


class Version(str, Enum):
    A = "3.12"

    def label(self) -> str:
        return self.value


def run_strip(text: str) -> str:
    return text.strip().lower()


def run_split(text: str) -> str:
    return text.split(".")[0].upper()


def run_dates(d: datetime.datetime) -> object:
    return d.astimezone().utctimetuple()


def run_sets(a: set[str], b: set[str]) -> set[str]:
    return a.union(b).difference(a)


def run_enum() -> list[str]:
    return Version.A.value.split(".")


def run_enum_parameter(v: Version) -> list[str]:
    return v.value.split(".")


def run_match(value: object) -> object:
    match value:
        case dict():
            return value.items()
    return None


def run_function(text: str) -> str:
    return shlex.split(text)[0].upper()


def run_class_method() -> str:
    return datetime.datetime.now().date().isoformat()


def run_inherited(flag: bool) -> bytes:
    return flag.bit_length().to_bytes(2, "big")


def run_reexported() -> str:
    return io.StringIO().getvalue().upper()


def run_property(d: datetime.datetime) -> int:
    return d.year.bit_length()


def run_field(path: str) -> int:
    return shutil.disk_usage(path).free.bit_length()


def run_type_argument(queue: collections.deque[str]) -> str:
    return queue.popleft().upper()


def run_index(data: bytes) -> int:
    return data[0].bit_length()
"#;

/// What stays unresolved: a result the stubs leave open (`Any`, a function's own type
/// variable, an overload of no one type), a library outside the standard library, an
/// enumeration whose `__new__` may give the value another class, and the subject past a class
/// pattern that failed.
const LIMITS: &str = r#"import json
import os.path
from enum import Enum

import vendor


class Planet(str, Enum):
    EARTH = "earth"

    def __new__(cls, value: str) -> "Planet":
        member = str.__new__(cls, value)
        member._value_ = len(value)
        return member


def any_result(text: str) -> object:
    return json.loads(text).get("x")  # unresolved


def own_type_variable(text: str) -> str:
    return os.path.basename(text).upper()  # unresolved


def joined_overloads(d: dict) -> object:
    return d.get("k").upper()  # unresolved


def outside_the_standard_library() -> object:
    return vendor.make().run()  # unresolved


def enum_with_new() -> str:
    return Planet.EARTH.value.upper()  # unresolved


def past_a_failed_pattern(value: object) -> object:
    match value:
        case dict():
            return None
        case _:
            return value.items()  # unresolved
"#;

/// The lines of `pkg/m.py` with a call the boundary reports heki cannot resolve.
fn unresolved_lines(source: &str, name: &str) -> Vec<u64> {
    let p = Project::new(name);
    p.write("pyproject.toml", CONFIG);
    p.write("pkg/__init__.py", "");
    p.write("pkg/m.py", source);
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let mut lines: Vec<u64> = diags
        .iter()
        .filter(|d| d["rule_code"] == "EFF201")
        .map(|d| {
            let message = d["message"].as_str().unwrap();
            assert!(message.contains("a call heki cannot resolve"), "{message}");
            d["line"].as_u64().unwrap()
        })
        .collect();
    lines.sort_unstable();
    lines
}

fn marked(source: &str) -> Vec<u64> {
    (source.lines().enumerate())
        .filter(|(_, line)| line.trim_end().ends_with("# unresolved"))
        .map(|(i, _)| i as u64 + 1)
        .collect()
}

#[test]
fn a_call_on_a_declared_result_resolves() {
    assert_eq!(
        unresolved_lines(SHAPES, "declared_results"),
        Vec::<u64>::new()
    );
}

#[test]
fn results_the_declarations_leave_open_stay_unresolved() {
    assert_eq!(unresolved_lines(LIMITS, "declared_limits"), marked(LIMITS));
}
