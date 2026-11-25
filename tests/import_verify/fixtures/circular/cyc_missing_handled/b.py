def helper_b() -> int:
    return 2


try:
    from cyc_missing_handled._speedups import fast
except ModuleNotFoundError:
    fast = None
from cyc_missing_handled.a import helper_a
