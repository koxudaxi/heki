"""Python 3.12-3.15 bindings made inside nested expressions: replacement fields and their format
specifications at any depth."""


def nested_format_spec(xs: list[int]) -> str:
    kept = []
    text = f"{0:{1:{kept.append(xs) or (width := 3)}}}"
    return text + str(width) + str(kept)



def headers_bind_in_the_enclosing_scope(factory: object, holder: dict) -> object:
    @(lambda fn, deco=(by_decorator := factory): fn)
    def unused(x: int = (by_default := 1)) -> int:
        return x

    class Unused((by_base := object)):
        pass

    by_lambda = lambda y=(by_lambda_default := 2): y
    holder[(by_target := 3)] = 0
    try:
        raise ValueError
    except (by_handler := ValueError):
        pass
    return by_decorator, by_default, by_base, by_lambda, by_lambda_default, by_target, by_handler


def walrus_in_generator_expressions(shared: list) -> list:
    acc = []
    next(((acc := shared) for _ in (0,)))
    next(next(((deep := shared) for _ in (0,)) for _ in (0,)))
    return acc + deep

