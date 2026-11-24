import cyc_import.a


def helper_b() -> int:
    return 1


def uses_a() -> int:
    return cyc_import.a.helper_a()
