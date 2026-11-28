#!/usr/bin/env python3
"""Adversarial soundness counterexample suite (S1-S28 + dcg `_target_binding.py:465`).

Every case here MUST be rejected (`Blocked` or `Disproved`) by `heki_soundness`
whenever the naive rewrite changes observable behavior (return value, exception type,
shared mutation, or termination). Running this script verifies the counterexamples
themselves actually diverge on the local CPython interpreter.
"""
from __future__ import annotations


def test_s23_for_iter_mutation_infinite_loop() -> None:
    """S23: `for v in x: x = x + [v]` terminates; naive `x.append(v)` loops infinitely!"""
    x = [1, 2]
    steps = 0
    for v in x:
        x = x + [v]
        steps += 1
        assert steps < 10
    assert x == [1, 2, 1, 2]

    # Naive rewrite diverges (we stop at 10 iterations to prove divergence).
    y = [1, 2]
    naive_steps = 0
    for v in y:
        y.append(v)
        naive_steps += 1
        if naive_steps >= 10:
            break
    assert naive_steps == 10, "naive rewrite must diverge"


def test_s24_derived_iter_live_at_write() -> None:
    """S24: `it = iter(x); x = x + [3]` leaves `it` pointing to old list `[1, 2]`."""
    x = [1, 2]
    it = iter(x)
    x = x + [3]
    orig_seen = list(it)

    y = [1, 2]
    it2 = iter(y)
    y.append(3)
    naive_seen = list(it2)
    assert orig_seen == [1, 2] and naive_seen == [1, 2, 3]


def test_s26_dict_keys_view_runtime_error() -> None:
    """S26: mutating dict while `dict.keys()` view is iterated raises RuntimeError."""
    d = {"a": 1}
    out_orig = []
    for k in d.keys():
        d = {**d, "b": 2}
        out_orig.append(k)
    assert out_orig == ["a"] and d == {"a": 1, "b": 2}

    d2 = {"a": 1}
    raised = False
    try:
        for _ in d2.keys():
            d2["b"] = 2
    except RuntimeError:
        raised = True
    assert raised, "naive dict mutation during keys() iteration must raise RuntimeError"


def test_dcg_right_side_dict_unpack_precedence() -> None:
    """dcg `_target_binding.py:465`: `{**other, **found}` preserves existing keys in `found`!"""
    items = [{"k": 1, "a": 10}, {"k": 2, "b": 20}]
    found: dict[str, int] = {}
    for site in items:
        found = {**site, **found}  # earlier key wins!

    found_naive: dict[str, int] = {}
    for site in items:
        found_naive.update(site)  # later key overwrites -> WRONG!

    assert found["k"] == 1 and found_naive["k"] == 2


def test_s27_loop_target_mutated_inside_loop() -> None:
    """S27: hoisting `e in lst` to `set(lst)` breaks when `lst` is mutated inside loop."""
    lst = [1, 2]
    hits_orig = []
    for x in [1, 3, 3]:
        if x in lst:
            hits_orig.append(x)
        if x == 3 and 3 not in lst:
            lst.append(3)

    lst2 = [1, 2]
    hoisted = set(lst2)
    hits_naive = []
    for x in [1, 3, 3]:
        if x in hoisted:
            hits_naive.append(x)
        if x == 3 and 3 not in lst2:
            lst2.append(3)

    assert hits_orig == [1, 3] and hits_naive == [1]


def test_s9b_list_subclass_custom_eq_contains() -> None:
    """S9b: PEP 484 `list[int]` annotation cannot be trusted by `heki_soundness`."""

    class WeirdList(list):
        def __contains__(self, item: object) -> bool:
            return True

    wl: list[int] = WeirdList([1, 2])
    assert (999 in wl) is True
    assert (999 in set(wl)) is False


def test_s22_zero_trip_loop_unhashable_exception() -> None:
    """S22: hoisting `set(lst)` out of a 0-trip loop raises TypeError when elements are unhashable."""
    lst = [[1], [2]]
    # Original: 0-trip loop never touches `lst` -> succeeds
    for x in []:
        _ = x in lst

    raised = False
    try:
        _ = set(lst)
    except TypeError:
        raised = True
    assert raised


def test_list_extend_asks_for_the_length() -> None:
    """OPT120 / OPT121: `list.extend(x)` and `list += x` ask an `x` that is not a builtin
    container for its length (`__len__`, else `__length_hint__`) before iterating it; the
    `deque` the fix makes never asks, so the call and its exception are gone."""
    from collections import deque

    asked: list[str] = []

    class Sized:
        def __iter__(self):
            return iter([1])

        def __len__(self) -> int:
            asked.append("len")
            return 1

    class Hinted:
        def __iter__(self):
            return iter([1])

        def __length_hint__(self) -> int:
            asked.append("hint")
            return 1

    class Unsized:
        def __iter__(self):
            return iter([1])

        def __len__(self) -> int:
            raise ValueError

    lst: list[int] = []
    lst.extend(Sized())
    lst.extend(Hinted())
    lst += Sized()
    assert asked == ["len", "hint", "len"]

    del asked[:]
    dq: deque[int] = deque()
    dq.extend(Sized())
    dq.extend(Hinted())
    dq += Sized()
    assert asked == []

    raised = False
    try:
        [].extend(Unsized())
    except ValueError:
        raised = True
    assert raised
    deque().extend(Unsized())


def main() -> int:
    tests = [
        test_s23_for_iter_mutation_infinite_loop,
        test_s24_derived_iter_live_at_write,
        test_s26_dict_keys_view_runtime_error,
        test_dcg_right_side_dict_unpack_precedence,
        test_s27_loop_target_mutated_inside_loop,
        test_s9b_list_subclass_custom_eq_contains,
        test_s22_zero_trip_loop_unhashable_exception,
        test_list_extend_asks_for_the_length,
    ]
    for fn in tests:
        fn()
        print(f"  PASS {fn.__name__}")
    print(f"All {len(tests)} adversarial counterexamples verified on CPython.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
