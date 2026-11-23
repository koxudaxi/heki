//! `heki recommend` (experimental), contract candidates end to end: the `write-owners`,
//! `read-owners`, and `effect-boundaries` entries it draws from the census, each kept only when
//! a trial of its rules reports nothing, their confidence, the candidates the trial rejects, what
//! the configuration already declares, the cap, and that `heki check` with the block applied
//! prints what the section promised.

mod common;

use common::{Project, schema};
use serde_json::{Value, json};

const MODELS: &str = r#"from typing import Protocol


class Order:
    limit = 10

    def __init__(self, oid: int) -> None:
        self.oid = oid
        self.items: list[str] = []
        self.notes: list[str] = []
        self.tags: list[str] = []
        self.status = "new"
        self.kind = "plain"
        self._cache: dict[str, int] = {}
        self._seen = 0
        self._hits = 0
        self._level = 0
        self._debug = False

    def lookup(self, key: str) -> int:
        return self._cache.get(key, 0)

    def hit(self) -> int:
        return self._hits

    def level(self) -> int:
        return self._level


class Cart:
    def __init__(self) -> None:
        self.tags: list[str] = []


class Base:
    def __init__(self) -> None:
        self.x = 0


class Sub(Base):
    pass


def _peek(order: Order) -> int:
    return order._seen


class Named(Protocol):
    status: str
"#;

/// The owner of `Order.items`, `Order.notes`, `Order.tags`, `Cart.tags`, and `Base.x`.
const STORE: &str = r#"from app.models import Base, Cart, Order, Sub


class Store:
    def add_item(self, order: Order, item: str) -> None:
        order.items.append(item)

    def clear(self, order: Order) -> None:
        order.items.clear()

    def note(self, order: Order, text: str) -> None:
        order.notes.append(text)

    def tag(self, order: Order, tag: str) -> None:
        order.tags.append(tag)

    def cart_tag(self, cart: Cart, tag: str) -> None:
        cart.tags.append(tag)

    def close(self, order: Order) -> None:
        order.status = "closed"

    def set_base(self, b: Base) -> None:
        b.x = 1

    def set_sub(self, s: Sub) -> None:
        s.x = 2


def make(oid: int) -> Order:
    order = Order(oid)
    order.kind = "made"
    return order
"#;

const VIEWS: &str = r#"from app.models import Base, Cart, Named, Order, Sub, _peek


def show(order: Order, cart: Cart, b: Base, s: Sub) -> str:
    return f"{order.oid} {order.items} {order.notes} {order.tags} {order.kind} {order.limit} {order._debug} {cart.tags} {b.x} {s.x} {_peek(order)}"


def label(n: Named) -> str:
    return n.status
"#;

const API: &str = r#"from app.models import Base, Cart, Order, Sub


def summary(order: Order, cart: Cart, b: Base, s: Sub) -> list[object]:
    return [order.oid, order.items, order.notes, order.tags, order.status, order.kind, order.limit, cart.tags, b.x, s.x]
"#;

/// A write of `Order.status` outside the store, a helper changing `Order.notes` in place, and
/// accesses of `tags` and `_hits` on objects of unknown class.
const ADMIN: &str = r#"from app.models import Order
from app.util import push


def force(order: Order) -> None:
    order.status = "forced"


def sneak(order: Order) -> None:
    push(order.notes, "x")


def retag(o) -> None:
    o.tags = []


def peek(o) -> int:
    return o._hits
"#;

const UTIL: &str = "def push(xs: list[str], x: str) -> None:\n    xs.append(x)\n";

/// A subclass of `Order` reading `_level` in a module of its own.
const RUSH: &str = r#"from app.models import Order


class Rush(Order):
    def speed(self) -> int:
        return self._level
"#;

/// A project whose classes keep ownership contracts, and some that do not.
fn owners(name: &str) -> Project {
    let p = Project::new(name);
    p.write("app/__init__.py", "")
        .write("app/models.py", MODELS)
        .write("app/store.py", STORE)
        .write("app/views.py", VIEWS)
        .write("app/api.py", API)
        .write("app/admin.py", ADMIN)
        .write("app/util.py", UTIL)
        .write("app/rush.py", RUSH);
    p
}

/// A package with an `__init__.py`, a namespace package, a pure module, a module calling a
/// library name the catalog lacks, one reaching `requests.get`, and tests.
fn layers(name: &str) -> Project {
    let p = Project::new(name);
    p.write("app/__init__.py", "")
        .write("app/domain/__init__.py", "def rate() -> int:\n    return 3\n")
        .write(
            "app/domain/rules.py",
            "from app.domain import rate\n\n\ndef apply(x: int) -> int:\n    return x * rate()\n\n\n\
             def check(x: int) -> bool:\n    return apply(x) > 0\n",
        )
        .write(
            "app/pricing.py",
            "def subtotal(prices: list[int]) -> int:\n    return sum(prices)\n\n\n\
             def tax(amount: int) -> int:\n    return amount // 10\n\n\n\
             def total(prices: list[int]) -> int:\n    return subtotal(prices) + tax(subtotal(prices))\n",
        )
        .write(
            "app/payments.py",
            "import stripe\n\n\ndef charge(amount: int) -> None:\n    stripe.Charge.create(amount=amount)\n\n\n\
             def refund(amount: int) -> None:\n    charge(-amount)\n\n\n\
             def fee(amount: int) -> int:\n    return amount // 100\n",
        )
        .write(
            "app/http.py",
            "import requests\n\nfrom app.pricing import total\n\n\n\
             def fetch(url: str) -> str:\n    return requests.get(url, timeout=5).text\n\n\n\
             def post(url: str) -> str:\n    return fetch(url)\n\n\n\
             def priced(url: str) -> int:\n    return total([len(fetch(url))])\n",
        )
        .write(
            "lib/a.py",
            "def one() -> int:\n    return 1\n\n\ndef two() -> int:\n    return 2\n",
        )
        .write("lib/b.py", "def three() -> int:\n    return 3\n")
        .write(
            "tests/test_pricing.py",
            "from app.pricing import total\n\n\ndef test_total() -> None:\n    assert total([1]) == 1\n\n\n\
             def test_more() -> None:\n    assert total([2]) == 2\n\n\n\
             def test_none() -> None:\n    assert total([]) == 0\n",
        );
    p
}

