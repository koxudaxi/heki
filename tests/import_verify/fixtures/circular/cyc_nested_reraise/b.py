def helper_b() -> int:
    return 2


try:
    try:
        from cyc_nested_reraise.a import helper_a
    except ImportError:
        try:
            raise ModuleNotFoundError("no helper")
        except ModuleNotFoundError:
            raise
except ModuleNotFoundError:
    helper_a = None
