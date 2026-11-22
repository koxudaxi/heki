//! `MUT202` and `MUT114` return flow and `MUT109` `write-without-invalidation` end to end.
//!
//! - `MUT202` (declared producers) and `MUT114` (`@lru_cache` / `@cache` functions): a function
//!   returning a protected value (or a part of it) is a producer itself, without configuration:
//!   wrappers several levels deep and through recursion, a copy on one path (protected) or on
//!   every path (not), attributes of deep and shallow producers, shallow copies, a mutating
//!   method on the call result itself, external and attribute producers, cached wrappers and
//!   property getters; generators, coroutines, and values stored in an attribute are not
//!   followed.
//! - `MUT109`: writes to `invalidation` targets (direct, through an alias, through private
//!   helpers) must reach an invalidator on every normal path, directly or through a helper
//!   that announces on all of its paths; not before the write, not only on one branch, not
//!   from an unawaited coroutine. Exempt writers, construction, objects of unknown origin,
//!   raising paths, and callers that announce in bulk (also from a distance) are not reported;
//!   a deferred context manager is not followed. Configuration errors and the language server.

mod common;

use common::Project;
use common::lsp::{Lsp, codes, uri};
use serde_json::{Value, json};

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

/// The related locations of a diagnostic: `(message, line)`.
fn related(diag: &Value) -> Vec<(String, u64)> {
    diag["related"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["message"].as_str().unwrap().to_owned(),
                r["line"].as_u64().unwrap(),
            )
        })
        .collect()
}

// ============================================================================== MUT202

const RO_CONFIG: &str = r#"[project]
name = "t"

[[tool.heki.readonly-returns]]
name = "shared-schema"
producers = ["pkg.graph.Graph.schema", "settings_lib.get_settings", "Model.fields"]
deep = true

[[tool.heki.readonly-returns]]
name = "flat"
producers = ["pkg.graph.Graph.flat"]
"#;

const GRAPH: &str = r#"from __future__ import annotations

from typing import Any


class Schema:
    def __init__(self) -> None:
        self.items: list[Any] = []
        self.ref: str | None = None

    def set_ref(self, ref: str) -> None:
        self.ref = ref


class Graph:
    def __init__(self) -> None:
        self._schemas: dict[int, Schema] = {}

    def schema(self, raw: object) -> Schema:
        return self._schemas.setdefault(id(raw), Schema())

    def flat(self, raw: object) -> Schema:
        return self._schemas.setdefault(id(raw), Schema())
"#;
const READERS: &str = r#"from __future__ import annotations

import copy

from settings_lib import get_settings

from pkg.graph import Graph, Schema


class Parser:
    def __init__(self) -> None:
        self.graph = Graph()


def schema_of(parser: Parser, raw: object) -> Schema:
    return parser.graph.schema(raw)


def reader(parser: Parser, raw: object) -> None:
    schema_of(parser, raw).items.append(1)


def reader_ok(parser: Parser, raw: object) -> None:
    own = copy.deepcopy(schema_of(parser, raw))
    own.items.append(1)


def _inner(parser: Parser, raw: object) -> Schema:
    return schema_of(parser, raw)


def outer(parser: Parser, raw: object) -> Schema:
    return _inner(parser, raw)


def two_levels(parser: Parser, raw: object) -> None:
    outer(parser, raw).ref = "x"


def maybe_copy(parser: Parser, raw: object, fresh: bool) -> Schema:
    if fresh:
        return copy.deepcopy(parser.graph.schema(raw))
    return parser.graph.schema(raw)


def copy_on_one_path(parser: Parser, raw: object) -> None:
    maybe_copy(parser, raw, True).items.clear()


def always_copy(parser: Parser, raw: object) -> Schema:
    return copy.deepcopy(parser.graph.schema(raw))


def copy_on_every_path(parser: Parser, raw: object) -> None:
    always_copy(parser, raw).items.clear()


def items_of(parser: Parser, raw: object) -> list:
    return parser.graph.schema(raw).items


def element_wrapper(parser: Parser, raw: object) -> None:
    items_of(parser, raw).append(1)


def flat_items(parser: Parser, raw: object) -> list:
    return parser.graph.flat(raw).items


def shallow_element_ok(parser: Parser, raw: object) -> None:
    flat_items(parser, raw).append(1)


def shallow_copy_of(parser: Parser, raw: object) -> Schema:
    return copy.copy(parser.graph.schema(raw))


def shallow_copy_wrapper(parser: Parser, raw: object) -> None:
    s = shallow_copy_of(parser, raw)
    s.ref = "own"
    s.items.append(1)


def method_on_result(parser: Parser, raw: object) -> None:
    schema_of(parser, raw).set_ref("y")


def settings():
    return get_settings()


def external_wrapper() -> None:
    settings().debug = True


class Model:
    def __init__(self) -> None:
        self.fields: list[str] = []


def fields_of(m: Model) -> list[str]:
    return m.fields


def attribute_wrapper(m: Model) -> None:
    fields_of(m).append("x")


def lookup(parser: Parser, raw: object, depth: int) -> Schema:
    if depth:
        return walk(parser, raw, depth - 1)
    return parser.graph.schema(raw)


def walk(parser: Parser, raw: object, depth: int) -> Schema:
    return lookup(parser, raw, depth)


def recursive_wrappers(parser: Parser, raw: object) -> None:
    walk(parser, raw, 3).items.pop()


