"""Python 3.12-3.15 lexical scope tricky forms (PEP 709, closures, class bodies, walrus, match, PEP 695)."""

GLOBAL_X = 1


def closure_and_nonlocal(param: int) -> int:
    captured = param + 1

    def inner(y: int) -> int:
        nonlocal captured
        captured += y
        return captured + GLOBAL_X

    return inner(2)


def pep709_comprehensions(items: list[int]) -> tuple[list[int], dict[int, int], set[int]]:
    # In Python 3.12+, list/dict/set comprehensions are inlined into the enclosing scope,
    # whereas generator expressions retain their own scope.
    xs = [x * 2 for x in items]
    ds = {k: k + 1 for k in items}
    ss = {v for v in items if (w := v) > 0}
    gen = (z + w for z in items)
    return xs, ds, ss | set(gen)


class OuterClass:
    # Class body names are NOT visible to nested method scopes directly.
    class_var = 10
    comp_in_class = [class_var + i for i in range(3)]

    class NestedClass:
        def __init__(self, value: int) -> None:
            self.value = value

    def method(self, x: int) -> int:
        match x:
            case int() as matched if matched > 0:
                return matched + GLOBAL_X
            case _:
                return 0


def global_mutation() -> None:
    global GLOBAL_X
    GLOBAL_X += 1


def pep695_generic[T](x: T) -> T:
    return x
