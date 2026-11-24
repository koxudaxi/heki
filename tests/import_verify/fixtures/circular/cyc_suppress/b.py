import contextlib


def helper_b() -> int:
    return 2


with contextlib.suppress(ImportError):
    from cyc_suppress.a import helper_a
