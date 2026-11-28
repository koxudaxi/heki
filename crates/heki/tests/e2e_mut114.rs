//! `MUT114` (`cached-result-mutation`) end to end: its exact claim (the function changes in place
//! the list, dict, or set that a `functools` cache keeps, the same object on every path), one
//! counterexample per premise of that claim, and the reports its other changes give.

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

/// `(line, narrowed)` of every `MUT114` report: whether its message states the exact claim.
fn claims(diags: &[Value]) -> Vec<(u64, bool)> {
    diags
        .iter()
        .filter(|d| d["rule_code"] == "MUT114")
        .map(|d| {
            let message = d["message"].as_str().unwrap();
            (
                d["line"].as_u64().unwrap(),
                message.contains(" changes in place the "),
            )
        })
        .collect()
}

const PRODUCERS: &str = r#"import functools
from functools import cache, lru_cache
from typing import Optional

from caching import cache as project_cache


@lru_cache(maxsize=None)
def entry_points(group, sub=None):
    if sub:
        return {k: v for k, v in [(group, sub)]}
    return {"a": 1}


@cache
def env_for(req: str) -> dict[str, str]:
    return dict(PATH=req)


@lru_cache(maxsize=0)
def nothing_kept(a) -> list:
    return [a]


@lru_cache
def no_key() -> list:
    return [1]


@lru_cache
def maybe(a) -> Optional[dict]:
    return None if a else {}


@lru_cache
def sometimes_none(a):
    if a:
        return [a]
    return None


@lru_cache
def nested(a) -> dict:
    return {"rows": [a]}


def wrapper(a):
    if a:
        return entry_points(a)
    return {}


def own_cache(f):
    return f


@own_cache
def not_functools(a) -> dict:
    return {}


@project_cache
def project_cached(a) -> dict:
    return {}


class Formatter:
    @functools.lru_cache(maxsize=100)
    def parts(self, value: str):
        return value.split("\n")

    @lru_cache
    def per_instance(self) -> list:
        return []

    def format(self, value: str):
        translate = self.parts
        lines = translate(value)
        lines[0] = "x"
        self.per_instance().append(1)


def grow(xs):
    xs.append(1)
"#;

const USES: &str = r#"from producers import (
    entry_points,
    env_for,
    grow,
    maybe,
    nested,
    no_key,
    not_functools,
    nothing_kept,
    project_cached,
    sometimes_none,
    wrapper,
)


def use(c, g):
    eps = entry_points(g)
    eps.pop("cosine")
    env = env_for(g)
    env.update(PATH="x")
    both = entry_points(g) if c else entry_points(c)
    both["z"] = 1
    loop = entry_points(g)
    for i in range(3):
        loop["i"] = i
        loop = loop
    nothing_kept(g).append(1)
    no_key().append(2)
    maybe(g)["k"] = 1
    sometimes_none(g).append(1)
    either = entry_points(g) if c else {}
    either["z"] = 1
    w = wrapper(g)
    w["k"] = 2
    nested(g)["rows"].append(1)
    wrong = entry_points(g)
    wrong.append(1)
    grow(entry_points(g))
    not_functools(g)["k"] = 1
    project_cached(g)["k"] = 1
"#;

const CACHING: &str = r#"def cache(f):
    return f
"#;

