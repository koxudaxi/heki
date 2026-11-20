//! Declarative contracts end to end through the real binary: `MUT201` exempt methods, messages,
//! and the class-family initializer rule; `MUT203` read owners (aliases, `getattr`, `__dict__`,
//! subclass targets, private delegation through the reverse call graph); `EFF201` effect
//! boundaries (forbidden effects 1-4 calls deep, `allowed-callees`, other scope functions,
//! un-awaited coroutines, `writes-targets`, `mutates-params`); configuration errors, Ratchet
//! baselines, and `heki rule`.

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

fn messages(diags: &[Value]) -> Vec<&str> {
    diags
        .iter()
        .map(|x| x["message"].as_str().unwrap())
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

// ======================================================================================= MUT201

const MUT201_CONFIG: &str = r#"[project]
name = "t"

[[tool.heki.write-owners]]
name = "generation-store"
targets = ["DataModel.fields"]
allowed-writers = ["Store.*"]
exempt-methods = ["__deepcopy__", "DataModel.copy_deep", "clone_*"]
forbid-cached-property-on = ["PydanticModel"]
message = "use GenerationStore.set_fields() instead"
"#;

const MUT201_MODEL: &str = r#"from functools import cached_property


class DataModel:
    def __init__(self):
        self.fields = []
        self._init_fields()

    def _init_fields(self):
        self.fields.append(0)

    def __deepcopy__(self, memo):
        new = DataModel()
        _fill(new, self.fields)
        return new

    def copy_deep(self):
        new = DataModel()
        new.fields = list(self.fields)
        return new

    def clone_shallow(self):
        new = DataModel()
        new.fields = self.fields
        return new

    def reset(self):
        self.fields = []


def _fill(m: DataModel, fields):
    m.fields = list(fields)


class PydanticModel(DataModel):
    def __init__(self):
        super().__init__()
        self.fields.append(1)
        fields = self.fields
        fields.append(2)

    @cached_property
    def size(self):
        return len(self.fields)


def _setup(m: DataModel):
    m.fields = []


class Annotated(DataModel):
    def __init__(self):
        _setup(self)


class Other(DataModel):
    def __init__(self, src: DataModel, peer: "Other", flag):
        super().__init__()
        self._init_fields()
        src.fields.append(3)
        self._copy_into(src)
        fresh = DataModel()
        fresh.fields.append(4)
        target = self if flag else peer
        target.fields.append(5)
        items = self.fields if flag else self.extra
        items.append(6)
        self.child = DataModel()
        self.child.fields.append(7)

    def _copy_into(self, target: DataModel):
        target.fields = []


class Unrelated:
    def __init__(self, m: DataModel):
        m.fields = []


class Store:
    def set_fields(self, m: DataModel):
        _fill(m, [])


def build():
    return PydanticModel()
"#;

#[test]
fn mut201_exempt_methods_messages_and_class_family_initializers() {
    let p = Project::new("v1x_mut201");
    p.write("pyproject.toml", MUT201_CONFIG)
        .write("model.py", MUT201_MODEL);
    let diags = diagnostics(&p, &["--select", "MUT201"]);
    let m = "model.py";
    let t = "DataModel.fields";
    // Not reported: exempt methods by plain name (`__deepcopy__`, also through its private
    // helper `_fill`), by dotted pattern (`DataModel.copy_deep`) and by glob (`clone_*`); the
    // allowed writer `Store.set_fields` through `_fill`; the class family's own initialization
    // on `self` — `DataModel.__init__` through the private `self._init_fields()`, the subclass
    // `PydanticModel.__init__` (inherited field, local alias, `super().__init__()`), the
    // inherited helper from `Other.__init__`, `_setup(self)` typed with the base class from
    // `Annotated.__init__`, and an object that may be `self` (`self if flag else peer`); a
    // merge of two different attributes (`self.fields if flag else self.extra`) is no alias;
    // and constructing an instance (`build`), also filling a fresh instance before it is stored
    // anywhere (`fresh-objects = "allow"`, the default).
    let reported = vec![
        d(m, 28, "DataModel.reset", t),
        d(m, 42, "PydanticModel.size", "PydanticModel.size"),
        // Other objects written from an initializer: another parameter, through a private
        // helper, an attribute of `self`; and an initializer of a class outside the target
        // family.
        d(m, 60, "Other.__init__", t),
        d(m, 61, "Other.__init__", t),
        d(m, 69, "Other.__init__", t),
        d(m, 77, "Unrelated.__init__", t),
    ];
    assert_eq!(found(&diags), reported, "{diags:#?}");
    // `fresh-objects = "report"`: the fresh instance's write is reported too.
    let strict = Project::new("mut201_report_fresh");
    strict
        .write(
            "pyproject.toml",
            &format!("{MUT201_CONFIG}fresh-objects = \"report\"\n"),
        )
        .write("model.py", MUT201_MODEL);
    let mut with_fresh = reported.clone();
    with_fresh.insert(4, d(m, 63, "Other.__init__", t));
    assert_eq!(
        found(&diagnostics(&strict, &["--select", "MUT201"])),
        with_fresh
    );
    // The contract's `message` is the help of every report, not part of the message.
    assert!(
        diags
            .iter()
            .all(|d| d["help"] == "use GenerationStore.set_fields() instead"),
        "{diags:#?}"
    );
    assert_eq!(
        diags[0]["message"],
        "`DataModel.reset` writes `DataModel.fields` (direct_attr) outside the write owners of `generation-store`"
    );
    assert_eq!(
        diags[1]["message"],
        "`@cached_property` `size` is defined on `PydanticModel`, which `generation-store` forbids caching on"
    );
    assert!(
        diags[3]["message"]
            .as_str()
            .unwrap()
            .contains("(direct_attr) via `model.Other._copy_into`"),
        "{diags:#?}"
    );
    assert_eq!(
        related(&diags[3]),
        ["`Other.__init__` calls `model.Other._copy_into`"]
    );
    assert!(diags.iter().all(|x| x["fix"].is_null()));
}

// ======================================================================================= MUT203

const MUT203_CONFIG: &str = r#"[[read-owners]]
name = "reference-children"
targets = ["Reference.children"]
allowed-readers = ["*GenerationIndex*"]
exempt-methods = ["__repr__", "Sub2.dump"]
message = "read children through GenerationIndex instead"
"#;

const MUT203_MODEL: &str = r#"class Reference:
    def __init__(self, parent: "Reference | None" = None):
        self.children = []
        self.size = len(self.children)
        self._index()
        if parent is not None:
            self.depth = len(parent.children)

    def _index(self):
        return list(self.children)

    def __repr__(self):
        return repr(self.children)

    def names(self):
        return [c.name for c in self.children]


class Sub(Reference):
    def first(self):
        return self.children[0]


class Sub2(Reference):
    def dump(self):
        return self.children


class GenerationIndex:
    def kids(self, r: Reference):
        return r.children

    def tree(self, r: Reference):
        return _collect(r)


def _collect(r: Reference):
    return r.children
"#;

const MUT203_APP: &str = r#"from pkg.model import Reference, Sub


def direct(r: Reference):
    return r.children


def alias(r: Reference, flag):
    x = r
    if flag:
        y = x
    else:
        y = r
    return y.children


def via_getattr(r: Reference):
    a = getattr(r, "children")
    b = getattr(r, "children", None)
    c = getattr(r, "name")
    return a, b, c


def via_dict(r: Reference):
    a = r.__dict__["children"]
    b = r.__dict__.get("children")
    c = r.__dict__.get("name")
    return a, b, c


def sub(s: Sub):
    return s.children


def untyped(r):
    return r.children


def _kids(r: Reference):
    return r.children


def _deeper(r: Reference):
    return _kids(r)


def build(r: Reference):
    _kids(r)
    return _deeper(r)


def _walk(r: Reference):
    for c in r.children:
        _walk(c)


def tree(r: Reference):
    _walk(r)


def _orphan(r: Reference):
    return r.children


def indexed(index, r: Reference):
    return _collect_again(r)


def _collect_again(r: Reference):
    return r.children
"#;

#[test]
fn mut203_reads_aliases_getattr_dict_and_private_delegation() {
    let p = Project::new("v1x_mut203");
    p.write("cfg/heki.toml", MUT203_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", MUT203_MODEL)
        .write("pkg/app.py", MUT203_APP);
    let diags = diagnostics(&p, &["--config", "cfg/heki.toml", "--select", "MUT203"]);
    let (a, m, t) = ("pkg/app.py", "pkg/model.py", "Reference.children");
    // Not reported: other attributes (`getattr(r, "name")`, `__dict__.get("name")`); a read
    // through an unannotated parameter (`untyped`, a known limitation); the private helpers
    // themselves (`_kids`, `_deeper`, `_walk`, `_collect_again`: their callers are reported);
    // the allowed reader `GenerationIndex` (also through its private helper `_collect`); the
    // exempt `__repr__` (plain name) and `Sub2.dump` (dotted pattern); and the initializer's
    // reads of its own `self` (directly and through `self._index()`).
    assert_eq!(
        found(&diags),
        vec![
            d(a, 5, "direct", t),
            // `x = r` then a phi of both: still a `Reference`.
            d(a, 14, "alias", t),
            d(a, 18, "via_getattr", t),
            d(a, 19, "via_getattr", t),
            d(a, 25, "via_dict", t),
            d(a, 26, "via_dict", t),
            // A subclass of the target class.
            d(a, 32, "sub", t),
            // Each call site of the outermost non-private caller: directly and two calls deep.
            d(a, 48, "build", t),
            d(a, 49, "build", t),
            // A recursive private helper: once, at its public caller.
            d(a, 58, "tree", t),
            // A private function without project callers reports for itself.
            d(a, 62, "_orphan", t),
            d(a, 66, "indexed", t),
            // An initializer reading another instance.
            d(m, 7, "Reference.__init__", t),
            d(m, 16, "Reference.names", t),
            d(m, 21, "Sub.first", t),
        ],
        "{diags:#?}"
    );
    assert!(
        diags
            .iter()
            .all(|d| d["help"] == "read children through GenerationIndex instead"),
        "{diags:#?}"
    );
    assert_eq!(
        diags[0]["message"],
        "`direct` reads `Reference.children` (direct_attr) outside the read owners of `reference-children`"
    );
    let kinds: Vec<&str> = messages(&diags)
        .iter()
        .filter_map(|m| m.split('(').nth(1)?.split(')').next())
        .collect();
    assert_eq!(
        kinds[..7],
        [
            "direct_attr",
            "direct_attr",
            "getattr",
            "getattr",
            "dict_direct",
            "dict_direct",
            "direct_attr"
        ]
    );
    let deep = &diags[8];
    assert_eq!(
        deep["message"],
        "`build` reads `Reference.children` (direct_attr) via `pkg.app._kids` outside the read owners of `reference-children`"
    );
    assert_eq!(
        related(deep),
        [
            "`build` calls `pkg.app._deeper`",
            "`_deeper` calls `pkg.app._kids`"
        ]
    );
    assert_eq!(deep["related"][1]["line"], 44);
    assert_eq!(related(&diags[7]), ["`build` calls `pkg.app._kids`"]);
    assert!(diags.iter().all(|x| x["fix"].is_null()));
}

// ======================================================================================= EFF201

const EFF201_EXEC_CONFIG: &str = r#"[project]
name = "t"

[[tool.heki.effect-boundaries]]
name = "pure-render"
scopes = ["*.render"]
forbid = ["blocking"]
allowed-callees = ["pkg.cache"]
message = "render entry points must stay pure"

[[tool.heki.effect-boundaries]]
name = "domain"
scopes = ["pkg.domain.*"]
forbid = ["blocking-net", "sleep"]
allowed-callees = ["requests.post"]

[[tool.heki.effect-boundaries]]
name = "jobs"
scopes = ["pkg.jobs.Job"]
forbid = ["blocking-db", "blocking-fs", "blocking-subprocess", "blocking-sync"]

[[tool.heki.ecosystem]]
symbol = "locklib.acquire"
effects = ["blocking_sync"]
"#;

const EFF201_IO: &str = r#"import requests
import time


def get_json(url):
    return requests.get(url)


def fetch(url):
    return get_json(url)


def fetch_deep(url):
    return fetch(url)


def nap():
    time.sleep(1)


async def anap():
    time.sleep(1)
"#;

const EFF201_CACHE: &str = r#"import time


def cached():
    time.sleep(1)
"#;

const EFF201_VIEWS: &str = r#"import requests

from pkg.cache import cached
from pkg.io import anap, fetch_deep, get_json, nap


class Page:
    def render(self):
        requests.get("x")
        return get_json("y")

    def helper(self):
        return fetch_deep("z")


class Card:
    def render(self):
        return self.body()

    def body(self):
        return fetch_deep("u")


class Nested:
    def render(self):
        Page().render()
        cached()

        def inner():
            nap()

        return inner


class Lazy:
    async def render(self):
        anap()
        await anap()


def outside():
    nap()
"#;

const EFF201_DOMAIN: &str = r#"import requests

from pkg.io import fetch, nap


def check():
    requests.post("a")
    fetch("b")
    nap()


def _private():
    nap()
"#;

const EFF201_JOBS: &str = r#"import sqlite3
import subprocess

import locklib


class Job:
    def run(self, path):
        conn = sqlite3.connect(path)
        open(path).read()
        subprocess.run(["true"])
        locklib.acquire()
        return conn


def free(path):
    return open(path).read()
"#;

#[test]
fn eff201_reaches_forbidden_effects_through_calls() {
    let p = Project::new("v1x_eff201_exec");
    p.write("pyproject.toml", EFF201_EXEC_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/io.py", EFF201_IO)
        .write("pkg/cache.py", EFF201_CACHE)
        .write("pkg/views.py", EFF201_VIEWS)
        .write("pkg/domain/__init__.py", "")
        .write("pkg/domain/rules.py", EFF201_DOMAIN)
        .write("pkg/jobs.py", EFF201_JOBS);
    let diags = diagnostics(&p, &["--select", "EFF201"]);
    let (r, j, v) = ("pkg/domain/rules.py", "pkg/jobs.py", "pkg/views.py");
    // Not reported: functions outside every scope (`Page.helper`, `Card.body`, `outside`,
    // `free`); the allowed `requests.post` (an ecosystem symbol: the net report is at the
    // `fetch` call, not before it) and `pkg.cache.cached` (a module prefix); `Page.render`
    // called from `Nested.render` (another scope function: reported once, by itself); the
    // un-awaited `anap()` (only creates a coroutine: the sleep is at the awaited call).
    assert_eq!(
        found(&diags),
        vec![
            // A module glob scope: private functions are in scope too.
            d(r, 8, "check", "domain:blocking_net"),
            d(r, 9, "check", "domain:sleep"),
            d(r, 13, "_private", "domain:sleep"),
            // A class scope, one report per forbidden kind (`blocking_sync` from a user
            // ecosystem entry).
            d(j, 9, "Job.run", "jobs:blocking_db"),
            d(j, 10, "Job.run", "jobs:blocking_fs"),
            d(j, 11, "Job.run", "jobs:blocking_subprocess"),
            d(j, 12, "Job.run", "jobs:blocking_sync"),
            // Two paths to `blocking_net`: one report, at the shortest (the direct call).
            d(v, 9, "Page.render", "pure-render:blocking_net"),
            // Four calls deep.
            d(v, 18, "Card.render", "pure-render:blocking_net"),
            // A nested function of a scope function is in scope and reports for itself.
            d(v, 30, "Nested.render.<locals>.inner", "pure-render:sleep"),
            d(v, 38, "Lazy.render", "pure-render:sleep"),
        ],
        "{diags:#?}"
    );
    let page = &diags[7];
    assert_eq!(page["help"], "render entry points must stay pure");
    assert_eq!(
        page["message"],
        "`Page.render` reaches blocking_net (`requests.get`), which effect boundary `pure-render` forbids"
    );
    assert_eq!(related(page), ["`Page.render` calls `requests.get`"]);
    let card = &diags[8];
    assert_eq!(
        card["message"],
        "`Card.render` reaches blocking_net (`requests.get` via `pkg.views.Card.body`), which effect boundary `pure-render` forbids"
    );
    assert_eq!(
        related(card),
        [
            "`Card.render` calls `pkg.views.Card.body`",
            "`Card.body` calls `pkg.io.fetch_deep`",
            "`fetch_deep` calls `pkg.io.fetch`",
            "`fetch` calls `pkg.io.get_json`",
            "`get_json` calls `requests.get`",
        ]
    );
    assert_eq!(card["related"][4]["path"], "pkg/io.py");
    assert_eq!(card["related"][4]["line"], 6);
    assert_eq!(
        diags[0]["message"],
        "`check` reaches blocking_net (`requests.get` via `pkg.io.fetch`), which effect boundary `domain` forbids"
    );
    assert!(diags.iter().all(|x| x["fix"].is_null()));
}

const EFF201_WRITES_CONFIG: &str = r#"[[effect-boundaries]]
name = "pure-render"
scopes = ["pkg.render_mod.Renderer", "*.draw"]
forbid = ["writes-targets", "mutates-params"]
write-targets = ["DataModel.fields"]
allowed-callees = ["pkg.store.*"]
"#;

const EFF201_MODEL: &str = r#"class DataModel:
    def __init__(self):
        self.fields = []
        self.name = ""


class SubModel(DataModel):
    pass


def _push(items):
    items.append(1)


def add_field(m: DataModel):
    m.fields.append(1)


def touch(m: DataModel):
    add_field(m)


def rename(m: DataModel, name):
    m.name = name
"#;

const EFF201_STORE: &str = r#"from pkg.model import DataModel


def save(m: DataModel):
    m.fields.append(2)
"#;

const EFF201_RENDER: &str = r#"from pkg.model import DataModel, SubModel, _push, rename, touch
from pkg.store import save


class Renderer:
    def direct(self, m: DataModel):
        m.fields = []

    def indirect(self, m: DataModel):
        _push(m.fields)

    def deep(self, m: DataModel):
        touch(m)

    def sub_direct(self, s: SubModel):
        s.fields.clear()

    def allowed(self, m: DataModel):
        save(m)

    def own_state(self):
        self.cache = {}

    def relabel(self, m: DataModel):
        rename(m, "x")

    def local_only(self):
        m = DataModel()
        m.name = "x"
        items = []
        items.append(1)
        return m


def draw(n):
    other = DataModel()
    _push(other.fields)
    return n
"#;

#[test]
fn eff201_writes_targets_and_mutates_params() {
    let p = Project::new("v1x_eff201_writes");
    p.write("heki.toml", EFF201_WRITES_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", EFF201_MODEL)
        .write("pkg/store.py", EFF201_STORE)
        .write("pkg/render_mod.py", EFF201_RENDER);
    let diags = diagnostics(&p, &["--config", "heki.toml", "--select", "EFF201"]);
    let r = "pkg/render_mod.py";
    let (w, mp) = ("pure-render:DataModel.fields", "pure-render:mutates-params");
    // Not reported: `allowed` (its only write and mutation happen in the allowed
    // `pkg.store.save`); `local_only` (constructing a model initializes a fresh object, local
    // objects are no parameters, `name` is no target); `draw`'s parameter is untouched.
    assert_eq!(
        found(&diags),
        vec![
            // `mutates-params` at the definition, the write at the write itself.
            d(r, 6, "Renderer.direct", mp),
            d(r, 7, "Renderer.direct", w),
            // A helper mutating the field it is handed.
            d(r, 9, "Renderer.indirect", mp),
            d(r, 10, "Renderer.indirect", w),
            // Two calls deep.
            d(r, 12, "Renderer.deep", mp),
            d(r, 13, "Renderer.deep", w),
            // A subclass of the target class.
            d(r, 15, "Renderer.sub_direct", mp),
            d(r, 16, "Renderer.sub_direct", w),
            // A method writing `self.x` mutates its parameter `self`.
            d(r, 21, "Renderer.own_state", mp),
            // A parameter attribute mutated by a helper, not a write target.
            d(r, 24, "Renderer.relabel", mp),
            // A fresh object's field handed to a mutating helper.
            d(r, 37, "draw", w),
        ],
        "{diags:#?}"
    );
    let forbids = ", which effect boundary `pure-render` forbids";
    let msgs = messages(&diags);
    assert_eq!(
        msgs[0],
        format!("`Renderer.direct` mutates its parameters (`m`){forbids}")
    );
    assert_eq!(
        msgs[1],
        format!("`Renderer.direct` writes `DataModel.fields` (direct_attr){forbids}")
    );
    assert_eq!(
        msgs[3],
        format!(
            "`Renderer.indirect` writes `DataModel.fields` (indirect_helper via `pkg.model._push`){forbids}"
        )
    );
    assert_eq!(
        msgs[5],
        format!(
            "`Renderer.deep` writes `DataModel.fields` (container_mut via `pkg.model.touch`){forbids}"
        )
    );
    assert_eq!(
        related(&diags[5]),
        [
            "`Renderer.deep` calls `pkg.model.touch`",
            "`touch` calls `pkg.model.add_field`"
        ]
    );
    assert_eq!(
        msgs[8],
        format!("`Renderer.own_state` mutates its parameters (`self`){forbids}")
    );
    assert!(diags.iter().all(|x| x["fix"].is_null()));
}

// ============================================================================ configuration

#[test]
fn contract_configuration_errors_exit_2() {
    let p = Project::new("v1x_cfg_errors");
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
        &boundary("forbid = [\"blocking\", \"network\"]\n"),
        "unknown `forbid` token `network` in effect-boundaries `b` (expected one of `blocking`, `blocking-net`, `blocking-db`, `blocking-fs`, `blocking-subprocess`, `blocking-sync`, `sleep`, `mutates-params`, `writes-targets`, `reads-targets`, `calls`)",
    );
    fails(
        &boundary("forbid = [\"writes-targets\"]\n"),
        "effect-boundaries `b` forbids `writes-targets` but sets no `write-targets`",
    );
    fails(
        &boundary("forbid = [\"writes-targets\"]\nwrite-targets = [\"fields\"]\n"),
        "invalid effect-boundaries.write-targets target `fields` (expected `Class.attr`)",
    );
    fails(
        &boundary("forbid = [\"writes-targets\"]\nwrite-targets = [\"Model.[x\"]\n"),
        "invalid glob `[x` in effect-boundaries.write-targets",
    );
    fails(
        "[[tool.heki.effect-boundaries]]\nname = \"b\"\nscopes = [\"a.[b\"]\nforbid = [\"sleep\"]\n",
        "invalid glob `a.[b` in effect-boundaries.scopes",
    );
    fails(
        &boundary("forbid = [\"sleep\"]\nallowed-callees = [\"c.[d\"]\n"),
        "invalid glob `c.[d` in effect-boundaries.allowed-callees",
    );
    let readers = |rest: &str| format!("[[tool.heki.read-owners]]\nname = \"r\"\n{rest}");
    fails(
        &readers("targets = [\"children\"]\n"),
        "invalid read-owners target `children` (expected `Class.attr`)",
    );
    fails(
        &readers("targets = [\"Ref.[w\"]\n"),
        "invalid glob `[w` in read-owners.targets",
    );
    fails(
        &readers("targets = [\"Ref.children\"]\nallowed-readers = [\"x.[y\"]\n"),
        "invalid glob `x.[y` in read-owners.allowed-readers",
    );
    fails(
        &readers("targets = [\"Ref.children\"]\nexempt-methods = [\"Ref.[z\"]\n"),
        "invalid glob `Ref.[z` in read-owners.exempt-methods",
    );
    fails(
        "[[tool.heki.write-owners]]\nname = \"w\"\ntargets = [\"Model.fields\"]\nexempt-methods = [\"[bad\"]\n",
        "invalid glob `[bad` in write-owners.exempt-methods",
    );
    // A contract whose rules do not run (`MUT203`, and its strict mode `MUT206`) is not
    // validated (as for `write-owners`).
    p.write(
        "pyproject.toml",
        &format!(
            "[project]\nname = \"t\"\n\n{}",
            readers("targets = [\"children\"]\n")
        ),
    );
    assert_eq!(p.check(&["--ignore", "MUT203,MUT206"]).0, 0);
}

const SILENT_SRC: &str = r#"import time


class Reference:
    def __init__(self):
        self.children = []


def render(r: Reference):
    time.sleep(1)
    r.children.append(1)
    return r.children
"#;

#[test]
fn contracts_not_configured_are_silent() {
    let p = Project::new("v1x_silent");
    p.write("app.py", SILENT_SRC);
    let (code, stdout, stderr) = p.check(&["--select", "MUT203,EFF201", "--output-format", "json"]);
    assert_eq!(code, 0, "{stdout}{stderr}");
    assert_eq!(stdout.trim(), "[]");
    // Configured, the same code is reported by both.
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.read-owners]]\nname = \"r\"\ntargets = [\"Reference.children\"]\n\n[[tool.heki.effect-boundaries]]\nname = \"pure\"\nscopes = [\"render\"]\nforbid = [\"sleep\", \"mutates-params\"]\n",
    );
    let diags = diagnostics(&p, &["--select", "MUT203,EFF201"]);
    assert_eq!(
        found(&diags),
        vec![
            d("app.py", 9, "render", "pure:mutates-params"),
            d("app.py", 10, "render", "pure:sleep"),
            d("app.py", 11, "render", "Reference.children"),
            d("app.py", 12, "render", "Reference.children"),
        ],
        "{diags:#?}"
    );
}