/// `heki recommend --output-format json <args>`, which must follow the published schema.
fn recommend_json(p: &Project, args: &[&str]) -> Value {
    let mut all = vec!["recommend", "--output-format", "json"];
    all.extend_from_slice(args);
    let (code, out, err) = p.heki(&all);
    assert_eq!(code, 0, "{err}");
    let report: Value = serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}{err}"));
    let (_, schema_text, _) = p.heki(&["recommend", "--output-format-json-schema"]);
    let published: Value = serde_json::from_str(&schema_text).unwrap();
    let errors = schema::errors(&published, &report);
    assert!(errors.is_empty(), "{errors:#?}\n{report:#}");
    report
}

/// The diagnostics `heki check --output-format json <args>` prints, of a run that checked every
/// file (exit code 0 or 1, not 2).
fn checked(p: &Project, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["check", "--output-format", "json"];
    all.extend_from_slice(args);
    let (code, out, err) = p.heki(&all);
    assert!(code == 0 || code == 1, "exit code {code}: {err}");
    serde_json::from_str::<Vec<Value>>(&out).unwrap_or_else(|e| panic!("{e}: {out}{err}"))
}

/// `(kind, target, confidence, unresolved)` of each candidate the block declares.
fn kept(report: &Value) -> Vec<(String, String, String, Value)> {
    let text = |v: &Value| v.as_str().unwrap().to_owned();
    report["contracts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                text(&c["kind"]),
                text(&c["target"]),
                text(&c["confidence"]),
                c["entry"]["unresolved"].clone(),
            )
        })
        .collect()
}

/// The candidate of `kind` on `target`.
fn candidate<'r>(report: &'r Value, kind: &str, target: &str) -> &'r Value {
    report["contracts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == kind && c["target"] == target)
        .unwrap_or_else(|| panic!("no {kind} {target}: {report:#}"))
}

/// The text of the contract candidates section.
fn section(out: &str) -> &str {
    let start = out.find("\nContract candidates\n").unwrap();
    let rest = &out[start + 1..];
    &rest[..rest.find("\n\n").unwrap_or(rest.len())]
}

fn s(text: &str) -> String {
    text.to_owned()
}