#[test]
fn mut114_states_its_claim_only_where_every_premise_holds() {
    let p = Project::new("mut114_premises");
    p.write("producers.py", PRODUCERS)
        .write("uses.py", USES)
        .write("caching.py", CACHING);
    // `MUT202` keeps the declared producers: a cached result is no report of its own.
    assert!(diagnostics(&p, &["--select", "MUT202"]).is_empty());
    let diags = diagnostics(&p, &["--select", "MUT114"]);
    let uses: Vec<Value> = diags
        .iter()
        .filter(|d| d["path"] == "uses.py")
        .cloned()
        .collect();
    assert_eq!(
        claims(&uses),
        vec![
            // The exact claim: a dict built on every return, a return annotation, two calls of
            // the same cached function joined, and the value a loop passes around.
            (18, true),
            (20, true),
            (22, true),
            (25, true),
            // Cached: `maxsize=0` keeps nothing.
            (27, false),
            // Keyed: no parameter.
            (28, false),
            // Container kind: `Optional[dict]`, and `None` on one return.
            (29, false),
            (30, false),
            // Same object: another value on one path, a wrapper returning the cached value on
            // one path only, an object below the result.
            (32, false),
            (34, false),
            (35, false),
            // Changed in place: `append` is no method of a dict.
            (37, false),
            // Changed by a helper.
            (38, false),
            // Cached: a project decorator named `cache`, not `functools.cache`.
            (40, false),
        ],
        "{uses:#?}"
    );
    // A decorator the module defines is no cache: nothing at line 39.
    let at = |line: u64| uses.iter().find(|d| d["line"] == line).unwrap();
    assert_eq!(
        at(18)["message"],
        "`use` changes in place the dict that `entry_points` returned; `entry_points` is cached by `functools.lru_cache`"
    );
    assert_eq!(at(18)["help"], "change a copy (`dict(...)`) instead");
    assert_eq!(
        at(34)["message"],
        "`use` mutates the shared object returned by cached function `producers.entry_points` through `producers.wrapper`"
    );
    // A cached method called through a bound-method alias, whose targets are not established
    // to be every implementation the call can run (`TypeCtx::inst_targets_complete` judges no
    // held method), and a method keyed by `self` alone.
    let formatter: Vec<Value> = diags
        .iter()
        .filter(|d| d["path"] == "producers.py")
        .cloned()
        .collect();
    assert_eq!(
        claims(&formatter),
        vec![(79, false), (80, false)],
        "{formatter:#?}"
    );
}

const KINDS: &str = r#"from functools import lru_cache
from typing import List

FLAG = True


@lru_cache
def tags(a) -> set[str]:
    return {a}


@lru_cache
def names(a) -> List[str]:
    return [a]


@lru_cache
def copied(a):
    x = [a]
    return x


@lru_cache
def joined(a, c):
    x = [a] if c else [c]
    return x


@lru_cache
def built(a):
    return sorted(a)


@lru_cache
def made_set(a):
    return set(a)


@lru_cache
def made_dict(a):
    return dict(a=a)


@lru_cache
def made_list(a):
    return list(a)


@lru_cache
def config(a) -> dict[str, int]:
    return {}


@lru_cache
def frozen(a):
    return frozenset(a)


@lru_cache
def pair(a):
    return (a,)


@lru_cache
def mixed_kinds(a):
    if a:
        return [a]
    return {a}


@lru_cache
def falls_off(a):
    if a:
        return [a]


@lru_cache
def raises(a):
    raise ValueError(a)


@lru_cache
def maybe_dict(a) -> dict | None:
    return {}


@lru_cache
def counted(a) -> int:
    return 1


@lru_cache
def either_kind(a, c) -> list[int] | dict[str, int]:
    return [a] if c else {"a": a}


@lru_cache
async def fetched(a) -> list:
    return [a]


@lru_cache
def generated(a):
    yield [a]


if FLAG:

    @lru_cache
    def twice(a) -> list:
        return [a]

else:

    @lru_cache
    def twice(a) -> set:
        return {a}


class Registry:
    @staticmethod
    @lru_cache
    def entries(kind) -> dict:
        return {}


def use(g, c):
    t = tags(g)
    t.add("x")
    t2 = tags(g)
    t2 |= {"y"}
    n = names(g)
    n.append("z")
    n2 = names(g)
    n2 += ["w"]
    copied(g).append(1)
    joined(g, c).append(1)
    built(g).append(1)
    made_set(g).add(1)
    made_dict(g)["k"] = 1
    made_list(g).append(1)
    frozen(g).add(1)
    pair(g)[0] = 1
    mixed_kinds(g).append(1)
    falls_off(g).append(1)
    raises(g).append(1)
    maybe_dict(g)["k"] = 1
    counted(g).append(1)
    either_kind(g, c).clear()
    d = config(g)
    d |= {"a": 2}
    twice(g).append(1)
    Registry.entries(g)["k"] = 1
    for x in generated(g):
        x.append(1)
    same([g]).append(1)
    unknown_size(g).append(1)


