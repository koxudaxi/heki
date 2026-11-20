"""heki frontend syntax tour: Python 3.12+ syntax (PEP 695 type parameters, PEP 701 f-strings).

Generic classes and type-parameter bounds, constraints and defaults are left out on purpose:
CPython gives them synthetic symbols and lazy scopes of their own, which heki does not model (they
are covered by `crates/heki/tests/e2e_frontend.rs` instead).
"""

type Alias = list[int]
type Pair[K, V] = tuple[K, V]
type Variadic[*Ts, **P] = tuple[*Ts]


def generic_function[T, *Ts, **P](value: T, *args: *Ts) -> T:
    return value


class Plain:
    def generic_method[S](self, value: S) -> S:
        return value


def pep701_fstrings(items, name, width):
    return f"{"-".join(items)}: {name!r:{"<"}{width}}"
