//! `include-overrides`, end to end through the real binary: with the key, an entry naming a
//! method (`Class.method`, `module:Class.method`) in a key that names places or callees also
//! names the methods of that name in the project subclasses of `Class`, wherever they are
//! defined; a key covering nested scopes (`scopes`, an entry ending in `{,.*}`) covers the
//! overrides' nested scopes too, and a private name overrides nothing. The reports equal those
//! of the same contract listing the overrides by hand, and without the key the overrides are
//! judged as any other function.

mod common;

use common::Project;
use serde_json::Value;

/// `heki check` on `files` (under `pkg/`) with the `[tool.heki]` tables `config`: the exit
/// code, the reports as JSON, and the standard error.
fn run(files: &[(&str, &str)], config: &str) -> (i32, Vec<Value>, String) {
    let p = Project::new("include_overrides");
    p.write(
        "pyproject.toml",
        &format!("[project]\nname = \"t\"\n\n{config}"),
    )
    .write("pkg/__init__.py", "");
    for (rel, text) in files {
        p.write(rel, text);
    }
    let (code, stdout, stderr) = p.check(&["--output-format", "json"]);
    assert!(code <= 1, "{stdout}{stderr}");
    (code, serde_json::from_str(&stdout).unwrap(), stderr)
}

/// The reports of [`run`] as `path:line:column: code message`.
fn reported(files: &[(&str, &str)], config: &str) -> Vec<String> {
    let (_, diags, stderr) = run(files, config);
    assert!(stderr.is_empty(), "{stderr}");
    diags
        .iter()
        .map(|d| {
            format!(
                "{}:{}:{}: {} {}",
                d["path"].as_str().unwrap(),
                d["line"],
                d["column"],
                d["rule_code"].as_str().unwrap(),
                d["message"].as_str().unwrap()
            )
        })
        .collect()
}

/// The reports with `key` (`include-overrides = true`) put in place of `KEY` in `config`, and
/// those of `by_hand`, the same contracts listing the overrides themselves, are the same, help
/// included; returns the reports.
fn same_as_by_hand(files: &[(&str, &str)], config: &str, by_hand: &str) -> Vec<String> {
    let with_key = config.replace("KEY", "include-overrides = true");
    let (_, keyed, stderr) = run(files, &with_key);
    assert!(stderr.is_empty(), "{stderr}");
    let (_, listed, _) = run(files, by_hand);
    assert_eq!(keyed, listed);
    reported(files, &with_key)
}

const MODEL: &str = r#"class Box:
    def __init__(self) -> None:
        self.items: list[int] = []
        self.secret = 0
        self.count = 0


class Base:
    def save(self, box: Box) -> None:
        box.items.append(1)

    def render(self, box: Box) -> int:
        return len(box.items)

    def __run(self, box: Box) -> None:
        box.items.append(2)

    def copy(self, box: Box) -> int:
        return box.secret
"#;

// A subclass in another module: an override writing itself and through a helper only it calls,
// an override inside the forbidden writers, a private method of the same name, and an
// exempt method's override. `Other.save` is no override.
const SUB: &str = r#"from pkg.model import Base, Box


class Sub(Base):
    def save(self, box: Box) -> None:
        box.items.append(3)
        _fill(box)

    def render(self, box: Box) -> int:
        box.count = 4
        return 0

    def __run(self, box: Box) -> None:
        box.items.append(5)

    def copy(self, box: Box) -> int:
        return box.secret


class Other:
    def save(self, box: Box) -> None:
        box.items.append(6)


def _fill(box: Box) -> None:
    box.items.append(7)
"#;

const OWNERS: &str = r#"[tool.heki.lint]
select = ["MUT201", "MUT203"]

[[tool.heki.write-owners]]
name = "items"
targets = ["Box.items"]
allowed-writers = ["pkg.model.Base.save", "Base.__run"]
KEY

[[tool.heki.write-owners]]
name = "render"
targets = ["Box.count"]
forbidden-writers = ["Base.render"]
KEY

[[tool.heki.read-owners]]
name = "secret"
targets = ["Box.secret"]
exempt-methods = ["Base.copy"]
KEY
"#;

