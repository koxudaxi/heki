# heki

heki (壁, Japanese for "wall") checks **what Python code does** across functions and modules: which
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

Found 2 diagnostic(s) in 3 file(s).
```

> **Experimental.** heki is under active development: rules, configuration keys, diagnostics,
> and output formats can change between releases without a deprecation period. Try it on your
> code and report what it gets wrong, but do not depend on its output staying the same yet.

## Why heki?

### Bugs that cross function boundaries

Each report shows the chain of calls that leads to the bug.

| What goes wrong | Rule |
| --- | --- |
| A blocking call several helpers below an `async def` | [`EFF101`](rules/async-blocking-call.md) |
| A database query, or a new HTTP client, on every loop iteration | [`EFF103`](rules/loop-db-query.md), [`EFF102`](rules/loop-unpooled-client.md) |
| A `@cached_property` left stale after another method changes what it read | [`MUT101`](rules/cached-property-stale-mutation.md) |
| An attribute that `__hash__` reads, changed after the object went into a `set` | [`MUT107`](rules/hash-attr-mutation.md) |
| A `Sequence` / `Mapping` argument changed in place by a helper | [`MUT110`](rules/readonly-param-mutation.md) |
| Opt-in: a frozen or `ReadOnly` field changed in place | [`MUT108`](rules/readonly-type-mutation.md) |
| Opt-in: I/O while a lock or a database transaction is held | [`EFF106`](rules/lock-or-tx-held-across-io.md) |
| Opt-in: a generator consumed twice | [`MUT106`](rules/iterator-double-consumption.md) |

### Your conventions, checked

Rules you keep today by review, by AST scripts, or by run-time guards in tests become a few lines
of [configuration](configuration.md):

```toml
[[tool.heki.write-owners]]
targets = ["app.models.Order.items"]
allowed-writers = ["app.store.*"]
```

heki follows aliases, helpers, and subclasses, with no decorators and nothing running at run
time. The other contracts are `read-owners`, `call-owners`, `readonly-returns`, `invalidation`,
and `effect-boundaries`.

### A safety net for reviews and coding agents

- The [language server](lsp.md) checks while you edit and re-checks only what an edit can affect.
- [`heki snapshot`](adoption.md#effect-snapshots-mut103) records what each public function does, and `MUT103` reports a function that gained an
  effect, which a diff does not show.
- [Coding agents](coding-agents.md) get the same checks as JSON.

### Fast, quiet, and easy to adopt

- Under a second for a 200,000-line project.
- A report needs evidence: when heki cannot tell, [it stays quiet](scope.md#when-heki-cannot-tell).
- Configuration, suppression comments, and output formats follow Ruff, and a [baseline](adoption.md#ratchet-baselines) fails CI
  only on new findings.

## How heki differs

| Tool | Checks | heki adds |
| --- | --- | --- |
| Ruff | one file's syntax and style | a blocking call reached through other functions |
| ty, Pyrefly, pyright, mypy | that values match their declared types | what functions do: in-place changes, through aliases and helpers, to `frozen`, `ReadOnly`, and `Final` fields and to `Sequence` / `Mapping` arguments |
| tach, import-linter | which modules may import which | which functions write or read a field, or reach I/O |

heki leaves to these tools what they already cover, so use it next to them.

## Rewrites proven under CPython's semantics

The `OPT` rules find quadratic list, dict, set, bytes, and queue patterns. `heki check --fix`
rewrites one only when it proves that the program behaves the same:

- it trusts no type annotation (a subclass can override `__add__`);
- any other reference to the object blocks an in-place update;
- evaluation order and exceptions are kept;
- builtins count only while the module does not rebind them.

The scope analysis is checked against CPython's `symtable` on 3.11 to 3.15, and adversarial
programs that break a naive rewrite must all be blocked on CPython. When a proof fails, the
report names the missing obligation instead of offering a fix. See [Fixes and safety](fixes.md).

See [Scope and limitations](scope.md) for what heki covers, where its knowledge comes from, and
what it does not do.

See the [rules reference](rules/index.md) for all 30 rules.

## Next steps

- See what heki [covers and does not cover](scope.md).
- [Install heki](installation.md) and [run it on a project](getting-started.md).
- [Configure](configuration.md) rule selection, ownership contracts, and the Python version.
- Read how [fixes are proven safe](fixes.md).
- [Adopt it gradually](adoption.md) with a Ratchet baseline in CI.
- Use it from [your editor](lsp.md) or [a coding agent](coding-agents.md).

## Name

*heki* is 壁, "wall", in its Sino-Japanese (*on*) reading, the one used in compounds such as
防火壁 (*bōkaheki*, "firewall"). On its own the character is usually read *kabe*.