def schemas(parser: Parser, raw: object):
    yield parser.graph.schema(raw)


def generator_ok(parser: Parser, raw: object) -> None:
    for s in schemas(parser, raw):
        s.ref = "g"


class Keeper:
    def __init__(self, parser: Parser) -> None:
        self.parser = parser
        self.kept: Schema | None = None

    def keep(self, raw: object) -> None:
        self.kept = schema_of(self.parser, raw)

    def change_kept(self) -> None:
        if self.kept is not None:
            self.kept.ref = "later"


def same_value_twice(parser: Parser, raw: object, flag: bool) -> Schema:
    s = parser.graph.schema(raw)
    if flag:
        return s
    return s


def twice_returned(parser: Parser, raw: object) -> None:
    same_value_twice(parser, raw, True).ref = "t"


if Parser.__doc__:

    def settings_variant():
        return get_settings()

else:

    def settings_variant():
        return get_settings()


def variant_wrapper() -> None:
    settings_variant().debug = False


def either(parser: Parser, raw: object, whole: bool) -> object:
    s = parser.graph.schema(raw)
    return s if whole else s.items


def either_wrapper(parser: Parser, raw: object) -> None:
    either(parser, raw, False).append(1)


def schema_task(parser: Parser, raw: object):
    yield None
    return parser.graph.schema(raw)


def task_of(parser: Parser, raw: object):
    return schema_task(parser, raw)


def generator_return_ok(parser: Parser, raw: object) -> None:
    task_of(parser, raw).items.pop()
"#;
const CACHED: &str = r#"from functools import lru_cache


@lru_cache
def load_table() -> dict[str, list[int]]:
    return {"a": []}


def table():
    return load_table()


def lru_wrapper() -> None:
    table()["a"].append(1)


def lru_wrapper_ok() -> int:
    return len(table()["a"])


class Holder:
    @property
    def rows(self) -> dict[str, list[int]]:
        return load_table()

    def via_property(self) -> None:
        self.rows["b"] = []


async def atable():
    return load_table()


async def async_wrapper_ok() -> None:
    (await atable())["c"] = []


@lru_cache
def load_pair() -> tuple[list[int], list[int]]:
    return [], []


def first_of_pair() -> list[int]:
    first, second = load_pair()
    return first


def unpacked_wrapper() -> None:
    first_of_pair().append(1)


@lru_cache
async def aload() -> dict[str, int]:
    return {}


def coroutine_of():
    return aload()


def use_coroutine_ok() -> None:
    coroutine_of().close()


def ready() -> bool:
    return True


def table_until_ready():
    while True:
        if ready():
            return {"a": []}
    return load_table()


def unreached_return_ok() -> None:
    table_until_ready()["a"].append(1)
"#;

