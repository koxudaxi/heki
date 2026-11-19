"""heki frontend syntax tour: Python 3.14+ syntax (PEP 750 template strings, PEP 758)."""


def templates(name, width, precision):
    plain = t"hello {name}"
    spec = t"{name!r:>{width}.{precision}}"
    literal_only = t"no interpolation"
    concatenated = t"{name}" t"-{width}"
    return plain, spec, literal_only, concatenated


def unparenthesized_except(fn):
    try:
        fn()
    except ValueError, TypeError:
        return None
