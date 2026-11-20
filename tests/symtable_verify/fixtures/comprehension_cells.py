"""Inlined (PEP 709) comprehension targets: captured only by scopes inside the comprehension."""
x = 7
i = 0


def sibling_closure_reads_the_global():
    [x for x in ()]
    return lambda: x


def closure_inside_captures_the_target(items):
    return [lambda: item for item in items]


def condition_closure_sees_a_later_target(a, b):
    return [y for x in a if (lambda: y) for y in b]


def nested_comprehensions(rows):
    return [[lambda: (cell, row) for cell in row] for row in rows]


def closure_outside_the_inner_comprehension(rows):
    return [(lambda: row, [col for col in row]) for row in rows]


def nested_lambda_inside(items):
    return [lambda: (lambda: item) for item in items]


fs = [lambda: i for i in range(2)]


class Holder:
    gs = [lambda: j for j in range(2)]
