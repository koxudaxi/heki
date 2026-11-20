//! Reads and calls through a value typed with a project protocol, end to end through the real
//! binary: when the protocol declares the member, the read and call contracts judge the access
//! by the protocol, not by the classes implementing it, and what an implementation's private
//! method does inside stays with it. A write is one of the implementations either way.

mod common;

use common::Project;
use serde_json::Value;

const CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT201", "MUT203", "MUT204"]

[[tool.heki.read-owners]]
name = "private-reads"
targets = ["Concrete._*"]
forbidden-readers = ["pkg.use.*"]

[[tool.heki.call-owners]]
name = "private-calls"
targets = ["Concrete._*"]
forbidden-callers = ["pkg.use.*"]

[[tool.heki.write-owners]]
name = "private-writes"
targets = ["Concrete._m"]
allowed-writers = ["Concrete.*"]
"#;

const HOST: &str = r#"from typing import Protocol


class Host(Protocol):
    _m: int

    def _helper(self) -> int: ...


class Concrete:
    def __init__(self) -> None:
        self._m = 1
        self._n = 2

    def _helper(self) -> int:
        return self._m
"#;

const USE: &str = r#"from pkg.host import Concrete, Host


def through_protocol(h: Host) -> int:
    return h._m + h._helper()


def undeclared(h: Host) -> int:
    return h._n


def direct(c: Concrete) -> int:
    return c._m + c._helper()


def write_through_protocol(h: Host) -> None:
    h._m = 3
"#;

#[test]
fn accesses_through_a_protocol_declaring_the_member_are_judged_by_the_protocol() {
    let p = Project::new("protocol_access");
    p.write("pyproject.toml", CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/host.py", HOST)
        .write("pkg/use.py", USE);
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let found: Vec<(u64, u64, &str, &str)> = diags
        .iter()
        .map(|d| {
            (
                d["line"].as_u64().unwrap(),
                d["column"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap(),
                d["qualname"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        found,
        vec![
            // `Host` declares `_m` and `_helper`: reading and calling them through it is no
            // access of `Concrete`'s, nor is what `Concrete._helper` reads inside. `_n` is no
            // member of `Host`: the read is one of the class implementing it.
            (9, 12, "MUT203", "undeclared"),
            // Through the class itself, every access counts.
            (13, 12, "MUT203", "direct"),
            (13, 19, "MUT203", "direct"),
            (13, 19, "MUT204", "direct"),
            // A write through the protocol changes the implementation's attribute.
            (17, 5, "MUT201", "write_through_protocol"),
        ]
    );
}

const CACHED: &str = r#"from functools import cached_property
from typing import Protocol


class Host(Protocol):
    total: int


class Concrete:
    def __init__(self) -> None:
        self.value = 1

    @cached_property
    def total(self) -> int:
        return self.value * 2

    def bump(self) -> None:
        self.value += 1


def read(h: Host) -> int:
    return h.total
"#;

/// The read through the protocol is still one of the implementation for the rules about what
/// the read returns: `Concrete.bump` can leave the cached `total` stale (`MUT111`).
#[test]
fn a_read_through_a_protocol_still_reads_the_implementation() {
    let p = Project::new("protocol_cached_read");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nextend-select = [\"MUT111\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write("pkg/m.py", CACHED);
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let found: Vec<(u64, &str)> = diags
        .iter()
        .map(|d| {
            (
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(found, vec![(22, "MUT111")]);
}
