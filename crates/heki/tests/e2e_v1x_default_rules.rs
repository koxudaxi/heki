//! The v1.x default rules end to end: `MUT107` (an attribute `__hash__` reads, mutated after
//! initialization), `OPT112` (`list.index` / `remove` / `count` inside a loop), and `OPT113`
//! (nested-loop equi-joins) — true positives, every false-positive filter, inline
//! suppressions, the ratchet baseline, and `heki rule`.

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

/// `(path, line, rule, qualname)` of every diagnostic.
fn found(diags: &[Value]) -> Vec<(String, u64, String, String)> {
    diags
        .iter()
        .map(|d| {
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["rule_code"].as_str().unwrap().to_owned(),
                d["qualname"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn d(path: &str, line: u64, rule: &str, qualname: &str) -> (String, u64, String, String) {
    (path.into(), line, rule.into(), qualname.into())
}

/// `target` of every diagnostic.
fn targets(diags: &[Value]) -> Vec<&str> {
    diags
        .iter()
        .map(|d| d["target"].as_str().unwrap())
        .collect()
}

/// The rules only report: no diagnostic carries a fix, so `--fix` never applies one.
fn assert_no_fix(diags: &[Value]) {
    for d in diags {
        assert!(d["fix"].is_null(), "{d:#}");
    }
}

// ==================================================================================== MUT107

const SHAPES: &str = r#"import threading


class Tagged:
    def __init__(self, name):
        self.name = name
        self.tags = []
        self.meta = {"id": 1, "note": ""}
        self.label = None
        self.parts = []
        self.alias = None
        self.slot = ""
        self.code = None
        self.lock = threading.Lock()
        self._cache = {}
        self.index = {}
        self._load()

    def _load(self):
        self.tags = ["default"]

    def _configure_meta(self):
        self.meta["id"] = 2

    def __setstate__(self, state):
        self.name = state

    def __hash__(self):
        return hash(self.key())

    def key(self):
        return (
            self.name,
            tuple(self.tags),
            self.meta["id"],
            self.label,
            tuple(self.parts),
            self.alias,
            self.slot,
            self.code,
            len(self._cache),
            len(self.index),
        )

    def rename(self, name):
        self.name = name

    def tag(self, t):
        tags = self.tags
        tags.append(t)

    def annotate(self, note):
        self.meta["note"] = note

    def ensure_label(self):
        if self.label is None:
            with self.lock:
                self.label = self.name.upper()
        return self.label

    def ensure_parts(self):
        if not self.parts:
            self.parts = list(self.tags)

    def ensure_alias(self):
        current = self.alias
        if current == None:
            self.alias = self.name

    def ensure_slot(self):
        if self.slot:
            return
        self.slot = "x"

    def ensure_code(self):
        if self.code is not None:
            return self.code
        self.code = 1
        return self.code

    def ensure_code_ne(self):
        if self.code != None:
            return self.code
        else:
            self.code = 2
        return self.code

    def recode(self, value):
        if self.code is not None:
            print(self.code)
        self.code = value

    def relabel(self, value):
        if self.label is None:
            self.label = "default"
        self.label = value

    def remember(self, k, v):
        self._cache[k] = v

    def lookup(self, k):
        if k not in self.index:
            self.index[k] = k
        self.index.setdefault(k, k)
        return self.index[k]

    def suppressed(self):
        self.name = ""  # heki: ignore[MUT107]


class SubTagged(Tagged):
    def reset(self):
        self.name = ""


class Cached:
    def __init__(self, v):
        self.v = v
        self._h = None

    def __hash__(self):
        return self._compute()

    def _compute(self):
        if self._h is None:
            self._h = hash(self.v)
        return self._h

    def set_v(self, v):
        self.v = v
        self._h = None


class Base:
    def __init__(self, name):
        self.name = name

    def __hash__(self):
        return hash(self.name)


class Child(Base):
    def __hash__(self):
        return hash((self.name, 1))

    def rename(self, name):
        self.name = name


class Outer:
    def __init__(self):
        self.name = "o"

    def rename(self, n):
        self.name = n

    class Inner:
        def __init__(self):
            self.name = "i"

        def __hash__(self):
            return hash(self.name)

        def rename(self, n):
            self.name = n


class Plain:
    def __init__(self):
        self.name = ""

    def rename(self, n):
        self.name = n


class Point:
    def __init__(self, x):
        self.x = x
        self._hash = None

    def __hash__(self):
        if self._hash is None:
            self._hash = hash(self.x)
        return self._hash

    def move(self, dx):
        self.x += dx
        self._hash = None


class Version:
    def __init__(self, major):
        self.major = major
        self.minor = None

    def __hash__(self):
        return hash((self.major, self.minor))

    def ensure_minor(self):
        if None is self.minor:
            self.minor = 0

    def bump(self):
        if self.major == 0:
            self.major = 1


class Stamp:
    def __new__(cls, state):
        self = object.__new__(cls)
        self.__setstate(state)
        return self

    def __setstate(self, state):
        self._day = state

    def _shift(self):
        self._day += 1

    def __hash__(self):
        return hash(self._day)


class Node:
    def __init_subclass__(cls):
        cls.keys = ("kind",)

    def __init__(self, origin):
        self.origin = origin
        self._hash = hash(origin)

    def _deserialize(self):
        self._hash = hash(self.origin)

    def __hash__(self):
        return hash((self._hash, self.keys))
"#;

const REMOTE: &str = r#"from shapes import Tagged


class Remote(Tagged):
    def rename_remote(self):
        self.name = "r"
"#;

#[test]
fn mut107_reports_writes_after_initialization_and_keeps_every_filter() {
    let p = Project::new("v1x_mut107");
    p.write("shapes.py", SHAPES).write("other.py", REMOTE);
    // Default-enabled: no `--select`.
    let diags: Vec<Value> = diagnostics(&p, &[])
        .into_iter()
        .filter(|d| d["rule_code"] == "MUT107")
        .collect();
    let s = "shapes.py";
    // Reported: a direct write, a write through a local alias, a fall-through after an
    // `is not None` check, the second (unguarded) write after a lazy one, subclass methods
    // (also in another module, and against `Child.__hash__`, which `Child` instances use instead
    // of `Base.__hash__`),
    // writes of attributes `__hash__` reads through `self.key()` / its `self._compute()` cache
    // helper / directly, a nested class, a write under a check that is not a `None` one, and
    // a private method no initializer calls.
    // Not reported: initialization (`_load` called from `__init__`, `_configure_*`,
    // `__setstate__`, `__setstate` called from `__new__` on the instance it allocates), a
    // class attribute set by the implicit class method `__init_subclass__`, a disjoint
    // constant key (`meta["note"]`), lazy initialization (`is None` around a `with`, `not`,
    // `== None` through a copy, a falsy early return, `is not None` / `!= None` early
    // returns, `None is`), memo dictionaries (`_cache`, the `not in` guard, `setdefault`),
    // resets of the hash's own cache (`self._h = None` / `self._hash = None`), a stored hash
    // a deserialization hook recomputes (`Node._deserialize`), the suppressed write, a class
    // whose `__hash__` is in a nested class only (`Outer`), and a class without `__hash__`.
    assert_eq!(
        found(&diags),
        vec![
            d("other.py", 6, "MUT107", "Remote.rename_remote"),
            d(s, 46, "MUT107", "Tagged.rename"),
            d(s, 50, "MUT107", "Tagged.tag"),
            d(s, 91, "MUT107", "Tagged.recode"),
            d(s, 96, "MUT107", "Tagged.relabel"),
            d(s, 113, "MUT107", "SubTagged.reset"),
            d(s, 130, "MUT107", "Cached.set_v"),
            d(s, 147, "MUT107", "Child.rename"),
            d(s, 165, "MUT107", "Outer.Inner.rename"),
            d(s, 187, "MUT107", "Point.move"),
            d(s, 205, "MUT107", "Version.bump"),
            d(s, 218, "MUT107", "Stamp._shift"),
        ],
        "{diags:#?}"
    );
    assert_eq!(
        targets(&diags),
        vec![
            "Tagged.name->__hash__",
            "Tagged.name->__hash__",
            "Tagged.tags->__hash__",
            "Tagged.code->__hash__",
            "Tagged.label->__hash__",
            "Tagged.name->__hash__",
            "Cached.v->__hash__",
            "Child.name->__hash__",
            "Outer.Inner.name->__hash__",
            "Point.x->__hash__",
            "Version.major->__hash__",
            "Stamp._day->__hash__",
        ]
    );
    assert_eq!(
        diags[1]["message"],
        "`Tagged.rename` mutates `self.name`, which `Tagged.__hash__` reads, so instances already stored in a `set` or used as `dict` keys can no longer be found"
    );
    assert_eq!(diags[1]["rule_slug"], "hash-attr-mutation");
    assert_no_fix(&diags);

    // `--select` / `--ignore` and per-file ignores turn it off.
    assert!(diagnostics(&p, &["--select", "MUT101"]).is_empty());
    assert!(
        diagnostics(&p, &["--ignore", "hash-attr-mutation"])
            .iter()
            .all(|d| d["rule_code"] != "MUT107")
    );
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nper-file-ignores = { \"shapes.py\" = [\"MUT107\"] }\n",
    );
    assert_eq!(
        found(&diagnostics(&p, &["--select", "MUT107"])),
        vec![d("other.py", 6, "MUT107", "Remote.rename_remote")]
    );
}

// ==================================================================================== OPT112

const CONSTS: &str = r#"LEVELS = ["low", "mid", "high"]
SIZES = ["xs", "s", "m", "l", "xl", "2xl", "3xl", "4xl", "5xl", "6xl", "7xl", "8xl", "9xl", "10xl", "11xl", "12xl", "13xl"]
"#;

const LOOKUPS: &str = r#"import time
from typing import ClassVar, List

import consts
from consts import LEVELS

KINDS = ["a", "b", "c"]
REGISTRY = []


def register(x):
    REGISTRY.append(x)


class Row:
    cells: list[int]


class Bag(list):
    pass


class Pile:
    def remove(self, x):
        pass


def positions(names: list[str], wanted):
    out = []
    for w in wanted:
        out.append(names.index(w))
    return out


def drop_all(items: List[int], unwanted):
    alias = items
    for x in unwanted:
        alias.remove(x)


def duplicates(xs):
    items = list(xs)
    for x in xs:
        if items.count(x) > 1:
            print(x)


def globals_and_constants(xs):
    for x in xs:
        KINDS.index(x)
        REGISTRY.index(x)
        LEVELS.index(x)
        consts.LEVELS.index(x)
        consts.SIZES.index(x)
        ["a", "b"].index(x)


def displays(xs, first, last):
    small = ["a", "b"]
    grow = ["a"]
    stored = ["a"]
    added = ["a"]
    other = added
    pair = [first, last]
    seventeen = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17]
    for x in xs:
        small.index(x)
        grow.count(x)
        grow.append(x)
        stored.index(x)
        stored[0] = x
        added.index(x)
        other += [x]
        pair.index(x)
        seventeen.index(x)


def one_shot(items: list[int], xs):
    for x in xs:
        if x:
            first = items.index(x)
            break
    for x in xs:
        return items.index(x)


def both_branches(items: list[int], xs):
    for x in xs:
        if items.index(x):
            return 1
        else:
            return 2


def conditional_break(items: list[int], xs):
    for x in xs:
        i = items.index(x)
        if i:
            break


def bounded(items: list[int], fn):
    for i in range(3):
        items.remove(i)
    for attempt in range(5):
        try:
            return items.index(fn())
        except ValueError:
            time.sleep(1)


class Worker:
    names: list[str]

    def __init__(self):
        self.items = list(self.names)
        self._running = True

    def drop(self, xs):
        for x in xs:
            self.items.remove(x)
            self.names.count(x)

    def serve(self, x):
        while self._running:
            self.items.remove(x)


def per_iteration(rows, xs, idxs, matrix):
    for x in xs:
        parts = [1, 2, 3, 4, 5, 6, 7, 8, 9]
        parts.index(x)
    for r in rows:
        row: Row = r
        row.cells.index(1)
    for i in idxs:
        line: list[int] = matrix[i]
        line.index(0)
    for x in xs:
        head: list[int] = matrix[0]
        head.index(x)


def not_lists(s: str, st: set[int], bag: Bag, pile: Pile, things, xs):
    for x in xs:
        s.index("a")
        s.count("a")
        st.remove(x)
        bag.remove(x)
        pile.remove(x)
        things.remove(x)


def header_only(items: list[int], x):
    for i in range(items.count(x)):
        print(i)


def comprehension(items: list[int], xs):
    return [items.index(x) for x in xs]


def phis(a: list[int], b: list[int], xs, c):
    t = a if c else b
    for x in xs:
        (a if c else b).remove(x)
        t.remove(x)
        u = a if c else []
        u.remove(x)


def rebound(items: list[int], xs):
    for x in xs:
        items.remove(x)
        items = list(items)


def nested(groups, xs):
    for g in groups:
        seen = []
        for y in xs:
            if seen.count(y) == 0:
                seen.append(y)


def drain(items: list[int], x):
    while items.count(x):
        items.remove(x)


def suppressed(items: list[int], xs):
    for x in xs:
        items.remove(x)  # heki: ignore[OPT112]


for _x in REGISTRY:
    KINDS.count(_x)
    REGISTRY.count(_x)


class Sniffer:
    def __init__(self):
        self.preferred = [",", "\t", ";"]
        self.seen = ["a"]

    def guess(self, rows, other: "Sniffer"):
        for row in rows:
            self.preferred.index(row)
            other.preferred.count(row)
            self.seen.index(row)

    def note(self, x):
        self.seen.append(x)


class Compiler:
    order: ClassVar[list[str]] = ["c++", "objc", "c"]
    history: ClassVar[list[str]] = []

    def pick(self, langs):
        for lang in langs:
            self.order.index(lang)
            self.history.count(lang)

    def log(self, lang):
        self.history.append(lang)


def cursor(chunks: list[str], marks):
    pos = 0
    for m in marks:
        pos = chunks.index(m, pos)


SUFFIXES = (".py", ".pyi")


def small_variables(abis: list[str], threaded):
    explicit = ("abi3", "abi3t", "none") if threaded else ("abi3", "none")
    for e in explicit:
        abis.remove(e)
    for s in SUFFIXES:
        abis.remove(s)


MODES = ["fast", "slow"]
DEFAULT_MODES = ["fast"]


def configure():
    global MODES
    MODES = DEFAULT_MODES


def modes(xs):
    for x in xs:
        MODES.index(x)
"#;

#[test]
fn opt112_reports_linear_scans_of_long_lived_lists_in_data_sized_loops() {
    let p = Project::new("v1x_opt112");
    p.write("consts.py", CONSTS).write("lookups.py", LOOKUPS);
    let diags: Vec<Value> = diagnostics(&p, &[])
        .into_iter()
        .filter(|d| d["rule_code"] == "OPT112")
        .collect();
    let l = "lookups.py";
    // Reported: annotated parameters (`list[str]`, `List[int]`, through an alias), `list(...)`,
    // a module global another function appends to or rebinds to another global (no known
    // display), a 17-element module constant, displays that grow (append, `+=` through an
    // alias, stored where other code may grow them) or of more than 16 elements, a conditional
    // `break`, `self.attr` lists (from `__init__`, from a class annotation, an instance or
    // class attribute another method appends to), a list behind a constant subscript, a
    // comprehension, phis of long-lived lists (in the loop and before it), a loop-carried
    // rebinding, an inner loop over a list of the outer iteration, a `while` condition and
    // body, and module-level code.
    // Not reported: small lists (displays of at most 16 items that nothing grows, also when
    // their items are not constants or are replaced in place: local, module global, imported,
    // `module.NAME`, an instance attribute its class only assigns one, also on another
    // instance, a class attribute), displays built per iteration, one-shot calls (`break` /
    // `return` on every path), small constant / retry / daemon loops (also over a local or
    // module constant tuple), lists of the loop element or behind a loop-varying subscript,
    // `str` / `set` / `list` subclass / project class / unknown receivers, the `for` iterable,
    // a list that may be a fresh one, `index(x, start)` (a moving cursor), and the suppressed
    // call.
    assert_eq!(
        found(&diags),
        vec![
            d(l, 31, "OPT112", "positions"),
            d(l, 38, "OPT112", "drop_all"),
            d(l, 44, "OPT112", "duplicates"),
            d(l, 51, "OPT112", "globals_and_constants"),
            d(l, 54, "OPT112", "globals_and_constants"),
            d(l, 68, "OPT112", "displays"),
            d(l, 72, "OPT112", "displays"),
            d(l, 75, "OPT112", "displays"),
            d(l, 97, "OPT112", "conditional_break"),
            d(l, 121, "OPT112", "Worker.drop"),
            d(l, 122, "OPT112", "Worker.drop"),
            d(l, 141, "OPT112", "per_iteration"),
            d(l, 160, "OPT112", "comprehension"),
            d(l, 166, "OPT112", "phis"),
            d(l, 167, "OPT112", "phis"),
            d(l, 174, "OPT112", "rebound"),
            d(l, 182, "OPT112", "nested"),
            d(l, 187, "OPT112", "drain"),
            d(l, 188, "OPT112", "drain"),
            d(l, 198, "OPT112", "<module>"),
            d(l, 210, "OPT112", "Sniffer.guess"),
            d(l, 223, "OPT112", "Compiler.pick"),
            d(l, 257, "OPT112", "modes"),
        ],
        "{diags:#?}"
    );
    assert_eq!(
        targets(&diags),
        vec![
            "names",
            "alias",
            "items",
            "REGISTRY",
            "consts.SIZES",
            "grow",
            "added",
            "seventeen",
            "items",
            "self.items",
            "self.names",
            "head",
            "items",
            "(a if c else b)",
            "t",
            "items",
            "seen",
            "items",
            "items",
            "REGISTRY",
            "self.seen",
            "self.history",
            "MODES",
        ]
    );
    let message = |i: usize| diags[i]["message"].as_str().unwrap();
    let help = |i: usize| diags[i]["help"].as_str().unwrap();
    assert_eq!(
        (message(0), help(0)),
        (
            "`names.index()` scans the list each time it runs in the loop",
            "build a `dict` (value → index) or a `set` once before the loop"
        )
    );
    assert_eq!(
        (message(1), help(1)),
        (
            "`alias.remove()` scans the list each time it runs in the loop",
            "collect the values to drop in a `set` and rebuild the list once with a comprehension"
        )
    );
    assert_eq!(
        (message(2), help(2)),
        (
            "`items.count()` scans the list each time it runs in the loop",
            "count once before the loop with `collections.Counter(items)`"
        )
    );
    assert_eq!(diags[0]["rule_slug"], "loop-linear-list-lookup");
    assert_no_fix(&diags);

    // `--fix` applies nothing for it (the default fix only covers `OPT101` / `OPT102`).
    let (_, _, stderr) = p.check(&["--select", "OPT112", "--fix"]);
    assert!(stderr.contains("Fixed 0 issue(s)"), "{stderr}");
    assert_eq!(p.read("lookups.py"), LOOKUPS);
}

// ==================================================================================== OPT113

const JOINS: &str = r#"def attach(orders, customers):
    for order in orders:
        for customer in customers:
            if order.customer_id == customer.id:
                order.customer = customer
                break


def reversed_sides(orders, customers):
    for order in orders:
        for customer in customers:
            if customer.id == order.customer_id:
                print(order)


def comprehension(orders, customers):
    return [[c for c in customers if c.id == o.cid] for o in orders]


def unpacked(pairs, rows):
    for k, v in pairs:
        for rk, rv in rows:
            if k == rk:
                print(v, rv)


def subscripts(a_rows, b_rows):
    for a in a_rows:
        for b in b_rows:
            if a["k"] == b["k"] and b[0]:
                print(a, b)
            if a[0] == b[1]:
                print(a)


def chained(items, others):
    for a in items:
        for b in others:
            if a.x.y == b.x.y and b.flag:
                print(a)


def common(xs, ys):
    out = []
    for x in xs:
        for y in ys:
            if x == y:
                out.append(x)
    return out


def stored(A, B):
    for a in A:
        for b in B:
            same = a.k == b.k
            if same:
                print(a)


def three_deep(A, B, C):
    for a in A:
        for b in B:
            for c in C:
                if c.k == a.k:
                    print(c)


def two_matches(A, B):
    for a in A:
        for b in B:
            if b.y == a.y:
                print(b)
            if a.x == b.x:
                print(a)


def traversal(nodes, index, fn):
    for a in nodes:
        for b in a.children:
            if a.k == b.k:
                print(a)
        for b in index[a.k]:
            if a.k == b.k:
                print(b)
        for b in fn(a):
            if a.k == b.k:
                print(b)


def per_element(A):
    for a in A:
        for x in a.items:
            for y in x.parts:
                if y.k == a.k:
                    print(y)


def skip_self(items):
    for a in items:
        for b in items:
            if a == b:
                continue
            print(a, b)


def skip_self_or(items):
    for a in items:
        for b in items:
            if a == b or b.hidden:
                continue
            print(a, b)


def non_equality(A, B):
    for a in A:
        for b in B:
            if not a.k == b.k:
                print(a)
            if a.k != b.k:
                print(b)
            if a.k < b.k:
                print(b)
            if a.k == 3 or b.k == a.j.k():
                print(b)


def results(A, B):
    out = []
    for a in A:
        for b in B:
            out.append(a.k == b.k)
    return out


def small(A):
    for a in A:
        for b in ("x", "y"):
            if a == b:
                print(a)
    for i in range(3):
        for a in A:
            if a == i:
                print(a)


def whiles(A, B):
    i = 0
    while i < len(A):
        j = 0
        while j < len(B):
            if A[i] == B[j]:
                print(i)
            j += 1
        i += 1
    for a in A:
        while a.next:
            if a.k == a.next.k:
                print(a)


def same_loop(A):
    for a in A:
        if a.x == a.y:
            print(a)
        for b in A:
            pass


def suppressed(A, B):
    for a in A:
        for b in B:
            if a.k == b.k:  # heki: ignore[OPT113]
                print(a)


def rotating(A, B):
    pool = B
    for a in A:
        for b in pool:
            if a.k == b.k:
                print(a)
        pool = pool[1:]


def computed_keys(A, B, i, pick):
    for a in A:
        for b in pick(B, B):
            if a[i] == b[i]:
                print(a)


def pairwise(items):
    for i, a in enumerate(items):
        for b in items[i + 1 :]:
            if a.k == b.k:
                print(a, b)


def keyed(A, index):
    for a in A:
        for b in index[f"{a.k}"]:
            if a.k == b.k:
                print(b)
        for b in [c for c in a.children if c.ok]:
            if a.k == b.k:
                print(b)


def filtered(A, B):
    for a in A:
        for b in [x for x in B if x.ok]:
            if a.k == b.k:
                print(b)


def nested_if(A, B):
    found = None
    for a in A:
        for b in B:
            if b.active:
                if a.k == b.k:
                    found = b
    return found


def alternatives(A, B, match):
    for a in A:
        for b in B:
            if a.k == b.k or match(a, b):
                print(a)
        for b in B:
            if b.any or a.k == b.k:
                print(a)


def per_pair_values(A, B):
    for a in A:
        print([1 if a.k == b.k else 0 for b in B])
        for b in B:
            if a.k == b.k:
                x = b
            else:
                x = None
            print(x)


PAIR = ("x", "y")


def small_inner(A):
    for a in A:
        for b in PAIR:
            if a == b:
                print(a)


def fetched(found, index, search):
    for title, body in found:
        for x in search(f'"{title}" in:title'):
            if x["title"] == title:
                print(x, body)
    for name in found:
        for row in index[f"{name}"]:
            if row.name == name:
                print(row)
        for row in index[f"{name!r:>{name}}"]:
            if row.name == name:
                print(row)
"#;

#[test]
fn opt113_reports_nested_loop_equijoins_but_not_traversals_or_all_pairs_loops() {
    let p = Project::new("v1x_opt113");
    p.write("joins.py", JOINS);
    let diags: Vec<Value> = diagnostics(&p, &[])
        .into_iter()
        .filter(|d| d["rule_code"] == "OPT113")
        .collect();
    let j = "joins.py";
    // Reported (once per inner loop, at its first match): attribute keys on either side, an
    // inner comprehension, unpacked components, constant subscripts (also under `and`),
    // attribute chains, whole elements, a stored comparison, a join two loops apart, the first
    // of two matches, an inner collection the outer loop rebinds without its element, a
    // comprehension over an independent collection, and a match under another `if`.
    // Not reported: traversals of the outer element (`a.children`, `index[a.k]`, `fn(a)`, an
    // element of an element, `items[i + 1:]` after `enumerate`, `index[f"{a.k}"]`, an
    // f-string of the element itself (`f"{name}"`, its conversion and specification), a
    // comprehension over `a.children`), all-pairs loops skipping matches (`continue`, also
    // under `or`), a match that is one alternative of an `or` (either side), comparisons that
    // pick a value for every pair (a conditional expression, an `if` / `else` assigning
    // both ways), negated / `!=` / ordering comparisons, comparisons with constants or
    // computed values (`a.j.k()`, `a[i]`), comparison results that are not tested, small
    // constant loops (also over a module constant tuple), `while` loops, comparisons within
    // one loop, and the suppressed join.
    assert_eq!(
        found(&diags),
        vec![
            d(j, 4, "OPT113", "attach"),
            d(j, 12, "OPT113", "reversed_sides"),
            d(j, 17, "OPT113", "comprehension"),
            d(j, 23, "OPT113", "unpacked"),
            d(j, 30, "OPT113", "subscripts"),
            d(j, 39, "OPT113", "chained"),
            d(j, 47, "OPT113", "common"),
            d(j, 55, "OPT113", "stored"),
            d(j, 64, "OPT113", "three_deep"),
            d(j, 71, "OPT113", "two_matches"),
            d(j, 180, "OPT113", "rotating"),
            d(j, 212, "OPT113", "filtered"),
            d(j, 221, "OPT113", "nested_if"),
        ],
        "{diags:#?}"
    );
    assert_eq!(
        targets(&diags),
        vec![
            "order.customer_id==customer.id",
            "customer.id==order.customer_id",
            "c.id==o.cid",
            "k==rk",
            "a[\"k\"]==b[\"k\"]",
            "a.x.y==b.x.y",
            "x==y",
            "a.k==b.k",
            "c.k==a.k",
            "b.y==a.y",
            "a.k==b.k",
            "a.k==b.k",
            "a.k==b.k",
        ]
    );
    let message = |i: usize| diags[i]["message"].as_str().unwrap();
    let help = |i: usize| diags[i]["help"].as_str().unwrap();
    assert_eq!(
        (message(0), help(0)),
        (
            "nested loops join `orders` and `customers` on `order.customer_id == customer.id`",
            "index `customers` by `customer.id` in a `dict` before the outer loop"
        )
    );
    assert_eq!(
        (message(6), help(6)),
        (
            "nested loops join `xs` and `ys` on `x == y`",
            "group the elements of `ys` by value in a `dict` (value → list of equal elements) before the outer loop, then read each outer key with `.get(key, [])` so that a key with no element keeps its empty list"
        )
    );
    assert_eq!(
        (message(8), help(8)),
        (
            "nested loops join `A` and `C` on `c.k == a.k`",
            "index `C` in a `dict` from `c.k` to the list of elements with that key before the outer loop, then read each outer key with `.get(key, [])` so that a key with no element keeps its empty list"
        )
    );
    assert_eq!(diags[0]["column"], 16);
    assert_eq!(diags[0]["rule_slug"], "nested-loop-equijoin");
    assert_no_fix(&diags);
}

const NATIVE: &str = r#"_OPERATIONS = ("open", "close", "read", "write", "seek", "tell", "flush", "stat", "list", "remove")


class Native:
    def __init__(self, config):
        self.config = config
        self.responses = {name: [call for call in config["calls"] if call["op"] == name] for name in _OPERATIONS}

    def reload(self):
        self.responses = {name: [call for call in self.config["calls"] if call["op"] == name] for name in _OPERATIONS}


def unknown(config, operations):
    return {name: [call for call in config["calls"] if call["op"] == name] for name in operations}
"#;

const KINDS: &str = r#"KINDS = ("file", "dir", "link", "fifo", "socket", "block", "char", "door", "port", "whiteout")
"#;

const FIXED_JOINS: &str = r#"from kinds import KINDS

LIMIT = 10
SIXTEEN = ("a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p")
SEVENTEEN = ("a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q")


class Table:
    COLUMNS = ("id", "name", "kind", "size", "owner", "group", "mode", "created", "updated", "path")

    def __init__(self):
        self.levels = ["trace", "debug", "info", "notice", "warning", "error", "critical", "alert", "fatal"]

    def by_column(self, cells):
        return {c: [cell for cell in cells if cell.column == c] for c in self.COLUMNS}

    def by_level(self, records):
        return {lv: [r for r in records if r.level == lv] for lv in self.levels}


def small_list(rows):
    out = {}
    for name in ["a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l"]:
        out[name] = [r for r in rows if r.name == name]
    return out


def ranges(rows):
    first = [[r for r in rows if r.k == i] for i in range(10)]
    second = [[r for r in rows if r.k == i] for i in range(LIMIT)]
    return first, second


def imported(rows):
    return {k: [r for r in rows if r.kind == k] for k in KINDS}


def fixed_inner(rows):
    for r in rows:
        for name in SIXTEEN:
            if r.name == name:
                print(r)


def too_long(rows):
    first = {n: [r for r in rows if r.name == n] for n in SEVENTEEN}
    second = [[r for r in rows if r.k == i] for i in range(17)]
    return first, second
"#;

#[test]
fn opt113_skips_joins_whose_loop_size_the_code_fixes() {
    let p = Project::new("v1x_opt113_fixed");
    p.write("native.py", NATIVE)
        .write("kinds.py", KINDS)
        .write("fixed.py", FIXED_JOINS);
    let diags = diagnostics(&p, &["--select", "OPT113"]);
    // Reported: an outer loop over a parameter, over a class attribute or an instance
    // attribute, and fixed sizes past 16 (a 17-item module constant, `range(17)`).
    // Not reported: one side fixed by the code, so the cost grows with the other side only: a
    // module constant tuple of string literals (outer loop of a dict comprehension filtering a
    // dict's list, also of `self.config`), a list display, `range(10)` and `range` of a module
    // constant, a constant of another module, and a fixed inner loop.
    assert_eq!(
        found(&diags),
        vec![
            d("fixed.py", 15, "OPT113", "Table.by_column"),
            d("fixed.py", 18, "OPT113", "Table.by_level"),
            d("fixed.py", 46, "OPT113", "too_long"),
            d("fixed.py", 47, "OPT113", "too_long"),
            d("native.py", 14, "OPT113", "unknown"),
        ],
        "{diags:#?}"
    );
    let native = &diags[4];
    assert_eq!(
        native["message"],
        "nested loops join `operations` and `config[\"calls\"]` on `call[\"op\"] == name`"
    );
    assert_eq!(
        native["help"],
        "index `config[\"calls\"]` in a `dict` from `call[\"op\"]` to the list of elements with that key before the outer loop, then read each outer key with `.get(key, [])` so that a key with no element keeps its empty list"
    );
}

// ================================================================== baseline, `heki rule`

#[test]
fn v1x_rules_take_part_in_the_ratchet_baseline() {
    let p = Project::new("v1x_baseline");
    p.write(
        "a.py",
        "def f(items: list[int], xs):\n    for x in xs:\n        items.remove(x)\n",
    );
    let (code, _, stderr) = p.check(&["--baseline", "heki-baseline.toml", "--update-baseline"]);
    assert_eq!(code, 0, "{stderr}");
    let text = p.read("heki-baseline.toml");
    assert!(
        text.contains("rule_code = \"OPT112\"") && text.contains("target = \"items\""),
        "{text}"
    );
    // The same violation (moved) is no regression.
    p.write(
        "a.py",
        "\n\ndef f(items: list[int], xs):\n    for x in xs:\n        items.remove(x)\n",
    );
    assert_eq!(p.check(&["--baseline", "heki-baseline.toml"]).0, 0);
    // A new one is, and only it is reported.
    p.write(
        "b.py",
        "class C:\n    def __init__(self):\n        self.k = 1\n\n    def __hash__(self):\n        return hash(self.k)\n\n    def bump(self):\n        self.k += 1\n",
    );
    let (code, stdout, _) = p.check(&[
        "--baseline",
        "heki-baseline.toml",
        "--output-format",
        "concise",
    ]);
    assert_eq!(code, 1);
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert!(stdout.starts_with("b.py:9:9: MUT107 "), "{stdout}");
}

#[test]
fn heki_rule_prints_the_v1x_rule_docs() {
    let p = Project::new("v1x_rule_docs");
    for (code, slug, first) in [
        (
            "MUT107",
            "hash-attr-mutation",
            "Checks methods that rebind or mutate an attribute that the class's `__hash__` reads",
        ),
        (
            "OPT112",
            "loop-linear-list-lookup",
            "Checks `list.index()`, `list.remove()`, and `list.count()` calls",
        ),
        (
            "OPT113",
            "nested-loop-equijoin",
            "Checks nested loops that join two collections",
        ),
    ] {
        let (status, doc, _) = p.heki(&["rule", code]);
        assert_eq!(status, 0);
        assert!(doc.starts_with(&format!("# {slug} ({code})\n\n")), "{doc}");
        assert!(doc.contains(&format!("## What it does\n{first}")), "{doc}");
        assert!(
            doc.contains("## Example\n```python\n") && doc.contains("Use instead:\n```python\n"),
            "{doc}"
        );
        assert!(doc.contains("## Fix safety\nNo automatic fix"), "{doc}");
        assert_eq!(p.heki(&["rule", slug]).1, doc);
    }
}