// ================================================================================== baseline

const BASELINE_CONFIG: &str = r#"[project]
name = "t"

[[tool.heki.read-owners]]
name = "children"
targets = ["Reference.children"]

[[tool.heki.effect-boundaries]]
name = "pure"
scopes = ["pkg.views"]
forbid = ["blocking"]
"#;

const BASELINE_VIEWS: &str = r#"import time

from pkg.model import Reference


def _kids(r: Reference):
    return r.children


def show(r: Reference):
    time.sleep(1)
    return _kids(r)
"#;

#[test]
fn contract_baselines_round_trip() {
    let p = Project::new("v1x_baseline");
    p.write("pyproject.toml", BASELINE_CONFIG)
        .write("pkg/__init__.py", "")
        .write(
            "pkg/model.py",
            "class Reference:\n    def __init__(self):\n        self.children = []\n",
        )
        .write("pkg/views.py", BASELINE_VIEWS);
    let args = [
        "--select",
        "MUT203,EFF201",
        "--baseline",
        "heki-baseline.toml",
    ];
    let mut update = args.to_vec();
    update.push("--update-baseline");
    let (code, _, stderr) = p.check(&update);
    assert_eq!(code, 0, "{stderr}");
    let text = p.read("heki-baseline.toml");
    for needle in [
        "rule_code = \"MUT203\"",
        "target = \"Reference.children\"",
        "qualname = \"show\"",
        "rule_code = \"EFF201\"",
        "target = \"pure:sleep\"",
    ] {
        assert!(text.contains(needle), "{needle}: {text}");
    }
    assert_eq!(p.check(&args).0, 0);
    // Moving code does not matter.
    p.write("pkg/views.py", &format!("# moved\n\n{BASELINE_VIEWS}"));
    assert_eq!(p.check(&args).0, 0);
    // A new read through the helper and a new blocking call are regressions; only they are
    // reported.
    p.write(
        "pkg/views.py",
        &format!(
            "{BASELINE_VIEWS}\n\ndef again(r: Reference):\n    time.sleep(2)\n    return _kids(r)\n"
        ),
    );
    let mut concise = args.to_vec();
    concise.extend(["--output-format", "concise"]);
    let (code, stdout, _) = p.check(&concise);
    assert_eq!(code, 1, "{stdout}");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "{stdout}");
    assert!(
        lines[0].starts_with("pkg/views.py:16:5: EFF201 "),
        "{stdout}"
    );
    assert!(
        lines[1].starts_with("pkg/views.py:17:12: MUT203 "),
        "{stdout}"
    );
}

