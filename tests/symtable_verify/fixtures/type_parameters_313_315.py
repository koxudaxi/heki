"""PEP 696 type parameter defaults: each evaluated lazily in a scope of its own."""


class Base: ...


class Default: ...


type WithDefaults[T: (int, str) = int, *Ts = *tuple[int, ...], **P = [int, str]] = dict[T, P]


def bounded[T: Base = Default](value: T) -> T:
    return value


class Generic[K: Base, V = object](Base):
    def get(self, key: K) -> V:
        return key
