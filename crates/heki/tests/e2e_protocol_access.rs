//! Reads and calls through a value typed with a project protocol, end to end through the real
//! binary: when the protocol declares the member, the read and call contracts judge the access
//! by the protocol, not by the classes implementing it, and what an implementation's private
//! method does inside stays with it. A write is one of the implementations either way. With
//! `allowed-protocols`, an access through a protocol the key names that declares the member is
//! allowed, also through a class object typed `type[P]` of it.

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
            "pkg/use.py:9:12: MUT204 [heuristic] `through_protocol` calls `pkg.host.Host._helper` outside the call owners of `compute`",
            "pkg/use.py:9:26: MUT204 [heuristic] `through_protocol` calls `pkg.host.Host._outer` outside the call owners of `compute`",
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

/// `(line, code, function, target)` of every report of `p`: the first two names a message quotes.
fn named(p: &Project) -> Vec<(u64, String, String, String)> {
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
    out
}

const FLAGS: &str = r#"from __future__ import annotations

from typing import Protocol


class RuntimeFlags(Protocol):
    _has_runtime_expressions: bool
    _items: list[int]

    def _reset(self) -> None: ...
"#;

const PARSER: &str = r#"from __future__ import annotations


class Parser:
    def __init__(self) -> None:
        self._has_runtime_expressions = False
        self._graph_builder: object = object()
        self._items: list[int] = []
        self._other = 1

    def _reset(self) -> None:
        self._items = []

    def _build(self) -> object:
        return self._graph_builder
"#;

const RENDER: &str = r#"from __future__ import annotations

from typing import Protocol

from pkg.parser._flags import RuntimeFlags
from pkg.parser.base import Parser


class RenderHost(RuntimeFlags, Protocol):
    _graph_builder: object

    def _build(self) -> object: ...


def assign(parser: RenderHost) -> object:
    return parser._graph_builder


def flagged(parser: RenderHost) -> bool:
    return parser._has_runtime_expressions


def mark(parser: RenderHost) -> None:
    parser._has_runtime_expressions = True
    parser._items.append(1)


def build(parser: RenderHost) -> object:
    parser._reset()
    return parser._build()


def _set(parser: RenderHost) -> None:
    parser._graph_builder = None


def run(parser: RenderHost) -> None:
    _set(parser)


def undeclared(parser: RenderHost) -> object:
    return parser._other


def concrete(parser: Parser) -> object:
    return parser._graph_builder


def bad_read(flags: RuntimeFlags) -> bool:
    return flags._has_runtime_expressions


def bad_write(flags: RuntimeFlags) -> None:
    flags._has_runtime_expressions = True
"#;

fn host_project(name: &str, keys: &str) -> Project {
    let contract = |section: &str, who: &str| {
        format!(
            "[[tool.heki.{section}]]\nname = \"{section}\"\ntargets = [\"pkg.parser.base.Parser._*\", \"pkg.parser._flags.RuntimeFlags._*\"]\nforbidden-{who} = [\"pkg.parser._render:*\"]\n{keys}\n"
        )
    };
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\", \"MUT203\", \"MUT204\"]\n\n{}{}{}",
            contract("read-owners", "readers"),
            contract("write-owners", "writers"),
            contract("call-owners", "callers"),
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/parser/__init__.py", "")
    .write("pkg/parser/_flags.py", FLAGS)
    .write("pkg/parser/base.py", PARSER)
    .write("pkg/parser/_render.py", RENDER);
    p
}

