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
    let s = |v: &str| v.to_owned();
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
}
