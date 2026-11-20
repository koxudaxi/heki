# Scope and Limitations

heki checks what Python code does across functions and modules: which objects it mutates,
which references it shares, and which blocking, network, or database work it reaches. This page
says where heki fits next to a linter and a type checker, where its knowledge comes from, what
it does when it cannot tell, and what it does not try to do.

## Where heki fits

| Tool | Looks at | Example |
| ---- | -------- | ------- |
| A linter such as [Ruff](https://docs.astral.sh/ruff/) | Syntax and style, one file at a time | `time.sleep()` written directly in an `async def`; a discarded `asyncio.create_task(...)` |
| A type checker such as [ty](https://github.com/astral-sh/ty) | Whether values match their declared types | a `str` passed where an `int` is expected |
| An import-graph checker such as [tach](https://github.com/gauge-sh/tach) or [import-linter](https://github.com/seddonym/import-linter) | Which modules may import which | `app.api` importing `app.db` when the declared layers forbid it |
| heki | What code does, through any number of calls | an `async def` that reaches `requests.get` three helpers away; a model field written outside the store that owns it; an attribute `__hash__` reads changing after the object went into a `set` |

heki's rules are built around what one file, one type, or one import cannot show. Where a Ruff
rule already covers a case, heki leaves it to Ruff: `EFF107` reports only tasks bound to a
variable, because Ruff's `RUF006` reports the discarded ones.

### Clean for Ruff, ty, and tach; broken all the same

```python
# app/clients.py
import requests


def fetch_json(url: str) -> dict:
    return requests.get(url, timeout=5).json()


# app/users.py
from app.clients import fetch_json


def load_profile(user_id: int) -> dict:
    return fetch_json(f"https://api.example.com/users/{user_id}")


def load_user(user_id: int) -> dict:
    return {"id": user_id, **load_profile(user_id)}


# app/api.py
from app.users import load_user


async def handler(user_id: int) -> dict:
    return load_user(user_id)  # blocks the event loop three calls down
```

```python
# app/models.py: only `Store.*` may write `Model.fields` (a write-owners contract)
class Model:
    def __init__(self) -> None:
        self.fields: list[str] = []


class Store:
    def add_field(self, model: Model, name: str) -> None:
        model.fields.append(name)


def add_default(fields: list[str]) -> None:
    fields.append("id")


def parse(model: Model) -> None:
    fields = model.fields
    add_default(fields)  # changes `Model.fields` behind the store's back
```

```toml
[[tool.heki.write-owners]]
name = "model-store"
targets = ["Model.fields"]
allowed-writers = ["Store.*"]
```

`ruff check --select ASYNC,B,RUF,PERF,PL,SIM,FURB,S`, `ty check`, and `tach check` (with
`app.api` → `app.users` → `app.clients` declared) all pass. heki reports both problems:

```console
$ heki check --output-format concise
app/api.py:5:12: EFF101 `async def handler` calls `app.users.load_user`, which reaches a blocking call `requests.get` (blocking_net) on the event loop; offload it (`asyncio.to_thread`) or use an async API
app/models.py:17:5: MUT201 `parse` writes `Model.fields` (indirect_helper) via `app.models.add_default` outside the write owners of `model-store`
Found 2 diagnostic(s) in 5 file(s).
```

Ruff's async rules see `requests.get` only when it is written in the `async def` itself; ty sees a
`list[str]` passed where a `list[str]` is expected; tach sees `app.api` importing `app.users`, which
is allowed. None of them follows the call into `load_user` or the alias into `add_default`.

## Where heki's knowledge comes from

1. **Its own analysis.** heki builds an SSA representation of every function, tracks aliases
   (copies, attributes, and the iterators and views derived from an object), summarizes what
   each function mutates and which effects it reaches, and propagates the summaries over the
   project's call graph until nothing changes.
2. **A catalog of library behavior.** The effects of standard-library and third-party APIs are
   declared, not inferred: `requests.get` blocks on the network, `asyncio.to_thread(fn)` and
   `@sync_to_async` run a function on another thread, `logging` calls are never reported, `sqlite3.connect()` returns a
   `Connection`, `threading.Lock` is a synchronous lock. heki does not analyze the source of
   the standard library or of installed packages. Add your own APIs with
   [`[[tool.heki.ecosystem]]`](configuration.md#ecosystem-knowledge-toolhekiecosystem).
3. **Contracts you declare.** What no type can say is declared once in configuration: who may
   write or read an attribute or call a function (`write-owners`, `read-owners`,
   `call-owners`), which returned objects are shared (`readonly-returns`), which changes must be
   announced to an invalidator (`invalidation`), and which functions must not reach I/O or writes
   to model state (`effect-boundaries`). heki needs no decorators, no special types, and nothing at run time.
4. **Type annotations, as hints only.** The checking rules (`MUT`, `EFF`, `DATA`, `OPT112`,
   `OPT113`) use annotations to learn which class an object has and which method a call reaches
   (`session: Session` makes `session.execute(...)` a database call). Calls are resolved from
   local constructors and imports first, then annotations, then attributes assigned in
   `__init__`, then the class hierarchy (up to eight overriding methods). Container item types
   (`list[Model]`), return annotations along the MRO, `isinstance` narrowing, unions of up to
   three classes, and project `Protocol`s are followed too. The rewriting rules never read
   annotations, because a subclass can override any method; see
   [Fixes and Safety](fixes.md).

## When heki cannot tell

The two kinds of rules make opposite choices when information is missing:

- **A rewrite needs a proof.** An `OPT` fix is applied only when every proof obligation holds.
  Anything heki cannot see counts as shared and blocks the fix; the diagnostic then names the
  obligation that failed (`[not auto-fixable: ...]`).
- **A report needs evidence.** The checking rules report only what they can show. A call heki
  cannot resolve is not followed: a value of type `Any`, an unannotated parameter, a method
  name computed at run time, more than eight possible overrides, or a function outside the
  checked files that the catalog does not list. heki would rather miss a finding than report
  one it cannot back with a call chain; reports that cross functions show that chain as
  related locations. A branch the caller's arguments switch off is not followed
  (`load(name, cache=False)` does not reach the request under `if cache:`), and a client or
  query created only while an attribute is unset counts once per object, not once per loop
  iteration.

The missed findings close as heki learns more: annotate values whose class it cannot see, add
your blocking or offloading APIs to `[[tool.heki.ecosystem]]`, and declare contracts for the
state that matters.

## Known limitations

- **Dynamic names.** `setattr(obj, "fields", v)`, `getattr(obj, "fields")`,
  `getattr(obj, "method")(...)`, and `obj.__dict__["fields"]` are followed only when the name is a
  constant: a literal, or a module-level `Final` string constant (also imported). Names assigned
  without `Final`, class-level constants, and computed names are not resolved; a `write-owners`
  contract with `unresolved = "report"` counts the writes heki cannot tie to a class (`MUT205`).
- **Objects without a known class.** Reads and writes through `Any` or unannotated parameters
  are not tied to a class, so contracts on that class do not see them. A write in an
  unannotated helper is still found where a caller passes it an object of a known class; a
  read is not.
- **Names the catalog does not list.** Catalog lookups are exact: a class reached through another
  public name (`from sqlalchemy import Engine` for `sqlalchemy.engine.Engine`, `redis.StrictRedis`
  for `redis.Redis`) or a method the catalog does not list contributes no effect, and objects
  reached through subscripts or framework attributes (`client["app"]["users"]`, Django's
  `Model.objects`) have no known class. Add what is missing with `[[tool.heki.ecosystem]]`.
- **Properties with a setter or deleter.** Reading a `@property` or `functools.cached_property`
  of a project class runs its getter: its effects, writes, and the attributes it reads count for
  the reader, also through base classes, overriding subclasses, and `super()`. A property that
  also defines a setter or deleter (`@x.setter`) is not followed, nor is `x = property(get_x)`,
  and the getter's return annotation types the value read. Values declared `Any`, generics
  substituted through `TypeVar`s, and calls of `type[C]` attributes are not typed.
- **Read-only types.** `MUT110` follows `Sequence` / `Mapping` / `ReadOnly` parameters into the
  helpers they are handed to, any number of calls deep; a helper parameter annotated with a
  mutable type is left to the type checker, and a container inside the argument does not count.
  `MUT108` (opt-in) follows frozen dataclasses, `NamedTuple`s, classes that a
  `dataclass_transform(frozen_default=True)` in the checked files builds, `Final` / `ReadOnly`
  attributes, and `ReadOnly` `TypedDict` keys; transforms of installed packages (attrs,
  Pydantic) are not seen, and an instance the function constructs itself counts as under
  construction.
- **Generators.** A generator's body runs where it is iterated: a `for` loop or comprehension,
  unpacking, `yield from`, a consuming call (`list(...)`, `"".join(...)`, `items.extend(...)`), a
  lazy wrapper iterated there (`enumerate(...)`), or a project function iterating it; one a
  function returns runs where its caller iterates it. A generator handed to anything else
  (`StreamingResponse(rows())`, a container), only stepped with `next()`, or unpacked into a call
  (`f(*rows())`) is not followed; a generator expression counts where it is written. A coroutine
  runs where it is awaited; one only scheduled (`asyncio.create_task`) is checked on its own.
  Iterating an object runs its class's `__iter__` only for classes the catalog lists (a
  SQLAlchemy `Query`); the `__iter__` methods of project classes are not followed.
- **Guards.** A test is followed when it checks the function's own input directly: truthiness,
  `is None`, or `isinstance` with builtin classes. `a or b` gives no guard on either value, the
  classes of project objects and annotated types do not decide `isinstance`, a lazy
  initialization needs a direct test and the store in the function itself, and `EFF201` does not
  use guards. An object a factory function returns counts as obtained once, like a pooled
  connection.
- **Separate scopes.** Generator expressions and lambdas are analyzed as functions of their
  own, so `next(b for b in bs if b.id == a.id)` inside a loop is not reported as a nested-loop
  join.
- **Metaprogramming and frameworks.** `__getattr__` proxies, metaclasses, classes created at
  run time, and framework machinery (validators, ORM lazy loading, dependency injection) are not
  modeled, and heki has no framework-specific rules.
- **The target Python version.** Results follow `target-version`. From Python 3.12, list, set,
  and dict comprehensions run in the enclosing function (PEP 709), so a comprehension inside a
  loop can be reported for `py312` and not for `py311`. heki infers the version from
  `requires-python`, which keeps it at the oldest version you support.
- **Sizes known only at run time.** Whether a collection is small is decided from the code: a
  list built from a short constant display is not reported, a list built at run time is, even
  when it always holds three items. A container counts as small only when it is a display of at
  most 8 items, with no `*` / `**` unpacking, that nothing grows or lets escape (shrinking it
  and read-only builtins are fine). This holds also through local names, module constants, and
  other modules' module-level displays and class attributes. Such rules are marked
  size-dependent.
- **Callers outside the checked files.** Contracts reason about the callers heki sees: a
  private helper hands its reports to its callers, and a function called only from allowed
  writers is not reported. Code outside the run, such as tests left out of it, is not known.

## How a rule gets into heki

heki only takes checks that need what it can prove. A proposed rule goes through four questions,
in order:

1. **Can the syntax of one file decide it?** Then it belongs in a linter such as Ruff.
2. **Is it a type error, or can an annotation express it?** Then it belongs in a type checker
   such as ty.
3. **Is it about which modules may import which?** Then it belongs in tach or import-linter.
4. **Does it need at least one of the proofs below?** Only then is it a heki rule. It is measured
   on a corpus and reviewed by hand before it is enabled by default, and where another tool
   covers part of it, heki keeps only the rest (`EFF107` and Ruff's `RUF006`).

The proofs heki can make, on five axes:

| Axis | What heki establishes | Used by |
| ---- | --------------------- | ------- |
| Control flow | which statements can run after which: CFG paths, loops and their bounds, exception edges | `MUT106`, `MUT109`, `EFF102`, `EFF103`, `EFF106`, `DATA101`, `OPT` |
| Calls | which functions a call reaches across modules, and what they do (summaries propagated over the call graph) | `EFF101`, `EFF201`, `MUT103`, `MUT108`, `MUT109`, `MUT110`, `MUT201`–`MUT204` |
| Aliases | which names, iterators, and views refer to one object; whether it is unique to its frame and of an exact builtin type | `MUT202`, `MUT201`, `MUT204`, `MUT108`, `MUT110`, `EFF107`, `OPT101`–`OPT130` |
| Effects | which blocking, network, file, or database work runs, from the catalog of library behavior | `EFF101`–`EFF106`, `EFF201`, `DATA101` |
| State | which attributes and containers a function reads, writes, or mutates, and who may do so | `MUT101`, `MUT107`, `MUT108`, `MUT109`, `MUT110`, `MUT201`–`MUT204` |

## What heki does not do

- **Style, formatting, and syntax checks:** use a linter such as Ruff.
- **Type errors:** use a type checker such as ty.
- **Import layering and import cycles:** use [tach](https://github.com/gauge-sh/tach) or
  [import-linter](https://github.com/seddonym/import-linter), and Python 3.15's
  `lazy import`.
- **Dead code and duplicated code:** use Ruff or [vulture](https://github.com/jendrikseipp/vulture).
- **Constant-factor micro-optimizations**, such as hoisting `re.compile`.
- **Changing your code's shape:** heki adds no decorators, contract types, or runtime checks,
  and does not compile or rewrite code at build time.
- **Running your program:** heki never runs or instruments it; `--profile` only reads a
  profile dump you recorded yourself.

## Where heki pays off

- Async services and web or database code written in layers of helpers: blocking calls that
  reach the event loop, clients and queries created inside loops (N+1), and I/O while a lock or
  a transaction is held.
- Large code bases that protect shared state by convention, review, or their own checkers:
  contracts turn those conventions into checks that follow aliases, helpers, and subclasses.
- Code with loops over growing collections: proven-safe rewrites and suggestions for quadratic
  patterns.

In small scripts, or code that is mostly synchronous and self-contained, a linter and a type
checker cover most of what matters, and heki reports little.
