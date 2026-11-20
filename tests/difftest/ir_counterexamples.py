#!/usr/bin/env python3
"""Shapes whose IR used to hide a binding, an alias, or a path from the Soundness lane, so that
`heki check --fix` (`--include-size-dependent` for the size-dependent rules) proved a rewrite
safe that changes behaviour on CPython. Each `cx_*` function is the original; `old_*` is the
exact text the fix produced for it. `main()` asserts that every old rewrite diverges from its
original, and `crates/heki/tests/e2e_soundness.rs` asserts that heki now reports each original
as `Blocked` or not at all and rewrites none of them. Run it with CPython 3.15.
"""


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


def cx_walrus_in_nested_generator_expressions(items, shared):
    acc = []
    next(next(((acc := shared) for _ in (0,)) for _ in (0,)))
    for x in items:
        acc = acc + [x]
    return len(acc)


def old_walrus_in_nested_generator_expressions(items, shared):
    acc = []
    next(next(((acc := shared) for _ in (0,)) for _ in (0,)))
    for x in items:
        acc.append(x)
    return len(acc)


# ------------------------------------------------------------------------------
# A cell read only by a grandchild scope is still a cell of the function
# ------------------------------------------------------------------------------
def cx_capture_through_function(items):
    acc = []
    def middle():
        return lambda: acc
    old = middle()()
    for x in items:
        acc = acc + [x]
    return old


def old_capture_through_function(items):
    acc = []
    def middle():
        return lambda: acc
    old = middle()()
    for x in items:
        acc.append(x)
    return old


def cx_capture_through_class(items):
    acc = []
    class Holder:
        def get(self):
            return acc
    old = Holder().get()
    for x in items:
        acc = acc + [x]
    return old


def old_capture_through_class(items):
    acc = []
    class Holder:
        def get(self):
            return acc
    old = Holder().get()
    for x in items:
        acc.append(x)
    return old


def cx_capture_through_generator_expression(items):
    acc = []
    old = next((lambda: acc) for _ in (0,))()
    for x in items:
        acc = acc + [x]
    return old


def old_capture_through_generator_expression(items):
    acc = []
    old = next((lambda: acc) for _ in (0,))()
    for x in items:
        acc.append(x)
    return old


# ------------------------------------------------------------------------------
# Function and class objects keep what their definition evaluates
# ------------------------------------------------------------------------------
def cx_default_keeps_value(items):
    acc = []
    def saved(old=acc):
        return old
    for x in items:
        acc = acc + [x]
    return saved()


def old_default_keeps_value(items):
    acc = []
    def saved(old=acc):
        return old
    for x in items:
        acc.append(x)
    return saved()


def cx_keyword_only_default_keeps_value(items):
    acc = []
    def saved(*, old=acc):
        return old
    for x in items:
        acc = acc + [x]
    return saved()


def old_keyword_only_default_keeps_value(items):
    acc = []
    def saved(*, old=acc):
        return old
    for x in items:
        acc.append(x)
    return saved()


def cx_lambda_default_keeps_value(items):
    acc = []
    saved = lambda old=acc: old
    for x in items:
        acc = acc + [x]
    return saved()


def old_lambda_default_keeps_value(items):
    acc = []
    saved = lambda old=acc: old
    for x in items:
        acc.append(x)
    return saved()


def cx_return_annotation_keeps_value(items):
    acc = []
    def tag() -> acc:
        pass
    old = tag.__annotations__["return"]
    for x in items:
        acc = acc + [x]
    return old


def old_return_annotation_keeps_value(items):
    acc = []
    def tag() -> acc:
        pass
    old = tag.__annotations__["return"]
    for x in items:
        acc.append(x)
    return old


def cx_parameter_annotation_keeps_value(items):
    acc = []
    def tag(*args: acc):
        pass
    old = tag.__annotations__["args"]
    for x in items:
        acc = acc + [x]
    return old


def old_parameter_annotation_keeps_value(items):
    acc = []
    def tag(*args: acc):
        pass
    old = tag.__annotations__["args"]
    for x in items:
        acc.append(x)
    return old


def cx_class_annotation_keeps_value(items):
    acc = []
    class Tagged:
        field: acc
    old = Tagged.__annotations__["field"]
    for x in items:
        acc = acc + [x]
    return old


def old_class_annotation_keeps_value(items):
    acc = []
    class Tagged:
        field: acc
    old = Tagged.__annotations__["field"]
    for x in items:
        acc.append(x)
    return old


def cx_type_parameter_bound_keeps_value(items):
    acc = []
    def generic[T: acc]():
        pass
    old = generic.__type_params__[0].__bound__
    for x in items:
        acc = acc + [x]
    return old


