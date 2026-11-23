//! Contract extensions end to end through the real binary: `MUT204` call ownership (receiver
//! classes, overrides, bound-method aliases, `getattr`, constructions, private delegation, the
//! allowed-context rule), the `EFF201` `reads-targets` / `calls` / `allow-writes` boundaries and
//! a class's own initialization in them, `forbidden-*` scopes of the owner contracts, per-site
//! Ratchet counts, configuration errors, and `heki rule MUT204`.

mod common;

use common::Project;
use serde_json::Value;

fn diagnostics(p: &Project, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["--output-format", "json"];
    all.extend_from_slice(args);
    let (code, stdout, stderr) = p.check(&all);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

/// `(path, line, qualname, target)` of every diagnostic.
fn found(diags: &[Value]) -> Vec<(String, u64, String, String)> {
    diags
        .iter()
        .map(|d| {
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

/// `sites` of each diagnostic (written only above 1).
fn sites(diags: &[Value]) -> Vec<u64> {
    diags
        .iter()
        .map(|x| x["sites"].as_u64().unwrap_or(1))
        .collect()
}

fn related(diag: &Value) -> Vec<&str> {
    diag["related"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["message"].as_str().unwrap())
        .collect()
}

fn by_qualname<'d>(diags: &'d [Value], qualname: &str) -> &'d Value {
    diags
        .iter()
        .find(|x| x["qualname"] == qualname)
        .unwrap_or_else(|| panic!("no diagnostic of `{qualname}`: {diags:#?}"))
}

// ======================================================================================= MUT204

const MUT204_CONFIG: &str = r#"[project]
name = "t"

[[tool.heki.call-owners]]
name = "reference-mutators"
targets = ["DataType.replace_reference", "FinalFacts", "Session", "pkg.util.reset_cache"]
allowed-callers = ["pkg.store.*"]
exempt-methods = ["__deepcopy__"]
message = "use a GenerationStore operation instead"
"#;

const MUT204_UTIL: &str = "def reset_cache() -> None:\n    pass\n";

const MUT204_MODEL: &str = r#"from dataclasses import dataclass


class Reference:
    pass


class Base:
    def replace_reference(self, ref) -> None:
        pass


class DataType(Base):
    def __init__(self, ref: Reference | None = None) -> None:
        self.reference = ref
        if ref is not None:
            self.replace_reference(ref)

    def replace_reference(self, ref) -> None:
        self.reference = ref

    def remove_reference(self) -> None:
        self.replace_reference(None)

    def __deepcopy__(self, memo):
        new = DataType()
        new.replace_reference(self.reference)
        return new


class EmptyDataType(DataType):
    def replace_reference(self, ref) -> None:
        super().replace_reference(None)


@dataclass
class FinalFacts:
    names: tuple[str, ...]

    @classmethod
    def empty(cls) -> "FinalFacts":
        return cls(names=())

    @classmethod
    def fresh(cls) -> "FinalFacts":
        return cls(names=())


class Session:
    def __init__(self) -> None:
        self.open = True
"#;

const MUT204_STORE: &str = r#"from pkg.app import mixed, only_from_store
from pkg.model import DataType, FinalFacts, Reference
from pkg.util import reset_cache


class GenerationStore:
    def replace_data_type_ref(self, data_type: DataType, ref: Reference) -> None:
        data_type.replace_reference(ref)
        reset_cache()
        only_from_store(data_type)
        mixed(data_type)

    def seal(self) -> FinalFacts:
        return FinalFacts.empty()
"#;

const MUT204_APP: &str = r#"from pkg import model
from pkg.model import Base, DataType, EmptyDataType, FinalFacts, Reference
from pkg.util import reset_cache


def swap(data_type: DataType, ref: Reference) -> None:
    data_type.replace_reference(ref)


def swap_empty(data_type: EmptyDataType, ref: Reference) -> None:
    data_type.replace_reference(ref)


def swap_base(node: Base, ref: Reference) -> None:
    node.replace_reference(ref)


def rename(text: str) -> str:
    return text.replace("a", "b")


def alias(data_type: DataType, ref: Reference) -> None:
    replace = data_type.replace_reference
    replace(ref)


def dynamic(data_type: DataType, ref: Reference, name: str) -> None:
    getattr(data_type, "replace_reference")(ref)
    getattr(data_type, "replace_reference", None)(ref)
    getattr(data_type, name)(ref)


def build() -> FinalFacts:
    return FinalFacts(names=("a",))


def build_via_module() -> model.FinalFacts:
    return model.FinalFacts(names=())


def _helper(data_type: DataType, ref: Reference) -> None:
    data_type.replace_reference(ref)
    data_type.replace_reference(None)


def public(data_type: DataType, ref: Reference) -> None:
    _helper(data_type, ref)


def untyped(data_type, ref):
    data_type.replace_reference(ref)


def clear() -> None:
    reset_cache()


def only_from_store(data_type: DataType) -> None:
    data_type.replace_reference(None)


def mixed(data_type: DataType) -> None:
    data_type.replace_reference(None)


def outside(data_type: DataType) -> None:
    mixed(data_type)


def ping(data_type: DataType, n: int) -> None:
    data_type.replace_reference(None)
    if n:
        pong(data_type, n - 1)


def pong(data_type: DataType, n: int) -> None:
    ping(data_type, n)


def connect() -> model.Session:
    return model.Session()
"#;

fn mut204_project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("pyproject.toml", MUT204_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/util.py", MUT204_UTIL)
        .write("pkg/model.py", MUT204_MODEL)
        .write("pkg/store.py", MUT204_STORE)
        .write("pkg/app.py", MUT204_APP);
    p
}

#[test]
fn mut204_calls_constructions_and_delegation() {
    let p = mut204_project("ext_mut204");
    let diags = diagnostics(&p, &["--select", "MUT204"]);
    let (a, m) = ("pkg/app.py", "pkg/model.py");
    let (t, facts) = ("DataType.replace_reference", "FinalFacts");
    // Not reported: the allowed callers (`pkg.store.*`); `swap_base`, whose receiver class
    // `Base` may run `Base.replace_reference`, which is no target; `str.replace` (`rename`); a
    // computed `getattr` name; an unannotated receiver (`untyped`); `only_from_store`, which only
    // the store calls; `DataType.__init__` on its own `self`; the exempt `__deepcopy__`; an
    // override calling the base implementation (targets may call each other); and
    // `FinalFacts.empty`, which only the store calls.
    assert_eq!(
        found(&diags),
        vec![
            d(a, 7, "swap", t),
            // An override in a subclass of the target's class.
            d(a, 11, "swap_empty", t),
            // A bound-method alias, `getattr` with a constant name (with and without a default).
            d(a, 24, "alias", t),
            d(a, 28, "dynamic", t),
            d(a, 29, "dynamic", t),
            // Constructions: a dataclass without a project `__init__`, through its module.
            d(a, 34, "build", facts),
            d(a, 38, "build_via_module", facts),
            // Two calls in a private helper: one report at its caller, two sites.
            d(a, 47, "public", t),
            // A module-level function target.
            d(a, 55, "clear", "pkg.util.reset_cache"),
            // Called from the store and from `outside`: not only inside allowed callers.
            d(a, 63, "mixed", t),
            // A cycle without an allowed caller.
            d(a, 71, "ping", t),
            // A class with an `__init__`, constructed through its module.
            d(a, 81, "connect", "Session"),
            // A method without callers.
            d(m, 23, "DataType.remove_reference", t),
            // `cls(...)` in a class method without callers.
            d(m, 46, "FinalFacts.fresh", facts),
        ],
        "{diags:#?}"
    );
    assert_eq!(sites(&diags), [1, 1, 1, 1, 1, 1, 1, 2, 1, 1, 1, 1, 1, 1]);
    // The contract's `message` is the help of each report, not part of its message.
    for d in &diags {
        assert_eq!(
            d["help"], "use a GenerationStore operation instead",
            "{d:#}"
        );
    }
    assert_eq!(
        diags[0]["message"],
        "`swap` calls `DataType.replace_reference` outside the call owners of `reference-mutators`"
    );
    assert_eq!(
        by_qualname(&diags, "build")["message"],
        "`build` constructs `FinalFacts` outside the call owners of `reference-mutators`"
    );
    let public = by_qualname(&diags, "public");
    assert_eq!(
        public["message"],
        "`public` calls `DataType.replace_reference` via `pkg.app._helper` outside the call owners of `reference-mutators`"
    );
    // The chain ends at the first call of the target inside the helper.
    assert_eq!(
        related(public),
        [
            "`public` calls `pkg.app._helper`",
            "`_helper` calls `pkg.model.DataType.replace_reference`"
        ]
    );
    assert_eq!(public["related"][1]["line"], 42);
    assert!(diags.iter().all(|x| x["fix"].is_null()));
}

#[test]
fn mut204_reports_nothing_without_a_contract() {
    let p = mut204_project("ext_mut204_silent");
    p.write("pyproject.toml", "[project]\nname = \"t\"\n");
    let (code, stdout, stderr) = p.check(&["--select", "MUT204", "--output-format", "json"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert_eq!(stdout.trim(), "[]");
    // Enabled by default: the configured contract reports without `--select`.
    p.write("pyproject.toml", MUT204_CONFIG);
    let diags = diagnostics(&p, &[]);
    assert!(
        diags.iter().any(|x| x["rule_code"] == "MUT204"),
        "{diags:#?}"
    );
}

// ======================================================================================= EFF201

const EFF201_CONFIG: &str = r#"[project]
name = "t"

[[tool.heki.effect-boundaries]]
name = "decisions"
scopes = ["pkg.decisions"]
forbid = ["reads-targets", "calls"]
read-targets = ["DataType.is_optional", "Model.class_name"]
call-targets = ["FinalFacts", "DataType.replace_reference"]
allowed-callees = ["pkg.views.render_name"]
message = "decisions read final facts only"

[[tool.heki.effect-boundaries]]
name = "pass-sort"
scopes = ["pkg.passes.sort_bases"]
allow-writes = ["Model.base_classes"]
write-universe = ["Model.*", "DataType.*"]

[[tool.heki.effect-boundaries]]
name = "pass-check"
scopes = ["pkg.passes.check"]
write-universe = ["Model.*"]

[[tool.heki.effect-boundaries]]
name = "pass-both"
scopes = ["pkg.passes.both"]
forbid = ["writes-targets"]
write-targets = ["Model.fields"]
allow-writes = ["Model.fields"]
write-universe = ["DataType.is_optional"]
"#;

const EFF201_MODEL: &str = r#"from dataclasses import dataclass


class DataType:
    def __init__(self) -> None:
        self.is_optional = False
        self.reference = None

    def replace_reference(self, ref) -> None:
        self.reference = ref

    @property
    def nullable(self) -> bool:
        return self.is_optional


class Model:
    def __init__(self) -> None:
        self.class_name = ""
        self.base_classes: list[str] = []
        self.fields: list[str] = []
        self.name = ""


class SubModel(Model):
    pass


@dataclass
class FinalFacts:
    names: tuple[str, ...]
"#;

const EFF201_HELPERS: &str = r#"from pkg.model import DataType


def nullable(t: DataType) -> bool:
    return _nullable(t)


def _nullable(t: DataType) -> bool:
    return t.is_optional
"#;

const EFF201_VIEWS: &str = r#"from pkg.model import Model


def render_name(m: Model) -> str:
    return m.class_name
"#;

const EFF201_DECISIONS: &str = r#"from pkg.helpers import nullable
from pkg.model import DataType, FinalFacts, Model, SubModel
from pkg.views import render_name


def decide(t: DataType) -> bool:
    return nullable(t)


def decide_twice(t: DataType) -> bool:
    return t.is_optional or t.is_optional


def decide_ok(t: DataType) -> bool:
    return t.reference is None


def decide_property(t: DataType) -> bool:
    return t.nullable


def decide_getattr(m: Model) -> str:
    return getattr(m, "class_name")


def decide_sub(m: SubModel) -> str:
    return m.class_name


def decide_allowed(m: Model) -> str:
    return render_name(m)


def seal(t: DataType) -> FinalFacts:
    t.replace_reference(None)
    return FinalFacts(names=())


def text(s: str) -> str:
    return s.replace("a", "b")
"#;

const EFF201_PASSES: &str = r#"from pkg.model import DataType, Model


def _touch(m: Model) -> None:
    m.fields.append("x")


def sort_bases(m: Model, t: DataType) -> None:
    m.base_classes.sort()
    _touch(m)
    t.is_optional = True
    t.is_optional = False


def check(m: Model, t: DataType) -> bool:
    t.is_optional = True
    fresh = Model()
    return bool(m.fields) or bool(fresh.fields)


def both(m: Model, t: DataType) -> None:
    m.fields.append("x")
    t.is_optional = True
"#;

#[test]
fn eff201_reads_calls_and_allowed_writes() {
    let p = Project::new("ext_eff201");
    p.write("pyproject.toml", EFF201_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", EFF201_MODEL)
        .write("pkg/helpers.py", EFF201_HELPERS)
        .write("pkg/views.py", EFF201_VIEWS)
        .write("pkg/decisions.py", EFF201_DECISIONS)
        .write("pkg/passes.py", EFF201_PASSES);
    let diags = diagnostics(&p, &["--select", "EFF201"]);
    let (dec, pas) = ("pkg/decisions.py", "pkg/passes.py");
    // Not reported: a read of another attribute (`decide_ok`), a read inside the allowed callee
    // `render_name`, `str.replace`, `DataType.__init__` reading and writing its own `self`; in
    // `pass-sort` the allowed `Model.base_classes`; in `pass-check` a write outside the universe
    // (`DataType`) and reads; in `pass-both` the write `allow-writes` names, also against
    // `write-targets`.
    assert_eq!(
        found(&diags),
        vec![
            // Two calls deep, through helpers outside the scope.
            d(dec, 7, "decide", "decisions:reads:DataType.is_optional"),
            d(
                dec,
                11,
                "decide_twice",
                "decisions:reads:DataType.is_optional"
            ),
            // Reading a property runs its getter, which reads the target.
            d(
                dec,
                19,
                "decide_property",
                "decisions:reads:DataType.is_optional"
            ),
            d(
                dec,
                23,
                "decide_getattr",
                "decisions:reads:Model.class_name"
            ),
            // A subclass of the target class.
            d(dec, 27, "decide_sub", "decisions:reads:Model.class_name"),
            d(
                dec,
                35,
                "seal",
                "decisions:calls:DataType.replace_reference"
            ),
            d(dec, 36, "seal", "decisions:calls:FinalFacts"),
            // Writes of the universe that `allow-writes` does not name.
            d(pas, 10, "sort_bases", "pass-sort:Model.fields"),
            d(pas, 11, "sort_bases", "pass-sort:DataType.is_optional"),
            d(pas, 23, "both", "pass-both:DataType.is_optional"),
        ],
        "{diags:#?}"
    );
    // One report per scope function and target; it counts every site the function reaches.
    assert_eq!(sites(&diags), [1, 2, 1, 1, 1, 1, 1, 1, 2, 1]);
    // The boundary's `message` is the help of each report of it.
    for d in &diags {
        if d["target"].as_str().unwrap().starts_with("decisions:") {
            assert_eq!(d["help"], "decisions read final facts only", "{d:#}");
        } else {
            assert!(d["help"].is_null(), "a boundary without a `message`: {d:#}");
        }
    }
    let forbids = "which effect boundary `decisions` forbids";
    assert_eq!(
        diags[0]["message"],
        format!(
            "`decide` reads `DataType.is_optional` (by attribute via `pkg.helpers.nullable`), {forbids}"
        )
    );
    assert_eq!(
        related(&diags[0]),
        [
            "`decide` calls `pkg.helpers.nullable`",
            "`nullable` calls `pkg.helpers._nullable`"
        ]
    );
    assert_eq!(
        diags[3]["message"],
        format!("`decide_getattr` reads `Model.class_name` (with `getattr`), {forbids}")
    );
    assert_eq!(
        diags[5]["message"],
        format!("`seal` calls `DataType.replace_reference`, {forbids}")
    );
    assert_eq!(
        diags[6]["message"],
        format!("`seal` constructs `FinalFacts`, {forbids}")
    );
    assert_eq!(related(&diags[6]), ["`seal` calls `pkg.model.FinalFacts`"]);
    assert_eq!(
        diags[7]["message"],
        "`sort_bases` writes `Model.fields` (in place via `pkg.passes._touch`), which effect boundary `pass-sort` does not allow"
    );
}

#[test]
fn eff201_calls_deep_and_allowed_callees() {
    // The `calls` form of a call-owners contract: a call of a target several calls below a scope
    // function, an allowed callee that calls one, and a target the search enters.
    let p = Project::new("ext_eff201_calls");
    p.write(
        "pyproject.toml",
        r#"[project]
name = "t"

[[tool.heki.effect-boundaries]]
name = "no-store-ops"
scopes = ["pkg.render"]
forbid = ["calls"]
call-targets = ["Store.*"]
allowed-callees = ["pkg.cache"]
"#,
    )
    .write("pkg/__init__.py", "")
    .write(
        "pkg/store.py",
        "class Store:\n    def save(self) -> None:\n        self.flush()\n\n    def flush(self) -> None:\n        pass\n",
    )
    .write(
        "pkg/cache.py",
        "from pkg.store import Store\n\n\ndef warm(store: Store) -> None:\n    store.save()\n",
    )
    .write(
        "pkg/util.py",
        "from pkg.store import Store\n\n\ndef persist(store: Store) -> None:\n    _persist(store)\n\n\ndef _persist(store: Store) -> None:\n    store.save()\n",
    )
    .write(
        "pkg/render.py",
        r#"from pkg.cache import warm
from pkg.store import Store
from pkg.util import persist


def page(store: Store) -> None:
    persist(store)


def cached(store: Store) -> None:
    warm(store)


def direct(store: Store) -> None:
    store.save()
    store.save()
"#,
    );
    let diags = diagnostics(&p, &["--select", "EFF201"]);
    let r = "pkg/render.py";
    // `cached` reaches `Store.save` only through the allowed `pkg.cache`; inside a reached target
    // (`Store.save` calls `Store.flush`) the search goes on as for any callee, and reports the
    // first chain per target text.
    assert_eq!(
        found(&diags),
        vec![
            d(r, 7, "page", "no-store-ops:calls:Store.*"),
            d(r, 15, "direct", "no-store-ops:calls:Store.*"),
        ],
        "{diags:#?}"
    );
    // `page`: `Store.save` in `_persist`, `Store.flush` in `Store.save`; `direct`: two calls of
    // `save` and the call of `flush` inside it.
    assert_eq!(sites(&diags), [2, 3]);
    assert_eq!(
        diags[0]["message"],
        "`page` calls `Store.*` (via `pkg.util.persist`), which effect boundary `no-store-ops` forbids"
    );
    assert_eq!(
        related(&diags[0]),
        [
            "`page` calls `pkg.util.persist`",
            "`persist` calls `pkg.util._persist`",
            "`_persist` calls `pkg.store.Store.save`"
        ]
    );
}

// ======================================================================== forbidden-* scopes

const SCOPES_CONFIG: &str = r#"[project]
name = "t"

[[tool.heki.write-owners]]
name = "parser-writes"
targets = ["Model.fields"]
forbidden-writers = ["pkg.parser.*"]
allowed-writers = ["pkg.parser.store.*"]

[[tool.heki.read-owners]]
name = "backend-inspection"
targets = ["Model.backend_data"]
forbidden-readers = ["pkg.parser.*"]
exempt-methods = ["Model._set_backend"]

[[tool.heki.call-owners]]
name = "backend-calls"
targets = ["Model.append_backend"]
forbidden-callers = ["pkg.parser.*"]
"#;

const SCOPES_MODEL: &str = r#"class Model:
    def __init__(self) -> None:
        self.fields: list[int] = []
        self.backend_data: dict[str, int] = {}

    def append_backend(self, key: str, value: int) -> None:
        self.backend_data[key] = value

    def _set_backend(self, key: str, value: int) -> None:
        self.backend_data[key] = value

    def _peek_backend(self) -> dict[str, int]:
        return self.backend_data
"#;

const SCOPES_PARSER: &str = r#"from pkg.model import Model
from pkg.render.helpers import _render_backend


def p_write(m: Model) -> None:
    m.fields.append(1)


def p_read(m: Model) -> dict[str, int]:
    return m.backend_data


def p_call(m: Model) -> None:
    m.append_backend("k", 1)


def p_exempt(m: Model) -> None:
    m._set_backend("k", 1)


def p_peek(m: Model) -> dict[str, int]:
    return m._peek_backend()


def p_other_layer(m: Model) -> dict[str, int]:
    return _render_backend(m)


def _p_private(m: Model) -> dict[str, int]:
    return m.backend_data
"#;

const SCOPES_RENDER: &str = r#"from pkg.model import Model


def r_write(m: Model) -> None:
    m.fields.append(3)


def r_read(m: Model) -> dict[str, int]:
    return m.backend_data


def r_call(m: Model) -> None:
    m.append_backend("k", 2)


def _render_backend(m: Model) -> dict[str, int]:
    return m.backend_data


def r_uses_parser_private(m: Model) -> dict[str, int]:
    from pkg.parser.passes import _p_private

    return _p_private(m)
"#;

const INITIALIZATION_CONFIG: &str = r#"[project]
name = "t"

[[tool.heki.effect-boundaries]]
name = "passes"
scopes = ["pkg.passes.*"]
forbid = ["reads-targets"]
read-targets = ["Node.tag"]
write-universe = ["Node.*"]
"#;

/// An initializer hands its `self` to helpers: a private function, a private method, a public
/// function, through `super().__init__()`; `Pair` also hands another node to one.
const INITIALIZATION_MODEL: &str = r#"from __future__ import annotations


class Node:
    def __init__(self, members: list[Node]) -> None:
        self.members = members
        self.tag = ""
        _link(self)
        self._init_fields()

    def _init_fields(self) -> None:
        _mark(self)

    def relink(self) -> None:
        _link(self)


class Pair(Node):
    def __init__(self, first: Node, second: Node) -> None:
        super().__init__([first, second])
        link(self)
        _mark(second)


def _link(node: Node) -> None:
    if len(node.members) > 1:
        node.optional = True


def _mark(node: Node) -> None:
    node.marked = bool(node.tag)


def link(node: Node) -> None:
    node.linked = True


def _relink(node: Node) -> None:
    _link(node)


def build_union(members: list[Node]) -> Node:
    return Node(members)
"#;

const INITIALIZATION_PASSES: &str = r#"from pkg.model import Node, Pair, _relink, build_union


def renders_a_hint(node: Node) -> int:
    return len(build_union([node, node]).members)


def constructs_directly(node: Node) -> int:
    return len(Node([node, node]).members)


def pairs(first: Node, second: Node) -> Pair:
    return Pair(first, second)


def links_another(node: Node) -> Node:
    made = Node([])
    _relink(node)
    return made


def relinks(node: Node) -> None:
    node.relink()


def writes_an_existing_node(node: Node) -> None:
    node.optional = True
"#;

/// What an initializer's helpers do to its `self` is the class's own initialization, as what the
/// initializer does to it: constructing a node (`Node(...)`, `build_union`, `Pair(...)` through
/// `super().__init__()`) writes and reads no existing node. The same helpers on another object
/// stay reported: `Pair` marking `second`, `_link` reached again through `_relink` (after the
/// path through `Node.__init__` reached it first), and a method calling it after `__init__`
/// returned.
#[test]
fn eff201_initialization_through_the_helpers_it_passes_self_to() {
    let p = Project::new("ext_eff201_initialization");
    p.write("pyproject.toml", INITIALIZATION_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", INITIALIZATION_MODEL)
        .write("pkg/passes.py", INITIALIZATION_PASSES);
    let diags = diagnostics(&p, &["--select", "EFF201"]);
    let pas = "pkg/passes.py";
    assert_eq!(
        found(&diags),
        vec![
            d(pas, 13, "pairs", "passes:Node.marked"),
            d(pas, 13, "pairs", "passes:reads:Node.tag"),
            d(pas, 18, "links_another", "passes:Node.optional"),
            d(pas, 23, "relinks", "passes:Node.optional"),
            d(pas, 27, "writes_an_existing_node", "passes:Node.optional"),
        ]
    );
    assert_eq!(
        related(by_qualname(&diags, "links_another")),
        [
            "`links_another` calls `pkg.model._relink`",
            "`_relink` calls `pkg.model._link`",
        ]
    );
    assert_eq!(
        related(by_qualname(&diags, "pairs")),
        [
            "`pairs` calls `pkg.model.Pair.__init__`",
            "`Pair.__init__` calls `pkg.model._mark`"
        ]
    );
}

#[test]
fn forbidden_scopes_restrict_owner_contracts() {
    let p = Project::new("ext_scopes");
    p.write("pyproject.toml", SCOPES_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/parser/__init__.py", "")
        .write("pkg/render/__init__.py", "")
        .write("pkg/model.py", SCOPES_MODEL)
        .write("pkg/parser/passes.py", SCOPES_PARSER)
        .write(
            "pkg/parser/store.py",
            "from pkg.model import Model\n\n\ndef save(m: Model) -> None:\n    m.fields.append(2)\n",
        )
        .write("pkg/render/helpers.py", SCOPES_RENDER);
    let diags = diagnostics(&p, &["--select", "MUT201,MUT203,MUT204"]);
    let (pp, rh) = ("pkg/parser/passes.py", "pkg/render/helpers.py");
    // Not reported: every access outside `pkg.parser` (`r_write`, `r_read`, `r_call`, the model's
    // own methods); the allowed writer `pkg.parser.store.save` inside the forbidden scope; the
    // exempt accessor `Model._set_backend`.
    assert_eq!(
        found(&diags),
        vec![
            d(pp, 6, "p_write", "Model.fields"),
            d(pp, 10, "p_read", "Model.backend_data"),
            d(pp, 14, "p_call", "Model.append_backend"),
            // A private method of the model hands its read to its caller in the parser.
            d(pp, 22, "p_peek", "Model.backend_data"),
            // So does a private helper of another layer.
            d(pp, 26, "p_other_layer", "Model.backend_data"),
            // A private helper of the parser reports at its caller outside: the read happens in
            // forbidden code.
            d(rh, 23, "r_uses_parser_private", "Model.backend_data"),
        ],
        "{diags:#?}"
    );
    assert_eq!(
        diags[0]["message"],
        "`p_write` writes `Model.fields` (in place) inside the forbidden writers of `parser-writes`"
    );
    assert_eq!(
        diags[1]["message"],
        "`p_read` reads `Model.backend_data` (by attribute) inside the forbidden readers of `backend-inspection`"
    );
    assert_eq!(
        diags[2]["message"],
        "`p_call` calls `Model.append_backend` inside the forbidden callers of `backend-calls`"
    );
    assert_eq!(
        diags[5]["message"],
        "`r_uses_parser_private` reads `Model.backend_data` (by attribute) via `pkg.parser.passes._p_private` inside the forbidden readers of `backend-inspection`"
    );
}

// ================================================================================== baseline

const BASELINE_CONFIG: &str = r#"[project]
name = "t"

[[tool.heki.write-owners]]
name = "store"
targets = ["Model.fields"]
allowed-writers = ["pkg.store.*"]

[[tool.heki.read-owners]]
name = "kids"
targets = ["Model.kids"]

[[tool.heki.call-owners]]
name = "ops"
targets = ["Model.reset"]

[[tool.heki.effect-boundaries]]
name = "pure"
scopes = ["pkg.m.render"]
forbid = ["sleep"]
"#;

const BASELINE_SRC: &str = r#"import time


class Model:
    def __init__(self) -> None:
        self.fields: list[str] = []
        self.kids: list[str] = []

    def reset(self) -> None:
        pass


def _extend(fields: list[str]) -> None:
    fields.append("id")


def direct(model: Model) -> None:
    model.fields = []


def via_helper(model: Model) -> None:
    _extend(model.fields)


def _private_writer(model: Model) -> None:
    model.fields.append("p")


def public_caller(model: Model) -> None:
    _private_writer(model)


def _peek(model: Model) -> int:
    return len(model.kids)


def reader(model: Model) -> int:
    return _peek(model)


def _do(model: Model) -> None:
    model.reset()


def caller(model: Model) -> None:
    _do(model)


def render() -> None:
    time.sleep(1)
"#;

/// The second violation of each kind: in a function, in a helper call, inside a private helper
/// that its caller's report stands for, and below a scope function.
fn second_violations(src: &str) -> String {
    src.replace(
        "    model.fields = []\n",
        "    model.fields = []\n    model.fields = [\"second\"]\n",
    )
    .replace(
        "    _extend(model.fields)\n",
        "    _extend(model.fields)\n    _extend(model.fields)\n",
    )
    .replace(
        "    model.fields.append(\"p\")\n",
        "    model.fields.append(\"p\")\n    model.fields.append(\"q\")\n",
    )
    .replace(
        "    return len(model.kids)\n",
        "    return len(model.kids) + len(model.kids)\n",
    )
    .replace(
        "    model.reset()\n",
        "    model.reset()\n    model.reset()\n",
    )
    .replace(
        "    time.sleep(1)\n",
        "    time.sleep(1)\n    time.sleep(2)\n",
    )
}

/// `(rule, qualname)` of every concise line.
fn regressed(stdout: &str) -> Vec<(String, String)> {
    stdout
        .lines()
        .map(|l| {
            let rule = l.split(' ').nth(1).unwrap().to_owned();
            let qualname = l.split('`').nth(1).unwrap().to_owned();
            (rule, qualname)
        })
        .collect()
}

fn rq(rule: &str, qualname: &str) -> (String, String) {
    (rule.into(), qualname.into())
}

#[test]
fn baselines_count_violation_sites() {
    let p = Project::new("ext_baseline_sites");
    p.write("pyproject.toml", BASELINE_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/m.py", BASELINE_SRC);
    let args = ["--baseline", "heki-baseline.toml"];
    let mut update = args.to_vec();
    update.push("--update-baseline");
    let (code, _, stderr) = p.check(&update);
    assert_eq!(code, 0, "{stderr}");
    let text = p.read("heki-baseline.toml");
    assert!(text.contains("\nversion = 2\n"), "{text}");
    assert_eq!(p.check(&args).0, 0);

    // Every second violation is a regression, the one inside the private helper too (its report
    // at `public_caller` now stands for two writes), and moving code still does not matter.
    p.write(
        "pkg/m.py",
        &format!("# moved\n\n{}", second_violations(BASELINE_SRC)),
    );
    let mut concise = args.to_vec();
    concise.extend(["--output-format", "concise"]);
    let (code, stdout, stderr) = p.check(&concise);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let mut got = regressed(&stdout);
    got.dedup();
    assert_eq!(
        got,
        vec![
            rq("MUT201", "direct"),
            rq("MUT201", "via_helper"),
            rq("MUT201", "public_caller"),
            rq("MUT203", "reader"),
            rq("MUT204", "caller"),
            rq("EFF201", "render"),
        ],
        "{stdout}"
    );
    assert!(
        stderr.contains(
            "note: baseline entry pkg/m.py::public_caller MUT201 `Model.fields` allows 1 site(s), found 2"
        ),
        "{stderr}"
    );
    // The baseline only shrinks: `--update-baseline` reports them and keeps the file.
    let recorded = p.read("heki-baseline.toml");
    let (code, _, stderr) = p.check(&update);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("error: baseline `heki-baseline.toml` not updated: 6 entry(ies) grew"),
        "{stderr}"
    );
    assert_eq!(p.read("heki-baseline.toml"), recorded);
    // Recording them takes a new baseline.
    std::fs::remove_file(p.dir.join("heki-baseline.toml")).unwrap();
    assert_eq!(p.check(&update).0, 0);
    assert!(p.read("heki-baseline.toml").contains("count = 2"));
    assert_eq!(p.check(&args).0, 0);
}

#[test]
fn baselines_without_a_version_keep_counting_diagnostics() {
    let p = Project::new("ext_baseline_v1");
    p.write("pyproject.toml", BASELINE_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/m.py", BASELINE_SRC);
    let args = ["--baseline", "heki-baseline.toml"];
    let mut update = args.to_vec();
    update.push("--update-baseline");
    assert_eq!(p.check(&update).0, 0);
    // A baseline written before baselines counted sites: no `version`.
    let old = p.read("heki-baseline.toml").replace("version = 2\n", "");
    p.write("heki-baseline.toml", &old);
    let (code, _, stderr) = p.check(&args);
    assert_eq!(code, 0, "{stderr}");
    assert!(
        stderr.contains(
            "note: baseline heki-baseline.toml counts diagnostics, not violation sites (written before version 2); `--update-baseline` rewrites it to count sites"
        ),
        "{stderr}"
    );
    // Its counts keep their meaning: new diagnostics are regressions, a second site behind a
    // recorded diagnostic is not.
    p.write("pkg/m.py", &second_violations(BASELINE_SRC));
    let mut concise = args.to_vec();
    concise.extend(["--output-format", "concise"]);
    let (code, stdout, stderr) = p.check(&concise);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let mut got = regressed(&stdout);
    got.dedup();
    assert_eq!(
        got,
        vec![rq("MUT201", "direct"), rq("MUT201", "via_helper")],
        "{stdout}"
    );
    assert!(
        stderr.contains(
            "note: baseline entry pkg/m.py::direct MUT201 `Model.fields` allows 1 diagnostic(s), found 2"
        ),
        "{stderr}"
    );
    // `--update-baseline` compares in the old unit first: the new diagnostics keep it as it is.
    let (code, _, stderr) = p.check(&update);
    assert_eq!(code, 1, "{stderr}");
    assert!(
        stderr.contains("error: baseline `heki-baseline.toml` not updated: 2 entry(ies) grew"),
        "{stderr}"
    );
    assert_eq!(p.read("heki-baseline.toml"), old);
    // Without them, it is rewritten to count sites.
    p.write("pkg/m.py", BASELINE_SRC);
    assert_eq!(p.check(&update).0, 0);
    assert!(p.read("heki-baseline.toml").contains("\nversion = 2\n"));
    // A version this heki does not know is an error.
    p.write("heki-baseline.toml", &format!("version = 3\n{old}"));
    let (code, _, stderr) = p.check(&args);
    assert_eq!(code, 2);
    assert!(
        stderr.contains(
            "error: heki-baseline.toml: the baseline has version 3; this heki reads versions 1 and 2"
        ),
        "{stderr}"
    );
}

// ============================================================================ configuration

#[test]
fn extension_configuration_errors_exit_2() {
    let p = Project::new("ext_cfg_errors");
    p.write("a.py", "def f():\n    return 1\n");
    let fails = |toml: &str, needle: &str| {
        p.write(
            "pyproject.toml",
            &format!("[project]\nname = \"t\"\n\n{toml}"),
        );
        let (code, stdout, stderr) = p.check(&[]);
        assert_eq!(code, 2, "{toml}: {stdout}{stderr}");
        assert!(stderr.contains(needle), "{toml}: {stderr}");
    };
    let boundary = |rest: &str| {
        format!("[[tool.heki.effect-boundaries]]\nname = \"b\"\nscopes = [\"pkg.*\"]\n{rest}")
    };
    fails(
        &boundary("forbid = [\"reads-targets\"]\n"),
        "effect-boundaries `b` forbids `reads-targets` but sets no `read-targets`",
    );
    fails(
        &boundary("forbid = [\"calls\"]\n"),
        "effect-boundaries `b` forbids `calls` but sets no `call-targets`",
    );
    fails(
        &boundary("allow-writes = [\"Model.fields\"]\n"),
        "effect-boundaries `b` sets `allow-writes` but no `write-universe`",
    );
    fails(
        &boundary("forbid = [\"reads-targets\"]\nread-targets = [\"fields\"]\n"),
        "invalid effect-boundaries.read-targets target `fields` (expected `Class.attr`)",
    );
    fails(
        &boundary("write-universe = [\"Model\"]\n"),
        "invalid effect-boundaries.write-universe target `Model` (expected `Class.attr`)",
    );
    fails(
        &boundary("write-universe = [\"Model.*\"]\nallow-writes = [\"fields\"]\n"),
        "invalid effect-boundaries.allow-writes target `fields` (expected `Class.attr`)",
    );
    fails(
        &boundary("forbid = [\"calls\"]\ncall-targets = [\"Store.[x\"]\n"),
        "invalid glob `Store.[x` in effect-boundaries.call-targets",
    );
    let owners = |rest: &str| format!("[[tool.heki.call-owners]]\nname = \"c\"\n{rest}");
    fails(
        &owners("targets = [\"Store.[save\"]\n"),
        "invalid glob `Store.[save` in call-owners.targets",
    );
    fails(
        &owners("targets = [\"Store.save\"]\nallowed-callers = [\"x.[y\"]\n"),
        "invalid glob `x.[y` in call-owners.allowed-callers",
    );
    fails(
        &owners("targets = [\"Store.save\"]\nforbidden-callers = [\"p.[q\"]\n"),
        "invalid glob `p.[q` in call-owners.forbidden-callers",
    );
    fails(
        &owners("targets = [\"Store.save\"]\nexempt-methods = [\"Store.[z\"]\n"),
        "invalid glob `Store.[z` in call-owners.exempt-methods",
    );
    fails(
        "[[tool.heki.write-owners]]\nname = \"w\"\ntargets = [\"Model.fields\"]\nforbidden-writers = [\"[w\"]\n",
        "invalid glob `[w` in write-owners.forbidden-writers",
    );
    fails(
        "[[tool.heki.read-owners]]\nname = \"r\"\ntargets = [\"Model.fields\"]\nforbidden-readers = [\"[r\"]\n",
        "invalid glob `[r` in read-owners.forbidden-readers",
    );
    // Globs that each compile but are too many to match together in one set.
    let globs: Vec<String> = (0..20_000)
        .map(|i| format!("\"*X{i}?[ab]*{{c,d}}*._*\""))
        .collect();
    fails(
        &owners(&format!("targets = [{}]\n", globs.join(", "))),
        "the globs of call-owners.targets are too many to match together",
    );
    // A contract whose rules do not run (`MUT204`, and its strict mode `MUT207`) is not
    // validated (as for the other contracts); a target that names nothing reports nothing.
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n{}",
            owners("targets = [\"Store.[save\"]\n")
        ),
    );
    assert_eq!(p.check(&["--ignore", "MUT204,MUT207"]).0, 0);
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n{}",
            owners("targets = [\"Nothing.here\", \"nowhere\"]\n")
        ),
    );
    assert_eq!(p.check(&[]).0, 0);
}

// ================================================================================ heki rule

#[test]
fn mut204_rule_doc() {
    let p = Project::new("ext_rule_docs");
    let (status, doc, _) = p.heki(&["rule", "MUT204"]);
    assert_eq!(status, 0);
    assert!(
        doc.starts_with("# call-owner-violation (MUT204)\n\n"),
        "{doc}"
    );
    let at = |h: &str| doc.find(h).unwrap_or_else(|| panic!("no {h}"));
    assert!(
        at("## What it does\n") < at("## Why is this bad?\n")
            && at("## Why is this bad?\n") < at("## Example\n")
            && at("## Example\n") < at("Use instead:\n")
            && at("Use instead:\n") < at("## Fix safety\nNo automatic fix.\n")
            && at("## Fix safety\n") < at("## Options\n"),
        "{doc}"
    );
    for key in [
        "allowed-callers",
        "forbidden-callers",
        "exempt-methods",
        "message",
    ] {
        assert!(doc.contains(key), "{key}: {doc}");
    }
    let (_, eff201, _) = p.heki(&["rule", "EFF201"]);
    for key in [
        "reads-targets",
        "call-targets",
        "allow-writes",
        "write-universe",
    ] {
        assert!(eff201.contains(key), "{key}: {eff201}");
    }
}
