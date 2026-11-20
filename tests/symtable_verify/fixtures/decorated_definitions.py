"""Decorated definitions: their scopes start on the `def` / `class` line."""
import functools


@staticmethod
def single():
    pass


@functools.lru_cache(
    maxsize=None,
)
# a comment between the decorator and the definition
async def multi_line():
    pass


@functools.total_ordering
@functools.wraps(single)  # trailing comment
class Decorated:
    @property
    def value(self):
        return 1
