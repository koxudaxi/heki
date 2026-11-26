//! An attribute annotated with a bare container class (`self.handlers: list = []`) is that
//! class, and the items its stores give it are the items of the attribute, as they are without
//! the annotation. An annotation that names its items keeps them, and stores of two classes
//! leave the items unknown.

mod common;

use common::Project;
use std::collections::BTreeSet;

const HEADER: &str = r#"import time
from collections import deque


class Handler:
    def work(self) -> None:
        time.sleep(1)


class Other:
    def work(self) -> None:
        pass
"#;

/// The `EFF101` reports of a project holding `body` after the header, each as its line (in
/// `body`) and its tier ([`common::tiered_code`]).
fn reports(name: &str, body: &str) -> BTreeSet<(u64, String)> {
    let p = Project::new(name);
    p.write("mod.py", &format!("{HEADER}{body}"));
    let (_, out, err) = p.check(&["--select", "EFF101", "--output-format", "json", "mod.py"]);
    assert!(!err.contains("error"), "{err}");
    let found: Vec<serde_json::Value> =
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    let header = HEADER.lines().count() as u64;
    found
        .iter()
        .map(|d| {
            let line = d["line"].as_u64().expect("a report has a line");
            (line - header, common::tiered_code(d).to_owned())
        })
        .collect()
}

/// The class `Bare` of the issue, with the annotation `annotation` of the attribute and the
/// `store` that adds a `Handler` to it: the loop in `run` reports `h.work()` (line 13 of the
/// body).
fn attribute(annotation: &str, init: &str, store: &str) -> String {
    format!(
        r#"
class Bare:
    def __init__(self) -> None:
        self.items{annotation} = {init}

    def add(self) -> None:
        {store}

    async def run(self) -> None:
        for h in self.items{iterate}:
            h.work()
"#,
        iterate = if init.starts_with('{') && !init.starts_with("{}")
            || init == "{}" && store.contains('[')
        {
            ".values()"
        } else {
            ""
        },
    )
}

fn reported(annotation: &str, init: &str, store: &str) -> BTreeSet<(u64, String)> {
    reports("bare", &attribute(annotation, init, store))
}

fn exact(line: u64) -> BTreeSet<(u64, String)> {
    BTreeSet::from([(line, "EFF101".to_owned())])
}

#[test]
fn a_bare_container_annotation_keeps_the_items_the_stores_give() {
    // `h.work()` is line 11 of the body: the class starts on line 2.
    let line = 11;
    for (annotation, init, store) in [
        (": list", "[]", "self.items.append(Handler())"),
        (": dict", "{}", r#"self.items["a"] = Handler()"#),
        (": set", "set()", "self.items.add(Handler())"),
        (": deque", "deque()", "self.items.append(Handler())"),
        // The controls: the same stores without an annotation, and with one naming the items.
        ("", "[]", "self.items.append(Handler())"),
        (": list[Handler]", "[]", "self.items.append(Handler())"),
    ] {
        assert_eq!(
            reported(annotation, init, store),
            exact(line),
            "{annotation} {init} {store}"
        );
    }
}

#[test]
fn an_annotation_naming_the_items_keeps_them() {
    let body = attribute(": list[Other]", "[]", "self.items.append(Handler())");
    assert_eq!(reports("named_items", &body), BTreeSet::new());
}

#[test]
fn stores_of_two_classes_leave_the_items_unknown() {
    let body = r#"
class Bare:
    def __init__(self) -> None:
        self.items: list = []

    def add(self) -> None:
        self.items.append(Handler())
        self.items.append(Other())

    async def run(self) -> None:
        for h in self.items:
            h.work()
"#;
    // At most the heuristic report the same stores give without the annotation.
    assert_eq!(
        reports("two_classes", body),
        BTreeSet::from([(12, "EFF101 [heuristic]".to_owned())])
    );
    let unannotated = body.replace("items: list", "items");
    assert_eq!(
        reports("two_classes_plain", &unannotated),
        reports("two_classes", body)
    );
}

#[test]
fn a_bare_global_annotation_is_unchanged() {
    let body = r#"
REGISTRY: dict = {}


def put() -> None:
    REGISTRY["a"] = Handler()


async def run() -> None:
    for h in REGISTRY.values():
        h.work()
"#;
    assert_eq!(reports("global", body), exact(11));
}

/// Where the stores do not give the annotated class's items, the attribute stays that class
/// with unknown items: a store of an unknown value, a store of another container class, and an
/// annotation no store accompanies. Nothing is reported on their items.
#[test]
fn stores_that_give_no_items_leave_the_annotation_as_it_is() {
    let body = r#"
def make():
    return []


class Unknown:
    def __init__(self) -> None:
        self.items: list = make()

    def add(self) -> None:
        self.items.append(Handler())

    async def run(self) -> None:
        for h in self.items:
            h.work()


class Mixed:
    def __init__(self) -> None:
        self.items: list = []

    def swap(self) -> None:
        self.items = deque()

    def count(self) -> None:
        self.size = len(self.items)

    async def run(self) -> None:
        for h in self.items:
            h.work()


class Declared:
    items: list

    def add(self) -> None:
        self.size = len(self.items)

    async def run(self) -> None:
        for h in self.items:
            h.work()
"#;
    assert_eq!(reports("no_items", body), BTreeSet::new());
}