#[test]
fn owner_entries_name_the_overrides_of_their_methods() {
    let files = [("pkg/model.py", MODEL), ("pkg/sub.py", SUB)];
    let sub_run = "pkg/sub.py:14:9: MUT201 `Sub.__run` writes `Box.items` (in place) outside the write owners of `items`";
    let other = "pkg/sub.py:22:9: MUT201 `Other.save` writes `Box.items` (in place) outside the write owners of `items`";
    // Without the key the overrides are functions like any other.
    let off = reported(&files, &OWNERS.replace("KEY", ""));
    assert_eq!(
        off,
        [
            "pkg/sub.py:6:9: MUT201 `Sub.save` writes `Box.items` (in place) outside the write owners of `items`",
            "pkg/sub.py:7:9: MUT201 `Sub.save` writes `Box.items` (in place) via `pkg.sub._fill` outside the write owners of `items`",
            sub_run,
            "pkg/sub.py:17:16: MUT203 `Sub.copy` reads `Box.secret` (by attribute) outside the read owners of `secret`",
            other,
        ]
    );
    assert_eq!(
        reported(&files, &OWNERS.replace("KEY", "include-overrides = false")),
        off
    );
    // With it, `Sub.save` and the helper only it calls are allowed, `Sub.render` is a forbidden
    // writer, and `Sub.copy` is exempt; the private `Sub.__run` overrides nothing.
    let by_hand = OWNERS
        .replace("KEY", "")
        .replace("\"Base.__run\"]", "\"Base.__run\", \"pkg.sub.Sub.save\"]")
        .replace("[\"Base.render\"]", "[\"Base.render\", \"Sub.render\"]")
        .replace("[\"Base.copy\"]", "[\"Base.copy\", \"Sub.copy\"]");
    assert_eq!(
        same_as_by_hand(&files, OWNERS, &by_hand),
        [
            "pkg/sub.py:10:9: MUT201 `Sub.render` writes `Box.count` (by attribute) inside the forbidden writers of `render`",
            sub_run,
            other,
        ]
    );
}

#[test]
fn the_defaults_table_sets_the_key_for_every_contract_of_its_kind() {
    let files = [("pkg/model.py", MODEL), ("pkg/sub.py", SUB)];
    let keyed = reported(&files, &OWNERS.replace("KEY", "include-overrides = true"));
    let defaults = OWNERS.replace("KEY", "").replace(
        "[[tool.heki.write-owners]]\nname = \"items\"",
        "[tool.heki.write-owners-defaults]\ninclude-overrides = true\n\n[tool.heki.read-owners-defaults]\ninclude-overrides = true\n\n[[tool.heki.write-owners]]\nname = \"items\"",
    );
    assert_eq!(reported(&files, &defaults), keyed);
}

#[test]
fn an_entry_names_only_methods_that_exist_as_before() {
    // `Base.extra` names no method of `Base`: only a subclass defines `extra`.
    let files = [
        ("pkg/model.py", MODEL),
        (
            "pkg/extra.py",
            "from pkg.model import Base\n\n\nclass Extra(Base):\n    def extra(self) -> None:\n        pass\n",
        ),
    ];
    let config = "[tool.heki.lint]\nselect = [\"MUT201\"]\nunmatched-contract-entries = \"error\"\n\n[[tool.heki.write-owners]]\nname = \"items\"\ntargets = [\"Box.items\"]\nallowed-writers = [\"Base.save\", \"Base.extra\"]\ninclude-overrides = true\n";
    let (code, _, stderr) = run(&files, config);
    assert_eq!(code, 1);
    assert!(
        stderr.contains("`allowed-writers` entry `Base.extra` of write-owners `items` names no function or method"),
        "{stderr}"
    );
}

const STORE: &str = "class Store:\n    def op(self) -> None:\n        pass\n";

const HOOKS: &str = r#"from pkg.store import Store


class Base:
    def hook(self, store: Store) -> None:
        store.op()

    def helper(self, store: Store) -> None:
        store.op()


class Sub(Base):
    def hook(self, store: Store) -> None:
        def inner() -> None:
            store.op()

        inner()


class Unrelated:
    def hook(self, store: Store) -> None:
        store.op()


class Plain:
    def other(self, store: Store) -> None:
        store.op()
"#;

// A subclass of a subclass, in a third module.
const MORE_HOOKS: &str = r#"from pkg.hooks import Sub
from pkg.store import Store


class SubSub(Sub):
    def hook(self, store: Store) -> None:
        store.op()
"#;

const HOOK: &str = r#"[tool.heki.lint]
select = ["MUT204"]

[[tool.heki.call-owners]]
name = "hook"
targets = ["pkg.store:Store.op"]
allowed-callers = ["pkg.hooks:Base.hook{,.*}"]
unresolved = "report"
KEY
"#;