@lru_cache
def same(a):
    return a


ZERO = 0


@lru_cache(maxsize=ZERO)
def unknown_size(a) -> list:
    return [a]
"#;

#[test]
fn mut114_knows_each_container_kind_by_annotation_or_construction() {
    let p = Project::new("mut114_kinds");
    p.write("kinds.py", KINDS);
    let diags = diagnostics(&p, &["--select", "MUT114"]);
    assert_eq!(
        claims(&diags),
        vec![
            // A set by annotation, its method and `|=`; a list by `typing.List`, its method and
            // `+=`; lists, sets, and dicts built by assignment, phi, `sorted`, `set`, `dict`, and
            // `list`.
            (129, true),
            (131, true),
            (133, true),
            (135, true),
            (136, true),
            (137, true),
            (138, true),
            (139, true),
            (140, true),
            (141, true),
            // A `frozenset`, a tuple, a list on one return and a set on another, a fall through
            // the end (`None`), and no return at all.
            (142, false),
            (143, false),
            (144, false),
            (145, false),
            (146, false),
            // `dict | None` establishes nothing, but every return builds a dict.
            (147, true),
            // An annotation that is no container, and a return built as none.
            (148, false),
            // A union of two container kinds, and returns of both.
            (149, false),
            // `|=` on a dict by annotation.
            (151, true),
            // Two definitions that hand out different kinds.
            (152, false),
            // A static method under the cache.
            (153, true),
            // An item of a generator's value.
            (155, false),
            // A cached function returning its parameter builds nothing.
            (156, false),
            // A `maxsize` heki does not evaluate may be `0`.
            (157, false),
        ],
        "{diags:#?}"
    );
}

/// Under R2 a cached function annotated `-> list` may hand out an instance of a project subclass
/// of `list`; when a project subclass overrides the changing method or operator (`sort`,
/// `setdefault`, `__ior__`), the change may change nothing, so the report states no exact claim.
#[test]
fn mut114_leaves_open_a_project_subclass_overriding_the_change() {
    for (name, subclass, producer, change) in [
        (
            "list_sort",
            "class Frozen(list):\n    def sort(self, *args, **kwargs):\n        pass\n",
            "@lru_cache(maxsize=8)\ndef get(name) -> list:\n    return Frozen([name, \"id\"])\n",
            "    result.sort()\n",
        ),
        (
            "dict_setdefault",
            "class FrozenDict(dict):\n    def setdefault(self, key, default=None):\n        pass\n",
            "@cache\ndef get(name) -> dict[str, object]:\n    return FrozenDict(dict(name=name))\n",
            "    result.setdefault(\"x\", 1)\n",
        ),
        (
            "set_ior",
            "class FrozenSet(set):\n    def __ior__(self, other):\n        pass\n",
            "@lru_cache\ndef get(name) -> set[str]:\n    return FrozenSet({name, \"id\"})\n",
            "    result |= {\"x\"}\n",
        ),
        // A base of the subclass outside `list` (a mixin) defines the method.
        (
            "list_mixin",
            "class NoSort:\n    def sort(self, *args, **kwargs):\n        pass\n\n\nclass Mixed(NoSort, list):\n    pass\n",
            "@lru_cache\ndef get(name) -> list:\n    return Mixed([name])\n",
            "    result.sort()\n",
        ),
        // A metaclass may install the method when it creates the subclass.
        (
            "list_metaclass",
            "class Meta(type):\n    pass\n\n\nclass Made(list, metaclass=Meta):\n    pass\n",
            "@lru_cache\ndef get(name) -> list:\n    return Made([name])\n",
            "    result.sort()\n",
        ),
        // `__getattribute__` decides what `result.sort` is.
        (
            "list_getattribute",
            "class Lazy(list):\n    def __getattribute__(self, name):\n        return object.__getattribute__(self, name)\n",
            "@lru_cache\ndef get(name) -> list:\n    return Lazy([name])\n",
            "    result.sort()\n",
        ),
    ] {
        let p = Project::new(&format!("mut114_override_{name}"));
        let source = format!(
            "from functools import cache, lru_cache\n\n\n{subclass}\n\n{producer}\n\ndef use(name):\n    result = get(name)\n{change}"
        );
        p.write("m.py", &source);
        let diags = diagnostics(&p, &["--select", "MUT114"]);
        // The change is the last line.
        let line = source.lines().count() as u64;
        assert_eq!(claims(&diags), vec![(line, false)], "{name}: {diags:#?}");
        assert_eq!(diags[0]["certainty"], "heuristic", "{name}");
    }
}