#[test]
fn ownership_candidates_follow_the_census_and_the_trial() {
    let p = owners("recommend_contract_owners");
    let report = recommend_json(&p, &["--ignore", "EFF201"]);
    let report_unresolved = json!("report");
    let ignore_unresolved = json!("ignore");
    assert_eq!(
        kept(&report),
        [
            // Changed in place first, then by audience.
            (
                s("write-owners"),
                s("app.models:Cart.tags"),
                s("medium"),
                ignore_unresolved.clone()
            ),
            (
                s("write-owners"),
                s("app.models:Order.items"),
                s("high"),
                report_unresolved.clone()
            ),
            (
                s("write-owners"),
                s("app.models:Order.tags"),
                s("medium"),
                ignore_unresolved.clone()
            ),
            // `Sub.x` is left out: the target on `Base` covers its subclasses.
            (
                s("write-owners"),
                s("app.models:Base.x"),
                s("high"),
                report_unresolved.clone()
            ),
            // Written only while its objects are constructed, a factory filling the object it
            // constructs included.
            (
                s("write-owners"),
                s("app.models:Order.kind"),
                s("high"),
                report_unresolved.clone()
            ),
            (
                s("write-owners"),
                s("app.models:Order.oid"),
                s("high"),
                report_unresolved.clone()
            ),
            (
                s("read-owners"),
                s("app.models:Order._cache"),
                s("high"),
                report_unresolved.clone()
            ),
            (
                s("read-owners"),
                s("app.models:Order._hits"),
                s("medium"),
                ignore_unresolved.clone()
            ),
            // A private helper of the module reading it for its callers is allowed where they
            // call it.
            (
                s("read-owners"),
                s("app.models:Order._seen"),
                s("high"),
                report_unresolved.clone()
            ),
        ],
        "{report:#}"
    );
    let items = candidate(&report, "write-owners", "app.models:Order.items");
    assert_eq!(
        items["entry"],
        json!({"name": "Order.items writers", "targets": ["app.models:Order.items"],
               "allowed-writers": ["app.store:Store.*"], "unresolved": "report"})
    );
    assert_eq!(
        items["evidence"],
        json!({
            "functions": {"count": 2, "items": ["app.store.Store.add_item", "app.store.Store.clear"],
                          "truncated": false},
            "modules": {"count": 1, "items": ["app.store"], "truncated": false},
            "audience": {"count": 2, "items": ["app.api", "app.views"], "truncated": false},
            "in_place": true,
            "unseen": {"count": 0, "items": [], "truncated": false},
        })
    );
    assert_eq!(
        items["trial"],
        json!({"reports": {"MUT201": 0, "MUT205": 0}, "suppressed": 0, "examples": []})
    );
    // An access of `tags` on an object of unknown class: either class's, so both contracts
    // count it.
    for target in ["app.models:Order.tags", "app.models:Cart.tags"] {
        let tags = candidate(&report, "write-owners", target);
        assert_eq!(tags["trial"]["reports"], json!({"MUT201": 0, "MUT205": 1}));
        assert_eq!(
            (
                &tags["trial"]["examples"][0]["path"],
                &tags["trial"]["examples"][0]["line"]
            ),
            (&json!("app/admin.py"), &json!(14))
        );
    }
    assert_eq!(
        candidate(&report, "write-owners", "app.models:Order.oid")["entry"],
        json!({"name": "Order.oid writers", "targets": ["app.models:Order.oid"],
               "unresolved": "report"})
    );
    assert_eq!(
        candidate(&report, "read-owners", "app.models:Order._seen")["entry"]["allowed-readers"],
        json!(["app.models:*"])
    );
    // A helper in another module changing `notes`, and a subclass reading `_level` in its own
    // module: the census does not see them, the trial does.
    let rejected: Vec<(&Value, &Value, &Value, &Value)> = report["rejected_contracts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                &r["target"],
                &r["run"],
                &r["trial"]["examples"][0]["rule"],
                &r["trial"]["examples"][0]["path"],
            )
        })
        .collect();
    assert_eq!(
        rejected,
        [
            (
                &json!("app.models:Order.notes"),
                &json!("contract-trial"),
                &json!("MUT201"),
                &json!("app/admin.py")
            ),
            (
                &json!("app.models:Order._level"),
                &json!("contract-trial"),
                &json!("MUT203"),
                &json!("app/rush.py")
            ),
        ]
    );
    // `Order.status` is written in two modules; `Named.status` is a protocol's; `Order.limit` is
    // never written, only read; `Order._debug` is read in another module.
    assert!(
        report["contracts"].as_array().unwrap().iter().all(|c| {
            let target = c["target"].as_str().unwrap();
            ![".status", ".limit", "._debug"]
                .iter()
                .any(|attr| target.ends_with(attr))
        }),
        "{report:#}"
    );
    let (_, out, _) = p.heki(&["recommend", "--ignore", "EFF201"]);
    let text = section(&out);
    for line in [
        "  write-owners  app.models:Order.items  high\n    written by 2 function(s), all in \
         app.store (Store.add_item, Store.clear), changed in place; read in 2 other module(s)\n    \
         trial: MUT201 0, MUT205 0 (unresolved = \"report\")\n",
        "  write-owners  app.models:Order.tags  medium\n    written by 1 function(s), all in \
         app.store (Store.tag), changed in place; read in 2 other module(s)\n    trial: MUT201 0, \
         MUT205 1 (at app/admin.py:14), so unresolved = \"ignore\"\n",
        "  write-owners  app.models:Order.oid  high\n    written only while its objects are \
         constructed; read in 2 other module(s)\n",
        "  read-owners  app.models:Order._cache  high\n    read by 1 function(s), all in \
         app.models (Order.lookup); the class is used in 4 other module(s)\n",
        "  Rejected: write-owners app.models:Order.notes (MUT201 1, MUT205 0): MUT201 at \
         app/admin.py:10\n",
        "  Rejected: read-owners app.models:Order._level (MUT203 1, MUT206 0): MUT203 at \
         app/rush.py:6\n",
    ] {
        assert!(text.contains(line), "{line}\nin\n{text}");
    }
    assert!(
        text.ends_with(
            "  For every candidate, accesses inside code heki cannot follow (unresolved calls, \
             libraries without a catalog) are not seen."
        ),
        "{text}"
    );
}

#[test]
fn a_class_of_a_test_module_gets_no_candidate_and_the_same_class_elsewhere_does() {
    let p = owners("recommend_contracts_test_module_class");
    let class = "class Probe:\n    def __init__(self) -> None:\n        self.log: list[str] = []\n\n\n\
                 def record(p: Probe, x: str) -> None:\n    p.log.append(x)\n";
    let reader = |module: &str, name: &str| {
        format!(
            "from {module} import Probe\n\n\ndef {name}(p: Probe) -> list[str]:\n    return p.log\n"
        )
    };
    p.write("tests/__init__.py", "")
        .write("tests/conftest.py", class)
        .write("tests/test_a.py", &reader("tests.conftest", "a"))
        .write("tests/test_b.py", &reader("tests.conftest", "b"))
        .write("app/probe.py", class)
        .write("app/use_a.py", &reader("app.probe", "a"))
        .write("app/use_b.py", &reader("app.probe", "b"));
    let report = recommend_json(&p, &[]);
    let targets: Vec<String> = kept(&report).into_iter().map(|c| c.1).collect();
    assert!(targets.contains(&s("app.probe:Probe.log")), "{targets:?}");
    assert!(
        !targets.iter().any(|t| t.starts_with("tests.")),
        "{targets:?}"
    );
}

