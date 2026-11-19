"""Python 3.15 PEP 810 explicit lazy imports fixture."""

import os
lazy import json
from pathlib import Path
lazy from collections.abc import Iterable as Iter


def use_lazy(items: Iter[str]) -> str:
    return json.dumps([str(Path(x)) for x in items] + [os.name])