/// `__getattribute__` does not decide the method an operator calls (`result += [...]` calls
/// `list.__iadd__` directly), and a list the cached function builds itself is no subclass's, so
/// its `append` and item delete stay the builtin's.
#[test]
fn mut114_keeps_operators_and_built_lists_exact_beside_overrides() {
    let p = Project::new("mut114_operators_and_built");
    p.write(
        "m.py",
        r#"from functools import lru_cache


class Lazy(list):
    def __getattribute__(self, name):
        return object.__getattribute__(self, name)

    def append(self, item):
        pass


@lru_cache
def annotated(name) -> list:
    return Lazy([name])


@lru_cache
def built(name):
    return [name, "id"]


def use(name):
    result = annotated(name)
    result += ["x"]
    fields = built(name)
    fields.append("extra")
    del fields[0]
"#,
    );
    let diags = diagnostics(&p, &["--select", "MUT114"]);
    assert_eq!(
        claims(&diags),
        vec![(24, true), (26, true), (27, true)],
        "{diags:#?}"
    );
}

const HAND_MEMOS: &str = r#"import copy
import shelve
from abc import abstractmethod
from typing import Any

_CACHE = {}
_BY_NAME = {}
_MEMO = {}


def logged(fn):
    return fn


def build(key):
    return {"name": key}


class Configs:
    _class_cache: dict = {}

    def __init__(self):
        self._cache = {}
        self._lists_cache = {}
        self._by_name = {}
        self._buckets = {}
        self._kept = {}
        self._memo = None
        self._shelf_cache = shelve.open("memo")

    def get(self, key):
        if key in self._cache:
            return self._cache[key]
        value = {"name": key}
        self._cache[key] = value
        return value

    def annotated(self, key) -> dict[str, Any]:
        if key in self._cache:
            return self._cache[key]
        value = {"name": key}
        self._cache[key] = value
        return value

    def filled_first(self, key) -> dict:
        if key not in self._cache:
            self._cache[key] = build(key)
        return self._cache[key]

    def defaulted(self, key) -> list:
        return self._lists_cache.setdefault(key, [key])

    def looked_up(self, key) -> dict:
        if (found := self._cache.get(key)) is None:
            found = self._cache[key] = build(key)
        return found

    def lazily(self, key) -> dict:
        if self._memo is None:
            self._memo = {}
        if key not in self._memo:
            self._memo[key] = build(key)
        return self._memo[key]

    @classmethod
    def of_class(cls, key) -> dict:
        if key not in cls._class_cache:
            cls._class_cache[key] = build(key)
        return cls._class_cache[key]

    @staticmethod
    def by_class_name(key) -> dict:
        if Configs._class_cache and key in Configs._class_cache:
            return Configs._class_cache[key]
        value = build(key)
        Configs._class_cache[key] = value
        return value

    def named(self, key) -> dict:
        if key not in self._by_name:
            self._by_name[key] = build(key)
        return self._by_name[key]

    def sometimes(self, key) -> dict:
        if not key:
            return {}
        if key not in self._cache:
            self._cache[key] = build(key)
        return self._cache[key]

    def or_default(self, key) -> dict:
        if key in self._cache:
            return self._cache.get(key, {})
        self._cache[key] = build(key)
        return self._cache[key]

    def deep_copied(self, key) -> dict:
        if key not in self._kept:
            self._kept[key] = build(key)
        return copy.deepcopy(self._kept[key])

    def copied(self, key) -> dict:
        if key not in self._kept:
            self._kept[key] = build(key)
        return dict(self._kept[key])

    def constant_key(self, key) -> dict:
        if "all" not in self._cache:
            self._cache["all"] = build(key)
        return self._cache["all"]

    def per_call(self, key) -> dict:
        self._kept = {}
        if key not in self._kept:
            self._kept[key] = build(key)
        return self._kept[key]

    def bucket(self, key) -> list:
        if key not in self._buckets:
            self._buckets[key] = []
        return self._buckets[key]

    def stored(self, key) -> dict:
        self._kept[key] = build(key)
        return self._kept[key]

    @logged
    def decorated(self, key) -> dict:
        if key not in self._cache:
            self._cache[key] = build(key)
        return self._cache[key]

    def evicting(self, key) -> dict:
        if len(self._cache) > 100:
            self._cache.clear()
        if key not in self._cache:
            self._cache[key] = build(key)
        return self._cache[key]

    @abstractmethod
    def abstract(self, key) -> dict:
        if key not in self._cache:
            self._cache[key] = build(key)
        return self._cache[key]

    def maybe_none(self, key) -> dict:
        if key is None:
            return
        if key not in self._cache:
            self._cache[key] = build(key)
        return self._cache[key]

    def shelved(self, key) -> dict:
        if key not in self._shelf_cache:
            self._shelf_cache[key] = build(key)
        return self._shelf_cache[key]


