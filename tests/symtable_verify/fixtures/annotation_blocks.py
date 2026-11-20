"""Variable annotations: one `__annotate__` scope per block (3.14+), in place before."""
import sys

x: int
if sys:
    y: str
else:
    z: (lambda: bytes)


def defined_between():
    pass


class Annotated:
    def method(self) -> None:
        pass

    if sys:
        a: int
    else:
        b: (lambda: bytes)


def local_annotations(items):
    first: int = 0
    if items:
        second: (lambda: str) = ""
    third: [item for item in range(2)]
    return first, second