#[test]
fn allowed_protocols_allow_the_accesses_through_a_named_protocol_declaring_the_member() {
    let at = |line: u64, code: &str, func: &str, target: &str| {
        (line, code.to_owned(), func.to_owned(), target.to_owned())
    };
    // `RenderHost` matches `*Host` and declares `_graph_builder` and `_build`, and inherits
    // `_has_runtime_expressions`, `_items`, and `_reset` from `RuntimeFlags`: reading, writing
    // (also inside a container, and through a private helper), and calling them through it is
    // allowed. `_other` is no member of the protocol, `concrete` reads through the class, and
    // `RuntimeFlags` is not named. An access matching both targets of a contract is reported
    // once, under the target configured first.
    let kept = || {
        vec![
            at(42, "MUT203", "undeclared", "pkg.parser.base.Parser._other"),
            at(
                46,
                "MUT203",
                "concrete",
                "pkg.parser.base.Parser._graph_builder",
            ),
            at(
                50,
                "MUT203",
                "bad_read",
                "pkg.parser._flags.RuntimeFlags._has_runtime_expressions",
            ),
            at(
                54,
                "MUT201",
                "bad_write",
                "pkg.parser.base.Parser._has_runtime_expressions",
            ),
        ]
    };
    let lexical = host_project(
        "allowed_protocols_lexical",
        "attribution = \"lexical\"\nallowed-protocols = [\"*Host\"]\n",
    );
    assert_eq!(named(&lexical), kept());
    // By the call graph, `Parser._reset` hands its write of its own `self` to the function
    // calling it through the protocol: that object is no value typed with the protocol there.
    let call_graph = host_project(
        "allowed_protocols_call_graph",
        "allowed-protocols = [\"*Host\"]\n",
    );
    let mut expected = kept();
    expected.insert(
        0,
        at(29, "MUT201", "build", "pkg.parser.base.Parser._items"),
    );
    assert_eq!(named(&call_graph), expected);
    // Without the key, every access through the protocol is reported, once per access.
    let without = named(&host_project(
        "allowed_protocols_unset",
        "attribution = \"lexical\"\n",
    ));
    assert_eq!(without.len(), 12, "{without:#?}");
    assert!(
        without.iter().any(
            |(line, code, func, _)| (*line, code.as_str(), func.as_str()) == (34, "MUT201", "_set")
        ),
        "{without:#?}"
    );
}

#[test]
fn allowed_protocols_name_classes_by_pattern_and_a_bad_glob_is_a_configuration_error() {
    // A pattern naming only the base protocol names its subprotocol too, as a class pattern
    // names its subclasses: `RenderHost` derives from `RuntimeFlags`.
    let base = host_project(
        "allowed_protocols_base",
        "attribution = \"lexical\"\nallowed-protocols = [\"RuntimeFlags\"]\n",
    );
    let found = named(&base);
    assert_eq!(
        found
            .iter()
            .map(|(_, _, func, _)| func.as_str())
            .collect::<Vec<_>>(),
        vec!["undeclared", "concrete"],
        "{found:#?}"
    );
    // With `protocol-subclasses = false` the pattern names `RuntimeFlags` alone: the accesses
    // through `RuntimeFlags` are allowed, those through `RenderHost` judged.
    let exact = host_project(
        "allowed_protocols_base_exact",
        "attribution = \"lexical\"\nallowed-protocols = [\"RuntimeFlags\"]\nprotocol-subclasses = false\n",
    );
    let found = named(&exact);
    assert_eq!(
        found
            .iter()
            .map(|(_, _, func, _)| func.as_str())
            .collect::<Vec<_>>(),
        vec![
            "assign",
            "flagged",
            "mark",
            "mark",
            "mark",
            "build",
            "build",
            "_set",
            "undeclared",
            "concrete"
        ],
        "{found:#?}"
    );
    let bad = host_project(
        "allowed_protocols_bad_glob",
        "allowed-protocols = [\"[Host\"]\n",
    );
    let (code, stdout, stderr) = bad.check(&[]);
    assert_eq!(code, 2, "{stdout}{stderr}");
    assert!(stderr.contains("allowed-protocols"), "{stderr}");
}

