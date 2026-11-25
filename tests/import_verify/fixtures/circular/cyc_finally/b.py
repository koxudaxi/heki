def helper_b() -> int:
    return 2


try:
    from cyc_finally.a import helper_a
finally:
    pass
