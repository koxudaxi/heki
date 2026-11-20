#!/usr/bin/env python3
"""End-to-end cases for the accumulation rules `OPT101`, `OPT102`, and `OPT103` (Milestone M8).

`crates/heki/tests/e2e_soundness.rs` pins every function's diagnostics and checks on CPython that
`heki check --fix` (and `--fix --include-size-dependent`) keeps this script's output unchanged.
"""
from __future__ import annotations


# ==============================================================================
# OPT101: statement shapes and edits
# ==============================================================================
def trailing_comma(items):
    x = []
    for i in items:
        x = x + [i,]
    return x


def starred(rows):
    x = []
    for row in rows:
        x = x + [*row]
    return x


def one_line_body(items):
    x = []
    for i in items: x = x + [i]  # grow
    return x


def parenthesized(items):
    """Not a plain `x = x + [...]` statement: proven safe, edited by hand."""
    x = []
    for i in items:
        x = (x + [i])
    return x


def parenthesized_operand(items):
    x = []
    for i in items:
        x = (x) + [i]
    return x


def annotated(items):
    x: list = []
    for i in items:
        x: list = x + [i]
    return x


def through_a_temporary(items):
    """`new = x + [i]; x = new` is not the accumulation pattern."""
    x = []
    for i in items:
        new = x + [i]
        x = new
    return x


def walrus_rebinds(items):
    """The right-hand side rebinds `x` (`Disproved`)."""
    x = []
    for i in items:
        x = x + [(x := [i])]
    return x


def rhs_changes_list(items):
    """The right-hand side changes `x` before the concatenation reads it (`Disproved`)."""
    x = [0]
    for i in items:
        x = x + [x.pop(), i]
    return x


def rhs_changes_other(items):
    """The right-hand side changes another list: still proven safe."""
    other = [9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9, 9]
    x = []
    for i in items:
        x = x + [other.pop()]
    return x


def deleted_accumulator(items):
    """`del x` before the read: the value is not tracked (the loop raises `NameError`)."""
    x = []
    for i in items:
        print(x)
        del x
        x = x + [i]
    return x


# ==============================================================================
# OPT102: dict merges and `|`
# ==============================================================================
def merge_exact_dict(items):
    d = {}
    extra = {"a": 1}
    for i in items:
        d = {**d, **extra}
    return d


def merge_unknown_mapping(items, other):
    d = {}
    for i in items:
        d = {**d, **other}
    return d


def merge_constant_keys(items):
    """Builtin keys, and the later entry reads a local name: `update` inserts them alike."""
    d = {}
    for i in items:
        j = -i
        d = {**d, "lo": i, "hi": j}
    return d


def key_not_simple_text(items):
    d = {}
    for k in items:
        d = {**d, (k): 1}
    return d


def constant_key(items):
    d = {}
    for v in items:
        d = {**d, None: v}
    return d


def parameter_key(items, key):
    d = {}
    for v in items:
        d = {**d, key: v}
    return d


def conditional_key(items):
    """`k` is a phi of a phi: still bound on every path."""
    d = {}
    k = "even"
    for v in items:
        if v % 2:
            k = "odd"
        d = {**d, k: v}
    return d


def dict_or(items):
    d = {}
    for k in items:
        d = d | {k: 1}
    return d


def set_or_list(items):
    """`set | list` raises `TypeError`: the right operand is a `list`."""
    s = set()
    for g in items:
        s = s | [g]
    return s


def set_or_parenthesized(items):
    s = set()
    for g in items:
        s = (s | {g})
    return s


def int_flags(items):
    """`int | int` is not a container merge."""
    flags = 0
    for f in items:
        flags = flags | f
    return flags


def int_or(n):
    """An exact `int` accumulated with `|` is not a container merge."""
    flags = 0
    for i in range(n):
        flags = flags | i
    return flags


def merge_other_first(items):
    """dcg `{**other, **d}`: `Disproved`."""
    found = {}
    for site in items:
        found = {**site, **found}
    return found


def merge_twice(items):
    d = {}
    for k in items:
        d = {**d, k: 1, **d}
    return d


# ==============================================================================
# OPT103: immutable accumulation (diagnostic only)
# ==============================================================================
def bytes_literal_pieces(chunks):
    acc = b""
    for c in chunks:
        acc = acc + b"."
    return acc + bytes(chunks)


def bytes_unknown_pieces(chunks):
    acc = b""
    for c in chunks:
        acc = acc + c
    return acc