/// Each contract kind reads its own `allowed-protocols`, so a bad pattern in any one of them is a
/// configuration error naming that contract's key.
#[test]
fn a_bad_allowed_protocols_pattern_is_a_configuration_error_in_each_contract_kind() {
    for (section, who, rule) in [
        ("read-owners", "readers", "MUT203"),
        ("write-owners", "writers", "MUT201"),
        ("call-owners", "callers", "MUT204"),
    ] {
        let p = Project::new(&format!("allowed_protocols_bad_glob_{rule}"));
        p.write(
            "pyproject.toml",
            &format!(
                "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"{rule}\"]\n\n[[tool.heki.{section}]]\nname = \"c\"\ntargets = [\"pkg.parser.base.Parser._*\"]\nforbidden-{who} = [\"pkg.parser._render:*\"]\nallowed-protocols = [\"[Host\"]\n"
            ),
        )
        .write("pkg/__init__.py", "")
        .write("pkg/parser/__init__.py", "")
        .write("pkg/parser/_flags.py", FLAGS)
        .write("pkg/parser/base.py", PARSER)
        .write("pkg/parser/_render.py", RENDER);
        let (code, stdout, stderr) = p.check(&[]);
        assert_eq!(code, 2, "{section}: {stdout}{stderr}");
        assert!(
            stderr.contains(&format!("{section}.allowed-protocols")),
            "{section}: {stderr}"
        );
    }
}

#[test]
fn allowed_protocols_leave_the_accesses_through_a_protocol_the_key_does_not_name() {
    let p = Project::new("allowed_protocols_repro");
    let contract = |section: &str, who: &str| {
        format!(
            "[[tool.heki.{section}]]\nname = \"host-private\"\ntargets = [\"pkg.parser.base.Parser._*\", \"pkg.parser._flags.RuntimeFlags._*\"]\nforbidden-{who} = [\"pkg.parser._render:*\"]\nattribution = \"lexical\"\nallowed-protocols = [\"*Host\"]\n\n"
        )
    };
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\", \"MUT203\"]\n\n{}{}",
            contract("read-owners", "readers"),
            contract("write-owners", "writers"),
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/parser/__init__.py", "")
    .write(
        "pkg/parser/_flags.py",
        "from __future__ import annotations\n\nfrom typing import Protocol\n\n\nclass RuntimeFlags(Protocol):\n    _has_runtime_expressions: bool\n",
    )
    .write(
        "pkg/parser/base.py",
        "from __future__ import annotations\n\n\nclass Parser:\n    def __init__(self) -> None:\n        self._has_runtime_expressions = False\n        self._graph_builder: object = object()\n",
    )
    .write(
        "pkg/parser/_render.py",
        r#"from __future__ import annotations

from typing import Protocol

from pkg.parser._flags import RuntimeFlags


class RenderHost(RuntimeFlags, Protocol):
    _graph_builder: object


def assign(parser: RenderHost) -> object:
    return parser._graph_builder


def flagged(parser: RenderHost) -> bool:
    return parser._has_runtime_expressions


def mark(parser: RenderHost) -> None:
    parser._has_runtime_expressions = True


def bad_read(flags: RuntimeFlags) -> bool:
    return flags._has_runtime_expressions


def bad_write(flags: RuntimeFlags) -> None:
    flags._has_runtime_expressions = True
"#,
    );
    let at = |line: u64, code: &str, func: &str, target: &str| {
        (line, code.to_owned(), func.to_owned(), target.to_owned())
    };
    // The write through `RuntimeFlags` is one of the protocol's member and of the attribute of
    // `Parser`, which implements it: two targets of the contract, one report under the target
    // configured first, naming the other.
    assert_eq!(
        named(&p),
        vec![
            at(
                25,
                "MUT203",
                "bad_read",
                "pkg.parser._flags.RuntimeFlags._has_runtime_expressions"
            ),
            at(
                29,
                "MUT201",
                "bad_write",
                "pkg.parser.base.Parser._has_runtime_expressions"
            ),
        ]
    );
    let (_, stdout, _) = p.check(&["--output-format", "concise"]);
    assert!(
        stdout.contains(
            "`bad_write` writes `pkg.parser.base.Parser._has_runtime_expressions` (by attribute) inside the forbidden writers of `host-private` (also `pkg.parser._flags.RuntimeFlags._has_runtime_expressions`)"
        ),
        "{stdout}"
    );
}

