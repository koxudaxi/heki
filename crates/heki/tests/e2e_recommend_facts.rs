//! The facts `heki recommend` reads besides the rules, end to end through
//! `heki dump-recommend-facts`: the calls heki is blind to (calls of library names the catalog
//! has no entry for, calls it does not resolve and why), and what they may hold up.

mod common;

use common::Project;
use serde_json::Value;

/// `heki dump-recommend-facts <args>`, which must exit with `code`: the JSON object.
fn facts_with(p: &Project, args: &[&str], code: i32) -> Value {
    let mut all = vec!["dump-recommend-facts"];
    all.extend_from_slice(args);
    let (exit, out, err) = p.heki(&all);
    assert_eq!(exit, code, "{out}\n{err}");
    serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}\n{err}"))
}

fn blind_spots(p: &Project) -> Value {
    facts_with(p, &[], 0)["blind_spots"].clone()
}

fn s(v: &str) -> String {
    v.to_owned()
}

/// `(calls, awaited, on_event_loop, in_loops)` of a counted entry.
fn counts(v: &Value) -> (u64, u64, u64, u64) {
    let n = |k: &str| v[k].as_u64().unwrap();
    (n("calls"), n("awaited"), n("on_event_loop"), n("in_loops"))
}

/// The uncatalogued symbol `symbol`, with its package's entry.
fn symbol<'v>(spots: &'v Value, symbol: &str) -> (&'v Value, &'v Value) {
    spots["uncatalogued"]
        .as_array()
        .unwrap()
        .iter()
        .find_map(|p| {
            let s = p["symbols"]
                .as_array()
                .unwrap()
                .iter()
                .find(|s| s["symbol"] == symbol)?;
            Some((p, s))
        })
        .unwrap_or_else(|| panic!("no uncatalogued `{symbol}` in {spots:#}"))
}

fn symbols(spots: &Value) -> Vec<String> {
    let packages = spots["uncatalogued"].as_array().unwrap();
    packages
        .iter()
        .flat_map(|p| p["symbols"].as_array().unwrap())
        .map(|s| s["symbol"].as_str().unwrap().to_owned())
        .collect()
}

/// One group of unresolved calls: `(function, reason, parameter, names, lines)`.
type Group = (String, String, Option<String>, Vec<String>, Vec<u64>);

/// Every group of unresolved calls.
fn unresolved(spots: &Value) -> Vec<Group> {
    spots["unresolved"]
        .as_array()
        .unwrap()
        .iter()
        .map(|g| {
            let names = g["names"].as_array().unwrap();
            let sites = g["sites"].as_array().unwrap();
            (
                g["function"].as_str().unwrap().to_owned(),
                g["reason"].as_str().unwrap().to_owned(),
                g["parameter"].as_str().map(str::to_owned),
                names
                    .iter()
                    .map(|n| n.as_str().unwrap().to_owned())
                    .collect(),
                sites.iter().map(|s| s["line"].as_u64().unwrap()).collect(),
            )
        })
        .collect()
}