#[test]
fn an_entry_ending_in_nested_scopes_covers_those_of_the_overrides() {
    let files = [
        ("pkg/store.py", STORE),
        ("pkg/hooks.py", HOOKS),
        ("pkg/more_hooks.py", MORE_HOOKS),
    ];
    let helper = "pkg/hooks.py:9:9: MUT204 `Base.helper` calls `pkg.store:Store.op` outside the call owners of `hook`";
    let unrelated = "pkg/hooks.py:22:9: MUT204 `Unrelated.hook` calls `pkg.store:Store.op` outside the call owners of `hook`";
    let plain = "pkg/hooks.py:27:9: MUT204 `Plain.other` calls `pkg.store:Store.op` outside the call owners of `hook`";
    let off = reported(&files, &HOOK.replace("KEY", ""));
    assert_eq!(
        off,
        [
            helper,
            "pkg/hooks.py:15:13: MUT204 `Sub.hook.<locals>.inner` calls `pkg.store:Store.op` outside the call owners of `hook`",
            unrelated,
            plain,
            "pkg/more_hooks.py:7:9: MUT204 `SubSub.hook` calls `pkg.store:Store.op` outside the call owners of `hook`",
        ]
    );
    let by_hand = HOOK.replace("KEY", "").replace(
        "[\"pkg.hooks:Base.hook{,.*}\"]",
        "[\"pkg.hooks:Base.hook{,.*}\", \"pkg.hooks:Sub.hook{,.*}\", \"pkg.more_hooks:SubSub.hook{,.*}\"]",
    );
    assert_eq!(
        same_as_by_hand(&files, HOOK, &by_hand),
        [helper, unrelated, plain]
    );
}

const VIEW: &str = r#"import time


class View:
    def render(self) -> None:
        time.sleep(1)

    def draw(self) -> None:
        def inner() -> None:
            time.sleep(1)

        inner()


class Pause:
    def wait(self) -> None:
        time.sleep(1)
"#;

const PAGE: &str = r#"import time

from pkg.view import Pause, View


class Page(View):
    def render(self) -> None:
        time.sleep(2)

    def draw(self) -> None:
        def inner() -> None:
            time.sleep(2)

        inner()


class Nap(Pause):
    def wait(self) -> None:
        time.sleep(2)


class Shown(View):
    def render(self, nap: Nap) -> None:
        nap.wait()


def show(nap: Nap, pause: Pause) -> None:
    pause.wait()
    nap.wait()
"#;

const BOUNDARY: &str = r#"[tool.heki.lint]
select = ["EFF201"]

[[tool.heki.effect-boundaries]]
name = "render"
scopes = ["View.render", "pkg.view:View.draw", "pkg.page:show"]
forbid = ["sleep"]
allowed-callees = ["Pause.wait"]
KEY
"#;

#[test]
fn boundary_scopes_and_allowed_callees_name_the_overrides() {
    let files = [("pkg/view.py", VIEW), ("pkg/page.py", PAGE)];
    let render = "pkg/view.py:6:9: EFF201 `View.render` reaches sleep (`time.sleep`), which effect boundary `render` forbids";
    let draw = "pkg/view.py:10:13: EFF201 `View.draw.<locals>.inner` reaches sleep (`time.sleep`), which effect boundary `render` forbids";
    assert_eq!(
        reported(&files, &BOUNDARY.replace("KEY", "")),
        [
            "pkg/page.py:28:5: EFF201 `show` reaches sleep (`time.sleep` via `pkg.page.Nap.wait`), which effect boundary `render` forbids",
            render,
            draw,
        ]
    );
    // The scopes nested in an override are in the boundary's scopes; `Nap.wait` is an allowed
    // callee, wherever it is called from.
    let by_hand = BOUNDARY
        .replace("KEY", "")
        .replace(
            "\"pkg.page:show\"]",
            "\"pkg.page:show\", \"Page.render\", \"Shown.render\", \"pkg.page:Page.draw\"]",
        )
        .replace("[\"Pause.wait\"]", "[\"Pause.wait\", \"Nap.wait\"]");
    let keyed = [
        "pkg/page.py:8:9: EFF201 `Page.render` reaches sleep (`time.sleep`), which effect boundary `render` forbids",
        "pkg/page.py:12:13: EFF201 `Page.draw.<locals>.inner` reaches sleep (`time.sleep`), which effect boundary `render` forbids",
        render,
        draw,
    ];
    assert_eq!(same_as_by_hand(&files, BOUNDARY, &by_hand), keyed);
    let defaults = BOUNDARY.replace("KEY", "").replace(
        "[[tool.heki.effect-boundaries]]",
        "[tool.heki.effect-boundaries-defaults]\ninclude-overrides = true\n\n[[tool.heki.effect-boundaries]]",
    );
    assert_eq!(reported(&files, &defaults), keyed);
}

