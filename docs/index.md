# heki

<p align="center">
  <img src="assets/heki-banner.png" alt="heki: a snake in front of a brick wall" width="600">
</p>

🧱 heki (壁, Japanese for "wall") is a static checker for Python, written in Rust. It tracks side
effects and mutations across function calls.

It catches blocking I/O in `async` code, database queries and HTTP clients in loops, and
quadratic loops. You can also add rules in `pyproject.toml` to restrict which functions may
mutate state or reach I/O.

![heki check in a terminal: EFF101 reports a blocking call two helpers below an async def, and EFF103 a database query one helper below a loop, each with its call chain](assets/example-check.svg)

By default, heki shows only *exact* reports: reports of a certified rule, built only from facts
heki established. To certify a rule, two reviewers check by hand every exact report it makes on
the development corpus (378 packages and 30.75 million lines, plus packages added for rules with
few reports), and one run on a frozen holdout of 100 packages is checked too. Neither may hold a
false claim. `EFF101` and `EFF103`, the rules of the two examples below, had 287 and 103 reports
on the corpus and 152 and 122 on the holdout, none false.
[Rule precision](precision.md#certifications) lists every certified rule.

*Heuristic* reports can be wrong about the code. They are opt-in (`heki check --heuristic`), and
every output labels them.

<p>
  <a class="md-button md-button--primary" href="getting-started/">Getting Started</a>
  <a class="md-button" href="rules/">Rules</a>
  <a class="md-button" href="configuration/">Configuration</a>
</p>

[![PyPI](https://img.shields.io/pypi/v/heki)](https://pypi.org/project/heki/)
[![Python versions](https://img.shields.io/pypi/pyversions/heki)](https://pypi.org/project/heki/)
[![CI](https://github.com/koxudaxi/heki/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/koxudaxi/heki/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://github.com/koxudaxi/heki/blob/main/LICENSE)

⚡ Try it in your project's root, without installing: `uvx heki check`

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
app/api.py:5:12: EFF101 (async-blocking-call) [warning] `app.users.load_user` always runs the waiting call `requests.get` on the event loop (2 calls down, on every path)
  |
5 |     return load_user(user_id)
  |            ^
  = related: app/api.py:5:12: `get_user` calls `app.users.load_user`
  = related: app/users.py:8:41: `load_user` calls `app.users.fetch_name`
  = related: app/users.py:5:12: `fetch_name` calls `requests.get`
  = help: offload it with `asyncio.to_thread`, or use an async client

Found 1 diagnostic(s) in 2 file(s).
note: 4 heuristic rule(s) are off; enable them with `--heuristic` or `lint.heuristic = true`
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
note: 4 heuristic rule(s) are off; enable them with `--heuristic` or `lint.heuristic = true`
```

Ruff, ty, Pyrefly, pyright, and mypy report nothing for either example with their default settings.

### What a first run shows

Run `uvx heki check` (or `heki check` once [installed](installation.md)) in the directory with
`pyproject.toml`:

- An error reports a bug: a wrong result, an exception, a lost update, a deadlock, or a broken
  contract. A warning reports slower code, like the two above, or a bug that is rarely harmful.
- Warnings are printed and do not fail the run (exit code 0). Errors fail it (exit code 1).
  [`--fail-on warning`](configuration.md#rule-selection-toolhekilint)
  (`lint.fail-on = "warning"`) makes warnings fail it too.
- A note at the end says how many heuristic rules were left off; `--heuristic` runs them.

### Your own conventions, too

The checks above need no configuration: `EFF101` and `EFF103` are exact and run by default.
The rules of your own project, which you keep today by review, by AST scripts, or by run-time
guards in tests, can be added as a few lines of [configuration](configuration.md):

```toml
[[tool.heki.write-owners]]
name = "order-store"
targets = ["app.models.Order.items"]
allowed-writers = ["app.store.*"]
```

heki follows aliases, helpers, and subclasses, with no decorators and nothing running at run
time. The other contracts are `read-owners`, `call-owners`, `readonly-returns`, `invalidation`,
and `effect-boundaries`.

!!! warning "Experimental"

    The exact rules are certified, but heki is still under active development: rules,
    configuration keys, diagnostics, and output formats can change between releases without a
    deprecation period. Try it on your code and report what it gets wrong.

## ✨ Why heki?

Types say what values are; heki checks what code does:

- **Aliases and in-place changes:** which objects a function changes, through any alias, helper,
  or subclass.
- **Contracts:** who may write, read, or call what, and which code may reach I/O.
- **Proven rewrites:** quadratic patterns are rewritten only when the rewrite is proven, not
  guessed.

### What code does, not what types it has

These rules are exact and run by default. Each report shows the chain of calls that leads to it.

| What goes wrong | Rule |
| --- | --- |
| A blocking call that a call in an `async def` always runs, several helpers below | [`EFF101`](rules/async-blocking-call.md) |
| A database statement on every iteration of a loop over data | [`EFF103`](rules/loop-db-query.md) |
| A `Sequence` / `Mapping` argument changed in place by a helper | [`MUT110`](rules/readonly-param-mutation.md) |
| A list, dict, or set that an `lru_cache` / `cache` function returned, changed in place | [`MUT114`](rules/cached-result-mutation.md) |
| A DataFrame or array concatenated with itself on every loop iteration | [`DATA101`](rules/loop-carried-concat.md) |
| A quadratic list, dict, set, bytes, or queue pattern | `OPT101`-`OPT103`, `OPT110`, `OPT111`, `OPT113`, `OPT120`, `OPT121`, `OPT130` ([fixes](fixes.md)) |

The other rules are heuristic or opt-in, and the [contract](configuration.md#contracts) rules run
once you declare a contract: see [Rules](rules/index.md) and [Rule precision](precision.md).

### A safety net for reviews and coding agents

- The [language server](lsp.md) checks while you edit and re-checks only what an edit can affect.
- [`heki snapshot`](adoption.md#effect-snapshots-mut103) records what each public function does, and `MUT103` reports a function that gained an
  effect, which a diff does not show.
- [Coding agents](coding-agents.md) get the same checks as JSON, and an
  [Agent Skill](coding-agents.md#agent-skill-experimental) teaches them the workflow.

### Fast, quiet, and easy to adopt

- A 100,000-line project is checked in a few seconds.
- A report needs evidence: when heki cannot tell, [it stays quiet](scope.md#when-heki-cannot-tell).
- Configuration, suppression comments, and output formats follow Ruff, and a [baseline](adoption.md#ratchet-baselines) fails CI
  only on new findings.

## 🔓 What Python leaves open

| What Python leaves open | What other languages enforce | What heki checks |
| --- | --- | --- |
| Any code can change any object it reaches | ownership and borrowing (Rust), `const` / `readonly` | read-only parameters and fields changed through helpers (`MUT110`, `MUT108`); `write-owners` / `read-owners` contracts (`MUT201`, `MUT203`) |
| A signature does not say what a function does | effect types, `IO` in the type (Haskell) | blocking calls reached from `async def`, and clients and queries in loops (`EFF101`–`EFF103`); `effect-boundaries` (`EFF201`); effect snapshots (`MUT103`) |
| Shared mutable objects hide changes | value semantics, immutability by default | stale cached properties and hashes (`MUT101`, `MUT107`); changed shared return values (`MUT114`, `MUT202`); writes an `invalidation` contract must announce (`MUT109`) |
| Dynamic dispatch blocks optimizations | static types let a compiler prove a rewrite | quadratic patterns (`OPT`), rewritten only when proven under CPython's semantics |

## 🔍 How heki differs

The difference is the model, not reaching across files: type checkers already follow types
across modules. heki models effects and aliases, what code does:

| Tool | Models | heki adds |
| --- | --- | --- |
| Ruff | syntax and style | the effects behind a call: a blocking call, a query, or a write, wherever it happens |
| ty, Pyrefly, pyright, mypy | declared types | in-place changes, through aliases and helpers, to `frozen`, `ReadOnly`, and `Final` fields and to `Sequence` / `Mapping` arguments |
| tach, import-linter | which modules may import which | which functions write or read a field, call a function, or reach I/O |

Their stricter settings do not close the gap. The second example above is fully annotated and
passes Ruff's annotation rules (`ANN`), `mypy --strict`, pyright's strict mode, and Pyrefly's
`strict` preset, yet it sends one query per order: the types are right, and what the code does
is slow.

heki leaves to these tools what they cover, and keeps doing so as they grow: use it next to them.

## 🔧 Rewrites proven under CPython's semantics

The `OPT` rules find quadratic list, dict, set, bytes, and queue patterns. `heki check --fix`
rewrites one only when it proves that the program behaves the same:

- it trusts no type annotation (a subclass can override `__add__`);
- any other reference to the object blocks an in-place update;
- evaluation order and exceptions are kept;
- builtins count only while the module does not rebind them.

A `collections.deque` rewrite also updates the annotation and adds the import:

```console
$ heki check --diff --include-size-dependent jobs.py
--- a/jobs.py
+++ b/jobs.py
@@ -1,11 +1,12 @@
 """Job queue."""
 
+import collections
 import os
 
 
 def drain(jobs: list[int]) -> int:
-    queue: list[int] = list(jobs)
+    queue: collections.deque[int] = collections.deque(list(jobs))
     total = 0
     while queue:
-        total += queue.pop(0)
+        total += queue.popleft()
     return total
Would fix 1 issue(s) in 1 file(s).
```

`--fix` alone applies the rewrites that are faster for every input (`OPT101`, `OPT102`); a
size-dependent one like this needs `--include-size-dependent`.

The scope analysis is checked against CPython's `symtable` on 3.11 to 3.15, and adversarial
programs that break a naive rewrite must all be blocked on CPython. When a proof fails, the
report names the missing obligation instead of offering a fix. See [Fixes and safety](fixes.md).

## 👉 Next steps

- 🎯 See what heki [covers and does not cover](scope.md).
- 📦 [Install heki](installation.md) and 🚀 [run it on a project](getting-started.md).
- ⚙️ [Configure](configuration.md) rule selection, contracts, and the Python version.
- 📏 Browse the [41 rules](rules/index.md), and see [how precise each one is](precision.md).
- 🔧 Read how [fixes are proven safe](fixes.md).
- 🪜 [Adopt it gradually](adoption.md) with a Ratchet baseline in CI.
- 🖥️ Use it from [your editor](lsp.md) or 🤖 [a coding agent](coding-agents.md).

## 📛 Name

*heki* is 壁, "wall", in its Sino-Japanese (*on*) reading, the one used in compounds such as
防火壁 (*bōkaheki*, "firewall"). On its own the character is usually read *kabe*.
