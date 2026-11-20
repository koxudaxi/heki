//! What the mutation rules (`MUT101`, `MUT107`, `MUT110`, `MUT202`) take as a cache, an
//! invalidation, a shared value, and a change: decorators known by their binding, invalidation on
//! every path, inherited writers, copies, and generator bodies (end to end, through the real
//! binary).

mod common;

use common::Project;
use serde_json::Value;

fn project(name: &str, config: &str) -> Project {
    let p = Project::new(name);
    p.write(
        "pyproject.toml",
        &format!("[project]\nname = \"t\"\n\n{config}"),
    );
    p
}

/// `(path, line, qualname)` of every diagnostic of `rule`.
fn reports(p: &Project, rule: &str) -> Vec<(String, u64, String)> {
    let (code, stdout, stderr) = p.check(&["--select", rule, "--output-format", "json"]);
    assert!(code == 0 || code == 1, "{stdout}{stderr}");
    let diags: Vec<Value> = serde_json::from_str(&stdout).unwrap();
    diags
        .iter()
        .map(|d| {
            assert_eq!(d["rule_code"], rule, "{d:#}");
            (
                d["path"].as_str().unwrap().to_owned(),
                d["line"].as_u64().unwrap(),
                d["qualname"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn at(path: &str, line: u64, qualname: &str) -> (String, u64, String) {
    (path.to_owned(), line, qualname.to_owned())
}

#[test]
fn cache_decorators_are_known_by_what_their_name_is_bound_to() {
    let p = project("cache_decorator_bindings", "");
    p.write(
        "aliased.py",
        r#"from functools import cached_property as cached


class C:
    def __init__(self, n: int) -> None:
        self.n = n

    @cached
    def doubled(self) -> int:
        return self.n * 2

    def set_n(self, n: int) -> None:
        self.n = n
"#,
    )
    .write(
        "memo.py",
        r#"import functools
from functools import lru_cache as _lru_cache


def cache(fn):
    return fn


@cache
def fresh() -> list[int]:
    return [1]


@_lru_cache
def shared() -> list[int]:
    return [1]


@functools.cache
def also_shared() -> list[int]:
    return [1]


def use() -> None:
    fresh().append(2)
    shared().append(2)
    also_shared().append(2)
"#,
    );
    assert_eq!(reports(&p, "MUT101"), vec![at("aliased.py", 13, "C.set_n")]);
    // `cache` is the module's own identity decorator: `fresh()` returns a new list.
    assert_eq!(
        reports(&p, "MUT202"),
        vec![at("memo.py", 26, "use"), at("memo.py", 27, "use")]
    );
}

#[test]
fn only_memo_dictionary_operations_are_exempt_by_name() {
    let p = project("memo_names", "");
    p.write(
        "app.py",
        r#"from functools import cached_property


class C:
    def __init__(self, n: int) -> None:
        self.memory_limit = n
        self.cache_size = n
        self._cache: dict[int, int] = {}
        self._resultCache: dict[int, int] = {}

    @cached_property
    def summary(self) -> int:
        return self.memory_limit + self.cache_size + len(self._cache) + len(self._resultCache)

    def set_limit(self, n: int) -> None:
        self.memory_limit = n

    def resize(self, n: int) -> None:
        self.cache_size = n

    def remember(self, k: int, v: int) -> None:
        self._cache[k] = v
        self._resultCache[k] = v

    def __hash__(self) -> int:
        return hash((self.memory_limit, self.cache_size, len(self._cache)))
"#,
    );
    // Rebinding a scalar whose name mentions `memory` or `cache` changes what the cache and the
    // hash were computed from; filling a memo dictionary in place does not count.
    let expected = vec![
        at("app.py", 16, "C.set_limit"),
        at("app.py", 19, "C.resize"),
    ];
    assert_eq!(reports(&p, "MUT101"), expected);
    assert_eq!(reports(&p, "MUT107"), expected);
}

const BASE: &str = r#"class Base:
    def __init__(self, name: str) -> None:
        self.name = name

    def rename(self, name: str) -> None:
        self.name = name

    def retitle(self, name: str) -> None:
        self.name = name
"#;

#[test]
fn methods_a_class_inherits_write_what_its_caches_and_hash_read() {
    let p = project("inherited_writers", "");
    p.write("pkg/__init__.py", "")
        .write("pkg/base.py", BASE)
        .write(
            "pkg/child.py",
            r#"from functools import cached_property

from pkg.base import Base


class Child(Base):
    @cached_property
    def label(self) -> str:
        return self.name.upper()

    def retitle(self, name: str) -> None:
        self.name = name
        del self.label

    def __hash__(self) -> int:
        return hash(self.name)
"#,
        );
    // `Base.rename` runs on `Child` instances; `Base.retitle` does not (`Child` overrides it,
    // and its override invalidates the cache).
    assert_eq!(
        reports(&p, "MUT101"),
        vec![at("pkg/base.py", 6, "Base.rename")]
    );
    assert_eq!(
        reports(&p, "MUT107"),
        vec![
            at("pkg/base.py", 6, "Base.rename"),
            at("pkg/child.py", 12, "Child.retitle"),
        ]
    );
}

#[test]
fn a_write_leaves_no_stale_cached_value_on_any_path() {
    let p = project("invalidation_paths", "");
    p.write(
        "app.py",
        r#"from functools import cached_property


class C:
    def __init__(self, n: int) -> None:
        self.n = n

    @cached_property
    def doubled(self) -> int:
        return self.n * 2

    def _reset(self) -> None:
        del self.doubled

    def only_if(self, n: int, clear: bool) -> None:
        self.n = n
        if clear:
            self.__dict__.pop("doubled", None)

    def before(self, n: int) -> None:
        self.__dict__.pop("doubled", None)
        self.n = n

    def before_reading(self, n: int) -> int:
        self.__dict__.pop("doubled", None)
        old = self.doubled
        self.n = n
        return old

    def always(self, n: int) -> None:
        self.n = n
        self.__dict__.pop("doubled", None)

    def both_branches(self, n: int, fast: bool) -> None:
        self.n = n
        if fast:
            del self.doubled
        else:
            self._reset()

    def through_helper(self, n: int) -> None:
        self.n = n
        self._reset()

    def or_raise(self, n: int) -> None:
        self.n = n
        if n < 0:
            raise ValueError(n)
        self.__dict__.clear()
"#,
    );
    // An invalidation before the write also leaves nothing stale when nothing reads the
    // property in between (`before`); reading it computes the old value again
    // (`before_reading`).
    assert_eq!(
        reports(&p, "MUT101"),
        vec![
            at("app.py", 16, "C.only_if"),
            at("app.py", 27, "C.before_reading")
        ]
    );
}

const DEEP_SHARED: &str = "[[tool.heki.readonly-returns]]\nname = \"shared\"\nproducers = [\"app.shared\"]\ndeep = true\n";

#[test]
fn a_value_shared_on_one_branch_stays_shared_after_the_merge() {
    let p = project("shared_merge", DEEP_SHARED);
    p.write(
        "app.py",
        r#"CACHE = [[1], [2]]


def shared() -> list[list[int]]:
    return CACHE


def copy_first(flag: bool) -> None:
    x = shared().copy() if flag else shared()
    x.clear()


def copy_second(flag: bool) -> None:
    x = shared() if flag else shared().copy()
    x.clear()


def in_branches(flag: bool) -> None:
    if flag:
        x = shared().copy()
    else:
        x = shared()
    x.clear()


def copies_only(flag: bool) -> None:
    x = shared().copy() if flag else list(shared())
    x.clear()
"#,
    );
    assert_eq!(
        reports(&p, "MUT202"),
        vec![
            at("app.py", 10, "copy_first"),
            at("app.py", 15, "copy_second"),
            at("app.py", 23, "in_branches"),
        ]
    );
}

#[test]
fn a_slice_of_a_shared_list_is_a_new_list_holding_the_shared_elements() {
    let p = project("shared_slices", DEEP_SHARED);
    p.write(
        "app.py",
        r#"import functools

CACHE = [[1], [2]]


def shared() -> list[list[int]]:
    return CACHE


@functools.cache
def rows() -> list[int]:
    return [1]


def window() -> None:
    w = shared()[:]
    w.clear()
    rows()[1:].append(2)


def element_of_slice() -> None:
    shared()[:][0].append(3)


def window_of_unknown(xs) -> None:
    w = xs()[:]
    w.clear()


def sliced() -> list[list[int]]:
    return shared()[:]


def through_wrapper() -> None:
    sliced().clear()
    sliced()[0].append(4)
"#,
    );
    // A wrapper returning a slice hands out a new list holding the shared elements.
    assert_eq!(
        reports(&p, "MUT202"),
        vec![
            at("app.py", 22, "element_of_slice"),
            at("app.py", 36, "through_wrapper")
        ]
    );
}

#[test]
fn a_generator_changes_what_it_is_handed_only_where_it_is_iterated() {
    let p = project(
        "generator_changes",
        "[[tool.heki.readonly-returns]]\nname = \"shared\"\nproducers = [\"app.shared\"]\n",
    );
    p.write(
        "app.py",
        r#"from collections.abc import Sequence
from dataclasses import dataclass


@dataclass(frozen=True)
class Record:
    items: list[int]


CACHE = [1]


def shared() -> list[int]:
    return CACHE


def wipe(xs):
    xs.clear()
    yield 1


def made_only(xs: Sequence[int], rec: Record) -> None:
    wipe(xs)
    wipe(rec.items)
    wipe(shared())


def consumed(xs: Sequence[int], rec: Record) -> None:
    list(wipe(xs))
    for _ in wipe(rec.items):
        pass
    sum(wipe(shared()))
"#,
    );
    assert_eq!(reports(&p, "MUT110"), vec![at("app.py", 29, "consumed")]);
    assert_eq!(reports(&p, "MUT108"), vec![at("app.py", 30, "consumed")]);
    assert_eq!(reports(&p, "MUT202"), vec![at("app.py", 32, "consumed")]);
}

#[test]
fn any_and_all_stop_early_and_leave_the_rest_of_an_iterator() {
    let p = project("short_circuit_consumers", "");
    p.write(
        "app.py",
        r#"import time


def after_any() -> list[int]:
    it = iter([1, 2])
    if any(it):
        return list(it)
    return []


def after_all() -> list[int]:
    it = iter([1, 2])
    all(it)
    return list(it)


def after_sum() -> list[int]:
    it = iter([1, 2])
    sum(it)
    return list(it)


def any_after_list() -> bool:
    it = iter([1, 2])
    list(it)
    return any(it)


def slow():
    time.sleep(1)
    yield 1


async def handler() -> bool:
    return any(slow())
"#,
    );
    let (code, stdout, stderr) =
        p.check(&["--select", "MUT106,EFF101", "--output-format", "concise"]);
    assert_eq!(code, 1, "{stdout}{stderr}");
    let lines: Vec<&str> = stdout.lines().collect();
    // `any` still runs the generator's body on the event loop.
    assert_eq!(lines.len(), 3, "{stdout}");
    assert!(lines[0].starts_with("app.py:20:12: MUT106"), "{stdout}");
    assert!(lines[1].starts_with("app.py:26:12: MUT106"), "{stdout}");
    assert!(lines[2].starts_with("app.py:35:12: EFF101"), "{stdout}");
}

#[test]
fn a_helper_rebinding_a_shallow_copys_attribute_leaves_the_shared_value_alone() {
    let p = project(
        "shallow_copy_helpers",
        "[[tool.heki.readonly-returns]]\nname = \"box\"\nproducers = [\"app.box\"]\ndeep = true\n",
    );
    p.write(
        "app.py",
        r#"import copy


class Inner:
    def __init__(self) -> None:
        self.n = 0


class Box:
    def __init__(self) -> None:
        self.name = "old"
        self.tags: list[str] = []
        self.inner = Inner()


BOX = Box()


def box() -> Box:
    return BOX


def rename(b):
    b.name = "new"


def tag(b):
    b.tags.append("x")


def bump(b):
    b.inner.n += 1


def clear_all(xs):
    xs.clear()


def tag_through(b):
    clear_all(b.tags)


def use() -> None:
    rename(copy.copy(box()))
    tag(copy.copy(box()))
    bump(copy.copy(box()))
    tag_through(copy.copy(box()))
"#,
    );
    assert_eq!(
        reports(&p, "MUT202"),
        vec![
            at("app.py", 45, "use"),
            at("app.py", 46, "use"),
            at("app.py", 47, "use")
        ]
    );
}
