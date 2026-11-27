# heki

<p align="center">
  <img src="assets/heki-banner.png" alt="heki: a snake in front of a brick wall" width="600">
</p>

🧱 heki (壁, Japanese for "wall") is a static checker for Python, written in Rust. It tracks side
effects and mutations across function calls, and checks a 100,000-line project in a few seconds.

Ruff checks syntax and style, and ty, mypy, and Pyright check types. heki checks what your code
does: which effects a call reaches and which state it changes, across functions and files.

Out of the box, it catches blocking calls in `async` code, database queries in loops, quadratic
loops, and in-place changes to read-only parameters and to `lru_cache` results. You can also write
your project's own rules as [contracts](configuration.md#contracts), such as which functions may
change a field or reach I/O.

![heki check in a terminal: EFF101 reports a blocking call two helpers below an async def, and EFF103 a database query one helper below a loop, each with its call chain](assets/example-check.svg)

<p>
  <a class="md-button md-button--primary" href="getting-started/">Getting Started</a>
  <a class="md-button" href="rules/">Rules</a>
  <a class="md-button" href="configuration/">Configuration</a>
</p>

[![PyPI](https://img.shields.io/pypi/v/heki)](https://pypi.org/project/heki/)
[![Python versions](https://img.shields.io/pypi/pyversions/heki)](https://pypi.org/project/heki/)
[![CI](https://github.com/koxudaxi/heki/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/koxudaxi/heki/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://github.com/koxudaxi/heki/blob/main/LICENSE)

⚡ Try it in your project's root, without installing: `uvx heki==0.9.0 check`. Then follow
[Try it in 5 minutes](#try-it-in-5-minutes).

---

### A blocking call two helpers below an `async def`

```python
# app/users.py
import requests

def fetch_name(user_id: int) -> str:
    return requests.get(f"https://api.example.com/users/{user_id}", timeout=5).text

def load_user(user_id: int) -> dict[str, str]:
    return {"id": str(user_id), "name": fetch_name(user_id)}
```

```python
# app/api.py
from app.users import load_user

async def get_user(user_id: int) -> dict[str, str]:
    return load_user(user_id)
```

```console
$ heki check
app/api.py:5:12: EFF101 (async-blocking-call) [error] `app.users.load_user` always runs the waiting call `requests.get` on the event loop (2 calls down, on every path)
  |
5 |     return load_user(user_id)
  |            ^
  = related: app/api.py:5:12: `get_user` calls `app.users.load_user`
  = related: app/users.py:8:41: `load_user` calls `app.users.fetch_name`
  = related: app/users.py:5:12: `fetch_name` calls `requests.get`
  = help: offload it with `asyncio.to_thread`, or use an async client

Found 1 diagnostic(s) in 2 file(s).
note: 5 heuristic rule(s) are off; enable them with `--heuristic` or `lint.heuristic = true`
EFF101 (async-blocking-call): `heki rule EFF101`, https://heki.koxudaxi.dev/rules/async-blocking-call/
```

### A query one helper below a loop

```python
# app/orders.py
import sqlite3

def customer_name(cursor: sqlite3.Cursor, customer_id: int) -> str:
    cursor.execute("SELECT name FROM customers WHERE id = ?", (customer_id,))
    return str(cursor.fetchone()[0])

def order_summaries(cursor: sqlite3.Cursor, orders: list[tuple[int, int]]) -> list[str]:
    lines: list[str] = []
    for order_id, customer_id in orders:
        lines.append(f"#{order_id} {customer_name(cursor, customer_id)}")
    return lines
```

```console
$ heki check
app/orders.py:11:37: EFF103 (loop-db-query) [warning] every iteration over parameter `orders` sends a database statement: `sqlite3.Cursor.execute` (through `app.orders.customer_name`)
   |
11 |         lines.append(f"#{order_id} {customer_name(cursor, customer_id)}")
   |                                     ^
  = related: app/orders.py:11:37: `order_summaries` calls `app.orders.customer_name`
  = related: app/orders.py:5:5: `customer_name` calls `sqlite3.Cursor.execute`
  = help: batch the queries, or load the data once before the loop

Found 1 diagnostic(s) in 1 file(s).
heki check does not fail on 1 report(s) shown now: warnings, and heuristic reports, which can be wrong about the code and count as warnings; `fail-on = "warning"` in the lint settings makes them fail it.
note: 5 heuristic rule(s) are off; enable them with `--heuristic` or `lint.heuristic = true`
EFF103 (loop-db-query): `heki rule EFF103`, https://heki.koxudaxi.dev/rules/loop-db-query/
```

Ruff, ty, Pyrefly, pyright, and mypy report nothing for either example with their default settings.
Strict settings don't help either: the second example is fully annotated and passes Ruff's `ANN`
rules, `mypy --strict`, pyright's strict mode, and Pyrefly's `strict` preset. It still sends one
query per order.

### How heki differs

| Tool | Models | heki adds |
| --- | --- | --- |
| Ruff | syntax and style | the effects behind a call: a blocking call, a query, or a write, wherever it happens |
| ty, Pyrefly, pyright, mypy | declared types | in-place changes, through aliases and helpers, to `frozen`, `ReadOnly`, and `Final` fields and to `Sequence` / `Mapping` / `AbstractSet` arguments |
| tach, import-linter | which modules may import which | which functions write or read a field, call a function, or reach I/O; import rules that know when each import runs (experimental); planned: reading these tools' contracts as heki contracts |

heki leaves syntax, style, and types to these tools, so use it next to them. Its configuration,
suppression comments, and output formats follow Ruff's.
[What Python leaves open](#what-python-leaves-open) compares its checks with what Rust and Haskell
enforce.

### Try it in 5 minutes

1. In your project's root (the directory with `pyproject.toml`), run `uvx heki==0.9.0 check`. An
   exact error, like the first example above, fails the run (exit code 1). Warnings, like the
   second, are printed but don't fail it unless you pass `--fail-on warning`.

2. Ask what fits the project with [`heki recommend`](recommend.md) (experimental). It writes no
   file, and it helps most when the first step finds nothing, as on the
   [sample service](https://github.com/koxudaxi/heki/tree/main/tests/sample_service) in heki's
   tests:

    ```console
    $ uvx heki==0.9.0 check
    All checks passed (4 file(s)).
    ...
    hint: `heki recommend` (experimental) lists the parameter types and contracts the code already keeps, the rules that fit this project, and what heki could not see
    $ uvx heki==0.9.0 recommend
    heki recommend (experimental) on .: 4 file(s), 11 function(s)
    ...
    Types the code already keeps
      1 read-only parameter group(s), exact; 0 heuristic
      No type checker configuration found: half of each guard is a type checker's.
        app/report.py:8  total(prices)  list[float] -> Sequence[float]  public
    ...
    What heki cannot see (no report either way)
      calls of packages heki's catalog does not cover: 1 call(s) of 1 name(s) in 1 package(s)
        name                  calls  async  loops  if it waits     if it sends a query
        stripe.Charge.create      1      1      0  EFF101 1 exact  -
    ...
    Contract candidates
      write-owners  app.models:Order.*  high
        written by 1 function(s), all in app.store (add_item), changed in place; read in 2 other module(s)
    ...
    ```

    `total` only reads `prices`, so its annotation can say so. heki's catalog doesn't say what
    `stripe.Charge.create` does, so heki reports nothing through it. If the call waits on the
    network, `EFF101` would report it in `async def checkout`. Only `app.store` writes
    `Order.items`, so the report ends with the keys that make that a contract.

3. Keep it running with the [pre-commit hook](adoption.md#pre-commit) and a CI step. If the project
   has findings today, record them in a [Ratchet baseline](adoption.md#ratchet-baselines) so only
   new ones fail: `uvx heki==0.9.0 check --baseline heki-baseline.toml --update-baseline`, then
   `baseline = "heki-baseline.toml"` in `[tool.heki]`. The
   [starter recipes](adoption.md#starter-recipes) have these setups ready to copy.

!!! warning "Pre-release"

    Rules, configuration keys, diagnostics, and output formats can change between releases without a
    deprecation period, and the release notes list every change. Pin the version (`heki==0.9.0`, and
    the hook's `rev`) so upgrades happen only when you choose. The default rules are exact and ready
    for pull request checks. Heuristic rules, including the contract rules, can be wrong about the
    code. Every output labels their reports, and they count as warnings. `heki recommend`,
    `heki inspect`, `heki skill`, and `heki check --generate-prompt` are experimental, as are the
    rules marked experimental. Please [report](https://github.com/koxudaxi/heki/issues) what heki
    gets wrong on your code.

## 🧭 What heki can do

| Area | What it covers |
| --- | --- |
| [State ownership and mutation](ownership.md) | Changes to read-only parameters, `lru_cache` results, and frozen fields, and contracts for who may write, read, or call what |
| [Async, I/O, and execution effects](effects.md) | Blocking calls in `async def`, queries and clients in loops, lock hazards, effect boundaries, and effect snapshots |
| [Imports and module boundaries](imports.md) | Import cycles, import-order crashes, test and development code loaded by a package, and import boundaries (experimental) |
| [Proven performance rewrites](fixes.md) | `OPT` findings, and the rewrites `heki check --fix` applies only after proving them |
| [Function inspection and configuration advice](recommend.md) | What one function does with `heki inspect`, and the rules and contracts that fit a project with `heki recommend` (both experimental) |
| [Coding-agent architecture walls](coding-agents.md) | Contracts, a Stop hook, JSON output, and an Agent Skill that keep an agent inside your architecture |

## ✅ Exact rules

These rules run by default. Each report shows the chain of calls that leads to it.

| What goes wrong | Rule |
| --- | --- |
| A blocking call that a call in an `async def` always runs, several helpers below | [`EFF101`](rules/async-blocking-call.md) |
| A database statement on every iteration of a loop over data | [`EFF103`](rules/loop-db-query.md) |
| A lock that is not reentrant, taken again while it is held, so the thread or task waits for itself | [`EFF109`](rules/lock-reacquired-while-held.md) |
| A call of a generator function whose generator nothing iterates, so its body never runs | [`EFF116`](rules/generator-never-iterated.md) |
| A `Sequence` / `Mapping` / `AbstractSet` argument changed in place by a helper | [`MUT110`](rules/readonly-param-mutation.md) |
| A list, dict, or set that an `lru_cache` / `cache` function returned, changed in place | [`MUT114`](rules/cached-result-mutation.md) |
| A change of a builtin container that is lost because the object is new and nothing holds it (`list(xs).sort()`) | [`MUT119`](rules/change-lost-on-new-object.md) |
| A DataFrame or array concatenated with itself on every loop iteration | [`DATA101`](rules/loop-carried-concat.md) |
| A list rebuilt, or a dict or set merged, on every loop iteration, and a nested-loop join | `OPT101`, `OPT102`, `OPT113` ([fixes](fixes.md)) |

The size-dependent rewrites (`OPT103`, `OPT110`, `OPT111`, `OPT120`, `OPT121`, `OPT130`) are exact
too, but opt-in, because the loops they report were rarely large enough to matter.

Two reviewers check by hand every report an exact rule makes on the development corpus (378 packages
and 30.75 million lines, plus packages added for rules with few reports). Every report of one run on
a frozen holdout of 100 packages is checked too. A rule is certified only when both have zero false
claims. `EFF101` and `EFF103` had 287 and 103 reports on the corpus and 152 and 122 on the holdout.
[Rule precision](precision.md#certifications) lists every certified rule. The other rules are
heuristic or opt-in; see [Rules](rules/index.md).

## 📜 Contracts

Most projects have rules of their own, kept today by code review, AST scripts, or run-time checks
in tests. With heki you write them down as contracts, in `heki.toml` or under `[tool.heki]` in
`pyproject.toml`. This one says that only the functions in `app.store` may change `Order.items`:

```toml
# heki.toml
[[write-owners]]
name = "order-store"
targets = ["app.models.Order.items"]
allowed-writers = ["app.store.*"]
message = "change Order.items through app.store"
fail = true   # a report of this contract fails the run
```

A function in `app/api.py` that appends to `order.items` through a helper breaks it:

```console
$ heki check
app/api.py:9:5: MUT201 (write-owner-violation) [error, heuristic] `add_gift_wrap` writes `app.models.Order.items` (through a helper) via `app.api._append` outside the write owners of `order-store`
  |
9 |     _append(order.items, "gift-wrap")
  |     ^
  = related: app/api.py:9:5: `add_gift_wrap` calls `app.api._append`
  = help: change Order.items through app.store

Found 1 diagnostic(s) in 3 file(s).
```

The run exits with code 1.

heki follows aliases, helpers, and subclasses, with no decorators and nothing added at run time. The
contract rules aren't [certified](precision.md#certifications) yet, so their reports are warnings.
`fail = true` makes that table's reports fail the run. Record today's findings in a
[Ratchet baseline](adoption.md#ratchet-baselines) first.

There is a contract for each kind of rule:

| What you want to keep | Contract | Main keys |
| --------------------- | -------- | --------- |
| Only `app.store` changes `Order.items` | `write-owners` | `targets = ["Order.items"]`, `allowed-writers = ["app.store.*"]` |
| Only `app.billing` reads `Order.card_token` | `read-owners` | `targets = ["Order.card_token"]`, `allowed-readers = ["app.billing.*"]` |
| Only `app.admin` calls `app.db.reset` | `call-owners` | `targets = ["app.db.reset"]`, `allowed-callers = ["app.admin.*"]` |
| No caller changes the settings `load_settings` returns to everyone | `readonly-returns` | `producers = ["app.config.load_settings"]`, `deep = true` |
| Every change of `Order.items` is followed by `order_changed()` | `invalidation` | `targets = ["Order.items"]`, `invalidators = ["app.cache.order_changed"]` |
| Code in `app.render` makes no blocking call | `effect-boundaries` | `scopes = ["app.render.*"]`, `forbid = ["blocking"]` |
| `app.domain` imports nothing from `app.web`, even lazily or inside a function | `import-boundaries` (experimental) | `importers = ["app.domain.*"]`, `forbidden-imports = ["app.web", "app.web.*"]` |

See [Contracts](configuration.md#contracts) for every key.

## 🤖 Teams and coding agents

A coding agent can miss a rule written in `AGENTS.md` or `CLAUDE.md`. Write it as a contract
instead, and `heki check` enforces it. List `heki.toml` and `heki-baseline.toml` in `CODEOWNERS`.
Then moving a wall takes a reviewed change, and an agent can't move one to make a report go away.

- The contract's `message` starts the help line of its reports, so the agent reads it as the fix.
- A [Stop hook](coding-agents.md#a-stop-hook-for-claude-code) runs `heki check` when Claude Code is
  about to end its turn and sends a failing report back, so the agent keeps working until the check
  passes.
- [`heki inspect app.api.checkout`](coding-agents.md#what-a-function-does-heki-inspect-experimental)
  (experimental) shows what one function reaches, changes, keeps, and returns, with the call chains,
  so an agent doesn't have to read its callees.
- `--output-format json` gives agents every check as JSON, and the
  [Agent Skill](coding-agents.md#agent-skill-experimental) (experimental) teaches them the routine
  after an edit and how to adopt heki in a repository.
- [`heki snapshot`](adoption.md#effect-snapshots-mut103) with `MUT103` reports a function that
  gained an effect, which a diff doesn't show.
- [The whole setup, end to end](coding-agents.md#the-whole-setup-end-to-end) has the CI workflow
  that checks each pull request against these walls, the hook, and the skill.

## 🔧 Rewrites proven under CPython's semantics

The `OPT` rules find quadratic list, dict, set, bytes, and queue patterns. `heki check --fix`
rewrites one only when it proves the program behaves the same. It trusts no type annotation and
keeps evaluation order and exceptions. It doesn't update an object in place while anything else
refers to it, or count on a builtin the module rebinds. When a proof fails, the report names the
missing obligation instead of offering a fix.

`--fix` alone applies the rewrites that are faster for every input (`OPT101`, `OPT102`).
`--include-size-dependent` adds the size-dependent ones, such as a `collections.deque` for
`list.pop(0)` (`OPT120`). [Fixes and safety](fixes.md) shows each rewrite and what it must prove.

## 🔓 What Python leaves open

| What Python leaves open | What other languages enforce | What heki checks |
| --- | --- | --- |
| Any code can change any object it reaches | ownership and borrowing (Rust), `const` / `readonly` | read-only parameters, fields, and views changed through helpers (`MUT110`, `MUT108`, `MUT120`); `write-owners` / `read-owners` contracts (`MUT201`, `MUT203`) |
| A signature does not say what a function does | effect types, `IO` in the type (Haskell) | blocking calls reached from `async def`, and clients and queries in loops (`EFF101`–`EFF103`); `effect-boundaries` (`EFF201`); effect snapshots (`MUT103`) |
| Shared mutable objects hide changes | value semantics, immutability by default | stale cached properties and hashes (`MUT101`, `MUT107`); changed shared return values (`MUT114`, `MUT202`); writes an `invalidation` contract must announce (`MUT208`) |
| Dynamic dispatch blocks optimizations | static types let a compiler prove a rewrite | quadratic patterns (`OPT`), rewritten only when proven under CPython's semantics |

## 🎯 Scope

When heki isn't sure, it doesn't report. It works from its own analysis, a catalog of library
behavior, and the contracts you declare. [Scope and limitations](scope.md) says what it checks and
what it leaves to other tools.

## 👉 Next steps

- 🎯 See what heki [covers and does not cover](scope.md).
- 📦 [Install heki](installation.md) and 🚀 [run it on a project](getting-started.md).
- ⚙️ [Configure](configuration.md) rule selection, contracts, and the Python version.
- 📏 Browse the [53 rules](rules/index.md), and see [how precise each one is](precision.md).
- 🔧 Read how [fixes are proven safe](fixes.md).
- 🪜 [Adopt it gradually](adoption.md) with a Ratchet baseline in CI.
- 🖥️ Use it from [your editor](lsp.md) or 🤖 [a coding agent](coding-agents.md).

## 📛 Name

*heki* is 壁, "wall", in its Sino-Japanese (*on*) reading, the one used in compounds such as
防火壁 (*bōkaheki*, "firewall"). On its own the character is usually read *kabe*.
