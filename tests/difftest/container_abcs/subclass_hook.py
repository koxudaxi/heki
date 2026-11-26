"""A `__subclasshook__` of a project subclass of `Mapping` may accept any class: an ABC's subclass
check asks the hooks of its subclasses. Iterating it after the write runs that class's code,
which removes the cached value: `MUT101` and `MUT111` stay heuristic."""

from collections.abc import Mapping
from functools import cached_property


class Holder:
    def __init__(self, raw: dict[str, str]) -> None:
        self.raw = raw

    @cached_property
    def features(self) -> str:
        return self.raw.get("k", "")

    def replace(self, raw: dict[str, str], rows: Mapping[str, str]) -> None:
        self.raw = raw
        for _ in rows:
            pass


def read(holder: Holder) -> str:
    return holder.features


HOLDER = Holder({"k": "old"})


def forget() -> None:
    HOLDER.__dict__.pop("features", None)


class Hooked(Mapping):
    @classmethod
    def __subclasshook__(cls, other: type) -> bool:
        return hasattr(other, "__iter__")


class Rows:
    def __iter__(self):
        forget()
        return iter(())


def main() -> None:
    rows = Rows()
    assert isinstance(rows, Mapping)
    assert read(HOLDER) == "old"
    HOLDER.replace({"k": "new"}, rows)
    # The value cached before the write is gone when the writer returns.
    assert "features" not in HOLDER.__dict__
    assert read(HOLDER) == "new"


if __name__ == "__main__":
    main()
