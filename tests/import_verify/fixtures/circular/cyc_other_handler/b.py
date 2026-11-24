def helper_b() -> int:
    return 2


try:
    from cyc_other_handler.a import helper_a
except (KeyError, ModuleNotFoundError):
    helper_a = None