#[test]
fn a_candidate_lists_the_first_accesses_on_objects_of_unknown_class() {
    let p = owners("recommend_contracts_unresolved_examples");
    p.write(
        "app/reset.py",
        "def a(o) -> None:\n    o.tags = []\n\n\ndef b(o) -> None:\n    o.tags = []\n\n\n\
         def c(o) -> None:\n    o.tags = []\n",
    );
    let (_, out, _) = p.heki(&["recommend", "--ignore", "EFF201"]);
    let line = "    trial: MUT201 0, MUT205 4 (at app/admin.py:14, app/reset.py:2, app/reset.py:6, \
                ...), so unresolved = \"ignore\"\n";
    assert!(section(&out).contains(line), "{out}");
}

#[test]
fn effect_boundaries_cover_modules_and_packages_reaching_no_blocking_call() {
    let p = layers("recommend_contract_boundaries");
    let report = recommend_json(&p, &[]);
    assert_eq!(
        kept(&report),
        [
            // Called from other modules first.
            (
                s("effect-boundaries"),
                s("app.pricing"),
                s("high"),
                Value::Null
            ),
            (
                s("effect-boundaries"),
                s("app.domain"),
                s("high"),
                Value::Null
            ),
            (
                s("effect-boundaries"),
                s("app.payments"),
                s("medium"),
                Value::Null
            ),
            (s("effect-boundaries"), s("lib"), s("high"), Value::Null),
        ],
        "{report:#}"
    );
    let scopes =
        |target: &str| candidate(&report, "effect-boundaries", target)["entry"]["scopes"].clone();
    // A package with an `__init__.py` is named with and without its module part; a namespace
    // package below it only.
    assert_eq!(
        scopes("app.domain"),
        json!(["app.domain:*", "app.domain.*:*"])
    );
    assert_eq!(scopes("lib"), json!(["lib.*:*"]));
    assert_eq!(scopes("app.pricing"), json!(["app.pricing:*"]));
    let pricing = candidate(&report, "effect-boundaries", "app.pricing");
    assert_eq!(
        (&pricing["entry"]["name"], &pricing["entry"]["forbid"]),
        (
            &json!("app.pricing without blocking calls"),
            &json!(["blocking"])
        )
    );
    assert_eq!(
        pricing["evidence"]["audience"]["items"],
        json!(["app.http", "tests.test_pricing"])
    );
    // A call of a library name without a catalog entry: heki cannot see what it does.
    let payments = candidate(&report, "effect-boundaries", "app.payments");
    assert_eq!(
        payments["evidence"]["unseen"]["items"],
        json!(["app.payments.charge", "app.payments.refund"])
    );
    assert_eq!(
        payments["caveats"][0],
        json!("2 function(s) of the scopes reach code heki does not see")
    );
    let (_, out, _) = p.heki(&["recommend"]);
    assert!(
        section(&out).contains(
            "  effect-boundaries  app.payments  medium\n    3 function(s) in 1 module(s) reach no \
             blocking call heki sees; 2 reach code heki does not see (app.payments.charge, \
             app.payments.refund)\n    trial: EFF201 0\n"
        ),
        "{out}"
    );
    // The module reaching `requests.get`, and the tests, keep no boundary.
    assert!(
        kept(&report)
            .iter()
            .all(|(_, target, _, _)| target != "app.http" && !target.starts_with("tests")),
        "{report:#}"
    );
    assert!(report["rejected_contracts"].as_array().unwrap().is_empty());
}

#[test]
fn a_package_holding_a_test_module_names_its_other_modules_one_by_one() {
    let p = layers("recommend_contract_boundary_tests_inside");
    p.write(
        "app/domain/test_rules.py",
        "import time\n\nfrom app.domain.rules import check\n\n\ndef test_check() -> None:\n    \
         time.sleep(1)\n    assert check(1)\n",
    );
    let report = recommend_json(&p, &[]);
    // A wildcard would cover the test module, and its blocking call would reject the boundary.
    let domain = candidate(&report, "effect-boundaries", "app.domain");
    assert_eq!(
        domain["entry"]["scopes"],
        json!(["app.domain:*", "app.domain.rules:*"])
    );
    assert_eq!(
        (
            &domain["evidence"]["modules"]["count"],
            &domain["evidence"]["modules"]["items"]
        ),
        (&json!(2), &json!(["app.domain", "app.domain.rules"])),
        "{domain:#}"
    );
    assert_eq!(domain["confidence"], "high");
    let lib = candidate(&report, "effect-boundaries", "lib");
    assert_eq!(lib["entry"]["scopes"], json!(["lib.*:*"]));
    assert_eq!(report["rejected_contracts"], json!([]), "{report:#}");
}

#[test]
fn a_module_defining_a_test_function_is_test_code_too() {
    let p = layers("recommend_contract_test_functions");
    // A Django-style `tests.py` (a test class's methods) in a package, and a `test_*` function
    // in a module that would qualify alone.
    p.write(
        "app/domain/tests.py",
        "from app.domain.rules import check\n\n\nclass RulesTests:\n    \
         def test_check(self) -> None:\n        assert check(1)\n",
    )
    .write(
        "lib/c.py",
        "from lib.a import one\n\n\ndef four() -> int:\n    return 4\n\n\n\
         def five() -> int:\n    return 5\n\n\ndef six() -> int:\n    return 6\n\n\n\
         def test_one() -> None:\n    assert one() == 1\n",
    );
    let report = recommend_json(&p, &[]);
    let domain = candidate(&report, "effect-boundaries", "app.domain");
    assert_eq!(
        domain["entry"]["scopes"],
        json!(["app.domain:*", "app.domain.rules:*"])
    );
    let lib = candidate(&report, "effect-boundaries", "lib");
    assert_eq!(lib["entry"]["scopes"], json!(["lib.a:*", "lib.b:*"]));
    assert!(
        kept(&report)
            .iter()
            .all(|(_, target, _, _)| target != "lib.c"),
        "{report:#}"
    );
}

