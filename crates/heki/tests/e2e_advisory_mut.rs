//! Mutation rules end to end: `MUT101` filters (aliases, deletions, memo caches, `__dict__`
//! invalidation, overriding properties, `per-file-ignores`), `MUT201` write owners (helpers,
//! cycles, shortest chains, `setattr`, deletions, phis), and `MUT202` read-only producers
//! (unpacking, properties, phis, in-place operators, helper calls, shallow copies).

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

const CART: &str = r#"from functools import cached_property


class Cart:
    def __init__(self):
        self.items = []
        self._cache = {}
        self.meta = {}

    @cached_property
    def total(self):
        return len(self.items) + len(self._cache) + len(self.meta)

    def add(self, x):
        items = self.items
        items.append(x)

    def drop(self):
        del self.items

    def pop_first(self):
        del self.items[0]

    def remember(self, k, v):
        self._cache[k] = v

    def reset(self):
        self.items.clear()
        del self.__dict__["total"]

    def rename(self, name, v):
        self.__dict__[name] = v
        self.__dict__.pop(name)
        self.__dict__.update({})
        d = self.__dict__
        d.pop("total")
        self.meta["k"] = v


class SpecialCart(Cart):
    @cached_property
    def total(self):
        return len(self.items)

    def grow(self, x):
        self.items.append(x)


class Ledger:
    def __init__(self):
        self.rows = []
        self.index = {}

    @cached_property
    def size(self):
        return len(self.rows) + len(self.index)

    def append(self, row):
        self._push(row)
        del self.size

    def _push(self, row):
        self.rows.append(row)

    def _orphan(self, row):
        self.rows.append(row)

    def lookup(self, k):
        if k not in self.index:
            self.index[k] = k
        return self.index[k]
"#;

#[test]
fn mut101_filters_aliases_deletions_and_overriding_properties() {
    let p = Project::new("mut101_edges");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[tool.heki.lint]\nper-file-ignores = { \"legacy.py\" = [\"MUT101\"] }\n",
    )
    .write("cart.py", CART)
    .write("legacy.py", CART);
    let diags = diagnostics(&p, &["--select", "MUT101"]);
    let c = "cart.py";
    // Reported: a write through a local alias, `del self.items`, `del self.items[0]`, and a
    // subclass method (once, although `SpecialCart.total` overrides `Cart.total`). Not
    // reported: the memo cache `_cache`, and methods that invalidate `total` through
    // `self.__dict__` (`del self.__dict__["total"]`, `d = self.__dict__; d.pop("total")`).
    // A private helper whose every caller invalidates `size` is covered (`Ledger._push`), one
    // without callers is not (`Ledger._orphan`); a memo-shaped store (`if k not in
    // self.index:`) is not a violation. `legacy.py` is ignored for `MUT101`.
    assert_eq!(
        found(&diags),
        vec![
            d(c, 16, "MUT101", "Cart.add"),
            d(c, 19, "MUT101", "Cart.drop"),
            d(c, 22, "MUT101", "Cart.pop_first"),
            d(c, 46, "MUT101", "SpecialCart.grow"),
            d(c, 66, "MUT101", "Ledger._orphan"),
        ],
        "{diags:#?}"
    );
    assert_eq!(diags[0]["target"], "Cart.items->total");
}

/// `name_k = name_(k-1)` copies of the attribute name, then `setattr(m, name_last, [])`.
fn setattr_through(copies: usize, name: &str) -> String {
    let mut body = format!("def {name}(m: Model):\n    a0 = \"fields\"\n");
    for k in 1..copies {
        body.push_str(&format!("    a{k} = a{}\n", k - 1));
    }
    body.push_str(&format!("    setattr(m, a{}, [])\n", copies - 1));
    body
}

