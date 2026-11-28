# State Ownership and Mutation

heki finds in-place changes of read-only parameters, cached results, and frozen fields, and enforces your own rules about who may change, read, or call what.

Python has no owner for a value: any code that reaches an object can change it. Type checkers see a change made on a value of a read-only type. heki follows the value through aliases and helper calls, so it also sees the change that a function three calls below makes. This page has three parts: who may change, read, or call what, how to leave a borrowed or shared value alone, and values that may be used only once. A table at the end maps the ideas to the words Rust uses for them. [Contracts](configuration.md#contracts) has every key, and [Rules](rules/index.md) has one page per rule.

Each entry names its rule and its tier, as `heki rule --all` gives it. An exact rule states a fact about the code. A heuristic rule can be wrong about the code, runs with `--heuristic` (or `lint.heuristic = true`) when it is on by default, and its reports count as warnings. An opt-in rule is off until you name it in `extend-select`, for example `--extend-select MUT115`; a prefix such as `MUT` or `ALL` never enables it. An experimental rule can change or go away between releases, and [Experimental Features](experimental.md) says why each one is marked.

## 📜 Who may change, read, or call what

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

The run exits with code 1. `read-owners` and `call-owners` take the same shape with `allowed-readers` and `allowed-callers`. A target covers the subclasses of its class too, and a read or call through a value typed with a project `Protocol` that only the class and its subclasses implement: with `Order` the one class implementing `HasItems`, `cart.items` with `cart: HasItems` reads `Order.items` ([`read-owners`](configuration.md#read-owners)).

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

A contract names a method as `Class.method`, and a subclass that overrides the method is another function. Set `include-overrides = true` on a contract (or in its [defaults table](configuration.md#defaults-for-the-tables-of-a-kind)) and an entry naming a method in `allowed-*`, `forbidden-*`, `exempt-methods`, `invalidators`, or `exempt-writers` also names the overrides of it in the project subclasses of `Class`. The key is off by default because it allows a subclass written later to do what its base class may, with no change of the configuration. See [Allowed and forbidden functions](configuration.md#allowed-and-forbidden-functions).

`lru_cache` and `cache` results are protected without a contract. A `readonly-returns` entry adds the producers that are not cached, such as a function that returns a module-level dict.

## 🤝 Not breaking a value you borrowed or share

A function that receives a list did not make it, and a function that gets a cached result did not make that either. Changing such a value changes it for everyone who holds it. These rules report the change.

```toml
# heki.toml
[lint]
extend-select = ["MUT113", "MUT115"]
```

### A read-only parameter changed by a helper

A parameter annotated `Sequence`, `Mapping`, or `AbstractSet` promises that the function only reads it. `MUT110` reports a call that hands such a parameter, as itself, to a function that changes it in place. Tier: exact, default ([`MUT110`](rules/readonly-param-mutation.md)).

```python
# app/stock.py
from collections.abc import Sequence


def sort_names(names) -> None:
    if isinstance(names, list):
        names.sort()


def prepare(names: Sequence[str]) -> list[str]:
    sort_names(names)
    return list(names)
```

```console
$ heki check
app/stock.py:10:5: MUT110 (readonly-param-mutation) [warning] `prepare` passes its read-only parameter `names` to `app.stock.sort_names`; whenever line 6 of `sort_names` runs, it changes the caller's object in place
   |
10 |     sort_names(names)
   |     ^
  = related: app/stock.py:10:5: `prepare` calls `app.stock.sort_names`
  = related: app/stock.py:6:9: `sort_names` changes the object in place here
  = help: pass a copy (`list(names)` / `dict(names)` / `set(names)`), or annotate the helper's parameter with a mutable type
...
```

`MUT110` is a warning, because the bug it reports is rarely harmful, so it does not fail the run. Copy the parameter to a local list, sort the copy, and return it, so the caller's object stays as it was:

```python
def prepare(names: Sequence[str]) -> list[str]:
    ordered = list(names)
    sort_names(ordered)
    return ordered
```

Or annotate the helper's parameter with a mutable type, so the type checker shows the mismatch at the call.

### A cached result changed in place

A function under `lru_cache` or `cache`, or one that keeps its results in a memo dict it fills itself, hands the same list, dict, or set to every caller with the same arguments. `MUT114` reports a change of that object. Tier: exact, default ([`MUT114`](rules/cached-result-mutation.md)).

```python
# app/stock.py
from functools import lru_cache


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
app/stock.py:11:5: MUT114 (cached-result-mutation) [error] `tag` changes in place the list that `default_tags` returned; `default_tags` is cached by `functools.lru_cache`
   |
11 |     tags.append(item)
   |     ^
  = help: change a copy (`list(...)`) instead
...
```


A function that keeps its results in a memo it fills itself is a cache too: it stores what it computes from its arguments in a dict that outlives the call and is named as a cache or memo (`self._cache`, `_CACHE`), and a later call with the same arguments returns what the dict keeps. `MUT114` reports a change of that object as well.

```python
# app/settings.py
_CACHE = {}


def load(name: str) -> dict:
    if name not in _CACHE:
        _CACHE[name] = {"name": name, "debug": False}
    return _CACHE[name]


def debug_settings(name: str) -> dict:
    settings = load(name)
    settings["debug"] = True
    return settings
```

```console
$ heki check
app/settings.py:12:5: MUT114 (cached-result-mutation) [error] `debug_settings` changes in place the dict that `load` returned; `load` keeps it in `_CACHE`
   |
12 |     settings["debug"] = True
   |     ^
  = help: change a copy (`dict(...)`) instead
...
```

Such a report is exact when every return of the memo function hands out what a builtin `dict` holds under one key when it returns, and its return annotation names `list`, `dict`, or `set`. It is heuristic when the function stores the value on some paths only, removes or changes the entry before it returns, or keeps the memo in a class attribute. A memo that clears or drops the key before every read keeps nothing and gives no report. The name decides because the code does not: a get-or-create registry of the same shape whose dict is named otherwise (`_sessions`, `_BY_NAME`) is not taken for a cache.

`MUT114` is an error, so it fails the run. A producer that is not cached, such as a function that returns a module-level dict it never fills itself, is covered by a `readonly-returns` contract, which also takes precedence over a memo.

### A shared return value changed

For a value that is shared without a cache, name its producers in a `readonly-returns` contract, with `deep = true` to cover what is below the result. `MUT202` reports a change of what a producer returned, directly or through the functions that return it. Tier: heuristic, contract ([`MUT202`](rules/readonly-return-mutation.md)).

```toml
# heki.toml
[[readonly-returns]]
name = "schemas"
producers = ["registry.schema"]
deep = true
```

```python
# app/m.py
from registry import schema


def schema_of(name: str) -> dict[str, list[str]]:
    return schema(name)


def extend(name: str) -> None:
    s = schema_of(name)
    s["fields"].append("extra")
```

heki reports: `extend` mutates the shared object returned by read-only producer `registry.schema` through `app.m.schema_of`. Change a `copy.deepcopy` of the value instead.

### A shallow copy shares its items

`dict(x)`, `x.copy()`, and `copy.copy(x)` copy one level. The inner lists are the same objects, so a change below the copy changes the original too. `MUT115` reports it when the original is state that outlives the call. Tier: heuristic, opt-in ([`MUT115`](rules/shallow-copy-nested-mutation.md)).

```python
def with_base(options: dict[str, list[str]], base: str) -> dict[str, list[str]]:
    result = options.copy()
    result["bases"].append(base)  # changes the list `options["bases"]` holds
    return result
```

heki reports: `with_base` changes in place an object that its shallow copy of `options` (`.copy()`, line 2) shares with `options`. Rebuild the inner value instead: `result["bases"] = [*result["bases"], base]`.

### A container changed while it is iterated

A dict, set, or deque that a loop is iterating raises `RuntimeError` at the next step once something changes it. `MUT113` reports a method that suspends at a `yield` inside such a loop while another method changes the container. Tier: heuristic, experimental, opt-in ([`MUT113`](rules/iterated-container-mutation.md)).

```python
class Subscriptions:
    def __init__(self) -> None:
        self.by_node: dict[str, object] = {}
        self._cursor = self._cycle()

    def _cycle(self):
        while self.by_node:
            yield from self.by_node.values()  # MUT113

    def next_subscriber(self) -> object | None:
        subscriber = next(self._cursor, None)
        if subscriber is None:
            self._cursor = self._cycle()  # no subscribers: return None, start over next time
        return subscriber

    def drop(self, node: str) -> None:
        self.by_node.pop(node, None)
```

heki reports: `Subscriptions._cycle` suspends at a `yield` while iterating `self.by_node`, which `Subscriptions.drop` changes in place, so resuming it after that change raises `RuntimeError`. Iterate over a snapshot: `yield from list(self.by_node.values())`.

### Other rules about values that change under you

| Rule | Reports | Tier |
| --- | --- | --- |
| [`MUT119`](rules/change-lost-on-new-object.md) | an in-place change of a new container that nothing keeps, such as `list(xs).sort()`: the change is lost | exact, default |
| [`MUT118`](rules/sentinel-lost-in-deepcopy.md) | `copy.deepcopy` of an instance whose `object()` sentinel a function tests by identity | heuristic, runs with `--heuristic` |
| [`MUT108`](rules/readonly-type-mutation.md) | an in-place change of a field that a standard type declares read-only: a frozen dataclass, a `NamedTuple`, `Final` and `ReadOnly` attributes, `ReadOnly` `TypedDict` keys | heuristic, opt-in |
| [`MUT107`](rules/hash-attr-mutation.md) | a method other than an initializer that changes an attribute the class's `__hash__` reads | heuristic, opt-in |
| [`MUT120`](rules/readonly-view-mutation.md) | a change of a value held through a read-only view (a protocol of properties without setters) that a type checker does not see | heuristic, experimental, opt-in |
| [`MUT112`](rules/conditional-ownership-mutation.md) | an in-place change of a value that is new on one path and borrowed state on another | heuristic, experimental, opt-in |
| [`MUT101`](rules/cached-property-stale-mutation.md), [`MUT111`](rules/cached-property-stale-read.md) | an attribute that a `@cached_property` reads, changed without invalidating the cache, and a read of such a stale property | exact, experimental, opt-in |

A write is a plain store, one premise of an exact `MUT101` or `MUT111` report, only when every base of the class is a project class, a plain one, or a library base the ecosystem catalog lists as keeping what its subclasses' code stores in the instance. Django's `models.Model` is one: neither it nor its metaclass defines attribute hooks, so a write of an attribute a model's code sets (in `__init__` or a method) is a plain store, and a stale `@cached_property` over it is an exact report once the rules are switched on. A class-body field (`expires = models.DateTimeField()`, which Django replaces with a descriptor) and `pk` (a property of `Model`) keep the report heuristic.

## 🔂 Single-use values used twice or not at all

An iterator or a generator gives each item once. Reading it a second time sees nothing, and a generator that nobody iterates never runs its body.

### An iterator consumed twice

`rows` below is a generator expression. `sum` uses it up, and the list comprehension after it sees no items. `MUT106` reports the second pass. Tier: heuristic, opt-in ([`MUT106`](rules/iterator-double-consumption.md)).

```python
def summarize(lines: list[str]) -> tuple[int, list[str]]:
    rows = (line.strip() for line in lines)
    count = sum(1 for _ in rows)
    kept = [r for r in rows if r]  # MUT106: `rows` was consumed by `sum`
    return count, kept
```

heki reports: `rows` is a single-pass iterator already consumed at line 3, so this pass sees no items. Build a list when the value is read more than once.

### A generator that is never iterated

Calling a generator function only creates the generator. Its body runs when something iterates it. `EFF116` reports a call whose generator is dropped: a statement, a value only tested for truth, a function handed to a thread, an `atexit` handler, or a `filter` predicate. Tier: exact, default, a warning; its heuristic reports, where a call may run another implementation, appear with `--heuristic` ([`EFF116`](rules/generator-never-iterated.md)).

```python
def parse_extra(text: str):
    for line in text.splitlines():
        yield line.strip()


def check(text: str) -> None:
    if not parse_extra(text):  # EFF116
        print("nothing to parse")
```

heki reports: only a truth test reads what `parse_extra` returns, and `parse_extra` is a generator function: a generator is always true, and the body does not run. Iterate it, as in `if not list(parse_extra(text))`.

## 👉 Next steps

- [Configuration: contracts](configuration.md#contracts) lists every key of the five contracts.
- [Adopting heki in a code base](adoption.md) shows how to start with a baseline.
- [Adoption recipes](adoption.md#starter-recipes) have contracts to copy for a web service and a layered application.
- [Coding agents](coding-agents.md) shows how the same contracts keep an agent out of the files it must not change.
- `heki inspect app.cart.remove` (experimental) shows what one function changes, keeps, and returns.

## 🦀 If you know Rust

Nothing here is enforced when Python compiles the code. heki reports the places where the code breaks the expectation that a Rust compiler would hold it to. [What Python leaves open](philosophy.md#what-python-leaves-open) puts these next to the effect and immutability checks.

| Rust | Closest in heki | How Python differs |
| --- | --- | --- |
| Ownership: one owner decides who may change a value | `write-owners`, `read-owners`, `call-owners` (`MUT201`, `MUT203`, `MUT204`) | Any code can reach any object. A contract names the owners, and heki reports the code outside them. |
| Shared borrow `&T` | `Sequence`, `Mapping`, `AbstractSet`, and `ReadOnly` parameters (`MUT110`); shared return values (`MUT114`, `MUT202`) | The annotation promises nothing at run time. heki reports the helper that changes the value anyway. |
| Exclusive borrow `&mut T`, no change while borrowed | a container changed while it is iterated (`MUT113`); a shallow copy that shares its items (`MUT115`) | Nothing stops the change, and it shows up later as a `RuntimeError` or a surprising value. |
| Move: a moved value cannot be used again | a single-use iterator or generator (`MUT106`, `EFF116`) | The name stays usable after the value is used up, and the second use sees nothing. |
| Lifetime: a reference does not outlive what it points to | cleanup that cancellation cuts short ([`EFF118`](effects.md#resource-and-cancellation-lifetimes)) | A Python object stays alive while a name refers to it, so nothing dangles. What can end early is the work around a resource. |
