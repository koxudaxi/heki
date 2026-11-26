"""A `register` call whose class heki cannot name may make any class a `Mapping`. Iterating it
after the write runs that class's code, which removes the cached value: `MUT101` and `MUT111`
stay heuristic."""

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


def iterate(self):
    forget()
    return iter(())


def make_rows() -> type:
    return type("Rows", (), {"__iter__": iterate, "__len__": lambda self: 0})


ROWS = make_rows()
Mapping.register(ROWS)


def main() -> None:
    rows = ROWS()
    assert isinstance(rows, Mapping)
    assert read(HOLDER) == "old"
    HOLDER.replace({"k": "new"}, rows)
    # The value cached before the write is gone when the writer returns.
    assert "features" not in HOLDER.__dict__
    assert read(HOLDER) == "new"


if __name__ == "__main__":
    main()
