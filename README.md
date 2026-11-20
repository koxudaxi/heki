# heki

heki (Japanese for "wall") checks **what Python code does** across functions and modules: which
objects it mutates, which references it shares, and which blocking, network, or database work
it reaches. It is the third layer of static analysis, next to a linter such as
[Ruff](https://docs.astral.sh/ruff/) and a type checker such as
[ty](https://github.com/astral-sh/ty): one Rust binary with a CLI and a language server.

```python
# app/users.py
import requests


def fetch_name(user_id: int) -> str:
    return requests.get(f"https://api.example.com/users/{user_id}", timeout=5).text


def load_user(user_id: int) -> dict[str, str]:
    return {"id": str(user_id), "name": fetch_name(user_id)}


# app/api.py
from app.users import load_user


async def get_user(user_id: int) -> dict[str, str]:
    return load_user(user_id)  # blocks the event loop two calls down


# app/stats.py
from collections.abc import Sequence


def sort_values(values):
    values.sort()


def median(values: Sequence[float]) -> float:
    sort_values(values)  # reorders the caller's list
    return values[len(values) // 2]
```

Ruff (every rule except the docstring and annotation rules), ty, Pyrefly, and pyright and mypy
in their default modes report nothing for this code. The annotation rules and the strict modes
ask for annotations on `sort_values`, but none of them sees that it reorders the caller's list.
heki reports both bugs, with the calls that lead to them:

```console
$ heki check
app/api.py:5:12: EFF101 (async-blocking-call) [error] `async def get_user` calls `app.users.load_user`, which reaches a blocking call `requests.get` (blocking_net) on the event loop; offload it (`asyncio.to_thread`) or use an async API
  |
5 |     return load_user(user_id)  # blocks the event loop two calls down
  |            ^
  = related: app/api.py:5:12: `get_user` calls `app.users.load_user`
  = related: app/users.py:9:41: `load_user` calls `app.users.fetch_name`
  = related: app/users.py:5:12: `fetch_name` calls `requests.get`

app/stats.py:9:5: MUT110 (readonly-param-mutation) [error] `median` passes `values` to `app.stats.sort_values`, which changes it in place: `values` is annotated read-only (`Sequence` / `Mapping`), so the caller was promised that its argument is only read, but the helper changes the caller's object; pass a copy (`list(values)` / `dict(values)`), or annotate the helper's parameter with a mutable type so that the type checker shows the mismatch
  |
9 |     sort_values(values)  # reorders the caller's list
  |     ^
  = related: app/stats.py:9:5: `median` calls `app.stats.sort_values`

Found 2 diagnostic(s) in 4 file(s).
```

> **Experimental.** heki is under active development: rules, configuration keys, diagnostics,
> and output formats can change between releases without a deprecation period. Try it on your
> code and report what it gets wrong, but do not depend on its output staying the same yet.

## Why heki?

- **Bugs that cross function boundaries.** A blocking call several helpers below an
  `async def` (`EFF101`), a database query or an HTTP client created on every iteration of a
  loop (`EFF103`, `EFF102`), a `@cached_property` left stale when another method changes what
  it read (`MUT101`), an attribute `__hash__` reads changing after the object went into a `set`
  (`MUT107`), and a `Sequence` / `Mapping` argument changed in place by a helper (`MUT110`).
  Opt-in rules add frozen dataclass and `ReadOnly` fields changed in place (`MUT108`), I/O while
  a `threading` lock or a database transaction is held (`EFF106`), and generators consumed twice
  (`MUT106`). A report that crosses functions shows its call chain.
- **Conventions turned into checks.** Rules a code base keeps by review, by its own AST scripts
  and allowlists, or by run-time guards in its tests ("only the store writes `Model.fields`",
  "rendering does no I/O") become a few lines of `[tool.heki]` configuration: `write-owners`,
  `read-owners`, `readonly-returns`, and `effect-boundaries`. heki checks them through aliases,
  helpers, and subclasses, with no decorators and nothing running at run time.
- **A safety net for reviews and coding agents.** The language server shows the findings while
  you edit and re-checks only what an edit can affect. `heki snapshot` records what every public
  function does (what it mutates, which I/O it reaches), and `MUT103` reports a function that
  gained an effect, which a diff does not show. Agents get the same checks as JSON.
- **Fast, quiet, and easy to adopt.** About 1.5 seconds for a 200,000-line project. A report
  needs evidence, so heki stays quiet when it cannot tell. Configuration, suppression comments,
  and output formats follow Ruff, and a Ratchet baseline fails CI only on new findings.

## How heki differs

| Tool | Checks | Does not see, and heki reports |
| ---- | ------ | ------------------------------ |
| Ruff | the syntax of one file at a time | a blocking call that is not written in the `async def` itself |
| ty, Pyrefly, pyright, mypy | whether values match their declared types | what a function does, which no annotation describes; in-place changes to `frozen`, `ReadOnly`, and `Final` fields and to `Sequence` / `Mapping` arguments made through aliases or helpers (types stop only reassignment and direct calls) |
| tach, import-linter | which modules may import which | which functions write or read a field, or reach I/O: code allowed to import a module can still change state it does not own |

heki leaves to these tools what they already cover (`EFF107` reports only tasks bound to a
variable, because Ruff's `RUF006` reports the discarded ones), so use it next to them.

## Rewrites proven under CPython's semantics

The `OPT` rules find quadratic list, dict, set, bytes, and queue patterns. `heki check --fix`
rewrites one only when a separate analysis proves that the program behaves the same under
CPython's semantics: it trusts no type annotation (a subclass can override `__add__`), any other
reference to the object blocks an in-place update, evaluation order and exceptions are kept, and
builtins count only while the module does not rebind them. Its scope analysis is checked against
CPython's `symtable` on 3.11 to 3.15, and adversarial programs on which a naive rewrite changes
the output are run on CPython and must all be blocked. When a proof fails, the diagnostic names
the obligation instead of offering a fix.

## Scope

- heki follows effects and aliases through calls. It is neither a linter nor a type checker
  and leaves their checks to them.
- Its knowledge comes from its own analysis, a catalog of library behavior, contracts you
  declare in `pyproject.toml`, and type annotations, which only help it resolve calls: rewrites
  never trust them.
- When it cannot tell, it stays quiet: a rewrite needs a proof and a report needs evidence. The
  price is missed findings, which annotations, catalog entries, and contracts close.
- It does not model `__getattr__` proxies, metaclasses, or framework machinery, and it never
  runs your code.

See [Scope and limitations](https://heki.koxudaxi.dev/scope/) for the details.

## Installation

```console
$ uv tool install heki      # or: pipx install heki / pip install heki
$ uvx heki check            # run without installing
```

## Usage

```console
$ heki check                                    # check the project
$ heki check --diff                             # preview proven-safe fixes
$ heki check --fix                              # apply them
$ heki check --baseline heki-baseline.toml      # fail only on new findings
$ heki rule EFF101                              # explain a rule
$ heki server                                   # LSP server for editors
```

## Documentation

- [Getting started](https://heki.koxudaxi.dev/getting-started/)
- [Scope and limitations](https://heki.koxudaxi.dev/scope/)
- [Configuration](https://heki.koxudaxi.dev/configuration/)
- [Rules](https://heki.koxudaxi.dev/rules/) (27 rules)
- [Fixes and safety](https://heki.koxudaxi.dev/fixes/)
- [Adopting heki in a code base](https://heki.koxudaxi.dev/adoption/)
- [Editors (LSP)](https://heki.koxudaxi.dev/lsp/) and
  [coding agents](https://heki.koxudaxi.dev/coding-agents/)
- [CLI reference](https://heki.koxudaxi.dev/cli-reference/)
- [Development](https://heki.koxudaxi.dev/development/)
- [Changelog](https://github.com/koxudaxi/heki/blob/main/CHANGELOG.md)

## License

[MIT](https://github.com/koxudaxi/heki/blob/main/LICENSE)
