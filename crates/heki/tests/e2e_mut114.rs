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
    // A cached method called through a bound-method alias, and a method keyed by `self` alone.
    let formatter: Vec<Value> = diags
        .iter()
        .filter(|d| d["path"] == "producers.py")
        .cloned()
        .collect();
    assert_eq!(
        claims(&formatter),
        vec![(79, true), (80, false)],
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
        ],
        "{diags:#?}"
    );
}
