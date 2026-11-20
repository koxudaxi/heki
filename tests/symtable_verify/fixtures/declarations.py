"""`global` / `nonlocal` declarations apart from bindings."""

global MODULE_LEVEL_NOOP


def reads_declared_global():
    global ONLY_READ
    return ONLY_READ


def writes_declared_global():
    global WRITTEN
    WRITTEN = 1


def read_only_nonlocal():
    y = 1

    def inner():
        nonlocal y
        return y

    return inner


def comprehension_target_under_global(items):
    global shadowed
    [shadowed for shadowed in items]
    return shadowed


def comprehension_target_under_nonlocal(items):
    target = 0

    def inner():
        nonlocal target
        [target for target in items]
        return target

    return inner


def generator_walrus_under_global():
    global walrus_global
    return (((walrus_global := i), walrus_global) for i in range(2))


def inlined_walrus_under_global():
    global inlined_walrus
    return [((inlined_walrus := i), inlined_walrus) for i in range(2)]


def init_state():
    global module_state
    module_state = 1


init_state()


def get_state():
    return module_state