#[test]
fn mut201_follows_helpers_cycles_and_attribute_aliases() {
    let p = Project::new("mut201_edges");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.write-owners]]\nname = \"store\"\ntargets = [\"Model.fields\", \"Model.a*\"]\nallowed-writers = [\"Store.*\"]\n",
    )
    .write(
        "app.py",
        &format!(
            r#"class Model:
    def __init__(self):
        self.fields = []
        self.inner = None
        self.a1 = 0
        self.a2 = 0


class Store:
    def add(self, m: Model, f):
        m.fields.append(f)


def _write(m: Model):
    m.fields.append(1)


def _via(m: Model):
    _write(m)


def longer_first(m: Model):
    _via(m)
    _write(m)


def shorter_first(m: Model):
    _write(m)
    _via(m)


def ping(m: Model, n):
    m.fields.append(n)
    if n:
        pong(m, n - 1)


def pong(m: Model, n):
    ping(m, n)


def add_field(fields):
    fields.append(1)


def build():
    m = Model()
    add_field(m.fields)
    return m


def _set_two(m: Model):
    m.a1 = 1
    m.a2 = 2


def two(m: Model):
    _set_two(m)


def named(m: Model):
    name = "fields"
    setattr(m, name, [])
    setattr(m, compute(), [])


def compute():
    return "fields"


def deep_write(m: Model):
    m.inner.x = 1


def remove(m: Model):
    del m.fields[0]


def choose(a: Model, b: Model, flag):
    if flag:
        fs = a.fields
    else:
        fs = b.fields
    fs.append(1)


{}

{}"#,
            setattr_through(15, "chain_ok"),
            setattr_through(16, "chain_long"),
        ),
    );
    let diags = diagnostics(&p, &["--select", "MUT201"]);
    let a = "app.py";
    assert_eq!(
        found(&diags),
        vec![
            // The shortest chain wins wherever it is found (`_write` directly).
            d(a, 24, "MUT201", "longer_first"),
            d(a, 28, "MUT201", "shorter_first"),
            // A recursion cycle: `ping` reports its own write; `pong` reaches it through a
            // public function, which reports for itself.
            d(a, 33, "MUT201", "ping"),
            // A helper mutating the field of a local `Model`.
            d(a, 48, "MUT201", "build"),
            // Two writes behind one call under one target pattern: one report.
            d(a, 58, "MUT201", "two"),
            // `setattr` with a constant name bound to a local.
            d(a, 63, "MUT201", "named"),
            d(a, 76, "MUT201", "remove"),
            d(a, 84, "MUT201", "choose"),
            // The name is followed through 15 copies, not through 16.
            d(a, 103, "MUT201", "chain_ok"),
        ],
        "{diags:#?}"
    );
    let kinds: Vec<&str> = diags
        .iter()
        .map(|x| x["message"].as_str().unwrap())
        .filter_map(|m| m.split('(').nth(1)?.split(')').next())
        .collect();
    assert_eq!(
        kinds,
        vec![
            "container_mut",
            "container_mut",
            "container_mut",
            "indirect_helper",
            "direct_attr",
            "set_attr",
            "container_mut",
            "alias_container_mut",
            "set_attr",
        ]
    );
    // `deep_write` mutates `m.inner`, not a guarded field.
    let (_, stdout, _) = p.heki(&["snapshot"]);
    let snap: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        snap["functions"]["app:deep_write"]["mutates_params"],
        serde_json::json!(["m"])
    );
}

const BOTH: &str = r#"class Model:
    def __init__(self, other: "Model") -> None:
        self.items: list[int] = []
        _both(self, other.items)


def _both(m: Model, xs) -> None:
    m.items = []
    xs.append(1)


def same(m: Model) -> None:
    _both(m, m.items)
"#;

#[test]
fn mut201_tells_apart_the_objects_a_helper_writes_through_its_arguments() {
    let p = Project::new("mut201_written_objects");
    p.write(
        "pyproject.toml",
        "[[tool.heki.write-owners]]\nname = \"store\"\ntargets = [\"Model.items\"]\nallowed-writers = [\"Store.*\"]\n",
    )
    .write("app.py", BOTH);
    let diags = diagnostics(&p, &["--select", "MUT201"]);
    // The initializer's own `self.items` is its construction; `other.items`, changed through
    // the helper's untyped parameter, is another object's field. When both arguments are one
    // object's (`same`), the helper's typed write of the field is that write: one site.
    assert_eq!(
        found(&diags),
        vec![
            d("app.py", 4, "MUT201", "Model.__init__"),
            d("app.py", 13, "MUT201", "same"),
        ],
        "{diags:#?}"
    );
    assert!(
        diags[0]["message"]
            .as_str()
            .unwrap()
            .contains("(indirect_helper) via `app._both`"),
        "{diags:#?}"
    );
    assert!(diags.iter().all(|x| x.get("sites").is_none()), "{diags:#?}");
}

