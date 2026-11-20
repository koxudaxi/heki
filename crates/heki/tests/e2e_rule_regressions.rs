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