def module_level(key) -> dict:
    if key not in _CACHE:
        _CACHE[key] = build(key)
    return _CACHE[key]


def registry(key) -> dict:
    if key not in _BY_NAME:
        _BY_NAME[key] = build(key)
    return _BY_NAME[key]


def handed_on(c: Configs, key):
    return c.annotated(key)


def fresh_global(key) -> dict:
    global _MEMO
    _MEMO = {}
    if key not in _MEMO:
        _MEMO[key] = build(key)
    return _MEMO[key]
"#;

const MEMO_USES: &str = r#"from memos import Configs, fresh_global, handed_on, module_level, registry


def tune(c: Configs, k):
    cfg = c.get(k)
    cfg["x"] = 1
    c.annotated(k)["x"] = 1
    c.filled_first(k)["x"] = 1
    c.defaulted(k).append(1)
    c.looked_up(k)["x"] = 1
    c.lazily(k)["x"] = 1
    Configs.of_class(k)["x"] = 1
    Configs.by_class_name(k)["x"] = 1
    module_level(k)["x"] = 1
    c.named(k)["x"] = 1
    registry(k)["x"] = 1
    c.sometimes(k)["x"] = 1
    c.or_default(k)["x"] = 1
    handed_on(c, k)["x"] = 1
    c.annotated(k)["rows"].append(1)
    c.deep_copied(k)["x"] = 1
    c.copied(k)["x"] = 1
    c.constant_key(k)["x"] = 1
    c.per_call(k)["x"] = 1
    c.bucket(k).append(1)
    c.stored(k)["x"] = 1
    c.decorated(k)["x"] = 1
    c.evicting(k)["x"] = 1
    c.abstract(k)["x"] = 1
    c.maybe_none(k)["x"] = 1
    fresh_global(k)["x"] = 1
    c.shelved(k)["x"] = 1
"#;