#[test]
fn a_module_a_pattern_cannot_name_gets_no_candidate() {
    let p = layers("recommend_contract_unnamed_modules");
    let functions = layers_module("route");
    // `[id]` is a character class to a glob, and `[1` does not parse as one. The only writer
    // of `Page.title` is in `notes[1`, which `allowed-writers` would have to name.
    let reader = |name: &str| {
        format!("from app.page import Page\n\n\ndef {name}(p: Page) -> str:\n    return p.title\n")
    };
    p.write("routes/[id].py", &functions)
        .write(
            "notes[1.py",
            &format!(
                "from app.page import Page\n\n\ndef rename(p: Page) -> None:\n    \
                 p.title = \"x\"\n\n\n{functions}"
            ),
        )
        .write(
            "app/page.py",
            "class Page:\n    def __init__(self) -> None:\n        self.title = \"\"\n",
        )
        .write("app/show.py", &reader("show"))
        .write("app/feed.py", &reader("feed"));
    let report = recommend_json(&p, &[]);
    let targets: Vec<String> = kept(&report).into_iter().map(|c| c.1).collect();
    assert!(
        targets.contains(&s("lib"))
            && !targets
                .iter()
                .any(|t| t.contains('[') || t.contains("Page.title")),
        "{targets:?}"
    );
    assert_eq!(report["rejected_contracts"], json!([]), "{report:#}");
}

#[test]
fn a_file_heki_cannot_parse_makes_every_candidate_medium() {
    let p = owners("recommend_contract_unparsed");
    p.write("app/broken.py", "def broken(:\n");
    let (code, out, err) = p.heki(&["recommend", "--output-format", "json", "--ignore", "EFF201"]);
    assert_eq!(code, 2, "{err}");
    let report: Value = serde_json::from_str(&out).unwrap();
    let kept = kept(&report);
    assert!(
        !kept.is_empty()
            && kept
                .iter()
                .all(|(_, _, confidence, unresolved)| confidence == "medium"
                    && unresolved == &json!("ignore")),
        "{report:#}"
    );
    assert_eq!(
        candidate(&report, "write-owners", "app.models:Order.items")["caveats"][0],
        json!("1 Python file(s) heki could not read or parse: their accesses are not seen")
    );
}

#[test]
fn a_candidate_hiding_a_report_of_a_configured_contract_is_rejected() {
    // The write of the key is on a `Store`, or on a `Child` the target on `Store` covers.
    for class in ["Store", "Child"] {
        let p = Project::new(&format!("recommend_contract_hides_unlisted_key_{class}"));
        p.write(
            "pyproject.toml",
            "[project]\nname = \"t\"\n\n[[tool.heki.write-owners]]\nname = \"a owners\"\n\
             targets = [\"app.models:Store.a\"]\nallowed-writers = [\"app.models:*\"]\n\
             unlisted-dict-keys = \"report\"\n",
        )
        .write("app/__init__.py", "")
        .write(
            "app/models.py",
            "class Store:\n    def __init__(self) -> None:\n        self.a = 0\n        self.b = 0\n\n\n\
             class Child(Store):\n    pass\n",
        )
        .write(
            "app/store.py",
            &format!(
                "from app.models import Child, Store\n\n\ndef bump(s: Store) -> None:\n    \
                 s.b = s.b + 1\n\n\ndef reset(s: {class}) -> None:\n    s.__dict__[\"b\"] = 0\n"
            ),
        )
        .write(
            "app/views.py",
            "from app.models import Store\n\n\ndef show(s: Store) -> int:\n    return s.b\n",
        )
        .write(
            "app/api.py",
            "from app.models import Store\n\n\ndef dump(s: Store) -> int:\n    return s.b + 1\n",
        );
        let reported_now = |p: &Project| -> Vec<(String, u64)> {
            checked(p, &["--select", "MUT201"])
                .iter()
                .map(|d| {
                    (
                        d["path"].as_str().unwrap().to_owned(),
                        d["line"].as_u64().unwrap(),
                    )
                })
                .collect()
        };
        assert_eq!(
            reported_now(&p),
            [("app/store.py".to_owned(), 9)],
            "{class}"
        );
        // Naming `Store.b` would list the key for `a owners` too, and allow its writer module.
        let report = recommend_json(&p, &[]);
        let rejected = &report["rejected_contracts"];
        assert_eq!(rejected.as_array().unwrap().len(), 1, "{report:#}");
        assert_eq!(
            (
                &rejected[0]["target"],
                &rejected[0]["run"],
                &rejected[0]["trial"]["reports"]
            ),
            (
                &json!("app.models:Store.b"),
                &json!("contract-trial"),
                &json!({"MUT201": 1, "MUT205": 0})
            ),
            "{class}"
        );
        assert!(
            kept(&report)
                .iter()
                .all(|(kind, _, _, _)| kind != "write-owners"),
            "{report:#}"
        );
        // With the block, the write is still reported.
        let block = report["config_toml"].as_str().unwrap();
        let existing = std::fs::read_to_string(p.dir.join("pyproject.toml")).unwrap();
        p.write("pyproject.toml", &applied(&existing, block));
        assert_eq!(
            reported_now(&p),
            [("app/store.py".to_owned(), 9)],
            "{class}"
        );
    }
}

