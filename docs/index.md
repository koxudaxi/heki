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


# app/settings.py
from dataclasses import dataclass


@dataclass(frozen=True)
class Settings:
    allowed_hosts: list[str]


def add_host(hosts: list[str], host: str) -> None:
    hosts.append(host)


def for_tenant(settings: Settings, host: str) -> Settings:
    add_host(settings.allowed_hosts, host)  # changes it for every holder
    return settings
```

Ruff (every rule except the docstring rules), ty, Pyrefly, and pyright and mypy in strict mode
report nothing for this code. heki reports both bugs, with the calls that lead to them (`MUT108`
is opt-in):

```console
$ heki check --extend-select MUT108
app/api.py:5:12: EFF101 (async-blocking-call) [error] `async def get_user` calls `app.users.load_user`, which reaches a blocking call `requests.get` (blocking_net) on the event loop; offload it (`asyncio.to_thread`) or use an async API
  |
5 |     return load_user(user_id)  # blocks the event loop two calls down
  |            ^
  = related: app/api.py:5:12: `get_user` calls `app.users.load_user`
  = related: app/users.py:9:41: `load_user` calls `app.users.fetch_name`
  = related: app/users.py:5:12: `fetch_name` calls `requests.get`

app/settings.py:14:5: MUT108 (readonly-type-mutation) [error] `for_tenant` changes the field `Settings.allowed_hosts` of a frozen instance in place via `app.settings.add_host` (indirect_helper): the class promises immutable instances, so they get shared (module-level defaults, caches, the same value handed to several owners), and every holder sees the change and different `==` results; copy it first or store an immutable type such as `tuple` / `frozenset`
   |
14 |     add_host(settings.allowed_hosts, host)  # changes it for every holder
   |     ^
  = related: app/settings.py:14:5: `for_tenant` calls `app.settings.add_host`

Found 2 diagnostic(s) in 4 file(s).
```

> **Experimental.** heki is under active development: rules, configuration keys, diagnostics,
> and output formats can change between releases without a deprecation period. Try it on your
> code and report what it gets wrong, but do not depend on its output staying the same yet.

## Why heki?

- **Bugs that cross function boundaries.** A blocking call several helpers below an
  `async def` ([`EFF101`](rules/async-blocking-call.md)), a database query or an HTTP client
  created on every iteration of a loop ([`EFF103`](rules/loop-db-query.md),
  [`EFF102`](rules/loop-unpooled-client.md)), a `@cached_property` left stale when another
  method changes what it read ([`MUT101`](rules/cached-property-stale-mutation.md)), and an
  attribute `__hash__` reads changing after the object went into a `set`
  ([`MUT107`](rules/hash-attr-mutation.md)). Opt-in rules add frozen dataclasses, `ReadOnly`
  fields, and `Sequence` / `Mapping` arguments changed in place through a helper
  ([`MUT108`](rules/readonly-type-mutation.md)), I/O while a `threading` lock or a database
  transaction is held ([`EFF106`](rules/lock-or-tx-held-across-io.md)), and generators consumed
  twice ([`MUT106`](rules/iterator-double-consumption.md)). A report that crosses functions
  shows its call chain.
- **Conventions turned into checks.** Rules a code base keeps by review, by its own AST scripts
  and allowlists, or by run-time guards in its tests ("only the store writes `Model.fields`",
  "rendering does no I/O") become a few lines of
  [`[tool.heki]` configuration](configuration.md): `write-owners`, `read-owners`,
  `readonly-returns`, and `effect-boundaries`. heki checks them through aliases, helpers, and
  subclasses, with no decorators and nothing running at run time.
- **A safety net for reviews and coding agents.** The [language server](lsp.md) shows the
  findings while you edit and re-checks only what an edit can affect.
  [`heki snapshot`](adoption.md#effect-snapshots-mut103) records what every public function does
  (what it mutates, which I/O it reaches), and `MUT103` reports a function that gained an
  effect, which a diff does not show. [Coding agents](coding-agents.md) get the same checks as
  JSON.
- **Fast, quiet, and easy to adopt.** About 1.5 seconds for a 200,000-line project. A report
  needs evidence, so heki [stays quiet when it cannot tell](scope.md#when-heki-cannot-tell).
  Configuration, suppression comments, and output formats follow Ruff, and a
  [Ratchet baseline](adoption.md#ratchet-baselines) fails CI only on new findings.

## How heki differs

| Tool | Checks | Does not see, and heki reports |
| ---- | ------ | ------------------------------ |
| Ruff | the syntax of one file at a time | a blocking call that is not written in the `async def` itself |
| ty, Pyrefly, pyright, mypy | whether values match their declared types | what a function does, which no annotation describes; in-place changes to `frozen`, `ReadOnly`, and `Final` fields and to `Sequence` / `Mapping` arguments made through aliases or helpers (types stop only reassignment and direct calls) |
| tach, import-linter | which modules may import which | which functions write or read a field, or reach I/O: code allowed to import a module can still change state it does not own |

heki leaves to these tools what they already cover (`EFF107` reports only tasks bound to a
variable, because Ruff's `RUF006` reports the discarded ones), so use it next to them. See
[Scope and limitations](scope.md) for what heki covers, where its knowledge comes from, and
what it does not do.

## Rewrites proven under CPython's semantics

The `OPT` rules find quadratic list, dict, set, bytes, and queue patterns. `heki check --fix`
rewrites one only when a separate analysis proves that the program behaves the same under
CPython's semantics: it trusts no type annotation (a subclass can override `__add__`), any other
reference to the object blocks an in-place update, evaluation order and exceptions are kept, and
builtins count only while the module does not rebind them. Its scope analysis is checked against
CPython's `symtable` on 3.11 to 3.15, and adversarial programs on which a naive rewrite changes
the output are run on CPython and must all be blocked. When a proof fails, the diagnostic names
the obligation instead of offering a fix. See [Fixes and safety](fixes.md).

See the [rules reference](rules/index.md) for all 26 rules.

## Next steps

- See what heki [covers and does not cover](scope.md).
- [Install heki](installation.md) and [run it on a project](getting-started.md).
- [Configure](configuration.md) rule selection, ownership contracts, and the Python version.
- Read how [fixes are proven safe](fixes.md).
- [Adopt it gradually](adoption.md) with a Ratchet baseline in CI.
- Use it from [your editor](lsp.md) or [a coding agent](coding-agents.md).