// ========================================================== inside the owners' operations

#[test]
fn functions_that_only_run_inside_allowed_contexts_are_not_reported() {
    // dcg `DataModel.set_base_class`: a public method writing its own field that only the class
    // initializer (on `self`) and the store call runs inside the owner's operations.
    let p = Project::new("v1x_inside_owners");
    p.write(
        "pyproject.toml",
        r#"[project]
name = "t"

[[tool.heki.write-owners]]
name = "store"
targets = ["Model.bases"]
allowed-writers = ["Store.*"]

[[tool.heki.read-owners]]
name = "index"
targets = ["Ref.children"]
allowed-readers = ["Index.*"]
"#,
    )
    .write("pkg/__init__.py", "")
    .write(
        "pkg/model.py",
        r#"class Model:
    def __init__(self):
        self.bases = []
        self.set_bases()

    def set_bases(self):
        self.bases = ["object"]

    def reorder(self):
        self.bases.reverse()

    def spin(self, n):
        self.bases = []
        if n:
            self.spin(n - 1)


class Store:
    def reset(self, model: Model):
        model.set_bases()
        model.reorder()


def rebuild(model: Model):
    model.reorder()


class Ref:
    def __init__(self):
        self.children = []

    def kids(self):
        return list(self.children)

    def count(self):
        return len(self.children)


class Index:
    def build(self, ref: Ref):
        return ref.kids() + [ref.count()]


def render(ref: Ref):
    return ref.count()
"#,
    );
    let diags = diagnostics(&p, &[]);
    assert_eq!(
        found(&diags),
        vec![
            // `reorder` also runs from `rebuild`, which nothing calls.
            d("pkg/model.py", 10, "Model.reorder", "Model.bases"),
            // `spin` only calls itself: no allowed context.
            d("pkg/model.py", 13, "Model.spin", "Model.bases"),
            // `count` also runs from `render`.
            d("pkg/model.py", 36, "Ref.count", "Ref.children"),
        ],
        "{diags:#?}"
    );
}

