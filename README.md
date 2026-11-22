# heki

<p align="center">
  <img src="https://raw.githubusercontent.com/koxudaxi/heki/main/docs/assets/heki-banner.png" alt="heki: a snake in front of a brick wall" width="600">
</p>

🧱 heki (壁, Japanese for "wall") is a static checker for Python, written in Rust. It tracks side
effects and mutations across function calls.

It catches blocking I/O in `async` code, database queries and HTTP clients in loops, and
quadratic loops. You can also add rules in `pyproject.toml` to restrict which functions may
mutate state or reach I/O.

![heki check in a terminal: EFF101 reports a blocking call two helpers below an async def, and EFF103 a database query one helper below a loop, each with its call chain](https://raw.githubusercontent.com/koxudaxi/heki/main/docs/assets/example-check.svg)

By default, heki shows only *exact* reports: reports of a certified rule, built only from facts
heki established. To certify a rule, two reviewers check by hand every exact report it makes on
the development corpus (378 packages and 30.75 million lines, plus packages added for rules with
few reports), and one run on a frozen holdout of 100 packages is checked too. Neither may hold a
false claim. `EFF101` and `EFF103`, the rules of the two examples below, had 287 and 103 reports
on the corpus and 152 and 122 on the holdout, none false.
[Rule precision](https://heki.koxudaxi.dev/precision/#certifications) lists every certified rule.

*Heuristic* reports can be wrong about the code. They are opt-in (`heki check --heuristic`), and
every output labels them.

📚 [Documentation](https://heki.koxudaxi.dev/) ·
🚀 [Getting started](https://heki.koxudaxi.dev/getting-started/) ·
📏 [Rules](https://heki.koxudaxi.dev/rules/)

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
note: 5 heuristic rule(s) are off; enable them with `--heuristic` or `lint.heuristic = true`
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
note: 5 heuristic rule(s) are off; enable them with `--heuristic` or `lint.heuristic = true`
```

Ruff, ty, Pyrefly, pyright, and mypy report nothing for either example with their default settings.

### What a first run shows

Run `uvx heki check` (or `heki check` once installed) in the directory with `pyproject.toml`:

- An error reports a bug: a wrong result, an exception, a lost update, a deadlock, or a broken
  contract. A warning reports slower code, like the two above, or a bug that is rarely harmful.
- Warnings are printed and do not fail the run (exit code 0). Errors fail it (exit code 1).
  `--fail-on warning` (`lint.fail-on = "warning"`) makes warnings fail it too.
- A note at the end says how many heuristic rules were left off; `--heuristic` runs them.

### Your own conventions, too

The checks above need no configuration: `EFF101` and `EFF103` are exact and run by default.
The rules of your own project, which you keep today by review, by AST scripts, or by run-time
guards in tests, can be added as a few lines of it:

```toml
[[tool.heki.write-owners]]
name = "order-store"
targets = ["app.models.Order.items"]
allowed-writers = ["app.store.*"]
```

heki follows aliases, helpers, and subclasses, with no decorators and nothing running at run
time. The other contracts are `read-owners`, `call-owners`, `readonly-returns`, `invalidation`,
and `effect-boundaries`.

> [!WARNING]
> 🧪 **Experimental.** The exact rules are certified, but heki is still under active development:
> rules, configuration keys, diagnostics, and output formats can change between releases without
> a deprecation period. Try it on your code and report what it gets wrong.

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
| A blocking call that a call in an `async def` always runs, several helpers below | `EFF101` |
| A database statement on every iteration of a loop over data | `EFF103` |
| A `Sequence` / `Mapping` argument changed in place by a helper | `MUT110` |
| A list, dict, or set that an `lru_cache` / `cache` function returned, changed in place | `MUT114` |
| A DataFrame or array concatenated with itself on every loop iteration | `DATA101` |
| A quadratic list, dict, set, bytes, or queue pattern | `OPT101`-`OPT103`, `OPT110`, `OPT111`, `OPT113`, `OPT120`, `OPT121`, `OPT130` |

The other rules are heuristic or opt-in, and the contract rules run once you declare a contract:
see [Rules](https://heki.koxudaxi.dev/rules/) and
[Rule precision](https://heki.koxudaxi.dev/precision/).

### A safety net for reviews and coding agents

- The language server checks while you edit and re-checks only what an edit can affect.
- `heki snapshot` records what each public function does, and `MUT103` reports a function that gained an
  effect, which a diff does not show.
- Coding agents get the same checks as JSON, and an
  [Agent Skill](https://heki.koxudaxi.dev/coding-agents/#agent-skill-experimental) teaches them the workflow.

### Fast, quiet, and easy to adopt

- A 100,000-line project is checked in a few seconds.
- A report needs evidence: when heki cannot tell, it stays quiet.
- Configuration, suppression comments, and output formats follow Ruff, and a baseline fails CI
  only on new findings.

## 🔓 What Python leaves open

Rust enforces ownership, Haskell puts `IO` in the type, and static types let a compiler prove a
rewrite. Python enforces none of these. heki checks what they would catch: objects changed
through helpers, effects behind a call, stale shared state, and quadratic patterns. See the
[comparison](https://heki.koxudaxi.dev/#what-python-leaves-open).

## 🔍 How heki differs

Ruff models syntax, type checkers model declared types, and tach and import-linter model imports.
heki models effects and aliases, what code does, so use it next to them: the second example
passes `mypy --strict` and pyright's strict mode, yet sends one query per order. See the
[comparison](https://heki.koxudaxi.dev/#how-heki-differs).

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
report names the missing obligation instead of offering a fix.

## 🎯 Scope

- Its knowledge comes from its own analysis, a catalog of library behavior, contracts you
  declare in `pyproject.toml`, and type annotations, which only help it resolve calls: rewrites
  never trust them.
- When it cannot tell, it stays quiet. Annotations, catalog entries, and contracts close the
  findings it misses.
- It does not model `__getattr__` proxies, metaclasses, or framework machinery, and it never
  runs your code.

See [Scope and limitations](https://heki.koxudaxi.dev/scope/) for the details.

## 📦 Installation

```console
$ uv tool install heki      # or: pipx install heki / pip install heki
$ uvx heki check            # run without installing
```

## 🏃 Usage

```console
$ heki check                                    # check the project
$ heki check --heuristic                        # also run the heuristic rules
$ heki check --diff                             # preview proven-safe fixes
$ heki check --fix                              # apply them
$ heki check --baseline heki-baseline.toml      # fail only on new findings
$ heki rule EFF101                              # explain a rule
$ heki server                                   # LSP server for editors
```

## 🪜 pre-commit and CI

A pre-commit hook (heki follows calls across files, so it checks the whole project):

```yaml
# .pre-commit-config.yaml
repos:
  - repo: https://github.com/koxudaxi/heki
    rev: v0.2.2
    hooks:
      - id: heki
```

pre-commit builds heki from that release's source with cargo on the first run, which takes a few
minutes, and installs Rust for the build when cargo is missing. A local hook that runs the wheel
from PyPI needs no compile:

```yaml
# .pre-commit-config.yaml
repos:
  - repo: local
    hooks:
      - id: heki
        name: heki
        entry: uvx heki check
        language: system   # needs uv on PATH
        types: [python]
        pass_filenames: false
```

A GitHub Actions workflow with code-scanning alerts (SARIF) and annotations:

```yaml
# .github/workflows/heki.yml
name: heki
on: [push, pull_request]
permissions:
  contents: read
  security-events: write   # for the SARIF upload
jobs:
  heki:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v7
      - uses: astral-sh/setup-uv@v10
      - run: uvx heki check --output-format sarif > heki.sarif || test "$?" -eq 1
      - uses: github/codeql-action/upload-sarif@v4
        with:
          sarif_file: heki.sarif
      - run: uvx heki check --output-format github
```

Errors fail the hook and the last step. Warnings, such as `EFF101` and `EFF103` reports, do not,
unless you add `--fail-on warning` or set `fail-on = "warning"` in `[tool.heki.lint]`. The SARIF
step keeps findings as alerts and fails only on a configuration, read, or syntax error. See
[Adopting heki](https://heki.koxudaxi.dev/adoption/) for a hook without uv and a Ratchet baseline.

## 📚 Documentation

**👉 [heki.koxudaxi.dev](https://heki.koxudaxi.dev/)**

- 🚀 [Getting started](https://heki.koxudaxi.dev/getting-started/) - run heki on a project, read a diagnostic, apply a fix
- 🎯 [Scope and limitations](https://heki.koxudaxi.dev/scope/) - what heki checks and what it leaves to other tools
- ⚙️ [Configuration](https://heki.koxudaxi.dev/configuration/) - rule selection, contracts, the Python version
- 📏 [Rules](https://heki.koxudaxi.dev/rules/) - all 43 rules
- 📐 [Rule precision](https://heki.koxudaxi.dev/precision/) - exact and heuristic rules, and how they are measured
- 🔧 [Fixes and safety](https://heki.koxudaxi.dev/fixes/) - how rewrites are proven safe
- 🪜 [Adopting heki](https://heki.koxudaxi.dev/adoption/) - Ratchet baselines, pre-commit, CI
- 🖥️ [Editors (LSP)](https://heki.koxudaxi.dev/lsp/) - VS Code, Neovim, and any LSP client
- 🤖 [Coding agents](https://heki.koxudaxi.dev/coding-agents/) - JSON output and agent setup
- ⌨️ [CLI reference](https://heki.koxudaxi.dev/cli-reference/) - every command and option
- 🛠️ [Development](https://heki.koxudaxi.dev/development/) - building, testing, and the gates
- 📝 [Changelog](https://github.com/koxudaxi/heki/blob/main/CHANGELOG.md)

## 📛 Name

*heki* is 壁, "wall", in its Sino-Japanese (*on*) reading, the one used in compounds such as
防火壁 (*bōkaheki*, "firewall"). On its own the character is usually read *kabe*.

## 📄 License

[MIT](https://github.com/koxudaxi/heki/blob/main/LICENSE)
