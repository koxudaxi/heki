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

const MIXED_CONFIG: &str = r#"[project]
name = "t"

[tool.heki.lint]
select = ["MUT203", "MUT204"]

[[tool.heki.read-owners]]
name = "m"
targets = ["Concrete._m"]
allowed-readers = ["pkg.use.allowed", "pkg.use.allowed_through_protocol"]

[[tool.heki.call-owners]]
name = "compute"
targets = ["Concrete._compute"]
allowed-callers = ["pkg.use.allowed", "pkg.use.allowed_through_protocol"]
"#;

const MIXED_HOST: &str = r#"from typing import Protocol


class Host(Protocol):
    def _helper(self) -> int: ...

    def _other(self) -> int: ...

    def _outer(self) -> int: ...


class Concrete:
    def __init__(self) -> None:
        self._m = 1

    def _compute(self) -> int:
        return 2

    def _helper(self) -> int:
        return self._m + self._compute()

    def _other(self) -> int:
        return self._m + self._compute()

    def _outer(self) -> int:
        return self._inner()

    def _inner(self) -> int:
        return self._m
"#;

const MIXED_USE: &str = r#"from pkg.host import Concrete, Host


def allowed(c: Concrete) -> int:
    return c._helper() + c._outer()


def through_protocol(h: Host) -> int:
    return h._helper() + h._outer()


def allowed_through_protocol(h: Host) -> int:
    return h._other()


def direct(c: Concrete) -> int:
    return c._other()
"#;

/// A private implementation method called through its class and through a protocol: the
/// callers through the class take its accesses over, and it keeps them for the protocol's.
#[test]
fn an_implementation_keeps_its_accesses_for_its_protocol_callers() {
    let p = Project::new("protocol_mixed_callers");
    p.write("pyproject.toml", MIXED_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/host.py", MIXED_HOST)
        .write("pkg/use.py", MIXED_USE);
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    let found: Vec<(&str, u64, u64, &str, &str)> = diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().rsplit('/').next().unwrap(),
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
            // `allowed` takes over what `_helper` does; `through_protocol` calls `Host._helper`
            // and leaves it with the implementation.
            ("host.py", 20, 16, "MUT203", "Concrete._helper"),
            ("host.py", 20, 26, "MUT204", "Concrete._helper"),
            // The same one call up: `Concrete._outer` keeps what `_inner` reads for
            // `through_protocol`, at its call of `_inner`.
            ("host.py", 26, 16, "MUT203", "Concrete._outer"),
            // The protocol caller of `_other` is allowed, its caller through the class is not.
            ("use.py", 17, 12, "MUT203", "direct"),
            ("use.py", 17, 12, "MUT204", "direct"),
        ]
    );
}

/// A glob target naming the protocol's members: a call through the protocol names the method
/// the protocol declares.
#[test]
fn a_glob_target_names_the_protocol_method_called() {
    let p = Project::new("protocol_glob_target");
    p.write(
        "pyproject.toml",
        &MIXED_CONFIG.replace(
            "targets = [\"Concrete._compute\"]",
            "targets = [\"Host._*\"]",
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/host.py", MIXED_HOST)
    .write("pkg/use.py", MIXED_USE);
    let (_, stdout, _) = p.check(&["--select", "MUT204", "--output-format", "concise"]);
    let lines: Vec<&str> = stdout.lines().filter(|l| l.contains("MUT204")).collect();
    assert_eq!(
        lines,
        vec![
            "pkg/use.py:9:12: MUT204 `through_protocol` calls `pkg.host.Host._helper` outside the call owners of `compute`",
            "pkg/use.py:9:26: MUT204 `through_protocol` calls `pkg.host.Host._outer` outside the call owners of `compute`",
        ],
        "{stdout}"
    );
}

/// An effect boundary forbidding calls of `Host._helper` judges `h._helper()` by the protocol's
/// declaration, and so does its `allowed-callees`: naming the declaration exempts the call.
#[test]
fn a_boundary_exempts_a_protocol_call_by_its_declaration() {
    let boundary = |allowed: &str| {
        format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"EFF201\"]\n\n[[tool.heki.effect-boundaries]]\nname = \"no-helpers\"\nscopes = [\"pkg.use.*\"]\nforbid = [\"calls\"]\ncall-targets = [\"Host._helper\"]\n{allowed}"
        )
    };
    let p = Project::new("protocol_boundary_allowed");
    p.write("pyproject.toml", &boundary(""))
        .write("pkg/__init__.py", "")
        .write("pkg/host.py", MIXED_HOST)
        .write("pkg/use.py", MIXED_USE);
    let calls = |p: &Project| -> Vec<String> {
        let (_, stdout, _) = p.check(&["--output-format", "concise"]);
        stdout
            .lines()
            .filter(|l| l.contains("EFF201"))
            .map(|l| l.split(": EFF201").next().unwrap_or(l).to_owned())
            .collect()
    };
    assert_eq!(calls(&p), vec!["pkg/use.py:9:12".to_owned()]);
    p.write(
        "pyproject.toml",
        &boundary("allowed-callees = [\"Host._helper\"]\n"),
    );
    assert!(calls(&p).is_empty());
}
