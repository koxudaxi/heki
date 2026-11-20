"""Python 3.12-3.15 bindings made inside nested expressions: replacement fields and their format
specifications at any depth."""


def nested_format_spec(xs: list[int]) -> str:
    kept = []
    text = f"{0:{1:{kept.append(xs) or (width := 3)}}}"
    return text + str(width) + str(kept)