/// A reference handing on a method through a protocol-typed value (`references`) keeps no
/// declaration of the protocol: `allowed-protocols` allows it by the receiver's protocol and the
/// method it names.
#[test]
fn allowed_protocols_allow_the_references_through_a_named_protocol() {
    let run = |keys: &str| {
        let p = Project::new(&format!("allowed_protocols_references_{}", keys.len()));
        p.write(
            "pyproject.toml",
            &format!(
                "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT204\"]\n\n[[tool.heki.call-owners]]\nname = \"c\"\ntargets = [\"pkg.host.Concrete._*\"]\nforbidden-callers = [\"pkg.use:*\"]\nreferences = \"reads\"\n{keys}"
            ),
        )
        .write("pkg/__init__.py", "")
        .write(
            "pkg/host.py",
            "from typing import Protocol\n\n\nclass Host(Protocol):\n    def _helper(self) -> int: ...\n\n\nclass Port(Protocol):\n    def _helper(self) -> int: ...\n\n\nclass Concrete:\n    def _helper(self) -> int:\n        return 1\n",
        )
        .write(
            "pkg/use.py",
            "from pkg.host import Host, Port\n\n\ndef hand(h: Host) -> object:\n    return h._helper\n\n\ndef other(p: Port) -> object:\n    return p._helper\n",
        );
        named(&p)
    };
    let at = |line: u64, func: &str| {
        (
            line,
            "MUT204".to_owned(),
            func.to_owned(),
            "pkg.host.Concrete._helper".to_owned(),
        )
    };
    assert_eq!(run(""), vec![at(5, "hand"), at(9, "other")]);
    assert_eq!(
        run("allowed-protocols = [\"Host\"]\n"),
        vec![at(9, "other")]
    );
}

const SUBPROTOCOL_PARSER: &str = r#"from __future__ import annotations


class Parser:
    def __init__(self) -> None:
        self._has_runtime_expressions = False
        self._graph_builder: object = object()

    def _build(self) -> object:
        return self._graph_builder
"#;

const SUBPROTOCOL_SHARED: &str = r#"from __future__ import annotations

from typing import Protocol


class GraphBuilderHost(Protocol):
    _graph_builder: object

    def _build(self) -> object: ...


class RuntimeExpressionFlags(GraphBuilderHost, Protocol):
    _has_runtime_expressions: bool


class RenderHost(RuntimeExpressionFlags, Protocol):
    pass


def builder(host: GraphBuilderHost) -> object:
    return host._graph_builder


def build(host: GraphBuilderHost) -> object:
    return host._build()


def builder_through_flags(host: RuntimeExpressionFlags) -> object:
    return host._graph_builder


def build_through_flags(host: RuntimeExpressionFlags) -> object:
    return host._build()


def register_runtime_expression(host: RuntimeExpressionFlags) -> None:
    host._has_runtime_expressions = True


def runtime_expressions(host: RuntimeExpressionFlags) -> bool:
    return host._has_runtime_expressions


def render_flag(host: RenderHost) -> bool:
    return host._has_runtime_expressions
"#;

