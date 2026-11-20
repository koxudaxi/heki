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
