# heki

🧱 heki (壁, "wall") is a static checker for Python, written in Rust. It tracks side effects and
mutations across function calls.

It catches blocking I/O in `async` code, database queries and HTTP clients in loops, and
quadratic loops. You can also add rules in `pyproject.toml` to restrict which functions may
mutate state or reach I/O.

A report is *exact* when heki established every premise of its claim and its rule was
certified, and only exact reports are shown by default. The others are *heuristic*: they can be
wrong, every output labels them, and `heki check --heuristic` shows them too. The exact rules
are listed in [Rule precision](https://heki.koxudaxi.dev/precision/#certifications).

📚 [Documentation](https://heki.koxudaxi.dev/) ·
🚀 [Getting started](https://heki.koxudaxi.dev/getting-started/) ·
📏 [Rules](https://heki.koxudaxi.dev/rules/)

[![PyPI](https://img.shields.io/pypi/v/heki)](https://pypi.org/project/heki/)
[![Python versions](https://img.shields.io/pypi/pyversions/heki)](https://pypi.org/project/heki/)
[![CI](https://github.com/koxudaxi/heki/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/koxudaxi/heki/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://github.com/koxudaxi/heki/blob/main/LICENSE)

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
$ heki check --heuristic
app/api.py:5:12: EFF101 (async-blocking-call) [warning] `app.users.load_user` always runs the waiting call `requests.get` on the event loop (2 calls down, on every path)
  |
5 |     return load_user(user_id)
  |            ^
  = related: app/api.py:5:12: `get_user` calls `app.users.load_user`
  = related: app/users.py:8:41: `load_user` calls `app.users.fetch_name`
  = related: app/users.py:5:12: `fetch_name` calls `requests.get`
  = help: offload it with `asyncio.to_thread`, or use an async client

Found 1 diagnostic(s) in 2 file(s).
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
$ heki check --heuristic
app/orders.py:11:37: EFF103 (loop-db-query) [warning] every iteration over parameter `orders` sends a database statement: `sqlite3.Cursor.execute` (through `app.orders.customer_name`)
   |
11 |         lines.append(f"#{order_id} {customer_name(cursor, customer_id)}")
   |                                     ^
  = related: app/orders.py:11:37: `order_summaries` calls `app.orders.customer_name`
  = related: app/orders.py:5:5: `customer_name` calls `sqlite3.Cursor.execute`

Found 1 diagnostic(s) in 1 file(s).
```

Ruff, ty, Pyrefly, pyright, and mypy report nothing for either example with their default settings.

### Your own conventions, too

The checks above need no configuration: `EFF101` and `EFF103` are exact and run by default.
The rules of your own project, which you keep today by review, by AST scripts, or by run-time
guards in tests, can be added as a few lines of it:

```toml
[[tool.heki.write-owners]]
targets = ["app.models.Order.items"]
allowed-writers = ["app.store.*"]
```

heki follows aliases, helpers, and subclasses, with no decorators and nothing running at run
time. The other contracts are `read-owners`, `call-owners`, `readonly-returns`, `invalidation`,
and `effect-boundaries`.

> [!WARNING]
> 🧪 **Experimental.** heki is under active development: rules, configuration keys, diagnostics,
> and output formats can change between releases without a deprecation period. Try it on your
> code and report what it gets wrong, but do not depend on its output staying the same yet.

## ✨ Why heki?

Types say what values are; heki checks what code does:

- **Aliases and in-place changes:** which objects a function changes, through any alias, helper,
  or subclass.
- **Contracts:** who may write, read, or call what, and which code may reach I/O.
- **Proven rewrites:** quadratic patterns are rewritten only when the rewrite is proven, not
  guessed.

### What code does, not what types it has

Each report shows the chain of calls that leads to the bug.

| What goes wrong | Rule | Certainty |
| --- | --- | --- |
| A blocking call that a call in an `async def` always runs, several helpers below | `EFF101` | exact |
| A blocking call an `async def` can reach | `EFF111` | heuristic |
| A database statement on every iteration of a loop over data | `EFF103` | exact |
| A database query, or a new HTTP client, a loop can run on every iteration | `EFF113`, `EFF102` | heuristic |
| An attribute that `__hash__` reads, changed after the object went into a `set` | `MUT107` | heuristic |
| A `Sequence` / `Mapping` argument changed in place by a helper | `MUT110` | exact |
| Opt-in: a frozen or `ReadOnly` field changed in place | `MUT108` | heuristic |
| Opt-in: I/O while a lock or a database transaction is held | `EFF106` | heuristic |
| Opt-in: a generator consumed twice | `MUT106` | heuristic |
| Opt-in, experimental: a `@cached_property` left stale after another method changes what it read | `MUT101` | heuristic |
| Opt-in, experimental: a function reading a `@cached_property` that another method leaves stale | `MUT111` | heuristic |
| Opt-in, experimental: a value that is a new object on one path and the caller's object on another, changed in place | `MUT112` | heuristic |
| Opt-in, experimental: a dict or set changed while a suspended generator, or a loop calling a helper, iterates it | `MUT113` | heuristic |

A heuristic rule runs when its code or name is selected, or with `--heuristic`
(`lint.heuristic = true` in the configuration). See
[Rule precision](https://heki.koxudaxi.dev/precision/) for how a rule becomes exact and what each
one measured.

### A safety net for reviews and coding agents

- The language server checks while you edit and re-checks only what an edit can affect.
- `heki snapshot` records what each public function does, and `MUT103` reports a function that gained an
  effect, which a diff does not show.
- Coding agents get the same checks as JSON.

### Fast, quiet, and easy to adopt

- Under a second for a 200,000-line project.
- A report needs evidence: when heki cannot tell, it stays quiet.
- Bugs fail the run; slower code is reported as a warning that does not, unless you ask for
  `--fail-on warning`.
- Configuration, suppression comments, and output formats follow Ruff, and a baseline fails CI
  only on new findings.

## 🔓 What Python leaves open

| What Python leaves open | What other languages enforce | What heki checks |
| --- | --- | --- |
| Any code can change any object it reaches | ownership and borrowing (Rust), `const` / `readonly` | read-only parameters and fields changed through helpers (`MUT110`, `MUT108`); `write-owners` / `read-owners` contracts (`MUT201`, `MUT203`) |
| A signature does not say what a function does | effect types, `IO` in the type (Haskell) | blocking calls reached from `async def`, and clients and queries in loops (`EFF101`–`EFF103`); `effect-boundaries` (`EFF201`); effect snapshots (`MUT103`) |
| Shared mutable objects hide changes | value semantics, immutability by default | stale cached properties and hashes (`MUT101`, `MUT107`); changed shared return values (`MUT202`); writes an `invalidation` contract must announce (`MUT109`) |
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

## 📚 Documentation

**👉 [heki.koxudaxi.dev](https://heki.koxudaxi.dev/)**

- 🚀 [Getting started](https://heki.koxudaxi.dev/getting-started/) - run heki on a project, read a diagnostic, apply a fix
- 🎯 [Scope and limitations](https://heki.koxudaxi.dev/scope/) - what heki checks and what it leaves to other tools
- ⚙️ [Configuration](https://heki.koxudaxi.dev/configuration/) - rule selection, contracts, the Python version
- 📏 [Rules](https://heki.koxudaxi.dev/rules/) - all 35 rules
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