#[test]
fn mut201_forbids_cached_properties_on_guarded_classes_and_their_subclasses() {
    let p = Project::new("mut201_cached");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.write-owners]]\nname = \"store\"\ntargets = [\"Model.fields\"]\nforbid-cached-property-on = [\"Model\"]\n",
    )
    .write(
        "app.py",
        r#"from functools import cached_property


class Model:
    def __init__(self):
        self.fields = []

    @cached_property
    def size(self):
        return len(self.fields)


class Sub(Model):
    @cached_property
    def first(self):
        return self.fields[0]


class Other:
    @cached_property
    def size(self):
        return 0
"#,
    );
    let diags = diagnostics(&p, &["--select", "MUT201"]);
    assert_eq!(
        found(&diags),
        vec![
            d("app.py", 8, "MUT201", "Model.size"),
            d("app.py", 14, "MUT201", "Sub.first"),
        ],
        "{diags:#?}"
    );
    assert_eq!(
        diags[0]["message"],
        "`@cached_property` `size` is defined on `Model`, which `store` forbids caching on"
    );
    assert_eq!(diags[1]["target"], "Sub.first");
}

const READONLY: &str = r#"import copy
from functools import lru_cache


class Schema:
    def __init__(self):
        self.fields = []
        self.extra = {}

    def reset(self):
        self.fields = []


class Mutator:
    def apply(self, items):
        items.append(1)


class Factory:
    @classmethod
    def create(cls, n):
        return cls()

    def again(self):
        return self.create(1)


@lru_cache
def load() -> list:
    return []


@lru_cache
def load_pair():
    return [], []


@lru_cache
def load_schema() -> Schema:
    return Schema()


def mutate(items):
    items.append(1)


def mutate_attr(s):
    s.fields.append(1)


def touch(items, **kw):
    return len(items)


def unpack():
    a, b = load_pair()
    a.append(1)


def prop(s: Schema):
    s.fields.append(1)


def phi(flag):
    if flag:
        x = load()
    else:
        x = []
    x.append(1)


def delete():
    s = load_schema()
    del s.extra


def inplace():
    items = load()
    items += [1]


def via_method(m: Mutator):
    m.apply(load())


def via_bound():
    s = load_schema()
    s.reset()


def via_kwargs():
    mutate(items=load())


def shallow():
    s = load_schema()
    c = copy.copy(s)
    mutate_attr(c)
    mutate(c)


def harmless():
    touch(load(), extra=1)


import typing

from settings_lib import get_settings
from typing import *


@lru_cache
def load_rows():
    return []


@lru_cache
def load_table():
    return {}


def typed_alias():
    rows: typing.List[int] = load_rows()
    rows += [1]


def typed_star():
    table: Dict[str, int] = load_table()
    table |= {"k": 1}


def external():
    s = get_settings()
    s.debug = True
    s["k"] = 1
"#;

#[test]
fn mut202_tracks_read_only_values_through_unpacking_phis_and_calls() {
    let p = Project::new("mut202_edges");
    p.write(
        "pyproject.toml",
        "[project]\nname = \"t\"\n\n[[tool.heki.readonly-returns]]\nname = \"schema\"\nproducers = [\"Schema.fields\", \"settings_lib.get_settings\"]\n",
    )
    .write("ro.py", READONLY);
    let diags = diagnostics(&p, &["--select", "MUT202"]);
    let r = "ro.py";
    // Not reported: mutating the top level of a shallow copy (`mutate(c)`), and passing a
    // read-only value to a function that does not mutate it (`touch`). In-place operators
    // on values annotated `typing.List[...]` or `Dict[...]` (a star import from `typing`)
    // mutate a mutable builtin; attribute and item stores mutate an external producer's value.
    assert_eq!(
        found(&diags),
        vec![
            d(r, 57, "MUT202", "unpack"),
            d(r, 61, "MUT202", "prop"),
            d(r, 69, "MUT202", "phi"),
            d(r, 74, "MUT202", "delete"),
            d(r, 79, "MUT202", "inplace"),
            d(r, 83, "MUT202", "via_method"),
            d(r, 88, "MUT202", "via_bound"),
            d(r, 92, "MUT202", "via_kwargs"),
            d(r, 98, "MUT202", "shallow"),
            d(r, 124, "MUT202", "typed_alias"),
            d(r, 129, "MUT202", "typed_star"),
            d(r, 134, "MUT202", "external"),
            d(r, 135, "MUT202", "external"),
        ],
        "{diags:#?}"
    );
    let producers: Vec<&str> = diags
        .iter()
        .map(|x| x["target"].as_str().unwrap())
        .collect();
    assert_eq!(producers[1], "ro.Schema.fields");
    assert!(
        producers[..11]
            .iter()
            .enumerate()
            .all(|(i, t)| i == 1 || t.starts_with("ro.load"))
    );
    assert_eq!(
        producers[11..],
        ["settings_lib.get_settings", "settings_lib.get_settings"]
    );
    // Calls through a project function carry the hop to it.
    assert_eq!(
        diags[5]["related"][0]["message"],
        "`via_method` calls `ro.Mutator.apply`"
    );
}

