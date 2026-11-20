# heki

heki (壁, Japanese for "wall") is a semantic linter for what Python leaves open. It checks **what
code does, not what types it has**: which objects it changes in place, which references it
shares, and which blocking, network, or database work it reaches. It runs next to a linter such
as [Ruff](https://docs.astral.sh/ruff/) and a type checker such as
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
ask for annotations on `sort_values`, but none of them sees that it reorders the caller's list:
the types are right, and what the code does is wrong. heki reports both bugs, with the calls
that lead to them:

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

Found 2 diagnostic(s) in 3 file(s).
```

> **Experimental.** heki is under active development: rules, configuration keys, diagnostics,
> and output formats can change between releases without a deprecation period. Try it on your
> code and report what it gets wrong, but do not depend on its output staying the same yet.

## Why heki?

Types say what values are; heki checks what code does:

- **Aliases and in-place changes:** which objects a function changes, through any alias, helper,
  or subclass.
- **Contracts:** who may write, read, or call what, and which code may reach I/O.
- **Proven rewrites:** quadratic patterns are rewritten only when the rewrite is proven, not
  guessed.

### What code does, not what types it has

Each report shows the chain of calls that leads to the bug.

| What goes wrong | Rule |
| --- | --- |
| A blocking call several helpers below an `async def` | `EFF101` |
| A database query, or a new HTTP client, on every loop iteration | `EFF103`, `EFF102` |
| A `@cached_property` left stale after another method changes what it read | `MUT101` |
| An attribute that `__hash__` reads, changed after the object went into a `set` | `MUT107` |
| A `Sequence` / `Mapping` argument changed in place by a helper | `MUT110` |
| Opt-in: a frozen or `ReadOnly` field changed in place | `MUT108` |
| Opt-in: I/O while a lock or a database transaction is held | `EFF106` |
| Opt-in: a generator consumed twice | `MUT106` |

### Your conventions, checked

Rules you keep today by review, by AST scripts, or by run-time guards in tests become a few lines
of configuration:

```toml
[[tool.heki.write-owners]]
targets = ["app.models.Order.items"]
allowed-writers = ["app.store.*"]
```

heki follows aliases, helpers, and subclasses, with no decorators and nothing running at run
time. The other contracts are `read-owners`, `call-owners`, `readonly-returns`, `invalidation`,
and `effect-boundaries`.

### A safety net for reviews and coding agents

- The language server checks while you edit and re-checks only what an edit can affect.
- `heki snapshot` records what each public function does, and `MUT103` reports a function that gained an
  effect, which a diff does not show.
- Coding agents get the same checks as JSON.

### Fast, quiet, and easy to adopt

- Under a second for a 200,000-line project.
- A report needs evidence: when heki cannot tell, it stays quiet.
- Configuration, suppression comments, and output formats follow Ruff, and a baseline fails CI
  only on new findings.

## What Python leaves open

| What Python leaves open | What other languages enforce | What heki checks |
| --- | --- | --- |
| Any code can change any object it reaches | ownership and borrowing (Rust), `const` / `readonly` | read-only parameters and fields changed through helpers (`MUT110`, `MUT108`); `write-owners` / `read-owners` contracts (`MUT201`, `MUT203`) |
| A signature does not say what a function does | effect types, `IO` in the type (Haskell) | blocking calls reached from `async def`, and clients and queries in loops (`EFF101`–`EFF103`); `effect-boundaries` (`EFF201`); effect snapshots (`MUT103`) |
| Shared mutable objects hide changes | value semantics, immutability by default | stale cached properties and hashes (`MUT101`, `MUT107`); changed shared return values (`MUT202`); writes an `invalidation` contract must announce (`MUT109`) |
| Dynamic dispatch blocks optimizations | static types let a compiler prove a rewrite | quadratic patterns (`OPT`), rewritten only when proven under CPython's semantics |

## How heki differs

The difference is the model, not reaching across files: type checkers already follow types
across modules. heki models effects and aliases, what code does:

| Tool | Models | heki adds |
| --- | --- | --- |
| Ruff | syntax and style | the effects behind a call: a blocking call, a query, or a write, wherever it happens |
| ty, Pyrefly, pyright, mypy | declared types | in-place changes, through aliases and helpers, to `frozen`, `ReadOnly`, and `Final` fields and to `Sequence` / `Mapping` arguments |
| tach, import-linter | which modules may import which | which functions write or read a field, call a function, or reach I/O |

heki leaves to these tools what they cover, and keeps doing so as they grow: use it next to them.

## Rewrites proven under CPython's semantics

The `OPT` rules find quadratic list, dict, set, bytes, and queue patterns. `heki check --fix`
rewrites one only when it proves that the program behaves the same:

- it trusts no type annotation (a subclass can override `__add__`);
- any other reference to the object blocks an in-place update;
- evaluation order and exceptions are kept;
- builtins count only while the module does not rebind them.

The scope analysis is checked against CPython's `symtable` on 3.11 to 3.15, and adversarial
programs that break a naive rewrite must all be blocked on CPython. When a proof fails, the
report names the missing obligation instead of offering a fix.

## Scope

- Its knowledge comes from its own analysis, a catalog of library behavior, contracts you
  declare in `pyproject.toml`, and type annotations, which only help it resolve calls: rewrites
  never trust them.
- When it cannot tell, it stays quiet. Annotations, catalog entries, and contracts close the
  findings it misses.
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
- [Rules](https://heki.koxudaxi.dev/rules/) (30 rules)
- [Fixes and safety](https://heki.koxudaxi.dev/fixes/)
- [Adopting heki in a code base](https://heki.koxudaxi.dev/adoption/)
- [Editors (LSP)](https://heki.koxudaxi.dev/lsp/) and
  [coding agents](https://heki.koxudaxi.dev/coding-agents/)
- [CLI reference](https://heki.koxudaxi.dev/cli-reference/)
- [Development](https://heki.koxudaxi.dev/development/)
- [Changelog](https://github.com/koxudaxi/heki/blob/main/CHANGELOG.md)

## Name

*heki* is 壁, "wall", in its Sino-Japanese (*on*) reading, the one used in compounds such as
防火壁 (*bōkaheki*, "firewall"). On its own the character is usually read *kabe*.

## License

[MIT](https://github.com/koxudaxi/heki/blob/main/LICENSE)