/// A function that fills a container outliving the call (an attribute of `self` or of the
/// class, a module global) under a key from its arguments, and returns what the container keeps
/// there, is a cache as a `functools` one is: `if k in c: return c[k]` / `c[k] = v; return v`,
/// `if k not in c: c[k] = v` / `return c[k]`, `setdefault`, `.get(k)` then a fill. The exact
/// claim needs every return to hand out the kept object, a builtin dict named as a cache or
/// memo, and a return annotation naming the kind; a copy, a key not from the arguments, a container
/// made per call (also a module global the function rebinds), a registry slot built from none of
/// the arguments, a store a call never finds kept, and a decorator heki does not know make no
/// cache.
#[test]
fn mut114_takes_a_memo_the_function_fills_itself_as_a_cache() {
    let p = Project::new("mut114_hand_memos");
    p.write("memos.py", HAND_MEMOS).write("uses.py", MEMO_USES);
    let diags = diagnostics(&p, &["--select", "MUT114"]);
    assert_eq!(
        claims(&diags),
        vec![
            // No return annotation: the value a call finding its key kept returns was stored by
            // an earlier call or by other code.
            (6, false),
            // Each form, of an instance's and of a module's memo.
            (7, true),
            (8, true),
            (9, true),
            (10, true),
            // A memo made lazily (`None` or a dict), and a class's memo through `cls` and through
            // the class: heki does not type the container a builtin dict there.
            (11, false),
            (12, false),
            (13, false),
            (14, true),
            // A container not named as a cache or memo, a return of a new dict on one path, a
            // `.get` default, a wrapper, and an object below the result.
            (15, false),
            (16, false),
            (17, false),
            (18, false),
            (19, false),
            (20, false),
            // Clearing the memo leaves what it keeps a cached result.
            (28, true),
            // A decorator a cache may not stand beside, and a return of `None`.
            (29, false),
            (30, false),
            // A mapping that is no builtin dict: a shelf stores a copy and reads a new object.
            (32, false),
        ],
        "{diags:#?}"
    );
    let at = |line: u64| diags.iter().find(|d| d["line"] == line).unwrap();
    assert_eq!(
        at(6)["message"],
        "`tune` mutates the shared object returned by cached function `memos.Configs.get`"
    );
    assert_eq!(
        at(7)["message"],
        "`tune` changes in place the dict that `Configs.annotated` returned; `Configs.annotated` keeps it in `self._cache`"
    );
    assert_eq!(at(7)["help"], "change a copy (`dict(...)`) instead");
    assert_eq!(
        at(13)["message"],
        "`tune` mutates the shared object returned by cached function `memos.Configs.by_class_name`"
    );
    assert_eq!(
        at(14)["message"],
        "`tune` changes in place the dict that `module_level` returned; `module_level` keeps it in `_CACHE`"
    );
    // The exact claims are the exact reports a default run shows; the others are heuristic.
    let exact: Vec<u64> = diagnostics(&p, &[])
        .iter()
        .filter(|d| d["rule_code"] == "MUT114" && d["certainty"] == "exact")
        .map(|d| d["line"].as_u64().unwrap())
        .collect();
    assert_eq!(exact, vec![7, 8, 9, 10, 14, 28]);
}

/// A memo function named as a `readonly-returns` producer reports under the contract, as before
/// heki took such a memo for a cache.
#[test]
fn mut114_leaves_a_declared_memo_producer_to_its_contract() {
    let p = Project::new("mut114_declared_memo");
    p.write("memos.py", HAND_MEMOS)
        .write("uses.py", MEMO_USES)
        .write(
            "pyproject.toml",
            "[[tool.heki.readonly-returns]]\nname = \"configs\"\nproducers = [\"memos.Configs.annotated\"]\ndeep = true\n",
        );
    let diags = diagnostics(&p, &["--select", "MUT114,MUT202"]);
    let rule_at = |line: u64| {
        let d = diags.iter().find(|d| d["line"] == line).unwrap();
        d["rule_code"].as_str().unwrap().to_owned()
    };
    for line in [7, 19, 20] {
        assert_eq!(rule_at(line), "MUT202", "{diags:#?}");
    }
    assert_eq!(rule_at(8), "MUT114");
}

