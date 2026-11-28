# heki

<p align="center">
  <img src="assets/heki-banner.png" alt="heki: a snake in front of a brick wall" width="600">
</p>

🧱 heki (壁, Japanese for "wall") checks the semantics of Python code. It does not replace Ruff or
your type checker (mypy, Pyright, Pyrefly, or ty): it is a third layer that runs next to them.
Ruff checks syntax and style, and the type checkers check declared types. heki follows aliases,
helpers, and file boundaries to find which function a call really runs, which state it changes,
and which effects it has. It is written in Rust and checks a 100,000-line project in a few
seconds.

This stops a pull request that passes the syntax checks and strict type checking but changes
what the code does. With no configuration, `heki check` reports a shared cache changed in place,
a read-only argument changed by a helper, a database query in a loop, a lock taken again while it
is held, and a blocking call in `async` code. The same run enforces the rules only your project
knows: who owns a piece of state, which change must invalidate a cache, and where I/O may happen.

You write those rules as [contracts](configuration.md#contracts). A contract uses the results of
the same analysis to decide, in every pull request, whether a change, a call, or an I/O is
allowed. The `async` checks are one use of that analysis among the others.

![heki check in a terminal: MUT114 reports, with no configuration, a dict that an lru_cache function returned and another file changes in place, and MUT201 a write to Order.items through a helper outside the functions a contract allows, with its call chain](assets/example-check.svg)

<p>
  <a class="md-button md-button--primary" href="getting-started/">Getting Started</a>
  <a class="md-button" href="rules/">Rules</a>
  <a class="md-button" href="configuration/#contracts">Contracts</a>
</p>

[![PyPI](https://img.shields.io/pypi/v/heki)](https://pypi.org/project/heki/)
[![Python versions](https://img.shields.io/pypi/pyversions/heki)](https://pypi.org/project/heki/)
[![CI](https://github.com/koxudaxi/heki/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/koxudaxi/heki/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://github.com/koxudaxi/heki/blob/main/LICENSE)

⚡ Try it in your project's root, without installing: `uvx heki check`. Then follow
[Try it in 5 minutes](#try-it-in-5-minutes).

---

### A bug found with no configuration

Each file looks fine on its own, and the types are right: the bug is in what the cache shares.

```python
# app/config.py
from functools import lru_cache


@lru_cache(maxsize=64)
def base_headers(region: str) -> dict[str, str]:
    return {"Accept": "application/json", "X-Region": region}
```

```python
# app/api.py
from app.config import base_headers


def request_headers(region: str, token: str) -> dict[str, str]:
    headers = base_headers(region)
    headers["Authorization"] = f"Bearer {token}"
    return headers
```

`lru_cache` hands the same dict to every call with the same `region`. `request_headers` writes a
token into it, so a later caller of `base_headers` for that region sends the token of whoever
called `request_headers` last.

### A wall your project writes

Only the functions in `app.store` may change `Order.items`. Write that down once:

```toml
# heki.toml
[[write-owners]]
name = "order-store"
targets = ["app.models.Order.items"]
allowed-writers = ["app.store.*"]
message = "change Order.items through app.store"
fail = true   # a report of this contract fails the run
```

```python
# app/models.py
class Order:
    def __init__(self) -> None:
        self.items: list[str] = []
```

```python
# app/store.py
from app.models import Order


def add_item(order: Order, item: str) -> None:
    order.items.append(item)
```

`app/checkout.py` changes the list through a helper in another file:

```python
# app/util.py
def append_to(items: list[str], item: str) -> None:
    items.append(item)
```

```python
# app/checkout.py
from app.models import Order
from app.util import append_to


def add_gift_wrap(order: Order) -> None:
    append_to(order.items, "gift-wrap")
```

tach and import-linter stop at the import layers, and every import here is allowed; heki follows
the write through the helper, as it follows any change, call, or I/O across helpers and aliases.

The image at the top is `heki check` on these examples: `MUT114` with no configuration, and
`MUT201` from the contract. The same code passes Ruff, and mypy, Pyright, Pyrefly, and ty in
strict mode (`mypy --strict`, Pyright's `strict`, Pyrefly's `strict` preset, every ty rule as an
error). Ruff with every rule on asks only for docstrings, copyright lines, and an `__init__.py`.

### Works alongside Ruff, your type checker, and import-linter

heki does not replace these tools: run it next to them. Each one models one layer of the code,
and heki checks what the code does across files.

| Tool | Models | heki adds |
| --- | --- | --- |
| Ruff | syntax and style | the behavior behind a call, across files: the state it changes, a shared cache changed in place, a database query in a loop, a lock taken again while it is held, I/O where it should not happen, a blocking call in `async` code |
| mypy, Pyright, Pyrefly, ty | declared types | in-place changes, through aliases and helpers, to `frozen`, `ReadOnly`, and `Final` fields and to `Sequence` / `Mapping` / `AbstractSet` arguments |
| tach, import-linter | the import layers: which modules may import which | which functions write or read a field, call a function, or reach I/O, and which changes must invalidate a cache, through helpers and aliases; import rules that know when each import runs (experimental); planned: reading these tools' contracts as heki contracts |

Its configuration, suppression comments, and output formats follow Ruff's.
[Design Philosophy](philosophy.md) says why heki works this way and compares its checks with what
Rust and Haskell enforce.

## 🧭 What heki can do

### ✅ Bug checks with no configuration

These rules run by default, and each report shows the chain of calls that leads to it. Every
report they made on 378 packages (about 30.75 million lines) and on a frozen holdout of 100 more
was checked by hand, with no false claim ([Rule precision](precision.md#certifications)).

| What goes wrong | Rule |
| --- | --- |
| A list, dict, or set that an `lru_cache` / `cache` function returned, changed in place | [`MUT114`](rules/cached-result-mutation.md) |
| A `Sequence` / `Mapping` / `AbstractSet` argument changed in place by a helper | [`MUT110`](rules/readonly-param-mutation.md) |
| A change of a builtin container that is lost because the object is new and nothing holds it (`list(xs).sort()`) | [`MUT119`](rules/change-lost-on-new-object.md) |
| A database statement on every iteration of a loop over data | [`EFF103`](rules/loop-db-query.md) |
| A new HTTP, database, or RPC client (or a request without a session) on every iteration of a loop over data | [`EFF102`](rules/loop-unpooled-client.md) |
| A lock that is not reentrant, taken again while it is held, so the thread or task waits for itself | [`EFF109`](rules/lock-reacquired-while-held.md) |
| A call of a generator function whose generator nothing iterates, so its body never runs | [`EFF116`](rules/generator-never-iterated.md) |
| A blocking call that a call in an `async def` always runs, several helpers below | [`EFF101`](rules/async-blocking-call.md) |
| A DataFrame or array concatenated with itself on every loop iteration | [`DATA101`](rules/loop-carried-concat.md) |
| A list rebuilt, or a dict or set merged, on every loop iteration, and a nested-loop join | `OPT101`, `OPT102`, `OPT113` ([fixes](fixes.md)) |

The size-dependent rewrites (`OPT103`, `OPT110`, `OPT111`, `OPT120`, `OPT121`, `OPT130`) are exact
too, but opt-in. The other rules are heuristic or opt-in ([Rules](rules/index.md)).

`heki check --fix` applies an `OPT` rewrite only when it proves, under CPython's semantics, that
the program behaves the same ([Fixes and safety](fixes.md)). More by problem:
[async and execution effects](effects.md), [state ownership and mutation](ownership.md), and
[imports and module boundaries](imports.md).

### 📜 Walls you write: contracts and coding agents

Write the rules your project keeps by review today as contract tables in `heki.toml` or under
`[tool.heki]`. heki follows aliases, helpers, and subclasses, with nothing added at run time.
Contract reports are heuristic, so they are warnings until the table sets `fail = true`.

| What you want to keep | Contract | Main keys |
| --------------------- | -------- | --------- |
| Only `app.store` changes `Order.items` | `write-owners` | `targets = ["Order.items"]`, `allowed-writers = ["app.store.*"]` |
| Only `app.billing` reads `Order.card_token` | `read-owners` | `targets = ["Order.card_token"]`, `allowed-readers = ["app.billing.*"]` |
| Only `app.admin` calls `app.db.reset` | `call-owners` | `targets = ["app.db.reset"]`, `allowed-callers = ["app.admin.*"]` |
| No caller changes the settings `load_settings` returns to everyone | `readonly-returns` | `producers = ["app.config.load_settings"]`, `deep = true` |
| Every change of `Order.items` is followed by `order_changed()` | `invalidation` | `targets = ["Order.items"]`, `invalidators = ["app.cache.order_changed"]` |
| Code in `app.render` makes no blocking call | `effect-boundaries` | `scopes = ["app.render.*"]`, `forbid = ["blocking"]` |
| `app.domain` imports nothing from `app.web`, even lazily or inside a function | `import-boundaries` (experimental) | `importers = ["app.domain.*"]`, `forbidden-imports = ["app.web", "app.web.*"]` |

See [Contracts](configuration.md#contracts) for every key. A coding agent can miss a rule written
in its instructions, but not a contract that `heki check` enforces:

- List `heki.toml` and `heki-baseline.toml` in `CODEOWNERS`: moving a wall takes a review.
- Run `heki check --report-suppressed <PATH>` in CI: it lists every inline `# heki: ignore` and `# noqa`
  that silences a report, so a change that goes around a wall shows in review ([details](adoption.md#auditing-inline-suppressions)).
- A contract's `message` starts the help line of its reports, so the agent reads it as the fix.
- A [Stop hook](coding-agents.md#a-stop-hook-for-claude-code) sends a failing check back before
  the agent ends its turn.
- [`heki inspect`](coding-agents.md#what-a-function-does-heki-inspect-experimental)
  (experimental) shows what one function reaches, changes, and returns.
- [`heki recommend --focus types --diff`](recommend.md#applying-the-suggestions) (experimental) prints the exact type suggestions
  as one patch that `git apply` takes, annotations only.
- `--output-format json` gives every check as JSON, and the
  [Agent Skill](coding-agents.md#agent-skill-experimental) (experimental) teaches the routine
  after an edit. [Coding agents](coding-agents.md) has the whole setup.

## 🚀 Try it in 5 minutes

1. In your project's root (the directory with `pyproject.toml`), run `uvx heki check`. An exact
   error, like `MUT114` above, fails the run (exit code 1). Warnings are printed but don't fail it
   unless you pass `--fail-on warning`.

2. Ask what fits the project with [`heki recommend`](recommend.md) (experimental). It writes no
   file and helps most when the first step finds nothing, as on heki's
   [sample service](https://github.com/koxudaxi/heki/tree/main/tests/sample_service):

    ```console
    $ uvx heki check
    All checks passed (4 file(s)).
    ...
    $ uvx heki recommend
    ...
    Contract candidates
      write-owners  app.models:Order.*  high
        written by 1 function(s), all in app.store (add_item), changed in place; read in 2 other module(s)
    ```

    Only `app.store` writes `Order.items`, so heki proposes that wall with the keys to add.
    [Getting Started](getting-started.md#ask-what-fits-the-project) shows more of the report.

3. Keep it running with the [pre-commit hook](adoption.md#pre-commit) and a CI step, and record
   today's findings in a [Ratchet baseline](adoption.md#ratchet-baselines) so only new ones fail:
   `uvx heki==0.11.0 check --baseline heki-baseline.toml --update-baseline`. The
   [starter recipes](adoption.md#starter-recipes) have these setups ready to copy.

!!! warning "Pre-release"

    Rules, configuration keys, and output formats can change in any release, and the release
    notes list every change. The default rules are exact. Heuristic rules, experimental rules, and
    experimental commands run only when you ask for them, and every output labels heuristic
    reports. Please [report](https://github.com/koxudaxi/heki/issues) what heki gets wrong.

## 👉 Next steps

- 🎯 When heki isn't sure, it doesn't report: [scope and limitations](scope.md).
- 📦 [Install heki](installation.md), 🚀 [run it on a project](getting-started.md), and ⚙️
  [configure](configuration.md) rules and contracts.
- 📏 Browse the [rules](rules/index.md) and [how precise each one is](precision.md), and 🪜
  [adopt heki gradually](adoption.md).
- 🖥️ Use it from [your editor](lsp.md) or 🤖 [a coding agent](coding-agents.md).

## 📛 Name

*heki* is 壁, "wall", in its Sino-Japanese (*on*) reading, the one used in compounds such as
防火壁 (*bōkaheki*, "firewall"). On its own the character is usually read *kabe*.