#[test]
fn calls_of_library_names_without_catalog_entries_are_counted_by_package_and_name() {
    let p = Project::new("recommend_uncatalogued");
    p.write(
        "app/models.py",
        "from dataclasses import dataclass\n\n\n@dataclass\nclass Order:\n    total: int = 0\n",
    );
    p.write(
        "app/pay.py",
        r#"import asyncio
import os
import requests
import stripe
from app.models import Order


async def checkout(order):
    stripe.Charge.create(amount=order.total)
    await stripe.Charge.acreate(amount=order.total)
    requests.get("https://example.com")
    requests.Session().get("https://example.com")
    requests.fancy_new_api()
    os.path.join("a", "b")
    Order()
    refund_all([order])
    await asyncio.to_thread(audit, order)


def refund_all(orders):
    for o in orders:
        stripe.Refund.create(charge=o)
    note()


def note():
    stripe.Event.list()


def audit(order):
    stripe.Audit.log(order)


def batch(items):
    for i in items:
        note()


def gen():
    yield stripe.Event.stream()


def make():
    return gen()


async def discarded(items):
    make()
    for i in items:
        make()


async def iterated():
    for event in make():
        pass
"#,
    );
    let spots = blind_spots(&p);
    // The catalog's own names, the standard library, and project classes are not blind spots.
    assert_eq!(
        symbols(&spots),
        [
            "requests.fancy_new_api",
            "stripe.Audit.log",
            "stripe.Charge.acreate",
            "stripe.Charge.create",
            "stripe.Event.list",
            "stripe.Event.stream",
            "stripe.Refund.create",
        ],
        "{spots:#}"
    );
    let (requests, fancy) = symbol(&spots, "requests.fancy_new_api");
    assert_eq!(requests["registered"], true);
    assert_eq!(counts(fancy), (1, 0, 1, 0));
    let (stripe, create) = symbol(&spots, "stripe.Charge.create");
    assert_eq!(stripe["registered"], false);
    assert_eq!(counts(stripe), (6, 1, 4, 2));
    assert_eq!(counts(create), (1, 0, 1, 0));
    let site = &create["sites"][0];
    assert_eq!(
        (&site["path"], &site["line"], &site["function"]),
        (
            &Value::from("app/pay.py"),
            &Value::from(9),
            &Value::from("app.pay.checkout")
        )
    );
    // Awaited, it holds no event loop up.
    assert_eq!(
        counts(symbol(&spots, "stripe.Charge.acreate").1),
        (1, 1, 0, 0)
    );
    // In a loop of a helper the `async def` calls.
    assert_eq!(
        counts(symbol(&spots, "stripe.Refund.create").1),
        (1, 0, 1, 1)
    );
    // Two levels below the `async def`, and below a loop of another function.
    assert_eq!(counts(symbol(&spots, "stripe.Event.list").1), (1, 0, 1, 1));
    // On a worker thread.
    assert_eq!(counts(symbol(&spots, "stripe.Audit.log").1), (1, 0, 0, 0));
    // In a generator a helper returns: its body runs where an `async def` iterates the helper's
    // result, not where the helper is called in a loop and its result dropped.
    assert_eq!(
        counts(symbol(&spots, "stripe.Event.stream").1),
        (1, 0, 1, 0)
    );
    let totals = &spots["totals"];
    // `--totals` prints the counts alone.
    let counted = facts_with(&p, &["--totals"], 0)["blind_spots"].clone();
    assert_eq!(counted, serde_json::json!({ "totals": totals }));
    assert_eq!(counts(&totals["uncatalogued"]), (7, 1, 5, 2));
    assert_eq!(
        (
            &totals["uncatalogued_packages"],
            &totals["uncatalogued_symbols"],
            &totals["registered_calls"],
            &totals["functions"],
        ),
        (
            &Value::from(2),
            &Value::from(7),
            &Value::from(1),
            &Value::from(5)
        )
    );
}