/// Each `MUT114` report of the project as (function, certainty, open premises "premise: cause").
fn open_premise_reports(p: &Project) -> Vec<(String, String, Vec<String>)> {
    let (code, stdout, stderr) = p.heki(&["dump-open-premises", "--select", "MUT114", "."]);
    assert_eq!(code, 0, "{stderr}");
    let out: Value = serde_json::from_str(&stdout).unwrap();
    out["reports"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            let open = r["open_premises"]
                .as_array()
                .unwrap()
                .iter()
                .map(|o| {
                    format!(
                        "{}: {}",
                        o["premise"].as_str().unwrap(),
                        o["cause"].as_str().unwrap()
                    )
                })
                .collect();
            (
                r["qualname"].as_str().unwrap().to_owned(),
                r["certainty"].as_str().unwrap().to_owned(),
                open,
            )
        })
        .collect()
}

/// The counterexamples `tests/difftest/cached_result_counterexamples.py` runs on CPython: a call
/// through a protocol, or on a class that takes the method from a base made at run time, may run
/// an implementation heki does not see, which caches nothing, so the change is heuristic with
/// "Every implementation" open; the same change through the cached class stays exact.
#[test]
fn mut114_through_a_call_whose_targets_are_not_every_implementation_is_heuristic() {
    let p = Project::new("mut114_incomplete_targets");
    let source = std::fs::read_to_string(
        common::repo_root().join("tests/difftest/cached_result_counterexamples.py"),
    )
    .unwrap();
    p.write("cases.py", &source);
    let reports = open_premise_reports(&p);
    let open = vec!["Every implementation: incomplete-targets".to_owned()];
    assert_eq!(
        reports,
        vec![
            ("add".to_owned(), "heuristic".to_owned(), open.clone()),
            ("add_listed".to_owned(), "heuristic".to_owned(), open),
            ("add_cached".to_owned(), "exact".to_owned(), Vec::new()),
        ]
    );
}

/// A value that is the result of a protocol call on one path and a new list on another is not
/// "the same object on every path", and its protocol call does not have every implementation as
/// its target: both premises are open, not only the first.
#[test]
fn mut114_a_protocol_result_joined_with_a_new_list_leaves_both_premises_open() {
    let p = Project::new("mut114_incomplete_joined");
    p.write(
        "cases.py",
        r#"import functools
from typing import Protocol


class Repo(Protocol):
    def items(self, k: str) -> list[str]: ...


class CachedRepo:
    @functools.lru_cache(maxsize=None)
    def items(self, k: str) -> list[str]:
        return [k, "id"]


def add(repo: Repo, k: str, flag: bool) -> list[str]:
    xs = repo.items(k) if flag else []
    xs.append("x")
    return xs
"#,
    );
    let open = vec![
        "Same object: not-the-result-on-every-path".to_owned(),
        "Every implementation: incomplete-targets".to_owned(),
    ];
    assert_eq!(
        open_premise_reports(&p),
        vec![("add".to_owned(), "heuristic".to_owned(), open)]
    );
}

/// A protocol call with two project implementations, one of them cached, has two callees, so no
/// one function is its result; its targets are still not every implementation, so "Every
/// implementation" is open next to "Same object".
#[test]
fn mut114_a_protocol_call_with_two_implementations_leaves_every_implementation_open() {
    let p = Project::new("mut114_incomplete_two_callees");
    p.write(
        "cases.py",
        r#"import functools
from typing import Protocol


class Repo(Protocol):
    def items(self, k: str) -> list[str]: ...


class CachedRepo:
    @functools.lru_cache(maxsize=None)
    def items(self, k: str) -> list[str]:
        return [k, "id"]


class PlainRepo:
    def items(self, k: str) -> list[str]:
        return [k]


def add(repo: Repo, k: str) -> list[str]:
    xs = repo.items(k)
    xs.append("x")
    return xs
"#,
    );
    let open = vec![
        "Same object: not-the-result-on-every-path".to_owned(),
        "Every implementation: incomplete-targets".to_owned(),
    ];
    assert_eq!(
        open_premise_reports(&p),
        vec![("add".to_owned(), "heuristic".to_owned(), open)]
    );
}