#[test]
fn inside_allowed_contexts_needs_callers_and_every_path_allowed() {
    // Edge cases of the "only runs inside allowed contexts" exclusion, for writes and reads:
    // a function without callers, a cycle of functions outside every allowed context, and
    // allowed and non-allowed callers mixed (`Store` and `Parser`).
    let p = Project::new("v1x_inside_owners_edges");
    p.write(
        "pyproject.toml",
        r#"[project]
name = "t"

[[tool.heki.write-owners]]
name = "store"
targets = ["Model.bases"]
allowed-writers = ["Store.*"]

[[tool.heki.read-owners]]
name = "index"
targets = ["Model.bases"]
allowed-readers = ["Store.*"]
"#,
    )
    .write("pkg/__init__.py", "")
    .write(
        "pkg/model.py",
        r#"class Model:
    def __init__(self):
        self.bases = []

    def orphan(self):
        self.bases = []

    def touch(self):
        self.bases.append(1)

    def peek(self):
        return self.bases


def ping(model: Model, n):
    model.bases = []
    if n:
        pong(model, n - 1)


def pong(model: Model, n):
    ping(model, n)


def tick(model: Model, n):
    model.bases = []
    if n:
        tock(model, n - 1)


def tock(model: Model, n):
    tick(model, n)


class Store:
    def save(self, model: Model):
        model.touch()
        tick(model, 2)
        Builder(model)
        return model.peek()


class Parser:
    def parse(self, model: Model):
        model.touch()
        return model.peek()


def stamp(model: Model):
    model.bases = ["x"]


class Builder:
    def __init__(self, model: Model):
        stamp(model)
"#,
    );
    let diags = diagnostics(&p, &[]);
    let w = "Model.bases";
    assert_eq!(
        found(&diags),
        vec![
            // No callers at all: never excluded (no vacuous "every caller is allowed").
            d("pkg/model.py", 6, "Model.orphan", w),
            // `Store.save` and `Parser.parse` both call them: `Parser.parse` is no allowed
            // context, so the writes and reads stay reported (`self.bases.append` is both).
            d("pkg/model.py", 9, "Model.touch", w),
            d("pkg/model.py", 9, "Model.touch", w),
            d("pkg/model.py", 12, "Model.peek", w),
            // `ping` <-> `pong` with no allowed caller anywhere on the cycle.
            d("pkg/model.py", 16, "ping", w),
            // `tick` <-> `tock` is entered only from `Store.save`: inside the store's
            // operation, so not reported.
            // `stamp` runs from `Builder.__init__`: constructor calls also bind subclass
            // initializers (CHA), so the walk does not trust them and keeps the report.
            d("pkg/model.py", 50, "stamp", w),
        ],
        "{diags:#?}"
    );
    assert_eq!(
        diags
            .iter()
            .map(|x| x["rule_code"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["MUT201", "MUT201", "MUT203", "MUT203", "MUT201", "MUT201"],
        "{diags:#?}"
    );
}

#[test]
fn comprehension_scopes_hand_their_reports_to_their_callers() {
    // Python 3.11 lowers a comprehension to its own scope (`_all.<locals>.<listcomp>`): it is
    // no API of its own, so a private writer reached through it is reported at the outermost
    // public caller (dcg `_copy_data_type`'s recursive list comprehension).
    let p = Project::new("v1x_comprehension_scopes");
    p.write(
        "pyproject.toml",
        r#"[project]
name = "t"
requires-python = ">=3.11"

[[tool.heki.write-owners]]
name = "store"
targets = ["Model.bases"]
allowed-writers = ["Store.*"]
"#,
    )
    .write("pkg/__init__.py", "")
    .write(
        "pkg/model.py",
        r#"class Model:
    def __init__(self):
        self.bases = []


def _write(m: Model):
    m.bases = []


def _all(models: list[Model]):
    return [_write(m) for m in models]


def run(models: list[Model]):
    _all(models)
"#,
    );
    let diags = diagnostics(&p, &["--select", "MUT201"]);
    assert_eq!(
        found(&diags),
        vec![d("pkg/model.py", 15, "run", "Model.bases")],
        "{diags:#?}"
    );
}

// ================================================================================ heki rule

#[test]
fn contract_rule_docs() {
    let p = Project::new("v1x_rule_docs");
    for (code, slug, option) in [
        ("MUT203", "read-owner-violation", "allowed-readers"),
        ("EFF201", "effect-boundary-violation", "allowed-callees"),
        ("MUT201", "write-owner-violation", "exempt-methods"),
    ] {
        let (status, doc, _) = p.heki(&["rule", code]);
        assert_eq!(status, 0);
        assert!(doc.starts_with(&format!("# {slug} ({code})\n\n")), "{doc}");
        let at = |h: &str| doc.find(h).unwrap_or_else(|| panic!("{code}: no {h}"));
        assert!(
            at("## What it does\n") < at("## Why is this bad?\n")
                && at("## Why is this bad?\n") < at("## Example\n")
                && at("## Example\n") < at("Use instead:\n")
                && at("Use instead:\n") < at("## Fix safety\nNo automatic fix.\n")
                && at("## Fix safety\n") < at("## Options\n"),
            "{doc}"
        );
        assert!(doc.contains(option), "{doc}");
    }
    let (_, doc, _) = p.heki(&["rule", "MUT201"]);
    assert!(doc.contains("`message`"), "{doc}");
}

/// A scope function calling a private helper that writes a target gets the write from the
/// helper's node (one report, with the helper on the chain).
#[test]
fn eff201_reports_a_write_a_private_helper_hands_over_once() {
    let p = Project::new("v1x_eff201_private_helper");
    p.write("heki.toml", EFF201_WRITES_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", EFF201_MODEL)
        .write("pkg/store.py", EFF201_STORE)
        .write(
            "pkg/render_mod.py",
            "from pkg.model import DataModel\n\n\ndef _reset(m: DataModel):\n    m.fields = []\n\n\nclass Renderer:\n    def via_private(self, m: DataModel):\n        _reset(m)\n",
        );
    let diags = diagnostics(&p, &["--config", "heki.toml", "--select", "EFF201"]);
    let r = "pkg/render_mod.py";
    assert_eq!(
        found(&diags),
        vec![
            d(r, 9, "Renderer.via_private", "pure-render:mutates-params"),
            d(
                r,
                10,
                "Renderer.via_private",
                "pure-render:DataModel.fields"
            ),
        ],
        "{diags:#?}"
    );
    assert!(messages(&diags)[1].contains("(direct_attr via `pkg.render_mod._reset`)"));
}

/// A scope function's work on a worker thread is not its own (the sleep there blocks no scope),
/// and iterating the generator a helper returns runs it in the scope: the parameter the
/// generator drains is the scope's.
#[test]
fn eff201_follows_where_offloaded_callables_and_returned_generators_run() {
    let p = Project::new("v1x_eff201_runs");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.effect-boundaries]]\nname = \"domain\"\nscopes = [\"pkg.views.*\"]\nforbid = [\"sleep\", \"mutates-params\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write(
        "pkg/helpers.py",
        "import time\n\n\ndef nap():\n    time.sleep(1)\n\n\ndef _drain(items):\n    while items:\n        yield items.pop()\n\n\ndef drained(items):\n    return _drain(items)\n",
    )
    .write(
        "pkg/views.py",
        "import asyncio\n\nfrom pkg.helpers import drained, nap\n\n\nasync def offload():\n    await asyncio.to_thread(nap)\n\n\ndef show(items):\n    for x in drained(items):\n        print(x)\n",
    );
    let diags = diagnostics(&p, &["--select", "EFF201"]);
    assert_eq!(
        found(&diags),
        vec![d("pkg/views.py", 10, "show", "domain:mutates-params")],
        "{diags:#?}"
    );
}
