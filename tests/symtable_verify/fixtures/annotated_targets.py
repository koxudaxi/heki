"""Annotated assignment targets: bare, parenthesized, with and without a value."""
x = 3


def parenthesized_without_value():
    (x): int
    return x


def parenthesized_with_value():
    (y): int = 1
    return y


def bare_without_value():
    z: int
    return z


class Holder:
    (attr): int
    (other): int = 2
    obj = None
    (obj.field): int
