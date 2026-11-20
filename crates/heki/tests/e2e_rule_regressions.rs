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
    diagnostics(p, &["--select", rule])
        .iter()
        .map(|d| {
            assert_eq!(d["rule_code"], rule, "{d:#}");
            (
                d["line"].as_u64().unwrap(),
                d["qualname"].as_str().unwrap().to_owned(),
                d["message"].as_str().unwrap().to_owned(),
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
    rows = [{"id": 1}, {"id": 2}, {"id": 3}, {"id": 4}, {"id": 5}, {"id": 6}, {"id": 7}, {"id": 8}, {"id": 9}]
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
            "nested loops join `A` and `rows` on `a == r` (O(n·m)); `r` is an unhashable `dict`, so index `rows` by a hashable form of it (a `tuple` or `frozenset` of its items) before the outer loop",
            "nested loops join `A` and `B` on `a.tags == tags` (O(n·m)); `tags` is an unhashable `list`, so index `B` by a hashable form of it (a `tuple` or `frozenset` of its items) before the outer loop",
            "nested loops join `expected` and `actual` on `actual_item == expected_item` (O(n·m)); build a `set` of `actual` before the outer loop",
            "nested loops join `A` and `rows + rows` on `a == r` (O(n·m)); build a `set` of `rows + rows` before the outer loop",
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
        "`sqlalchemy.orm.Query.all` runs a database query (`sqlalchemy.orm.Query.all`) on every loop iteration (N+1); batch it or load the data once before the loop"
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
    let found = reports(&p, "EFF101");
    assert_eq!(found.len(), 1, "{found:#?}");
    assert_eq!(found[0].0, 5);
    assert!(
        found[0]
            .2
            .contains("`sqlalchemy.orm.Query.__iter__` (blocking_db)"),
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
    let found = reports(&p, "OPT112");
    let advice: Vec<(u64, &str)> = found
        .iter()
        .map(|(line, _, m)| (*line, m.split_once("(O(n²)); ").unwrap().1))
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
    rows = [[1], [2], [3], [4], [5], [6], [7], [8], [9]]
    for a in A:
        for r in rows:
            if a == r:
                print(a)


def sets(A, c):
    rows = [{1}, {2}, {3}, {4}, {5}, {6}, {7}, {8}, {9}] if c else [{0}, {1}, {2}, {3}, {4}, {5}, {6}, {7}, {8}]
    alias = rows
    for a in A:
        for r in alias:
            if a == r:
                print(a)


def mixed(A):
    rows = [[1], (2,), [3], [4], [5], [6], [7], [8], [9]]
    for a in A:
        for r in rows:
            if a == r:
                print(a)
"#;

#[test]
fn opt113_unhashable_display_elements() {
    let p = Project::new("regr_opt113_displays");
    p.write("displays.py", DISPLAYS);
    let found = reports(&p, "OPT113");
    let advice: Vec<(u64, &str)> = found
        .iter()
        .map(|(line, _, m)| (*line, m.split_once("(O(n·m)); ").unwrap().1))
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
            (22, "build a `set` of `rows` before the outer loop"),
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
    let found = reports(&p, "OPT112");
    let advice: Vec<(u64, &str)> = found
        .iter()
        .map(|(line, _, m)| {
            (
                *line,
                m.split_once("(O(n²)); ")
                    .unwrap()
                    .1
                    .split(',')
                    .next()
                    .unwrap(),
            )
        })
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
    let found = reports(&p, "OPT121");
    assert_eq!(found.len(), 1, "{found:#?}");
    assert!(
        found[0]
            .2
            .ends_with("[not auto-fixable: DequeEquivalentOps(work): `work` : this use has no `deque` equivalent (line 5)]"),
        "{found:#?}"
    );
}