/// `existing` with `block` added as a user would: the block's tables after the file's own.
fn applied(existing: &str, block: &str) -> String {
    format!("{existing}\n{block}")
}

#[test]
fn the_block_gives_the_reports_the_section_promised() {
    // `MUT106` reports a generator read twice: the block enables it.
    let reused = "def total_and_count(rows):\n    items = (r for r in rows)\n    total = sum(items)\n    \
                  count = len(list(items))\n    return total, count\n";
    for (file, existing) in [
        ("pyproject.toml", "[project]\nname = \"t\"\n"),
        ("heki.toml", "[lint]\nextend-select = [\"EFF105\"]\n"),
    ] {
        for (name, p) in [
            (
                "owners",
                owners(&format!("recommend_contract_trip_owners_{file}")),
            ),
            (
                "layers",
                layers(&format!("recommend_contract_trip_layers_{file}")),
            ),
        ] {
            p.write("app/reuse.py", reused);
            if file == "heki.toml" {
                std::fs::remove_file(p.dir.join("pyproject.toml")).unwrap();
            }
            p.write(file, existing);
            let every_rule = "ALL";
            let before = checked(&p, &["--select", every_rule, "--heuristic"]);
            let report = recommend_json(&p, &[]);
            assert!(
                !report["contracts"].as_array().unwrap().is_empty(),
                "{name}"
            );
            let block = report["config_toml"].as_str().unwrap();
            // The heki.toml form adds the rules to the array the file sets.
            let merged = if existing.contains("extend-select") {
                let line = block
                    .lines()
                    .find(|l| l.starts_with("extend-select"))
                    .unwrap();
                let rest: String = block
                    .split("\n\n")
                    .filter(|part| !part.starts_with("[lint]"))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                applied(
                    &existing.replace("extend-select = [\"EFF105\"]", line),
                    &rest,
                )
            } else {
                applied(existing, block)
            };
            p.write(file, &merged);
            // No contract of the block reports.
            let after = checked(&p, &[]);
            let contract_rules = ["MUT201", "MUT203", "MUT205", "MUT206", "EFF201"];
            assert!(
                after
                    .iter()
                    .all(|d| !contract_rules.contains(&d["rule_code"].as_str().unwrap())),
                "{name} {file}: {after:#?}\n{merged}"
            );
            // The rule the block enables reports what it promised.
            let promised = report["rules"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["code"] == "MUT106")
                .unwrap();
            assert_eq!(promised["in_block"], json!(true), "{name} {file}");
            let mut106 = after.iter().filter(|d| d["rule_code"] == "MUT106").count();
            assert_eq!(json!(mut106), promised["reports"], "{name} {file}");
            // Every other report is unchanged.
            let others = |diags: &[Value]| -> Vec<Value> {
                diags
                    .iter()
                    .filter(|d| d["rule_code"] != "MUT106")
                    .cloned()
                    .collect()
            };
            let all_after = checked(&p, &["--select", every_rule, "--heuristic"]);
            assert_eq!(others(&all_after), others(&before), "{name} {file}");
        }
    }
}

#[test]
fn files_the_paths_leave_out_make_every_candidate_medium() {
    let p = layers("recommend_contract_left_out");
    let report = recommend_json(&p, &["app"]);
    let confidences: Vec<String> = kept(&report).into_iter().map(|c| c.2).collect();
    assert_eq!(confidences, ["medium", "medium", "medium"], "{report:#}");
    assert_eq!(
        candidate(&report, "effect-boundaries", "app.pricing")["caveats"],
        json!([
            "Python files below the project root were not analyzed: their accesses are not seen",
            "accesses inside code heki cannot follow (unresolved calls, libraries without a \
             catalog) are not seen"
        ])
    );
    // An ownership candidate medium only because files were left out does not report the
    // accesses of unknown class either: there may be some in those files.
    let q = owners("recommend_contract_left_out_owners");
    q.write(
        "tests/test_store.py",
        "def test_store() -> None:\n    pass\n",
    );
    let report = recommend_json(&q, &["app", "--ignore", "EFF201"]);
    let kept = kept(&report);
    assert!(
        !kept.is_empty()
            && kept
                .iter()
                .all(|(_, _, confidence, unresolved)| confidence == "medium"
                    && unresolved == &json!("ignore")),
        "{report:#}"
    );
    let (_, out, _) = q.heki(&["recommend", "app", "--ignore", "EFF201"]);
    assert!(
        section(&out).contains(
            "  write-owners  app.models:Order.items  medium\n    written by 2 function(s), all \
             in app.store (Store.add_item, Store.clear), changed in place; read in 2 other \
             module(s)\n    trial: MUT201 0, MUT205 0 (unresolved = \"ignore\", as for every \
             medium candidate)\n"
        ),
        "{out}"
    );
}