/// A project of the host rule's shape: the private members of every class of `pkg.parser` are
/// targets inside `pkg.parser._shared`, and the accesses through `*Host` protocols are allowed,
/// with `keys` added to each contract.
fn subprotocol_project(name: &str, keys: &str) -> Project {
    let contract = |section: &str, who: &str| {
        format!(
            "[[tool.heki.{section}]]\nname = \"{section}\"\ntargets = [\"pkg.parser.*._*\"]\nforbidden-{who} = [\"pkg.parser._shared:*\"]\nattribution = \"lexical\"\nallowed-protocols = [\"*Host\"]\n{keys}\n"
        )
    };
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\", \"MUT203\", \"MUT204\"]\n\n{}{}{}",
            contract("read-owners", "readers"),
            contract("write-owners", "writers"),
            contract("call-owners", "callers"),
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/parser/__init__.py", "")
    .write("pkg/parser/base.py", SUBPROTOCOL_PARSER)
    .write("pkg/parser/_shared.py", SUBPROTOCOL_SHARED);
    p
}

/// An `allowed-protocols` pattern names the subprotocols of the protocols it matches, unless
/// `protocol-subclasses = false`: then the accesses through `RuntimeExpressionFlags`, a protocol
/// deriving from `GraphBuilderHost`, stay judged, also of the members it inherits from it, while
/// those through `GraphBuilderHost` and `RenderHost` (a subprotocol whose own name matches) stay
/// allowed. Each contract kind reads the key.
#[test]
fn protocol_subclasses_false_names_the_matching_protocols_only() {
    let at = |line: u64, code: &str, func: &str, target: &str| {
        (line, code.to_owned(), func.to_owned(), target.to_owned())
    };
    let flags = "pkg.parser._shared.RuntimeExpressionFlags";
    assert_eq!(
        named(&subprotocol_project("protocol_subclasses_unset", "")),
        vec![]
    );
    assert_eq!(
        named(&subprotocol_project(
            "protocol_subclasses_true",
            "protocol-subclasses = true\n"
        )),
        vec![]
    );
    assert_eq!(
        named(&subprotocol_project(
            "protocol_subclasses_false",
            "protocol-subclasses = false\n"
        )),
        vec![
            at(
                29,
                "MUT203",
                "builder_through_flags",
                &format!("{flags}._graph_builder")
            ),
            at(
                33,
                "MUT204",
                "build_through_flags",
                "pkg.parser._shared.GraphBuilderHost._build"
            ),
            at(
                37,
                "MUT201",
                "register_runtime_expression",
                &format!("{flags}._has_runtime_expressions")
            ),
            at(
                41,
                "MUT203",
                "runtime_expressions",
                &format!("{flags}._has_runtime_expressions")
            ),
        ]
    );
    let p = subprotocol_project("protocol_subclasses_bad", "protocol-subclasses = \"no\"\n");
    let (code, stdout, stderr) = p.check(&[]);
    assert_eq!(code, 2, "{stdout}{stderr}");
    assert!(stderr.contains("protocol-subclasses"), "{stderr}");
}

const KEYWORDS_BASE: &str = r#"class Parser:
    _kind: str = ""

    @classmethod
    def _items_constraints(cls, obj: dict) -> dict:
        return obj

    @classmethod
    def _other(cls, obj: dict) -> dict:
        return obj

    def _instance_helper(self, obj: dict) -> dict:
        return obj
"#;

const KEYWORDS: &str = r#"from typing import Optional, Protocol

from pkg.parser.base import Parser


class KeywordsHost(Protocol):
    _kind: str

    @classmethod
    def _items_constraints(cls, obj: dict) -> dict: ...

    def _instance_helper(self, obj: dict) -> dict: ...


class OtherShape(Protocol):
    @classmethod
    def _items_constraints(cls, obj: dict) -> dict: ...


def through_class_object(cls: type[KeywordsHost], obj: dict) -> dict:
    return cls._items_constraints(obj)


def through_optional(cls: Optional[type[KeywordsHost]], obj: dict) -> dict:
    assert cls is not None
    return cls._items_constraints(obj)


def through_instance(host: KeywordsHost, obj: dict) -> dict:
    return host._instance_helper(obj)


def through_the_class(obj: dict) -> dict:
    return Parser._items_constraints(obj)


