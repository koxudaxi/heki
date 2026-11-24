from cyc_h7.a import helper_a


def helper_b() -> int:
    return 1


def uses_a() -> int:
    return helper_a()
