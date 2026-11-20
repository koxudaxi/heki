//! `MUT113` (`iterated-container-mutation`) end to end: a dict, set, or deque of `self` changed
//! in place while a method iterates it, by code outside the loop. A generator suspended inside
//! the iteration while another method of the class family runs (what can run follows where the
//! generator object goes), and a function the loop's body calls. The container classes, the
//! views and wrappers iterated, snapshots, the methods left out (initialization, private
//! methods, the interface of a public generator), class families across modules, the opt-in
//! selection, suppressions, and `heki rule`. The shapes reported raise `RuntimeError` under
//! CPython; the snapshots do not.

mod common;

use common::{Project, run_python};
use serde_json::Value;

fn diagnostics(p: &Project, args: &[&str]) -> Vec<Value> {
    let mut all = vec!["--output-format", "json"];
    all.extend_from_slice(args);
    let (code, stdout, stderr) = p.check(&all);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

/// `(path, line, qualname, target)` of every `MUT113` diagnostic.
fn reports(diags: &[Value]) -> Vec<(String, u64, String, String)> {
    diags
        .iter()
        .filter(|d| d["rule_code"] == "MUT113")
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

/// `(line, message)` of the related locations of the `MUT113` diagnostic at `path:line`.
fn related(diags: &[Value], path: &str, line: u64) -> Vec<(u64, String)> {
    let diag = diags
        .iter()
        .find(|d| d["rule_code"] == "MUT113" && d["path"] == path && d["line"] == line)
        .unwrap_or_else(|| panic!("no MUT113 at {path}:{line}: {diags:#?}"));
    diag["related"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["line"].as_u64().unwrap(),
                r["message"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// A generator kept on the instance steps through a dict's values while other methods add and
/// remove entries: the next step after a change raises `RuntimeError`.
const ROTATION: &str = r#"class Rotation:
    def __init__(self) -> None:
        self.by_node: dict[str, str] = {}
        self._cursor = self._cycle()

    def _cycle(self):
        while True:
            yield from self.by_node.values()

    def next_subscriber(self) -> str:
        return next(self._cursor)

    def drop(self, node: str) -> None:
        self.by_node.pop(node, None)

    def subscribe(self, node: str, sub: str) -> None:
        self._add(node, sub)

    def _add(self, node: str, sub: str) -> None:
        self.by_node[node] = sub
"#;

const ROTATION_RUN: &str = r#"from pkg.rotation import Rotation

r = Rotation()
r.subscribe("a", "x")
r.subscribe("b", "y")
r.next_subscriber()
r.drop("b")
try:
    r.next_subscriber()
    print("no error")
except RuntimeError as e:
    print(e)
"#;

#[test]
fn mut113_reports_a_kept_generator_suspended_over_a_dict_that_other_methods_change() {
    let p = Project::new("mut113_kept");
    p.write("pkg/__init__.py", "")
        .write("pkg/rotation.py", ROTATION)
        .write("run.py", ROTATION_RUN);
    let diags = diagnostics(&p, &["--extend-select", "MUT113"]);
    assert_eq!(
        reports(&diags),
        vec![d(
            "pkg/rotation.py",
            8,
            "Rotation._cycle",
            "Rotation.by_node"
        )]
    );
    let diag = diags.iter().find(|d| d["rule_code"] == "MUT113").unwrap();
    assert_eq!(
        diag["message"],
        "`Rotation._cycle` suspends at a `yield` while iterating `self.by_node`, which `Rotation.drop` changes in place, so resuming it after that change raises `RuntimeError`"
    );
    assert!(
        diag["help"]
            .as_str()
            .unwrap()
            .starts_with("iterate over a snapshot taken before the loop"),
        "{diag:#?}"
    );
    // Each public method changing the dict, where it changes it: `subscribe` through the private
    // `_add`, which counts only through its caller. `__init__` is initialization.
    assert_eq!(
        related(&diags, "pkg/rotation.py", 8),
        vec![
            (14, "`Rotation.drop` changes `self.by_node`".to_owned()),
            (17, "`Rotation.subscribe` changes `self.by_node`".to_owned()),
        ]
    );
    assert_eq!(
        run_python(&p.dir, "run.py").trim(),
        "dictionary changed size during iteration"
    );
    // The fix: a snapshot taken before each round.
    let fixed = ROTATION.replace(
        "yield from self.by_node.values()",
        "yield from list(self.by_node.values())",
    );
    p.write("pkg/rotation.py", &fixed);
    assert_eq!(
        reports(&diagnostics(&p, &["--extend-select", "MUT113"])),
        vec![]
    );
    assert_eq!(run_python(&p.dir, "run.py").trim(), "no error");
}

/// Where a private generator's object goes decides what runs while it is suspended.
const REGISTRY: &str = r#"class Registry:
    def __init__(self) -> None:
        self.entries: dict[str, int] = {}

    def _walk(self):
        for k in self.entries:
            yield k

    def prune(self):
        for k in self._walk():
            self.remove(k)

    def remove(self, k):
        del self.entries[k]

    def _keys(self):
        for k in self.entries:
            yield k

    def names(self):
        return sorted(self._keys())

    def _values(self):
        yield from self.entries.values()

    def total(self):
        n = 0
        for v in self._values():
            n += v
        return n

    def _items(self):
        for kv in self.entries.items():
            yield kv

    def items(self):
        return self._items()

    def _inner(self):
        for k in self.entries:
            yield k

    def _outer(self):
        yield from self._inner()

    def start(self):
        self.cursor = self._outer()

    def _pairs(self):
        for kv in self.entries.items():
            yield kv

    def dump(self):
        return consume(self._pairs())


def consume(rows):
    return [r for r in rows]
"#;

const BAG: &str = r#"class Bag:
    def __init__(self) -> None:
        self.tags: set[str] = set()

    def iter_tags(self):
        for t in self.tags:
            yield t

    def drop(self, t):
        self.tags.discard(t)

    def _sorted(self):
        for t in self.tags:
            yield t


def clean(bag: Bag):
    for t in bag.iter_tags():
        if t.startswith("_"):
            bag.drop(t)
        note(t)


def note(t):
    return t


def clean_private(bag: Bag):
    for t in bag._sorted():
        bag.drop(t)


def count(bag: Bag):
    return sum(1 for _ in bag._sorted())
"#;

#[test]
fn mut113_follows_where_a_generator_object_goes() {
    let p = Project::new("mut113_exposure");
    p.write("pkg/__init__.py", "")
        .write("pkg/registry.py", REGISTRY)
        .write("pkg/bag.py", BAG);
    let diags = diagnostics(&p, &["--extend-select", "MUT113"]);
    // Reported: a loop over the generator calling a method that changes the dict (`prune`), a
    // generator iterated by another one that is kept on the instance (`_inner` in `_outer`), and
    // loops in other functions calling a method that changes the set, over a public generator
    // and over a private one. Not reported: a consuming call (`sorted`), a loop that changes
    // nothing, a generator a public function returns (its interface has no read that changes
    // the dict), one passed to a project function, and a generator expression's `sum`.
    assert_eq!(
        reports(&diags),
        vec![
            d("pkg/bag.py", 6, "Bag.iter_tags", "Bag.tags"),
            d("pkg/bag.py", 13, "Bag._sorted", "Bag.tags"),
            d("pkg/registry.py", 6, "Registry._walk", "Registry.entries"),
            d("pkg/registry.py", 40, "Registry._inner", "Registry.entries"),
        ]
    );
    assert_eq!(
        related(&diags, "pkg/registry.py", 6),
        vec![(
            11,
            "`Registry.prune` calls `Registry.remove` while iterating `Registry._walk`".to_owned()
        )]
    );
    assert_eq!(
        related(&diags, "pkg/bag.py", 6),
        vec![(
            20,
            "`clean` calls `Bag.drop` while iterating `Bag.iter_tags`".to_owned()
        )]
    );
    // Kept on the instance: every public method changing the dict counts.
    assert_eq!(
        related(&diags, "pkg/registry.py", 40),
        vec![
            (11, "`Registry.prune` changes `self.entries`".to_owned()),
            (14, "`Registry.remove` changes `self.entries`".to_owned()),
        ]
    );
}

/// A generator whose object outlives one use of it: stored, handed to code heki does not follow,
/// returned by a coroutine, or stepped (`next()`, `send()`, a loop leaving early, `any()`, a
/// wrapper stopping first) and then used again, also when the later use consumes it. A step with
/// no later use, an exhausting loop, a comparison, and `del` do not hold it.
const HELD: &str = r#"import itertools


class Table:
    def __init__(self) -> None:
        self.table: dict[str, int] = {}
        self.pending: list[object] = []

    def remove(self, k: str) -> None:
        del self.table[k]

    def note(self, k: str) -> str:
        return k

    def _stepped(self):
        yield from self.table

    def stepped_then_consumed(self):
        g = self._stepped()
        next(g)
        self.remove("b")
        return list(g)

    def _broken(self):
        yield from self.table

    def first_then_rest(self):
        it = self._broken()
        for first in it:
            break
        self.remove("b")
        rest = []
        for k in it:
            rest.append(k)
        return rest

    def _tested(self):
        yield from self.table

    def any_then_rest(self):
        g = self._tested()
        any(g)
        self.remove("b")
        return list(g)

    def _sliced(self):
        yield from self.table

    def sliced_then_rest(self):
        g = self._sliced()
        head = list(itertools.islice(g, 1))
        self.remove("b")
        return head + list(g)

    def _zipped(self):
        yield from self.table

    def zipped_then_rest(self):
        g = self._zipped()
        for _, k in zip(range(1), g):
            self.note(k)
        self.remove("b")
        return list(g)

    def _sent(self):
        yield from self.table

    def sent_then_rest(self):
        g = self._sent()
        g.send(None)
        self.remove("b")
        return list(g)

    def _handed(self):
        yield from self.table

    def stepped_then_handed(self):
        g = self._handed()
        next(g)
        self.remove("b")
        return consume(g)

    def _started(self):
        yield from self.table

    def started(self):
        g = self._started()
        next(g)
        return g

    def _queued(self):
        yield from self.table

    def queue(self):
        self.pending.append(self._queued())

    async def _later(self):
        return self._awaited()

    def _awaited(self):
        yield from self.table

    def _first(self):
        yield from self.table

    def first_only(self):
        return next(self._first())

    def _checked(self):
        yield from self.table

    def checked_then_iterated(self, flag: bool):
        g = self._checked() if flag else None
        if g is not None:
            for k in g:
                self.note(k)
        del g
        self.remove("b")

    def _left(self):
        yield from self.table

    def left_by_return(self):
        for k in self._left():
            if k == "a":
                return True
        self.remove("b")
        return False

    def _discarded(self):
        yield from self.table

    def discarded(self):
        self._discarded()
        self.remove("b")

    def _exhausted(self):
        yield from self.table

    def exhausted_then_reused(self):
        g = self._exhausted()
        for k in g:
            self.note(k)
        self.remove("b")
        return list(g)


def consume(rows):
    return [r for r in rows]
"#;

const HELD_RUN: &str = r#"import asyncio
from pkg.table import Table


def fresh():
    t = Table()
    t.table.update(a=1, b=2, c=3)
    return t


def attempt(name, fn):
    try:
        fn()
        print(name, "ok")
    except RuntimeError as e:
        print(name, e)


attempt("stepped_then_consumed", lambda: fresh().stepped_then_consumed())
attempt("first_then_rest", lambda: fresh().first_then_rest())
attempt("any_then_rest", lambda: fresh().any_then_rest())
attempt("sliced_then_rest", lambda: fresh().sliced_then_rest())
attempt("zipped_then_rest", lambda: fresh().zipped_then_rest())
attempt("sent_then_rest", lambda: fresh().sent_then_rest())
attempt("stepped_then_handed", lambda: fresh().stepped_then_handed())


def started():
    t = fresh()
    g = t.started()
    t.remove("b")
    list(g)


attempt("started", started)


def queued():
    t = fresh()
    t.queue()
    g = t.pending[0]
    next(g)
    t.remove("b")
    list(g)


attempt("queue", queued)


def later():
    t = fresh()
    g = asyncio.run(t._later())
    next(g)
    t.remove("b")
    list(g)


attempt("later", later)
attempt("first_only", lambda: (lambda t: (t.first_only(), t.remove("b")))(fresh()))
attempt("checked_then_iterated", lambda: fresh().checked_then_iterated(True))
attempt("left_by_return", lambda: fresh().left_by_return())
attempt("discarded", lambda: fresh().discarded())
attempt("exhausted_then_reused", lambda: fresh().exhausted_then_reused())
"#;

#[test]
fn mut113_reports_a_generator_that_outlives_one_use_of_it() {
    let p = Project::new("mut113_held");
    p.write("pkg/__init__.py", "")
        .write("pkg/table.py", HELD)
        .write("run.py", HELD_RUN);
    let held = |line, generator| d("pkg/table.py", line, generator, "Table.table");
    assert_eq!(
        reports(&diagnostics(&p, &["--extend-select", "MUT113"])),
        vec![
            held(16, "Table._stepped"),
            held(25, "Table._broken"),
            held(38, "Table._tested"),
            held(47, "Table._sliced"),
            held(56, "Table._zipped"),
            held(66, "Table._sent"),
            held(75, "Table._handed"),
            held(84, "Table._started"),
            held(92, "Table._queued"),
            held(101, "Table._awaited"),
        ]
    );
    let raised = "dictionary changed size during iteration";
    let expected: Vec<String> = [
        ("stepped_then_consumed", raised),
        ("first_then_rest", raised),
        ("any_then_rest", raised),
        ("sliced_then_rest", raised),
        ("zipped_then_rest", raised),
        ("sent_then_rest", raised),
        ("stepped_then_handed", raised),
        ("started", raised),
        ("queue", raised),
        ("later", raised),
        ("first_only", "ok"),
        ("checked_then_iterated", "ok"),
        ("left_by_return", "ok"),
        ("discarded", "ok"),
        ("exhausted_then_reused", "ok"),
    ]
    .iter()
    .map(|(name, outcome)| format!("{name} {outcome}"))
    .collect();
    let output = run_python(&p.dir, "run.py");
    assert_eq!(output.lines().collect::<Vec<_>>(), expected);
}

/// A loop over a generator calls a method that changes the container: on the generator's own
/// instance (the same `self`, parameter, local through a copy, attribute, or `super()`), or on
/// another object (another instance, another parameter, a new object, another attribute, a
/// property's new object at each read).
const RECEIVERS: &str = r#"class Registry:
    def __init__(self) -> None:
        self.entries: dict[str, int] = {}

    def remove(self, k: str) -> None:
        del self.entries[k]

    def _own(self):
        for k in self.entries:
            yield k

    def prune(self):
        for k in self._own():
            self.remove(k)

    def _given(self):
        for k in self.entries:
            yield k

    def move_into(self, other: "Registry"):
        for k in self._given():
            other.remove(k)

    def _param(self):
        for k in self.entries:
            yield k

    def _pair(self):
        for k in self.entries:
            yield k

    def _local(self):
        for k in self.entries:
            yield k

    def _fresh(self):
        for k in self.entries:
            yield k

    def _held(self):
        for k in self.entries:
            yield k

    def _spared(self):
        for k in self.entries:
            yield k

    def _inherited(self):
        for k in self.entries:
            yield k

    def _copied(self):
        for k in self.entries:
            yield k


class Journal(Registry):
    def remove(self, k: str) -> None:
        super().remove(k)

    def compact(self):
        for k in super()._inherited():
            self.remove(k)


def drop_all(r: Registry):
    for k in r._param():
        r.remove(k)


def copy_over(source: Registry, destination: Registry):
    for k in source._pair():
        destination.remove(k)


def rebuild():
    r = Registry()
    r.entries.update(a=1, b=2)
    alias = r
    for k in r._local():
        alias.remove(k)


def fresh_copy(source: Registry):
    destination = Registry()
    destination.entries.update(source.entries)
    for k in source._fresh():
        destination.remove(k)


class Owner:
    def __init__(self) -> None:
        self.registry = Registry()
        self.spare = Registry()

    def purge(self):
        for k in self.registry._held():
            self.registry.remove(k)

    def swap(self):
        for k in self.registry._spared():
            self.spare.remove(k)

    @property
    def snapshot(self) -> Registry:
        copy = Registry()
        copy.entries.update(self.registry.entries)
        return copy

    def trim_snapshot(self):
        for k in self.snapshot._copied():
            self.snapshot.remove(k)
"#;

const RECEIVERS_RUN: &str = r#"from pkg.registry import Journal, Owner, Registry, copy_over, drop_all, fresh_copy, rebuild


def filled(cls=Registry):
    r = cls()
    r.entries.update(a=1, b=2)
    return r


def attempt(name, fn):
    try:
        fn()
        print(name, "ok")
    except RuntimeError as e:
        print(name, e)


def owner():
    o = Owner()
    o.registry.entries.update(a=1, b=2)
    o.spare.entries.update(a=1, b=2)
    return o


attempt("prune", lambda: filled().prune())
attempt("move_into", lambda: filled().move_into(filled()))
attempt("drop_all", lambda: drop_all(filled()))
attempt("copy_over", lambda: copy_over(filled(), filled()))
attempt("rebuild", rebuild)
attempt("fresh_copy", lambda: fresh_copy(filled()))
attempt("purge", lambda: owner().purge())
attempt("swap", lambda: owner().swap())
attempt("trim_snapshot", lambda: owner().trim_snapshot())
attempt("compact", lambda: filled(Journal).compact())
"#;

#[test]
fn mut113_counts_loop_calls_on_the_generators_own_instance_only() {
    let p = Project::new("mut113_receivers");
    p.write("pkg/__init__.py", "")
        .write("pkg/registry.py", RECEIVERS)
        .write("run.py", RECEIVERS_RUN);
    let diags = diagnostics(&p, &["--extend-select", "MUT113"]);
    let own = |line, generator| d("pkg/registry.py", line, generator, "Registry.entries");
    assert_eq!(
        reports(&diags),
        vec![
            own(9, "Registry._own"),
            own(25, "Registry._param"),
            own(33, "Registry._local"),
            own(41, "Registry._held"),
            own(49, "Registry._inherited"),
        ]
    );
    assert_eq!(
        related(&diags, "pkg/registry.py", 49),
        vec![(
            63,
            "`Journal.compact` calls `Journal.remove` while iterating `Registry._inherited`"
                .to_owned()
        )]
    );
    let raised = "dictionary changed size during iteration";
    let expected: Vec<String> = [
        ("prune", raised),
        ("move_into", "ok"),
        ("drop_all", raised),
        ("copy_over", "ok"),
        ("rebuild", raised),
        ("fresh_copy", "ok"),
        ("purge", raised),
        ("swap", "ok"),
        ("trim_snapshot", "ok"),
        ("compact", raised),
    ]
    .iter()
    .map(|(name, outcome)| format!("{name} {outcome}"))
    .collect();
    let output = run_python(&p.dir, "run.py");
    assert_eq!(output.lines().collect::<Vec<_>>(), expected);
}

/// A method returning its generator or its own call on another instance (`walk_all`): the
/// generator's instance is its receiver on one path and its argument on the other, and following
/// the recursion reaches `walk_all` again.
const WALKER: &str = r#"class Walker:
    def __init__(self) -> None:
        self.entries: dict[str, int] = {}

    def _walk(self):
        for k in self.entries:
            yield k

    def remove(self, k: str) -> None:
        del self.entries[k]

    def discard(self, k: str) -> None:
        self.entries.pop(k, None)

    def forget(self, k: str) -> None:
        self.entries.pop(k)

    def walk_all(self, other: "Walker | None" = None):
        if other is None:
            return self._walk()
        return other.walk_all()


def prune(w: Walker):
    for k in w.walk_all():
        w.remove(k)


def prune_other(w: Walker, v: Walker):
    for k in w.walk_all(v):
        v.discard(k)


def copy_over(w: Walker, v: Walker):
    for k in w.walk_all():
        v.forget(k)
"#;

const WALKER_RUN: &str = r#"from pkg.walker import Walker, copy_over, prune, prune_other


def filled():
    w = Walker()
    w.entries.update(a=1, b=2)
    return w


def attempt(name, fn):
    try:
        fn()
        print(name, "ok")
    except RuntimeError as e:
        print(name, e)


attempt("prune", lambda: prune(filled()))
attempt("prune_other", lambda: prune_other(filled(), filled()))
attempt("copy_over", lambda: copy_over(filled(), filled()))
"#;

#[test]
fn mut113_follows_a_generator_through_a_method_returning_its_own_recursive_call() {
    let p = Project::new("mut113_recursive");
    p.write("pkg/__init__.py", "")
        .write("pkg/walker.py", WALKER)
        .write("run.py", WALKER_RUN);
    let diags = diagnostics(&p, &["--extend-select", "MUT113"]);
    assert_eq!(
        reports(&diags),
        vec![d("pkg/walker.py", 6, "Walker._walk", "Walker.entries")]
    );
    // The receiver's own `remove`, and `discard` on the argument whose generator `walk_all`
    // returns; not `forget` on another instance.
    assert_eq!(
        related(&diags, "pkg/walker.py", 6),
        vec![
            (
                31,
                "`prune_other` calls `Walker.discard` while iterating `Walker._walk`".to_owned()
            ),
            (
                26,
                "`prune` calls `Walker.remove` while iterating `Walker._walk`".to_owned()
            ),
        ]
    );
    let raised = "dictionary changed size during iteration";
    assert_eq!(
        run_python(&p.dir, "run.py").lines().collect::<Vec<_>>(),
        vec![
            format!("prune {raised}"),
            format!("prune_other {raised}"),
            "copy_over ok".to_owned(),
        ]
    );
}

/// The container classes whose iterators check for changes, and the changes each checks.
const CONTAINERS: &str = r#"import collections
from collections import OrderedDict, deque
from collections.abc import MutableMapping
from typing import Deque


class Containers:
    def __init__(self, table: MutableMapping[str, int]) -> None:
        self.d: dict[str, int] = {}
        self.s: set[int] = set()
        self.q: Deque[int] = deque()
        self.od: OrderedDict[str, int] = OrderedDict()
        self.c = collections.Counter()
        self.groups: collections.defaultdict[str, list[int]] = collections.defaultdict(list)
        self.items: list[int] = []
        self.table = table
        self._held = [
            self._d(), self._s(), self._q(), self._od(), self._c(), self._groups(),
            self._items(), self._table(),
        ]

    def _d(self):
        for k in self.d:
            yield k

    def _s(self):
        yield from self.s

    def _q(self):
        for x in self.q:
            yield x

    def _od(self):
        for k, v in self.od.items():
            yield k

    def _c(self):
        for k in self.c:
            yield k

    def _groups(self):
        for k in self.groups:
            yield k

    def _items(self):
        for x in self.items:
            yield x

    def _table(self):
        for k in self.table:
            yield k

    def store(self, k: str, v: int) -> None:
        self.d[k] = v

    def merge(self, other: set[int]) -> None:
        self.s |= other

    def replace_first(self, x: int) -> None:
        self.q[0] = x

    def rotate(self) -> None:
        self.q.rotate(1)

    def touch(self, k: str) -> None:
        self.od.move_to_end(k)

    def bump(self, k: str) -> None:
        self.c[k] += 1

    def group(self, k: str, v: int) -> None:
        self.groups[k].append(v)

    def peek(self, k: str) -> list[int]:
        if k in self.groups:
            return self.groups[k]
        return []

    def grow(self, x: int) -> None:
        self.items.append(x)
        self.table[str(x)] = x
"#;

const CONTAINERS_RUN: &str = r#"from collections.abc import MutableMapping
from pkg.containers import Containers

c = Containers({})
c.d["a"] = 1
c.s.add(1)
c.q.append(1)
c.q.append(2)
c.od["a"] = 1
c.od["b"] = 2
c.c["a"] = 1
c.groups["a"].append(1)
c.items.append(1)
for gen, change in [
    (c._d(), lambda: c.store("b", 2)),
    (c._s(), lambda: c.merge({2})),
    (c._q(), lambda: c.rotate()),
    (c._q(), lambda: c.replace_first(5)),
    (c._od(), lambda: c.touch("a")),
    (c._c(), lambda: c.bump("b")),
    (c._groups(), lambda: c.group("b", 1)),
    (c._items(), lambda: c.grow(2)),
]:
    next(gen)
    change()
    try:
        list(gen)
        print("ok")
    except RuntimeError as e:
        print(e)
"#;

#[test]
fn mut113_counts_the_changes_each_container_class_checks() {
    let p = Project::new("mut113_containers");
    p.write("pkg/__init__.py", "")
        .write("pkg/containers.py", CONTAINERS)
        .write("run.py", CONTAINERS_RUN);
    let diags = diagnostics(&p, &["--extend-select", "MUT113"]);
    // A dict item store (a new key), a set's `|=` stored back, a deque's `rotate` (not its item
    // store), `OrderedDict.move_to_end`, a `Counter` item update, and reading a missing key of
    // an annotated `defaultdict`. Not a list, or an abstract `MutableMapping`.
    assert_eq!(
        reports(&diags),
        vec![
            d("pkg/containers.py", 23, "Containers._d", "Containers.d"),
            d("pkg/containers.py", 27, "Containers._s", "Containers.s"),
            d("pkg/containers.py", 30, "Containers._q", "Containers.q"),
            d("pkg/containers.py", 34, "Containers._od", "Containers.od"),
            d("pkg/containers.py", 38, "Containers._c", "Containers.c"),
            d(
                "pkg/containers.py",
                42,
                "Containers._groups",
                "Containers.groups"
            ),
        ]
    );
    assert_eq!(
        related(&diags, "pkg/containers.py", 30),
        vec![(63, "`Containers.rotate` changes `self.q`".to_owned())]
    );
    // Reading a key the method tests for first inserts nothing (`peek`).
    assert_eq!(
        related(&diags, "pkg/containers.py", 42),
        vec![(72, "`Containers.group` changes `self.groups`".to_owned())]
    );
    assert_eq!(
        run_python(&p.dir, "run.py").trim(),
        [
            "dictionary changed size during iteration",
            "Set changed size during iteration",
            "deque mutated during iteration",
            "ok",
            "OrderedDict mutated during iteration",
            "dictionary changed size during iteration",
            "dictionary changed size during iteration",
            "ok",
        ]
        .join("\n")
    );
}

/// Views, lazy wrappers, local copies, and snapshots of a container, a `yield` the loop does not
/// go on from, and an async generator.
const SHAPES: &str = r#"import itertools


class Shapes:
    def __init__(self) -> None:
        self.d: dict[str, int] = {}
        self._held = [
            self._keys(), self._items(), self._enumerated(), self._zipped(), self._reversed(),
            self._chained(), self._genexpr(), self._copied(), self._snapshot(), self._sorted(),
            self._copy_method(), self._first(), self._alias(), self._agen(),
        ]

    def _keys(self):
        for k in self.d.keys():
            yield k

    def _items(self):
        for k, v in self.d.items():
            yield k

    def _enumerated(self):
        for i, k in enumerate(self.d):
            yield i

    def _zipped(self):
        for a, b in zip(self.d, range(3)):
            yield a

    def _reversed(self):
        for k in reversed(self.d):
            yield k

    def _chained(self):
        yield from itertools.chain(self.d, [])

    def _genexpr(self):
        yield from (k.upper() for k in self.d)

    def _alias(self):
        d = self.d
        for k in d:
            yield k

    async def _agen(self):
        for k in self.d:
            yield k

    def _copied(self):
        yield from list(self.d)

    def _snapshot(self):
        for k in tuple(self.d.items()):
            yield k

    def _sorted(self):
        for k in sorted(self.d):
            yield k

    def _copy_method(self):
        for k in self.d.copy():
            yield k

    def _first(self):
        for k in self.d:
            yield k
            return

    def clear(self) -> None:
        self.d.clear()
"#;

#[test]
fn mut113_follows_views_wrappers_and_copies_but_not_snapshots() {
    let p = Project::new("mut113_shapes");
    p.write("pkg/__init__.py", "")
        .write("pkg/shapes.py", SHAPES);
    let lines: Vec<u64> = reports(&diagnostics(&p, &["--extend-select", "MUT113"]))
        .into_iter()
        .map(|r| r.1)
        .collect();
    assert_eq!(lines, vec![14, 18, 22, 26, 30, 34, 37, 41, 45]);
}

/// A public generator is an iteration the class offers, like `dict.items()`: its public methods
/// changing the container are the caller's to avoid while iterating. Changes made by reads
/// count.
const OFFERED: &str = r#"class Table:
    def __init__(self) -> None:
        self._data: dict[str, int] = {}

    def __iter__(self):
        yield from self._data

    def items(self):
        for kv in self._data.items():
            yield kv

    def __setitem__(self, k: str, v: int) -> None:
        self._data[k] = v

    def add(self, k: str) -> None:
        self._data[k] = 1

    def __len__(self) -> int:
        self._data.setdefault("", 0)
        return len(self._data)

    @property
    def size(self) -> int:
        self._data.pop("", None)
        return len(self._data)


class Plain:
    def __init__(self) -> None:
        self._data: dict[str, int] = {}

    def __iter__(self):
        yield from self._data

    def __setitem__(self, k: str, v: int) -> None:
        self._data[k] = v

    def add(self, k: str) -> None:
        self._data[k] = 1
"#;

#[test]
fn mut113_counts_only_reads_for_a_public_generator() {
    let p = Project::new("mut113_offered");
    p.write("pkg/__init__.py", "")
        .write("pkg/table.py", OFFERED);
    let diags = diagnostics(&p, &["--extend-select", "MUT113"]);
    assert_eq!(
        reports(&diags),
        vec![
            d("pkg/table.py", 6, "Table.__iter__", "Table._data"),
            d("pkg/table.py", 9, "Table.items", "Table._data"),
        ]
    );
    assert_eq!(
        related(&diags, "pkg/table.py", 6),
        vec![
            (19, "`Table.__len__` changes `self._data`".to_owned()),
            (24, "`Table.size` changes `self._data`".to_owned()),
        ]
    );
}

/// Initialization runs before any iteration; a private method counts through the methods
/// calling it.
const PHASES: &str = r#"class Phases:
    def __init__(self) -> None:
        self.d: dict[str, int] = {}
        self._fill()
        self._cursor = self._cycle()

    def _cycle(self):
        while True:
            yield from self.d

    def _fill(self) -> None:
        self.d["a"] = 1

    def setup(self) -> None:
        self.d["b"] = 2

    def _init_defaults(self) -> None:
        self.d["c"] = 3

    def __setstate__(self, state) -> None:
        self.d.update(state)

    def _unused(self) -> None:
        self.d.clear()

    def reset(self) -> None:
        self.d = {}
"#;

#[test]
fn mut113_leaves_out_initialization_private_methods_and_rebinding() {
    let p = Project::new("mut113_phases");
    p.write("pkg/__init__.py", "")
        .write("pkg/phases.py", PHASES);
    assert_eq!(
        reports(&diagnostics(&p, &["--extend-select", "MUT113"])),
        vec![]
    );
    // A public method changing it in place is reported.
    let changing = PHASES.replace("self.d = {}", "self.d.clear()");
    p.write("pkg/phases.py", &changing);
    assert_eq!(
        reports(&diagnostics(&p, &["--extend-select", "MUT113"])),
        vec![d("pkg/phases.py", 9, "Phases._cycle", "Phases.d")]
    );
}

/// A loop over a container whose body calls a function that changes it.
const POOL: &str = r#"class Pool:
    def __init__(self) -> None:
        self.conns: set[str] = set()
        self.waiters: dict[str, int] = {}

    def _evict(self, c: str) -> None:
        self.conns.discard(c)

    def close_all(self) -> None:
        for c in self.conns:
            self._evict(c)

    def close_first(self) -> None:
        for c in self.conns:
            self._evict(c)
            break

    def close_snapshot(self) -> None:
        for c in list(self.conns):
            self._evict(c)

    def close_directly(self) -> None:
        for c in self.conns:
            self.conns.discard(c)

    def handed(self) -> None:
        for c in self.conns:
            forget(self.conns, c)

    def logged(self) -> None:
        for c in self.conns:
            self.log(c)

    def log(self, c: str) -> None:
        print(c, len(self.conns))

    def notify(self) -> None:
        for k in self.waiters:
            try:
                self.wake(k)
                break
            except KeyError:
                continue

    def wake(self, k: str) -> None:
        self.waiters.pop(k)
        raise KeyError(k)


def forget(conns: set[str], c: str) -> None:
    conns.discard(c)
"#;

#[test]
fn mut113_reports_calls_in_a_loop_body_that_change_the_iterated_container() {
    let p = Project::new("mut113_calls");
    p.write("pkg/__init__.py", "").write("pkg/pool.py", POOL);
    let diags = diagnostics(&p, &["--extend-select", "MUT113"]);
    // A method on `self`, a helper handed the set, and a call after which only its exception
    // goes on with the loop. Not: leaving the loop after the call, a snapshot, a change written
    // in the body itself (the linters' check), and a call that only reads.
    assert_eq!(
        reports(&diags),
        vec![
            d("pkg/pool.py", 11, "Pool.close_all", "Pool.conns"),
            d("pkg/pool.py", 28, "Pool.handed", "Pool.conns"),
            d("pkg/pool.py", 40, "Pool.notify", "Pool.waiters"),
        ]
    );
    let diag = diags
        .iter()
        .find(|d| d["rule_code"] == "MUT113" && d["line"] == 11)
        .unwrap();
    assert_eq!(
        diag["message"],
        "`Pool.close_all` calls `Pool._evict`, which changes `self.conns` in place while `Pool.close_all` iterates it, so the loop's next step raises `RuntimeError`"
    );
    assert_eq!(
        related(&diags, "pkg/pool.py", 11),
        vec![(10, "`Pool.close_all` iterates `self.conns` here".to_owned())]
    );
}

/// A class family across modules: the generator in a base class, the change in a subclass of
/// another module; and a method a subclass inherits from a base in a third module.
const BASE: &str = r#"class Base:
    def __init__(self) -> None:
        self.jobs: dict[str, int] = {}
        self._cursor = self._cycle()

    def _cycle(self):
        while True:
            yield from self.jobs
"#;

const CHILD: &str = r#"from pkg.base import Base


class Child(Base):
    def finish(self, job: str) -> None:
        del self.jobs[job]
"#;

const MIXIN: &str = r#"class Cleanup:
    def __init__(self) -> None:
        self.jobs: dict[str, int] = {}

    def drain(self) -> None:
        self.jobs.clear()
"#;

const WORKER: &str = r#"from pkg.mixin import Cleanup


class Worker(Cleanup):
    def __init__(self) -> None:
        super().__init__()
        self._cursor = self._cycle()

    def _cycle(self):
        while True:
            yield from self.jobs
"#;

#[test]
fn mut113_follows_class_families_across_modules() {
    let p = Project::new("mut113_family");
    p.write("pkg/__init__.py", "")
        .write("pkg/base.py", BASE)
        .write("pkg/child.py", CHILD)
        .write("pkg/mixin.py", MIXIN)
        .write("pkg/worker.py", WORKER);
    let diags = diagnostics(&p, &["--extend-select", "MUT113"]);
    assert_eq!(
        reports(&diags),
        vec![
            d("pkg/base.py", 8, "Base._cycle", "Base.jobs"),
            d("pkg/worker.py", 11, "Worker._cycle", "Worker.jobs"),
        ]
    );
    let child = related(&diags, "pkg/base.py", 8);
    assert_eq!(
        child,
        vec![(6, "`Child.finish` changes `self.jobs`".to_owned())]
    );
    let related_path = diags
        .iter()
        .find(|d| d["rule_code"] == "MUT113" && d["line"] == 8)
        .unwrap()["related"][0]["path"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(related_path.ends_with("pkg/child.py"), "{related_path}");
}

/// Optional and union annotations, a merged value, `iter()` / `map()` / `filter()`, a class
/// object in an attribute, a method's two definitions, and an item stored through
/// `__setitem__`.
const TYPED: &str = r#"from collections import OrderedDict
from typing import Counter, Optional


class Typed:
    def __init__(self, flag: bool) -> None:
        self.opt: Optional[dict[str, int]] = None
        self.pipe: dict[str, int] | None = None
        self.mixed: dict[str, int] | set[str] = set()
        self.counts: Counter[str] = Counter()
        self.a: dict[str, int] = {}
        self.b: set[str] = set()
        self.kind = OrderedDict
        self.flag = flag
        self._held = [
            self._opt(), self._pipe(), self._mixed(), self._counts(), self._either(),
            self._iter(), self._mapped(), self._filtered(), self._kind(), self._twice(),
        ]

    def _opt(self):
        for k in self.opt:
            yield k

    def _pipe(self):
        for k in self.pipe:
            yield k

    def _mixed(self):
        for k in self.mixed:
            yield k

    def _counts(self):
        for k in self.counts:
            yield k

    def _either(self):
        for k in (self.a if self.flag else self.b):
            yield k

    def _iter(self):
        it = iter(self.a)
        for k in it:
            yield k

    def _mapped(self):
        for k in map(str.upper, self.a):
            yield k

    def _filtered(self):
        for k in filter(None, self.b):
            yield k

    def _kind(self):
        for k in self.kind():
            yield k

    if True:
        def _twice(self):
            yield from self.a

        def change(self, k: str) -> None:
            self.opt.__setitem__(k, 1)
    else:
        def _twice(self):
            yield from self.b

        def change(self, k: str) -> None:
            self.pipe.pop(k)
            self.mixed.clear()
            self.counts[k] += 1
            self.a.clear()
            self.b.clear()
"#;

#[test]
fn mut113_reads_container_classes_through_unions_merges_and_wrappers() {
    let p = Project::new("mut113_typed");
    p.write("pkg/__init__.py", "").write("pkg/typed.py", TYPED);
    let diags = diagnostics(&p, &["--extend-select", "MUT113"]);
    // Not `mixed` (a dict or a set) or the class object in `kind`; `_either` iterates either of
    // two containers; each definition of `_twice` and `change` counts.
    let found: Vec<(u64, String)> = reports(&diags).into_iter().map(|r| (r.1, r.3)).collect();
    assert_eq!(
        found,
        vec![
            (21, "Typed.opt".to_owned()),
            (25, "Typed.pipe".to_owned()),
            (33, "Typed.counts".to_owned()),
            (37, "Typed.a".to_owned()),
            (37, "Typed.b".to_owned()),
            (42, "Typed.a".to_owned()),
            (46, "Typed.a".to_owned()),
            (50, "Typed.b".to_owned()),
            (59, "Typed.a".to_owned()),
            (65, "Typed.b".to_owned()),
        ]
    );
    // Where the definition of `change` changing it does.
    assert_eq!(
        related(&diags, "pkg/typed.py", 21),
        vec![(62, "`Typed.change` changes `self.opt`".to_owned())]
    );
}

#[test]
fn mut113_is_opt_in_suppressible_and_documented() {
    let p = Project::new("mut113_selection");
    p.write("pkg/__init__.py", "")
        .write("pkg/rotation.py", ROTATION);
    assert_eq!(reports(&diagnostics(&p, &[])), vec![]);
    assert_eq!(reports(&diagnostics(&p, &["--select", "MUT"])), vec![]);
    assert_eq!(
        reports(&diagnostics(
            &p,
            &["--extend-select", "iterated-container-mutation"]
        ))
        .len(),
        1
    );
    let suppressed = ROTATION.replace(
        "yield from self.by_node.values()",
        "yield from self.by_node.values()  # heki: ignore[MUT113]",
    );
    p.write("pkg/rotation.py", &suppressed);
    assert_eq!(
        reports(&diagnostics(&p, &["--extend-select", "MUT113"])),
        vec![]
    );
    let (code, stdout, _) = p.heki(&["rule", "MUT113"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("iterated-container-mutation"), "{stdout}");
    assert!(stdout.contains("## What it does"), "{stdout}");
}