def protocol_not_allowed(cls: type[OtherShape], obj: dict) -> dict:
    return cls._items_constraints(obj)


def member_not_declared(cls: type[KeywordsHost], obj: dict) -> dict:
    return cls._other(obj)


def through_local(make, obj: dict) -> dict:
    cls: type[KeywordsHost] = make()
    return cls._items_constraints(obj)


class Holder:
    host: type[KeywordsHost]

    def through_attribute(self, obj: dict) -> dict:
        return self.host._items_constraints(obj)


def reads_through_class_object(cls: type[KeywordsHost]) -> str:
    return cls._kind


def writes_through_class_object(cls: type[KeywordsHost]) -> None:
    cls._kind = "x"


from pkg.parser.hosts import Schemas, SchemaT


def through_a_bound_class_object(cls: type[SchemaT], obj: dict) -> dict:
    return cls._items_constraints(obj)


def through_an_aliased_class_object(cls: type[Schemas], obj: dict) -> dict:
    return cls._items_constraints(obj)
"#;

/// A protocol of another module, which `KEYWORDS` names through a bound type variable and a type
/// alias.
const HOSTS: &str = r#"from typing import Protocol, TypeVar


class SchemaHost(Protocol):
    @classmethod
    def _items_constraints(cls, obj: dict) -> dict: ...


SchemaT = TypeVar("SchemaT", bound=SchemaHost)
type Schemas = SchemaHost
"#;

/// The three owner contracts on `Parser._*`, strict, judging the code where it is written, with
/// `allowed-protocols` set to `allowed`.
fn class_object_project(name: &str, allowed: &str) -> Project {
    let contract = |section: &str, who: &str| {
        format!(
            "[[tool.heki.{section}]]\nname = \"{section}\"\ntargets = [\"pkg.parser.base.Parser._*\"]\nforbidden-{who} = [\"pkg.parser._keywords:*\"]\nattribution = \"lexical\"\nallowed-protocols = {allowed}\nprotocol-subclasses = false\nunresolved = \"report\"\n\n"
        )
    };
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n[tool.heki.lint]\nselect = [\"MUT201\", \"MUT203\", \"MUT204\", \"MUT205\", \"MUT206\", \"MUT207\"]\n\n{}{}{}",
            contract("call-owners", "callers"),
            contract("read-owners", "readers"),
            contract("write-owners", "writers"),
        ),
    )
    .write("pkg/__init__.py", "")
    .write("pkg/parser/__init__.py", "")
    .write("pkg/parser/base.py", KEYWORDS_BASE)
    .write("pkg/parser/_keywords.py", KEYWORDS)
    .write("pkg/parser/hosts.py", HOSTS);
    p
}

/// A class object typed `type[P]` of a protocol is of a class heki cannot tell (any
/// implementation's), so its accesses are the strict rules' (`MUT205`, `MUT206`, `MUT207`); with
/// `allowed-protocols` naming the protocol, those of a member it declares are allowed as through a
/// value typed `P`: a parameter (also `Optional`), a local, or an attribute annotated so, also
/// with a type variable bound to the protocol or an alias of it in another module. The
/// call through the class itself, through a protocol the key does not name, and of a member the
/// protocol does not declare stay reported.
#[test]
fn allowed_protocols_allow_the_accesses_through_a_class_object_of_a_named_protocol() {
    let at = |line: u64, code: &str, func: &str, target: &str| {
        (line, code.to_owned(), func.to_owned(), target.to_owned())
    };
    let kept = || {
        vec![
            at(
                34,
                "MUT204",
                "through_the_class",
                "pkg.parser.base.Parser._items_constraints",
            ),
            at(38, "MUT207", "protocol_not_allowed", "_items_constraints"),
            at(42, "MUT207", "member_not_declared", "_other"),
        ]
    };
    let allowed = class_object_project("class_object_allowed", "[\"*Host\"]");
    assert_eq!(named(&allowed), kept());
    let mut without = kept();
    without.extend([
        at(21, "MUT207", "through_class_object", "_items_constraints"),
        at(26, "MUT207", "through_optional", "_items_constraints"),
        at(47, "MUT207", "through_local", "_items_constraints"),
        at(
            54,
            "MUT207",
            "Holder.through_attribute",
            "_items_constraints",
        ),
        at(58, "MUT206", "reads_through_class_object", "_kind"),
        at(62, "MUT205", "writes_through_class_object", "_kind"),
        at(
            69,
            "MUT207",
            "through_a_bound_class_object",
            "_items_constraints",
        ),
        at(
            73,
            "MUT207",
            "through_an_aliased_class_object",
            "_items_constraints",
        ),
    ]);
    without.sort();
    assert_eq!(
        named(&class_object_project("class_object_unset", "[]")),
        without
    );
}