#[test]
fn a_count_of_the_files_left_out_that_stops_early_makes_every_candidate_medium() {
    let p = layers("recommend_contract_count_stops");
    // Every Python file is analyzed, but the count stops in `data/` before it reaches them all.
    for i in 0..10_000 {
        p.write(&format!("data/f{i:05}.txt"), "");
    }
    let report = recommend_json(&p, &["app", "lib", "tests"]);
    assert_eq!(
        report["analyzed"]["left_out"],
        json!({"python_files": 0, "files": [], "truncated": true})
    );
    let confidences: Vec<String> = kept(&report).into_iter().map(|c| c.2).collect();
    assert!(
        !confidences.is_empty() && confidences.iter().all(|c| c == "medium"),
        "{report:#}"
    );
    assert_eq!(
        candidate(&report, "effect-boundaries", "app.pricing")["caveats"][0],
        json!(
            "the count of the Python files below the project root stopped before the end: files \
             it did not reach may be left out, and their accesses are not seen"
        )
    );
    assert!(
        report["notes"][0]
            .as_str()
            .unwrap()
            .starts_with("the count of the Python files below the project root stopped before"),
        "{report:#}"
    );
}

#[test]
fn what_the_configuration_declares_or_ignores_is_not_proposed() {
    let p = owners("recommend_contract_declared");
    p.write("app/pricing.py", &layers_module("pricing"));
    p.write("app/domain.py", &layers_module("domain"));
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n\
         [[tool.heki.write-owners]]\nname = \"Order.oid writers\"\ntargets = [\"Store.nothing\"]\n\n\
         [[tool.heki.write-owners]]\nname = \"app.models:Order.oid writers\"\ntargets = [\"Store.none\"]\n\n\
         [[tool.heki.write-owners]]\nname = \"items\"\ntargets = [\"Order.items\"]\nallowed-writers = [\"app.store.*\"]\n\n\
         [[tool.heki.read-owners]]\nname = \"cache\"\ntargets = [\"app.models:Order._cache\"]\nallowed-readers = [\"app.models:*\"]\n\n\
         [[tool.heki.effect-boundaries]]\nname = \"pricing\"\nscopes = [\"app.pricing:*\"]\nforbid = [\"blocking\"]\n\n\
         [[tool.heki.effect-boundaries]]\nname = \"domain sleeps\"\nscopes = [\"app.domain:*\"]\nforbid = [\"sleep\"]\n",
    );
    let report = recommend_json(&p, &[]);
    let targets: Vec<String> = kept(&report).into_iter().map(|c| c.1).collect();
    // Declared: `Order.items` (a suffix target), `Order._cache`, and `app.pricing` under a
    // boundary forbidding every blocking effect. A boundary forbidding `sleep` only leaves
    // `app.domain` to propose.
    for target in [
        "app.models:Order.items",
        "app.models:Order._cache",
        "app.pricing",
    ] {
        assert!(!targets.contains(&s(target)), "{target}: {targets:?}");
    }
    assert!(targets.contains(&s("app.domain")), "{targets:?}");
    // The names the configuration has: the full form, then a number.
    assert_eq!(
        candidate(&report, "write-owners", "app.models:Order.oid")["entry"]["name"],
        json!("app.models:Order.oid writers (2)")
    );
    // A kind whose rule the configuration ignores.
    let ignoring = recommend_json(&p, &["--ignore", "MUT203,EFF201"]);
    assert!(
        kept(&ignoring)
            .iter()
            .all(|(kind, _, _, _)| kind == "write-owners"),
        "{ignoring:#}"
    );
    assert_eq!(
        ignoring["notes"],
        json!([
            "the configuration ignores MUT203, so no read-owners contract is proposed",
            "the configuration ignores EFF201, so no effect-boundaries contract is proposed"
        ])
    );
}

/// A module of three functions that reach no blocking call.
fn layers_module(name: &str) -> String {
    (0..3)
        .map(|i| format!("def {name}_{i}() -> int:\n    return {i}\n\n\n"))
        .collect()
}

#[test]
fn a_selection_without_the_contract_rules_gets_them_from_the_block() {
    let p = owners("recommend_contract_selection");
    let report = recommend_json(&p, &["--select", "EFF101", "--ignore", "EFF201"]);
    assert_eq!(
        report["config"]["lint"],
        json!({"extend-select": ["MUT201", "MUT203", "MUT205", "MUT206"]})
    );
}

#[test]
fn a_rule_of_unresolved_accesses_the_configuration_ignores_stays_off() {
    let p = owners("recommend_contract_strict_ignored");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nignore = [\"EFF201\", \"MUT205\"]\n",
    );
    let report = recommend_json(&p, &[]);
    let unresolved = |kind: &str, target: &str| {
        let c = candidate(&report, kind, target);
        (c["confidence"].clone(), c["entry"]["unresolved"].clone())
    };
    // A high `write-owners` candidate sets `unresolved = "ignore"`; `read-owners` still reports.
    assert_eq!(
        unresolved("write-owners", "app.models:Order.items"),
        (json!("high"), json!("ignore"))
    );
    assert_eq!(
        unresolved("read-owners", "app.models:Order._cache"),
        (json!("high"), json!("report"))
    );
    let selected = &report["config"]["lint"]["extend-select"];
    assert!(
        !selected
            .as_array()
            .is_some_and(|rules| rules.contains(&json!("MUT205"))),
        "{report:#}"
    );
    let (_, out, _) = p.heki(&["recommend"]);
    assert!(
        section(&out).contains(
            "  write-owners  app.models:Order.items  high\n    written by 2 function(s), all in \
             app.store (Store.add_item, Store.clear), changed in place; read in 2 other \
             module(s)\n    trial: MUT201 0, MUT205 0 (unresolved = \"ignore\": the configuration \
             ignores MUT205)\n"
        ),
        "{out}"
    );
}

