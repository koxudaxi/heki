#!/usr/bin/env python3
"""`OPT` rewrite differential cases for `heki check --fix` (Milestone M6).

- `pos_*`: rewrites heki must prove safe; `OPT101` / `OPT102` are applied by the default `--fix`.
- `sd_*`: proven-safe size-dependent rewrites (`OPT110`, `OPT111`, `OPT120`, `OPT121`,
  `OPT130`), applied only with `--fix --include-size-dependent`.
- `cx_*`: the seven counterexamples of `counterexamples.py` (S23, S24, S26, dcg
  `{**other, **d}`, S27, S9b, S22) written in the exact shape the rules match, and `neg_*`
  further unsafe shapes. heki must judge every one `Blocked` or `Disproved` and never
  rewrite it; `naive_*` are the rewrites a naive tool would produce, and `main()` asserts that
  each one diverges from its original on CPython.

`python3.15 tests/difftest/opt_rewrites.py` prints a transcript of every case on several
inputs. The `heki` CLI tests run it before and after `--fix` and require identical output.
"""
from __future__ import annotations

import collections


# ==============================================================================
# OPT101 / OPT102 positives (default `--fix`)
# ==============================================================================
def pos_append(items):
    x = []
    for i in items:
        x = x + [i]
    return x


def pos_extend_pairs(items):
    out = []
    for i in items:
        out = out + [i, i * 2]
    return out


def pos_conditional(items):
    acc = []
    for i in items:
        if i % 2:
            acc = acc + [i]
    return acc


def pos_nested(rows):
    flat = []
    for row in rows:
        for v in row:
            flat = flat + [v]
    return flat


def pos_fresh_per_outer(rows):
    out = []
    for row in rows:
        cur = []
        for v in row:
            cur = cur + [v * 10]
        out.append(cur)
    return out


def pos_while(n):
    x = []
    i = 0
    while i < n:
        x = x + [i]
        i += 1
    return x


def pos_list_call(items):
    x = list(items)
    for i in items:
        x = x + [i]
    return x


