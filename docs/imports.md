# Imports and Module Boundaries

heki finds import cycles, import-order crashes, and test or development code loaded by a package, enforces `import-boundaries` contracts, and shows what a module loads with `heki inspect --imports` (all experimental).

The import rules know when each import runs: while its module loads, when a function runs, or only under `TYPE_CHECKING`. The rules and the contract can change between releases, and none of them runs without configuration. [Rules](rules/index.md) has one page per rule, and [Imports](configuration.md#imports-toolhekiimports) has the settings that say which imports count.

## 🔁 Cycles and import-order crashes

Two modules that import each other while they load are an import cycle. The cycle itself can work, until one import order makes a `from M import name` run before `M` has bound the name. Then Python raises `ImportError`, often only in a test run or an entry point that imports the modules in another order.

```toml
# heki.toml
[lint]
extend-select = ["IMP101", "IMP102"]
fail = ["IMP101", "IMP102"]
```

`fail` makes the reports of those rules fail the run, whatever their tier. Without it, a heuristic report is a warning.

```python
# pkg/a.py
from pkg.b import make_b


def make_a() -> str:
    return "a" + make_b()
```

```python
# pkg/b.py
from pkg.a import make_a


def make_b() -> str:
    return "b"


def both() -> str:
    return make_a()
```

```console
$ heki check --output-format concise
pkg/a.py:1:19: IMP102 [heuristic] `<module>` imports `make_b` from `pkg.b` while `pkg.b` is still loading, at its import of `pkg.a`, and has not bound it: importing `pkg.b` while no module of the import cycle has loaded raises `ImportError` here
pkg/b.py:1:19: IMP101 [heuristic] `<module>` imports `pkg.a` while `pkg.b` loads, which closes the import cycle `pkg.a` -> `pkg.b` -> `pkg.a`
pkg/b.py:1:19: IMP102 [heuristic] `<module>` imports `make_a` from `pkg.a` while `pkg.a` is still loading, at its import of `pkg.b`, and has not bound it: importing `pkg.a` while no module of the import cycle has loaded raises `ImportError` here
Found 3 diagnostic(s) in 3 file(s).
```

This output is from heki 0.9.0. The full format adds the source line and a `help` that names the ways out: import the name inside the function that uses it, import the module and read the name where it is used, or bind the name before the cyclic import.

## 📦 What package code loads while it loads

A package that works in the repository can fail after `pip install`: it imports a test helper, a development dependency, or a module that only an extra provides. These rules compare the imports that run while a module loads with the files of the project and the dependencies in `pyproject.toml`.

```toml
# heki.toml
[lint]
extend-select = ["IMP104", "IMP301"]
```

```python
# pkg/core.py
from tests.helpers import fake
```

```python
# pkg/fixtures.py
from faker import Faker
```

With `faker` declared only in the `dev` dependency group:

```console
$ heki check --output-format concise
pkg/core.py:1:27: IMP104 [heuristic] `<module>` imports `tests.helpers` while `pkg.core` loads, test code outside the package (`tests/helpers.py`) that the installed package does not include
pkg/fixtures.py:1:19: IMP301 [heuristic] `<module>` imports `faker` while `pkg.fixtures` loads, and the project declares `faker`, which provides it, only in the development group `dev`
Found 2 diagnostic(s) in 5 file(s).
```

heki matches a module to its distribution by name (`PyYAML` provides `yaml`, `Pillow` provides `PIL`). For any other module, `module-distributions` in `[tool.heki.imports]` says which distribution provides it.

## 🧱 Architecture import boundaries

`import-boundaries` says which modules may import which. It counts every import that runs, including one inside a function.

```toml
# heki.toml
[[import-boundaries]]
name = "domain-leaf"
importers = ["app.domain.*"]
forbidden-imports = ["app.web", "app.web.*"]
message = "the domain must not know the web layer"
fail = true
```

`app/domain/orders.py` imports the web layer inside a function:

```console
$ heki check
app/domain/orders.py:2:31: IMP201 (import-boundary-violation) [error, heuristic] `total` imports `app.web.views.render` when it runs, a forbidden import of `domain-leaf`
  |
2 |     from app.web.views import render
  |                               ^
  = help: the domain must not know the web layer

Found 1 diagnostic(s) in 5 file(s).
```

The run exits with code 1. A table can also list `allowed-imports` (with `<stdlib>` for the standard library) so that everything else is forbidden. With `unresolved = "report"`, an import whose module heki cannot name is reported as `IMP202`.

## 🔬 Inspecting the module load graph

`heki inspect --imports` takes a module name or a glob. For each project module it lists every import with the time it runs and what it loads, and what loading the module loads while it loads:

```console
$ heki inspect --imports app.domain.orders
app.domain.orders (app/domain/orders.py, open code)
  imports:
    2:31  from-import of `render`: app.web.views (project)  [function, in `total`, unconditional]
  loads while it loads, at least:
    project modules: none
    other modules: none
```

The import is in `total`, so it runs when `total` runs, and loading `app.domain.orders` loads no other module. Use it to see why an import cycle exists, or what an entry point pulls in at start-up. The command is experimental, like the rest of `heki inspect`.

## 📋 The rules and the contract

| Rule | Reports | Tier |
| --- | --- | --- |
| [`IMP101`](rules/import-cycle.md) | project modules that import each other while they load, once per group | heuristic, experimental, opt-in |
| [`IMP102`](rules/circular-import-failure.md) | a `from M import name` that raises `ImportError` when a module of its cycle is imported first | heuristic, experimental, opt-in |
| [`IMP104`](rules/development-code-import.md) | a module of the distributed package that imports test code, or a module outside the source roots of a `src` layout | heuristic, experimental, opt-in |
| [`IMP301`](rules/development-dependency-at-import-time.md) | a package module that loads, while it loads, a module provided only by a development dependency | heuristic, experimental, opt-in |
| [`IMP302`](rules/optional-dependency-at-import-time.md) | an import of a top-level package or an entry point's module that loads a module provided only by an extra | heuristic, experimental, opt-in |
| [`IMP201`](rules/import-boundary-violation.md) | an import that an `import-boundaries` contract forbids or does not allow | heuristic, experimental, contract |
| [`IMP202`](rules/unresolved-guarded-import.md) | strict mode of `import-boundaries`: an import whose module heki cannot name | heuristic, experimental, contract |

[Experimental Features](experimental.md) says why each one is marked. Which imports count for `IMP101` is the `cycle-edges` setting of `[tool.heki.imports]`: `"body"` by default, and `"lazy"`, `"function"`, and `"type-checking"` on request.

## 👉 Next steps

- [Configuration: import-boundaries](configuration.md#import-boundaries) lists every key of the contract.
- [Configuration: imports](configuration.md#imports-toolhekiimports) lists `cycle-edges` and `module-distributions`.
- [Adopting heki in a code base](adoption.md) shows how to start with a baseline.
- [State Ownership and Mutation](ownership.md) and [Async and Execution Effects](effects.md) cover the other contracts.
