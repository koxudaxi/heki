# Scope and Limitations

This page says what heki checks, what it leaves to other tools, where its knowledge comes from,
and what it does when it cannot tell.

## Where heki fits

| Tool | Looks at | Example |
| ---- | -------- | ------- |
| A linter such as [Ruff](https://docs.astral.sh/ruff/) | Syntax and style, one file at a time | `time.sleep()` written directly in an `async def`; a discarded `asyncio.create_task(...)` |
| A type checker such as [ty](https://github.com/astral-sh/ty) | Whether values match their declared types | a `str` passed where an `int` is expected |
| An import-graph checker such as [tach](https://github.com/gauge-sh/tach) or [import-linter](https://github.com/seddonym/import-linter) | Which modules may import which | `app.api` importing `app.db` when the declared layers forbid it |
| heki | What code does, not what types it has: effects and aliases, through any number of calls | an `async def` that reaches `requests.get` three helpers away; a model field written outside the store that owns it; an attribute `__hash__` reads changing after the object went into a `set` |

The difference is the model, not reaching across files: heki models effects and aliases, and
leaves syntax, types, and imports to the tools that model them, also as they grow. Where another
tool covers part of a case, heki keeps the rest: Ruff's `RUF006` reports discarded tasks, so
`EFF107` reports only the tasks bound to a variable.

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

Each of these passes, because each tool sees one step:

| Command | Sees | Does not see |
| ------- | ---- | ------------ |
| `ruff check --select ASYNC,B,RUF,PERF,PL,SIM,FURB,S` | `requests.get` only when it is written in the `async def` itself | the call three helpers down |
| `ty check` | a `list[str]` passed where a `list[str]` is expected | that `add_default` changes `Model.fields` |
| `tach check` (with `app.api` → `app.users` → `app.clients` declared) | `app.api` importing `app.users`, which is allowed | what `load_user` does |

heki follows the call into `load_user` and the alias into `add_default`:

```console
$ heki check --heuristic --output-format concise
app/api.py:5:12: EFF101 [heuristic] `handler` reaches the blocking call `requests.get` on the event loop
app/models.py:17:5: MUT201 [heuristic] `parse` writes `Model.fields` (indirect_helper) via `app.models.add_default` outside the write owners of `model-store`
Found 2 diagnostic(s) in 4 file(s).
```

## Where heki pays off

| Code | What heki finds |
| ---- | --------------- |
| Async services, and web or database code written in layers of helpers | blocking calls that reach the event loop, clients and queries created inside loops (N+1), I/O while a lock or a transaction is held |
| Large code bases that protect shared state by convention, review, or their own checkers | contract violations, followed through aliases, helpers, and subclasses |
| Loops over growing collections | proven-safe rewrites and suggestions for quadratic patterns |

In small scripts, or code that is mostly synchronous and self-contained, a linter and a type
checker cover most of what matters, and heki reports little.

## What heki does not do

| Check | Use instead |
| ----- | ----------- |
| Style, formatting, and syntax | a linter such as Ruff |
| Type errors | a type checker such as ty |
| Import layering and import cycles | [tach](https://github.com/gauge-sh/tach) or [import-linter](https://github.com/seddonym/import-linter), and Python 3.15's `lazy import` |
| Dead code and duplicated code | Ruff or [vulture](https://github.com/jendrikseipp/vulture) |
| Constant-factor micro-optimizations, such as hoisting `re.compile` | no heki rule |

- heki does not change your code's shape: it adds no decorators, contract types, or runtime
  checks, and it does not compile or rewrite code at build time.
- heki never runs or instruments your program. `--profile` only reads a profile you recorded
  yourself.

## Where heki's knowledge comes from

| Source | What it tells heki | Extend it with |
| ------ | ------------------ | -------------- |
| Its own analysis | an SSA form of every function; aliases (copies, attributes, and the iterators and views derived from an object); what each function mutates and which effects it reaches, propagated over the project's call graph until nothing changes | - |
| A catalog of library behavior | `requests.get` blocks on the network; `asyncio.to_thread(fn)` and `@sync_to_async` run a function on another thread; `logging` calls are never reported; `sqlite3.connect()` returns a `Connection`; `threading.Lock` is a synchronous lock | [`[[tool.heki.ecosystem]]`](configuration.md#ecosystem-knowledge-toolhekiecosystem) |
| Contracts you declare | who may write, read, or call (`write-owners`, `read-owners`, `call-owners`); which returned objects are shared (`readonly-returns`); which changes must be announced (`invalidation`); which functions must not reach I/O or writes (`effect-boundaries`) | [contracts](configuration.md#contracts) in the configuration |
| Type annotations, as hints only | which class an object has and which method a call reaches: `session: Session` makes `session.execute(...)` a database call | annotations in your code |

- heki does not analyze the source of the standard library or of installed packages: the
  catalog declares their effects.
- Contracts need no decorators, no special types, and nothing at run time.
- The checking rules (`MUT`, `EFF`, `DATA`, `OPT112`, `OPT113`) use annotations. The rewriting
  rules never read them, because a subclass can override any method (see
  [Fixes and Safety](fixes.md#what-a-fix-must-prove)).

### How a call is resolved

heki looks for the callee in this order:

1. local constructors and imports;
2. annotations;
3. attributes assigned in `__init__`;
4. the class hierarchy, up to eight overriding methods.

It also follows container item types (`list[Model]`), return annotations along the MRO,
`isinstance` narrowing, unions of up to three classes, and project `Protocol`s.

## When heki cannot tell

A rewrite and an exact report make the same choice when information is missing, in opposite
directions; a heuristic report makes it only for some premises:

| | A rewrite (`OPT` fix) | An exact report | A heuristic report |
| - | --------------------- | --------------- | ------------------ |
| Needs | a proof | every premise of its claim established | evidence, with some premises open |
| What heki cannot see | counts as shared and blocks the fix | stops the report | stops it for an unresolved call; does not stop it for the premises its rule page marks open |
| What you get | the diagnostic names the failed obligation: `[not auto-fixable: ...]` | no report: a missed finding, never a guess | a report labelled `heuristic`, which can be wrong about the code |

Each rule page lists the premises of its claim under "What a report states", and which of them
a heuristic report may not have established ([Rule Precision](precision.md)).

A report that crosses functions shows its call chain as related locations. A call heki cannot
resolve is not followed:

- a value of type `Any`, or an unannotated parameter;
- a method name computed at run time;
- a call with more than eight possible overrides;
- a function outside the checked files that the catalog does not list.

Two more cases stay quiet by design:

- A branch the caller's arguments switch off is not followed: `load(name, cache=False)` does not
  reach the request under `if cache:`.
- A client or query created only while an attribute is unset counts once per object, not once
  per loop iteration, and so does a `functools.cached_property` getter; one created only while
  a module global or a local variable the loop carries is unset (or a first-iteration flag
  holds), or right before the loop ends (`break`, `return`, `raise`), counts once per loop.
- A blocking wait on a `concurrent.futures.Future` known to be done (`if f.done():`, after
  `await asyncio.wrap_future(f)`) returns at once.

A file that cannot be read or does not parse is reported (exit code 2), and every other file is
checked as usual. A file that does not parse cannot run either, so heki assumes nothing it would
do: it calls nothing, defines no subclass, and patches no `builtins`. Its module still owns its
name: `import requests` reaches a project's own `requests.py`, and while that file does not
parse, the names under it resolve to nothing, never to the `requests` of the catalog.

A function whose model would pass its size budget (4 units per byte of its source, at most
50,000; real code stays below one unit per byte) is left unmodeled: heki rewrites nothing in it
and treats its body as one call it cannot resolve. `heki assumptions` lists it with the hazard
`unmodeled`. A function whose proof state (a fact per variable per block) would pass 8 cells per
byte of its source, at most 250,000, gets no `OPT` finding: heki does not try to prove a rewrite
in it. A function whose lazy initializations (the blocks each covers, and the walks that find
them) would take more than 8 cells per byte of its source, at most 250,000, has none of them
recognized: in a heuristic report its clients and queries count as made on every iteration,
and an exact report stops there.

A heuristic report goes the other way for the premises its rule page marks open: a branch heki
cannot decide counts as taken, a loop in which heki cannot show that a call runs once counts as
running it on every iteration, and a size heki cannot bound counts as data-sized (the code does
not fix it at 16 items or fewer). A loop over a list that is small by its nature (the columns of
a table, a handful of options) is reported: the report states a loop whose size the code does
not bound, and whether the loop is slow is the harm, which the rule cannot see. When an open
premise is false (a bound the code states that heki does not read, a call that cannot run on
every iteration), the report is wrong about the code, which is why it is heuristic. An exact
report never claims a size: it names the input a loop steps through.

Missed findings close as heki learns more:

| heki lacks | Add |
| ---------- | --- |
| the class of a value | an annotation |
| your blocking or offloading APIs | an entry in [`[[tool.heki.ecosystem]]`](configuration.md#ecosystem-knowledge-toolhekiecosystem) |
| the rules of your shared state | a [contract](configuration.md#contracts) |

## Known limitations

### Dynamic names

`setattr(obj, "fields", v)`, `getattr(obj, "fields")`, `getattr(obj, "method")(...)`, and
`obj.__dict__["fields"]`:

- **Followed** when the name is a constant: a literal, a module-level `Final` string
  (`KEY: Final = "_key"`), also when imported, re-exported, or bound to another `Final` name, a
  module-level string bound once in its module, or a class constant declared `ClassVar` or
  `Final` and bound once (`Model.KEY`, `self.KEY`, `cls.KEY`).
- **Not followed** for a module-level name without `Final` that another module rebinds or deletes,
  one bound in a loop or before a star import that may rebind it, a class constant stored on its
  class or an instance outside the class body (`Model.KEY = "x"`; from another module, only one
  without `Final`), a class constant a subclass defines again, is given its own value
  (`Child.KEY = "x"`), or inherits first from another base it lists before the class
  (`class Child(Other, Model)`; read on that subclass, or on an instance or `cls` that may be one),
  a `Final` field of a dataclass or of a class a `dataclass_transform` builds read on an instance
  (`Keys("x").KEY` is `"x"`), or on a class object whose default a class with `slots=True` or built
  by a transform replaces, or a computed name.
- A `write-owners` contract with `unresolved = "report"` reports the writes heki cannot tie to a
  class ([`MUT205`](rules/unresolved-guarded-write.md)).

### Objects without a known class

- Reads and writes through `Any` or unannotated parameters are not tied to a class, so contracts
  on that class do not see them. A contract with `unresolved = "report"` reports them by name:
  writes under [`MUT205`](rules/unresolved-guarded-write.md), reads under
  [`MUT206`](rules/unresolved-guarded-read.md), and method calls under
  [`MUT207`](rules/unresolved-guarded-call.md).
- A write in an unannotated helper is still found where a caller passes it an object of a known
  class. A read is not.
- Not typed: values declared `Any`, generics substituted through `TypeVar`s, and calls of
  `type[C]` attributes.

### Names the catalog does not list

Catalog lookups are exact. These contribute no effect until you add them with
[`[[tool.heki.ecosystem]]`](configuration.md#ecosystem-knowledge-toolhekiecosystem):

- a class reached through another public name: `from sqlalchemy import Engine` for
  `sqlalchemy.engine.Engine`, `redis.StrictRedis` for `redis.Redis`;
- a method the catalog does not list;
- objects reached through subscripts or framework attributes, which have no known class:
  `client["app"]["users"]`, Django's `Model.objects`.

### Properties

- **Followed:** reading a `@property` or `functools.cached_property` of a project class runs its
  getter. Its effects, writes, and the attributes it reads count for the reader, also through
  base classes, overriding subclasses, and `super()`. The getter's return annotation types the
  value read.
- **Not followed:** a property that also defines a setter or deleter (`@x.setter`), and
  `x = property(get_x)`.

### Read-only types

- `MUT110` follows `Sequence` / `Mapping` / `ReadOnly` parameters into the helpers they are
  handed to, any number of calls deep. A helper parameter annotated with a mutable type is left
  to the type checker, and a container inside the argument does not count.
- `MUT108` (opt-in) follows frozen dataclasses, `NamedTuple`s, classes that a
  `dataclass_transform(frozen_default=True)` in the checked files builds, `Final` / `ReadOnly`
  attributes, and `ReadOnly` `TypedDict` keys. It does not see the transforms of installed
  packages (attrs, Pydantic), and an instance the function constructs itself counts as under
  construction.

### Generators and coroutines

A generator's body runs where it is iterated:

| Followed | Not followed |
| -------- | ------------ |
| a `for` loop or a comprehension | a generator handed to anything else: `StreamingResponse(rows())`, a container |
| unpacking, `yield from` | a generator only stepped with `next()` |
| a consuming call: `list(...)`, `"".join(...)`, `items.extend(...)` | a generator unpacked into a call: `f(*rows())` |
| a lazy wrapper iterated there: `enumerate(...)` | |
| a project function iterating it | |

- A generator a function returns runs where its caller iterates it.
- A generator expression counts where it is written.
- A coroutine runs where it is awaited; one only scheduled (`asyncio.create_task`) is checked on
  its own.
- Iterating an object runs its class's `__iter__` only for classes the catalog lists (a
  SQLAlchemy `Query`). The `__iter__` methods of project classes are not followed.

### Guards

- **Followed:** a test of the function's own input: truthiness, `is None`, `isinstance` with
  builtin classes, the `io` classes, or `os.PathLike`, or `hasattr` with a string literal, also
  after the function rebinds the input on some paths (`if buf is None: buf = StringIO()`).
- **Decided by:** constants, displays and comprehensions, `io.StringIO` / `io.BytesIO` objects
  the caller constructs, and the literal defaults of omitted parameters (`hasattr` only for the
  methods of the file protocol: `read`, `write`, `close`, ...); not by an f-string, the result of
  another call, or an instance of a subclass.
- **Not followed:** `a or b` (no guard on either value), and `isinstance` with project classes or
  annotated types.
- A lazy initialization needs a direct test and the store in the function itself: a test in a
  caller of the method that stores, a test combined with `or` / `and`, and memos keyed by an
  argument (`@lru_cache`, a dict) are not followed.
- A future is known to be done only through `f.done()` and `await asyncio.wrap_future(f)`, not
  through an event or another future its done callback sets.
- `EFF201` does not use guards.
- An object a factory function returns counts as obtained once, like a pooled connection.

### Separate scopes

- Generator expressions and lambdas are analyzed as functions of their own:
  `next(b for b in bs if b.id == a.id)` inside a loop is not reported as a nested-loop join.
- Two lambdas in one function share the qualified name `f.<locals>.<lambda>`, so heki may
  attribute one lambda's effects to a call of the other:

```python
import requests


async def handler(url: str) -> int:
    fetch = lambda: requests.get(url, timeout=5)
    size = lambda: len(url)
    return size()  # reported as reaching `requests.get` (EFF101)
```

### Metaprogramming and frameworks

`__getattr__` proxies, metaclasses, classes created at run time, and framework machinery
(validators, ORM lazy loading, dependency injection) are not modeled. heki has no
framework-specific rules.

### The target Python version

Results follow [`target-version`](configuration.md#top-level-settings), which defaults to the
oldest version `requires-python` admits.

- From Python 3.12, list, set, and dict comprehensions run in the enclosing function (PEP 709): a
  comprehension inside a loop can be reported for `py312` and not for `py311`.
- From Python 3.14, annotations are evaluated lazily in `__annotate__` scopes (PEP 649).
- The parser reads Python 3.15 syntax for every target version: `lazy import` (PEP 810) is a lazy
  import also in code for an older version.
- Syntax newer than the target version is analyzed, not reported: that is Ruff's check.

### Source encodings

heki reads the encoding a PEP 263 declaration names: UTF-8 (with or without a BOM), Latin-1, and
ASCII. A file declaring any other encoding is reported as an error and not checked. Unlike a file
that does not parse, such a file runs: what it defines and does stays unknown to the analysis of
the other files.

### Sizes known only at run time

Whether a collection is small is decided from the code: a list whose size depends on the
input counts as large, even when it always holds three items. The rules this affects are marked size-dependent;
[Fixes and Safety](fixes.md#when-the-size-is-fixed) lists what counts as a fixed size.

### Callers outside the checked files

Contracts reason about the callers heki sees: a private helper hands its reports to its callers,
and a function called only from allowed writers is not reported. Code outside the run, such as
tests left out of it, is not known.

## How a rule gets into heki

heki only takes checks that need what it can prove. A proposed rule goes through four questions,
in order:

1. **Can the syntax of one file decide it?** Then it belongs in a linter such as Ruff.
2. **Is it a type error, or can an annotation express it?** Then it belongs in a type checker
   such as ty.
3. **Is it about which modules may import which?** Then it belongs in tach or import-linter.
4. **Does it need at least one of the proofs below?** Only then is it a heki rule.

A heki rule is measured on a corpus and reviewed by hand before it is enabled by default. Where
another tool covers part of it, heki keeps only the rest (`EFF107` and Ruff's `RUF006`).

The proofs heki can make, on five axes:

| Axis | What heki establishes | Used by |
| ---- | --------------------- | ------- |
| Control flow | which statements can run after which: CFG paths, loops and their bounds, exception edges | `MUT106`, `MUT109`, `MUT113`, `EFF102`, `EFF103`, `EFF106`, `EFF109`, `DATA101`, `OPT` |
| Calls | which functions a call reaches across modules, and what they do (summaries propagated over the call graph) | `EFF101`, `EFF109`, `EFF201`, `MUT103`, `MUT108`, `MUT109`, `MUT110`, `MUT112`, `MUT113`, `MUT201`–`MUT204` |
| Aliases | which names, iterators, and views refer to one object; whether it is unique to its frame and of an exact builtin type | `MUT202`, `MUT201`, `MUT204`, `MUT108`, `MUT110`, `MUT112`, `EFF107`, `EFF109`, `OPT101`–`OPT130` |
| Effects | which blocking, network, file, or database work runs, from the catalog of library behavior | `EFF101`–`EFF106`, `EFF201`, `DATA101` |
| State | which attributes and containers a function reads, writes, or mutates, and who may do so | `MUT101`, `MUT107`, `MUT108`, `MUT109`, `MUT110`, `MUT111`, `MUT112`, `MUT113`, `MUT201`–`MUT204` |