const NODES: &str = r#"class Node:
    def __init__(self) -> None:
        self.kids: list[int] = []


class Index:
    def refresh(self) -> None:
        pass


class Builder:
    def build(self, node: Node) -> None:
        node.kids.append(0)
"#;

// An invalidator's override announcing a write, and an exempt writer's override writing itself
// and through a public helper that only it calls.
const MORE_NODES: &str = r#"from pkg.nodes import Builder, Index, Node


class SubIndex(Index):
    def refresh(self) -> None:
        pass


class FastBuilder(Builder):
    def build(self, node: Node) -> None:
        node.kids.append(1)
        add(node)


def add(node: Node) -> None:
    node.kids.append(2)


def grow(node: Node, index: SubIndex) -> None:
    node.kids.append(3)
    index.refresh()


def careless(node: Node) -> None:
    node.kids.append(4)
"#;

const INVALIDATION: &str = r#"[tool.heki.lint]
select = ["MUT208"]

[[tool.heki.invalidation]]
name = "index"
targets = ["Node.kids"]
invalidators = ["Index.refresh"]
exempt-writers = ["Builder.build"]
KEY
"#;

#[test]
fn invalidators_and_exempt_writers_name_the_overrides() {
    let files = [("pkg/nodes.py", NODES), ("pkg/more_nodes.py", MORE_NODES)];
    let careless = "pkg/more_nodes.py:25:5: MUT208 `careless` writes `Node.kids` (in place) and returns on some path without calling an invalidator of `index`";
    assert_eq!(
        reported(&files, &INVALIDATION.replace("KEY", "")),
        [
            "pkg/more_nodes.py:11:9: MUT208 `FastBuilder.build` writes `Node.kids` (in place) and returns on some path without calling an invalidator of `index`",
            "pkg/more_nodes.py:16:5: MUT208 `add` writes `Node.kids` (in place) and returns on some path without calling an invalidator of `index`",
            "pkg/more_nodes.py:20:5: MUT208 `grow` writes `Node.kids` (in place) and returns on some path without calling an invalidator of `index`",
            careless,
        ]
    );
    let by_hand = INVALIDATION
        .replace("KEY", "")
        .replace(
            "[\"Index.refresh\"]",
            "[\"Index.refresh\", \"SubIndex.refresh\"]",
        )
        .replace(
            "[\"Builder.build\"]",
            "[\"Builder.build\", \"FastBuilder.build\"]",
        );
    assert_eq!(same_as_by_hand(&files, INVALIDATION, &by_hand), [careless]);
}

// A write in a private helper reached through an allowed override and around it: the chain
// around it is reported.
const CHAINS: &str = r#"from pkg.model import Base, Box


def _put(box: Box) -> None:
    box.items.append(1)


class Sub(Base):
    def save(self, box: Box) -> None:
        _put(box)


def both(sub: Sub, box: Box) -> None:
    sub.save(box)
    _put(box)


def around(sub: Sub, box: Box) -> None:
    _put(box)
    sub.save(box)
"#;

const CHAIN_OWNER: &str = r#"[tool.heki.lint]
select = ["MUT201"]

[[tool.heki.write-owners]]
name = "items"
targets = ["Box.items"]
allowed-writers = ["Base.save", "Base.__run"]
KEY
"#;

#[test]
fn a_chain_through_an_allowed_override_is_kept_apart_from_one_around_it() {
    let files = [("pkg/model.py", MODEL), ("pkg/chains.py", CHAINS)];
    let both = "pkg/chains.py:15:5: MUT201 `both` writes `Box.items` (in place) via `pkg.chains._put` outside the write owners of `items`";
    let around = "pkg/chains.py:19:5: MUT201 `around` writes `Box.items` (in place) via `pkg.chains._put` outside the write owners of `items`";
    assert_eq!(
        reported(&files, &CHAIN_OWNER.replace("KEY", "")),
        [
            "pkg/chains.py:10:9: MUT201 `Sub.save` writes `Box.items` (in place) via `pkg.chains._put` outside the write owners of `items`",
            both,
            around,
        ]
    );
    let by_hand = CHAIN_OWNER
        .replace("KEY", "")
        .replace("\"Base.__run\"]", "\"Base.__run\", \"Sub.save\"]");
    assert_eq!(
        same_as_by_hand(&files, CHAIN_OWNER, &by_hand),
        [both, around]
    );
}
