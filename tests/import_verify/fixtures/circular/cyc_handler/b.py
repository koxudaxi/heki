try:
    from cyc_handler.a import helper_a
except ImportError:
    helper_a = None


def helper_b() -> int:
    return 2
