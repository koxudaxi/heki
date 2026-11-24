//! The help of a contract report names the allowed functions that do what it shows
//! (`allowed_examples`): up to three public project functions whose own body writes, reads, or
//! calls the target from a place the contract allows, or writes an `invalidation` target and
//! announces it, by fully qualified name (end to end, through the real binary).

mod common;

use common::Project;
use serde_json::{Value, json};

fn project(name: &str, config: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!("[project]\nname = \"t\"\n\n{config}"),
    );
    p.write("pkg/__init__.py", "");
    p
}

/// Every diagnostic of `rules`.
fn diagnostics(p: &Project, rules: &str) -> Vec<Value> {
    let (code, stdout, stderr) = p.check(&["--select", rules, "--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

/// The diagnostic of `rule` in the function `qualname`.
fn of<'d>(diags: &'d [Value], rule: &str, qualname: &str) -> &'d Value {
    diags
        .iter()
        .find(|d| d["rule_code"] == rule && d["qualname"] == qualname)
        .unwrap_or_else(|| panic!("no {rule} in {qualname}: {diags:#?}"))
}

fn example(fqn: &str, path: &str) -> Value {
    json!({"fqn": fqn, "path": path})
}

const MODEL: &str = r#"class Registry:
    def __init__(self) -> None:
        self.entries: list[int] = []
        self.tags: list[str] = []
        self.name = ""


class Vault:
    def __init__(self) -> None:
        self.secret = ""


class Ledger:
    def __init__(self) -> None:
        self.balance = 0

    def post(self, x: int) -> int:
        return x


class Receipt:
    pass
"#;

const WRITE_CONFIG: &str = r#"[[tool.heki.write-owners]]
name = "registry"
targets = ["Registry.entries", "Registry.tags"]
allowed-writers = ["Store.*", "pkg.more.*"]
message = "add entries through `Store`"
unresolved = "report"

[[tool.heki.write-owners]]
name = "vault"
targets = ["Vault.secret"]
allowed-writers = ["pkg.keeper.*"]
"#;

const STORE: &str = r#"from pkg.model import Registry


def _fill(xs: list[int]) -> None:
    xs.append(1)


class Store:
    def put(self, r: Registry) -> None:
        r.name = "put"
        r.entries = [1]

    def add(self, r: Registry, x: int) -> None:
        r.entries.append(x)
        r.entries.append(x)

    def make(self) -> Registry:
        fresh = Registry()
        fresh.tags = []
        return fresh

    def fill(self, r: Registry) -> None:
        _fill(r.entries)

    def _stash(self, r: Registry) -> None:
        r.entries = []

    def __iadd__(self, r: Registry) -> "Store":
        r.entries.append(0)
        return self
"#;

const MORE: &str = r#"from pkg.model import Registry


def b_write(r: Registry) -> None:
    r.entries.clear()


def a_write(r: Registry) -> None:
    del r.entries[0]
"#;

const KEEPER: &str = r#"from pkg.model import Vault


def rotate(v: Vault) -> None:
    v.secret = "new"
"#;

const WRITE_APP: &str = r#"from pkg.model import Registry, Vault


def sneak(r: Registry) -> None:
    r.entries.append(2)


def tag(r: Registry) -> None:
    r.tags.append("x")


def steal(v: Vault) -> None:
    v.secret = ""


def untyped(r) -> None:
    r.entries = []
"#;

#[test]
fn write_owner_reports_name_the_allowed_writers_that_write_the_target() {
    let p = project("help_writers", WRITE_CONFIG);
    p.write("pkg/model.py", MODEL)
        .write("pkg/store.py", STORE)
        .write("pkg/more.py", MORE)
        .write("pkg/keeper.py", KEEPER)
        .write("pkg/app.py", WRITE_APP);
    let diags = diagnostics(&p, "MUT201,MUT205");
    // Four public allowed writers write `Registry.entries` themselves; the help names the first
    // three by fully qualified name. `Store.fill` hands the container to a helper, which is no
    // write of its own, and a private method and a special method are no function to call.
    let sneak = of(&diags, "MUT201", "sneak");
    assert_eq!(
        sneak["allowed_examples"],
        json!([
            example("pkg.more.a_write", "pkg/more.py"),
            example("pkg.more.b_write", "pkg/more.py"),
            example("pkg.store.Store.add", "pkg/store.py"),
        ])
    );
    // The contract's `message` comes first.
    assert_eq!(
        sneak["help"],
        "add entries through `Store`; allowed writers of `Registry.entries`: `pkg.more.a_write`, \
         `pkg.more.b_write`, `pkg.store.Store.add`"
    );
    // No allowed writer writes `Registry.tags` of an object that existed before (`Store.make`
    // constructs the one it writes): the help is the message alone.
    let tag = of(&diags, "MUT201", "tag");
    assert_eq!(tag["help"], "add entries through `Store`");
    assert!(tag.get("allowed_examples").is_none(), "{tag:#}");
    // Each contract names its own allowed writers; one without a `message` starts with them.
    assert_eq!(
        of(&diags, "MUT201", "steal")["help"],
        "allowed writers of `Vault.secret`: `pkg.keeper.rotate`"
    );
    // A write on an object of unknown class names the allowed writers of the targets of its
    // attribute.
    let untyped = of(&diags, "MUT205", "untyped");
    assert_eq!(
        untyped["allowed_examples"], sneak["allowed_examples"],
        "{untyped:#}"
    );
    assert!(
        untyped["help"]
            .as_str()
            .unwrap()
            .starts_with("add entries through `Store`; allowed writers of `entries`: `pkg.more"),
        "{untyped:#}"
    );
    // The full output prints the help line.
    let (_, full, _) = p.check(&["--select", "MUT201"]);
    assert!(
        full.contains("  = help: allowed writers of `Vault.secret`: `pkg.keeper.rotate`\n"),
        "{full}"
    );
}

const READ_CONFIG: &str = r#"[[tool.heki.read-owners]]
name = "vault"
targets = ["Vault.secret"]
allowed-readers = ["Store.*"]
unresolved = "report"

[[tool.heki.read-owners]]
name = "layer"
targets = ["Ledger.balance"]
forbidden-readers = ["pkg.layer.*"]
"#;

const READ_STORE: &str = r#"from pkg.model import Ledger, Vault


class Store:
    def peek(self, v: Vault) -> str:
        return v.secret


def audit(ledger: Ledger) -> int:
    return ledger.balance
"#;

const LAYER: &str = r#"from pkg.model import Ledger, Vault


def view(ledger: Ledger) -> int:
    return ledger.balance


def leak(v: Vault) -> str:
    return v.secret


def untyped(v) -> str:
    return v.secret
"#;

#[test]
fn read_owner_reports_name_the_allowed_readers_that_read_the_target() {
    let p = project("help_readers", READ_CONFIG);
    p.write("pkg/model.py", MODEL)
        .write("pkg/store.py", READ_STORE)
        .write("pkg/layer.py", LAYER);
    let diags = diagnostics(&p, "MUT203,MUT206");
    assert_eq!(
        of(&diags, "MUT203", "leak")["help"],
        "allowed readers of `Vault.secret`: `pkg.store.Store.peek`"
    );
    // With `forbidden-readers`, every function outside them reads from an allowed place.
    assert_eq!(
        of(&diags, "MUT203", "view")["allowed_examples"],
        json!([example("pkg.store.audit", "pkg/store.py")])
    );
    assert_eq!(
        of(&diags, "MUT206", "untyped")["help"],
        "allowed readers of `secret`: `pkg.store.Store.peek`"
    );
}

const CALL_CONFIG: &str = r#"[[tool.heki.call-owners]]
name = "ledger"
targets = ["Ledger.post"]
allowed-callers = ["Store.*"]
unresolved = "report"

[[tool.heki.call-owners]]
name = "receipt"
targets = ["Receipt"]
allowed-callers = ["pkg.billing.*"]
unresolved = "report"
"#;

const CALL_STORE: &str = r#"from pkg.model import Ledger, Receipt


class Store:
    def record(self, ledger: Ledger, x: int) -> Receipt:
        ledger.post(x)
        return Receipt()
"#;

const BILLING: &str = r#"from pkg.model import Ledger, Receipt


def bill(ledger: Ledger) -> Receipt:
    ledger.post(1)
    return Receipt()
"#;

const CALL_APP: &str = r#"from pkg.model import Ledger


def post(ledger: Ledger) -> None:
    ledger.post(1)


def untyped(ledger) -> None:
    ledger.post(1)
"#;

#[test]
fn call_owner_reports_name_the_allowed_callers_that_make_the_call() {
    let p = project("help_callers", CALL_CONFIG);
    p.write("pkg/model.py", MODEL)
        .write("pkg/store.py", CALL_STORE)
        .write("pkg/billing.py", BILLING)
        .write("pkg/app.py", CALL_APP);
    let diags = diagnostics(&p, "MUT204,MUT207");
    assert_eq!(
        of(&diags, "MUT204", "post")["help"],
        "allowed callers of `Ledger.post`: `pkg.store.Store.record`"
    );
    // `bill` constructs `Receipt` from where its contract allows, and `Store.record` from
    // outside it.
    assert_eq!(
        of(&diags, "MUT204", "Store.record")["allowed_examples"],
        json!([example("pkg.billing.bill", "pkg/billing.py")])
    );
    assert_eq!(
        of(&diags, "MUT207", "untyped")["help"],
        "allowed callers of `post`: `pkg.store.Store.record`"
    );
}

const INVALIDATION_CONFIG: &str = r#"[[tool.heki.invalidation]]
name = "index"
targets = ["Node.kids"]
invalidators = ["Index.refresh"]
exempt-writers = ["pkg.tree.Builder.*"]
message = "tell the index"
"#;

const TREE: &str = r#"class Node:
    def __init__(self) -> None:
        self.kids: list[int] = []
        self.name = ""


class Index:
    def refresh(self) -> None:
        pass


class Builder:
    def graft(self, node: Node, kid: int) -> None:
        node.name = "grafted"
        node.kids.append(kid)


def attach(index: Index, node: Node, kid: int) -> None:
    node.kids.append(kid)
    index.refresh()


def _quiet(node: Node, kid: int) -> None:
    node.kids.append(kid)


def careless(node: Node, kid: int) -> None:
    node.kids.append(kid)


def through(node: Node, kid: int) -> None:
    _quiet(node, kid)


def twice(index: Index, node: Node, kid: int) -> None:
    node.kids.append(kid)
    index.refresh()
    node.kids.append(kid)


def relayed(index: Index, node: Node, kid: int) -> None:
    node.kids.append(kid)
    index.refresh()
    _quiet(node, kid)
"#;

#[test]
fn invalidation_reports_name_the_writers_that_announce_the_change() {
    let p = project("help_announcers", INVALIDATION_CONFIG);
    p.write("pkg/tree.py", TREE);
    let diags = diagnostics(&p, "MUT208");
    // An exempt writer and a function announcing its write on every path; the writes no path
    // announces (`careless`, the private `_quiet`) are not named, nor the functions that announce
    // one write of the target and leave another, their own (`twice`) or a helper's (`relayed`),
    // unannounced: calling them leaves the change unannounced.
    let named = json!([
        example("pkg.tree.Builder.graft", "pkg/tree.py"),
        example("pkg.tree.attach", "pkg/tree.py"),
    ]);
    for qualname in ["careless", "through", "twice", "relayed"] {
        let d = of(&diags, "MUT208", qualname);
        assert_eq!(d["allowed_examples"], named, "{d:#}");
    }
    assert_eq!(
        of(&diags, "MUT208", "careless")["help"],
        "tell the index; announcing writers of `Node.kids`: `pkg.tree.Builder.graft`, \
         `pkg.tree.attach`"
    );
}
