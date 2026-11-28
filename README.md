# heki

<p align="center">
  <img src="https://raw.githubusercontent.com/koxudaxi/heki/main/docs/assets/heki-banner.png" alt="heki: a snake in front of a brick wall" width="600">
</p>

🧱 heki (壁, Japanese for "wall") is a static checker for Python, written in Rust. It tracks side
effects and mutations across function calls, and checks a 100,000-line project in a few seconds.

Ruff checks syntax and style, and mypy, Pyright, Pyrefly, and ty check types. heki checks what
your code does: which effects a call reaches and which state it changes, across functions and
files.

It works on two levels. With no configuration, it catches bugs such as blocking calls in `async`
code, database queries in loops, and in-place changes to read-only parameters. On top of that, it
enforces the walls your project writes as
[contracts](https://heki.koxudaxi.dev/configuration/#contracts): which functions may change a
field, call a function, or reach I/O.

![heki check in a terminal: EFF101 reports a blocking call two helpers below an async def with no configuration, and MUT201 a write to Order.items through a helper outside the functions a contract allows, each with its call chain](https://raw.githubusercontent.com/koxudaxi/heki/main/docs/assets/example-check.svg)

📚 [Documentation](https://heki.koxudaxi.dev/) ·
🚀 [Getting started](https://heki.koxudaxi.dev/getting-started/) ·
📏 [Rules](https://heki.koxudaxi.dev/rules/) ·
📜 [Contracts](https://heki.koxudaxi.dev/configuration/#contracts)

[![PyPI](https://img.shields.io/pypi/v/heki)](https://pypi.org/project/heki/)
[![Python versions](https://img.shields.io/pypi/pyversions/heki)](https://pypi.org/project/heki/)
[![CI](https://github.com/koxudaxi/heki/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/koxudaxi/heki/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](https://github.com/koxudaxi/heki/blob/main/LICENSE)

⚡ Try it in your project's root, without installing: `uvx heki check`. Then follow
[Try it in 5 minutes](#-try-it-in-5-minutes).

---

### A bug found with no configuration

Each file looks fine on its own: the bug is in the call chain.

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

The image at the top is `heki check` on these examples: `EFF101` with no configuration, and
`MUT201` from the contract. The same code passes mypy, Pyright, Pyrefly, and ty in strict mode
(`mypy --strict`, Pyright's `strict`, Pyrefly's `strict` preset, every ty rule as an error), and
Ruff with every rule on asks only for docstrings and copyright lines.

### How heki differs

| Tool | Models | heki adds |
| --- | --- | --- |
| Ruff | syntax and style | the effects behind a call: a blocking call, a query, or a write, wherever it happens |
| mypy, Pyright, Pyrefly, ty | declared types | in-place changes, through aliases and helpers, to `frozen`, `ReadOnly`, and `Final` fields and to `Sequence` / `Mapping` / `AbstractSet` arguments |
| tach, import-linter | which modules may import which | which functions write or read a field, call a function, or reach I/O; import rules that know when each import runs (experimental); planned: reading these tools' contracts as heki contracts |

Use heki next to these tools; its configuration, suppression comments, and output formats follow
Ruff's. [Design Philosophy](https://heki.koxudaxi.dev/philosophy/) says why heki works this way
and compares its checks with what Rust and Haskell enforce.

## 🧭 What heki can do

### ✅ Bug checks with no configuration

These rules run by default, and each report shows the chain of calls that leads to it. Every
report they made on 378 packages (about 30.75 million lines) and on a frozen holdout of 100 more
was checked by hand, with no false claim
([Rule precision](https://heki.koxudaxi.dev/precision/#certifications)).

| What goes wrong | Rule |
| --- | --- |
| A blocking call that a call in an `async def` always runs, several helpers below | `EFF101` |
| A new HTTP, database, or RPC client (or a request without a session) on every iteration of a loop over data | `EFF102` |
| A database statement on every iteration of a loop over data | `EFF103` |
| A lock that is not reentrant, taken again while it is held, so the thread or task waits for itself | `EFF109` |
| A call of a generator function whose generator nothing iterates, so its body never runs | `EFF116` |
| A `Sequence` / `Mapping` / `AbstractSet` argument changed in place by a helper | `MUT110` |
| A list, dict, or set that an `lru_cache` / `cache` function returned, changed in place | `MUT114` |
| A change of a builtin container that is lost because the object is new and nothing holds it (`list(xs).sort()`) | `MUT119` |
| A DataFrame or array concatenated with itself on every loop iteration | `DATA101` |
| A list rebuilt, or a dict or set merged, on every loop iteration, and a nested-loop join | `OPT101`, `OPT102`, `OPT113` |

The size-dependent rewrites (`OPT103`, `OPT110`, `OPT111`, `OPT120`, `OPT121`, `OPT130`) are exact
too, but opt-in. The other rules are heuristic or opt-in ([Rules](https://heki.koxudaxi.dev/rules/)).

A call on the result of a builtin or standard-library call is followed too
(`text.strip().lower()`, `shlex.split(line)[0].upper()`), with result types from typeshed's
stubs: heki infers a method's or function's result, a library attribute, an enumeration member and
its value, and a `match` class pattern's subject. It stays unresolved where overloads join to no
single type (`d.get(key).upper()`), for module variables, and for packages outside the standard
library ([How a call is resolved](https://heki.koxudaxi.dev/scope/#how-a-call-is-resolved)).

`heki check --fix` applies an `OPT` rewrite only when it proves, under CPython's semantics, that
the program behaves the same ([Fixes and safety](https://heki.koxudaxi.dev/fixes/)). More by
problem: [async and execution effects](https://heki.koxudaxi.dev/effects/),
[state ownership and mutation](https://heki.koxudaxi.dev/ownership/), and
[imports and module boundaries](https://heki.koxudaxi.dev/imports/).

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

See [Contracts](https://heki.koxudaxi.dev/configuration/#contracts) for every key. A coding agent
can miss a rule written in its instructions, but not a contract that `heki check` enforces:

- List `heki.toml` and `heki-baseline.toml` in `CODEOWNERS`: moving a wall takes a review.
- A contract's `message` starts the help line of its reports, so the agent reads it as the fix.
- A [Stop hook](https://heki.koxudaxi.dev/coding-agents/#a-stop-hook-for-claude-code) sends a
  failing check back before the agent ends its turn.
- [`heki inspect`](https://heki.koxudaxi.dev/coding-agents/#what-a-function-does-heki-inspect-experimental)
  (experimental) shows what one function reaches, changes, and returns.
- [`heki recommend --focus types --diff`](https://heki.koxudaxi.dev/recommend/#applying-the-suggestions) (experimental) prints the exact type suggestions
  as one patch that `git apply` takes, annotations only.
- `--output-format json` gives every check as JSON, and the
  [Agent Skill](https://heki.koxudaxi.dev/coding-agents/#agent-skill-experimental)
  (experimental) teaches the routine after an edit.
  [Coding agents](https://heki.koxudaxi.dev/coding-agents/) has the whole setup.

## 🚀 Try it in 5 minutes

1. In your project's root (the directory with `pyproject.toml`), run `uvx heki check`. An exact
   error, like `EFF101` above, fails the run (exit code 1). Warnings are printed but don't fail it
   unless you pass `--fail-on warning`.

2. Ask what fits the project with [`heki recommend`](https://heki.koxudaxi.dev/recommend/)
   (experimental). It writes no file and helps most when the first step finds nothing, as on
   heki's [sample service](https://github.com/koxudaxi/heki/tree/main/tests/sample_service):

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
    [Getting started](https://heki.koxudaxi.dev/getting-started/#ask-what-fits-the-project) shows
    more of the report.

3. Keep it running with the [pre-commit hook](https://heki.koxudaxi.dev/adoption/#pre-commit) and
   a CI step, and record today's findings in a
   [Ratchet baseline](https://heki.koxudaxi.dev/adoption/#ratchet-baselines) so only new ones
   fail: `uvx heki==0.11.0 check --baseline heki-baseline.toml --update-baseline`. The
   [starter recipes](https://heki.koxudaxi.dev/adoption/#starter-recipes) have these setups ready
   to copy.

> [!WARNING]
> 🧪 **Pre-release.** Rules, configuration keys, and output formats can change in any release,
> and the release notes list every change. Pin the version (`heki==0.11.0`, and the hook's `rev`)
> so upgrades happen only when you choose. The default rules are exact. Heuristic rules,
> experimental rules, and experimental commands run only when you ask for them, and every output
> labels heuristic reports. Please [report](https://github.com/koxudaxi/heki/issues) what heki
> gets wrong.

## 📦 Installation

```console
$ uv tool install heki      # or: pipx install heki / pip install heki
$ uvx heki check            # run without installing
```

heki ships prebuilt wheels on PyPI for Linux (x86-64, AArch64, x86-64 musl), macOS (Apple silicon,
Intel), and Windows (x86-64). `uvx heki`, `pip install heki`, and the pre-commit hook need no Rust
toolchain and compile nothing. Building from source is for other platforms and for development:
see [Installation](https://heki.koxudaxi.dev/installation/).

## ⌨️ Usage

```console
$ heki check                                    # check the project
$ heki check --heuristic                        # also run the heuristic rules
$ heki check --diff                             # preview proven-safe fixes
$ heki check --fix                              # apply them
$ heki check --baseline heki-baseline.toml      # fail only on new findings
$ heki rule EFF101                              # explain a rule
$ heki server                                   # LSP server for editors
$ heki inspect app.users.load_user              # what one function does (experimental)
$ heki recommend                                # configuration advice (experimental)
```

## 🪜 pre-commit and CI

```yaml
# .pre-commit-config.yaml
repos:
  - repo: https://github.com/koxudaxi/heki-pre-commit
    rev: v0.11.0
    hooks:
      - id: heki
```

```yaml
# A step of a GitHub Actions job that has uv
- run: uvx heki==0.11.0 check
```

The hook runs `heki check --touched` on the staged files, so old findings in other files don't
block a commit. It still fails on a report in an unstaged file whose call chain passes through a
staged file, and, with a baseline, on every regression. Run the whole project in CI. In GitHub
Actions, the reports show up as annotations.
[Adopting heki](https://heki.koxudaxi.dev/adoption/#continuous-integration) has the SARIF run for
code-scanning alerts, a local hook, and how to check only the files a pull request changes.

## 🛡️ Stability

On the way to 1.0, heki keeps these stable: the exit codes `0`, `1`, and `2`, the exact rules that
run by default, the format of the Ratchet baseline, the contract tables `write-owners`,
`read-owners`, `call-owners`, `readonly-returns`, `invalidation`, and `effect-boundaries` with
`fail = true` and `[lint] fail = [...]`, and the `json`, `sarif`, and `github` output formats. A
change to one of them is listed under "Breaking changes" in the release notes. The experimental
features can change in any release: `heki recommend`, `heki inspect`, `heki skill`,
`--generate-prompt`, and the rules marked experimental
([the list](https://heki.koxudaxi.dev/experimental/)).

## 📚 Documentation

**👉 [heki.koxudaxi.dev](https://heki.koxudaxi.dev/)**

When heki isn't sure, it doesn't report.
[Scope and limitations](https://heki.koxudaxi.dev/scope/) says what it checks, where its
knowledge comes from, and what it leaves to other tools.

- 🚀 [Getting started](https://heki.koxudaxi.dev/getting-started/) - run heki on a project, read a diagnostic, apply a fix
- ⚙️ [Configuration & Contracts](https://heki.koxudaxi.dev/configuration/) - rule selection, contracts, the Python version
- 📏 [Rules](https://heki.koxudaxi.dev/rules/) - every rule, with its tier
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
