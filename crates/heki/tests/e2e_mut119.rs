//! `MUT119` (`change-lost-on-new-object`) end to end: an in-place change of a builtin container
//! applied to a new object that nothing else holds and no name keeps, reported at the change, and
//! the code whose change some other code sees.

mod common;

use common::Project;
use serde_json::Value;

fn diagnostics(p: &Project) -> Vec<Value> {
    let (code, stdout, stderr) = p.check(&["--output-format", "json", "--select", "MUT119"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    serde_json::from_str(&stdout).unwrap()
}

/// `(file, line)` of every `MUT119` report.
fn reported(diags: &[Value]) -> Vec<(String, u64)> {
    diags
        .iter()
        .filter(|d| d["rule_code"] == "MUT119")
        .map(|d| {
            let path = d["path"].as_str().unwrap().replace('\\', "/");
            let file = path.rsplit('/').next().unwrap().to_owned();
            (file, d["line"].as_u64().unwrap())
        })
        .collect()
}

fn at(file: &str, lines: &[u64]) -> Vec<(String, u64)> {
    lines.iter().map(|l| (file.to_owned(), *l)).collect()
}

/// Shapes found on a corpus probe, reduced: an environment override bound to a name, an option
/// stored as the `None` that `extend` returns, a sort of a copy, and item stores through a
/// property whose getter returns a new dict.
const HARMS: &str = r#"import os


def home_override():
    environ = os.environ.copy().update(HOME="/tmp")
    return environ


def merge(options, key, current_val, new_val):
    options[key] = list(current_val).extend(new_val)


class Beam:
    def __init__(self, points):
        self._points = points

    def plot(self):
        list(self._points).sort()
        return self._points


class DockerTask:
    @property
    def environment(self):
        return {}

    def run(self):
        self.environment["PYTHONPATH"] = "/app"
"#;

#[test]
fn changes_of_new_objects_found_on_a_corpus() {
    let p = Project::new("mut119_harms");
    p.write("pkg/__init__.py", "").write("pkg/harms.py", HARMS);
    let diags = diagnostics(&p);
    assert_eq!(reported(&diags), at("harms.py", &[5, 10, 18, 28]));
    assert_eq!(
        diags[0]["message"],
        "`update` changes a new copy (a `dict`), which nothing else holds and no name keeps: the change is lost when the statement ends, and the assignment stores the `None` that `update` returns"
    );
    assert_eq!(
        diags[2]["message"],
        "`sort` changes a new `list`, which nothing else holds and no name keeps: the change is lost when the statement ends"
    );
    assert_eq!(
        diags[3]["message"],
        "the item assignment changes the new `dict` that `DockerTask.environment` returns, which nothing else holds and no name keeps: the change is lost when the statement ends"
    );
    assert_eq!(diags[3]["target"], "dict.__setitem__");
    assert_eq!(
        diags[3]["help"],
        "bind the new `dict` to a name and change it there, or build the changed value in one expression (`sorted(...)`, `{**a, **b}`, `[*a, *b]`)"
    );
    // An experimental opt-in rule: heuristic until it is certified, and off unless named.
    assert!(diags.iter().all(|d| d["certainty"] == "heuristic"));
    let (_, stdout, _) = p.heki(&["check", "--output-format", "json", "--heuristic"]);
    assert!(!stdout.contains("MUT119"), "{stdout}");
}

/// Every builtin container kind, made by its constructor, a display, a copy (a method, or a
/// function of `copy` called through its module or by its own name), or a slice; a value a
/// changing method returns that the statement drops; item stores, augmented item stores, and item
/// deletions; and a `None` result stored in an attribute, a global, or a closure cell.
const KINDS: &str = r#"import copy
from collections import deque
from copy import deepcopy


def shapes(xs: list[int], d: dict[str, int], s: set[int]):
    sorted(xs).reverse()
    xs[:].append(1)
    d.copy().update(a=1)
    copy.copy(d).clear()
    deepcopy(d).clear()
    set(s).add(1)
    {1, 2}.discard(1)
    bytearray(b"ab").extend(b"c")
    deque(xs).appendleft(0)
    {"a": 1}.update(b=2)
    {}.setdefault("a", 1)
    list(xs).pop()
    dict(d)["a"] = 1
    dict(d)["a"] += 1
    del dict(d)["a"]


class Holder:
    def keep(self, xs):
        self.items = list(xs).extend(xs)


def outer(xs):
    found = None

    def inner():
        nonlocal found
        found = sorted(xs).reverse()

    inner()
    return found


X = list(range(3)).sort()
"#;

#[test]
fn every_container_kind_and_every_way_a_new_one_is_made() {
    let p = Project::new("mut119_kinds");
    p.write("pkg/__init__.py", "").write("pkg/kinds.py", KINDS);
    let diags = diagnostics(&p);
    assert_eq!(
        reported(&diags),
        at(
            "kinds.py",
            &[
                7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 26, 34, 40
            ]
        )
    );
    let message = |line: u64| {
        diags
            .iter()
            .find(|d| d["line"] == line)
            .map(|d| d["message"].as_str().unwrap().to_owned())
            .unwrap()
    };
    assert!(message(11).starts_with("`clear` changes a new copy (a `dict`),"));
    assert!(message(14).starts_with("`extend` changes a new `bytearray`,"));
    assert!(message(15).starts_with("`appendleft` changes a new `deque`,"));
    assert!(message(21).starts_with("the item deletion changes a new `dict`,"));
    assert!(message(26).ends_with("and the assignment stores the `None` that `extend` returns"));
}

/// Changes some other code sees, or results the code reads: a method of a project class named
/// like a container's, a named copy, the appends and item stores that build a comprehension
/// whose result is dropped or indexed, a result used, returned, or passed on, a change no
/// container of the kind has, a deep copy of a project instance, a copy of a value of unknown
/// type, a dict a getter also keeps, a getter returning its field, a cached property, and a plain
/// attribute.
const KEPT: &str = r#"import copy
from functools import cached_property


class Inventory:
    def __init__(self, nets):
        self.nets = nets

    def remove(self, name):
        return Inventory([n for n in self.nets if n != name])


def drop(inv: Inventory):
    copy.deepcopy(inv).remove("x")


def named(xs):
    items = list(xs)
    items.sort()
    return items


def used(xs, x):
    [print(x) for x in xs]
    {x: x for x in xs}
    found = {x: len(x) for x in xs}[x]
    {x for x in xs}
    taken = list(xs).pop()
    print(list(xs).sort())
    set(xs).append(1)
    copy.copy(x).append(1)
    return list(xs).sort() or taken


class Cache:
    def __init__(self):
        self._last = None
        self._env = {}
        self.plain = {}

    @property
    def settings(self):
        d = {}
        self._last = d
        return d

    @property
    def env(self):
        return self._env

    @cached_property
    def memo(self):
        return {}

    def set(self):
        self.settings["a"] = 1
        self.env["a"] = 1
        self.memo["a"] = 1
        self.plain["a"] = 1
"#;

#[test]
fn changes_other_code_sees_and_results_the_code_reads() {
    let p = Project::new("mut119_kept");
    p.write("pkg/__init__.py", "").write("pkg/kept.py", KEPT);
    assert_eq!(reported(&diagnostics(&p)), Vec::new());
}

/// Project functions whose every return is a new container nothing else holds (directly, through
/// another such function, through copies, branches, and the uses that keep nothing, among them
/// methods returning a count or an item and views only a loop reads, through a copy or a slice of
/// a typed value, and two definitions of one function), and those for which it is not so: two
/// kinds, recursion, a path returning `None`, a dict stored, handed to a call that may keep it,
/// or captured by a closure, a cached function, a tuple, a slice of a value of unknown type, two
/// definitions of different kinds, a new instance of a project subclass of `dict`, and a view,
/// an iterator, a bound method, or a `getattr` default kept in an attribute.
const MAKERS: &str = r#"import sys
from functools import lru_cache


def empty():
    return []


def through():
    return empty()


def table(keys):
    out = {}
    for k in keys:
        out[k] = len(out)
    if out == {} or not out:
        print(f"{out!r}", out["a"])
    for k in out:
        del out[k]
    out.update(a=1)
    return out


def either(flag, pairs):
    if flag:
        result = {}
    else:
        result = dict(pairs)
    return result


def grown(xs):
    items = list(xs)
    alias = items
    alias += [1]
    del alias
    if items:
        return items
    return sorted(xs)


def copies(d: dict):
    return d.copy()


def head(xs: list[int]):
    return xs[:2]


def tail(xs):
    return xs[1:]


def mixed(flag):
    return [] if flag else {}


def countdown(n):
    return countdown(n - 1) if n else []


def maybe(flag):
    if flag:
        return []


def stored(owner):
    d = {}
    owner.last = d
    return d


def handed(sink):
    d = {}
    sink.append(d)
    return d


def passed(xs):
    items = list(xs)
    keep(items)
    return items


def keep(value):
    return value


def captured():
    d = {}
    return d if callable(lambda: d) else {}


@lru_cache(maxsize=None)
def cached(key):
    return [key]


def pair():
    return (1, 2)


if sys.version_info >= (3, 9):
    def twice():
        return []
else:
    def twice():
        return list()

if sys.version_info >= (3, 9):
    def unlike():
        return []
else:
    def unlike():
        return {}


class Settings(dict):
    def __init__(self):
        super().__init__()


def settings():
    return Settings()


def viewed(owner):
    d = {}
    owner.view = d.values()
    return d


def walked(owner, xs):
    items = list(xs)
    rest = items.__iter__()
    owner.rest = rest
    return items


def iterated(owner, xs):
    items = list(xs)
    owner.rest = iter(items)
    return items


def defaulted(owner):
    d = {}
    owner.config = getattr(owner, "config", d)
    return d


def bound(owner):
    d = {}
    owner.add = d.setdefault
    return d


def counted(xs, key):
    items = list(xs)
    if len(items) > 3 and items.count(key) and key in items:
        print(items.index(key), items.copy())
    return items


def looped(pairs):
    d = dict(pairs)
    for k, v in d.items():
        print(k, len(d.keys()), v in d.values(), d.get(k))
    return d
"#;

const MAKERS_USE: &str = r#"from . import makers
from .makers import empty


def run(xs, pairs, owner, sink, d):
    makers.empty().append(1)
    makers.through().append(1)
    makers.table(xs).clear()
    makers.either(True, pairs).update(b=2)
    makers.grown(xs).sort()
    makers.copies(d).clear()
    makers.head(xs).append(1)
    makers.twice().append(1)
    makers.mixed(True).clear()
    makers.countdown(3).append(1)
    makers.maybe(True).append(1)
    makers.stored(owner).clear()
    makers.handed(sink).clear()
    makers.passed(xs).sort()
    makers.captured().clear()
    makers.cached(1).append(2)
    makers.pair().append(3)
    makers.unlike().clear()
    makers.settings().update(a=1)
    makers.tail(xs).append(1)
    empty().append(4)
    makers.viewed(owner).update(k=1)
    makers.walked(owner, xs).append(1)
    makers.iterated(owner, xs).append(1)
    makers.defaulted(owner).clear()
    makers.bound(owner).clear()
    makers.counted(xs, 1).sort()
    makers.looped(pairs).clear()
"#;

#[test]
fn functions_whose_every_return_is_a_new_container_nothing_else_holds() {
    let p = Project::new("mut119_makers");
    p.write("pkg/__init__.py", "")
        .write("pkg/makers.py", MAKERS)
        .write("pkg/use.py", MAKERS_USE);
    let diags = diagnostics(&p);
    assert_eq!(
        reported(&diags),
        at("use.py", &[6, 7, 8, 9, 10, 11, 12, 13, 26, 32, 33])
    );
    assert_eq!(
        diags[1]["message"],
        "`append` changes the new `list` that `through` returns, which nothing else holds and no name keeps: the change is lost when the statement ends"
    );
}
