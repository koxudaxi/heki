//! Glob targets end to end through the real binary: the last segment of a call target names the
//! function, method, or class (a glob there matches within that name, as the attribute of a
//! `Class.attr` target does), and a report of a glob target names the member it matched.

mod common;

use common::Project;
use serde_json::Value;

const CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT201", "MUT203", "MUT204", "MUT109"]

[[tool.heki.read-owners]]
name = "host-private"
targets = ["pkg.*._*"]
forbidden-readers = ["pkg.parser._*"]

[[tool.heki.call-owners]]
name = "host-private-calls"
targets = ["pkg.*._*"]
forbidden-callers = ["pkg.parser._*"]

[[tool.heki.write-owners]]
name = "flags"
targets = ["DataType._*", "DataType.name", 'DataType.tags["k"]']
allowed-writers = ["DataType.*"]

[[tool.heki.invalidation]]
name = "revisions"
targets = ["DataType._*"]
invalidators = ["DataType.note"]
exempt-writers = ["DataType.*"]
"#;

const TYPES: &str = r#"from functools import cached_property


class DataType:
    def __init__(self) -> None:
        self._flag = True
        self._mark = 0
        self.name = ""
        self.tags: dict[str, int] = {}

    def _is_container(self) -> bool:
        return self._flag

    @property
    def _texts(self) -> str:
        return ""

    @cached_property
    def _cached(self) -> int:
        return 1

    def note(self) -> None:
        pass


class Other:
    def _pure(self) -> int:
        return 1
"#;

const CONCERN: &str = r#"from pkg.types import DataType, Other


def typed(data_type: DataType) -> bool:
    return data_type._flag


def method_call(data_type: DataType) -> bool:
    return data_type._is_container()


def prop(data_type: DataType) -> str:
    return data_type._texts


def pure_method(other: Other) -> int:
    return other._pure()


def _helper(data_type: DataType) -> bool:
    return data_type._is_container()


def writes(data_type: DataType) -> None:
    _both(data_type)
    data_type.name = "n"


def _both(data_type: DataType) -> None:
    data_type._flag = False
    data_type._mark = 1


def tag(data_type: DataType) -> None:
    data_type.tags["k"] = 1
"#;

fn project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("pyproject.toml", CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/parser/__init__.py", "")
        .write("pkg/types.py", TYPES)
        .write("pkg/parser/_concern.py", CONCERN);
    p
}

/// `(line, code, message)` of every diagnostic, in order.
fn reports(p: &Project) -> Vec<(u64, String, String)> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    diags
        .iter()
        .map(|d| {
            (
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
                d["message"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

#[test]
fn a_glob_call_target_names_members_and_reports_name_what_they_matched() {
    let p = project("glob_targets");
    let found = reports(&p);
    let calls: Vec<(u64, &str)> = found
        .iter()
        .filter(|r| r.1 == "MUT204")
        .map(|r| (r.0, r.2.as_str()))
        .collect();
    // `pkg.*._*` names the private methods and properties of the classes in `pkg` (and the
    // private helper `_both`), not every function of the private module `pkg.parser._concern`:
    // the forbidden callers are no targets, so their calls are reported.
    assert_eq!(
        calls,
        vec![
            (
                9,
                "`method_call` calls `pkg.types.DataType._is_container` inside the forbidden callers of `host-private-calls`"
            ),
            (
                13,
                "`prop` calls `pkg.types.DataType._texts` inside the forbidden callers of `host-private-calls`"
            ),
            (
                17,
                "`pure_method` calls `pkg.types.Other._pure` inside the forbidden callers of `host-private-calls`"
            ),
            (
                25,
                "`writes` calls `pkg.parser._concern._both` inside the forbidden callers of `host-private-calls`"
            ),
        ]
    );
    let message = |line: u64, code: &str| {
        found
            .iter()
            .find(|r| r.0 == line && r.1 == code)
            .unwrap_or_else(|| panic!("no {code} at {line}: {found:#?}"))
            .2
            .clone()
    };
    // Reads name the attribute of the class they read.
    assert_eq!(
        message(5, "MUT203"),
        "`typed` reads `pkg.types.DataType._flag` (direct_attr) inside the forbidden readers of `host-private`"
    );
    // One report of a glob target stands for every member a helper writes; it names the first
    // and lists the others. A literal target is named as configured.
    assert_eq!(
        message(25, "MUT201"),
        "`writes` writes `pkg.types.DataType._flag` (direct_attr) via `pkg.parser._concern._both` outside the write owners of `flags` (also `pkg.types.DataType._mark`)"
    );
    assert_eq!(
        message(25, "MUT109"),
        "`writes` writes `pkg.types.DataType._flag` (direct_attr) via `pkg.parser._concern._both` and returns on some path without calling an invalidator of `revisions` (also `pkg.types.DataType._mark`)"
    );
    assert_eq!(
        message(26, "MUT201"),
        "`writes` writes `DataType.name` (direct_attr) outside the write owners of `flags`"
    );
    // The key of an entry target is no glob.
    assert_eq!(
        message(35, "MUT201"),
        "`tag` writes `DataType.tags[\"k\"]` (container_mut) outside the write owners of `flags`"
    );
}