def old_type_parameter_bound_keeps_value(items):
    acc = []
    def generic[T: acc]():
        pass
    old = generic.__type_params__[0].__bound__
    for x in items:
        acc.append(x)
    return old


def cx_type_alias_keeps_value(items):
    acc = []
    type Alias = acc
    old = Alias.__value__
    for x in items:
        acc = acc + [x]
    return old


def old_type_alias_keeps_value(items):
    acc = []
    type Alias = acc
    old = Alias.__value__
    for x in items:
        acc.append(x)
    return old


class Recorder:
    seen = []

    def __init_subclass__(cls, tag=None, **kwargs):
        Recorder.seen.append(tag)


def cx_class_keyword_reaches_user_code(items):
    acc = []
    class Tagged(Recorder, tag=acc):
        pass
    for x in items:
        acc = acc + [x]
    return Recorder.seen[-1]


def old_class_keyword_reaches_user_code(items):
    acc = []
    class Tagged(Recorder, tag=acc):
        pass
    for x in items:
        acc.append(x)
    return Recorder.seen[-1]


# ------------------------------------------------------------------------------
# A chained comparison stops at its first false link
# ------------------------------------------------------------------------------
def cx_chained_comparison_short_circuits(items, acc):
    3 < 2 == (acc := [])
    for x in items:
        acc = acc + [x]
    return len(acc)


def old_chained_comparison_short_circuits(items, acc):
    3 < 2 == (acc := [])
    for x in items:
        acc.append(x)
    return len(acc)


def cx_membership_in_chained_comparison(n):
    keys = list(range(9))
    hit = None
    for i in range(n):
        hit = i in keys in keys
    return hit


def old_membership_in_chained_comparison(n):
    keys = list(range(9))
    hit = None
    keys_set = set(keys)
    for i in range(n):
        hit = i in keys in keys_set
    return hit


# ------------------------------------------------------------------------------
# Capture patterns bind the matched component, not the subject
# ------------------------------------------------------------------------------
def cx_sequence_pattern_capture(items, shared):
    match [shared]:
        case [acc]:
            for x in items:
                acc = acc + [x]
            return len(acc)


def old_sequence_pattern_capture(items, shared):
    match [shared]:
        case [acc]:
            for x in items:
                acc.append(x)
            return len(acc)


def cx_as_pattern_capture(items, shared):
    match [shared]:
        case [[] as acc]:
            for x in items:
                acc = acc + [x]
            return len(acc)


def old_as_pattern_capture(items, shared):
    match [shared]:
        case [[] as acc]:
            for x in items:
                acc.append(x)
            return len(acc)


def cx_mapping_pattern_capture(items, shared):
    match {"k": shared}:
        case {"k": acc}:
            for x in items:
                acc = {**acc, x: x}
            return len(acc)


def old_mapping_pattern_capture(items, shared):
    match {"k": shared}:
        case {"k": acc}:
            for x in items:
                acc[x] = x
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
        (
            cx_walrus_in_nested_generator_expressions,
            old_walrus_in_nested_generator_expressions,
            with_owned(list),
        ),
        (cx_capture_through_function, old_capture_through_function, lambda f: f([1])),
        (cx_capture_through_class, old_capture_through_class, lambda f: f([1])),
        (
            cx_capture_through_generator_expression,
            old_capture_through_generator_expression,
            lambda f: f([1]),
        ),
        *[
            (cx, old, lambda f: f([1]))
            for cx, old in [
                (cx_default_keeps_value, old_default_keeps_value),
                (cx_keyword_only_default_keeps_value, old_keyword_only_default_keeps_value),
                (cx_lambda_default_keeps_value, old_lambda_default_keeps_value),
                (cx_return_annotation_keeps_value, old_return_annotation_keeps_value),
                (cx_parameter_annotation_keeps_value, old_parameter_annotation_keeps_value),
                (cx_class_annotation_keeps_value, old_class_annotation_keeps_value),
                (cx_type_parameter_bound_keeps_value, old_type_parameter_bound_keeps_value),
                (cx_type_alias_keeps_value, old_type_alias_keeps_value),
                (cx_class_keyword_reaches_user_code, old_class_keyword_reaches_user_code),
            ]
        ],
        (
            cx_chained_comparison_short_circuits,
            old_chained_comparison_short_circuits,
            with_owned(list),
        ),
        (cx_membership_in_chained_comparison, old_membership_in_chained_comparison, lambda f: f(1)),
        (cx_sequence_pattern_capture, old_sequence_pattern_capture, with_owned(list)),
        (cx_as_pattern_capture, old_as_pattern_capture, with_owned(list)),
        (cx_mapping_pattern_capture, old_mapping_pattern_capture, with_owned(dict)),
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