#[test]
fn past_the_cap_candidates_are_not_tried() {
    let p = owners("recommend_contract_cap");
    // 31 contracts of the configuration leave one `write-owners` candidate to try.
    let declared: String = (0..31)
        .map(|i| {
            format!("[[tool.heki.write-owners]]\nname = \"c{i}\"\ntargets = [\"Store.n{i}\"]\n\n")
        })
        .collect();
    p.write(
        "pyproject.toml",
        &format!("[project]\nname = \"t\"\n\n{declared}"),
    );
    let report = recommend_json(&p, &["--ignore", "EFF201,MUT203"]);
    assert_eq!(
        report["contracts_not_tried"],
        json!([{"kind": "write-owners", "tried": 1, "candidates": 7}])
    );
    // The first by rank, `Order.notes`, read in the most other modules, is tried and rejected.
    assert_eq!(kept(&report), []);
    assert_eq!(
        report["rejected_contracts"][0]["target"],
        json!("app.models:Order.notes")
    );
    let (_, out, _) = p.heki(&["recommend", "--ignore", "EFF201,MUT203"]);
    assert!(
        section(&out).contains("\n  Not tried: 7 more write-owners candidate(s) past the first 1"),
        "{out}"
    );
    // Without a candidate to try, no trial runs.
    let none: String = (0..32)
        .map(|i| {
            format!("[[tool.heki.write-owners]]\nname = \"c{i}\"\ntargets = [\"Store.n{i}\"]\n\n")
        })
        .collect();
    p.write(
        "pyproject.toml",
        &format!("[project]\nname = \"t\"\n\n{none}"),
    );
    let untried = recommend_json(&p, &["--ignore", "EFF201,MUT203"]);
    assert_eq!(untried["runs"], json!([{"label": "discovery"}]));
    assert_eq!(
        untried["contracts_not_tried"],
        json!([{"kind": "write-owners", "tried": 0, "candidates": 8}])
    );
}

#[test]
fn reports_an_inline_suppression_drops_are_counted_apart() {
    let p = owners("recommend_contract_suppressed");
    p.write(
        "app/admin.py",
        &ADMIN.replace(
            "    push(order.notes, \"x\")",
            "    push(order.notes, \"x\")  # heki: ignore[MUT201]",
        ),
    );
    let report = recommend_json(&p, &["--ignore", "EFF201"]);
    let notes = candidate(&report, "write-owners", "app.models:Order.notes");
    assert_eq!(
        (&notes["trial"]["reports"], &notes["trial"]["suppressed"]),
        (&json!({"MUT201": 0, "MUT205": 0}), &json!(1))
    );
    let (_, out, _) = p.heki(&["recommend", "--ignore", "EFF201"]);
    assert!(
        section(&out).contains(
            "    trial: MUT201 0, MUT205 0; 1 suppressed inline (unresolved = \"report\")\n"
        ),
        "{out}"
    );
}

#[test]
fn a_scope_of_many_functions_lists_the_first_fifty() {
    let p = Project::new("recommend_contract_many_functions");
    let functions: String = (0..60)
        .map(|i| format!("def f{i:02}() -> None:\n    stripe.f{i:02}()\n\n\n"))
        .collect();
    p.write("wide.py", &format!("import stripe\n\n\n{functions}"));
    let report = recommend_json(&p, &[]);
    let evidence = &candidate(&report, "effect-boundaries", "wide")["evidence"];
    for list in ["functions", "unseen"] {
        assert_eq!(
            (&evidence[list]["count"], &evidence[list]["truncated"]),
            (&json!(60), &json!(true))
        );
        assert_eq!(evidence[list]["items"].as_array().unwrap().len(), 50);
    }
    let (_, out, _) = p.heki(&["recommend"]);
    assert!(
        section(&out).contains(
            "    60 function(s) in 1 module(s) reach no blocking call heki sees; 60 reach code heki \
             does not see (wide.f00, wide.f01, wide.f02, ...)\n"
        ),
        "{out}"
    );
}

#[test]
fn a_block_setting_src_leaves_contracts_to_a_run_with_it() {
    // `import config.env` names `backend/config/env.py` once `backend` is a source root, which
    // renames the modules below it.
    let p = Project::new("recommend_contract_src");
    p.write(
        "backend/config/env.py",
        "import time\n\n\ndef load():\n    time.sleep(1)\n    return {}\n",
    )
    .write(
        "backend/main.py",
        "from config.env import load\nimport config.env\n\n\nasync def handler():\n    return \
         load()\n\n\nasync def other():\n    return config.env.load()\n",
    )
    .write("backend/pricing.py", &layers_module("price"));
    let report = recommend_json(&p, &[]);
    assert_eq!(report["config"]["src"], json!(["backend"]));
    assert_eq!(
        (&report["contracts"], &report["rejected_contracts"]),
        (&json!([]), &json!([]))
    );
    assert!(
        report["runs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["label"] != "contract-trial"),
        "{report:#}"
    );
    assert!(
        report["notes"].as_array().unwrap().contains(&json!(
            "the block sets src, which renames modules and resolves imports heki does not resolve \
             now: run heki recommend again with it for contract candidates"
        )),
        "{report:#}"
    );
    // With `src` set, the module keeps its boundary under its new name.
    let block = report["config_toml"].as_str().unwrap();
    p.write(
        "pyproject.toml",
        &format!("[project]\nname = \"t\"\n\n{block}"),
    );
    let again = recommend_json(&p, &[]);
    assert_eq!(
        kept(&again),
        [(s("effect-boundaries"), s("pricing"), s("high"), Value::Null)]
    );
}
