//! Regressions from the hand review of the open-source corpus before the first preview: every
//! false-positive shape and every piece of advice that could not be followed, each with the
//! true positive next to it (end to end, through the real binary).

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

/// `(line, qualname, message)` of every diagnostic of `rule`.
fn reports(p: &Project, rule: &str) -> Vec<(u64, String, String)> {
    reports_with(p, rule, "message")
}

/// `(line, qualname, help)` of every diagnostic of `rule`.
fn helps(p: &Project, rule: &str) -> Vec<(u64, String, String)> {
    reports_with(p, rule, "help")
}

/// `rule` is a code with its tier ([`common::with_tier`]): `EFF101` for the exact reports of the
/// rule, `EFF101 [heuristic]` for its heuristic ones.
fn reports_with(p: &Project, rule: &str, field: &str) -> Vec<(u64, String, String)> {
    let code = common::code_of(rule);
    diagnostics(p, &["--select", code])
        .iter()
        .filter(|d| {
            assert_eq!(d["rule_code"], code, "{d:#}");
            common::tiered_code(d) == rule
        })
        .map(|d| {
            (
                d["line"].as_u64().unwrap(),
                d["qualname"].as_str().unwrap().to_owned(),
                d[field].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

/// `(line, qualname)` of every diagnostic of `rule`.
fn lines(p: &Project, rule: &str) -> Vec<(u64, String)> {
    reports(p, rule)
        .into_iter()
        .map(|(line, qualname, _)| (line, qualname))
        .collect()
}

fn at(line: u64, qualname: &str) -> (u64, String) {
    (line, qualname.to_owned())
}

// ======================================================================= OPT113: all pairs

/// R02 (pydantic `tests/test_main.py:2833`, seaborn `tests/test_axisgrid.py:973`) and R03
/// (pydantic `tests/test_main.py:2909`): loops that need every pair are no joins. The pairs
/// whose keys differ do work of their own (an `else`, an `assert` that raises, statements a
/// matching pair skips with `break`); an index of the inner collection would skip them.
const ALL_PAIRS: &str = r#"def check_pairs(models: list) -> None:
    for i, a in enumerate(models):
        for j, b in enumerate(models):
            if i == j:
                assert a == b
            else:
                assert a != b


def check_all_equal(models: list, wrap) -> None:
    for m1 in models:
        for m2 in models:
            assert m1 == m2
            assert wrap(m1) == wrap(m2)


def only_assert(models: list) -> None:
    for m1 in models:
        for m2 in models:
            assert m1 == m2


def grid(x_vars, y_vars, axes):
    for j, x_var in enumerate(x_vars):
        for i, y_var in enumerate(y_vars):
            ax = axes[i, j]
            if x_var == y_var:
                print(ax.bounds)
            else:
                x, y = ax.offsets()
                print(x, y)


def logs_misses(orders, customers):
    for order in orders:
        for customer in customers:
            if order.customer_id == customer.id:
                order.customer = customer
                break
            print(customer)


def matches_only(models: list) -> None:
    for i, a in enumerate(models):
        for j, b in enumerate(models):
            if i == j:
                print(a, b)


def collects_matches(models: list) -> list:
    out = []
    for m1 in models:
        for m2 in models:
            if m1 == m2:
                out.append(m2)
    return out


def else_continues(A, B):
    for a in A:
        for b in B:
            if a.k == b.k:
                print(a)
            else:
                continue


def then_runs_more(A, B):
    for a in A:
        for b in B:
            if a.k == b.k and b.ok:
                print(a)
            print(b)


def conjunction_else(A, B):
    for a in A:
        for b in B:
            if a.k == b.k and b.ok:
                print(a)
            else:
                print(b)


def attach(orders, customers):
    for order in orders:
        for customer in customers:
            if order.customer_id == customer.id:
                order.customer = customer
                break
"#;

#[test]
fn opt113_does_not_take_all_pairs_loops_for_joins() {
    let p = Project::new("regr_opt113_all_pairs");
    p.write("pairs.py", ALL_PAIRS);
    // Reported: joins whose mismatches do nothing (also an `else: continue`, statements every
    // pair runs after the `if`, an `and` with nothing on its false side). Not reported: an
    // `else` doing work, `assert`s (with and without statements after them), statements only a
    // mismatch runs (the match `break`s), and an `and` whose `else` does work.
    assert_eq!(
        lines(&p, "OPT113"),
        vec![
            at(46, "matches_only"),
            at(54, "collects_matches"),
            at(62, "else_continues"),
            at(71, "then_runs_more"),
            at(88, "attach"),
        ]
    );
}

/// The redis-py review item (`tests/test_search.py:5180`): a `set` of `dict`s cannot be built.
/// When the keys are known to be unhashable, the advice indexes a hashable form of them; when
/// nothing is known about them (unannotated parameters), the plain advice stays.
const KEYS: &str = r#"def dict_rows(A):
    rows = [{"id": 1}, {"id": 2}, {"id": 3}, {"id": 4}, {"id": 5}, {"id": 6}, {"id": 7}, {"id": 8}, {"id": 9}, {"id": 10}, {"id": 11}, {"id": 12}, {"id": 13}, {"id": 14}, {"id": 15}, {"id": 16}, {"id": 17}]
    for a in A:
        for r in rows:
            if a == r:
                print(a)


def typed(A, B):
    for a in A:
        for b in B:
            tags: list[str] = b.tags
            if a.tags == tags:
                print(a)


def unknown(actual, expected):
    for expected_item in expected:
        for actual_item in actual:
            if actual_item == expected_item:
                break


def scalar_rows(A):
    rows = [1, 2, 3]
    for a in A:
        for r in rows + rows:
            if a == r:
                print(a)
"#;

#[test]
fn opt113_does_not_advise_an_index_of_unhashable_keys() {
    let p = Project::new("regr_opt113_keys");
    p.write("keys.py", KEYS);
    let found = reports(&p, "OPT113");
    let messages: Vec<&str> = found.iter().map(|(_, _, m)| m.as_str()).collect();
    assert_eq!(
        messages,
        vec![
            "nested loops join `A` and `rows` on `a == r`",
            "nested loops join `A` and `B` on `a.tags == tags`",
            "nested loops join `expected` and `actual` on `actual_item == expected_item`",
            "nested loops join `A` and `rows + rows` on `a == r`",
        ],
        "{found:#?}"
    );
    let found = helps(&p, "OPT113");
    let advice: Vec<&str> = found.iter().map(|(_, _, h)| h.as_str()).collect();
    assert_eq!(
        advice,
        vec![
            "`r` is an unhashable `dict`, so index `rows` by a hashable form of it (a `tuple` or `frozenset` of its items) before the outer loop",
            "`tags` is an unhashable `list`, so index `B` by a hashable form of it (a `tuple` or `frozenset` of its items) before the outer loop",
            "if the keys are hashable, build a `set` of `actual` before the outer loop",
            "if the keys are hashable, group the elements of `rows + rows` by value in a `dict` (value → list of equal elements) before the outer loop, then read each outer key with `.get(key, [])` so that a key with no element keeps its empty list",
        ],
        "{found:#?}"
    );
}

// ================================================================ MUT101: held by reference

/// R13 (datamodel-code-generator `parser/jsonschema.py:9270` / `:9286`): a cached value that
/// holds an attribute's object itself sees the changes made to it in place; only rebinding the
/// attribute leaves the cache behind. A copy of the attribute, a value computed from it, a
/// `self` method or property that reads it, and `self` handed to other code all can go stale.
const HELD: &str = r#"from functools import cached_property


def summarize(obj):
    return len(obj.items)


class Parser:
    def __init__(self) -> None:
        self._empty: set[str] = set()
        self._patterns: dict[str, str] = {}
        self._engines = ()
        self.items: list[str] = []
        self.other: list[str] = []

    @cached_property
    def context(self) -> dict[str, object]:
        ctx: dict[str, object] = {"empty": self._empty}
        if engines := self._engines:
            ctx["patterns"] = (engines, self._patterns)
        return ctx

    @cached_property
    def view(self) -> list[str]:
        return self.items

    @cached_property
    def collected(self) -> list[object]:
        out = []
        out.append(self.items)
        return out

    @cached_property
    def either(self) -> list[str]:
        return self.items if self.other else []

    def report(self) -> None:
        for where in sorted(self._empty):
            print(where)
        self._empty.clear()
        self._patterns.clear()
        self.items.append("x")
        self.items[0] = "y"

    def reset(self) -> None:
        self._empty = set()
        self.items = []


class Copies:
    def __init__(self) -> None:
        self.items: list[str] = []

    def _count(self) -> int:
        return len(self.items)

    @property
    def size(self) -> int:
        return len(self.items)

    @cached_property
    def unpacked(self) -> tuple[str, ...]:
        return (*self.items,)

    @cached_property
    def with_length(self) -> dict[str, object]:
        return {"items": self.items, "n": len(self.items)}

    @cached_property
    def with_helper(self) -> tuple[object, int]:
        return (self.items, self._count())

    @cached_property
    def with_property(self) -> list[object]:
        return [self.items, self.size]

    @cached_property
    def with_self(self) -> tuple[object, int]:
        return (self.items, summarize(self))

    @cached_property
    def unknown_method(self) -> tuple[object, object]:
        return (self.items, self.missing())

    @cached_property
    def tested(self) -> list[str]:
        return self.items if self.items else []

    @cached_property
    def as_key(self) -> dict[object, int]:
        return {self.items: 1}

    def add(self, x: str) -> None:
        self.items.append(x)
"#;

#[test]
fn mut101_reports_in_place_changes_only_of_attributes_the_cache_derives_from() {
    let p = Project::new("regr_mut101_held");
    p.write("held.py", HELD);
    let found = reports(&p, "MUT101");
    let got: Vec<(u64, &str, &str)> = found
        .iter()
        .map(|(line, qualname, message)| {
            let property = message.rsplit_once("`@cached_property` `").unwrap().1;
            (
                *line,
                qualname.as_str(),
                property.split('`').next().unwrap(),
            )
        })
        .collect();
    // Not reported: the in-place changes of `_empty`, `_patterns`, and `items` in
    // `Parser.report` (a dict display and an item assignment of a tuple holding them, the
    // attribute itself, a list it is appended to, a conditional expression). Reported: their
    // rebindings, and every in-place change a copy, a computed value, a helper, a property, a
    // call with `self`, an unknown method, a truthiness test, or a dict key derives from.
    assert_eq!(
        got,
        vec![
            (46, "Parser.reset", "context"),
            (47, "Parser.reset", "collected"),
            (47, "Parser.reset", "either"),
            (47, "Parser.reset", "view"),
            (94, "Copies.add", "as_key"),
            (94, "Copies.add", "tested"),
            (94, "Copies.add", "unknown_method"),
            (94, "Copies.add", "unpacked"),
            (94, "Copies.add", "with_helper"),
            (94, "Copies.add", "with_length"),
            (94, "Copies.add", "with_property"),
            (94, "Copies.add", "with_self"),
        ],
        "{found:#?}"
    );
}

/// A cached property in one module, the in-place change in a subclass in another: the
/// getter's alias fact travels in its summary.
#[test]
fn mut101_held_attributes_cross_modules() {
    let p = Project::new("regr_mut101_modules");
    let base = |body: &str| {
        format!(
            "from functools import cached_property\n\n\nclass Base:\n    def __init__(self):\n        self.items = []\n\n    @cached_property\n    def view(self):\n        return {body}\n"
        )
    };
    p.write("pkg/__init__.py", "")
        .write("pkg/base.py", &base("self.items"))
        .write(
            "pkg/derived.py",
            "from pkg.base import Base\n\n\nclass Derived(Base):\n    def push(self, x):\n        self.items.append(x)\n",
        );
    assert_eq!(lines(&p, "MUT101"), vec![]);
    p.write("pkg/base.py", &base("list(self.items)"));
    assert_eq!(lines(&p, "MUT101"), vec![at(6, "Derived.push")]);
}

/// A getter that calls a `self` method for what it does (`self.finalize()`, result discarded)
/// and returns the attribute itself: the call reaches the cached value only through what it
/// writes. When it writes nothing the getter reads, the in-place changes of the returned
/// attribute are seen; when the getter reads what it writes (`_names`, `finalized`), the call
/// may have derived that from the attribute, and they are reported.
const DISCARDED: &str = r#"from functools import cached_property


def note(message):
    print(message)


class Registry:
    def __init__(self):
        self.finalized = False
        self._tasks = {}
        self._names = []

    def finalize(self):
        if not self.finalized:
            self.finalized = True
            for task in self._tasks.values():
                print(task)

    def _index(self):
        self._names = sorted(self._tasks)

    @cached_property
    def tasks(self):
        self.finalize()
        return self._tasks

    @cached_property
    def indexed(self):
        self._index()
        return (self._tasks, self._names)

    @cached_property
    def with_state(self):
        self.finalize()
        return (self._tasks, self.finalized)

    @cached_property
    def summary(self):
        names = self._index()
        return (self._tasks, names)

    def _names_of(self):
        for name in self._tasks:
            yield name

    @cached_property
    def noted(self):
        note("tasks")
        self._names_of()
        return self._tasks

    def register(self, name, task):
        self._tasks[name] = task
"#;

#[test]
fn mut101_getters_calling_self_methods_for_their_effects() {
    let p = Project::new("regr_mut101_discarded");
    p.write("registry.py", DISCARDED);
    let found = reports(&p, "MUT101");
    let got: Vec<(u64, &str, &str)> = found
        .iter()
        .map(|(line, qualname, message)| {
            let property = message.rsplit_once("`@cached_property` `").unwrap().1;
            (
                *line,
                qualname.as_str(),
                property.split('`').next().unwrap(),
            )
        })
        .collect();
    // Not reported: `register` for `tasks`, and for `noted`, whose calls run nothing that
    // reaches the value (a function on no `self`, a generator method's body runs only when its
    // result is iterated). Reported: `register` for the getters that read what their discarded
    // call writes (`indexed`, `with_state`) or use its result (`summary`).
    assert_eq!(
        got,
        vec![
            (16, "Registry.finalize", "with_state"),
            (21, "Registry._index", "indexed"),
            (54, "Registry.register", "indexed"),
            (54, "Registry.register", "summary"),
            (54, "Registry.register", "with_state"),
        ],
        "{found:#?}"
    );
}

// ========================================================== MUT101 / MUT107: custom metaclasses

/// A metaclass other than `type` / `abc.ABCMeta` may replace attributes with descriptors when it
/// creates the class: here `_attr_is_on` becomes a property whose setter invalidates `is_on`, so
/// `self._attr_is_on = True` is a descriptor call heki does not see. The write rules stay quiet
/// on such classes and their subclasses; a plain class, an `abc.ABCMeta` class, and one with
/// `metaclass=type` are reported.
const METACLASSES: &str = r#"from abc import ABCMeta
from functools import cached_property


def _attr_property(name):
    def getter(obj):
        return getattr(obj, f"__attr_{name}")

    def setter(obj, value):
        setattr(obj, f"__attr_{name}", value)
        obj.__dict__.pop(name, None)

    return property(getter, setter)


class CachedProperties(type):
    def __init__(cls, name, bases, namespace, **kwargs):
        super().__init__(name, bases, namespace, **kwargs)
        for prop in namespace.get("_cached", ()):
            setattr(cls, f"_attr_{prop}", _attr_property(prop))


class Entity(metaclass=CachedProperties):
    _cached = ("is_on",)

    def __init__(self):
        self._attr_is_on = False

    @cached_property
    def is_on(self):
        return self._attr_is_on

    def __hash__(self):
        return hash(self._attr_is_on)


class Switch(Entity):
    def turn_on(self):
        self._attr_is_on = True


class Plain:
    def __init__(self):
        self._attr_is_on = False

    @cached_property
    def is_on(self):
        return self._attr_is_on

    def __hash__(self):
        return hash(self._attr_is_on)

    def turn_on(self):
        self._attr_is_on = True


class Abstract(metaclass=ABCMeta):
    def __init__(self):
        self._attr_is_on = False

    @cached_property
    def is_on(self):
        return self._attr_is_on

    def turn_on(self):
        self._attr_is_on = True


class Typed(metaclass=type):
    def __init__(self):
        self._attr_is_on = False

    @cached_property
    def is_on(self):
        return self._attr_is_on

    def turn_on(self):
        self._attr_is_on = True
"#;

#[test]
fn mut101_and_mut107_stay_quiet_on_classes_a_metaclass_may_give_descriptors() {
    let p = Project::new("regr_mut101_metaclass");
    p.write("entities.py", METACLASSES);
    assert_eq!(
        lines(&p, "MUT101"),
        vec![
            at(54, "Plain.turn_on"),
            at(66, "Abstract.turn_on"),
            at(78, "Typed.turn_on"),
        ]
    );
    assert_eq!(lines(&p, "MUT107"), vec![at(54, "Plain.turn_on")]);
}

// ================================================ MUT101 / MUT107: writes that run code

/// A store or delete of `self.<attr>` that is no plain store: a metaclass given through the bases
/// (a call building the base, or a module variable holding one), a property of that name (with
/// a setter, a raising deleter, or none, when the store raises), and `__setattr__`. The write
/// rules claim a plain store, so they stay quiet; `Plain` is reported.
const WRITES_THAT_RUN_CODE: &str = r#"from functools import cached_property


class _Meta(type):
    pass


def with_metaclass(meta, *bases):
    return meta("NewBase", bases, {})


Made = with_metaclass(_Meta)


class Proxy(with_metaclass(_Meta)):
    def __init__(self, wrapped):
        self.wrapped = wrapped

    def __hash__(self):
        return hash(self.wrapped)

    def __iadd__(self, other):
        self.wrapped += other
        return self


class ViaVariable(Made):
    def __init__(self, v):
        self.v = v

    def __hash__(self):
        return hash(self.v)

    def bump(self):
        self.v += 1


class ResourceId:
    def __init__(self, value):
        self.__dict__["id"] = value

    @property
    def id(self):
        return self.__dict__["id"]

    @id.deleter
    def id(self):
        raise Exception("The resource id cannot be deleted.")

    def __hash__(self):
        return hash(self.id)

    def forget(self):
        del self.id


class Settable:
    def __init__(self, x):
        self._x = x

    x = property(lambda self: self._x)

    @cached_property
    def doubled(self):
        return self.x * 2

    def __hash__(self):
        return hash(self.x)

    def change(self):
        self.x = 1


class Frozen:
    def __init__(self, x):
        object.__setattr__(self, "x", x)

    def __setattr__(self, name, value):
        raise AttributeError(name)

    @cached_property
    def doubled(self):
        return self.x * 2

    def __hash__(self):
        return hash(self.x)

    def change(self):
        self.x = 1


class Plain:
    def __init__(self, v):
        self.v = v

    @cached_property
    def doubled(self):
        return self.v * 2

    def __hash__(self):
        return hash(self.v)

    def bump(self):
        self.v += 1
"#;

#[test]
fn mut101_and_mut107_stay_quiet_on_writes_that_run_code() {
    let p = Project::new("regr_mut107_writes_run_code");
    p.write("shapes.py", WRITES_THAT_RUN_CODE);
    assert_eq!(lines(&p, "MUT101"), vec![at(104, "Plain.bump")]);
    assert_eq!(lines(&p, "MUT107"), vec![at(104, "Plain.bump")]);
}

/// The `__hash__` the writer's instances use reads the attribute through `self.identity`, each
/// call running the definition their class's MRO finds: `Combined` runs `Expression.identity`,
/// which reads only `self.args`, although `Lookup` overrides `identity` to read `self.lhs`.
const HASH_READS_PER_CLASS: &str = r#"class Expression:
    def __init__(self, *args):
        self.args = args

    @property
    def identity(self):
        return (type(self), self.args)

    def __eq__(self, other):
        return isinstance(other, Expression) and other.identity == self.identity

    def __hash__(self):
        return hash(self.identity)


class Combined(Expression):
    def __init__(self, lhs, rhs):
        super().__init__(lhs, rhs)
        self.lhs = lhs
        self.rhs = rhs

    def set_source_expressions(self, exprs):
        self.lhs, self.rhs = exprs


class Lookup(Expression):
    def __init__(self, lhs, rhs):
        super().__init__()
        self.lhs = lhs
        self.rhs = rhs

    @property
    def identity(self):
        return type(self), self.lhs, self.rhs

    def __eq__(self, other):
        return isinstance(other, Lookup) and self.identity == other.identity

    def __hash__(self):
        return hash(self.identity)


class LookupChild(Lookup):
    def set_lhs(self, v):
        self.lhs = v


class Tagged(Expression):
    @property
    def identity(self):
        return (self.tag,)

    def retag(self, t):
        self.tag = t
"#;

#[test]
fn mut107_reads_the_hash_the_writers_instances_use() {
    let p = Project::new("regr_mut107_hash_per_class");
    p.write("expressions.py", HASH_READS_PER_CLASS);
    assert_eq!(
        lines(&p, "MUT107"),
        vec![at(45, "LookupChild.set_lhs"), at(54, "Tagged.retag")]
    );
}

// ============================================================= MUT101: lazy initialization

/// Lazy initializations (`if self._conn is None: self._conn = ...`, `if not self.finalized:`, in
/// a method or a property getter) that the cached property runs itself, when it reads the
/// attribute nowhere else: computing the cached value set the attribute, so the test cannot find
/// it unset again. A cached property reading it directly may have cached its unset value.
const LAZY: &str = r#"import threading
from functools import cached_property


def open_connection(url):
    return object()


class Client:
    _conn = None
    _pool = None

    def __init__(self, url):
        self.url = url
        self._lock = threading.Lock()
        self.finalized = False
        self._tasks = {}

    def _connect(self):
        if self._conn is None:
            self._conn = open_connection(self.url)
        return self._conn

    @property
    def pool(self):
        if self._pool is None:
            self._pool = [self.url]
        return self._pool

    def finalize(self):
        with self._lock:
            if not self.finalized:
                self.finalized = True
                for task in self._tasks.values():
                    print(task)

    @cached_property
    def database(self):
        return self._connect().db

    @cached_property
    def client(self):
        return tuple(self.pool)

    @cached_property
    def tasks(self):
        self.finalize()
        return dict(self._tasks)

    @cached_property
    def connected(self):
        return self._conn is not None

    @cached_property
    def label(self):
        return repr(self._conn) + repr(self._connect())

    def reconnect(self):
        if not self._conn:
            self._conn = open_connection(self.url)

    def close(self):
        self._conn = None

"#;

#[test]
fn mut101_skips_lazy_initializations_the_cached_property_runs() {
    let p = Project::new("regr_mut101_lazy");
    p.write("lazy.py", LAZY);
    let found = reports(&p, "MUT101");
    let got: Vec<(u64, &str, &str)> = found
        .iter()
        .map(|(line, qualname, message)| {
            let property = message.rsplit_once("`@cached_property` `").unwrap().1;
            (
                *line,
                qualname.as_str(),
                property.split('`').next().unwrap(),
            )
        })
        .collect();
    // Not reported: `_connect`, `reconnect`, and the getter `pool` for the properties that
    // read the attribute only through those initializations (`database`, `client`), and
    // `finalize` for `tasks`. Reported: the same initializations for the properties that read
    // the attribute directly (`connected`, `label`), and the rebinding that is no lazy
    // initialization (`close`).
    assert_eq!(
        got,
        vec![
            (21, "Client._connect", "connected"),
            (21, "Client._connect", "label"),
            (60, "Client.reconnect", "connected"),
            (60, "Client.reconnect", "label"),
            (63, "Client.close", "connected"),
            (63, "Client.close", "database"),
            (63, "Client.close", "label"),
        ],
        "{found:#?}"
    );
}

/// `setup_*` and `_setup_*` methods are two-phase initialization, as `setup` and `_init_*` are
/// (here a subclass's `__init__` calls one, nothing calls the other). Another method setting the
/// attribute is reported.
const SETUP: &str = r#"from functools import cached_property


class Terminal:
    @cached_property
    def colored(self):
        return not self.no_color

    def setup_terminal(self, no_color=False):
        self.no_color = no_color

    def _setup_streams(self, out):
        self.no_color = not out.isatty()

    def set_color(self, no_color):
        self.no_color = no_color


class Tool(Terminal):
    def __init__(self, no_color=False):
        self.setup_terminal(no_color)
"#;

#[test]
fn mut101_counts_setup_methods_as_two_phase_initialization() {
    let p = Project::new("regr_mut101_setup");
    p.write("setup.py", SETUP);
    assert_eq!(lines(&p, "MUT101"), vec![at(16, "Terminal.set_color")]);
}

// ======================================================= EFF103: `Session.query` builds a query

/// R19: `sqlalchemy.orm.Session.query(...)` and the `Query` builder methods only build a lazy
/// `Query`; executing it (`all()`, `first()`, ...) or iterating it runs the query.
const QUERIES: &str = r#"from sqlalchemy.orm import Session


def build(session: Session, models: list) -> list:
    queries = []
    for model in models:
        queries.append(session.query(model).filter(model.id > 0).order_by(model.id))
    return queries


def run_all(session: Session, models: list) -> list:
    out = []
    for model in models:
        out.append(session.query(model).filter(model.id > 0).all())
    return out


def first(session: Session, ids):
    for i in ids:
        session.query(object).filter_by(id=i).first()


def iterate(session: Session, models):
    for model in models:
        for row in session.query(model):
            print(row)


def built_before(session: Session, models):
    q = session.query(object)
    for model in models:
        for row in q:
            print(row, model)


def comprehension(session: Session, models):
    for model in models:
        print([r for r in session.query(model).filter(model.ok)])


def generator(session: Session, models):
    for model in models:
        print(sum(1 for _ in session.query(model)))


def once(session: Session):
    for row in session.query(object):
        print(row)
"#;

#[test]
fn eff103_counts_executed_and_iterated_queries_not_built_ones() {
    let p = Project::new("regr_eff103_query");
    p.write("queries.py", QUERIES);
    // Not reported: queries built in the loop and never run there, and an iteration outside any
    // loop. Reported once each, at the expression that runs the query: the executing call, the
    // `Query` a nested `for` iterates (also one built before the loop), the comprehension, the
    // generator expression. Comprehensions run in the function from Python 3.12 (PEP 709) and
    // in a function of their own before.
    for (target, comprehension) in [("py312", (38, 27)), ("py311", (38, 15))] {
        let found: Vec<(u64, u64, String, String)> =
            diagnostics(&p, &["--select", "EFF103", "--target-version", target])
                .iter()
                .map(|d| {
                    (
                        d["line"].as_u64().unwrap(),
                        d["column"].as_u64().unwrap(),
                        d["qualname"].as_str().unwrap().to_owned(),
                        d["target"].as_str().unwrap().to_owned(),
                    )
                })
                .collect();
        let r = |line, column, qualname: &str, target: &str| {
            (line, column, qualname.to_owned(), target.to_owned())
        };
        assert_eq!(
            found,
            vec![
                r(14, 20, "run_all", "sqlalchemy.orm.Query.all"),
                r(20, 9, "first", "sqlalchemy.orm.Query.first"),
                r(25, 20, "iterate", "sqlalchemy.orm.Query.__iter__"),
                r(32, 20, "built_before", "sqlalchemy.orm.Query.__iter__"),
                r(
                    comprehension.0,
                    comprehension.1,
                    "comprehension",
                    "sqlalchemy.orm.Query.__iter__"
                ),
                r(43, 19, "generator", "sqlalchemy.orm.Query.__iter__"),
            ],
            "{target}"
        );
    }
    let messages: Vec<String> = reports(&p, "EFF103")
        .into_iter()
        .map(|(_, _, m)| m)
        .collect();
    assert_eq!(
        messages[0],
        "every iteration over parameter `models` sends a database statement: `sqlalchemy.orm.Query.all`"
    );
}

/// Iterating a `Query` in an `async def` blocks the event loop, as executing it does.
#[test]
fn eff101_sees_a_query_iterated_on_the_event_loop() {
    let p = Project::new("regr_eff101_query");
    p.write(
        "handler.py",
        "from sqlalchemy.orm import Session\n\n\nasync def handler(session: Session):\n    for row in session.query(object):\n        print(row)\n    return session.query(object).filter_by(id=1)\n",
    );
    let found = reports(&p, "EFF101 [heuristic]");
    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(found[0].0, 5);
    assert!(
        found[0]
            .2
            .contains("the blocking call `sqlalchemy.orm.Query.__iter__`"),
        "{found:#?}"
    );
    // The help of a database call names an async driver.
    assert!(
        helps(&p, "EFF101 [heuristic]")[0]
            .2
            .ends_with("or use an async driver"),
        "{found:#?}"
    );
}

// ====================================================== OPT112: advice the loop lets you follow

/// The pytest review item (`src/_pytest/raises.py:1279`, greedy matching): the loop iterates the
/// list it removes from, so the removals cannot wait for a rebuild after the loop. The advice
/// follows what the loop does with the list: deferred removals only when it reads the list
/// nowhere else (a membership test guarding the removal aside), an index or a count built once
/// only when it does not change the list.
const SCANS: &str = r#"def greedy(expected: list, actual: list, check) -> list:
    remaining_actual = list(range(len(actual)))
    failed = []
    for i_exp, exp in enumerate(expected):
        for i_rem in remaining_actual:
            if check(exp, actual[i_rem]):
                remaining_actual.remove(i_rem)
                break
        else:
            failed.append(i_exp)
    return failed


def drop(items: list[int], unwanted):
    for x in unwanted:
        items.remove(x)


def guarded(items: list[int], unwanted):
    for x in unwanted:
        if x in items:
            items.remove(x)


def drain(queue: list[int], pick):
    while queue:
        queue.remove(pick())


def path(stack: list[int], nodes):
    for node in nodes:
        i = stack.index(node)
        stack.append(i)


def counts(items: list[int], xs):
    for x in xs:
        if items.count(x):
            items.append(x)


def positions(names: list[str], wanted):
    return [names.index(w) for w in wanted]
"#;

#[test]
fn opt112_advice_holds_for_what_the_loop_does_with_the_list() {
    let p = Project::new("regr_opt112_advice");
    p.write("scans.py", SCANS);
    let found = helps(&p, "OPT112");
    let advice: Vec<(u64, &str)> = found
        .iter()
        .map(|(line, _, h)| (*line, h.as_str()))
        .collect();
    let keep = "the loop reads the list after each removal, so keep its values in a `set` or a `dict` (`dict.fromkeys(...)` keeps their order) and delete them from it";
    let defer =
        "collect the values to drop in a `set` and rebuild the list once with a comprehension";
    assert_eq!(
        advice,
        vec![
            (7, keep),
            (16, defer),
            (22, defer),
            (27, keep),
            (
                32,
                "the loop changes the list, so keep a `dict` (value → index) up to date with each change"
            ),
            (
                38,
                "the loop changes the list, so keep a `collections.Counter(items)` up to date with each change"
            ),
            (
                43,
                "build a `dict` (value → index) or a `set` once before the loop"
            ),
        ],
        "{found:#?}"
    );
}

/// More getter shapes: what keeps an attribute held (writes to other attributes, item
/// assignment into a returned container, `insert` / `extend`, a project method through
/// `super()`), and what hands it, or `self`, to other code (`{**self.items}`, `self.__dict__`,
/// `self` stored, returned, merged with another value, or tested).
const GETTERS: &str = r#"from functools import cached_property


class Base:
    def zero(self) -> int:
        return 0


class Shapes(Base):
    def __init__(self) -> None:
        self.items: list[str] = []
        self.hits = 0
        self.tmp = None

    @cached_property
    def counted(self):
        self.hits = 1
        del self.tmp
        return self.items

    @cached_property
    def inserted(self):
        out = [1]
        out.insert(0, self.items)
        out.extend([2])
        return out

    @cached_property
    def keyed(self):
        out = {}
        out["items"] = self.items
        return out, super().zero()

    @cached_property
    def spread(self):
        return {**self.items}

    @cached_property
    def through_dict(self):
        return (self.items, self.__dict__)

    @cached_property
    def stores_self(self):
        self.me = self
        return self.items

    @cached_property
    def returns_self(self):
        if self.hits:
            return self
        return self.items

    @cached_property
    def merges_self(self, other=None):
        x = self if other is None else other
        return (self.items, x)

    @cached_property
    def tests_self(self):
        if self:
            return self.items
        return None

    def add(self, x: str) -> None:
        self.items.append(x)
"#;

#[test]
fn mut101_getters_that_hold_attributes_and_getters_that_hand_them_on() {
    let p = Project::new("regr_mut101_getters");
    p.write("getters.py", GETTERS);
    let found = reports(&p, "MUT101");
    let mut properties: Vec<&str> = found
        .iter()
        .filter(|(line, _, _)| *line == 65)
        .map(|(_, _, m)| m.rsplit_once("`@cached_property` `").unwrap().1)
        .map(|rest| rest.split('`').next().unwrap())
        .collect();
    properties.sort_unstable();
    assert_eq!(
        properties,
        vec![
            "merges_self",
            "returns_self",
            "spread",
            "stores_self",
            "tests_self",
            "through_dict"
        ],
        "{found:#?}"
    );
}

/// More unhashable-key and display shapes.
const DISPLAYS: &str = r#"def lists(A):
    rows = [[1], [2], [3], [4], [5], [6], [7], [8], [9], [10], [11], [12], [13], [14], [15], [16], [17]]
    for a in A:
        for r in rows:
            if a == r:
                print(a)


def sets(A, c):
    rows = [{1}, {2}, {3}, {4}, {5}, {6}, {7}, {8}, {9}, {10}, {11}, {12}, {13}, {14}, {15}, {16}, {17}] if c else [{0}, {1}, {2}, {3}, {4}, {5}, {6}, {7}, {8}, {9}, {10}, {11}, {12}, {13}, {14}, {15}, {16}]
    alias = rows
    for a in A:
        for r in alias:
            if a == r:
                print(a)


def mixed(A):
    rows = [[1], (2,), [3], [4], [5], [6], [7], [8], [9], [10], [11], [12], [13], [14], [15], [16], [17]]
    for a in A:
        for r in rows:
            if a == r:
                print(a)


def lists_and_sets(A):
    rows = [{1}, [2], [3], [4], [5], [6], [7], [8], [9], [10], [11], [12], [13], [14], [15], [16], [17]]
    for a in A:
        for r in rows:
            if a == r:
                print(a)


def classes(A):
    rows = [int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int, int]
    for a in A:
        for r in rows:
            if a == r:
                print(a)
"#;

#[test]
fn opt113_unhashable_display_elements() {
    let p = Project::new("regr_opt113_displays");
    p.write("displays.py", DISPLAYS);
    let found = helps(&p, "OPT113");
    let advice: Vec<(u64, &str)> = found
        .iter()
        .map(|(line, _, h)| (*line, h.as_str()))
        .collect();
    assert_eq!(
        advice,
        vec![
            (
                5,
                "`r` is an unhashable `list`, so index `rows` by a hashable form of it (a `tuple` or `frozenset` of its items) before the outer loop"
            ),
            (
                14,
                "`r` is an unhashable `set`, so index `alias` by a hashable form of it (a `tuple` or `frozenset` of its items) before the outer loop"
            ),
            // Lists among the elements: a `set` or `dict` holds them only if they are hashable.
            (
                22,
                "if the keys are hashable, group the elements of `rows` by value in a `dict` (value → list of equal elements) before the outer loop, then read each outer key with `.get(key, [])` so that a key with no element keeps its empty list"
            ),
            // Elements of two unhashable classes; class objects are hashable.
            (
                30,
                "`r` is an unhashable `list`, so index `rows` by a hashable form of it (a `tuple` or `frozenset` of its items) before the outer loop"
            ),
            // heki does not type the items of a display of classes.
            (
                38,
                "if the keys are hashable, group the elements of `rows` by value in a `dict` (value → list of equal elements) before the outer loop, then read each outer key with `.get(key, [])` so that a key with no element keeps its empty list"
            ),
        ],
        "{found:#?}"
    );
}

/// More uses of the list in the loop: attribute paths, item assignment and `+=` (changes),
/// a copy inside the loop, and the list returned from it (a read).
const USES: &str = r#"class Bag:
    def __init__(self) -> None:
        self.items: list[int] = []

    def drop(self, xs):
        for x in xs:
            self.items.remove(x)
            print(self.items[0])

    def where(self, xs):
        for x in xs:
            i = self.items.index(x)
            self.items[i] = 0

    def grow(self, xs):
        for x in xs:
            if self.items.count(x):
                self.items += [x]


def iterated(items: list[int]):
    for x in items:
        if x < 0:
            items.remove(x)


def copied(items: list[int], xs):
    for x in xs:
        alias = items
        items.remove(x)
        if not x:
            return alias
    return None
"#;

#[test]
fn opt112_reads_and_changes_through_paths_copies_and_returns() {
    let p = Project::new("regr_opt112_uses");
    p.write("uses.py", USES);
    let found = helps(&p, "OPT112");
    let advice: Vec<(u64, &str)> = found
        .iter()
        .map(|(line, _, h)| (*line, h.split(',').next().unwrap()))
        .collect();
    assert_eq!(
        advice,
        vec![
            (7, "the loop reads the list after each removal"),
            (12, "the loop changes the list"),
            (17, "the loop changes the list"),
            (24, "the loop reads the list after each removal"),
            (30, "the loop reads the list after each removal"),
        ],
        "{found:#?}"
    );
}

/// The IR copy of a starred display element does not change how the rewriting rules name what
/// blocks them: `[*work, *names]` is a use of `work`.
#[test]
fn starred_elements_keep_the_names_in_blockers() {
    let p = Project::new("regr_starred_blocker");
    p.write(
        "walk.py",
        "def walk(paths, names):\n    work = []\n    for p in paths:\n        work.insert(0, p)\n    todo = [*work, *names]\n    return todo\n",
    );
    let found = helps(&p, "OPT121");
    assert_eq!(found.len(), 1, "{found:#?}");
    assert!(
        found[0]
            .2
            .ends_with("[not auto-fixable: DequeEquivalentOps(work): `work` : this use has no `deque` equivalent (line 5)]"),
        "{found:#?}"
    );
}

/// `OPT113`'s exact claim, and the shapes that leave a premise of it open: the outer loop over a
/// module constant of 20 items, work before the test or after the `if` for every pair, an earlier
/// `and` operand, a generator, a `filter` call, or a value of unknown type (it may be a generator)
/// as the inner iterable, an inner collection the outer loop changes, also through another name
/// bound to it, rebinds, or may let other code change (`yield`, a builtin handed a function, a
/// closure capturing it), a store into the compared key of an inner item, also by a function handed
/// the item, and a trip count the code fixes.
const PREMISES: &str = r#"from dataclasses import dataclass, field
from typing import NamedTuple

from library import Base as LibraryBase


@dataclass
class Row:
    k: int
    id: int
    customer_id: int
    type_var: int
    key: int
    active: bool
    ok: bool
    flag: bool
    pending: bool
    used: bool = field(default=False)


class Plain:
    def __init__(self, k):
        self.k = k


class Pair(NamedTuple):
    k: int


class External(LibraryBase):
    def __init__(self, k):
        self.k = k


class Prop:
    @property
    def k(self):
        return 1


class Base:
    def __init__(self, k):
        self.k = k


class Sub(Base):
    @property
    def k(self):
        return 2


class Meta(type):
    pass


class Made(metaclass=Meta):
    def __init__(self, k):
        self.k = k


def described():
    return None


class Desc:
    k = described()


class KeyMixin:
    @property
    def k(self):
        return 3


class Keyed:
    def __init__(self, k):
        self.k = k


class MixedKeyed(KeyMixin, Keyed):
    pass


def plain_items(xs: list[Plain], ys: list[Plain]):
    out = []
    for a in xs:
        for b in ys:
            if a.k == b.k:
                out.append(b)
    return out


def tuple_fields(xs: list[Pair], ys: list[Pair]):
    out = []
    for a in xs:
        for b in ys:
            if a.k == b.k:
                out.append(b)
    return out


def union_items(xs: list[Row | Plain], ys: list[Row]):
    out = []
    for a in xs:
        for b in ys:
            if a.k == b.k:
                out.append(b)
    return out


def inert_work(xs: list[Row], ys: list[Row]):
    out = []
    for a in xs:
        for b in ys:
            n = len(ys)
            if a.k == b.k:
                out.append(n)
    return out


def external_base(xs: list[External], ys: list[External]):
    out = []
    for a in xs:
        for b in ys:
            if a.k == b.k:
                out.append(b)
    return out


def property_key(xs: list[Prop], ys: list[Prop]):
    out = []
    for a in xs:
        for b in ys:
            if a.k == b.k:
                out.append(b)
    return out


def subclass_property(xs: list[Base], ys: list[Base]):
    out = []
    for a in xs:
        for b in ys:
            if a.k == b.k:
                out.append(b)
    return out


def metaclass_made(xs: list[Made], ys: list[Made]):
    out = []
    for a in xs:
        for b in ys:
            if a.k == b.k:
                out.append(b)
    return out


def descriptor_key(xs: list[Desc], ys: list[Desc]):
    out = []
    for a in xs:
        for b in ys:
            if a.k == b.k:
                out.append(b)
    return out


def untyped_items(xs, ys: list):
    out = []
    for a in xs:
        for b in ys:
            if a.k == b.k:
                out.append(b)
    return out


RUNNERS = (
    "a", "b", "c", "d", "e", "f", "g", "h", "i", "j",
    "k", "l", "m", "n", "o", "p", "q", "r", "s", "t",
)


def exact_join(orders: list[Row], customers: list[Row]):
    out = []
    for order in orders:
        for customer in customers:
            if order.customer_id == customer.id:
                out.append((order, customer))
        del order.pending
    return out


def outer_constant(jobs):
    out = []
    for kind in RUNNERS:
        for j in jobs:
            if j["kind"] == kind:
                out.append(j)
    return out


def work_before_test(xs: list[Row], ys: list[Row], check):
    out = []
    for c1 in xs:
        for c2 in ys:
            skip = check(c2)
            if c1.type_var == c2.type_var and skip:
                out.append(c1)
    return out


def inner_changed(records: list[Row], events: list[Row]):
    for record in records:
        for event in events:
            if event.key == record.key:
                events.remove(event)
                break


def lazy_inner(names, calls, receiver_of):
    out = []
    for name in names:
        for r in (receiver_of(c) for c in calls):
            if r == name:
                out.append(r)
    return out


def and_filter(xs: list[Row], ys: list[Row]):
    out = []
    for a in xs:
        for b in ys:
            if b.active and a.k == b.k:
                out.append(a)
    return out


def sorted_inner(xs: list[Row], ys: list[Row]):
    out = []
    for a in xs:
        for b in sorted(ys):
            if a.k == b.k:
                out.append(a)
    return out


def sliced(xs: list[Row], ys: list[Row]):
    out = []
    for a in xs:
        for b in ys[:100]:
            if a.k == b.k:
                out.append(a)
    return out


def str_keys(xs: list[str], ys: list[str]):
    out = []
    for a in xs:
        for b in ys:
            if a == b:
                out.append(a)
    return out


def mixed_keys(xs: list[str], ys: list[str | int]):
    out = []
    for a in xs:
        for b in ys:
            if a == b:
                out.append(a)
    return out


def after_if_work(xs: list[Row], ys: list[Row], log):
    out = []
    for a in xs:
        for b in ys:
            if a.k == b.k:
                out.append(a)
            log(b)
    return out


def filtered_inner(xs: list[Row], ys: list[Row], keep):
    out = {}
    for a in xs:
        for b in filter(keep, ys):
            if a.k == b.k:
                out[a.k] = b
    return out


def enumerated(xs: list[Row], ys: list[Row]):
    out = []
    for a in xs:
        for i, b in enumerate(ys):
            if a.k == b.k:
                out.append(i)
    return out


def view(xs: list[Row], table: dict[str, Row]):
    out = []
    for a in xs:
        for b in table.values():
            if a.k == b.k:
                out.append(b)
    return out


def either(xs: list[Row], ys: list[Row], zs: list[Row], flag):
    rows = ys if flag else zs
    out = []
    for a in xs:
        for b in rows:
            if a.k == b.k:
                out.append(b)
    return out


def rebound(xs: list[Row], ys: list[Row], zs: list[Row]):
    out = []
    for a in xs:
        rows = ys if a.flag else zs
        for b in rows:
            if a.k == b.k:
                out.append(b)
    return out


def generator_before(names, calls, receiver_of):
    receivers = (receiver_of(c) for c in calls)
    out = []
    for name in names:
        for r in receivers:
            if r == name:
                out.append(r)
    return out


def unknown_type(xs: list[Row], ys):
    out = []
    for a in xs:
        for b in ys:
            if a.k == b.k:
                out.append(b)
    return out


def aliased(xs: list[Row], ys: list[Row]):
    rows = ys
    out = []
    for a in xs:
        for b in rows:
            if a.k == b.k:
                out.append(b)
        ys.pop()
    return out


def prefiltered(xs: list[Row], ys: list[Row]):
    kept = [b for b in ys if b.ok]
    out = []
    for a in xs:
        for b in kept:
            if a.k == b.k:
                out.append(b)
    return out


def words(xs: list[Row], text: str):
    out = []
    for a in xs:
        for w in text.split():
            if a.k == w:
                out.append(w)
    return out


def rekeyed(xs: list[Row], ys: list[Row]):
    for a in xs:
        for b in ys:
            if a.k == b.k:
                b.k = None


def item_rekeyed(xs: list[dict], ys: list[dict]):
    for a in xs:
        for b in ys:
            if a["k"] == b["k"]:
                b["k"] = None


def marks(xs: list[Row], ys: list[Row], cache: dict):
    for a in xs:
        for b in ys:
            if a.k == b.k:
                b.used = True
        del cache[a.k]


def by_key(xs: list[Row], d: dict):
    out = []
    for a in xs:
        for k, v in d.items():
            if a.k == k:
                out.append(v)
    return out


def by_keys(xs: list[Row], d: dict):
    out = []
    for a in xs:
        for k in d.keys():
            if k == a.k:
                out.append(k)
    return out


def by_value(xs: list[Row], d: dict[str, Row]):
    out = []
    for a in xs:
        for k, v in d.items():
            if v.k == a.k:
                out.append(k)
    return out


def by_item_value(xs: list[Row], d: dict):
    out = []
    for a in xs:
        for k, v in d.items():
            if v == a.k:
                out.append(k)
    return out


def sorted_with_key(xs: list[Row], ys: list[Row], shrink):
    for a in xs:
        for b in ys:
            if a.k == b.k:
                print(b)
        sorted(ys, key=shrink)


def filter_before(xs: list[Row], ys: list[Row], keep):
    kept = filter(keep, ys)
    out = []
    for a in xs:
        for b in kept:
            if a.k == b.k:
                out.append(b)
    return out


def helper_rekeys(xs: list[Row], ys: list[Row], rekey):
    for a in xs:
        for b in ys:
            if a.k == b.k:
                rekey(b)


def closure_drops(xs: list[Row], ys: list[Row]):
    drop = lambda: ys.pop()
    out = []
    for a in xs:
        for b in ys:
            if a.k == b.k:
                out.append(b)
        drop()
    return out


def lambda_in_loop(xs: list[Row], ys: list[Row], other: list):
    for a in xs:
        for b in ys:
            if a.k == b.k:
                print(b)
        other.sort(key=lambda x: ys.index(x))


def extended_alias(xs: list[Row], ys: list[Row]):
    rows = ys
    for a in xs:
        for b in ys:
            if a.k == b.k:
                print(b)
        rows += ["new"]


def yielding(xs: list[Row], ys: list[Row]):
    for a in xs:
        for b in ys:
            if a.k == b.k:
                print(b)
        yield a


def mixin_property(xs: list[Keyed], ys: list[Keyed]):
    out = []
    for a in xs:
        for b in ys:
            if a.k == b.k:
                out.append(b)
    return out
"#;

#[test]
fn opt113_states_its_exact_claim_only_when_every_premise_holds() {
    let p = Project::new("regr_opt113_premises");
    p.write("app.py", PREMISES);
    let (_, rule, _) = p.heki(&["rule", "OPT113", "--output-format", "json"]);
    let rule: serde_json::Value = serde_json::from_str(&rule).unwrap();
    let (_, json, stderr) = p.heki(&["check", "--select", "OPT113", "--output-format", "json"]);
    let diags: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    let exact = "does nothing else for an item that does not match";
    let tiers: Vec<(&str, bool, &serde_json::Value)> = diags
        .iter()
        .map(|d| {
            (
                d["qualname"].as_str().unwrap(),
                d["message"].as_str().unwrap().ends_with(exact),
                &d["certainty"],
            )
        })
        .collect();
    let heuristic = serde_json::json!("heuristic");
    let tier = |exact: bool| {
        if exact {
            &rule["certainty"]
        } else {
            &heuristic
        }
    };
    assert_eq!(
        tiers,
        [
            ("plain_items", true, tier(true)),
            ("tuple_fields", true, tier(true)),
            ("union_items", true, tier(true)),
            ("inert_work", false, tier(false)),
            ("external_base", false, tier(false)),
            ("property_key", false, tier(false)),
            ("subclass_property", false, tier(false)),
            ("metaclass_made", false, tier(false)),
            ("descriptor_key", false, tier(false)),
            ("untyped_items", false, tier(false)),
            ("exact_join", true, tier(true)),
            ("outer_constant", false, tier(false)),
            ("work_before_test", false, tier(false)),
            ("inner_changed", false, tier(false)),
            ("lazy_inner", false, tier(false)),
            ("and_filter", false, tier(false)),
            ("sorted_inner", true, tier(true)),
            ("sliced", false, tier(false)),
            ("str_keys", true, tier(true)),
            ("mixed_keys", true, tier(true)),
            ("after_if_work", false, tier(false)),
            ("filtered_inner", false, tier(false)),
            ("enumerated", true, tier(true)),
            ("view", true, tier(true)),
            ("either", true, tier(true)),
            ("rebound", false, tier(false)),
            ("generator_before", false, tier(false)),
            ("unknown_type", false, tier(false)),
            ("aliased", false, tier(false)),
            ("prefiltered", true, tier(true)),
            ("words", true, tier(true)),
            ("rekeyed", false, tier(false)),
            ("item_rekeyed", false, tier(false)),
            ("marks", true, tier(true)),
            ("by_key", true, tier(true)),
            ("by_keys", true, tier(true)),
            ("by_value", true, tier(true)),
            ("by_item_value", true, tier(true)),
            ("sorted_with_key", false, tier(false)),
            ("filter_before", false, tier(false)),
            ("helper_rekeys", false, tier(false)),
            ("closure_drops", false, tier(false)),
            ("lambda_in_loop", false, tier(false)),
            ("extended_alias", false, tier(false)),
            ("yielding", false, tier(false)),
            ("mixin_property", false, tier(false)),
        ],
        "{json}{stderr}"
    );
    let help = |name: &str| {
        diags.iter().find(|d| d["qualname"] == name).unwrap()["help"]
            .as_str()
            .unwrap()
            .to_owned()
    };
    // The advice names a `set` or a `dict` only for keys known to be hashable, and an index
    // built once only for an inner collection that is the same on every outer iteration.
    assert!(help("str_keys").starts_with("group the elements of `ys`"));
    assert!(help("mixed_keys").starts_with("group the elements of `ys`"));
    assert!(help("exact_join").starts_with("index `customers`"));
    assert!(help("untyped_items").starts_with("if the keys are hashable, "));
    assert!(help("inner_changed").starts_with(
        "if `events` is the same collection, with the same keys, on every iteration of the outer loop, "
    ));
    // A store into the compared key of an inner item would leave an index built once stale.
    assert!(help("rekeyed").starts_with(
        "if `ys` is the same collection, with the same keys, on every iteration of the outer loop"
    ));
    // A mapping stepped through by its own keys needs no index: a lookup in it does.
    assert_eq!(
        help("by_key"),
        "`d` is already keyed by `k`: look each outer key up in it (`d.get(...)`) instead of looping over it"
    );
    assert!(help("by_keys").starts_with("`d` is already keyed by `k`"));
    assert!(help("by_value").starts_with("index `d.items()`"));
    assert!(help("by_item_value").starts_with("if the keys are hashable,"));
}

/// Under assumption R2 a value annotated `list` may be an instance of a project subclass: one
/// with its own `__iter__` (or a `dict` subclass with its own `items`) runs code to advance the
/// inner loop, so the joins over such annotated values are heuristic.
const OVERRIDES: &str = r#"from dataclasses import dataclass


@dataclass
class Row:
    k: int


class LoudList(list):
    def __iter__(self):
        print("next")
        return super().__iter__()


class LoudDict(dict):
    def items(self):
        print("items")
        return super().items()


def over_list(xs: list[Row], ys: list[Row]):
    out = []
    for a in xs:
        for b in ys:
            if a.k == b.k:
                out.append(b)
    return out


def over_items(xs: list[Row], d: dict[str, Row]):
    out = []
    for a in xs:
        for k, v in d.items():
            if v.k == a.k:
                out.append(k)
    return out


def over_sorted(xs: list[Row], ys: list[Row]):
    out = []
    for a in xs:
        for b in sorted(ys):
            if a.k == b.k:
                out.append(b)
    return out
"#;

#[test]
fn opt113_leaves_annotated_iterables_open_when_a_project_subclass_iterates_its_own_way() {
    let p = Project::new("regr_opt113_overrides");
    p.write("app.py", OVERRIDES);
    let (_, json, stderr) = p.heki(&["check", "--select", "OPT113", "--output-format", "json"]);
    let diags: Vec<serde_json::Value> = serde_json::from_str(&json).unwrap();
    let exact = "does nothing else for an item that does not match";
    let tiers: Vec<(&str, bool)> = diags
        .iter()
        .map(|d| {
            (
                d["qualname"].as_str().unwrap(),
                d["message"].as_str().unwrap().ends_with(exact),
            )
        })
        .collect();
    // `sorted(...)` builds a builtin list whatever it is given.
    assert_eq!(
        tiers,
        [
            ("over_list", false),
            ("over_items", false),
            ("over_sorted", true)
        ],
        "{json}{stderr}"
    );
}
