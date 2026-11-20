#!/usr/bin/env python3
"""Shapes whose IR used to hide a binding, an alias, or a path from the Soundness lane, so that
`heki check --fix` (`--include-size-dependent` for the size-dependent rules) proved a rewrite
safe that changes behaviour on CPython. Each `cx_*` function is the original; `old_*` is the
exact text the fix produced for it. `main()` asserts that every old rewrite diverges from its
original, and `crates/heki/tests/e2e_soundness.rs` asserts that heki now reports each original
as `Blocked` or not at all and rewrites none of them.
"""
from __future__ import annotations


# ------------------------------------------------------------------------------
# Replacement fields of f-strings bind names and run code
# ------------------------------------------------------------------------------
def cx_walrus_in_fstring(items, factory):
    f"{(list := factory)}"
    acc = list()
    for x in items:
        acc = acc + [x]
    return len(acc)


def old_walrus_in_fstring(items, factory):
    f"{(list := factory)}"
    acc = list()
    for x in items:
        acc.append(x)
    return len(acc)


def cx_nested_format_spec(items):
    xs = []
    kept = []
    f"{0:{1:{kept.append(xs) or 0}}}"
    for item in items:
        xs = xs + [item]
    return kept[0], xs


def old_nested_format_spec(items):
    xs = []
    kept = []
    f"{0:{1:{kept.append(xs) or 0}}}"
    for item in items:
        xs.append(item)
    return kept[0], xs


# ------------------------------------------------------------------------------
# Definition headers, handler types, and target expressions bind in the enclosing scope
# ------------------------------------------------------------------------------
def cx_walrus_in_default(items, factory):
    def unused(x=(list := factory)):
        return x
    acc = list()
    for x in items:
        acc = acc + [x]
    return len(acc)


def old_walrus_in_default(items, factory):
    def unused(x=(list := factory)):
        return x
    acc = list()
    for x in items:
        acc.append(x)
    return len(acc)


def cx_walrus_in_lambda_default(items, factory):
    unused = lambda x=(list := factory): x
    acc = list()
    for x in items:
        acc = acc + [x]
    return len(acc)


def old_walrus_in_lambda_default(items, factory):
    unused = lambda x=(list := factory): x
    acc = list()
    for x in items:
        acc.append(x)
    return len(acc)


def cx_walrus_in_decorator(items, factory):
    @(lambda fn, f=(list := factory): fn)
    def unused():
        pass
    acc = list()
    for x in items:
        acc = acc + [x]
    return len(acc)


def old_walrus_in_decorator(items, factory):
    @(lambda fn, f=(list := factory): fn)
    def unused():
        pass
    acc = list()
    for x in items:
        acc.append(x)
    return len(acc)


def cx_walrus_in_class_base(items, factory):
    class Unused((list := factory) and object):
        pass
    acc = list()
    for x in items:
        acc = acc + [x]
    return len(acc)


def old_walrus_in_class_base(items, factory):
    class Unused((list := factory) and object):
        pass
    acc = list()
    for x in items:
        acc.append(x)
    return len(acc)


def cx_walrus_in_handler_type(items, factory):
    try:
        raise ValueError
    except (list := factory) and ValueError:
        pass
    acc = list()
    for x in items:
        acc = acc + [x]
    return len(acc)


def old_walrus_in_handler_type(items, factory):
    try:
        raise ValueError
    except (list := factory) and ValueError:
        pass
    acc = list()
    for x in items:
        acc.append(x)
    return len(acc)


def cx_walrus_in_subscript_target(items, factory):
    holder = {}
    holder[(list := factory)] = 0
    acc = list()
    for x in items:
        acc = acc + [x]
    return len(acc)


def old_walrus_in_subscript_target(items, factory):
    holder = {}
    holder[(list := factory)] = 0
    acc = list()
    for x in items:
        acc.append(x)
    return len(acc)


# ------------------------------------------------------------------------------
# Comprehension scopes: assignment expressions bind the enclosing function (a cell)
# ------------------------------------------------------------------------------
def cx_walrus_in_generator_expression(items, shared):
    acc = []
    next(((acc := shared) for _ in (0,)))
    for x in items:
        acc = acc + [x]
    return len(acc)


def old_walrus_in_generator_expression(items, shared):
    acc = []
    next(((acc := shared) for _ in (0,)))
    for x in items:
        acc.append(x)
    return len(acc)


def run(call, fn):
    try:
        return repr(call(fn))
    except Exception as e:  # noqa: BLE001 - the transcript records every outcome
        return type(e).__name__


def with_owned(*make):
    """A call of `f` with fresh caller-owned containers: its result and what became of them."""

    def call(f):
        owned = [m() for m in make]
        return f([1], *owned), owned

    return call


def with_owned_factory(f):
    """A call of `f` with a factory returning a caller-owned list: its result and that list."""
    owned = []
    return f([1], lambda: owned), owned


def main() -> int:
    pairs = [
        (cx_walrus_in_fstring, old_walrus_in_fstring, with_owned_factory),
        (cx_nested_format_spec, old_nested_format_spec, lambda f: f([1])),
        *[
            (cx, old, with_owned_factory)
            for cx, old in [
                (cx_walrus_in_default, old_walrus_in_default),
                (cx_walrus_in_lambda_default, old_walrus_in_lambda_default),
                (cx_walrus_in_decorator, old_walrus_in_decorator),
                (cx_walrus_in_class_base, old_walrus_in_class_base),
                (cx_walrus_in_handler_type, old_walrus_in_handler_type),
                (cx_walrus_in_subscript_target, old_walrus_in_subscript_target),
            ]
        ],
        (cx_walrus_in_generator_expression, old_walrus_in_generator_expression, with_owned(list)),
    ]
    lines = []
    for cx, old, call in pairs:
        original, rewritten = run(call, cx), run(call, old)
        assert original != rewritten, cx.__name__
        lines.append(f"{cx.__name__}: {original}")
    lines.append(f"old rewrites diverge: {len(pairs)}/{len(pairs)}")
    print("\n".join(lines))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
