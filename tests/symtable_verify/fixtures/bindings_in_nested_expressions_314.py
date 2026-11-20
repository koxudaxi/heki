"""Python 3.14-3.15 bindings made inside template-string replacement fields."""


def walrus_in_template_string(value: object) -> object:
    template = t"{(bound := value)!r:>{(spec := 8)}}"
    return template, bound, spec
