"""heki frontend syntax tour: `from __future__ import annotations` (annotations are never
evaluated, so no annotation scopes and no annotation reads)."""

from __future__ import annotations

LIMIT: int = 3


def annotated(value: Limit, *args: str, **kwargs: float) -> list[Limit]:
    local: dict[str, Limit] = {}
    return [value, local]


class Annotated:
    field: Limit = 0

    def method(self) -> Annotated:
        return self