#[test]
fn mut202_follows_protected_values_through_returns() {
    let p = Project::new("mut202_returns");
    p.write("pyproject.toml", RO_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/graph.py", GRAPH)
        .write("pkg/readers.py", READERS)
        .write("pkg/cached.py", CACHED);
    let diags = diagnostics(&p, &["--select", "MUT202,MUT114"]);
    let (r, c) = ("pkg/readers.py", "pkg/cached.py");
    // Not reported: a deep copy of a wrapper's value (`reader_ok`), a wrapper that copies on
    // every path, an element of a shallow producer's value, a shallow copy's own attribute
    // (line 80), a generator's values, a value stored in an attribute and changed in another
    // method (`Keeper`: not followed), a read of a cached value, a coroutine's result (also an
    // `@lru_cache` coroutine returned without `await`), and a generator's `return` value (it
    // ends the iteration; calling the generator hands out the generator), and a cached value
    // returned only after a `while True:` loop no `break` leaves. A value returned on two paths, an
    // item of an unpacked cached pair, and a producer wrapped by both definitions of a
    // conditionally defined function are followed.
    assert_eq!(
        found(&diags),
        vec![
            d(c, 14, "MUT114", "lru_wrapper"),
            d(c, 27, "MUT114", "Holder.via_property"),
            d(c, 49, "MUT114", "unpacked_wrapper"),
            d(r, 20, "MUT202", "reader"),
            d(r, 37, "MUT202", "two_levels"),
            d(r, 47, "MUT202", "copy_on_one_path"),
            d(r, 63, "MUT202", "element_wrapper"),
            d(r, 81, "MUT202", "shallow_copy_wrapper"),
            d(r, 85, "MUT202", "method_on_result"),
            d(r, 93, "MUT202", "external_wrapper"),
            d(r, 106, "MUT202", "attribute_wrapper"),
            d(r, 120, "MUT202", "recursive_wrappers"),
            d(r, 153, "MUT202", "twice_returned"),
            d(r, 168, "MUT202", "variant_wrapper"),
            d(r, 177, "MUT202", "either_wrapper"),
        ],
        "{diags:#?}"
    );
    let by_name = |q: &str| diags.iter().find(|x| x["qualname"] == q).unwrap();
    // The target is the declared producer; the message names the wrapper called here.
    let reader = by_name("reader");
    assert_eq!(reader["target"], "pkg.graph.Graph.schema");
    assert_eq!(
        reader["message"],
        "`reader` mutates the shared object returned by read-only producer `pkg.graph.Graph.schema` through `pkg.readers.schema_of`"
    );
    // The chain goes from the wrapper's call down to the producer's call.
    assert_eq!(
        related(by_name("two_levels")),
        vec![
            ("`two_levels` calls `pkg.readers.outer`".to_owned(), 37),
            ("`outer` calls `pkg.readers._inner`".to_owned(), 33),
            ("`_inner` calls `pkg.readers.schema_of`".to_owned(), 29),
            ("`schema_of` calls `pkg.graph.Graph.schema`".to_owned(), 16),
        ]
    );
    assert_eq!(by_name("lru_wrapper")["target"], "pkg.cached.load_table");
    assert_eq!(by_name("lru_wrapper")["rule_code"], "MUT114");
    assert_eq!(
        by_name("lru_wrapper")["message"],
        "`lru_wrapper` mutates the shared object returned by cached function `pkg.cached.load_table` through `pkg.cached.table`"
    );
    assert_eq!(
        by_name("external_wrapper")["target"],
        "settings_lib.get_settings"
    );
    assert_eq!(
        by_name("attribute_wrapper")["target"],
        "pkg.readers.Model.fields"
    );
    // `@lru_cache` wrappers need no configuration; `MUT202` reports nothing without a declared
    // producer.
    p.write("pyproject.toml", "[project]\nname = \"t\"\n");
    let unconfigured = diagnostics(&p, &["--select", "MUT202,MUT114"]);
    assert_eq!(
        found(&unconfigured),
        vec![
            d(c, 14, "MUT114", "lru_wrapper"),
            d(c, 27, "MUT114", "Holder.via_property"),
            d(c, 49, "MUT114", "unpacked_wrapper"),
        ],
        "{unconfigured:#?}"
    );
}

/// Comprehensions collecting what wrappers of a shared value return.
const COLLECTING: &str = r#"from __future__ import annotations

from collections.abc import Sequence
from functools import lru_cache

from pkg.graph import Graph, Schema


def load(graph: Graph, raw: object) -> Schema:
    return graph.schema(raw)


def own_or_loaded(graph: Graph, item: Schema, raw: object) -> Schema:
    if raw is not None:
        return load(graph, raw)
    return item


def collect(graph: Graph, items: list[Schema], raw: object) -> list[Schema]:
    return [own_or_loaded(graph, item, raw) for item in items]


def collect_set(graph: Graph, items: Sequence[Schema], raw: object) -> set[Schema]:
    return {load(graph, raw) for _ in items}


def changed(schema: Schema) -> Schema:
    schema.items.append(1)
    return schema


def change_each(graph: Graph, raws: Sequence[object]) -> list[Schema]:
    return [changed(load(graph, raw)) for raw in raws]


@lru_cache
def table(name: str) -> dict[str, int]:
    return {name: 1}


def filled(acc: list[int]) -> dict[str, int]:
    acc.append(1)
    return table("a")


def collect_tables(accs: list[list[int]]) -> list[dict[str, int]]:
    return [filled(acc) for acc in accs]
"#;

#[test]
fn mut202_comprehension_collecting_wrapper_results_mutates_nothing() {
    let p = Project::new("mut202_collecting");
    p.write("pyproject.toml", RO_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/graph.py", GRAPH)
        .write("pkg/collecting.py", COLLECTING);
    // A comprehension appends its element, the result of the element's own call: no argument of
    // that call. Handing the shared value to a helper that changes it is a mutation. Python 3.11
    // runs a comprehension in a frame of its own, 3.12 inlines it (PEP 709).
    for (version, qualname) in [
        ("py311", "change_each.<locals>.<listcomp>"),
        ("py312", "change_each"),
    ] {
        let diags = diagnostics(&p, &["--select", "MUT202", "--target-version", version]);
        assert_eq!(
            found(&diags),
            vec![d("pkg/collecting.py", 33, "MUT202", qualname)],
            "{version}: {diags:#?}"
        );
    }
}

// ============================================================================== MUT109

const INV_CONFIG: &str = r#"[project]
name = "t"

[[tool.heki.invalidation]]
name = "graph-index"
targets = ["DataType.data_types", "DataModel.fields"]
invalidators = ["GenerationStore.note_changed_models", "GenerationStore.refresh_now", "GenerationStore.notify", "settings_lib.flush"]
exempt-writers = ["pkg.generation.*"]
message = "tell the store which models changed"
"#;

const MODEL: &str = r#"from __future__ import annotations


class DataType:
    def __init__(self) -> None:
        self.data_types: list[DataType] = []


class DataModel:
    def __init__(self, fields: list[str] | None = None) -> None:
        self.fields: list[str] = []
        if fields:
            self._take(fields)

    def _take(self, fields: list[str]) -> None:
        self.fields.extend(fields)


def make_type() -> DataType:
    return DataType()


def _relink(node: DataType) -> None:
    node.data_types = []


def retarget(node: DataType) -> None:
    node.data_types = []
"#;
const GENERATION: &str = r#"from __future__ import annotations

from contextlib import contextmanager

from pkg.model import DataModel, DataType, _relink, retarget


class GenerationStore:
    def note_changed_models(self, *models: DataModel) -> None:
        self.changed = models

    def refresh_now(self) -> None:
        self.changed = ()

    async def notify(self, model: DataModel) -> None:
        self.changed = (model,)

    def set_types(self, node: DataType, items: list[DataType]) -> None:
        node.data_types = items

    def lookup(self) -> DataType:
        return DataType()

    def relink(self, node: DataType) -> None:
        _relink(node)
        retarget(node)

    @contextmanager
    def defer_refresh(self):
        yield
        self.refresh_now()
"#;
const PASSES: &str = r#"from __future__ import annotations

import copy

from settings_lib import flush

from pkg.generation import GenerationStore
from pkg.model import DataModel, DataType, make_type

ROOT = DataModel()


def narrow(store: GenerationStore, owner: DataModel, node: DataType, item: DataType) -> None:
    node.data_types = [item]
    if owner.fields:
        store.note_changed_models(owner)


def narrow_ok(store: GenerationStore, owner: DataModel, node: DataType, item: DataType) -> None:
    node.data_types = [item]
    store.note_changed_models(owner)


def branches_ok(store: GenerationStore, owner: DataModel, node: DataType, item: DataType) -> None:
    types = node.data_types
    types.append(item)
    if owner.fields:
        store.note_changed_models(owner)
    else:
        store.refresh_now()


def before_write(store: GenerationStore, node: DataType, item: DataType) -> None:
    store.refresh_now()
    node.data_types = [item]


def _announce(store: GenerationStore, owner: DataModel) -> None:
    store.note_changed_models(owner)


def through_helper_ok(store: GenerationStore, owner: DataModel) -> None:
    owner.fields.append("id")
    _announce(store, owner)


def _announce_if(store: GenerationStore, owner: DataModel, changed: bool) -> None:
    if changed:
        store.note_changed_models(owner)


def through_conditional_helper(store: GenerationStore, owner: DataModel) -> None:
    owner.fields.append("id")
    _announce_if(store, owner, True)


def _write(node: DataType, item: DataType) -> None:
    node.data_types = [item]


def _write_twice(node: DataType, item: DataType) -> None:
    _write(node, item)


def helper_write(store: GenerationStore, node: DataType, item: DataType) -> None:
    _write(node, item)


def helper_write_ok(store: GenerationStore, node: DataType, item: DataType) -> None:
    _write(node, item)
    store.refresh_now()


def two_level_helper(node: DataType, item: DataType) -> None:
    _write_twice(node, item)


def raises_ok(store: GenerationStore, node: DataType, item: DataType) -> None:
    node.data_types = [item]
    if not item.data_types:
        raise ValueError("empty")
    store.refresh_now()


def store_op_ok(store: GenerationStore, node: DataType) -> None:
    store.set_types(node, [])


def built_ok(store: GenerationStore, node: DataType) -> DataType:
    fresh = DataType()
    fresh.data_types = [node]
    copied = copy.copy(node)
    copied.data_types = []
    made = make_type()
    made.data_types.append(node)
    found = store.lookup()
    found.data_types = []
    _write(fresh, node)
    return fresh


def public_writer(node: DataType) -> None:
    node.data_types = []


def bulk_caller_ok(store: GenerationStore, node: DataType) -> None:
    public_writer(node)
    store.refresh_now()


def unannounced_writer(node: DataType) -> None:
    node.data_types.clear()


def announced_caller(store: GenerationStore, node: DataType) -> None:
    unannounced_writer(node)
    store.refresh_now()


def silent_caller(node: DataType) -> None:
    unannounced_writer(node)


def distant_writer(node: DataType) -> None:
    node.data_types.pop()


def _middle(node: DataType) -> None:
    distant_writer(node)


def distant_caller_ok(store: GenerationStore, node: DataType) -> None:
    _middle(node)
    store.note_changed_models()


def loop_ok(store: GenerationStore, node: DataType, items: list[DataType]) -> None:
    for item in items:
        node.data_types.append(item)
    store.refresh_now()


def loop_last_write(store: GenerationStore, node: DataType, items: list[DataType]) -> None:
    for item in items:
        store.refresh_now()
        node.data_types.append(item)


def until_full_ok(store: GenerationStore, node: DataType, item: DataType) -> None:
    while True:
        node.data_types.append(item)
        if len(node.data_types) > 3:
            break
    store.refresh_now()


def handled_ok(store: GenerationStore, node: DataType, item: DataType) -> None:
    try:
        node.data_types = [item]
        store.refresh_now()
    except KeyError:
        return


def deferred(store: GenerationStore, node: DataType) -> None:
    with store.defer_refresh():
        node.data_types = []


async def awaited_ok(store: GenerationStore, owner: DataModel) -> None:
    owner.fields.clear()
    await store.notify(owner)


async def not_awaited(store: GenerationStore, owner: DataModel) -> None:
    owner.fields.clear()
    store.notify(owner)


def external_invalidator_ok(node: DataType) -> None:
    node.data_types = []
    flush()


def reset_root() -> None:
    ROOT.fields.clear()


def _orphan(node: DataType) -> None:
    node.data_types = []


class Typed(DataType):
    def __init__(self, item: DataType) -> None:
        super().__init__()
        self.data_types = [item]

    def retype(self, item: DataType) -> None:
        self.data_types = [item]


def flagged_ok(store: GenerationStore, node: DataType, items: list[DataType]) -> None:
    changed = False
    for item in items:
        if item.data_types:
            node.data_types.append(item)
            changed = True
    if changed:
        store.refresh_now()


def flag_not_set(store: GenerationStore, node: DataType, items: list[DataType]) -> None:
    changed = False
    for item in items:
        node.data_types.append(item)
        if item.data_types:
            changed = True
    if changed:
        store.refresh_now()


def negated_flag_ok(store: GenerationStore, node: DataType, item: DataType) -> None:
    done = False
    node.data_types = [item]
    done = True
    if not done:
        return
    store.refresh_now()


def dead_branch(store: GenerationStore, node: DataType) -> None:
    node.data_types = []
    if False:
        store.refresh_now()


def phi_write(a: DataType, b: DataType, c: bool) -> None:
    node = a if c else b
    node.data_types = []


def compare_flag_ok(store: GenerationStore, node: DataType, items: list[DataType]) -> None:
    changed = False
    for item in items:
        node.data_types.append(item)
        changed = True
    if changed is True:
        store.refresh_now()


async def _aput(node: DataType) -> None:
    node.data_types = []


def schedule_put_ok(node: DataType) -> None:
    _aput(node)


async def awrite(node: DataType) -> None:
    node.data_types = []


def schedule(store: GenerationStore, node: DataType) -> None:
    awrite(node)
    store.refresh_now()


def _ping(node: DataType, n: int) -> None:
    node.data_types = []
    if n:
        _pong(node, n - 1)


def _pong(node: DataType, n: int) -> None:
    _ping(node, n)


def recurse(node: DataType) -> None:
    _ping(node, 2)


def _reset_twice(node: DataType, item: DataType) -> None:
    node.data_types = []
    node.data_types.append(item)


def reset_twice(node: DataType, item: DataType) -> None:
    _reset_twice(node, item)


def by_key(table: dict[str, DataType]) -> None:
    node: DataType = table["a"]
    node.data_types = []


def many_flags(store: GenerationStore, node: DataType, c0: bool, c1: bool, c2: bool, c3: bool, c4: bool, c5: bool, c6: bool, c7: bool, c8: bool, c9: bool, c10: bool, c11: bool, c12: bool) -> None:
    node.data_types = []
    f0 = False
    if c0:
        f0 = True
    f1 = False
    if c1:
        f1 = True
    f2 = False
    if c2:
        f2 = True
    f3 = False
    if c3:
        f3 = True
    f4 = False
    if c4:
        f4 = True
    f5 = False
    if c5:
        f5 = True
    f6 = False
    if c6:
        f6 = True
    f7 = False
    if c7:
        f7 = True
    f8 = False
    if c8:
        f8 = True
    f9 = False
    if c9:
        f9 = True
    f10 = False
    if c10:
        f10 = True
    f11 = False
    if c11:
        f11 = True
    f12 = False
    if c12:
        f12 = True
    if f0 and all([f1, f2, f3, f4, f5, f6, f7, f8, f9, f10, f11, f12]):
        store.refresh_now()


def _fill_kids(node: DataType, items: list[DataType]):
    for item in items:
        node.data_types.append(item)
        yield item


def iterate_fill(node: DataType, items: list[DataType]) -> None:
    for _ in _fill_kids(node, items):
        pass


def iterate_fill_ok(store: GenerationStore, node: DataType, items: list[DataType]) -> None:
    for _ in _fill_kids(node, items):
        pass
    store.refresh_now()


def _drain(values) -> None:
    for _ in values:
        pass


def drain_fill(node: DataType, items: list[DataType]) -> None:
    _drain(_fill_kids(node, items))


def announce_before_iterating(store: GenerationStore, node: DataType, items: list[DataType]) -> None:
    pending = _fill_kids(node, items)
    store.refresh_now()
    for _ in pending:
        pass


def fill_kids(node: DataType, items: list[DataType]):
    for item in items:
        node.data_types.append(item)
        yield item


def consume_fill_ok(store: GenerationStore, node: DataType, items: list[DataType]) -> None:
    for _ in fill_kids(node, items):
        pass
    store.refresh_now()
"#;

fn inv_project(name: &str) -> Project {
    let p = Project::new(name);
    p.write("pyproject.toml", INV_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/model.py", MODEL)
        .write("pkg/generation.py", GENERATION)
        .write("pkg/passes.py", PASSES);
    p
}

#[test]
fn mut109_requires_an_invalidator_on_every_path_after_a_write() {
    let p = inv_project("mut109_paths");
    let diags = diagnostics(&p, &["--select", "MUT109"]);
    let s = "pkg/passes.py";
    // Not reported: an invalidator after the write (`narrow_ok`), one in each branch, a helper
    // that always announces, a helper write the caller announces, a raising path, a store
    // operation (exempt writer), objects the function builds or gets from a call, a public
    // writer every caller announces after the call (directly or through a private helper),
    // writes in a loop announced after it, a `while True:` loop, an exception handler, an
    // awaited invalidator, an external invalidator, a private helper without callers,
    // initializers writing their own `self`, and a flag set after every write that guards the
    // invalidator (`flagged_ok`, `negated_flag_ok`, `compare_flag_ok`), a private `async` helper
    // that is never awaited, and a public writer the exempt store calls (`retarget`). A flag some
    // writes leave unset, an invalidator behind `if False:`, the caller of an `async` writer that
    // does not await it (`awrite`), mutually recursive private writers (`recurse`), two writes of
    // one helper (one report at the call), an object reached through an item of a parameter, and
    // a function with more flag combinations than the path search follows are reported. A
    // private generator's write runs where it is iterated: reported at the iteration (also
    // after an invalidator called between the generator's call and its iteration) or at the
    // call of a helper that iterates it, and not when the change is announced after the loop
    // (`iterate_fill_ok`); a public generator whose caller announces after iterating it is not
    // reported (`consume_fill_ok`).
    assert_eq!(
        found(&diags),
        vec![
            d(s, 14, "MUT109", "narrow"),
            d(s, 35, "MUT109", "before_write"),
            d(s, 53, "MUT109", "through_conditional_helper"),
            d(s, 66, "MUT109", "helper_write"),
            d(s, 75, "MUT109", "two_level_helper"),
            d(s, 112, "MUT109", "unannounced_writer"),
            d(s, 146, "MUT109", "loop_last_write"),
            d(s, 167, "MUT109", "deferred"),
            d(s, 176, "MUT109", "not_awaited"),
            d(s, 186, "MUT109", "reset_root"),
            d(s, 199, "MUT109", "Typed.retype"),
            d(s, 215, "MUT109", "flag_not_set"),
            d(s, 232, "MUT109", "dead_branch"),
            d(s, 239, "MUT109", "phi_write"),
            d(s, 260, "MUT109", "awrite"),
            d(s, 279, "MUT109", "recurse"),
            d(s, 288, "MUT109", "reset_twice"),
            d(s, 293, "MUT109", "by_key"),
            d(s, 297, "MUT109", "many_flags"),
            d(s, 348, "MUT109", "iterate_fill"),
            d(s, 364, "MUT109", "drain_fill"),
            d(s, 370, "MUT109", "announce_before_iterating"),
        ],
        "{diags:#?}"
    );
    let by_name = |q: &str| diags.iter().find(|x| x["qualname"] == q).unwrap();
    let narrow = by_name("narrow");
    assert_eq!(narrow["target"], "graph-index:DataType.data_types");
    assert_eq!(
        narrow["message"],
        "`narrow` writes `DataType.data_types` (by attribute) and returns on some path without calling an invalidator of `graph-index`"
    );
    assert_eq!(narrow["help"], "tell the store which models changed");
    // A helper write is reported at the call, with the hops down to the write.
    let two = by_name("two_level_helper");
    assert!(
        two["message"]
            .as_str()
            .unwrap()
            .contains("(by attribute) via `pkg.passes._write`"),
        "{two}"
    );
    assert_eq!(
        related(two),
        vec![
            (
                "`two_level_helper` calls `pkg.passes._write_twice`".to_owned(),
                75
            ),
            ("`_write_twice` calls `pkg.passes._write`".to_owned(), 62),
        ]
    );
    // One report at the call stands for both writes of the helper (two violation sites);
    // every other report stands for one.
    let sites: Vec<(&str, u64)> = diags
        .iter()
        .filter(|x| x.get("sites").is_some())
        .map(|x| {
            (
                x["qualname"].as_str().unwrap(),
                x["sites"].as_u64().unwrap(),
            )
        })
        .collect();
    assert_eq!(sites, vec![("reset_twice", 2)], "{diags:#?}");
    // A generator handed to a helper that iterates it: the chain goes through the helper.
    assert_eq!(
        related(by_name("drain_fill")),
        vec![
            ("`drain_fill` calls `pkg.passes._drain`".to_owned(), 364),
            ("`_drain` calls `pkg.passes._fill_kids`".to_owned(), 364),
        ]
    );
    // Without a contract the rule reports nothing.
    p.write("pyproject.toml", "[project]\nname = \"t\"\n");
    assert!(diagnostics(&p, &["--select", "MUT109"]).is_empty());
}

const BINDING_CONFIG: &str = r#"[[tool.heki.invalidation]]
name = "model"
targets = ["Model.items", "Model.x"]
invalidators = ["notify"]
"#;
const INITIALIZERS: &str = r#"class Model:
    def __init__(self) -> None:
        self.x = 0
        _reset_x(self)


def notify(m: Model) -> None:
    pass


def _reset_x(m: Model) -> None:
    m.x = 0


class Holder:
    model: Model

    def __init__(self, model: Model) -> None:
        self.model = model
        self.model.x = 1


class Keeper:
    model: Model

    def __init__(self, model: Model) -> None:
        self.model = model
        _touch(self)


def _touch(k: Keeper) -> None:
    k.model.x = 2


class Filled(Model):
    def __init__(self, xs: list[int]) -> None:
        super().__init__()
        self.items: list[int] = []
        any(self.items.append(x) for x in xs)

    def refill(self, xs: list[int]) -> None:
        any(self.items.append(x) for x in xs)
"#;

#[test]
fn mut109_initializers_build_only_their_own_self() {
    let p = Project::new("mut109_initializers");
    p.write("pyproject.toml", BINDING_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/models.py", INITIALIZERS);
    let diags = diagnostics(&p, &["--select", "MUT109"]);
    let m = "pkg/models.py";
    // An initializer builds its own `self`, also through a helper it hands `self` to and a
    // generator expression using it; an object reached through `self`'s attributes existed
    // before (`Holder`, and `Keeper` through a helper), and so does the `self` of another method.
    assert_eq!(
        found(&diags),
        vec![
            d(m, 20, "MUT109", "Holder.__init__"),
            d(m, 28, "MUT109", "Keeper.__init__"),
            d(m, 42, "MUT109", "Filled.refill"),
        ],
        "{diags:#?}"
    );
    assert!(
        diags[1]["message"]
            .as_str()
            .unwrap()
            .contains("(by attribute) via `pkg.models._touch`"),
        "{diags:#?}"
    );
}

const BINDING: &str = r#"from __future__ import annotations


class Model:
    def __init__(self, other: Model | None = None) -> None:
        self.items: list[int] = []
        self.x = 0
        _fill(self.items)
        _set_x(self)
        if other is not None:
            _both(self, other.items)


def notify(m: Model) -> None:
    pass


def _fill(xs) -> None:
    xs.append(1)


def _set_x(o) -> None:
    o.x = 1


def _both(m: Model, xs) -> None:
    m.items = []
    xs.append(1)


def fill(m: Model) -> None:
    _fill(m.items)


def fill_ok(m: Model) -> None:
    _fill(m.items)
    notify(m)


def set_x(m: Model) -> None:
    _set_x(m)


def set_x_ok(m: Model) -> None:
    _set_x(m)
    notify(m)


import typing


class Positioned(typing.Protocol):
    x: int


def set_positioned(p: Positioned) -> None:
    _set_x(p)
"#;

#[test]
fn mut109_follows_the_fields_and_instances_bound_to_untyped_helper_arguments() {
    let p = Project::new("mut109_binding");
    p.write("pyproject.toml", BINDING_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/models.py", BINDING);
    let diags = diagnostics(&p, &["--select", "MUT109"]);
    let m = "pkg/models.py";
    // A helper changing an argument it does not type writes the field or the instance the
    // caller binds to it (`fill`, `set_x`, `set_positioned`, and `other.items` in
    // `Model.__init__`); the initializer's own fields are its construction.
    assert_eq!(
        found(&diags),
        vec![
            d(m, 11, "MUT109", "Model.__init__"),
            d(m, 32, "MUT109", "fill"),
            d(m, 41, "MUT109", "set_x"),
            // An object typed with a protocol `Model` implements.
            d(m, 57, "MUT109", "set_positioned"),
        ],
        "{diags:#?}"
    );
    assert!(
        diags[1]["message"]
            .as_str()
            .unwrap()
            .starts_with("`fill` writes `Model.items` (through a helper) via `pkg.models._fill`"),
        "{diags:#?}"
    );
}

const PROTOCOL_BINDING: &str = r#"import typing


class Positioned(typing.Protocol):
    x: int


class Model:
    def __init__(self) -> None:
        self.x = 0


class Point:
    def __init__(self) -> None:
        self.x = 0


def notify(o) -> None:
    pass


def _set_x(o) -> None:
    o.x = 1


def set_positioned(p: Positioned) -> None:
    _set_x(p)
"#;

/// A helper writing an attribute of an object typed with a protocol writes it on the protocol
/// and on each implementation: each write is matched against the targets of its own class
/// (`Model.x` once, not once per class the object may be), and a target on the protocol covers
/// the write too.
#[test]
fn mut109_ties_a_helper_write_through_a_protocol_to_each_class_once() {
    let p = Project::new("mut109_protocol_binding");
    p.write(
        "pyproject.toml",
        "[[tool.heki.invalidation]]\nname = \"model\"\ntargets = [\"Model.x\"]\ninvalidators = [\"notify\"]\n\n[[tool.heki.invalidation]]\nname = \"positioned\"\ntargets = [\"Positioned.x\"]\ninvalidators = [\"notify\"]\n",
    )
    .write("pkg/__init__.py", "")
    .write("pkg/models.py", PROTOCOL_BINDING);
    let diags = diagnostics(&p, &["--select", "MUT109"]);
    let mut messages: Vec<&str> = diags
        .iter()
        .map(|d| d["message"].as_str().unwrap())
        .collect();
    messages.sort_unstable();
    assert_eq!(
        messages,
        vec![
            "`set_positioned` writes `Model.x` (through a helper) via `pkg.models._set_x` and returns on some path without calling an invalidator of `model`",
            "`set_positioned` writes `Positioned.x` (through a helper) via `pkg.models._set_x` and returns on some path without calling an invalidator of `positioned`",
        ],
        "{diags:#?}"
    );
}

#[test]
fn mut109_configuration_errors() {
    let p = inv_project("mut109_config");
    let fails = |config: &str, needle: &str| {
        p.write(
            "pyproject.toml",
            &format!("[project]\nname = \"t\"\n\n{config}"),
        );
        let (code, _, stderr) = p.check(&["--select", "MUT109"]);
        assert_eq!(code, 2, "{stderr}");
        assert!(stderr.contains(needle), "{stderr}");
    };
    fails(
        "[[tool.heki.invalidation]]\nname = \"x\"\ntargets = [\"fields\"]\n",
        "invalid invalidation target `fields` (expected `Class.attr`)",
    );
    fails(
        "[[tool.heki.invalidation]]\nname = \"x\"\ntargets = [\"M.[a\"]\n",
        "invalid glob `[a` in invalidation.targets",
    );
    fails(
        "[[tool.heki.invalidation]]\nname = \"x\"\ninvalidators = [\"a.[b\"]\n",
        "invalid glob `a.[b` in invalidation.invalidators",
    );
    fails(
        "[[tool.heki.invalidation]]\nname = \"x\"\nexempt-writers = [\"a.[b\"]\n",
        "invalid glob `a.[b` in invalidation.exempt-writers",
    );
    let many: String = (0..65)
        .map(|i| format!("[[tool.heki.invalidation]]\nname = \"c{i}\"\n"))
        .collect();
    fails(
        &many,
        "65 `invalidation` contracts declared; at most 64 are supported",
    );
}

// ======================================================================= language server

const LSP_CONFIG: &str = r#"[[invalidation]]
name = "index"
targets = ["Node.kids"]
invalidators = ["Index.refresh"]
"#;
const NODES: &str = "class Node:\n    def __init__(self):\n        self.kids = []\n\n\nclass Index:\n    def refresh(self):\n        self.stale = False\n";
const ANNOUNCE: &str =
    "from pkg.nodes import Index\n\n\ndef announce(index: Index):\n    index.refresh()\n";
const ANNOUNCE_SILENT: &str =
    "from pkg.nodes import Index\n\n\ndef announce(index: Index):\n    return index\n";
const ADD: &str = "from pkg.announce import announce\nfrom pkg.nodes import Index, Node\n\n\ndef _put(node: Node, kid):\n    node.kids.append(kid)\n\n\ndef add(index: Index, node: Node, kid):\n    _put(node, kid)\n    announce(index)\n";

/// Edit, write, and save `rel` (the way an editor saves a changed document).
fn save(lsp: &mut Lsp, p: &Project, rel: &str, version: i32, text: &str) {
    let u = uri(&p.dir.join(rel));
    lsp.change(&u, version, text);
    p.write(rel, text);
    lsp.notify(
        "textDocument/didSave",
        json!({"textDocument": {"uri": u}, "text": text}),
    );
}

#[test]
fn lsp_mut109_follows_a_helper_that_stops_announcing() {
    let p = Project::new("lsp_mut109");
    p.write("heki.toml", LSP_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/nodes.py", NODES)
        .write("pkg/announce.py", ANNOUNCE)
        .write("pkg/add.py", ADD);
    let add = uri(&p.dir.join("pkg/add.py"));
    let mut lsp = Lsp::start(&p.dir, &["server", "--stdio"]);
    lsp.request(
        "initialize",
        json!({"processId": null, "rootUri": uri(&p.dir), "capabilities": {}}),
    );
    lsp.notify("initialized", json!({}));
    lsp.ready();
    lsp.open(&add, ADD);
    lsp.diagnostics(&add, <[Value]>::is_empty);
    // The helper in another file stops calling the invalidator: the write its caller makes
    // through a private helper is reported at the call; calling it again clears the report.
    lsp.open(&uri(&p.dir.join("pkg/announce.py")), ANNOUNCE);
    save(&mut lsp, &p, "pkg/announce.py", 2, ANNOUNCE_SILENT);
    let diags = lsp.diagnostics(&add, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["MUT109"]);
    assert_eq!(
        diags[0]["range"]["start"],
        json!({"line": 9, "character": 4})
    );
    assert!(
        diags[0]["message"]
            .as_str()
            .unwrap()
            .contains("via `pkg.add._put`"),
        "{diags:#?}"
    );
    save(&mut lsp, &p, "pkg/announce.py", 3, ANNOUNCE);
    lsp.diagnostics(&add, <[Value]>::is_empty);
    lsp.stop(true);
}

const GRAFT: &str =
    "from pkg.nodes import Node\n\n\ndef graft(node: Node, kid):\n    node.kids.append(kid)\n";
const RELAY: &str = "from pkg.graft import graft\nfrom pkg.nodes import Node\n\n\ndef forward(node: Node, kid):\n    graft(node, kid)\n";
const PLANTER: &str = "from pkg.nodes import Index, Node\nfrom pkg.relay import forward\n\n\ndef plant(index: Index, node: Node, kid):\n    forward(node, kid)\n    index.refresh()\n";
const PLANTER_SILENT: &str = "from pkg.nodes import Index, Node\nfrom pkg.relay import forward\n\n\ndef plant(index: Index, node: Node, kid):\n    forward(node, kid)\n";

#[test]
fn lsp_mut109_follows_a_distant_caller_that_stops_announcing() {
    // Only an `invalidation` contract is configured: the walk over the callers above a writer
    // alone makes the writer's file depend on every caller above it.
    let p = Project::new("lsp_mut109_distant");
    p.write("heki.toml", LSP_CONFIG)
        .write("pkg/__init__.py", "")
        .write("pkg/nodes.py", NODES)
        .write("pkg/graft.py", GRAFT)
        .write("pkg/relay.py", RELAY)
        .write("pkg/planter.py", PLANTER);
    let graft = uri(&p.dir.join("pkg/graft.py"));
    let mut lsp = Lsp::start(&p.dir, &["server", "--stdio"]);
    lsp.request(
        "initialize",
        json!({"processId": null, "rootUri": uri(&p.dir), "capabilities": {}}),
    );
    lsp.notify("initialized", json!({}));
    lsp.ready();
    lsp.open(&graft, GRAFT);
    lsp.diagnostics(&graft, <[Value]>::is_empty);
    // The function two calls above the public writer stops announcing the change: the writer
    // is reported in its own file; announcing again clears the report.
    lsp.open(&uri(&p.dir.join("pkg/planter.py")), PLANTER);
    save(&mut lsp, &p, "pkg/planter.py", 2, PLANTER_SILENT);
    let diags = lsp.diagnostics(&graft, |d| !d.is_empty());
    assert_eq!(codes(&diags), vec!["MUT109"]);
    assert_eq!(
        diags[0]["range"]["start"],
        json!({"line": 4, "character": 4})
    );
    save(&mut lsp, &p, "pkg/planter.py", 3, PLANTER);
    lsp.diagnostics(&graft, <[Value]>::is_empty);
    lsp.stop(true);
}