def tuple_in_place(items):
    acc = ()
    for c in items:
        acc += (c,)
    return acc


def bytes_plus_str(chunks):
    acc = b""
    for c in chunks:
        acc = acc + "x"
    return acc


def str_accumulation(chunks):
    """Local `str` concatenation is optimized by CPython: not reported."""
    acc = ""
    for c in chunks:
        acc = acc + c
    return acc


def number_accumulation(items):
    acc = 0
    for c in items:
        acc = acc + c
    return acc


def read_in_loop(chunks, log):
    acc = ()
    for c in chunks:
        acc += (c,)
        log(len(acc))
    return acc


def tested_in_loop(chunks):
    acc = b""
    for c in chunks:
        acc = acc + b"-"
        if acc:
            continue
    return acc


def handler_reads(chunks):
    acc = b""
    try:
        for c in chunks:
            acc = acc + b"+"
            int(c)
    except ValueError:
        return acc
    return acc


def few_iterations():
    acc = b""
    for i in range(4):
        acc = acc + b"x"
    return acc


def retry(fetch):
    acc = b""
    for attempt in range(3):
        try:
            return fetch()
        except ValueError:
            acc = acc + b"!"
    return acc


class Report:
    def __init__(self):
        self.s = ""
        self.n = 0

    def render(self, xs):
        for x in xs:
            self.s += f"{x},"
            self.n += 1
        return self.s, self.n


def capped_by_test(chunks):
    """`while len(data) < 10` caps the accumulator: no copy is longer than 10 bytes."""
    data = b""
    it = iter(chunks)
    while len(data) < 10:
        data += next(it, b"0123456789")
    return data


class Item:
    def __init__(self, name):
        self.name = name


def element_attributes(names):
    """Each iteration appends once to another object's attribute."""
    items = [Item(str(n)) for n in names]
    for item in items:
        item.name += "_x"
    return [i.name for i in items]


def comment_after_element(items):
    """The element keeps the line break that ends its comment."""
    x = []
    for i in items:
        x = x + [i  # keep this comment
                 ]
    return x


def comment_before_starred(rows):
    """A comment first: the starred element is still found (`extend`)."""
    x = []
    for row in rows:
        x = x + [  # spread
            *row]
    return x


def merge_with_comment(items):
    d = {}
    extra = {"a": 1}
    for i in items:
        d = {**d, **extra  # keep this comment
             }
    return d


def run(fn, *args):
    try:
        out = fn(*args)
        return repr(out)
    except Exception as e:  # noqa: BLE001 - the transcript records every outcome
        return f"{type(e).__name__}"


def main() -> int:
    lines = []
    for n in [0, 1, 4, 9]:
        items = list(range(n))
        rows = [[i, -i] for i in items]
        logged = []

        def fetch():
            raise ValueError("no")

        lines += [
            run(trailing_comma, items),
            run(starred, rows),
            run(one_line_body, items),
            run(parenthesized, items),
            run(parenthesized_operand, items),
            run(annotated, items),
            run(through_a_temporary, items),
            run(walrus_rebinds, items),
            run(rhs_changes_list, items),
            run(rhs_changes_other, items),
            run(merge_exact_dict, items),
            run(merge_unknown_mapping, items, {"b": n}),
            run(merge_constant_keys, items),
            run(comment_after_element, items),
            run(comment_before_starred, [items, items]),
            run(merge_with_comment, items),
            run(key_not_simple_text, items),
            run(constant_key, items),
            run(parameter_key, items, "k"),
            run(conditional_key, items),
            run(dict_or, items),
            run(set_or_list, items),
            run(set_or_parenthesized, items),
            run(int_flags, items),
            run(int_or, n),
            run(merge_other_first, [{"k": i} for i in items]),
            run(merge_twice, items),
            run(bytes_literal_pieces, items),
            run(bytes_unknown_pieces, [b"a"] * n),
            run(tuple_in_place, items),
            run(bytes_plus_str, items),
            run(str_accumulation, ["a"] * n),
            run(number_accumulation, items),
            run(read_in_loop, items, logged.append),
            run(tested_in_loop, items),
            run(handler_reads, ["1", "x"][: n % 3]),
            run(few_iterations),
            run(retry, fetch),
            run(Report().render, items),
            run(capped_by_test, [b"ab"] * n),
            run(element_attributes, items),
            repr(logged),
        ]
    print("\n".join(lines))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