/// The shape datamodel-code-generator reported: a module function taking `cls:
/// type[KeywordsHost]` calls the class method the protocol declares, which is allowed as the call
/// through an instance typed `KeywordsHost` is; the call through the class stays reported.
#[test]
fn a_class_object_of_an_allowed_protocol_calls_its_class_method() {
    let p = Project::new("class_object_repro");
    p.write(
        "pyproject.toml",
        r#"[project]
name = "h1"
version = "0"
requires-python = ">=3.11"

[tool.heki.lint]
select = ["MUT203", "MUT204", "MUT206", "MUT207"]

[[tool.heki.call-owners]]
name = "host-private-call"
targets = ["pkg.parser.base.Parser._*"]
forbidden-callers = ["pkg.parser._keywords:*"]
attribution = "lexical"
own-receiver = "allow"
allowed-protocols = ["*Host"]
protocol-subclasses = false
unresolved = "report"
"#,
    )
    .write("pkg/__init__.py", "")
    .write("pkg/parser/__init__.py", "")
    .write(
        "pkg/parser/base.py",
        "class Parser:\n    @classmethod\n    def _items_constraints(cls, obj: dict) -> dict:\n        return obj\n\n    def _instance_helper(self, obj: dict) -> dict:\n        return obj\n",
    )
    .write(
        "pkg/parser/_keywords.py",
        r#"from typing import Protocol

from pkg.parser.base import Parser


class KeywordsHost(Protocol):
    @classmethod
    def _items_constraints(cls, obj: dict) -> dict: ...

    def _instance_helper(self, obj: dict) -> dict: ...


def through_class_object(cls: type[KeywordsHost], obj: dict) -> dict:
    return cls._items_constraints(obj)


def through_instance(host: KeywordsHost, obj: dict) -> dict:
    return host._instance_helper(obj)


def through_the_class(obj: dict) -> dict:
    return Parser._items_constraints(obj)
"#,
    );
    assert_eq!(
        named(&p),
        vec![(
            22,
            "MUT204".to_owned(),
            "through_the_class".to_owned(),
            "pkg.parser.base.Parser._items_constraints".to_owned()
        )]
    );
}

/// A parameter typed by a `TypeVar` without a bound, imported from another module, may hold
/// any class, and no protocol: its accesses stay the strict rules' whatever protocols the
/// contract allows.
#[test]
fn a_type_variable_without_a_bound_names_no_protocol() {
    let p = class_object_project("class_object_type_var", "[\"*Host\"]");
    p.write(
        "pkg/parser/types.py",
        "from typing import TypeVar\n\nT = TypeVar(\"T\")\n",
    )
    .write(
        "pkg/parser/_keywords.py",
        "from pkg.parser.types import T\n\n\ndef through_type_var(cls: T, obj: dict) -> dict:\n    return cls._items_constraints(obj)\n",
    );
    assert_eq!(
        named(&p),
        vec![(
            5,
            "MUT207".to_owned(),
            "through_type_var".to_owned(),
            "_items_constraints".to_owned()
        )]
    );
}