#[test]
fn unresolved_calls_are_grouped_by_function_and_reason() {
    let p = Project::new("recommend_unresolved");
    p.write(
        "app/shapes.py",
        r#"from typing import Protocol


class Base:
    def area(self):
        return 0

    @property
    def size(self):
        return 0


class Unknown(Protocol):
    pass
"#,
    );
    for i in 0..9 {
        p.write(
            &format!("app/s{i}.py"),
            &format!(
                "from app.shapes import Base\n\n\nclass S{i}(Base):\n    def area(self):\n        return {i}\n\n    @property\n    def size(self):\n        return {i}\n"
            ),
        );
    }
    p.write(
        "app/jobs.py",
        r#"import os
from typing import Any, Callable
from mystery import *
from app.shapes import Base, Unknown


class Client:
    def get(self, url):
        return url


class Partial(Missing):
    pass


def run(client, items, cls: type, hook: Callable[[], None], anything: Any, plain: object,
        shape: Base, port: Unknown, make):
    client.get("a")
    client.post("b")
    fetch = client.fetch
    fetch()
    client()
    cls()
    hook()
    anything.go()
    plain.go()
    create()
    port.run()
    make().go()
    client.attr.go()
    getattr(client, os.environ["NAME"])()
    shape.area()
    shape.size
    Partial().missing()
    Client().missing()
    items[0].go()
    try:
        client.close()
    finally:
        client.flush()
    if items:
        return 1
    return 2


class Holder:
    for name in ("a", "b"):
        register(name)


def build():
    class Local:
        hook = make_hook()
    return Local
"#,
    );
    let spots = blind_spots(&p);
    let names = |v: &[&str]| v.iter().map(|n| s(n)).collect::<Vec<_>>();
    let run = "app.jobs.run";
    let group = |reason: &str, parameter: Option<&str>, called: &[&str], lines: &[u64]| {
        (
            s(run),
            s(reason),
            parameter.map(s),
            names(called),
            lines.to_vec(),
        )
    };
    assert_eq!(
        unresolved(&spots),
        [
            // A class body the module runs is the module's; one a function runs is that
            // function's.
            (
                s("app.jobs.<module>"),
                s("unbound-name"),
                None,
                names(&["register"]),
                vec![48],
            ),
            (
                s("app.jobs.build"),
                s("unbound-name"),
                None,
                names(&["make_hook"]),
                vec![53],
            ),
            group(
                "unannotated-parameter",
                Some("client"),
                // The call in the `finally` body once, though the body is built once per way
                // out of the `try`.
                &["client", "close", "fetch", "flush", "get", "post"],
                &[18, 19, 21, 22, 38, 40],
            ),
            group("unannotated-parameter", Some("make"), &["make"], &[29]),
            group("unresolved-annotation", Some("cls"), &["cls"], &[23]),
            group("any-value", None, &["go"], &[25, 26]),
            group("callable-value", None, &["hook"], &[24]),
            group("unbound-name", None, &["create"], &[27]),
            group("unlisted-protocol", None, &["run"], &[28]),
            group("call-result", None, &["go"], &[29]),
            group("attribute", None, &["go"], &[30]),
            group("computed-name", None, &[], &[31]),
            // A call and a property read.
            group(
                "too-many-implementations",
                None,
                &["area", "size"],
                &[32, 33]
            ),
            group("unknown-member", None, &["missing"], &[34, 35]),
            group("untyped-value", None, &["go"], &[36]),
        ],
        "{spots:#}"
    );
    let totals = &spots["totals"];
    // The call in the class body's loop.
    assert_eq!(counts(&totals["unresolved"]), (23, 0, 0, 1));
    assert_eq!(totals["unresolved_by_reason"]["unannotated-parameter"], 7);
    assert_eq!(totals["unresolved_by_reason"]["unbound-name"], 3);
    assert_eq!(totals["functions"], 3);
}

#[test]
fn a_file_that_does_not_parse_leaves_the_facts_of_the_others() {
    let p = Project::new("recommend_parse_error");
    p.write(
        "app/ok.py",
        "import stripe\n\n\ndef pay():\n    stripe.pay()\n",
    );
    p.write("app/bad.py", "def broken(:\n");
    let (code, out, err) = p.heki(&["dump-recommend-facts"]);
    assert_eq!(code, 2, "{out}\n{err}");
    assert!(err.contains("app/bad.py"), "{err}");
    let facts: Value = serde_json::from_str(&out).unwrap();
    assert_eq!(symbols(&facts["blind_spots"]), ["stripe.pay"]);
    // The file heki could not read has no functions to count.
    let modules = facts["census"]["modules"].as_array().unwrap();
    let names: Vec<&str> = modules
        .iter()
        .map(|m| m["module"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["app.ok"]);
}

fn census(p: &Project) -> Value {
    facts_with(p, &[], 0)["census"].clone()
}

/// The census entry of `class.attr`.
fn attribute<'v>(census: &'v Value, class: &str, attr: &str) -> &'v Value {
    census["attributes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["class"] == class && a["attr"] == attr)
        .unwrap_or_else(|| panic!("no `{class}.{attr}` in {census:#}"))
}

