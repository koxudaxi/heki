# State Ownership and Mutation

heki finds in-place changes of read-only parameters, cached results, and frozen fields, and enforces your own rules about who may change, read, or call what.

Type checkers see a change made on a value of a read-only type. heki follows the value through aliases and helper calls, so it also sees the change that a function three calls below makes. This page lists what runs without configuration, what you have to switch on, and which contract fits which rule. [Contracts](configuration.md#contracts) has every key, and [Rules](rules/index.md) has one page per rule.

## 🔍 What runs without configuration

These rules need no `heki.toml`. The first three are exact: a report states a fact about the code, and an error fails `heki check`.

| Rule | Reports | Tier |
| --- | --- | --- |
| [`MUT110`](rules/readonly-param-mutation.md) | a `Sequence`, `Mapping`, `AbstractSet`, or `ReadOnly` parameter handed, as itself, to a function that changes it in place | exact, default |
| [`MUT114`](rules/cached-result-mutation.md) | an in-place change of the list, dict, or set that an `lru_cache` or `cache` function returned | exact, default |
| [`MUT119`](rules/change-lost-on-new-object.md) | an in-place change of a new container that nothing keeps, such as `list(xs).sort()`: the change is lost | exact, default |
| [`MUT118`](rules/sentinel-lost-in-deepcopy.md) | `copy.deepcopy` of an instance whose `object()` sentinel a function tests by identity | heuristic, runs with `--heuristic` |

Heuristic rules can be wrong about the code. They run with `--heuristic` (or `lint.heuristic = true`), and their reports count as warnings.

A function that hands its `Sequence` parameter to a helper that sorts it, and a function that appends to a cached list:

```python
# app/stock.py
from collections.abc import Sequence
from functools import lru_cache


def sort_names(names) -> None:
    if isinstance(names, list):
        names.sort()


def prepare(names: Sequence[str]) -> list[str]:
    sort_names(names)
    return list(names)


@lru_cache(maxsize=None)
def default_tags(shop: str) -> list[str]:
    return ["new", shop]


def tag(shop: str, item: str) -> list[str]:
    tags = default_tags(shop)
    tags.append(item)
    return tags
```

```console
$ heki check
app/stock.py:11:5: MUT110 (readonly-param-mutation) [warning] `prepare` passes its read-only parameter `names` to `app.stock.sort_names`; whenever line 7 of `sort_names` runs, it changes the caller's object in place
   |
11 |     sort_names(names)
   |     ^
  = related: app/stock.py:11:5: `prepare` calls `app.stock.sort_names`
  = related: app/stock.py:7:9: `sort_names` changes the object in place here
  = help: pass a copy (`list(names)` / `dict(names)` / `set(names)`), or annotate the helper's parameter with a mutable type

app/stock.py:22:5: MUT114 (cached-result-mutation) [error] `tag` changes in place the list that `default_tags` returned; `default_tags` is cached by `functools.lru_cache`
   |
22 |     tags.append(item)
   |     ^
  = help: change a copy (`list(...)`) instead
...
```

This output is from heki 0.9.0. `MUT110` is a warning because the bug it reports is rarely harmful, and `MUT114` is an error, so only `MUT114` fails the run.

## 🔧 Rules to switch on

These rules are off by default. Name one in `extend-select`, or on the command line with `--extend-select MUT108`. A prefix such as `MUT` or `ALL` never enables them.

```toml
# heki.toml
[lint]
extend-select = ["MUT108", "MUT115"]
```

| Rule | Reports | Tier |
| --- | --- | --- |
| [`MUT108`](rules/readonly-type-mutation.md) | an in-place change of a field that a standard type declares read-only: a frozen dataclass, a `NamedTuple`, `Final` and `ReadOnly` attributes, `ReadOnly` `TypedDict` keys | heuristic, opt-in |
| [`MUT115`](rules/shallow-copy-nested-mutation.md) | a change below a shallow copy (`dict(x)`, `x.copy()`, `copy.copy(x)`) of state that outlives the call: the copy holds the same inner object | heuristic, opt-in |
| [`MUT106`](rules/iterator-double-consumption.md) | a single-pass iterator or generator consumed a second time, which sees no items | heuristic, opt-in |
| [`MUT107`](rules/hash-attr-mutation.md) | a method other than an initializer that changes an attribute the class's `__hash__` reads | heuristic, opt-in |
| [`MUT120`](rules/readonly-view-mutation.md) | a change of a value held through a read-only view (a protocol of properties without setters) that a type checker does not see | heuristic, experimental, opt-in |
| [`MUT112`](rules/conditional-ownership-mutation.md) | an in-place change of a value that is new on one path and borrowed state on another | heuristic, experimental, opt-in |
| [`MUT113`](rules/iterated-container-mutation.md) | a dict, set, or deque of the instance changed while a method iterates it | heuristic, experimental, opt-in |
| [`MUT101`](rules/cached-property-stale-mutation.md), [`MUT111`](rules/cached-property-stale-read.md) | an attribute that a `@cached_property` reads, changed without invalidating the cache, and a read of such a stale property | exact, experimental, opt-in |

Experimental rules can change or go away between releases. [Experimental Features](experimental.md) says why each one is marked.

## 📜 Contracts: your own ownership rules

Some rules belong to your project, not to Python: only `app.store` may append to `Order.items`, only `app.billing` reads a card token, nothing changes the settings object that `load_settings` returns. Write them down once and `heki check` enforces them across files, aliases, and helpers.

| You want | Contract | Rules | Tier |
| --- | --- | --- | --- |
| Only some functions change an attribute or container | `write-owners` | [`MUT201`](rules/write-owner-violation.md), [`MUT205`](rules/unresolved-guarded-write.md) | heuristic, contract |
| Only some functions read an attribute | `read-owners` | [`MUT203`](rules/read-owner-violation.md), [`MUT206`](rules/unresolved-guarded-read.md) | heuristic, contract |
| Only some functions call a function, method, or class | `call-owners` | [`MUT204`](rules/call-owner-violation.md), [`MUT207`](rules/unresolved-guarded-call.md) | heuristic, contract |
| Nobody changes a shared value that a producer returns | `readonly-returns` | [`MUT202`](rules/readonly-return-mutation.md) | heuristic, contract |
| Every change of an attribute is followed by a call that announces it | `invalidation` | [`MUT208`](rules/write-without-invalidation.md) | heuristic, contract |

The second rule of each owner contract (`MUT205`, `MUT206`, `MUT207`) is its strict mode. It reports what heki cannot place: an attribute named like a target that is written on an object of unknown class. Set `unresolved = "report"` in the contract to get it.

A contract rule is on as soon as a contract table exists. Its reports are warnings until you set `fail = true` in the table. Record today's findings in a [Ratchet baseline](adoption.md#ratchet-baselines) first, so only new breaks fail.

### Only the store changes the order

```toml
# heki.toml
[[write-owners]]
name = "order-store"
targets = ["app.models.Order.items"]
allowed-writers = ["app.store.*"]
message = "change Order.items through app.store"
fail = true
```

A function in `app/api.py` appends to `order.items` through a helper:

```console
$ heki check
app/api.py:9:5: MUT201 (write-owner-violation) [error, heuristic] `add_gift_wrap` writes `app.models.Order.items` (through a helper) via `app.api._append` outside the write owners of `order-store`
  |
9 |     _append(order.items, "gift-wrap")
  |     ^
  = related: app/api.py:9:5: `add_gift_wrap` calls `app.api._append`
  = help: change Order.items through app.store; allowed writers of `app.models.Order.items`: `app.store.add_item`

Found 1 diagnostic(s) in 4 file(s).
```

The run exits with code 1. `read-owners` and `call-owners` take the same shape with `allowed-readers` and `allowed-callers`.

### Every change must be announced

```toml
# heki.toml
[[invalidation]]
name = "order-total"
targets = ["app.models.Order.items"]
invalidators = ["app.cache.order_changed"]
message = "call order_changed() after changing Order.items"
```

`add` appends and then calls `order_changed()`. `remove` does not:

```console
$ heki check
app/cart.py:11:5: MUT208 (write-without-invalidation) [error, heuristic] `remove` writes `app.models.Order.items` (in place) and returns on some path without calling an invalidator of `order-total`
   |
11 |     order.items.remove(name)
   |     ^
  = help: call order_changed() after changing Order.items; announcing writers of `app.models.Order.items`: `app.cart.add`
...
```

Without `fail = true` this run exits with code 0, because a heuristic report counts as a warning.

### Which contract do I need?

| If the rule you have in mind says | Use |
| --- | --- |
| "only X may change this field" | `write-owners` |
| "only X may read this field" | `read-owners` |
| "only X may call this function" or "only X may construct this class" | `call-owners` |
| "nobody may change what this function returns" | `readonly-returns`, with `producers = [...]` and `deep = true` for what is below the result |
| "after changing this field, always call that function" | `invalidation` |
| "this code must not write, read, or call anything of that kind, however deep" | `effect-boundaries`, see [Async and Execution Effects](effects.md#contracts-for-execution-effects) |

`lru_cache` and `cache` results are protected without a contract. A `readonly-returns` entry adds the producers that are not cached, such as a function that returns a module-level dict.

## 👉 Next steps

- [Configuration: contracts](configuration.md#contracts) lists every key of the five contracts.
- [Adopting heki in a code base](adoption.md) shows how to start with a baseline.
- [Adoption recipes](adoption.md#starter-recipes) have contracts to copy for a web service and a layered application.
- [Coding agents](coding-agents.md) shows how the same contracts keep an agent out of the files it must not change.
- `heki inspect app.cart.remove` (experimental) shows what one function changes, keeps, and returns.
