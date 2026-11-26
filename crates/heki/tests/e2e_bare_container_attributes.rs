//! An attribute annotated with a bare container class (`self.handlers: list = []`) is that
//! class, and the items its stores give it are the items of the attribute, as they are without
//! the annotation: also those other modules and subclasses store. An annotation that names its
//! items keeps them, and stores of two classes leave the items unknown. A container built from
//! an iterable passed by keyword (`deque(iterable=xs)`) holds its items, and one built from an
//! iterable `*` or `**` unpacking may pass holds unknown ones.

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
    reports_with(name, body, None)
}

/// [`reports`] of a project that also holds the module `other.py` with the text `other`.
fn reports_with(name: &str, body: &str, other: Option<&str>) -> BTreeSet<(u64, String)> {
    let p = Project::new(name);
    p.write("mod.py", &format!("{HEADER}{body}"));
    let mut args = vec!["--select", "EFF101", "--output-format", "json", "mod.py"];
    if let Some(other) = other {
        p.write("other.py", other);
        args.push("other.py");
    }
    let (_, out, err) = p.check(&args);
    assert!(!err.contains("error"), "{err}");
    let found: Vec<serde_json::Value> =
        serde_json::from_str(&out).unwrap_or_else(|e| panic!("{e}: {out}"));
    let header = HEADER.lines().count() as u64;
    found
        .iter()
        .map(|d| {
            assert_eq!(d["path"], "mod.py", "{d}");
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

/// What other modules and subclasses store into a bare-annotated attribute joins its items as
/// it does without the annotation: another module replacing the list or adding to it, and a
/// subclass's `__init__` storing another list. Each leaves `Handler` and `Other` in the list,
/// which the heuristic tier reports at most.
#[test]
fn stores_of_other_modules_and_subclasses_join_the_items() {
    let body = r#"
class Bare:
    def __init__(self) -> None:
        self.items: list = [Handler()]

    async def run(self) -> None:
        for h in self.items:
            h.work()
"#;
    let subclass = r#"

class Sub(Bare):
    def __init__(self) -> None:
        super().__init__()
        self.items = [Other()]
"#;
    let heuristic = BTreeSet::from([(8, "EFF101 [heuristic]".to_owned())]);
    let replaced =
        "from mod import Bare, Other\n\n\ndef swap(b: Bare) -> None:\n    b.items = [Other()]\n";
    let grown = "from mod import Bare, Other\n\n\ndef grow(b: Bare) -> None:\n    b.items.append(Other())\n";
    for (name, body, other) in [
        ("replaced", body.to_owned(), Some(replaced)),
        ("grown", body.to_owned(), Some(grown)),
        ("subclass", format!("{body}{subclass}"), None),
    ] {
        let annotated = reports_with(name, &body, other);
        assert_eq!(annotated, heuristic, "{name}");
        let unannotated = body.replace("items: list", "items");
        let plain = format!("{name}_plain");
        assert_eq!(
            reports_with(&plain, &unannotated, other),
            annotated,
            "{name}"
        );
    }
    // Alone, the module's own store gives the items.
    assert_eq!(reports("alone", body), exact(8));
}

/// The class `Built`, whose `__init__` binds `self.items` to `init` (with `annotation`), and
/// whose `add` adds a `Handler` with `store`: the loop in `run` calls `h.work()` on line 11 of
/// the body.
fn built(annotation: &str, init: &str, store: &str, iterate: &str) -> String {
    format!(
        r#"
class Built:
    def __init__(self, *parts, **named) -> None:
        self.items{annotation} = {init}

    def add(self) -> None:
        {store}

    async def run(self) -> None:
        for h in self.items{iterate}:
            h.work()
"#
    )
}

/// The reports on `Built` with the bare container annotation `annotation`, and the same without
/// it, which must agree.
fn built_reports(name: &str, annotation: &str, init: &str) -> BTreeSet<(u64, String)> {
    let (store, iterate) = match annotation {
        ": dict" => (r#"self.items["b"] = Handler()"#, ".values()"),
        ": set" | ": frozenset" => ("self.items.add(Handler())", ""),
        _ => ("self.items.append(Handler())", ""),
    };
    let annotated = reports(name, &built(annotation, init, store, iterate));
    let plain = reports(&format!("{name}_plain"), &built("", init, store, iterate));
    assert_eq!(annotated, plain, "{annotation} {init}");
    annotated
}

/// `deque` takes its iterable by position or by the keyword `iterable`, and `dict` its entries
/// by position, by keyword, and from a `**` mapping: an `Other` passed any of these ways is in
/// the container with the `Handler` added, which the heuristic tier reports at most. An
/// iterable `*` or `**` unpacking may pass gives unknown items, on which nothing is reported.
/// `deque(maxlen=3)` passes no iterable: it holds only the `Handler`.
#[test]
fn an_iterable_passed_by_keyword_or_unpacking_gives_its_items() {
    let heuristic = BTreeSet::from([(11, "EFF101 [heuristic]".to_owned())]);
    for (annotation, init) in [
        (": deque", "deque([Other()])"),
        (": deque", "deque(iterable=[Other()])"),
        (": deque", "deque(iterable=[Other()], maxlen=3)"),
        (": dict", "dict(a=Other())"),
        (": dict", r#"dict(**{"a": Other()})"#),
        (": dict", r#"dict({"a": Handler()}, c=Other())"#),
    ] {
        assert_eq!(
            built_reports("keyword", annotation, init),
            heuristic,
            "{annotation} {init}"
        );
    }
    for (annotation, init) in [
        (": deque", "deque(*parts)"),
        (": deque", "deque(**named)"),
        (": dict", "dict(*parts)"),
        (": dict", "dict(**named)"),
        (": dict", "dict(*parts, c=Other())"),
        (": list", "list(*parts)"),
        (": set", "set(*parts)"),
        (": frozenset", "frozenset(*parts)"),
        (": list", "list(tuple(*parts))"),
    ] {
        assert_eq!(
            built_reports("unpacked", annotation, init),
            BTreeSet::new(),
            "{annotation} {init}"
        );
    }
    assert_eq!(
        built_reports("maxlen", ": deque", "deque(maxlen=3)"),
        exact(11)
    );
    // `enumerate` also takes its iterable by the keyword; `zip` and `itertools.chain` over
    // iterables `*` unpacks give unknown items.
    let body = r#"
import itertools


async def run(groups: list) -> None:
    for _, h in enumerate(iterable=[Handler()]):
        h.work()
    for h in itertools.chain(*groups):
        h.work()
    for (h,) in zip(*groups):
        h.work()
"#;
    assert_eq!(reports("enumerate", body), exact(7));
}

/// A dataclass field annotated with a bare container class holds what the generated `__init__`
/// binds, which no store of the project shows: a subclass's own store (here only when the
/// field is empty) does not give the items of the list.
#[test]
fn a_field_the_generated_init_binds_leaves_the_items_unknown() {
    let body = r#"
from dataclasses import dataclass, field


@dataclass
class Base:
    items: list = field(default_factory=list)


class Sub(Base):
    def __init__(self, items: list) -> None:
        super().__init__(items)
        if not self.items:
            self.items = [Handler()]

    async def run(self) -> None:
        for h in self.items:
            h.work()
"#;
    assert_eq!(reports("field", body), BTreeSet::new());
}

/// `list`, `set`, `frozenset` and `tuple` take their iterable by position only, so a `**`
/// mapping passes them none: `list(**named)` is `[]` for an empty mapping and raises
/// `TypeError` for any other. The `Handler` added is the only item.
#[test]
fn list_set_frozenset_and_tuple_take_the_iterable_by_position_only() {
    for (annotation, init) in [
        (": list", "list(**named)"),
        (": set", "set(**named)"),
        (": list", "list(tuple(**named))"),
    ] {
        assert_eq!(
            built_reports("positional", annotation, init),
            exact(11),
            "{annotation} {init}"
        );
    }
}
