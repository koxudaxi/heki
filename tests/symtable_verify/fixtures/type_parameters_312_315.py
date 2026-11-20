"""PEP 695 type parameters: lazy bounds and constraints, and `type` statement values."""


class Base: ...


class Meta(type): ...


type Plain = list[int]
type Bounded[T: Base] = T
type Constrained[T: (int, str), *Ts, **P] = dict[T, P]


def bounded[T: Base](value: T) -> T:
    return value


class Generic[K: Base, V](Base, metaclass=Meta):
    def get(self, key: K) -> V:
        return key