/// `(function, line, sites)` of each access listed under `key`.
fn accesses(entry: &Value, key: &str) -> Vec<(String, u64, u64)> {
    entry[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| {
            (
                a["function"].as_str().unwrap().to_owned(),
                a["line"].as_u64().unwrap(),
                a["sites"].as_u64().unwrap(),
            )
        })
        .collect()
}

/// `(function, rebinds, in_place, fresh)` of each writer.
fn writers(entry: &Value) -> Vec<(String, bool, bool, bool)> {
    entry["writers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| {
            (
                w["function"].as_str().unwrap().to_owned(),
                w["rebinds"].as_bool().unwrap(),
                w["in_place"].as_bool().unwrap(),
                w["fresh"].as_bool().unwrap(),
            )
        })
        .collect()
}

#[test]
fn the_census_lists_each_attributes_writers_initializers_and_readers() {
    let p = Project::new("recommend_census_attributes");
    p.write(
        "app/models.py",
        r#"from typing import Protocol


class Order:
    def __init__(self, other: "Order | None" = None):
        self.items = []
        self.status = "new"
        if other is not None:
            other.status = "copied"
        self.items.extend(())

    def reset(self):
        self.items = []


class Special(Order):
    def __init__(self):
        super().__init__()
        self.extra = 1


class Named(Protocol):
    status: str
"#,
    );
    p.write(
        "app/store.py",
        r#"from app.models import Named, Order


class Store:
    def add(self, order: Order, x):
        order.items.append(x)
        order.items.append(x)

    def close(self, order: Order):
        order.status = "closed"
        setattr(order, "status", "closed")
        del order.status

    def keep(self, order: Order, x):
        items = order.items
        items.append(x)


class Box:
    def __init__(self, order: Order):
        order.items = []


def make():
    o = Order()
    o.status = "draft"
    return o


def shown(n: Named):
    return n.status
"#,
    );
    p.write(
        "app/views.py",
        "from app.models import Order\n\n\ndef show(o: Order):\n    return o.status, o.status\n",
    );
    let census = census(&p);
    let items = attribute(&census, "app.models.Order", "items");
    assert_eq!(
        writers(items),
        [
            (s("app.models.Order.reset"), true, false, false),
            // An initializer of another class writing an `Order` is a writer.
            (s("app.store.Box.__init__"), true, false, false),
            (s("app.store.Store.add"), false, true, false),
            (s("app.store.Store.keep"), false, true, false),
        ],
        "{items:#}"
    );
    assert_eq!(
        accesses(items, "initializers"),
        [(s("app.models.Order.__init__"), 6, 2)]
    );
    // `add` reads the attribute twice to change it; the first read's line.
    assert_eq!(
        accesses(items, "readers"),
        [
            (s("app.models.Order.__init__"), 10, 1),
            (s("app.store.Store.add"), 6, 2),
            (s("app.store.Store.keep"), 15, 1),
        ]
    );
    let status = attribute(&census, "app.models.Order", "status");
    assert_eq!(
        writers(status),
        [
            // The initializer's write of another instance is no initialization.
            (s("app.models.Order.__init__"), true, false, false),
            (s("app.store.Store.close"), true, false, false),
            // A factory filling the object it constructs.
            (s("app.store.make"), true, false, true),
        ],
        "{status:#}"
    );
    let close = &status["writers"][1];
    assert_eq!(
        (&close["line"], &close["sites"]),
        (&Value::from(10), &Value::from(3))
    );
    // Through a protocol declaring the attribute, the read is the protocol's.
    assert_eq!(accesses(status, "readers"), [(s("app.views.show"), 5, 2)]);
    assert_eq!(
        accesses(attribute(&census, "app.models.Named", "status"), "readers"),
        [(s("app.store.shown"), 31, 1)]
    );
    // The subclass's own instance: its initializer's write.
    let extra = attribute(&census, "app.models.Special", "extra");
    assert_eq!(
        accesses(extra, "initializers"),
        [(s("app.models.Special.__init__"), 19, 1)]
    );
    assert_eq!(
        census["classes"],
        serde_json::json!([
            {"class": "app.models.Named", "modules": ["app.store"]},
            {"class": "app.models.Order", "modules": ["app.models", "app.store", "app.views"]},
            {"class": "app.models.Special", "modules": ["app.models"]},
        ])
    );
    assert_eq!(
        census["totals"],
        serde_json::json!({
            "classes": 3, "attributes": 4, "write_sites": 10, "initializer_sites": 4,
            "read_sites": 7, "modules": 3, "functions": 10,
        }),
        "{census:#}"
    );
    let counted = facts_with(&p, &["--totals"], 0)["census"].clone();
    assert_eq!(counted, serde_json::json!({ "totals": census["totals"] }));
}

