"""Python 3.11-3.15 lexical scope fixture (comprehensions, closures, walrus, match, class bodies)."""

GLOBAL_VAL = 10


def outer(a: int, b: int = GLOBAL_VAL) -> int:
    x = a + b
    comp = [i + (last := i) for i in range(x)]

    def inner(y: int) -> int:
        nonlocal x
        x += y + last
        return x + GLOBAL_VAL

    return inner(len(comp))


class Box:
    size = 3
    __secret = 42

    def get_secret(self) -> int:
        return self.__secret

    def match_case(self, obj: object) -> int:
        match obj:
            case {"k": val, **rest}:
                return len(rest) + int(bool(val))
            case [head, *tail]:
                return len(tail) + int(bool(head))
            case _:
                return 0
