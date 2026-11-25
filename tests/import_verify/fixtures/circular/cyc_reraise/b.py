def helper_b() -> int:
    return 2


try:
    from cyc_reraise.a import helper_a
except ImportError:
    raise