#[test]
fn the_census_lists_each_modules_functions_and_what_they_reach() {
    let p = Project::new("recommend_census_modules");
    p.write(
        "app/domain.py",
        "def total(xs):\n    return sum(xs)\n\n\ndef double(x):\n    return 2 * x\n",
    );
    p.write(
        "app/io.py",
        r#"import requests
import stripe
from app.domain import total
from app.jobs import run
import lib


def fetch(url):
    return requests.get(url)


def pay():
    stripe.pay()


def summed(xs):
    return total(xs)


def job(client):
    run(client)


def evaluated(text):
    return eval(text)


def vendored():
    lib.helper()


def stream():
    yield stripe.events()


def opener():
    return stream()


def reader():
    for event in opener():
        pass
"#,
    );
    p.write("app/jobs.py", "def run(client):\n    client.go()\n");
    p.write("lib.pyi", "def helper() -> None: ...\n");
    p.write("app/twice.py", "def one():\n    return 1\n");
    p.write("app/twice.pyi", "def one() -> int: ...\n");
    let census = census(&p);
    let modules: Vec<(String, u64, Vec<String>, Vec<String>)> = census["modules"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            let names = |k: &str| {
                m[k].as_array()
                    .unwrap()
                    .iter()
                    .map(|f| f.as_str().unwrap().to_owned())
                    .collect::<Vec<_>>()
            };
            (
                m["module"].as_str().unwrap().to_owned(),
                m["functions"].as_u64().unwrap(),
                names("blocking"),
                names("unseen"),
            )
        })
        .collect();
    let names = |v: &[&str]| v.iter().map(|n| s(n)).collect::<Vec<_>>();
    assert_eq!(
        modules,
        [
            (s("app.domain"), 2, vec![], vec![]),
            (
                s("app.io"),
                9,
                names(&["app.io.fetch"]),
                // An uncatalogued call, an unresolved one a callee makes, an A1 hazard, a function
                // known only from a stub, and a generator's call where its body runs: in itself
                // and where a helper's result is iterated, not in the helper returning it.
                names(&[
                    "app.io.evaluated",
                    "app.io.job",
                    "app.io.pay",
                    "app.io.reader",
                    "app.io.stream",
                    "app.io.vendored",
                ]),
            ),
            (s("app.jobs"), 1, vec![], names(&["app.jobs.run"])),
            // Defined in the implementation and its stub: counted once.
            (s("app.twice"), 1, vec![], vec![]),
            (s("lib"), 1, vec![], names(&["lib.helper"])),
        ],
        "{census:#}"
    );
}