/// An invalidation before a `try` covers the write in its body; a key computed at run time
/// (`f"_{name}_cache"`) names no cached property, so the path popping it leaves `total` stale.
const CART_RELOADS: &str = r#"from functools import cached_property


class Cart:
    def __init__(self):
        self.items = []

    @cached_property
    def total(self):
        return len(self.items)

    def retitle(self, rows, name, full):
        self.items = list(rows)
        if full:
            del self.total
        else:
            self.__dict__.pop(f"_{name}_cache", None)

    def reload(self, load):
        del self.total
        try:
            self.items = load()
        finally:
            print("reloaded")
"#;

#[test]
fn mut101_covers_writes_in_a_try_after_an_invalidation_but_not_a_computed_key() {
    let p = Project::new("mut101_reloads");
    p.write("cart.py", CART_RELOADS);
    let diags = diagnostics(&p, &["--select", "MUT101"]);
    assert_eq!(
        found(&diags),
        vec![d("cart.py", 13, "MUT101", "Cart.retitle")],
        "{diags:#?}"
    );
}

/// A method call under an `isinstance` check of its receiver mutates what the methods of the
/// receiver's declared class and its subclasses mutate.
const NARROWED_RECEIVERS: &str = r#"import functools


@functools.cache
def cached():
    return [1, 2]


class Base:
    def clear(self, xs):
        xs.clear()


class Sub(Base):
    def clear(self, xs):
        xs.clear()


def narrowed(b: Base):
    if isinstance(b, Sub):
        b.clear(cached())
"#;

#[test]
fn mut202_follows_a_call_on_a_narrowed_receiver() {
    let p = Project::new("mut202_narrowed");
    p.write("app.py", NARROWED_RECEIVERS);
    let diags = diagnostics(&p, &["--select", "MUT202"]);
    assert_eq!(
        found(&diags),
        vec![d("app.py", 21, "MUT202", "narrowed")],
        "{diags:#?}"
    );
}

/// A string constant names the cached property a method removes from `self.__dict__`, as for
/// the contracts: a module-level name declared `Final` or bound once, a `ClassVar`.
#[test]
fn mut101_follows_constants_naming_the_property() {
    let p = Project::new("mut101_final_keys");
    p.write(
        "cart.py",
        r#"from functools import cached_property
from typing import ClassVar, Final

TOTAL: Final = "total"
OTHER: Final = "other"
ONCE = "total"


class Cart:
    TOTAL_KEY: ClassVar[str] = "total"

    def __init__(self):
        self.items = []

    @cached_property
    def total(self):
        return len(self.items)

    def reset(self):
        self.items = []
        self.__dict__.pop(TOTAL, None)

    def reset_other(self):
        self.items = []
        self.__dict__.pop(OTHER, None)

    def clear(self):
        self.items = []
        del self.__dict__[TOTAL]

    def reset_once(self):
        self.items = []
        self.__dict__.pop(ONCE, None)

    def reset_class_key(self):
        self.items = []
        self.__dict__.pop(self.TOTAL_KEY, None)
"#,
    );
    let diags = diagnostics(&p, &["--select", "MUT101"]);
    assert_eq!(
        found(&diags),
        vec![d("cart.py", 24, "MUT101", "Cart.reset_other")]
    );
}