def pos_try_handler(items):
    x = []
    for i in items:
        try:
            x = x + [10 // i]
        except ZeroDivisionError:
            x = x + [None]
    return x


def pos_generator(items):
    x = []
    for i in items:
        x = x + [str(i)]
    yield x


def pos_dict_kv(items):
    d = {}
    for k, v in items:
        d = {**d, k: v}
    return d


def pos_dict_multi(items):
    d = {}
    for k, v in items:
        d = {**d, k: v, f"{k}_len": len(str(v))}
    return d


def pos_dict_merge(dicts):
    merged = {}
    for extra in dicts:
        merged = {**merged, **extra}
    return merged


LOG = []


def key_of(v):
    LOG.append(("k", v))
    return v % 3


def val_of(v):
    LOG.append(("v", v))
    return 10 // (v - 3)


def pos_dict_effects(items):
    d = {}
    for i in items:
        d = {**d, key_of(i): val_of(i)}
    return d


def pos_set_union(groups):
    seen = set()
    for g in groups:
        seen = seen | {g}
    return sorted(seen)


# ==============================================================================
# Size-dependent positives (`--fix --include-size-dependent`)
# ==============================================================================
def sd_membership(probes):
    allowed = list(range(0, 50, 3))
    hits = []
    for p in range(probes):
        if p in allowed:
            hits.append(p)
    return hits


def sd_rebuild(probes):
    keys = [1, 2, 3, 5, 8, 13, 21, 34, 55]
    hits = 0
    for p in range(probes):
        if p in set(keys):
            hits += 1
    return hits


def sd_dedup(n):
    seen = []
    for v in range(n):
        w = v % 7
        if w not in seen:
            seen.append(w)
    return seen


def sd_queue(n):
    q = [0]
    out = []
    while q:
        cur = q.pop(0)
        out.append(cur)
        if cur < n:
            q.append(cur + 1)
    return out


def sd_front_insert(n):
    q = []
    for i in range(n):
        q.insert(0, i)
    return list(q)


# ==============================================================================
# The seven counterexamples in rule shape (must stay unchanged)
# ==============================================================================
def cx_s23():
    """S23: the `for` loop's iterator still walks the old list."""
    x = [1, 2]
    steps = 0
    for v in x:
        x = x + [v]
        steps += 1
        if steps >= 10:
            break
    return x, steps


def naive_s23():
    x = [1, 2]
    steps = 0
    for v in x:
        x.append(v)
        steps += 1
        if steps >= 10:
            break
    return x, steps


def cx_s24(n):
    """S24: `it = iter(x)` is live after the loop."""
    x = [1, 2]
    it = iter(x)
    for _ in range(n):
        x = x + [3]
    return list(it)


def naive_s24(n):
    x = [1, 2]
    it = iter(x)
    for _ in range(n):
        x.append(3)
    return list(it)


def cx_s26():
    """S26: the `dict.keys()` view is iterated while `d` is rebuilt."""
    d = {"a": 1}
    out = []
    for k in d.keys():
        d = {**d, "b": 2}
        out.append(k)
    return out, d


def naive_s26():
    d = {"a": 1}
    out = []
    for k in d.keys():
        d["b"] = 2
        out.append(k)
    return out, d


def cx_dcg(items):
    """dcg `_target_binding.py:465`: `{**site, **found}` keeps the earlier key."""
    found = {}
    for site in items:
        found = {**site, **found}
    return found


def naive_dcg(items):
    found = {}
    for site in items:
        found.update(site)
    return found


def cx_s27():
    """S27: `lst` is mutated inside the loop."""
    lst = [1, 2]
    hits = []
    for x in [1, 3, 3]:
        if x in lst:
            hits.append(x)
        if x == 3 and 3 not in lst:
            lst.append(3)
    return hits


def naive_s27():
    lst = [1, 2]
    lst_set = set(lst)
    hits = []
    for x in [1, 3, 3]:
        if x in lst_set:
            hits.append(x)
        if x == 3 and 3 not in lst_set:
            lst.append(3)
    return hits


class WeirdList(list):
    def __contains__(self, item):
        return True


def cx_s9b(probes):
    """S9b: the `list[int]` annotation hides a list subclass with its own `__contains__`."""
    wl: list[int] = WeirdList([1, 2])
    hits = []
    for p in probes:
        if p in wl:
            hits.append(p)
    return hits


def naive_s9b(probes):
    wl: list[int] = WeirdList([1, 2])
    wl_set = set(wl)
    hits = []
    for p in probes:
        if p in wl_set:
            hits.append(p)
    return hits


def cx_s22():
    """S22: hoisting `set(lst)` raises for unhashable elements even for a 0-trip loop."""
    lst = [[1], [2]]
    seen = []
    for x in []:
        seen.append(x in lst)
    return seen


def naive_s22():
    lst = [[1], [2]]
    lst_set = set(lst)
    seen = []
    for x in []:
        seen.append(x in lst_set)
    return seen


# ==============================================================================
# More unsafe shapes (must stay unchanged)
# ==============================================================================
def neg_alias_after(items):
    x = []
    snapshot = x
    for i in items:
        x = x + [i]
    return snapshot, x


def neg_prefix_snapshots(items):
    x = []
    prefixes = []
    for i in items:
        x = x + [i]
        prefixes.append(x)
    return prefixes


def neg_param_accumulator(acc, items):
    for i in items:
        acc = acc + [i]
    return acc


def neg_dict_reunpack(items):
    d = {}
    for k in items:
        d = {**d, k: 1, **d}
    return d


def neg_closure(items):
    x = []
    def peek():
        return len(x)
    sizes = []
    for i in items:
        x = x + [i]
        sizes.append(peek())
    return sizes


def neg_queue_returned(n):
    q = [0]
    while q and len(q) < n:
        q.append(q.pop(0) + 1)
        q.append(len(q))
    return q


def run(fn, *args):
    try:
        out = fn(*args)
        if hasattr(out, "__next__"):
            out = list(out)
        return repr(out)
    except Exception as e:  # noqa: BLE001 - the transcript records every outcome
        return f"{type(e).__name__}: {e}"


def main() -> int:
    lines = []
    for n in [0, 1, 2, 5, 30]:
        items = list(range(n))
        rows = [list(range(i)) for i in range(n)]
        pairs = [(f"k{i % 4}", i) for i in range(n)]
        cases = [
            (pos_append, (items,)),
            (pos_extend_pairs, (items,)),
            (pos_conditional, (items,)),
            (pos_nested, (rows,)),
            (pos_fresh_per_outer, (rows,)),
            (pos_while, (n,)),
            (pos_list_call, (items,)),
            (pos_try_handler, (items,)),
            (pos_generator, (items,)),
            (pos_dict_kv, (pairs,)),
            (pos_dict_multi, (pairs,)),
            (pos_dict_merge, ([{"a": i, f"b{i % 2}": -i} for i in range(n)],)),
            (pos_dict_effects, (items,)),
            (pos_set_union, ([i % 5 for i in range(n)],)),
            (sd_membership, (n,)),
            (sd_rebuild, (n,)),
            (sd_dedup, (n,)),
            (sd_queue, (n,)),
            (sd_front_insert, (n,)),
            (cx_s24, (n,)),
            (cx_dcg, ([{"k": i, f"x{i}": i} for i in range(n)],)),
            (cx_s9b, (items,)),
            (neg_alias_after, (items,)),
            (neg_prefix_snapshots, (items,)),
            (neg_param_accumulator, ([9], items)),
            (neg_dict_reunpack, (items,)),
            (neg_closure, (items,)),
            (neg_queue_returned, (n,)),
        ]
        for fn, args in cases:
            lines.append(f"{fn.__name__}({n}) = {run(fn, *args)}")
    lines.append(f"LOG = {LOG}")
    for fn in [cx_s23, cx_s26, cx_s27, cx_s22]:
        lines.append(f"{fn.__name__}() = {run(fn)}")
    lines.append(f"deque type = {type(collections.deque()).__name__}")

    # Each counterexample's naive rewrite diverges on CPython.
    assert run(cx_s23) != run(naive_s23)
    assert run(cx_s24, 2) != run(naive_s24, 2)
    assert run(cx_s26) != run(naive_s26)
    dcg_items = [{"k": 1, "a": 10}, {"k": 2, "b": 20}]
    assert run(cx_dcg, dcg_items) != run(naive_dcg, dcg_items)
    assert run(cx_s27) != run(naive_s27)
    assert run(cx_s9b, [999]) != run(naive_s9b, [999])
    assert run(cx_s22) != run(naive_s22)
    lines.append("naive rewrites diverge: 7/7")
    print("\n".join(lines))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
