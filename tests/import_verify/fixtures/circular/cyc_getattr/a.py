def __getattr__(name: str) -> object:
    return len


from cyc_getattr.b import helper_b


def helper_a() -> int:
    return 1
